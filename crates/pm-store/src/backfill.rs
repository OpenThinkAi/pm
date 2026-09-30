//! Migration 0007's backfill (AGT-1385): one config op per existing
//! workspace field, state, actor and project, so a database written before
//! config was op-logged becomes op-derived with no data change. And
//! migration 0008's ([`doc_identity`], AGT-1413): one `project.doc_add` per
//! existing project document, so document identity is too.
//!
//! Run by [`crate::Store::open`] inside the migration's transaction, right
//! after `migrations/0007_config_ops.sql`. Adds `project.ulid` (the one
//! statement the SQL file cannot make idempotent) and then, for a database
//! with a workspace, appends the ops and writes the views
//! ([`WorkspaceView`] / [`ProjectView`]) from them; the rows themselves are
//! not touched — they already hold exactly what replaying these ops
//! produces, which `pm doctor` then confirms. Actor `migrate`, HLCs
//! strictly below the log's oldest op (one wall-clock millisecond under
//! it, counters ascending) so any later real config write wins LWW over
//! the backfill, whatever its own stamp.
//!
//! Idempotent, like the SQL: a workspace that already has a
//! `workspace_view` row and a project that already has a `ulid` are left
//! alone, so re-running the migration (a test rolling the schema version
//! back) changes nothing.

use std::collections::{BTreeMap, BTreeSet};

use pm_core::op::{BodyEdit, ProjectCreate, ProjectDocAdd, ProjectSet};
use pm_core::{
    ActorId, Body, DocView, Hlc, Op, Payload, ProjectStatus, ProjectView, WorkspaceView, apply_doc,
    apply_project, apply_workspace,
};
use rusqlite::{OptionalExtension, Transaction, params};
use ulid::Ulid;

use crate::codec::{enum_from_name, from_json, json, ulid};
use crate::commit::{append_op, ensure_actor, now_ms};
use crate::config::{actors, load_project_view, record_doc_owners, workspace_diff, workspace_row};
use crate::error::Result;

/// The actor every backfilled op records.
pub const MIGRATE_ACTOR: &str = "migrate";

/// What the backfill appended.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Backfilled {
    pub ops: usize,
    pub projects: usize,
}

struct Stamper {
    wall_ms: u64,
    counter: u32,
    actor: ActorId,
}

impl Stamper {
    fn op(&mut self, entity: Ulid, payload: Payload) -> Op {
        let hlc = Hlc::new(self.wall_ms, self.counter);
        self.counter += 1;
        Op::new(Ulid::new(), hlc, self.actor.clone(), entity, payload)
    }
}

/// Stamps strictly below the log's oldest op: one wall-clock millisecond
/// under it, counter 0 up (now, on an empty log).
fn stamper_below_oldest(tx: &Transaction<'_>) -> Result<Stamper> {
    let oldest: Option<i64> = tx
        .query_row("SELECT MIN(hlc_wall_ms) FROM ops", [], |r| r.get(0))
        .optional()?
        .flatten();
    let wall_ms = match oldest {
        Some(ms) => (ms as u64).saturating_sub(1),
        None => now_ms(),
    };
    let actor = ActorId::new(MIGRATE_ACTOR);
    ensure_actor(tx, &actor)?;
    Ok(Stamper {
        wall_ms,
        counter: 0,
        actor,
    })
}

