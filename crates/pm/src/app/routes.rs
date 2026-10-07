//! The routes (`docs/app-api.md`). Reads call the CLI's own JSON
//! renderers; writes plan ops with the CLI verbs' own planners and commit
//! them through `Store::commit_batch` under the server's `Stamper`, then
//! nudge the event watcher. Every handler opens the workspace in a
//! blocking task, exactly as one CLI invocation would.

use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router, middleware};
use pm_core::op::{BodyEdit, FieldSet};
use pm_core::{Body, BodyState, BodyUpdate, Payload, Priority, Ticket, Workspace};
use pm_store::{Store, TicketFilter};
use serde::Deserialize;
use serde_json::{Map, Value, json};
use ulid::Ulid;

use super::{AppState, auth, events, initiatives};
use crate::exit::{self, CliError, Result};
use crate::verbs::{
    NewTicket, SCHEMA, Stamper, display_id, file_ticket, find, non_empty, parse_assignment,
    require_project, ticket_json, ticket_json_with_comments,
};
use crate::{mutate, project, ready, workspace};

pub(crate) fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/workspace", get(workspace_info))
        .route("/tickets", get(list).post(create))
        .route("/tickets/{id}", get(ticket))
        .route("/tickets/{id}/body", get(body).post(body_edit))
        .route("/tickets/{id}/fields", axum::routing::post(fields))
        .route("/tickets/{id}/labels", axum::routing::post(labels))
        .route("/tickets/{id}/state", axum::routing::post(state_move))
        .route("/projects", get(projects))
        .route("/initiatives", get(initiatives_tree))
        .route("/projects/{id}", get(project_show))
        .route(
            "/projects/{id}/body",
            get(design_doc_body).post(design_doc_edit),
        )
        .route(
            "/projects/{id}/docs/{name}/body",
            get(named_doc_body).post(named_doc_edit),
        )
        .route("/ready", get(ready_frontier))
        .route("/events", get(events::events))
        .fallback(not_found)
        .layer(middleware::from_fn_with_state(state.clone(), auth::guard))
        .with_state(state)
}

// ----------------------------------------------------------------- errors

/// `{"schema": 1, "error": "..."}` with `status`.
pub(crate) fn error_response(status: StatusCode, message: impl std::fmt::Display) -> Response {
    (
        status,
        Json(json!({ "schema": SCHEMA, "error": message.to_string() })),
    )
        .into_response()
}

/// A verb failure as HTTP: the CLI's exit codes map one-to-one.
pub(crate) struct ApiError(CliError);

impl From<CliError> for ApiError {
    fn from(e: CliError) -> Self {
        ApiError(e)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let status = match self.0.code {
            exit::USAGE => StatusCode::BAD_REQUEST,
            exit::NOT_FOUND => StatusCode::NOT_FOUND,
            exit::TAKEN => StatusCode::CONFLICT,
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        };
        error_response(status, format!("{:#}", self.0.error))
    }
}

type ApiResult<T = Json<Value>> = std::result::Result<T, ApiError>;

async fn not_found() -> Response {
    error_response(StatusCode::NOT_FOUND, "no such route")
}

// ---------------------------------------------------------------- helpers

/// Runs `f` on a blocking thread with the workspace open.
async fn with_store<T: Send + 'static>(
    state: &AppState,
    f: impl FnOnce(&mut Store, &Workspace) -> Result<T> + Send + 'static,
) -> ApiResult<T> {
    let dir = state.dir.clone();
    let result = tokio::task::spawn_blocking(move || {
        let (mut store, ws) = workspace::open(&dir)?;
        f(&mut store, &ws)
    })
    .await
    .map_err(|e| CliError::error(format!("api task: {e}")))?;
    Ok(result?)
}

/// A JSON object body, or a 400 that says what was wrong with it.
fn json_object(body: &Bytes) -> Result<Map<String, Value>> {
    match serde_json::from_slice::<Value>(body) {
        Ok(Value::Object(map)) => Ok(map),
        Ok(_) => Err(CliError::usage("the request body must be a JSON object")),
        Err(e) => Err(CliError::usage(format!(
            "the request body is not JSON: {e}"
        ))),
    }
}

