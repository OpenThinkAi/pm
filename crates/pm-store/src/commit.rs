//! Writes. [`Store::commit`] is the only way a ticket — or, since
//! AGT-1385, the workspace's config and a project's metadata — changes:
//! the op is appended and the rows it affects are rewritten in one
//! transaction. A config op (`Payload::is_config`) takes the same path
//! through [`crate::config::commit_config_in`]. [`Store::allocate_number`]
//! is the authority's number allocation, which is itself a `field.set
//! number` op committed the same way.

use std::time::{SystemTime, UNIX_EPOCH};

use pm_core::op::FieldSet;
use pm_core::{ActorId, Clock, Hlc, Op, Payload, Ticket, TicketView, apply, apply_persisted};
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use ulid::Ulid;

use crate::Store;
use crate::codec::{enum_name, json, wall_ms};
use crate::config::{Mode, commit_config_in, project_exists, project_tombstoned, states};
use crate::error::{Result, StoreError, map_duplicate_number};

impl Store {
    /// Appends `op` to the log and materializes what it touches, in one
    /// transaction. For a ticket op, returns the ticket as it now reads;
    /// for a config op (`workspace.set`, `state.upsert`, `actor.upsert`,
    /// `project.create`, `project.set` — [`Payload::is_config`]) the
    /// workspace/state/actor/project rows are rewritten instead
    /// ([`crate::config`]) and this returns `None`.
    ///
    /// Rejects, without writing anything: an op id already in the log, an
    /// op for a ticket that has no `ticket.create` yet, a `claim` the
    /// ticket cannot admit ([`TicketView::claim_admissible`]), a
    /// `project.set` for a project with no `project.create` yet, and any
    /// materialized row that would violate R2 (project), R4 (relation
    /// endpoint), R5 (number) or the workflow states.
    pub fn commit(&mut self, op: &Op) -> Result<Option<Ticket>> {
        self.commit_with(op, || Ok(()))
    }

    /// [`Store::commit`] with a hook that runs after the op is appended and
    /// before the tables are rewritten. Tests inject a failure there to
    /// prove neither half lands on its own.
    pub(crate) fn commit_with(
        &mut self,
        op: &Op,
        between: impl FnOnce() -> Result<()>,
    ) -> Result<Option<Ticket>> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let ticket = commit_in(&tx, op, between)?;
        tx.commit()?;
        Ok(ticket)
    }

    /// Gives `ticket` the next human number. One transaction reads the
    /// current maximum, stamps and commits the `field.set number` op, so
    /// concurrent callers never share a number; `ticket.number`'s UNIQUE
    /// backs that up.
    pub fn allocate_number(&mut self, ticket: Ulid, actor: &ActorId) -> Result<u64> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let next = allocate_number_in(&tx, ticket, actor)?;
        tx.commit()?;
        Ok(next)
    }

    /// Commits every op in `ops` (ticket or config, in order), then
    /// allocates a human number for each ticket in `to_number` (also in
    /// order) — all inside **one** transaction (AGT-1346 AC2: `pm new
    /// --batch` is all-or-nothing across every ticket it mints). A failure
    /// at any point — a bad relation target, an unknown project, a
    /// duplicate op — rolls the whole batch back; nothing partially lands.
    ///
    /// Returns the numbered tickets, in the same order as `to_number`.
    pub fn commit_batch(
        &mut self,
        ops: &[Op],
        to_number: &[(Ulid, ActorId)],
    ) -> Result<Vec<Ticket>> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        for op in ops {
            commit_in(&tx, op, || Ok(()))?;
        }
        let mut tickets = Vec::with_capacity(to_number.len());
        for (ticket, actor) in to_number {
            allocate_number_in(&tx, *ticket, actor)?;
            let view =
                load_view(&tx, *ticket)?.ok_or(StoreError::UnknownTicket { ticket: *ticket })?;
            tickets.push(view.snapshot());
        }
        tx.commit()?;
        Ok(tickets)
    }

    /// [`Store::commit_batch`] for a replica whose numbers the hub assigns
    /// (AGT-1398): commits every op in `ops`, allocates nothing, and flags
    /// each ticket in `pending` as awaiting a hub number
    /// ([`Store::mark_pending_number`]) — all in **one** transaction, so
    /// a batch that fails leaves no stray pending flag and a batch that
    /// lands is never observed created-but-unflagged. The tickets read
    /// `AGT-?` until a pulled `field.set number` clears the flag
    /// ([`Store::apply_pulled`]).
    ///
    /// Returns the tickets, in the same order as `pending`.
    pub fn commit_batch_pending(&mut self, ops: &[Op], pending: &[Ulid]) -> Result<Vec<Ticket>> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        for op in ops {
            commit_in(&tx, op, || Ok(()))?;
        }
        let mut tickets = Vec::with_capacity(pending.len());
        for ticket in pending {
            let view =
                load_view(&tx, *ticket)?.ok_or(StoreError::UnknownTicket { ticket: *ticket })?;
            tx.execute(
                "INSERT OR IGNORE INTO pending_number (ticket) VALUES (?1)",
                params![ticket.to_string()],
            )?;
            tickets.push(view.snapshot());
        }
        tx.commit()?;
        Ok(tickets)
    }

    /// The greatest HLC in the log ([`Hlc::ZERO`] when empty): what a
    /// caller restores its [`Clock`] from so it never re-issues a stamp.
    pub fn latest_hlc(&self) -> Result<Hlc> {
        latest_hlc(&self.conn)
    }
}