pub(crate) fn run(tx: &Transaction<'_>) -> Result<Backfilled> {
    let has_ulid: bool = tx.query_row(
        "SELECT COUNT(*) > 0 FROM pragma_table_info('project') WHERE name = 'ulid'",
        [],
        |r| r.get(0),
    )?;
    if !has_ulid {
        tx.execute_batch(
            "ALTER TABLE project ADD COLUMN ulid TEXT;
             CREATE UNIQUE INDEX IF NOT EXISTS project_ulid ON project(ulid) WHERE ulid IS NOT NULL;",
        )?;
    }
    let Some(ws) = workspace_row(tx)? else {
        return Ok(Backfilled::default()); // a fresh database: nothing to carry
    };
    let already: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM workspace_view WHERE workspace = ?1)",
        params![ws.id.to_string()],
        |r| r.get(0),
    )?;
    if already {
        return Ok(Backfilled::default()); // re-run over its own work
    }
    // Read the actors before `migrate` joins them: it is recorded through
    // the ops' own FK, like any actor, not as a backfilled upsert.
    let actor_rows = actors(tx)?;
    let mut stamper = stamper_below_oldest(tx)?;
    let mut done = Backfilled::default();

    // Workspace: every field, every state, every actor.
    let mut view = WorkspaceView::new(ws.id);
    let mut payloads = workspace_diff(None, &ws);
    payloads.extend(actor_rows.into_iter().map(Payload::ActorUpsert));
    for payload in payloads {
        let op = stamper.op(ws.id, payload);
        apply_workspace(&mut view, &op)?;
        append_op(tx, &op)?;
        done.ops += 1;
    }
    tx.execute(
        "INSERT INTO workspace_view (workspace, view) VALUES (?1, ?2)",
        params![ws.id.to_string(), json(&view)],
    )?;

    // Projects, parents first (a child's create names its parent, and a
    // replay in seq order must find it — R2).
    for row in project_rows(tx)? {
        let ulid = Ulid::new();
        let mut view = ProjectView::new(ulid);
        let mut payloads = vec![Payload::ProjectCreate(ProjectCreate {
            id: row.id.clone(),
            title: row.title,
            status: row.status,
            parent: row.parent,
            // Bound by migration 0008's `project.doc_add` instead
            // ([`doc_identity`]), which runs right after.
            doc_id: None,
        })];
        payloads.extend(
            row.repos
                .into_iter()
                .map(|repo| Payload::ProjectSet(ProjectSet::RepoAdd(repo))),
        );
        for payload in payloads {
            let op = stamper.op(ulid, payload);
            apply_project(&mut view, &op)?;
            append_op(tx, &op)?;
            done.ops += 1;
        }
        tx.execute(
            "UPDATE project SET ulid = ?1 WHERE id = ?2",
            params![ulid.to_string(), row.id],
        )?;
        tx.execute(
            "INSERT INTO project_view (project, view) VALUES (?1, ?2)",
            params![ulid.to_string(), json(&view)],
        )?;
        done.projects += 1;
    }
    Ok(done)
}

struct ProjectRow {
    id: String,
    title: String,
    status: ProjectStatus,
    parent: Option<String>,
    repos: BTreeSet<String>,
}

/// Every project row without a `ulid` yet, parents before children.
fn project_rows(tx: &Transaction<'_>) -> Result<Vec<ProjectRow>> {
    let mut stmt = tx.prepare(
        "SELECT id, title, status, parent, repos FROM project WHERE ulid IS NULL ORDER BY id",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, Option<String>>(3)?,
            r.get::<_, String>(4)?,
        ))
    })?;
    let mut pending: BTreeMap<String, ProjectRow> = BTreeMap::new();
    for row in rows {
        let (id, title, status, parent, repos) = row?;
        pending.insert(
            id.clone(),
            ProjectRow {
                id,
                title,
                status: enum_from_name("project.status", status)?,
                parent,
                repos: from_json("project.repos", &repos)?,
            },
        );
    }
    let mut sorted = Vec::with_capacity(pending.len());
    let mut placed: BTreeSet<String> = BTreeSet::new();
    while !pending.is_empty() {
        let ready: Vec<String> = pending
            .values()
            .filter(|p| {
                p.parent
                    .as_ref()
                    .is_none_or(|parent| placed.contains(parent) || !pending.contains_key(parent))
            })
            .map(|p| p.id.clone())
            .collect();
        // Every parent is a row (foreign key), so each pass places at
        // least one project; the fallback keeps a cycle from looping.
        let ready = if ready.is_empty() {
            pending.keys().take(1).cloned().collect()
        } else {
            ready
        };
        for id in ready {
            placed.insert(id.clone());
            sorted.push(pending.remove(&id).expect("listed from pending"));
        }
    }
    Ok(sorted)
}