/// `"a,b"` -> `["a", "b"]`; blanks dropped.
fn csv(value: Option<String>) -> Vec<String> {
    value
        .map(|v| {
            v.split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// The ticket as it now reads: the **Ticket** shape with its `comments`,
/// exactly `pm show --json` (AGT-1430), so a view can replace what it
/// shows with the answer to its own write.
fn current(store: &Store, ws: &Workspace, id: Ulid) -> Result<Value> {
    let ticket = store
        .ticket(id)?
        .ok_or_else(|| CliError::error(format!("ticket {id} vanished after mutation")))?;
    ticket_json_with_comments(ws, store, &ticket)
}

/// Commits `ops` on `ticket` as one batch and answers with the ticket.
/// An empty plan commits nothing (a no-op edit is not an op).
fn commit(
    state: &AppState,
    store: &mut Store,
    ws: &Workspace,
    ticket: &Ticket,
    ops: &[pm_core::Op],
) -> Result<Value> {
    if !ops.is_empty() {
        store.commit_batch(ops, &[])?;
        state.nudge.notify_one();
    }
    current(store, ws, ticket.id)
}

// ------------------------------------------------------------------ reads

/// `GET /workspace`
async fn workspace_info(State(state): State<Arc<AppState>>) -> ApiResult {
    let actor = state.actor.as_str().to_string();
    let dir = state.dir.clone();
    with_store(&state, move |_store, ws| {
        Ok(Json(json!({
            "schema": SCHEMA,
            "id": ws.id.to_string(),
            "prefix": ws.prefix,
            "states": ws.states,
            "gate_labels": ws.gate_labels,
            "actor": actor,
            "workspace": dir,
        })))
    })
    .await
}

/// `GET /tickets?…`: `pm list --json`'s filters, comma-separated where
/// the flag is.
#[derive(Deserialize, Default)]
struct ListQuery {
    project: Option<String>,
    state: Option<String>,
    label: Option<String>,
    repo: Option<String>,
    assignee: Option<String>,
    held: Option<bool>,
    github: Option<String>,
    search: Option<String>,
    archived: Option<bool>,
}

async fn list(State(state): State<Arc<AppState>>, Query(q): Query<ListQuery>) -> ApiResult {
    with_store(&state, move |store, ws| {
        let filter = TicketFilter {
            state: csv(q.state),
            project: csv(q.project),
            label: csv(q.label),
            repo: csv(q.repo),
            assignee: csv(q.assignee)
                .into_iter()
                .map(pm_core::ActorId::new)
                .collect(),
            held: q.held.unwrap_or(false),
            github: csv(q.github),
            search: q.search.filter(|s| !s.trim().is_empty()),
            archived: q.archived.unwrap_or(false),
        };
        let mut out = Vec::new();
        for t in store.tickets(&filter)? {
            out.push(ticket_json(ws, store, &t)?);
        }
        Ok(Json(Value::Array(out)))
    })
    .await
}

/// `GET /tickets/{id}`: `pm show --json`, comments included (AGT-1430).
async fn ticket(State(state): State<Arc<AppState>>, Path(id): Path<String>) -> ApiResult {
    with_store(&state, move |store, ws| {
        let t = find(store, ws, &id)?;
        Ok(Json(ticket_json_with_comments(ws, store, &t)?))
    })
    .await
}

/// `GET /tickets/{id}/body[?since=<base64 version>]`: the description as
/// a CRDT document plus its text. Without `since`, the whole Loro
/// snapshot (base64), for an editor binding a fresh `loro-crdt` document
/// to it. With `since` — the editor document's encoded oplog version
/// vector (`doc.oplogVersion().encode()`) — only the ops it lacks, as one
/// update: how an open editor catches up after an op event.
#[derive(Deserialize, Default)]
struct BodyQuery {
    since: Option<String>,
}

/// `since=`, decoded: a `+` a client forgot to percent-encode arrives as a
/// space.
fn since_version(q: BodyQuery) -> Result<Option<Vec<u8>>> {
    q.since
        .map(|b64| {
            pm_core::bytes::decode(&b64.replace(' ', "+"))
                .map_err(|e| CliError::usage(format!("since: not a base64 version vector: {e}")))
        })
        .transpose()
}

/// Adds `text` and either `snapshot` (no `since`) or `update` (the ops
/// `since` lacks) for `body` to `out` — the one body answer every body
/// endpoint (a ticket's description, a project document) gives.
fn body_answer(out: &mut Value, body: &BodyState, since: Option<Vec<u8>>) -> Result<()> {
    out["text"] = json!(body.text());
    match since {
        None => {
            let snapshot = body
                .snapshot()
                .map_err(|e| CliError::error(format!("reading body history: {e}")))?;
            out["snapshot"] = json!(pm_core::bytes::encode(snapshot.as_bytes()));
        }
        Some(version) => {
            let update = body.updates_since(&version).map_err(|e| match e {
                pm_core::BodyError::Import(e) => {
                    CliError::usage(format!("since: not a Loro version vector: {e}"))
                }
                e => CliError::error(format!("reading body history: {e}")),
            })?;
            out["update"] = json!(pm_core::bytes::encode(update.as_bytes()));
        }
    }
    Ok(())
}

async fn body(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Query(q): Query<BodyQuery>,
) -> ApiResult {
    let since = since_version(q)?;
    with_store(&state, move |store, ws| {
        let t = find(store, ws, &id)?;
        let view = store
            .ticket_view(t.id)?
            .ok_or_else(|| CliError::not_found(format!("no ticket {}", display_id(ws, &t))))?;
        let mut out = json!({
            "schema": SCHEMA,
            "id": display_id(ws, &t),
            "ulid": t.id.to_string(),
        });
        body_answer(&mut out, &view.body, since)?;
        Ok(Json(out))
    })
    .await
}

/// `GET /projects[?status=…]`
#[derive(Deserialize, Default)]
struct ProjectsQuery {
    status: Option<String>,
}

async fn projects(State(state): State<Arc<AppState>>, Query(q): Query<ProjectsQuery>) -> ApiResult {
    let status = project_status(q.status)?;
    with_store(&state, move |store, _ws| {
        let mut projects = store.projects()?;
        if let Some(status) = status {
            projects.retain(|p| p.status == status);
        }
        let out: Vec<Value> = projects.iter().map(project::project_json).collect();
        Ok(Json(json!({ "schema": SCHEMA, "projects": out })))
    })
    .await
}

/// A `?status=` value, parsed as `pm project list --status` parses it.
fn project_status(status: Option<String>) -> Result<Option<pm_core::ProjectStatus>> {
    status
        .map(|s| s.parse::<pm_core::ProjectStatus>().map_err(CliError::usage))
        .transpose()
}

/// `GET /initiatives[?status=…]` (AGT-1491): the initiative -> project
/// tree with per-node ticket rollups (`app/initiatives.rs`). Takes the
/// same `status` as `GET /projects`.
async fn initiatives_tree(
    State(state): State<Arc<AppState>>,
    Query(q): Query<ProjectsQuery>,
) -> ApiResult {
    let status = project_status(q.status)?;
    with_store(&state, move |store, ws| {
        let projects = store.projects()?;
        let tickets = store.tickets(&TicketFilter::default())?;
        let own = initiatives::own_counts(ws, &tickets);
        Ok(Json(initiatives::tree(&projects, &own, status)))
    })
    .await
}

/// `GET /projects/{id}`
async fn project_show(State(state): State<Arc<AppState>>, Path(id): Path<String>) -> ApiResult {
    with_store(&state, move |store, _ws| {
        let p = store
            .project(&id)?
            .ok_or_else(|| CliError::not_found(format!("no project '{id}'")))?;
        Ok(Json(project::project_json(&p)))
    })
    .await
}

// ---------------------------------------------------------- project docs

/// A project document: the design doc (`name` = `None`) or a named one.
/// Its `doc_id` — the entity its `body.edit` ops target (AGT-1344/1413) —
/// and its merge state (empty until its first edit).
struct ProjectDoc {
    project: String,
    name: Option<String>,
    doc_id: Ulid,
    body: BodyState,
}

fn project_doc(store: &Store, project: &str, name: Option<&str>) -> Result<ProjectDoc> {
    let p = store
        .project(project)?
        .ok_or_else(|| CliError::not_found(format!("no project '{project}'")))?;
    let doc_id = match name {
        None => store.design_doc_id(project)?.ok_or_else(|| {
            CliError::error(format!(
                "project '{project}' has no design doc bound yet (its binding has not synced here)"
            ))
        })?,
        Some(name) => {
            if !p.documents.contains_key(name) {
                return Err(CliError::not_found(format!(
                    "project '{project}' has no document '{name}'"
                )));
            }
            store.named_doc_id(project, name)?.ok_or_else(|| {
                CliError::error(format!(
                    "project '{project}' document '{name}' has no id bound yet (its binding has not synced here)"
                ))
            })?
        }
    };
    let body = store
        .doc_view(doc_id)?
        .map(|view| view.body)
        .unwrap_or_default();
    Ok(ProjectDoc {
        project: project.to_string(),
        name: name.map(str::to_string),
        doc_id,
        body,
    })
}

/// `{"schema", "project", "doc", "doc_id"}`: which document a body answer
/// is. `doc` is `null` for the design doc.
fn doc_header(doc: &ProjectDoc) -> Value {
    json!({
        "schema": SCHEMA,
        "project": doc.project,
        "doc": doc.name,
        "doc_id": doc.doc_id.to_string(),
    })
}

async fn doc_body(
    state: Arc<AppState>,
    id: String,
    name: Option<String>,
    q: BodyQuery,
) -> ApiResult {
    let since = since_version(q)?;
    with_store(&state, move |store, _ws| {
        let doc = project_doc(store, &id, name.as_deref())?;
        let mut out = doc_header(&doc);
        body_answer(&mut out, &doc.body, since)?;
        Ok(Json(out))
    })
    .await
}

/// `GET /projects/{id}/body[?since=]`: the design doc, as
/// `GET /tickets/{id}/body` serves a description.
async fn design_doc_body(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Query(q): Query<BodyQuery>,
) -> ApiResult {
    doc_body(state, id, None, q).await
}

/// `GET /projects/{id}/docs/{name}/body[?since=]`: a named document.
async fn named_doc_body(
    State(state): State<Arc<AppState>>,
    Path((id, name)): Path<(String, String)>,
    Query(q): Query<BodyQuery>,
) -> ApiResult {
    doc_body(state, id, Some(name), q).await
}

async fn doc_edit(
    state: Arc<AppState>,
    id: String,
    name: Option<String>,
    body: Bytes,
) -> ApiResult {
    let edit = BodyEditRequest::parse(&body)?;
    let actor = state.actor.clone();
    let shared = state.clone();
    with_store(&state, move |store, _ws| {
        let doc = project_doc(store, &id, name.as_deref())?;
        let update = edit.plan(&doc.body)?;
        if !update.is_empty() {
            let mut stamper = Stamper::new(store, actor)?;
            let op = stamper.op(doc.doc_id, Payload::BodyEdit(BodyEdit { update }));
            // The document analogue of `commit_batch`: the op joins the log
            // (and the outbox) and the document is re-materialized, in one
            // transaction — exactly `pm project edit`'s write.
            store.commit_doc_edit(doc.doc_id, &op)?;
            shared.nudge.notify_one();
        }
        let doc = project_doc(store, &id, name.as_deref())?;
        let mut out = doc_header(&doc);
        out["text"] = json!(doc.body.text());
        Ok(Json(out))
    })
    .await
}

/// `POST /projects/{id}/body`: `{"update"}` or `{"text"}`, as
/// `POST /tickets/{id}/body`; one `body.edit` on the design doc's `doc_id`.
async fn design_doc_edit(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    body: Bytes,
) -> ApiResult {
    doc_edit(state, id, None, body).await
}

/// `POST /projects/{id}/docs/{name}/body`: the same, on a named document.
async fn named_doc_edit(
    State(state): State<Arc<AppState>>,
    Path((id, name)): Path<(String, String)>,
    body: Bytes,
) -> ApiResult {
    doc_edit(state, id, Some(name), body).await
}

/// `GET /ready?…`: `pm ready --json`'s flags.
#[derive(Deserialize, Default)]
struct ReadyQuery {
    project: Option<String>,
    ids: Option<String>,
    limit: Option<usize>,
    model: Option<String>,
    exclude_label: Option<String>,
}

async fn ready_frontier(
    State(state): State<Arc<AppState>>,
    Query(q): Query<ReadyQuery>,
) -> ApiResult {
    let args = ready::ReadyArgs {
        project: q.project,
        ids: csv(q.ids),
        limit: q.limit,
        model: q.model,
        exclude_labels: csv(q.exclude_label),
        explain: false,
    };
    with_store(&state, move |store, ws| {
        Ok(Json(ready::compute(store, ws, &args)?.json))
    })
    .await
}

// ----------------------------------------------------------------- writes

/// `POST /tickets/{id}/fields` `{"<key>": <value>, …}`: one `field.set`
/// per key, the keys `pm set` takes, `null` or `""` clearing an optional
/// field; an unknown key lands in `ext` as it does for `pm set`.
async fn fields(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    body: Bytes,
) -> ApiResult {
    let object = json_object(&body)?;
    if object.is_empty() {
        return Err(CliError::usage("no fields given").into());
    }
    let mut sets: Vec<FieldSet> = Vec::with_capacity(object.len());
    for (key, value) in object {
        let text = match value {
            Value::Null => String::new(),
            Value::String(s) => s,
            Value::Bool(b) => b.to_string(),
            Value::Number(n) => n.to_string(),
            _ => {
                return Err(CliError::usage(format!(
                    "{key}: expected a string, number, boolean or null"
                ))
                .into());
            }
        };
        sets.push(parse_assignment(&format!("{key}={text}"))?);
    }
    let actor = state.actor.clone();
    let shared = state.clone();
    with_store(&state, move |store, ws| {
        let ticket = find(store, ws, &id)?;
        for set in &sets {
            if let FieldSet::Project(Some(project)) = set {
                require_project(store, project)?;
            }
        }
        let mut stamper = Stamper::new(store, actor)?;
        let ops: Vec<_> = sets
            .into_iter()
            .map(|set| stamper.op(ticket.id, Payload::FieldSet(set)))
            .collect();
        Ok(Json(commit(&shared, store, ws, &ticket, &ops)?))
    })
    .await
}

/// `POST /tickets/{id}/labels` `{"add": [...], "remove": [...]}`.
async fn labels(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    body: Bytes,
) -> ApiResult {
    let object = json_object(&body)?;
    let names = |key: &str| -> Result<Vec<String>> {
        match object.get(key) {
            None | Some(Value::Null) => Ok(Vec::new()),
            Some(Value::Array(items)) => items
                .iter()
                .map(|item| match item.as_str().map(str::trim) {
                    Some(label) if !label.is_empty() => Ok(label.to_string()),
                    _ => Err(CliError::usage(format!(
                        "{key}: every label must be a non-empty string"
                    ))),
                })
                .collect(),
            Some(_) => Err(CliError::usage(format!(
                "{key}: expected an array of labels"
            ))),
        }
    };
    let changes: Vec<mutate::LabelChange> = names("add")?
        .into_iter()
        .map(mutate::LabelChange::Add)
        .chain(
            names("remove")?
                .into_iter()
                .map(mutate::LabelChange::Remove),
        )
        .collect();
    if changes.is_empty() {
        return Err(CliError::usage("nothing to change: give add and/or remove").into());
    }
    let actor = state.actor.clone();
    let shared = state.clone();
    with_store(&state, move |store, ws| {
        let ticket = find(store, ws, &id)?;
        let mut stamper = Stamper::new(store, actor)?;
        let ops = mutate::label_ops(store, ws, &ticket, &mut stamper, &changes)?;
        Ok(Json(commit(&shared, store, ws, &ticket, &ops)?))
    })
    .await
}

/// `POST /tickets/{id}/state` `{"state": "...", "keep_assignee": false}`:
/// `pm move`, assignee clear included.
async fn state_move(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    body: Bytes,
) -> ApiResult {
    let object = json_object(&body)?;
    let target = object
        .get("state")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| CliError::usage("state: a non-empty string is required"))?
        .to_string();
    let keep_assignee = match object.get("keep_assignee") {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(_) => return Err(CliError::usage("keep_assignee: expected a boolean").into()),
    };
    let actor = state.actor.clone();
    let shared = state.clone();
    with_store(&state, move |store, ws| {
        let ticket = find(store, ws, &id)?;
        let mut stamper = Stamper::new(store, actor)?;
        let (ops, _cleared) = mutate::move_ops(ws, &ticket, &mut stamper, &target, keep_assignee)?;
        Ok(Json(commit(&shared, store, ws, &ticket, &ops)?))
    })
    .await
}

/// A body write's request: `{"update": "<base64>"}` — a Loro update the
/// editor's own document produced (its peer id must be fresh per session,
/// as `crate::edit` explains) — or `{"text": "..."}`, the whole body,
/// diffed under a fresh session peer exactly as `pm edit` diffs a save.
enum BodyEditRequest {
    Update(Vec<u8>),
    Text(String),
}

impl BodyEditRequest {
    fn parse(body: &Bytes) -> Result<Self> {
        let object = json_object(body)?;
        match (object.get("update"), object.get("text")) {
            (Some(_), Some(_)) => Err(CliError::usage("give either update or text, not both")),
            (Some(Value::String(b64)), None) => {
                let bytes = pm_core::bytes::decode(b64)
                    .map_err(|e| CliError::usage(format!("update: not base64 Loro bytes: {e}")))?;
                if bytes.is_empty() {
                    return Err(CliError::usage("update: empty"));
                }
                Ok(BodyEditRequest::Update(bytes))
            }
            (None, Some(Value::String(text))) => Ok(BodyEditRequest::Text(text.clone())),
            _ => Err(CliError::usage(
                "expected {\"update\": \"<base64>\"} or {\"text\": \"...\"}",
            )),
        }
    }

    /// The `body.edit` update for `body`, relative to the history it has so
    /// a concurrent edit merges instead of being reverted; empty when
    /// nothing changes (text equal to the current body).
    fn plan(self, body: &BodyState) -> Result<Vec<u8>> {
        match self {
            BodyEditRequest::Text(text) => {
                if text == body.text() {
                    Ok(Vec::new())
                } else {
                    crate::edit::body_update(body, &text, crate::edit::session_peer(Ulid::new()))
                }
            }
            BodyEditRequest::Update(bytes) => {
                // Prove the bytes apply on top of what the body has before
                // logging them: a corrupt update, or one whose history this
                // replica lacks, is refused here rather than left for the
                // store (or every other replica) to trip on.
                let err = |e: pm_core::BodyError| {
                    CliError::usage(format!("update: not a Loro update for this body: {e}"))
                };
                let mut probe = Body::new();
                probe.apply(&body.snapshot().map_err(err)?).map_err(err)?;
                let pending = probe
                    .apply_awaiting(&BodyUpdate::from_bytes(bytes.clone()))
                    .map_err(err)?;
                if pending {
                    return Err(CliError::usage(
                        "update: depends on history this replica does not have; reload the body and retry",
                    ));
                }
                Ok(bytes)
            }
        }
    }
}

/// `POST /tickets/{id}/body`: a [`BodyEditRequest`]; one `body.edit` op.
async fn body_edit(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    body: Bytes,
) -> ApiResult {
    let edit = BodyEditRequest::parse(&body)?;
    let actor = state.actor.clone();
    let shared = state.clone();
    with_store(&state, move |store, ws| {
        let ticket = find(store, ws, &id)?;
        let view = store
            .ticket_view(ticket.id)?
            .ok_or_else(|| CliError::not_found(format!("no ticket {}", display_id(ws, &ticket))))?;
        let update = edit.plan(&view.body)?;
        let ops = if update.is_empty() {
            Vec::new()
        } else {
            let mut stamper = Stamper::new(store, actor)?;
            vec![stamper.op(ticket.id, Payload::BodyEdit(BodyEdit { update }))]
        };
        Ok(Json(commit(&shared, store, ws, &ticket, &ops)?))
    })
    .await
}

// ----------------------------------------------------------- new tickets

/// An optional string field of a `POST /tickets` body: absent, `null` or
/// blank is `None`.
fn opt_string(object: &Map<String, Value>, key: &str) -> Result<Option<String>> {
    match object.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) if s.trim().is_empty() => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.trim().to_string())),
        Some(_) => Err(CliError::usage(format!("{key}: expected a string"))),
    }
}

