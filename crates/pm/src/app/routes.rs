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
use pm_core::{Body, BodyUpdate, Payload, Ticket, Workspace};
use pm_store::{Store, TicketFilter};
use serde::Deserialize;
use serde_json::{Map, Value, json};
use ulid::Ulid;

use super::{AppState, auth, events};
use crate::exit::{self, CliError, Result};
use crate::verbs::{
    SCHEMA, Stamper, display_id, find, parse_assignment, require_project, ticket_json,
    ticket_json_with_comments,
};
use crate::{mutate, project, ready, workspace};

pub(crate) fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/workspace", get(workspace_info))
        .route("/tickets", get(list))
        .route("/tickets/{id}", get(ticket))
        .route("/tickets/{id}/body", get(body).post(body_edit))
        .route("/tickets/{id}/fields", axum::routing::post(fields))
        .route("/tickets/{id}/labels", axum::routing::post(labels))
        .route("/tickets/{id}/state", axum::routing::post(state_move))
        .route("/projects", get(projects))
        .route("/projects/{id}", get(project_show))
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

async fn body(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Query(q): Query<BodyQuery>,
) -> ApiResult {
    // A `+` a client forgot to percent-encode arrives as a space.
    let since = q
        .since
        .map(|b64| {
            pm_core::bytes::decode(&b64.replace(' ', "+"))
                .map_err(|e| CliError::usage(format!("since: not a base64 version vector: {e}")))
        })
        .transpose()?;
    with_store(&state, move |store, ws| {
        let t = find(store, ws, &id)?;
        let view = store
            .ticket_view(t.id)?
            .ok_or_else(|| CliError::not_found(format!("no ticket {}", display_id(ws, &t))))?;
        let mut out = json!({
            "schema": SCHEMA,
            "id": display_id(ws, &t),
            "ulid": t.id.to_string(),
            "text": view.body.text(),
        });
        match since {
            None => {
                let snapshot = view
                    .body
                    .snapshot()
                    .map_err(|e| CliError::error(format!("reading description history: {e}")))?;
                out["snapshot"] = json!(pm_core::bytes::encode(snapshot.as_bytes()));
            }
            Some(version) => {
                let update = view.body.updates_since(&version).map_err(|e| match e {
                    pm_core::BodyError::Import(e) => {
                        CliError::usage(format!("since: not a Loro version vector: {e}"))
                    }
                    e => CliError::error(format!("reading description history: {e}")),
                })?;
                out["update"] = json!(pm_core::bytes::encode(update.as_bytes()));
            }
        }
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
    let status = q
        .status
        .map(|s| {
            serde_json::from_value::<pm_core::ProjectStatus>(Value::String(s.clone())).map_err(
                |_| {
                    CliError::usage(format!(
                        "unknown status '{s}': expected one of in-progress, complete, abandoned"
                    ))
                },
            )
        })
        .transpose()?;
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

/// `POST /tickets/{id}/body`: either `{"update": "<base64>"}` — a Loro
/// update the editor's own document produced (its peer id must be fresh
/// per session, as `crate::edit` explains) — or `{"text": "..."}`, the
/// whole description, diffed here under a fresh session peer exactly as
/// `pm edit` diffs a save. Either way one `body.edit` op, relative to the
/// history the ticket has, so a concurrent edit merges instead of being
/// reverted.
async fn body_edit(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    body: Bytes,
) -> ApiResult {
    let object = json_object(&body)?;
    enum Edit {
        Update(Vec<u8>),
        Text(String),
    }
    let edit = match (object.get("update"), object.get("text")) {
        (Some(_), Some(_)) => {
            return Err(CliError::usage("give either update or text, not both").into());
        }
        (Some(Value::String(b64)), None) => {
            let bytes = pm_core::bytes::decode(b64)
                .map_err(|e| CliError::usage(format!("update: not base64 Loro bytes: {e}")))?;
            if bytes.is_empty() {
                return Err(CliError::usage("update: empty").into());
            }
            Edit::Update(bytes)
        }
        (None, Some(Value::String(text))) => Edit::Text(text.clone()),
        _ => {
            return Err(CliError::usage(
                "expected {\"update\": \"<base64>\"} or {\"text\": \"...\"}",
            )
            .into());
        }
    };
    let actor = state.actor.clone();
    let shared = state.clone();
    with_store(&state, move |store, ws| {
        let ticket = find(store, ws, &id)?;
        let view = store.ticket_view(ticket.id)?.ok_or_else(|| {
            CliError::not_found(format!("no ticket {}", display_id(ws, &ticket)))
        })?;
        let update = match edit {
            Edit::Text(text) => {
                if text == view.body.text() {
                    Vec::new()
                } else {
                    crate::edit::body_update(&view, &text, crate::edit::session_peer(Ulid::new()))?
                }
            }
            Edit::Update(bytes) => {
                // Prove the bytes apply on top of what the ticket has
                // before logging them: a corrupt update, or one whose
                // history this replica lacks, is refused here rather than
                // left for the store (or every other replica) to trip on.
                let err = |e: pm_core::BodyError| {
                    CliError::usage(format!("update: not a Loro update for this body: {e}"))
                };
                let mut probe = Body::new();
                probe
                    .apply(&view.body.snapshot().map_err(err)?)
                    .map_err(err)?;
                let pending = probe
                    .apply_awaiting(&BodyUpdate::from_bytes(bytes.clone()))
                    .map_err(err)?;
                if pending {
                    return Err(CliError::usage(
                        "update: depends on history this replica does not have; reload the body and retry",
                    ));
                }
                bytes
            }
        };
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