/// Migration 0008's backfill (AGT-1413): for every project row whose view
/// has no design doc bound yet, one `project.doc_add` binding the row's
/// `doc_id` as the design doc and one per `project_doc` row binding its
/// `doc_id` to its name — actor `migrate`, HLCs below the log's oldest op
/// ([`run`]'s rule, so any later real binding of the same slot is the one
/// that loses: the earliest binding wins). The ops are appended and the
/// project views and `project_doc_owner` written from them; the rows
/// already hold what materializing those views produces.
///
/// A document with no `doc_id` (written directly before AGT-1344) gets a
/// fresh one, and — when its cached text is not empty — a `body.edit`
/// carrying that text, so it replays to exactly what it holds.
///
/// Idempotent: a project whose view already binds a design doc is left
/// alone.
pub(crate) fn doc_identity(tx: &Transaction<'_>) -> Result<Backfilled> {
    let mut pending: Vec<(ProjectView, String, Option<String>, String)> = Vec::new();
    {
        let mut stmt = tx.prepare(
            "SELECT ulid, id, doc_id, doc FROM project WHERE ulid IS NOT NULL ORDER BY id",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, Option<String>>(2)?,
                r.get::<_, String>(3)?,
            ))
        })?;
        for row in rows {
            let (project, slug, doc_id, doc) = row?;
            let Some(view) = load_project_view(tx, ulid("project.ulid", &project)?)? else {
                continue; // 0007 gave every row a view; nothing to bind to
            };
            if view.design_doc_id().is_none() {
                pending.push((view, slug, doc_id, doc));
            }
        }
    }
    if pending.is_empty() {
        return Ok(Backfilled::default());
    }
    let mut stamper = stamper_below_oldest(tx)?;
    let mut done = Backfilled::default();
    for (mut view, slug, doc_id, doc) in pending {
        let mut docs = vec![(None, doc_id, doc)];
        let mut stmt = tx.prepare(
            "SELECT name, doc_id, body FROM project_doc WHERE project = ?1 ORDER BY name",
        )?;
        let named = stmt.query_map(params![slug], |r| {
            Ok((
                Some(r.get::<_, String>(0)?),
                r.get::<_, Option<String>>(1)?,
                r.get::<_, String>(2)?,
            ))
        })?;
        docs.extend(named.collect::<rusqlite::Result<Vec<_>>>()?);
        for (name, doc_id, text) in docs {
            let doc_id = match doc_id {
                Some(text) => ulid("project.doc_id", &text)?,
                None => {
                    let doc_id = Ulid::new();
                    bind_legacy_doc(tx, &mut stamper, &slug, name.as_deref(), doc_id, &text)?;
                    done.ops += usize::from(!text.is_empty());
                    doc_id
                }
            };
            let op = stamper.op(
                view.id,
                Payload::ProjectDocAdd(ProjectDocAdd { name, doc_id }),
            );
            apply_project(&mut view, &op)?;
            append_op(tx, &op)?;
            done.ops += 1;
        }
        record_doc_owners(tx, &view)?;
        tx.execute(
            "UPDATE project_view SET view = ?1 WHERE project = ?2",
            params![json(&view), view.id.to_string()],
        )?;
        done.projects += 1;
    }
    Ok(done)
}

/// Gives a document written without a `doc_id` its new one, and carries
/// its cached text into the log as a `body.edit` (with the matching
/// `project_doc_view`) so a replay reproduces it.
fn bind_legacy_doc(
    tx: &Transaction<'_>,
    stamper: &mut Stamper,
    slug: &str,
    name: Option<&str>,
    doc_id: Ulid,
    text: &str,
) -> Result<()> {
    match name {
        None => tx.execute(
            "UPDATE project SET doc_id = ?1 WHERE id = ?2",
            params![doc_id.to_string(), slug],
        )?,
        Some(name) => tx.execute(
            "UPDATE project_doc SET doc_id = ?1 WHERE project = ?2 AND name = ?3",
            params![doc_id.to_string(), slug, name],
        )?,
    };
    if text.is_empty() {
        return Ok(());
    }
    let update = Body::new().diff_from_text(text)?;
    let op = stamper.op(
        doc_id,
        Payload::BodyEdit(BodyEdit {
            update: update.into_bytes(),
        }),
    );
    let mut view = DocView::new(doc_id);
    apply_doc(&mut view, &op)?;
    append_op(tx, &op)?;
    tx.execute(
        "INSERT INTO project_doc_view (doc_id, view) VALUES (?1, ?2)",
        params![doc_id.to_string(), json(&view)],
    )?;
    Ok(())
}