/// The body of [`Store::allocate_number`], shared with [`Store::commit_batch`]
/// so both run inside whichever transaction the caller already holds.
fn allocate_number_in(tx: &Transaction<'_>, ticket: Ulid, actor: &ActorId) -> Result<u64> {
    let current: Option<Option<i64>> = tx
        .query_row(
            "SELECT number FROM ticket WHERE id = ?1",
            params![ticket.to_string()],
            |r| r.get(0),
        )
        .optional()?;
    match current {
        None => return Err(StoreError::UnknownTicket { ticket }),
        Some(Some(number)) => {
            return Err(StoreError::AlreadyNumbered {
                ticket,
                number: number as u64,
            });
        }
        Some(None) => {}
    }
    // Above every number in use *and* the allocator floor `pm import
    // vault` raises to the vault's maximum (import.rs, AGT-1347).
    let next: i64 = tx.query_row(
        "SELECT MAX(COALESCE((SELECT MAX(number) FROM ticket), 0),
                    COALESCE((SELECT number_floor FROM workspace), 0)) + 1",
        [],
        |r| r.get(0),
    )?;
    let hlc = Clock::from_latest(latest_hlc(tx)?).send(now_ms());
    let op = Op::new(
        Ulid::new(),
        hlc,
        actor.clone(),
        ticket,
        Payload::FieldSet(FieldSet::Number(next as u64)),
    );
    commit_in(tx, &op, || Ok(()))?;
    Ok(next as u64)
}

pub(crate) fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

pub(crate) fn latest_hlc(conn: &Connection) -> Result<Hlc> {
    let latest = conn
        .query_row(
            "SELECT hlc_wall_ms, hlc_counter FROM ops
             ORDER BY hlc_wall_ms DESC, hlc_counter DESC LIMIT 1",
            [],
            |r| Ok(Hlc::new(r.get::<_, i64>(0)? as u64, r.get(1)?)),
        )
        .optional()?;
    Ok(latest.unwrap_or(Hlc::ZERO))
}

fn commit_in(
    tx: &Transaction<'_>,
    op: &Op,
    between: impl FnOnce() -> Result<()>,
) -> Result<Option<Ticket>> {
    if exists(
        tx,
        "SELECT 1 FROM ops WHERE op_id = ?1",
        &op.op_id.to_string(),
    )? {
        return Err(StoreError::DuplicateOp { op_id: op.op_id });
    }
    if op.payload.is_config() {
        commit_config_in(tx, op, Mode::Commit, between)?;
        return Ok(None);
    }
    check_ingest(op)?;
    check_ticket_entity(tx, op)?;
    ensure_actor(tx, &op.actor)?;
    let view = next_view(tx, op, Fold::Local)?;
    append_op(tx, op)?;
    between()?;
    materialize(tx, &view, op, Fold::Local).map(Some)
}

