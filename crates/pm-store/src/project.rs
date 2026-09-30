//! Project documents as op-derived text (AGT-1344).
//!
//! A project's *metadata* (title/status/parent/repos) is config-op
//! derived since AGT-1385 (`config.rs`: `project.create` / `project.set`
//! against the project's own Ulid). Its *document bodies* — the design doc
//! (`project.doc`) and any number of named documents (`project_doc.body`)
//! — each get a stable `doc_id` (a Ulid, distinct from both the project's
//! kebab-case `id` and its Ulid) that `body.edit` ops target, the exact op
//! kind and [`pm_core::Body`] CRDT a ticket's description uses (AGT-1338).
//! There is exactly one body format in the op log, ever.
//!
//! [`commit_doc_edit`] is the write path — append the op, fold it into the
//! document's [`DocView`], and update the cached text column in the same
//! transaction, mirroring [`crate::commit::commit_in`] for tickets.
//! [`replay_project_docs`] is the read-side twin `pm doctor` / `--rebuild`
//! calls (via [`crate::doctor`]'s `replay_all`, after the config and ticket
//! replays): it resets every row with a `doc_id` and refolds it from the
//! `body.edit` ops in the log, so drift there is caught exactly as
//! `ticket.description` drift is.

use std::collections::{BTreeMap, BTreeSet};

use pm_core::{ActorId, DocView, Op, ProjectStatus, apply_doc};
use rusqlite::{OptionalExtension, Transaction, TransactionBehavior, params};
use ulid::Ulid;

use crate::Store;
use crate::codec::{json, ulid};
use crate::commit::{append_op, ensure_actor, exists};
use crate::config::{project_exists, upsert_meta_in};
use crate::error::{Result, StoreError};
use crate::query::read_ops;

