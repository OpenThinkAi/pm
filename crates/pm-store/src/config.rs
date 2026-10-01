//! The configuration tables — workspace + states + actors, and a project's
//! metadata — materialized from config ops (AGT-1385; README §Sync & hub
//! "Config must become ops", decision A4).
//!
//! Until AGT-1385 these were direct table writes, which push/pull could not
//! carry. Now they have the shape the ticket tables have: a config op
//! (`workspace.set`, `state.upsert`, `actor.upsert`, `project.create`,
//! `project.set`, `project.delete`, `project.doc_add`; AGT-1384, AGT-1386,
//! AGT-1413) is appended to the log and the rows it affects
//! are rewritten in the same transaction ([`commit_config_in`], reached
//! through [`Store::commit`]); the merge state lives in `workspace_view` /
//! `project_view` (a [`WorkspaceView`] / [`ProjectView`] as JSON, the
//! analogue of `ticket_view`) so the CRDT rules run once, in pm-core; and
//! `pm doctor --rebuild` regenerates the rows by replaying the same ops
//! through the same code ([`replay_config`]).
//!
//! The public writers ([`Store::init_workspace`], [`Store::put_project`],
//! [`Store::set_gate_labels`], [`Store::set_project_status`], and
//! `project.rs`/`import.rs`'s `create_project` / `upsert_project`) take the
//! *target* state and an actor: they diff it against the current view
//! ([`workspace_diff`], [`project_diff`]) and commit exactly the ops that
//! close the gap — an unchanged field emits nothing, so re-applying a
//! snapshot (`pm backup --restore` over a log that already carries its
//! config ops) is a no-op on the log.
//!
//! Document identity is op-derived too (AGT-1413): `project.doc_id` and
//! the `project_doc` rows (name → `doc_id`) are materialized from the
//! project's view — a `project.create`'s `doc_id` and `project.doc_add`
//! ops, earliest binding per document ([`pm_core::DocClaims`]) — together
//! with `project_doc_owner`, every `doc_id` ever bound (winner or not).
//! The *text* of a document (`project.doc`, `project_doc.body`) is the
//! `body.edit` fold `project.rs` materializes under that id (AGT-1344); a
//! newly bound document starts from whatever its own view holds.
//!
//! What is *not* op-derived, deliberately:
//! - `workspace.number_floor` — allocator bookkeeping (`import.rs`), left
//!   alone by materialization and rebuild, like `backup_target`.
//!
//! A project's existence *is* op-derived (AGT-1386): `project.delete`
//! tombstones its [`ProjectView`] (`deleted_at`, permanent), and
//! materializing a tombstoned view removes the project row, its named
//! documents and their cached merge state while keeping the view itself —
//! so a `project.set` that syncs in later folds without resurrecting a
//! row, and a rebuild reproduces the deletion instead of needing to
//! remember it. A *live* commit (not a rebuild, not a pull) refuses the
//! deletes `pm project delete` refuses: a ticket still in the project, or
//! a child project naming it as `parent` (R2). A pulled delete applies
//! regardless (AGT-1464, see [`Mode`]): what still names the project keeps
//! the name in its view and reads `NULL` on its row, and `pm check`
//! reports the tickets. A database that deleted a project
//! before this kind existed has no tombstone for it; a rebuild re-creates
//! that row (without documents) from its `project.create`.
//! - `actor` rows are never deleted (`ops.actor` references them) and an
//!   actor's first appearance is [`crate::commit::ensure_actor`]'s
//!   `INSERT OR IGNORE` from the op that names it — the kind an
//!   [`pm_core::ActorId`] implies. `actor.upsert` is the explicit write of
//!   a kind; both paths run again on replay, in the same order, so the
//!   table comes out the same.

use std::collections::{BTreeMap, BTreeSet};

use pm_core::op::{
    ActorUpsert, ProjectCreate, ProjectDocAdd, ProjectSet, StateUpsert, WorkspaceSet,
};
use pm_core::{
    ActorId, ActorKind, Clock, DocsOwner, Op, Payload, Project, ProjectStatus, ProjectView, State,
    Workspace, WorkspaceView, apply_project, apply_workspace,
};
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use ulid::Ulid;

use crate::Store;
use crate::codec::{enum_from_name, enum_name, from_json, json, opt_from_json, ulid};
use crate::commit::{append_op, ensure_actor, exists, latest_hlc, now_ms};
use crate::error::{Result, StoreError};
use crate::project::set_doc_text_in;
use crate::query::read_ops;

/// The config kinds, as `ops.kind` spells them — what [`replay_config`]
/// selects and what `apply_pulled` routes here.
pub(crate) const CONFIG_KINDS: &str = "'workspace.set', 'state.upsert', 'actor.upsert', 'project.create', 'project.set', 'project.delete', 'project.doc_add'";

/// The document ids a writer wants a project's documents to have when it
/// binds them ([`Store::put_project_with_doc_ids`]): `pm backup --restore`
/// passes the ids a backup's `body.edit` ops target, so a log written
/// before those ids were op-carried (AGT-1413) still finds its documents.
/// A document that already has an id keeps it; one not listed gets a
/// fresh id.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DocIds {
    pub design: Option<Ulid>,
    pub named: BTreeMap<String, Ulid>,
}