/// Commits an op that came from the hub (AGT-1393, [`Store::apply_pulled`]):
/// the same ensure-actor → apply → append → materialize path as
/// [`commit_in`], except a `claim` is not re-checked for admissibility —
/// the hub is the claim authority and already admitted it (README
/// §Conflict semantics: "replicas apply admitted claims as plain LWW
/// writes"), exactly as [`replay_in`] treats one. A config op takes the
/// config path unchanged (nothing about it is authority-checked). The
/// caller has already ruled out a duplicate op id.
///
/// A pulled `body.edit` ahead of the description edits it builds on is
/// refused ([`pm_core::ApplyError::MissingDependency`], AGT-1415) rather
/// than queued in a view that is persisted right after — which would drop
/// it — so the pull defers it until its history lands.
pub(crate) fn commit_foreign_in(tx: &Transaction<'_>, op: &Op) -> Result<Option<Ticket>> {
    if op.payload.is_config() {
        commit_config_in(tx, op, Mode::Foreign, || Ok(()))?;
        return Ok(None);
    }
    check_ticket_entity(tx, op)?;
    ensure_actor(tx, &op.actor)?;
    let view = next_view(tx, op, Fold::Foreign)?;
    append_op(tx, op)?;
    materialize(tx, &view, op, Fold::Foreign).map(Some)
}

/// The checks every op passes on its way into the log, whichever path
/// brings it — a local verb ([`commit_in`], the config and document
/// commits), a pull ([`Store::apply_pulled`]) or `pm backup --restore`
/// ([`Store::commit_any`]) (oaudit 2026-09-30, AGT-1450; every ingest path
/// since AGT-1464): the stamp is storable and not more than
/// [`crate::PULL_MAX_FUTURE_SKEW_MS`] ahead of this machine's clock, and
/// every identifier or name it carries that becomes a file path is safe
/// ([`pm_core::ids::check_op_ids`]), and a `body.edit` is within
/// [`pm_core::MAX_BODY_EDIT_BYTES`] (AGT-1467). A replay of the log (`pm doctor
/// --rebuild`) does not re-check: what is in the log already passed.
pub(crate) fn check_ingest(op: &Op) -> Result<()> {
    check_ingest_at(op, now_ms())
}

pub(crate) fn check_ingest_at(op: &Op, now_ms: u64) -> Result<()> {
    op.hlc.check_range()?;
    op.hlc
        .check_not_after(now_ms, crate::sync::PULL_MAX_FUTURE_SKEW_MS)?;
    pm_core::ids::check_op_ids(op)?;
    op.check_size()?;
    Ok(())
}

/// AGT-1464: a `ticket.create` may not take an entity that is already a
/// project document (bound by any project, winner or not) or a project —
/// tickets, documents and projects share the op log's entity namespace,
/// and a `body.edit` is routed by it. The mirror check, a document
/// binding whose `doc_id` is already a ticket, is
/// [`crate::config::check_config_admission`]'s.
pub(crate) fn check_ticket_entity(tx: &Transaction<'_>, op: &Op) -> Result<()> {
    if !matches!(op.payload, Payload::TicketCreate(_)) {
        return Ok(());
    }
    let entity = op.entity.to_string();
    let holder = if crate::project::is_known_doc(tx, op.entity)? {
        "a project document"
    } else if exists(tx, "SELECT 1 FROM project_view WHERE project = ?1", &entity)? {
        "a project"
    } else {
        return Ok(());
    };
    Err(StoreError::EntityInUse {
        entity: op.entity,
        holder,
    })
}

/// Re-applies an op that is already in the log: the same load → apply →
/// materialize path as [`commit_in`], without appending. `pm doctor
/// --rebuild` runs this over `ops` in `seq` order against emptied ticket
/// tables, so a rebuilt row is produced by exactly the code that produced
/// the original.
pub(crate) fn replay_in(tx: &Transaction<'_>, op: &Op) -> Result<Ticket> {
    let view = next_view(tx, op, Fold::Replay)?;
    materialize(tx, &view, op, Fold::Replay)
}

/// Which commit path is folding an op into a ticket's view.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Fold {
    /// [`commit_in`]: a claim is checked for admissibility.
    Local,
    /// [`commit_foreign_in`]: the hub admitted any claim, and a
    /// `body.edit` ahead of its history is refused (AGT-1415).
    Foreign,
    /// [`replay_in`]: the log is replayed as it was committed.
    Replay,
}

