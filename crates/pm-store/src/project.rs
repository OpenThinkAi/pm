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
//! The `doc_id`s themselves are bound by config ops since AGT-1413 (the
//! design doc's by `project.create`, a named document's by
//! `project.doc_add`) and materialized onto the rows by `config.rs`; this
//! module only ever writes a document's *text*.
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

use pm_core::op::{BodyEdit, ProjectDocAdd};
use pm_core::{
    ActorId, Body, Clock, DocView, Op, Payload, ProjectKind, ProjectStatus, apply_doc,
    apply_doc_persisted,
};
use rusqlite::{OptionalExtension, Transaction, TransactionBehavior, params};
use ulid::Ulid;

use crate::Store;
use crate::codec::{json, ulid};
use crate::commit::{append_op, ensure_actor, exists, latest_hlc, now_ms};
use crate::config::{commit_payloads, project_ulid, upsert_meta_in};
use crate::error::{Result, StoreError};
use crate::query::read_ops;

impl Store {
    /// `pm project new` (AC1): commits the project's `project.create` —
    /// carrying a fresh design-doc id (AGT-1413) — and one `project.set
    /// repo_add` per repo under `actor`, and returns the design doc's id.
    /// Fails if `id` is already taken, or if `parent` is given and does not
    /// exist (R2, the rule a ticket's `project` field already obeys).
    /// A plain project: [`Store::create_project_of_kind`] names the kind.
    pub fn create_project(
        &mut self,
        id: &str,
        title: &str,
        repos: &BTreeSet<String>,
        parent: Option<&str>,
        actor: &ActorId,
    ) -> Result<Ulid> {
        self.create_project_of_kind(id, title, ProjectKind::Project, repos, parent, actor)
    }

    /// [`Store::create_project`] of `kind` (AGT-1488), which the create op
    /// fixes for good. An initiative cannot have a `parent`
    /// ([`StoreError::InitiativeParent`]).
    pub fn create_project_of_kind(
        &mut self,
        id: &str,
        title: &str,
        kind: ProjectKind,
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
        let doc_id = Ulid::new();
        upsert_meta_in(
            &tx,
            id,
            title,
            kind,
            ProjectStatus::InProgress,
            parent,
            repos,
            Some(doc_id),
            actor,
        )?;
        tx.commit()?;
        Ok(doc_id)
    }

    /// The design doc's stable id (what a `pm project edit` targets), when
    /// the project exists and has one bound. Every project pm writes has
    /// one; only a project pulled from a replica that has not sent its
    /// binding yet reads as `None`.
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
    /// fresh id — a `project.doc_add` op under `actor` (AGT-1413) — and
    /// returns the id; the caller commits the first `body.edit` against it
    /// (mirroring how a new ticket's description is its first `body.edit`,
    /// `pm/src/verbs.rs::build_create_ops`). Fails if the project does not
    /// exist, or `name` is already taken.
    pub fn add_named_doc(&mut self, project: &str, name: &str, actor: &ActorId) -> Result<Ulid> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let Some(project_id) = project_ulid(&tx, project)? else {
            return Err(StoreError::UnknownProject {
                project: project.to_string(),
            });
        };
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
        commit_payloads(
            &tx,
            project_id,
            actor,
            vec![Payload::ProjectDocAdd(ProjectDocAdd {
                name: Some(name.to_string()),
                doc_id,
            })],
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
    /// reads. Refuses, writing nothing, an op that is not a `body.edit`
    /// whose `entity` is `doc_id` ([`StoreError::NotADocumentEdit`],
    /// AGT-1467).
    pub fn commit_doc_edit(&mut self, doc_id: Ulid, op: &Op) -> Result<String> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let text = commit_doc_edit_in(&tx, doc_id, op, crate::commit::Origin::Local)?;
        tx.commit()?;
        Ok(text)
    }

    /// Whether some project has ever bound `doc_id` to one of its
    /// documents (`project_doc_owner`, AGT-1413) — including a binding
    /// that lost to an earlier one and a deleted project's documents. The
    /// op log shares one `entity` namespace between tickets and documents
    /// (both are Ulids); this is how `pm backup --restore` and
    /// `apply_pulled` tell a document's `body.edit` apart from a ticket op
    /// — see [`Store::commit_any`].
    pub fn is_known_doc_id(&self, doc_id: Ulid) -> Result<bool> {
        is_known_doc(&self.conn, doc_id)
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
}

/// Whether a `project` or `project_doc` row shows document `doc_id`.
fn doc_has_row(conn: &rusqlite::Connection, doc_id: Ulid) -> Result<bool> {
    Ok(conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM project WHERE doc_id = ?1)
             OR EXISTS(SELECT 1 FROM project_doc WHERE doc_id = ?1)",
        params![doc_id.to_string()],
        |r| r.get(0),
    )?)
}