impl Store {
    /// Brings the workspace and its states to `ws` by committing config
    /// ops under `actor` — every field on a fresh database, only the
    /// changed ones on an existing workspace (see [`workspace_diff`]). A
    /// state absent from `ws.states` is left in place: there is no
    /// `state.remove` kind, and a state tickets still reference could not
    /// go anyway (foreign key). `ws.id` must be this database's workspace
    /// id if it already has one ([`StoreError::ForeignWorkspace`]).
    ///
    /// Returns how many ops were committed.
    pub fn init_workspace(&mut self, ws: &Workspace, actor: &ActorId) -> Result<usize> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        check_workspace_id(&tx, ws.id)?;
        let current = load_workspace_view(&tx, ws.id)?;
        let payloads = workspace_diff(current.as_ref(), ws);
        let n = commit_payloads(&tx, ws.id, actor, payloads)?;
        tx.commit()?;
        Ok(n)
    }

    /// The workspace's merge state, if any config op has landed.
    pub fn workspace_view(&self) -> Result<Option<WorkspaceView>> {
        let id: Option<String> = self
            .conn
            .query_row("SELECT workspace FROM workspace_view", [], |r| r.get(0))
            .optional()?;
        match id {
            Some(id) => load_workspace_view(&self.conn, ulid("workspace_view.workspace", &id)?),
            None => Ok(None),
        }
    }

    pub fn workspace(&self) -> Result<Option<Workspace>> {
        workspace_row(&self.conn)
    }

    /// Brings a whole [`Project`] to `project` with ops under `actor`:
    /// its metadata (title, status, parent, repos) with config ops,
    /// creating the project if `project.id` is new; its design doc and
    /// every named document in `project.documents` bound to a `doc_id`
    /// (`project.create`'s `doc_id`, `project.doc_add`) when not yet; and
    /// each document's text with a `body.edit` when it differs from what
    /// the document's ops already say. A document the project has but
    /// `project.documents` does not list is left alone (there is no
    /// document-remove kind). Used by tests that construct a project at
    /// once; `pm backup --restore` uses
    /// [`Store::put_project_with_doc_ids`]. A `parent` must already exist
    /// (R2).
    ///
    /// Returns the project's Ulid (the entity its config ops target).
    pub fn put_project(&mut self, project: &Project, actor: &ActorId) -> Result<Ulid> {
        self.put_project_with_doc_ids(project, &DocIds::default(), actor)
    }

    /// [`Store::put_project`], binding each not-yet-bound document to the
    /// id `ids` lists for it (see [`DocIds`]); a name only in `ids.named`
    /// is bound (empty) too.
    pub fn put_project_with_doc_ids(
        &mut self,
        project: &Project,
        ids: &DocIds,
        actor: &ActorId,
    ) -> Result<Ulid> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let ulid = upsert_meta_in(
            &tx,
            &project.id,
            &project.title,
            project.status,
            project.parent.as_deref(),
            &project.repos,
            ids.design,
            actor,
        )?;
        let view = load_project_view(&tx, ulid)?.unwrap_or_else(|| ProjectView::new(ulid));
        let names: BTreeSet<&String> = project.documents.keys().chain(ids.named.keys()).collect();
        let adds: Vec<Payload> = names
            .into_iter()
            .filter(|name| view.doc_id(name).is_none())
            .map(|name| {
                Payload::ProjectDocAdd(ProjectDocAdd {
                    name: Some(name.clone()),
                    doc_id: ids.named.get(name).copied().unwrap_or_else(fresh_id),
                })
            })
            .collect();
        commit_payloads(&tx, ulid, actor, adds)?;
        let view = load_project_view(&tx, ulid)?.unwrap_or_else(|| ProjectView::new(ulid));
        if let Some(doc_id) = view.design_doc_id() {
            set_doc_text_in(&tx, doc_id, &project.doc, actor)?;
        }
        for (name, body) in &project.documents {
            if let Some(doc_id) = view.doc_id(name) {
                set_doc_text_in(&tx, doc_id, body, actor)?;
            }
        }
        tx.commit()?;
        Ok(ulid)
    }

    pub fn project(&self, id: &str) -> Result<Option<Project>> {
        let mut found = load_projects(&self.conn, "WHERE id = ?1", params![id])?;
        Ok(found.pop())
    }

    /// A project's merge state, by its kebab-case id, if it exists.
    pub fn project_view(&self, id: &str) -> Result<Option<ProjectView>> {
        match project_ulid(&self.conn, id)? {
            Some(ulid) => load_project_view(&self.conn, ulid),
            None => Ok(None),
        }
    }

    /// Sets a project's `status` with a `project.set` op under `actor`
    /// (nothing is committed when it already is `status`), leaving
    /// everything else untouched. Used by `pm archive --auto` to retire an
    /// idle project to `complete` (AGT-1351 AC1).
    pub fn set_project_status(
        &mut self,
        id: &str,
        status: ProjectStatus,
        actor: &ActorId,
    ) -> Result<()> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let ulid = project_ulid(&tx, id)?.ok_or_else(|| StoreError::UnknownProject {
            project: id.to_string(),
        })?;
        let view = load_project_view(&tx, ulid)?.unwrap_or_else(|| ProjectView::new(ulid));
        if view.status.stamp.is_none() || view.status.value != status {
            commit_payloads(
                &tx,
                ulid,
                actor,
                vec![Payload::ProjectSet(ProjectSet::Status(status))],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// `pm project set` (AGT-1489): sets any of a project's `title`,
    /// `status` and `parent` (`Some(None)` clears it) with one `project.set`
    /// op per field that actually changes, all in one transaction, under
    /// `actor`; `None` leaves a field alone. A new parent must exist
    /// ([`StoreError::UnknownProject`]) and must not be the project itself
    /// or one of its descendants ([`StoreError::ProjectCycle`]); either
    /// refusal commits nothing.
    pub fn set_project(
        &mut self,
        id: &str,
        title: Option<&str>,
        status: Option<ProjectStatus>,
        parent: Option<Option<&str>>,
        actor: &ActorId,
    ) -> Result<()> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let ulid = project_ulid(&tx, id)?.ok_or_else(|| StoreError::UnknownProject {
            project: id.to_string(),
        })?;
        if let Some(Some(parent)) = parent {
            if !project_exists(&tx, parent)? {
                return Err(StoreError::UnknownProject {
                    project: parent.to_string(),
                });
            }
            check_parent_cycle(&tx, id, parent)?;
        }
        let view = load_project_view(&tx, ulid)?.unwrap_or_else(|| ProjectView::new(ulid));
        let mut payloads = Vec::new();
        if let Some(title) = title
            && (view.title.stamp.is_none() || view.title.value != title)
        {
            payloads.push(ProjectSet::Title(title.to_string()));
        }
        if let Some(status) = status
            && (view.status.stamp.is_none() || view.status.value != status)
        {
            payloads.push(ProjectSet::Status(status));
        }
        if let Some(parent) = parent
            && (view.parent.stamp.is_none() || view.parent.value.as_deref() != parent)
        {
            payloads.push(ProjectSet::Parent(parent.map(str::to_string)));
        }
        let payloads = payloads.into_iter().map(Payload::ProjectSet).collect();
        commit_payloads(&tx, ulid, actor, payloads)?;
        tx.commit()?;
        Ok(())
    }

    /// `pm project delete` (AC4): commits a `project.delete` op under
    /// `actor`, whose materialization removes the project, its documents
    /// and their cached merge state (see [`remove_project_rows`]). Refused
    /// while a ticket still references the project or a child project
    /// still names it as `parent`, and for an unknown project; deleting
    /// one already deleted is refused the same way (its row is gone). The
    /// op log itself is never pruned — a `body.edit` for a deleted
    /// document's `doc_id` simply has nothing left to materialize into.
    pub fn delete_project(&mut self, id: &str, actor: &ActorId) -> Result<()> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let ulid = project_ulid(&tx, id)?.ok_or_else(|| StoreError::UnknownProject {
            project: id.to_string(),
        })?;
        commit_payloads(&tx, ulid, actor, vec![Payload::ProjectDelete])?;
        tx.commit()?;
        Ok(())
    }

    /// Every project, by id.
    pub fn projects(&self) -> Result<Vec<Project>> {
        load_projects(&self.conn, "", [])
    }

    /// `workspace.set docs_owned_by` (AGT-1406) and nothing else: who owns
    /// project design docs. A no-op (no op committed) when already `owner`.
    pub fn set_docs_owned_by(&mut self, owner: DocsOwner, actor: &ActorId) -> Result<()> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let id: Option<String> = tx
            .query_row("SELECT workspace FROM workspace_view", [], |r| r.get(0))
            .optional()?;
        let Some(id) = id else {
            return Err(StoreError::NoWorkspace);
        };
        let id = ulid("workspace_view.workspace", &id)?;
        let view = load_workspace_view(&tx, id)?.ok_or(StoreError::NoWorkspace)?;
        let mut target = view.snapshot();
        target.docs_owned_by = owner;
        let payloads = workspace_diff(Some(&view), &target);
        commit_payloads(&tx, id, actor, payloads)?;
        tx.commit()?;
        Ok(())
    }

    /// Sets the workspace's gate labels to exactly `labels` with
    /// `workspace.set gate_label_add` / `gate_label_remove` ops under
    /// `actor` (AGT-1380: `pm workspace gate-label add|remove`), leaving
    /// everything else (prefix, states, model labels, template sections,
    /// stale days) untouched. A remove cites the add-tags this replica has
    /// observed, so a concurrent re-add elsewhere survives (OR-set).
    pub fn set_gate_labels(&mut self, labels: &BTreeSet<String>, actor: &ActorId) -> Result<()> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let id: Option<String> = tx
            .query_row("SELECT workspace FROM workspace_view", [], |r| r.get(0))
            .optional()?;
        let Some(id) = id else {
            return Err(StoreError::NoWorkspace);
        };
        let id = ulid("workspace_view.workspace", &id)?;
        let view = load_workspace_view(&tx, id)?.ok_or(StoreError::NoWorkspace)?;
        let mut target = view.snapshot();
        target.gate_labels = labels.clone();
        let payloads = workspace_diff(Some(&view), &target);
        commit_payloads(&tx, id, actor, payloads)?;
        tx.commit()?;
        Ok(())
    }
}