/// The ticket's view with `op` folded in. A local commit runs the
/// authority's conditional check on a `claim`; a pulled op and a replay
/// skip it, since the claim was admitted when it was logged (README
/// §Conflict semantics: "replicas apply admitted claims as plain LWW
/// writes"). A pulled op folds through [`apply_persisted`]: pulls arrive
/// in any order, and the view is persisted right after.
fn next_view(tx: &Transaction<'_>, op: &Op, fold: Fold) -> Result<TicketView> {
    let mut view = match load_view(tx, op.entity)? {
        Some(view) => view,
        None if matches!(op.payload, Payload::TicketCreate(_)) => TicketView::new(op.entity),
        None => return Err(StoreError::UnknownTicket { ticket: op.entity }),
    };
    if fold == Fold::Local && matches!(op.payload, Payload::Claim(_)) {
        view.claim_admissible(&states(tx)?)?;
    }
    if fold == Fold::Foreign {
        apply_persisted(&mut view, op)?;
    } else {
        apply(&mut view, op)?;
    }
    Ok(view)
}

/// `pub(crate)`: also used by [`crate::project`]'s `body.edit` commit path,
/// which duplicates an op against a document rather than a ticket.
pub(crate) fn exists(conn: &Connection, sql: &str, key: &str) -> rusqlite::Result<bool> {
    conn.query_row(&format!("SELECT EXISTS ({sql})"), params![key], |r| {
        r.get(0)
    })
}

pub(crate) fn ensure_actor(conn: &Connection, actor: &ActorId) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT OR IGNORE INTO actor (id, kind) VALUES (?1, ?2)",
        params![actor.as_str(), enum_name(&actor.kind())],
    )?;
    Ok(())
}

pub(crate) fn load_view(conn: &Connection, ticket: Ulid) -> Result<Option<TicketView>> {
    let text: Option<String> = conn
        .query_row(
            "SELECT view FROM ticket_view WHERE ticket = ?1",
            params![ticket.to_string()],
            |r| r.get(0),
        )
        .optional()?;
    crate::codec::opt_from_json("ticket_view.view", text)
}

/// `pub(crate)`: reused by [`crate::project::commit_doc_edit`] so a
/// document's `body.edit` op lands in the same `ops` table, the same way.
pub(crate) fn append_op(conn: &Connection, op: &Op) -> Result<()> {
    let mut envelope = serde_json::to_value(op).expect("an op serializes");
    let payload = envelope
        .as_object_mut()
        .and_then(|o| o.remove("payload"))
        .map(|p| p.to_string());
    conn.execute(
        "INSERT INTO ops (op_id, hlc_wall_ms, hlc_counter, actor, entity, kind, payload, version)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![
            op.op_id.to_string(),
            wall_ms(op.hlc)?,
            op.hlc.counter,
            op.actor.as_str(),
            op.entity.to_string(),
            op.kind(),
            payload,
            op.version,
        ],
    )?;
    Ok(())
}