/// Whether `doc_id` is bound to any project document, winner or not.
pub(crate) fn is_known_doc(conn: &rusqlite::Connection, doc_id: Ulid) -> Result<bool> {
    Ok(conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM project_doc_owner WHERE doc_id = ?1)",
        params![doc_id.to_string()],
        |r| r.get(0),
    )?)
}

/// Commits the `body.edit` (under `actor`) that takes document `doc_id`
/// from the text its ops say now to `text` — nothing when they already
/// agree. The diff continues the document's own history (its cached
/// snapshot), so it merges with concurrent edits like any other.
pub(crate) fn set_doc_text_in(
    tx: &Transaction<'_>,
    doc_id: Ulid,
    text: &str,
    actor: &ActorId,
) -> Result<()> {
    let view = load_doc_view(tx, doc_id)?;
    if view.as_ref().map(DocView::text).unwrap_or_default() == text {
        return Ok(());
    }
    let mut body = Body::new();
    if let Some(view) = &view {
        body.apply(&view.body.snapshot()?)?;
    }
    let update = body.diff_from_text(text)?;
    let hlc = Clock::from_latest(latest_hlc(tx)?).send(now_ms());
    let op = Op::new(
        Ulid::new(),
        hlc,
        actor.clone(),
        doc_id,
        Payload::BodyEdit(BodyEdit {
            update: update.into_bytes(),
        }),
    );
    commit_doc_edit_in(tx, doc_id, &op, crate::commit::Origin::Local)?;
    Ok(())
}

pub(crate) fn load_doc_view(conn: &rusqlite::Connection, doc_id: Ulid) -> Result<Option<DocView>> {
    let text: Option<String> = conn
        .query_row(
            "SELECT view FROM project_doc_view WHERE doc_id = ?1",
            params![doc_id.to_string()],
            |r| r.get(0),
        )
        .optional()?;
    crate::codec::opt_from_json("project_doc_view.view", text)
}

pub(crate) fn commit_doc_edit_in(
    tx: &Transaction<'_>,
    doc_id: Ulid,
    op: &Op,
    origin: crate::commit::Origin,
) -> Result<String> {
    // AGT-1467: the op is folded into `doc_id`'s view but logged under
    // `op.entity`, and `commit_any` routes any op whose entity is a bound
    // document here — so both must say the same document, and only a
    // `body.edit` belongs in one.
    if op.entity != doc_id || !matches!(op.payload, Payload::BodyEdit(_)) {
        return Err(StoreError::NotADocumentEdit {
            op_id: op.op_id,
            kind: op.kind(),
            entity: op.entity,
            doc_id,
        });
    }
    if exists(
        tx,
        "SELECT 1 FROM ops WHERE op_id = ?1",
        &op.op_id.to_string(),
    )? {
        return Err(StoreError::DuplicateOp { op_id: op.op_id });
    }
    crate::commit::check_ingest(op, origin)?;
    ensure_actor(tx, &op.actor)?;
    if !doc_has_row(tx, doc_id)? {
        if !is_known_doc(tx, doc_id)? {
            return Err(StoreError::UnknownDocument { doc_id });
        }
        // A document bound but shown by no row — a binding that lost to
        // an earlier one, or a deleted project's document (AGT-1413): the
        // edit joins the log, and nothing is materialized for it (the
        // replay rebuilds rows' documents only), so there is no view to
        // fold it into either. Its update must still decode (AGT-1467):
        // it is folded into a scratch view first, so bytes no replica
        // could import never reach the log (or the hub, on the next push).
        apply_doc(&mut DocView::new(doc_id), op)?;
        append_op(tx, op)?;
        return Ok(String::new());
    }
    let mut view = load_doc_view(tx, doc_id)?.unwrap_or_else(|| DocView::new(doc_id));
    // The view is persisted right after: an edit ahead of its history is
    // refused (a pulled one defers), never half-kept.
    apply_doc_persisted(&mut view, op)?;
    append_op(tx, op)?;
    materialize_doc(tx, &view)
}

/// Rewrites `project_doc_view` and whichever `project`/`project_doc` row
/// owns `view.id`. Errors if no row does ([`StoreError::UnknownDocument`]):
/// callers only materialize a row's document.
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