impl Store {
    /// `pm project new` (AC1): commits the project's `project.create` (and
    /// one `project.set repo_add` per repo) under `actor`, stamps the new
    /// row with a fresh design-doc id, and returns that id. Fails if `id`
    /// is already taken, or if `parent` is given and does not exist (R2,
    /// the rule a ticket's `project` field already obeys).
    pub fn create_project(
        &mut self,
        id: &str,
        title: &str,
        repos: &BTreeSet<String>,
        parent: Option<&str>,
        actor: &ActorId,
    ) -> Result<Ulid> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if exists(&tx, "SELECT 1 FROM project WHERE id = ?1", id)? {
            return Err(StoreError::DuplicateProject { id: id.to_string() });
        }
        upsert_meta_in(
            &tx,
            id,
            title,
            ProjectStatus::InProgress,
            parent,
            repos,
            actor,
        )?;
        let doc_id = Ulid::new();
        tx.execute(
            "UPDATE project SET doc_id = ?1 WHERE id = ?2",
            params![doc_id.to_string(), id],
        )?;
        tx.commit()?;
        Ok(doc_id)
    }

    /// The design doc's stable id (what a `pm project edit` targets), when
    /// the project exists and has one. A project created before this
    /// ticket (`put_project`, without a `doc_id`) reads as `None` until
    /// `put_project` runs again and assigns one.
    pub fn design_doc_id(&self, project: &str) -> Result<Option<Ulid>> {
        let row: Option<Option<String>> = self
            .conn
            .query_row(
                "SELECT doc_id FROM project WHERE id = ?1",
                params![project],
                |r| r.get(0),
            )
            .optional()?;
        match row.flatten() {
            Some(text) => Ok(Some(ulid("project.doc_id", &text)?)),
            None => Ok(None),
        }
    }

    /// A named document's stable id, when the project and document both
    /// exist and it has one.
    pub fn named_doc_id(&self, project: &str, name: &str) -> Result<Option<Ulid>> {
        let row: Option<Option<String>> = self
            .conn
            .query_row(
                "SELECT doc_id FROM project_doc WHERE project = ?1 AND name = ?2",
                params![project, name],
                |r| r.get(0),
            )
            .optional()?;
        match row.flatten() {
            Some(text) => Ok(Some(ulid("project_doc.doc_id", &text)?)),
            None => Ok(None),
        }
    }

    /// `pm project doc add` (AC3): creates an empty named document with a
    /// fresh id and returns it — the caller commits the first `body.edit`
    /// against it (mirroring how a new ticket's description is its first
    /// `body.edit`, `pm/src/verbs.rs::build_create_ops`). Fails if the
    /// project does not exist, or `name` is already taken.
    pub fn add_named_doc(&mut self, project: &str, name: &str) -> Result<Ulid> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if !project_exists(&tx, project)? {
            return Err(StoreError::UnknownProject {
                project: project.to_string(),
            });
        }
        let taken: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM project_doc WHERE project = ?1 AND name = ?2)",
            params![project, name],
            |r| r.get(0),
        )?;
        if taken {
            return Err(StoreError::DuplicateDocument {
                project: project.to_string(),
                name: name.to_string(),
            });
        }
        let doc_id = Ulid::new();
        tx.execute(
            "INSERT INTO project_doc (project, name, body, doc_id) VALUES (?1, ?2, '', ?3)",
            params![project, name, doc_id.to_string()],
        )?;
        tx.commit()?;
        Ok(doc_id)
    }

    /// The document's merge state — what a caller needs before it can
    /// produce the next minimal `body.edit` diff (load the cached snapshot,
    /// import it into a fresh [`pm_core::Body`], then `diff_from_text`).
    pub fn doc_view(&self, doc_id: Ulid) -> Result<Option<DocView>> {
        load_doc_view(&self.conn, doc_id)
    }

    /// The latest `body.edit` stamp across a project's design doc and every
    /// named document — "no doc edit in `stale_days`" (AGT-1351 AC1).
    /// `None` means the project has no document that has ever been edited
    /// through a `body.edit` op (including one imported via
    /// [`Store::put_project`], which never assigns a `doc_id` at all), so
    /// there is no recency to measure — a caller treating that as "stale"
    /// (`pm_core::archive::project_idle`) is deliberate, not a gap.
    pub fn project_doc_last_edit(&self, project: &str) -> Result<Option<pm_core::Hlc>> {
        let mut doc_ids: Vec<Ulid> = Vec::new();
        if let Some(id) = self.design_doc_id(project)? {
            doc_ids.push(id);
        }
        let names: Vec<String> = self
            .conn
            .prepare("SELECT name FROM project_doc WHERE project = ?1")?
            .query_map(params![project], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        for name in names {
            if let Some(id) = self.named_doc_id(project, &name)? {
                doc_ids.push(id);
            }
        }
        let mut latest: Option<pm_core::Hlc> = None;
        for doc_id in doc_ids {
            if let Some(stamp) = self.doc_view(doc_id)?.and_then(|v| v.updated) {
                latest = Some(latest.map_or(stamp.hlc, |l| l.max(stamp.hlc)));
            }
        }
        Ok(latest)
    }

    /// Appends a `body.edit` op and re-materializes the document it
    /// targets, in one transaction — the document analogue of
    /// [`crate::Store::commit`]. Returns the document's text as it now
    /// reads.
    pub fn commit_doc_edit(&mut self, doc_id: Ulid, op: &Op) -> Result<String> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let text = commit_doc_edit_in(&tx, doc_id, op)?;
        tx.commit()?;
        Ok(text)
    }

    /// `pm project delete` (AC4): refused while a ticket still references
    /// the project (R-style FK) or a child project still names it as
    /// `parent`; otherwise removes the project, its documents, and their
    /// cached merge state (`project_view` too). The op log itself is never
    /// pruned — a `body.edit` for a deleted document's `doc_id` simply has
    /// nothing left to materialize into, and the project's own config ops
    /// are skipped by a rebuild (`config.rs`, `Mode::Rebuild`).
    pub fn delete_project(&mut self, id: &str) -> Result<()> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let has_tickets: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM ticket WHERE project = ?1)",
            params![id],
            |r| r.get(0),
        )?;
        if has_tickets {
            return Err(StoreError::ProjectHasTickets {
                project: id.to_string(),
            });
        }
        let has_children: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM project WHERE parent = ?1)",
            params![id],
            |r| r.get(0),
        )?;
        if has_children {
            return Err(StoreError::ProjectHasChildren {
                project: id.to_string(),
            });
        }
        tx.execute(
            "DELETE FROM project_doc_view WHERE doc_id IN
                (SELECT doc_id FROM project_doc WHERE project = ?1 AND doc_id IS NOT NULL
                 UNION SELECT doc_id FROM project WHERE id = ?1 AND doc_id IS NOT NULL)",
            params![id],
        )?;
        tx.execute("DELETE FROM project_doc WHERE project = ?1", params![id])?;
        tx.execute(
            "DELETE FROM project_view WHERE project IN
                (SELECT ulid FROM project WHERE id = ?1 AND ulid IS NOT NULL)",
            params![id],
        )?;
        tx.execute("DELETE FROM project WHERE id = ?1", params![id])?;
        tx.commit()?;
        Ok(())
    }

    /// Whether `doc_id` belongs to some project's design doc or a named
    /// document. The op log shares one `entity` namespace between tickets
    /// and documents (both are Ulids); this is how `pm backup --restore`
    /// tells a document's `body.edit` apart from a ticket op when replaying
    /// a log that interleaves both without otherwise knowing which is
    /// which — see [`Store::commit_any`].
    pub fn is_known_doc_id(&self, doc_id: Ulid) -> Result<bool> {
        let text = doc_id.to_string();
        let in_project: bool = self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM project WHERE doc_id = ?1)",
            params![text],
            |r| r.get(0),
        )?;
        if in_project {
            return Ok(true);
        }
        Ok(self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM project_doc WHERE doc_id = ?1)",
            params![text],
            |r| r.get(0),
        )?)
    }

    /// Commits `op` against whichever entity it targets: a ticket
    /// ([`Store::commit`]) or a project document
    /// ([`Store::commit_doc_edit`]), told apart by [`Store::is_known_doc_id`].
    /// `pm backup --restore` replays a JSONL log of tickets and document
    /// edits interleaved, with no other way to tell which is which — every
    /// other writer (the CLI's own verbs) already knows and calls the
    /// specific method directly.
    pub fn commit_any(&mut self, op: &Op) -> Result<()> {
        if self.is_known_doc_id(op.entity)? {
            self.commit_doc_edit(op.entity, op)?;
        } else {
            self.commit(op)?;
        }
        Ok(())
    }

    /// Restores a design doc's original `doc_id` onto an existing project
    /// row (`pm backup --restore`'s counterpart to [`Store::create_project`],
    /// which always mints a fresh one) — so the log's `body.edit` ops for
    /// it, replayed afterward via [`Store::commit_any`], still find their
    /// row instead of being mistaken for ticket ops.
    pub fn set_design_doc_id(&mut self, project: &str, doc_id: Ulid) -> Result<()> {
        let n = self.conn.execute(
            "UPDATE project SET doc_id = ?1 WHERE id = ?2",
            params![doc_id.to_string(), project],
        )?;
        if n == 0 {
            return Err(StoreError::UnknownProject {
                project: project.to_string(),
            });
        }
        Ok(())
    }

    /// Restores a named document's original `doc_id` (`pm backup
    /// --restore`'s counterpart to [`Store::add_named_doc`]). The row
    /// itself is expected to already exist (`put_project`'s `documents`
    /// map creates it with the restored body text but no `doc_id`); if it
    /// somehow does not, this creates it empty rather than fail restore
    /// over a document whose body a later op will fill in anyway.
    pub fn set_named_doc_id(&mut self, project: &str, name: &str, doc_id: Ulid) -> Result<()> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if !project_exists(&tx, project)? {
            return Err(StoreError::UnknownProject {
                project: project.to_string(),
            });
        }
        tx.execute(
            "INSERT INTO project_doc (project, name, body, doc_id) VALUES (?1, ?2, '', ?3)
             ON CONFLICT(project, name) DO UPDATE SET doc_id = excluded.doc_id",
            params![project, name, doc_id.to_string()],
        )?;
        tx.commit()?;
        Ok(())
    }
}