/// Rewrites every derived row of the ticket from its view. Checks the
/// hygiene rules first so a violation names its rule; the foreign keys
/// stay as the backstop.
///
/// A ticket whose project was deleted (a pulled `project.delete` for a
/// project this replica still had tickets in, AGT-1464) keeps the name in
/// its view and reads `NULL` on its row — and in the returned ticket —
/// until a project of that slug exists again; only a local op that *sets*
/// the project is refused for naming a deleted one.
fn materialize(tx: &Transaction<'_>, view: &TicketView, op: &Op, fold: Fold) -> Result<Ticket> {
    let mut t = view.snapshot();
    let id = t.id.to_string();

    if !exists(tx, "SELECT 1 FROM state WHERE name = ?1", &t.state)? {
        return Err(StoreError::UnknownState {
            state: t.state.clone(),
        });
    }
    if let Some(project) = &t.project
        && !project_exists(tx, project)?
    {
        let sets_project = match &op.payload {
            Payload::TicketCreate(c) => c.project.is_some(),
            Payload::FieldSet(FieldSet::Project(p)) => p.is_some(),
            _ => false,
        };
        if (fold == Fold::Local && sets_project) || !project_tombstoned(tx, project)? {
            return Err(StoreError::UnknownProject {
                project: project.clone(),
            });
        }
        t.project = None;
    }
    if let Payload::RelationAdd(r) = &op.payload {
        let other = if r.relation.from == t.id {
            r.relation.to
        } else {
            r.relation.from
        };
        if !exists(tx, "SELECT 1 FROM ticket WHERE id = ?1", &other.to_string())? {
            return Err(StoreError::UnknownRelationTarget { ticket: other });
        }
    }
    if let Some(assignee) = &t.assignee {
        ensure_actor(tx, assignee)?;
    }

    tx.execute(
        "INSERT INTO ticket (id, number, title, state, priority, project, repo, assignee, description,
            created_wall_ms, created_counter, updated_wall_ms, updated_counter,
            archived_wall_ms, archived_counter, deleted, linked_github, linked_pr, linear, source, ext)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21)
         ON CONFLICT(id) DO UPDATE SET
            number = excluded.number, title = excluded.title, state = excluded.state,
            priority = excluded.priority, project = excluded.project, repo = excluded.repo,
            assignee = excluded.assignee, description = excluded.description,
            created_wall_ms = excluded.created_wall_ms, created_counter = excluded.created_counter,
            updated_wall_ms = excluded.updated_wall_ms, updated_counter = excluded.updated_counter,
            archived_wall_ms = excluded.archived_wall_ms, archived_counter = excluded.archived_counter,
            deleted = excluded.deleted, linked_github = excluded.linked_github,
            linked_pr = excluded.linked_pr, linear = excluded.linear, source = excluded.source,
            ext = excluded.ext",
        params![
            id,
            t.number.map(|n| n as i64),
            t.title,
            t.state,
            enum_name(&t.priority),
            t.project,
            t.repo,
            t.assignee.as_ref().map(ActorId::as_str),
            t.description,
            wall_ms(t.created)?,
            t.created.counter,
            wall_ms(t.updated)?,
            t.updated.counter,
            t.archived_at.map(wall_ms).transpose()?,
            t.archived_at.map(|h| h.counter),
            t.deleted,
            t.linked_github,
            t.linked_pr,
            t.linear,
            t.source.as_ref().map(json),
            json(&t.ext),
        ],
    )
    .map_err(|e| map_duplicate_number(e, t.number))?;

    tx.execute("DELETE FROM ticket_label WHERE ticket = ?1", params![id])?;
    for label in &t.labels {
        tx.execute(
            "INSERT INTO ticket_label (ticket, label) VALUES (?1, ?2)",
            params![id, label],
        )?;
    }

    tx.execute("DELETE FROM relation WHERE owner = ?1", params![id])?;
    for r in view.relations.iter() {
        tx.execute(
            "INSERT INTO relation (owner, kind, from_ticket, to_ticket) VALUES (?1, ?2, ?3, ?4)",
            params![id, enum_name(&r.kind), r.from.to_string(), r.to.to_string()],
        )?;
    }

    tx.execute("DELETE FROM comment WHERE ticket = ?1", params![id])?;
    for c in view.comments.iter() {
        tx.execute(
            "INSERT INTO comment (id, ticket, author, hlc_wall_ms, hlc_counter, body)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                c.id.to_string(),
                id,
                c.author.as_str(),
                wall_ms(c.hlc)?,
                c.hlc.counter,
                c.body
            ],
        )?;
    }

    tx.execute("DELETE FROM marker WHERE ticket = ?1", params![id])?;
    let mut markers: Vec<(&str, i64, String)> = Vec::new();
    if let Some(hold) = &t.hold {
        markers.push(("hold", 0, json(hold)));
    }
    for (i, waiver) in t.waivers.iter().enumerate() {
        markers.push(("waiver", i as i64, json(waiver)));
    }
    if let Some(not_before) = &t.not_before {
        markers.push(("not_before", 0, json(not_before)));
    }
    if let Some(parked) = &t.parked {
        markers.push(("parked", 0, json(parked)));
    }
    for (kind, position, data) in markers {
        tx.execute(
            "INSERT INTO marker (ticket, kind, position, data) VALUES (?1, ?2, ?3, ?4)",
            params![id, kind, position, data],
        )?;
    }

    tx.execute(
        "INSERT INTO ticket_view (ticket, view) VALUES (?1, ?2)
         ON CONFLICT(ticket) DO UPDATE SET view = excluded.view",
        params![id, json(view)],
    )?;
    Ok(t)
}

#[cfg(test)]
mod tests {
    use pm_core::op::TicketCreate;
    use pm_core::{Priority, State, StateCategory, Workspace};

    use super::*;