// ------------------------------------------------------------ commit path

/// Whether a materialization is a live commit (which enforces R2 on a
/// `project.delete`), a pulled op, or a replay of ops already in the log
/// (a rebuild empties the ticket tables first and must reproduce whatever
/// the log says).
///
/// A pulled `project.delete` is not refused for R2 (AGT-1464): the
/// tombstone is already a fact on the replica that sent it, and refusing
/// it would fail every retry of the pulled batch for good. It folds as a
/// replay's does, and whatever still points at the project — this
/// replica's tickets filed in it, a child project naming it as `parent` —
/// keeps the reference in its view while its row reads `NULL` (see
/// [`remove_project_rows`]); `pm check` reports those tickets
/// (`deleted-project`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Mode {
    Commit,
    Foreign,
    Rebuild,
}

/// The view a config op folds into.
enum ConfigView {
    Workspace(WorkspaceView),
    Project(ProjectView),
}

/// Appends a config op and rewrites the rows it affects — the config
/// twin of [`crate::commit::commit_in`]'s ensure-actor → apply → append
/// → materialize. `between` runs after the append and before the rows are
/// rewritten (tests inject a failure there to prove neither half lands on
/// its own). The caller has ruled out a duplicate op id.
pub(crate) fn commit_config_in(
    tx: &Transaction<'_>,
    op: &Op,
    mode: Mode,
    between: impl FnOnce() -> Result<()>,
) -> Result<()> {
    let origin = match mode {
        Mode::Foreign => crate::commit::Origin::Pulled,
        Mode::Commit | Mode::Rebuild => crate::commit::Origin::Local,
    };
    crate::commit::check_ingest(op, origin)?;
    check_config_admission(tx, op)?;
    ensure_actor(tx, &op.actor)?;
    let view = next_view(tx, op)?;
    append_op(tx, op)?;
    between()?;
    materialize(tx, &view, mode)
}

/// What a config op must satisfy against the log before it is admitted
/// (AGT-1464), on every commit path — local, pulled or restored; a replay
/// of the log does not re-check:
/// - a `project.create` for a project that already has one is refused
///   ([`StoreError::DuplicateProjectCreate`]): a second, backdated create
///   would move the stamp document identity is anchored to
///   ([`pm_core::DocClaims`]), and no writer ever sends one;
/// - a document binding (`project.create`'s `doc_id`, `project.doc_add`)
///   whose `doc_id` is already a ticket or a project is refused
///   ([`StoreError::EntityInUse`]) — the mirror of
///   [`crate::commit::check_ticket_entity`].
///
/// Both are admission rules, not merge rules: the hub refuses the same
/// ops at its push (pm-hub `views`), so its log — what every replica
/// pulls — never holds one, and no replica has to pick between two.
pub(crate) fn check_config_admission(tx: &Transaction<'_>, op: &Op) -> Result<()> {
    let doc_id = match &op.payload {
        Payload::ProjectCreate(create) => {
            if exists(
                tx,
                "SELECT 1 FROM ops WHERE entity = ?1 AND kind = 'project.create'",
                &op.entity.to_string(),
            )? {
                return Err(StoreError::DuplicateProjectCreate { project: op.entity });
            }
            create.doc_id
        }
        Payload::ProjectDocAdd(add) => Some(add.doc_id),
        _ => None,
    };
    let Some(doc_id) = doc_id else {
        return Ok(());
    };
    let id = doc_id.to_string();
    let holder = if exists(
        tx,
        "SELECT 1 FROM ops WHERE entity = ?1 AND kind = 'ticket.create'",
        &id,
    )? {
        "a ticket"
    } else if doc_id == op.entity
        || exists(tx, "SELECT 1 FROM project_view WHERE project = ?1", &id)?
    {
        "a project"
    } else {
        return Ok(());
    };
    Err(StoreError::EntityInUse {
        entity: doc_id,
        holder,
    })
}

/// Re-applies a config op already in the log — load → apply →
/// materialize without the append — the way `commit::replay_in` does for
/// a ticket op.
fn replay_config_in(tx: &Transaction<'_>, op: &Op) -> Result<()> {
    let view = next_view(tx, op)?;
    materialize(tx, &view, Mode::Rebuild)
}

/// The config replay `pm doctor` / `--rebuild` run first (before the
/// ticket replay, which needs the states and projects to exist): every
/// config op in `seq` order against emptied `state`, `workspace_view` and
/// `project_view` tables. The caller empties them; this only refolds.
pub(crate) fn replay_config(tx: &Transaction<'_>) -> Result<()> {
    for (seq, op) in read_ops(tx, &format!("WHERE kind IN ({CONFIG_KINDS})"), [])? {
        replay_config_in(tx, &op).map_err(|source| StoreError::Replay {
            seq,
            op_id: op.op_id,
            kind: op.kind(),
            source: Box::new(source),
        })?;
    }
    check_replayed_projects(tx)
}

/// After a config replay, every live (not tombstoned) project view must
/// have its row: the one that does not lost its slug to another identity
/// ([`StoreError::DuplicateProject`], deferred from
/// [`materialize_project`]'s rebuild path).
fn check_replayed_projects(tx: &Transaction<'_>) -> Result<()> {
    let mut stmt = tx.prepare("SELECT project, view FROM project_view")?;
    let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
    for row in rows {
        let (_, text) = row?;
        let view: ProjectView = from_json("project_view.view", &text)?;
        if view.deleted_at.is_none() && view.created.is_some() && !project_row_exists(tx, view.id)?
        {
            return Err(StoreError::DuplicateProject {
                id: view.slug.value,
            });
        }
    }
    Ok(())
}

/// The entity's view with `op` folded in.
fn next_view(tx: &Transaction<'_>, op: &Op) -> Result<ConfigView> {
    match &op.payload {
        Payload::WorkspaceSet(_) | Payload::StateUpsert(_) | Payload::ActorUpsert(_) => {
            check_workspace_id(tx, op.entity)?;
            let mut view = load_workspace_view(tx, op.entity)?
                .unwrap_or_else(|| WorkspaceView::new(op.entity));
            apply_workspace(&mut view, op)?;
            Ok(ConfigView::Workspace(view))
        }
        Payload::ProjectCreate(_)
        | Payload::ProjectSet(_)
        | Payload::ProjectDelete
        | Payload::ProjectDocAdd(_) => {
            let mut view = match load_project_view(tx, op.entity)? {
                Some(view) => view,
                // A delete needs no slug, so it may fold ahead of its
                // create (the tombstone then holds when the create lands).
                None if matches!(
                    op.payload,
                    Payload::ProjectCreate(_) | Payload::ProjectDelete
                ) =>
                {
                    ProjectView::new(op.entity)
                }
                // A `project.set` or `project.doc_add` ahead of its
                // `project.create`: the view could fold it (pm-core allows
                // it), but a row needs the slug the create carries — a
                // pull parks it and retries it once a config op lands
                // (`sync::disposition`).
                None => {
                    return Err(StoreError::UnknownProjectEntity { project: op.entity });
                }
            };
            apply_project(&mut view, op)?;
            Ok(ConfigView::Project(view))
        }
        other => Err(StoreError::NotAConfigOp {
            op_id: op.op_id,
            kind: other.kind(),
        }),
    }
}