/// A string-array field (`labels`, `blocked_by`): absent or `null` is
/// empty; every item a non-empty string.
fn string_list(object: &Map<String, Value>, key: &str) -> Result<Vec<String>> {
    match object.get(key) {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::Array(items)) => items
            .iter()
            .map(|item| match item.as_str().map(str::trim) {
                Some(s) if !s.is_empty() => Ok(s.to_string()),
                _ => Err(CliError::usage(format!(
                    "{key}: every item must be a non-empty string"
                ))),
            })
            .collect(),
        Some(_) => Err(CliError::usage(format!(
            "{key}: expected an array of strings"
        ))),
    }
}

/// `POST /tickets` `{"title", "project"?, "priority"?, "repo"?, "labels"?,
/// "description"?, "blocked_by"?}`: `pm new` with those flags — the same
/// validation, op set and numbering (`verbs::file_ticket`): numbered
/// locally, or pending (`AGT-?`, named by its ULID) while a configured hub
/// has yet to number it. Answers `201` with the ticket as `pm new --json`
/// prints it. Any other key is a `400`: the view files what `pm new`
/// files, nothing more.
async fn create(State(state): State<Arc<AppState>>, body: Bytes) -> ApiResult<Response> {
    let object = json_object(&body)?;
    const KEYS: &[&str] = &[
        "title",
        "project",
        "priority",
        "repo",
        "labels",
        "description",
        "blocked_by",
    ];
    if let Some(key) = object.keys().find(|k| !KEYS.contains(&k.as_str())) {
        return Err(CliError::usage(format!(
            "unknown key '{key}': expected one of {}",
            KEYS.join(", ")
        ))
        .into());
    }
    let title = match object.get("title") {
        Some(Value::String(t)) => non_empty("title", t)?,
        _ => return Err(CliError::usage("title: a non-empty string is required").into()),
    };
    let priority = match opt_string(&object, "priority")? {
        None => Priority::default(),
        Some(p) => serde_json::from_value::<Priority>(Value::String(p.clone())).map_err(|_| {
            CliError::usage(format!(
                "unknown priority '{p}': expected one of low, medium, high, critical"
            ))
        })?,
    };
    let spec = NewTicket {
        title,
        project: opt_string(&object, "project")?,
        repo: opt_string(&object, "repo")?,
        priority,
        labels: string_list(&object, "labels")?.into_iter().collect(),
        description: opt_string(&object, "description")?,
        blocked_by: string_list(&object, "blocked_by")?,
        source: None,
        linked_github: None,
    };
    let actor = state.actor.clone();
    let env = state.env.clone();
    let shared = state.clone();
    let ticket = with_store(&state, move |store, ws| {
        let ticket = file_ticket(store, ws, &env, &actor, spec)?;
        shared.nudge.notify_one();
        ticket_json(ws, store, &ticket)
    })
    .await?;
    Ok((StatusCode::CREATED, Json(ticket)).into_response())
}