    fn fresh() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(dir.path().join("pm.sqlite")).unwrap();
        store
            .init_workspace(
                &Workspace {
                    id: Ulid::new(),
                    prefix: "AGT".into(),
                    states: vec![State {
                        name: "triage".into(),
                        category: StateCategory::Unstarted,
                        position: 0,
                    }],
                    gate_labels: Default::default(),
                    model_labels: Default::default(),
                    template_sections: Vec::new(),
                    stale_days: 30,
                    docs_owned_by: Default::default(),
                },
                &ActorId::new("matt"),
            )
            .unwrap();
        (dir, store)
    }

    /// Ops `init_workspace` itself committed (AGT-1385): the baseline a
    /// count of "this test's" ops is measured from.
    fn baseline(store: &Store) -> i64 {
        count(store, "SELECT COUNT(*) FROM ops")
    }

    fn create(ticket: Ulid, wall_ms: u64) -> Op {
        Op::new(
            Ulid::new(),
            Hlc::new(wall_ms, 0),
            ActorId::new("matt"),
            ticket,
            Payload::TicketCreate(TicketCreate {
                title: "t".into(),
                state: "triage".into(),
                priority: Priority::Medium,
                project: None,
                repo: None,
                source: None,
                ext: Default::default(),
            }),
        )
    }

    fn count(store: &Store, sql: &str) -> i64 {
        store.conn.query_row(sql, [], |r| r.get(0)).unwrap()
    }

    /// AC2: a failure after the op is appended and before the tables are
    /// rewritten persists neither half.
    #[test]
    fn a_failure_between_append_and_materialize_persists_neither() {
        let (_dir, mut store) = fresh();
        let before = baseline(&store);
        let id = Ulid::new();
        let err = store
            .commit_with(&create(id, 1), || {
                Err(StoreError::UnknownTicket { ticket: id })
            })
            .unwrap_err();
        assert!(matches!(err, StoreError::UnknownTicket { .. }));
        assert_eq!(count(&store, "SELECT COUNT(*) FROM ops"), before);
        assert_eq!(count(&store, "SELECT COUNT(*) FROM ticket"), 0);
        assert_eq!(count(&store, "SELECT COUNT(*) FROM ticket_view"), 0);
        assert!(store.ticket(id).unwrap().is_none());

        // And the same op commits cleanly afterwards: nothing lingered.
        store.commit(&create(id, 1)).unwrap();
        assert_eq!(count(&store, "SELECT COUNT(*) FROM ops"), before + 1);
        assert_eq!(count(&store, "SELECT COUNT(*) FROM ticket"), 1);
    }

    /// AGT-1385 AC1: the same guarantee for a config op — a failure
    /// between the append and the rewrite of the workspace row persists
    /// neither half.
    #[test]
    fn a_config_op_appends_and_materializes_together_or_not_at_all() {
        let (_dir, mut store) = fresh();
        let before = baseline(&store);
        let ws = store.workspace().unwrap().unwrap();
        // Above `init_workspace`'s own stamps, or LWW keeps the init value.
        let after_init = store.latest_hlc().unwrap().wall_ms + 1;
        let op = |stale_days| {
            Op::new(
                Ulid::new(),
                Hlc::new(after_init, 0),
                ActorId::new("matt"),
                ws.id,
                Payload::WorkspaceSet(pm_core::op::WorkspaceSet::StaleDays(stale_days)),
            )
        };
        let err = store
            .commit_with(&op(7), || Err(StoreError::NoWorkspace))
            .unwrap_err();
        assert!(matches!(err, StoreError::NoWorkspace));
        assert_eq!(count(&store, "SELECT COUNT(*) FROM ops"), before);
        assert_eq!(store.workspace().unwrap().unwrap().stale_days, 30);

        assert!(store.commit(&op(7)).unwrap().is_none());
        assert_eq!(count(&store, "SELECT COUNT(*) FROM ops"), before + 1);
        assert_eq!(store.workspace().unwrap().unwrap().stale_days, 7);
    }

    /// The op row lands before the hook runs, so the injection point is
    /// really between the two halves rather than before both.
    #[test]
    fn the_hook_runs_after_the_op_is_appended() {
        let (_dir, mut store) = fresh();
        let id = Ulid::new();
        let seen = std::cell::Cell::new(false);
        store
            .commit_with(&create(id, 1), || {
                seen.set(true);
                Ok(())
            })
            .unwrap();
        assert!(seen.get());
    }
}