fn materialize(tx: &Transaction<'_>, view: &ConfigView, mode: Mode) -> Result<()> {
    match view {
        ConfigView::Workspace(view) => materialize_workspace(tx, view),
        ConfigView::Project(view) => materialize_project(tx, view, mode),
    }
}

/// Rewrites the `workspace` row (all but `number_floor`), upserts every
/// state and actor the view holds, and saves the view. The `workspace`
/// row waits for a prefix: `prefix` is NOT NULL and non-empty, so until a
/// `workspace.set prefix` has folded in (a batch may deliver a
/// `state.upsert` first) the states and actors land but the workspace
/// itself does not exist yet ([`Store::workspace`] is `None`).
fn materialize_workspace(tx: &Transaction<'_>, view: &WorkspaceView) -> Result<()> {
    let ws = view.snapshot();
    if view.prefix.stamp.is_some() {
        tx.execute(
            "INSERT INTO workspace (singleton, id, prefix, gate_labels, model_labels, template_sections, stale_days, docs_owned_by)
             VALUES (1, ?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(singleton) DO UPDATE SET
               id = excluded.id, prefix = excluded.prefix, gate_labels = excluded.gate_labels,
               model_labels = excluded.model_labels, template_sections = excluded.template_sections,
               stale_days = excluded.stale_days, docs_owned_by = excluded.docs_owned_by",
            params![
                ws.id.to_string(),
                ws.prefix,
                json(&ws.gate_labels),
                json(&ws.model_labels),
                json(&ws.template_sections),
                ws.stale_days,
                ws.docs_owned_by.as_str(),
            ],
        )?;
    }
    for state in &ws.states {
        tx.execute(
            "INSERT INTO state (name, category, position) VALUES (?1, ?2, ?3)
             ON CONFLICT(name) DO UPDATE SET category = excluded.category, position = excluded.position",
            params![state.name, enum_name(&state.category), state.position],
        )?;
    }
    for actor in view.actors() {
        tx.execute(
            "INSERT INTO actor (id, kind) VALUES (?1, ?2)
             ON CONFLICT(id) DO UPDATE SET kind = excluded.kind",
            params![actor.id.as_str(), enum_name(&actor.kind)],
        )?;
    }
    tx.execute(
        "INSERT INTO workspace_view (workspace, view) VALUES (?1, ?2)
         ON CONFLICT(workspace) DO UPDATE SET view = excluded.view",
        params![view.id.to_string(), json(view)],
    )?;
    Ok(())
}

/// Rewrites the project's metadata columns and document identity
/// ([`materialize_doc_identity`]) from its view and saves the view. Checks
/// R2 for `parent` first so a violation names its rule, and that a new
/// slug is free ([`StoreError::DuplicateProject`]). Every `doc_id` the
/// view has bound is recorded in `project_doc_owner` first, whatever
/// becomes of the row.
fn materialize_project(tx: &Transaction<'_>, view: &ProjectView, mode: Mode) -> Result<()> {
    let p = view.snapshot(String::new(), BTreeMap::new());
    let ulid = view.id.to_string();
    record_doc_owners(tx, view)?;
    if view.deleted_at.is_some() {
        remove_project_rows(tx, view.id, mode)?;
        return save_project_view(tx, view);
    }
    // A parent that was deleted (its tombstone folded) reads NULL on the
    // row; the view keeps the name (see `Mode`).
    let parent = match &p.parent {
        Some(parent) if !project_exists(tx, parent)? => {
            if !project_tombstoned(tx, parent)? {
                return Err(StoreError::UnknownProject {
                    project: parent.clone(),
                });
            }
            None
        }
        other => other.clone(),
    };
    if project_row_exists(tx, view.id)? {
        tx.execute(
            "UPDATE project SET id = ?1, title = ?2, status = ?3, parent = ?4, repos = ?5
             WHERE ulid = ?6",
            params![
                p.id,
                p.title,
                enum_name(&p.status),
                parent,
                json(&p.repos),
                ulid
            ],
        )?;
    } else {
        if project_exists(tx, &p.id)? {
            if mode == Mode::Rebuild {
                // Rows outlive a rebuild while the log replays in order:
                // a slug freed by a `project.delete` and re-created under
                // a new Ulid is, at the old identity's create, still held
                // by the new identity's row. That row is rewritten when
                // its own create replays; only the view is kept here.
                // [`check_replayed_projects`] catches a genuine clash.
                return save_project_view(tx, view);
            }
            return Err(StoreError::DuplicateProject { id: p.id });
        }
        tx.execute(
            "INSERT INTO project (ulid, id, title, status, parent, repos, doc)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, '')",
            params![
                ulid,
                p.id,
                p.title,
                enum_name(&p.status),
                parent,
                json(&p.repos)
            ],
        )?;
        relink(tx, &p.id)?;
    }
    materialize_doc_identity(tx, view, &p.id)?;
    save_project_view(tx, view)
}

/// Records every `doc_id` `view` has bound in `project_doc_owner` (see
/// migration 0008). A `doc_id` already owned by another project or slot
/// is refused: only a foreign writer reuses one.
pub(crate) fn record_doc_owners(tx: &Transaction<'_>, view: &ProjectView) -> Result<()> {
    let project = view.id.to_string();
    for (name, doc_id) in view.doc_ids() {
        let owner: Option<(String, Option<String>)> = tx
            .query_row(
                "SELECT project, name FROM project_doc_owner WHERE doc_id = ?1",
                params![doc_id.to_string()],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        match owner {
            None => {
                tx.execute(
                    "INSERT INTO project_doc_owner (doc_id, project, name) VALUES (?1, ?2, ?3)",
                    params![doc_id.to_string(), project, name],
                )?;
            }
            Some((p, n)) if p == project && n.as_deref() == name => {}
            Some(_) => return Err(StoreError::DocIdInUse { doc_id }),
        }
    }
    Ok(())
}

/// Points the project row's `doc_id` and its `project_doc` rows at the
/// documents `view` binds (each slot's earliest binding). A document whose
/// id changes — a binding that synced in earlier than the local one —
/// takes the text its own view holds (empty until its `body.edit` ops
/// land), and the displaced document's cached merge state goes: only a
/// row's document has one ([`crate::project`]'s replay rebuilds exactly
/// those).
fn materialize_doc_identity(tx: &Transaction<'_>, view: &ProjectView, slug: &str) -> Result<()> {
    let current: Option<String> = tx.query_row(
        "SELECT doc_id FROM project WHERE ulid = ?1",
        params![view.id.to_string()],
        |r| r.get(0),
    )?;
    let want = view.design_doc_id();
    if current != want.map(|id| id.to_string()) {
        drop_doc_view(tx, current.as_deref())?;
        tx.execute(
            "UPDATE project SET doc_id = ?1, doc = ?2 WHERE ulid = ?3",
            params![
                want.map(|id| id.to_string()),
                doc_text(tx, want)?,
                view.id.to_string()
            ],
        )?;
    }
    for name in view.documents.keys() {
        let Some(want) = view.doc_id(name) else {
            continue;
        };
        let current: Option<Option<String>> = tx
            .query_row(
                "SELECT doc_id FROM project_doc WHERE project = ?1 AND name = ?2",
                params![slug, name],
                |r| r.get(0),
            )
            .optional()?;
        match current {
            Some(Some(id)) if id == want.to_string() => {}
            Some(old) => {
                drop_doc_view(tx, old.as_deref())?;
                tx.execute(
                    "UPDATE project_doc SET doc_id = ?1, body = ?2 WHERE project = ?3 AND name = ?4",
                    params![want.to_string(), doc_text(tx, Some(want))?, slug, name],
                )?;
            }
            None => {
                tx.execute(
                    "INSERT INTO project_doc (project, name, body, doc_id) VALUES (?1, ?2, ?3, ?4)",
                    params![slug, name, doc_text(tx, Some(want))?, want.to_string()],
                )?;
            }
        }
    }
    Ok(())
}

fn drop_doc_view(tx: &Transaction<'_>, doc_id: Option<&str>) -> Result<()> {
    if let Some(doc_id) = doc_id {
        tx.execute(
            "DELETE FROM project_doc_view WHERE doc_id = ?1",
            params![doc_id],
        )?;
    }
    Ok(())
}

/// The text a document's cached merge state holds, `""` without one.
fn doc_text(tx: &Transaction<'_>, doc_id: Option<Ulid>) -> Result<String> {
    Ok(match doc_id {
        Some(id) => crate::project::load_doc_view(tx, id)?
            .map(|v| v.text())
            .unwrap_or_default(),
        None => String::new(),
    })
}

fn save_project_view(tx: &Transaction<'_>, view: &ProjectView) -> Result<()> {
    tx.execute(
        "INSERT INTO project_view (project, view) VALUES (?1, ?2)
         ON CONFLICT(project) DO UPDATE SET view = excluded.view",
        params![view.id.to_string(), json(view)],
    )?;
    Ok(())
}

/// Whether `slug` names a deleted project: some project view with that
/// slug is tombstoned (and, as the caller has checked, no live row holds
/// the slug). A reference to one is kept in its view and reads `NULL`
/// on its row; an unknown slug is still [`StoreError::UnknownProject`].
pub(crate) fn project_tombstoned(conn: &Connection, slug: &str) -> Result<bool> {
    Ok(exists(
        conn,
        "SELECT 1 FROM project_view
         WHERE json_extract(view, '$.slug.value') = ?1
           AND json_extract(view, '$.deleted_at') IS NOT NULL",
        slug,
    )?)
}

/// A project row with `slug` has just been inserted: every ticket and
/// child project whose view names `slug` but whose row reads `NULL` (it
/// pointed at a deleted project of that slug) points at it again — what
/// materializing those rows now would write, and what a rebuild writes.
fn relink(tx: &Transaction<'_>, slug: &str) -> Result<()> {
    tx.execute(
        "UPDATE ticket SET project = ?1
         WHERE project IS NULL AND id IN
           (SELECT ticket FROM ticket_view WHERE json_extract(view, '$.project.value') = ?1)",
        params![slug],
    )?;
    tx.execute(
        "UPDATE project SET parent = ?1
         WHERE parent IS NULL AND id <> ?1 AND ulid IN
           (SELECT project FROM project_view WHERE json_extract(view, '$.parent.value') = ?1)",
        params![slug],
    )?;
    Ok(())
}

/// Removes a tombstoned project's row, its named documents and the cached
/// merge state of every document it owned (the `project_view` row stays:
/// it carries the tombstone). A live commit refuses while a ticket is in
/// the project or a child project names it as `parent`; a pulled op and a
/// replay do not (AGT-1464, see [`Mode`]) — the tickets and children left
/// pointing at it are detached (their rows read `NULL`; their views keep
/// the name).
fn remove_project_rows(tx: &Transaction<'_>, id: Ulid, mode: Mode) -> Result<()> {
    let ulid = id.to_string();
    let slug: Option<String> = tx
        .query_row(
            "SELECT id FROM project WHERE ulid = ?1",
            params![ulid],
            |r| r.get(0),
        )
        .optional()?;
    let Some(slug) = slug else {
        return Ok(());
    };
    if mode == Mode::Commit {
        if exists(tx, "SELECT 1 FROM ticket WHERE project = ?1", &slug)? {
            return Err(StoreError::ProjectHasTickets { project: slug });
        }
        if exists(tx, "SELECT 1 FROM project WHERE parent = ?1", &slug)? {
            return Err(StoreError::ProjectHasChildren { project: slug });
        }
    }
    tx.execute(
        "UPDATE ticket SET project = NULL WHERE project = ?1",
        params![slug],
    )?;
    tx.execute(
        "UPDATE project SET parent = NULL WHERE parent = ?1",
        params![slug],
    )?;
    tx.execute(
        "DELETE FROM project_doc_view WHERE doc_id IN
            (SELECT doc_id FROM project_doc WHERE project = ?1 AND doc_id IS NOT NULL
             UNION SELECT doc_id FROM project WHERE id = ?1 AND doc_id IS NOT NULL)",
        params![slug],
    )?;
    tx.execute("DELETE FROM project_doc WHERE project = ?1", params![slug])?;
    tx.execute("DELETE FROM project WHERE id = ?1", params![slug])?;
    Ok(())
}

/// Stamps and commits `payloads` against `entity` under `actor`, in
/// order, inside the caller's transaction. The clock is seeded from the
/// log's newest HLC once, so a run of ops is strictly increasing.
pub(crate) fn commit_payloads(
    tx: &Transaction<'_>,
    entity: Ulid,
    actor: &ActorId,
    payloads: Vec<Payload>,
) -> Result<usize> {
    let mut clock = Clock::from_latest(latest_hlc(tx)?);
    let n = payloads.len();
    for payload in payloads {
        let op = Op::new(
            Ulid::new(),
            clock.send(now_ms()),
            actor.clone(),
            entity,
            payload,
        );
        commit_config_in(tx, &op, Mode::Commit, || Ok(()))?;
    }
    Ok(n)
}

/// Creates or updates a project's metadata through config ops and returns
/// its Ulid; also binds the project's design doc when it has none yet
/// (AGT-1413: a new project's `project.create` carries the `doc_id`, an
/// older one gets a `project.doc_add`), to `design_doc` if given, else to
/// a fresh id. Shared by [`Store::put_project`],
/// `project.rs::create_project` and `import.rs::upsert_project`, which
/// differ only in what they do with the project's documents afterwards.
#[allow(clippy::too_many_arguments)]
pub(crate) fn upsert_meta_in(
    tx: &Transaction<'_>,
    slug: &str,
    title: &str,
    status: ProjectStatus,
    parent: Option<&str>,
    repos: &BTreeSet<String>,
    design_doc: Option<Ulid>,
    actor: &ActorId,
) -> Result<Ulid> {
    if let Some(parent) = parent
        && !project_exists(tx, parent)?
    {
        return Err(StoreError::UnknownProject {
            project: parent.to_string(),
        });
    }
    let existing: Option<Option<String>> = tx
        .query_row(
            "SELECT ulid FROM project WHERE id = ?1",
            params![slug],
            |r| r.get(0),
        )
        .optional()?;
    let ulid = match existing {
        Some(Some(text)) => crate::codec::ulid("project.ulid", &text)?,
        Some(None) => {
            // A row from before migration 0007's backfill could not have
            // survived it; still, give a ulid-less row an identity rather
            // than fail on it.
            let ulid = Ulid::new();
            tx.execute(
                "UPDATE project SET ulid = ?1 WHERE id = ?2",
                params![ulid.to_string(), slug],
            )?;
            ulid
        }
        None => Ulid::new(),
    };
    let view = load_project_view(tx, ulid)?;
    let design_doc = design_doc.unwrap_or_else(fresh_id);
    let payloads = project_diff(
        view.as_ref(),
        slug,
        title,
        status,
        parent,
        repos,
        design_doc,
    );
    commit_payloads(tx, ulid, actor, payloads)?;
    Ok(ulid)
}

/// A newly minted id. (Not `Ulid::default()`, which clippy's
/// `unwrap_or_default` suggests for `unwrap_or_else(Ulid::new)`: that is
/// the nil id.)
fn fresh_id() -> Ulid {
    Ulid::new()
}

// ------------------------------------------------------------------ diffs

/// The `workspace.set` / `state.upsert` payloads that take `current` to
/// `target`. A register the view has never written (`stamp: None`) is
/// always emitted, so a fresh workspace records every field explicitly;
/// after that only changes are. Gate labels and model labels diff as
/// sets/maps (a remove cites the observed add-tags); a state is re-upserted
/// when its record differs. States only in `current` are left alone.
pub fn workspace_diff(current: Option<&WorkspaceView>, target: &Workspace) -> Vec<Payload> {
    let fresh = WorkspaceView::new(target.id);
    let view = current.unwrap_or(&fresh);
    let mut out = Vec::new();
    if view.prefix.stamp.is_none() || view.prefix.value != target.prefix {
        out.push(Payload::WorkspaceSet(WorkspaceSet::Prefix(
            target.prefix.clone(),
        )));
    }
    if view.template_sections.stamp.is_none()
        || view.template_sections.value != target.template_sections
    {
        out.push(Payload::WorkspaceSet(WorkspaceSet::TemplateSections(
            target.template_sections.clone(),
        )));
    }
    if view.stale_days.stamp.is_none() || view.stale_days.value != target.stale_days {
        out.push(Payload::WorkspaceSet(WorkspaceSet::StaleDays(
            target.stale_days,
        )));
    }
    // Emitted only on a change: a workspace that never chose an owner has
    // the default (vault) and needs no op to say so.
    if view.docs_owned_by.value != target.docs_owned_by {
        out.push(Payload::WorkspaceSet(WorkspaceSet::DocsOwnedBy(
            target.docs_owned_by,
        )));
    }
    for label in &target.gate_labels {
        if !view.gate_labels.contains(label) {
            out.push(Payload::WorkspaceSet(WorkspaceSet::GateLabelAdd(
                label.clone(),
            )));
        }
    }
    for label in view.gate_labels.iter() {
        if !target.gate_labels.contains(label) {
            out.push(Payload::WorkspaceSet(WorkspaceSet::GateLabelRemove {
                label: label.clone(),
                observed: view.gate_labels.observed(label),
            }));
        }
    }
    for (label, model) in &target.model_labels {
        let have = view
            .model_labels
            .get(label)
            .and_then(|r| r.value.as_deref());
        if have != Some(model.as_str()) {
            out.push(Payload::WorkspaceSet(WorkspaceSet::ModelLabel {
                label: label.clone(),
                model: Some(model.clone()),
            }));
        }
    }
    for (label, reg) in &view.model_labels {
        if reg.value.is_some() && !target.model_labels.contains_key(label) {
            out.push(Payload::WorkspaceSet(WorkspaceSet::ModelLabel {
                label: label.clone(),
                model: None,
            }));
        }
    }
    for state in &target.states {
        if view.states.get(&state.name).map(|r| &r.value) != Some(state) {
            out.push(Payload::StateUpsert(StateUpsert {
                name: state.name.clone(),
                category: state.category,
                position: state.position,
            }));
        }
    }
    out
}

/// The `project.create` / `project.set` payloads that take `current` to
/// the given metadata: a create (plus one `repo_add` per repo) when the
/// view has never seen one, otherwise a `project.set` per changed scalar
/// and a repo add/remove per set difference. `design_doc` is the design
/// doc's id for a project that has none bound yet: carried by the create,
/// or by a `project.doc_add` for a project created without one
/// (AGT-1413); ignored once one is bound.
pub fn project_diff(
    current: Option<&ProjectView>,
    slug: &str,
    title: &str,
    status: ProjectStatus,
    parent: Option<&str>,
    repos: &BTreeSet<String>,
    design_doc: Ulid,
) -> Vec<Payload> {
    let mut out = Vec::new();
    match current.filter(|v| v.created.is_some()) {
        None => {
            let bound = current.and_then(ProjectView::design_doc_id);
            out.push(Payload::ProjectCreate(ProjectCreate {
                id: slug.to_string(),
                title: title.to_string(),
                status,
                parent: parent.map(str::to_string),
                doc_id: bound.is_none().then_some(design_doc),
            }));
            for repo in repos {
                out.push(Payload::ProjectSet(ProjectSet::RepoAdd(repo.clone())));
            }
        }
        Some(view) => {
            if view.title.value != title {
                out.push(Payload::ProjectSet(ProjectSet::Title(title.to_string())));
            }
            if view.status.value != status {
                out.push(Payload::ProjectSet(ProjectSet::Status(status)));
            }
            if view.parent.value.as_deref() != parent {
                out.push(Payload::ProjectSet(ProjectSet::Parent(
                    parent.map(str::to_string),
                )));
            }
            for repo in repos {
                if !view.repos.contains(repo) {
                    out.push(Payload::ProjectSet(ProjectSet::RepoAdd(repo.clone())));
                }
            }
            for repo in view.repos.iter() {
                if !repos.contains(repo) {
                    out.push(Payload::ProjectSet(ProjectSet::RepoRemove {
                        repo: repo.clone(),
                        observed: view.repos.observed(repo),
                    }));
                }
            }
            if view.design_doc_id().is_none() {
                out.push(Payload::ProjectDocAdd(ProjectDocAdd {
                    name: None,
                    doc_id: design_doc,
                }));
            }
        }
    }
    out
}

// ------------------------------------------------------------------ reads

/// One workspace per database: an op (or an `init_workspace`) for a
/// different workspace id than the one already here is refused rather
/// than silently rewriting the row.
fn check_workspace_id(conn: &Connection, entity: Ulid) -> Result<()> {
    let current: Option<String> = conn
        .query_row("SELECT id FROM workspace", [], |r| r.get(0))
        .optional()?;
    if let Some(current) = current {
        let workspace = ulid("workspace.id", &current)?;
        if workspace != entity {
            return Err(StoreError::ForeignWorkspace { entity, workspace });
        }
    }
    Ok(())
}

pub(crate) fn load_workspace_view(conn: &Connection, id: Ulid) -> Result<Option<WorkspaceView>> {
    let text: Option<String> = conn
        .query_row(
            "SELECT view FROM workspace_view WHERE workspace = ?1",
            params![id.to_string()],
            |r| r.get(0),
        )
        .optional()?;
    opt_from_json("workspace_view.view", text)
}

pub(crate) fn load_project_view(conn: &Connection, id: Ulid) -> Result<Option<ProjectView>> {
    let text: Option<String> = conn
        .query_row(
            "SELECT view FROM project_view WHERE project = ?1",
            params![id.to_string()],
            |r| r.get(0),
        )
        .optional()?;
    opt_from_json("project_view.view", text)
}

fn project_row_exists(conn: &Connection, id: Ulid) -> rusqlite::Result<bool> {
    exists(
        conn,
        "SELECT 1 FROM project WHERE ulid = ?1",
        &id.to_string(),
    )
}

/// A project's Ulid by its kebab-case id, if the row exists (and has one).
pub(crate) fn project_ulid(conn: &Connection, slug: &str) -> Result<Option<Ulid>> {
    let row: Option<Option<String>> = conn
        .query_row(
            "SELECT ulid FROM project WHERE id = ?1",
            params![slug],
            |r| r.get(0),
        )
        .optional()?;
    match row.flatten() {
        Some(text) => Ok(Some(ulid("project.ulid", &text)?)),
        None => Ok(None),
    }
}

pub(crate) fn workspace_row(conn: &Connection) -> Result<Option<Workspace>> {
    // Migrations 0007/0008 read the workspace (their backfills) before
    // migration 0010 adds `docs_owned_by`; until then the owner is the
    // default, `vault`.
    let has_owner: bool = conn.query_row(
        "SELECT COUNT(*) > 0 FROM pragma_table_info('workspace') WHERE name = 'docs_owned_by'",
        [],
        |r| r.get(0),
    )?;
    let owner_column = if has_owner {
        "docs_owned_by"
    } else {
        "'vault'"
    };
    let row = conn
        .query_row(
            &format!(
                "SELECT id, prefix, gate_labels, model_labels, template_sections, stale_days, {owner_column} FROM workspace"
            ),
            [],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, String>(4)?,
                    r.get::<_, u32>(5)?,
                    r.get::<_, String>(6)?,
                ))
            },
        )
        .optional()?;
    let Some((id, prefix, gate_labels, model_labels, template_sections, stale_days, docs_owned_by)) =
        row
    else {
        return Ok(None);
    };
    Ok(Some(Workspace {
        id: ulid("workspace.id", &id)?,
        prefix,
        states: states(conn)?,
        gate_labels: from_json("workspace.gate_labels", &gate_labels)?,
        model_labels: from_json("workspace.model_labels", &model_labels)?,
        template_sections: from_json("workspace.template_sections", &template_sections)?,
        stale_days,
        docs_owned_by: enum_from_name("workspace.docs_owned_by", docs_owned_by)?,
    }))
}