fn load_doc_view(conn: &rusqlite::Connection, doc_id: Ulid) -> Result<Option<DocView>> {
    let text: Option<String> = conn
        .query_row(
            "SELECT view FROM project_doc_view WHERE doc_id = ?1",
            params![doc_id.to_string()],
            |r| r.get(0),
        )
        .optional()?;
    crate::codec::opt_from_json("project_doc_view.view", text)
}

pub(crate) fn commit_doc_edit_in(tx: &Transaction<'_>, doc_id: Ulid, op: &Op) -> Result<String> {
    if exists(
        tx,
        "SELECT 1 FROM ops WHERE op_id = ?1",
        &op.op_id.to_string(),
    )? {
        return Err(StoreError::DuplicateOp { op_id: op.op_id });
    }
    ensure_actor(tx, &op.actor)?;
    let mut view = load_doc_view(tx, doc_id)?.unwrap_or_else(|| DocView::new(doc_id));
    apply_doc(&mut view, op)?;
    append_op(tx, op)?;
    materialize_doc(tx, &view)
}

/// Rewrites `project_doc_view` and whichever `project`/`project_doc` row
/// owns `view.id`. Errors if `view.id` belongs to neither — only a foreign
/// writer or a schema bug produces that, since `body.edit` op entities are
/// always a `doc_id` this store itself assigned.
fn materialize_doc(tx: &Transaction<'_>, view: &DocView) -> Result<String> {
    let doc_id = view.id;
    let text = view.text();
    let owning_project: Option<String> = tx
        .query_row(
            "SELECT id FROM project WHERE doc_id = ?1",
            params![doc_id.to_string()],
            |r| r.get(0),
        )
        .optional()?;
    match owning_project {
        Some(project) => {
            tx.execute(
                "UPDATE project SET doc = ?1 WHERE id = ?2",
                params![text, project],
            )?;
        }
        None => {
            let owner: Option<(String, String)> = tx
                .query_row(
                    "SELECT project, name FROM project_doc WHERE doc_id = ?1",
                    params![doc_id.to_string()],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()?;
            match owner {
                Some((project, name)) => {
                    tx.execute(
                        "UPDATE project_doc SET body = ?1 WHERE project = ?2 AND name = ?3",
                        params![text, project, name],
                    )?;
                }
                None => return Err(StoreError::UnknownDocument { doc_id }),
            }
        }
    }
    tx.execute(
        "INSERT INTO project_doc_view (doc_id, view) VALUES (?1, ?2)
         ON CONFLICT(doc_id) DO UPDATE SET view = excluded.view",
        params![doc_id.to_string(), json(view)],
    )?;
    Ok(text)
}

/// Every doc_id this replica knows about — every `project.doc_id` and
/// `project_doc.doc_id` that is not NULL. The op log shares one `entity`
/// namespace between tickets and documents (both are Ulids), so this is
/// how a reader tells a document's `body.edit` apart from a ticket's: an
/// entity in this set is a document, never a ticket ([`crate::doctor`]'s
/// ticket replay excludes it for the same reason it excludes markers or
/// relations belonging to a different ticket).
pub(crate) fn known_doc_ids(tx: &Transaction<'_>) -> Result<BTreeSet<Ulid>> {
    let mut known = BTreeSet::new();
    let mut stmt = tx.prepare("SELECT doc_id FROM project WHERE doc_id IS NOT NULL")?;
    for row in stmt.query_map([], |r| r.get::<_, String>(0))? {
        known.insert(ulid("project.doc_id", &row?)?);
    }
    let mut stmt = tx.prepare("SELECT doc_id FROM project_doc WHERE doc_id IS NOT NULL")?;
    for row in stmt.query_map([], |r| r.get::<_, String>(0))? {
        known.insert(ulid("project_doc.doc_id", &row?)?);
    }
    Ok(known)
}

/// The read side of this module: resets every document row that has a
/// `doc_id` and refolds it from the `body.edit` ops in the log, exactly the
/// way [`crate::doctor`]'s `replay_all` does for
/// [`crate::doctor::TICKET_TABLES`]. Called from `replay_all` itself (which
/// snapshots [`crate::doctor::PROJECT_DOC_TABLES`] before and after, with
/// every other derived table), so both `pm doctor` and `pm doctor
/// --rebuild` pick it up.
pub(crate) fn replay_project_docs(tx: &Transaction<'_>) -> Result<()> {
    let known = known_doc_ids(tx)?;

    tx.execute("DELETE FROM project_doc_view", [])?;
    tx.execute("UPDATE project SET doc = '' WHERE doc_id IS NOT NULL", [])?;
    tx.execute(
        "UPDATE project_doc SET body = '' WHERE doc_id IS NOT NULL",
        [],
    )?;

    let mut views: BTreeMap<Ulid, DocView> = BTreeMap::new();
    for (seq, op) in read_ops(tx, "WHERE kind = ?1", params!["body.edit"])? {
        if !known.contains(&op.entity) {
            continue; // a ticket's body.edit, or an orphaned doc_id
        }
        let view = views
            .entry(op.entity)
            .or_insert_with(|| DocView::new(op.entity));
        apply_doc(view, &op).map_err(|source| StoreError::Replay {
            seq,
            op_id: op.op_id,
            kind: "body.edit",
            source: Box::new(StoreError::DocApply(source)),
        })?;
    }
    for view in views.values() {
        materialize_doc(tx, view)?;
    }
    Ok(())
}
