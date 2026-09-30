//! Migration 0007's backfill (AGT-1385): one config op per existing
//! workspace field, state, actor and project, so a database written before
//! config was op-logged becomes op-derived with no data change.
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

use pm_core::op::{ProjectCreate, ProjectSet};
use pm_core::{
    ActorId, Hlc, Op, Payload, ProjectStatus, ProjectView, WorkspaceView, apply_project,
    apply_workspace,
};
use rusqlite::{OptionalExtension, Transaction, params};
use ulid::Ulid;

use crate::codec::{enum_from_name, from_json, json};
use crate::commit::{append_op, ensure_actor, now_ms};
use crate::config::{actors, workspace_diff, workspace_row};
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
    let oldest: Option<i64> = tx
        .query_row("SELECT MIN(hlc_wall_ms) FROM ops", [], |r| r.get(0))
        .optional()?
        .flatten();
    let wall_ms = match oldest {
        Some(ms) => (ms as u64).saturating_sub(1),
        None => now_ms(),
    };
    // Read the actors before `migrate` joins them: it is recorded through
    // the ops' own FK, like any actor, not as a backfilled upsert.
    let actor_rows = actors(tx)?;
    let actor = ActorId::new(MIGRATE_ACTOR);
    ensure_actor(tx, &actor)?;
    let mut stamper = Stamper {
        wall_ms,
        counter: 0,
        actor,
    };
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