/// Migration 0010's one step (AGT-1406): adds `workspace.docs_owned_by`
/// unless the table already has it (migrations from 0005 on must be
/// re-runnable).
pub(crate) fn add_docs_owned_by_column(conn: &Connection) -> Result<()> {
    let has: bool = conn.query_row(
        "SELECT COUNT(*) > 0 FROM pragma_table_info('workspace') WHERE name = 'docs_owned_by'",
        [],
        |r| r.get(0),
    )?;
    if !has {
        conn.execute_batch(
            "ALTER TABLE workspace ADD COLUMN docs_owned_by TEXT NOT NULL DEFAULT 'vault' CHECK (docs_owned_by IN ('vault', 'pm'))",
        )?;
    }
    Ok(())
}

pub(crate) fn states(conn: &Connection) -> Result<Vec<State>> {
    let mut stmt =
        conn.prepare("SELECT name, category, position FROM state ORDER BY position, name")?;
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, u32>(2)?,
        ))
    })?;
    rows.map(|row| {
        let (name, category, position) = row?;
        Ok(State {
            name,
            category: enum_from_name("state.category", category)?,
            position,
        })
    })
    .collect()
}

/// Every `actor` row, by id — what migration 0007's backfill turns into
/// `actor.upsert` ops.
pub(crate) fn actors(conn: &Connection) -> Result<Vec<ActorUpsert>> {
    let mut stmt = conn.prepare("SELECT id, kind FROM actor ORDER BY id")?;
    let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
    rows.map(|row| {
        let (id, kind) = row?;
        let kind: ActorKind = enum_from_name("actor.kind", kind)?;
        Ok(ActorUpsert {
            id: ActorId::new(id),
            kind,
        })
    })
    .collect()
}

/// Refuses making `parent` the parent of `id` when that closes a loop
/// (AGT-1489): `parent` is `id` itself, or `id` is among `parent`'s
/// ancestors (so `parent` is one of `id`'s descendants). The error names
/// the loop child-first, e.g. `a -> b -> a`. Walks the materialized
/// `project.parent` column; a loop already in the data (only concurrent
/// re-parents on two replicas make one) ends the walk instead of spinning.
fn check_parent_cycle(conn: &Connection, id: &str, parent: &str) -> Result<()> {
    let mut chain = vec![id.to_string(), parent.to_string()];
    let mut seen = BTreeSet::from([parent.to_string()]);
    let mut at = parent.to_string();
    while at != id {
        let up: Option<String> = conn
            .query_row(
                "SELECT parent FROM project WHERE id = ?1",
                params![at],
                |r| r.get(0),
            )
            .optional()?
            .flatten();
        match up {
            Some(up) if seen.insert(up.clone()) => {
                chain.push(up.clone());
                at = up;
            }
            _ => return Ok(()),
        }
    }
    Err(StoreError::ProjectCycle {
        project: id.to_string(),
        parent: parent.to_string(),
        cycle: chain.join(" -> "),
    })
}

pub(crate) fn project_exists(conn: &Connection, id: &str) -> rusqlite::Result<bool> {
    conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM project WHERE id = ?1)",
        params![id],
        |r| r.get(0),
    )
}

fn load_projects(
    conn: &Connection,
    where_clause: &str,
    args: impl rusqlite::Params,
) -> Result<Vec<Project>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT id, title, status, parent, repos, doc FROM project {where_clause} ORDER BY id"
    ))?;
    let rows = stmt.query_map(args, |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, Option<String>>(3)?,
            r.get::<_, String>(4)?,
            r.get::<_, String>(5)?,
        ))
    })?;
    let mut docs =
        conn.prepare("SELECT name, body FROM project_doc WHERE project = ?1 ORDER BY name")?;
    let mut projects = Vec::new();
    for row in rows {
        let (id, title, status, parent, repos, doc) = row?;
        let documents: BTreeMap<String, String> = docs
            .query_map(params![id], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?;
        let repos: BTreeSet<String> = from_json("project.repos", &repos)?;
        projects.push(Project {
            id,
            title,
            status: enum_from_name("project.status", status)?,
            parent,
            repos,
            doc,
            documents,
        });
    }
    Ok(projects)
}

#[cfg(test)]
mod tests {
    use pm_core::{Hlc, State, StateCategory, Workspace};
    use ulid::Ulid;

    use super::*;
    use crate::Store;

    fn matt() -> ActorId {
        ActorId::new("matt")
    }

    fn workspace() -> Workspace {
        Workspace {
            id: Ulid::new(),
            prefix: "AGT".into(),
            states: vec![State {
                name: "triage".into(),
                category: StateCategory::Unstarted,
                position: 0,
            }],
            gate_labels: ["manual".to_string()].into(),
            model_labels: [("model:fable-5".to_string(), "fable".to_string())].into(),
            template_sections: Vec::new(),
            stale_days: 30,
            docs_owned_by: Default::default(),
        }
    }

    fn fresh() -> (tempfile::TempDir, Store, Workspace) {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(dir.path().join("pm.sqlite")).unwrap();
        let ws = workspace();
        store.init_workspace(&ws, &matt()).unwrap();
        (dir, store, ws)
    }

    fn kinds(store: &Store) -> Vec<&'static str> {
        store
            .ops_since(0)
            .unwrap()
            .into_iter()
            .map(|(_, op)| op.kind())
            .collect()
    }

    /// A fresh workspace records every field: one op per scalar, per gate
    /// label, per model label and per state — and re-applying the same
    /// snapshot emits nothing.
    #[test]
    fn init_workspace_emits_every_field_once_and_is_idempotent() {
        let (_dir, mut store, ws) = fresh();
        assert_eq!(
            kinds(&store),
            vec![
                "workspace.set",
                "workspace.set",
                "workspace.set",
                "workspace.set",
                "workspace.set",
                "state.upsert"
            ]
        );
        assert_eq!(store.workspace().unwrap().unwrap(), ws);
        assert_eq!(store.init_workspace(&ws, &matt()).unwrap(), 0);
        assert_eq!(kinds(&store).len(), 6);
    }

    #[test]
    fn set_gate_labels_replaces_the_set_and_leaves_everything_else_untouched() {
        let (_dir, mut store, before) = fresh();
        assert_eq!(before.gate_labels, ["manual".to_string()].into());

        let wanted: BTreeSet<String> = ["manual".to_string(), "matt-gated".to_string()].into();
        store.set_gate_labels(&wanted, &matt()).unwrap();
        let after = store.workspace().unwrap().unwrap();
        assert_eq!(after.gate_labels, wanted);
        assert_eq!(after.id, before.id);
        assert_eq!(after.prefix, before.prefix);
        assert_eq!(after.states, before.states);
        assert_eq!(after.stale_days, before.stale_days);
        assert_eq!(kinds(&store).len(), 7, "exactly one add");

        // Removing one cites the observed add-tag (OR-set) and drops it.
        let narrowed: BTreeSet<String> = ["matt-gated".to_string()].into();
        store.set_gate_labels(&narrowed, &matt()).unwrap();
        assert_eq!(store.workspace().unwrap().unwrap().gate_labels, narrowed);
        let (_, last) = store.ops_since(0).unwrap().pop().unwrap();
        assert!(matches!(
            last.payload,
            Payload::WorkspaceSet(WorkspaceSet::GateLabelRemove { ref label, ref observed })
                if label == "manual" && observed.len() == 1
        ));
    }

    #[test]
    fn set_gate_labels_errors_when_no_workspace_exists_yet() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(dir.path().join("pm.sqlite")).unwrap();
        let err = store
            .set_gate_labels(&["manual".to_string()].into(), &matt())
            .unwrap_err();
        assert!(matches!(err, StoreError::NoWorkspace), "{err:?}");
    }

    #[test]
    fn a_second_workspace_id_is_refused() {
        let (_dir, mut store, mut ws) = fresh();
        ws.id = Ulid::new();
        let err = store.init_workspace(&ws, &matt()).unwrap_err();
        assert!(
            matches!(err, StoreError::ForeignWorkspace { .. }),
            "{err:?}"
        );
    }

    /// The workspace row waits for its prefix: states from a batch that
    /// delivers `state.upsert` first land, the row appears with the prefix.
    #[test]
    fn the_workspace_row_appears_once_a_prefix_has_folded_in() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(dir.path().join("pm.sqlite")).unwrap();
        let id = Ulid::new();
        let op = |wall_ms, payload| Op::new(Ulid::new(), Hlc::new(wall_ms, 0), matt(), id, payload);
        store
            .commit(&op(
                1,
                Payload::StateUpsert(StateUpsert {
                    name: "triage".into(),
                    category: StateCategory::Unstarted,
                    position: 0,
                }),
            ))
            .unwrap();
        assert!(store.workspace().unwrap().is_none());
        assert_eq!(states(&store.conn).unwrap().len(), 1);
        store
            .commit(&op(
                2,
                Payload::WorkspaceSet(WorkspaceSet::Prefix("AGT".into())),
            ))
            .unwrap();
        assert_eq!(store.workspace().unwrap().unwrap().prefix, "AGT");
    }

    #[test]
    fn a_ticket_op_is_not_a_config_op_and_vice_versa() {
        let (_dir, mut store, ws) = fresh();
        // A config op for a ticket entity folds into a *workspace* view for
        // that entity — but the id is not this database's workspace.
        let err = store
            .commit(&Op::new(
                Ulid::new(),
                Hlc::new(9, 0),
                matt(),
                Ulid::new(),
                Payload::WorkspaceSet(WorkspaceSet::StaleDays(1)),
            ))
            .unwrap_err();
        assert!(
            matches!(err, StoreError::ForeignWorkspace { .. }),
            "{err:?}"
        );
        assert_eq!(
            store.workspace().unwrap().unwrap().stale_days,
            ws.stale_days
        );
    }

    #[test]
    fn project_diff_creates_then_only_emits_changes() {
        let (_dir, mut store, _) = fresh();
        let repos: BTreeSet<String> = ["OpenThinkAi/pm".to_string()].into();
        let p = Project {
            id: "pm".into(),
            title: "pm".into(),
            status: ProjectStatus::InProgress,
            parent: None,
            repos: repos.clone(),
            doc: "# pm\n".into(),
            documents: [("notes".to_string(), "n\n".to_string())].into(),
        };
        let ulid = store.put_project(&p, &matt()).unwrap();
        let n = kinds(&store).len();
        // AGT-1413: the create binds the design doc, a `project.doc_add`
        // the named one, and each document's text is a `body.edit`.
        assert_eq!(
            &kinds(&store)[n - 5..],
            [
                "project.create",
                "project.set",
                "project.doc_add",
                "body.edit",
                "body.edit"
            ]
        );
        assert_eq!(store.project("pm").unwrap().unwrap(), p);
        let view = store.project_view("pm").unwrap().unwrap();
        assert_eq!(view.id, ulid);
        assert_eq!(view.design_doc_id(), store.design_doc_id("pm").unwrap());
        assert_eq!(
            view.doc_id("notes"),
            store.named_doc_id("pm", "notes").unwrap()
        );

        // The same project again: no ops.
        assert_eq!(store.put_project(&p, &matt()).unwrap(), ulid);
        assert_eq!(kinds(&store).len(), n);

        let mut changed = p.clone();
        changed.title = "pm (renamed)".into();
        changed.status = ProjectStatus::Complete;
        changed.repos = ["OpenThinkAi/pm-hub".to_string()].into();
        store.put_project(&changed, &matt()).unwrap();
        assert_eq!(
            kinds(&store).len(),
            n + 4,
            "title, status, repo add, repo remove"
        );
        assert_eq!(store.project("pm").unwrap().unwrap(), changed);

        store
            .set_project_status("pm", ProjectStatus::Abandoned, &matt())
            .unwrap();
        store
            .set_project_status("pm", ProjectStatus::Abandoned, &matt())
            .unwrap();
        assert_eq!(
            kinds(&store).len(),
            n + 5,
            "an unchanged status emits nothing"
        );
        assert_eq!(
            store.project("pm").unwrap().unwrap().status,
            ProjectStatus::Abandoned
        );
    }

    #[test]
    fn a_project_set_ahead_of_its_create_is_an_unknown_project_entity() {
        let (_dir, mut store, _) = fresh();
        let project = Ulid::new();
        let err = store
            .commit(&Op::new(
                Ulid::new(),
                Hlc::new(9, 0),
                matt(),
                project,
                Payload::ProjectSet(ProjectSet::Title("x".into())),
            ))
            .unwrap_err();
        assert!(
            matches!(err, StoreError::UnknownProjectEntity { project: p } if p == project),
            "{err:?}"
        );
    }
}
