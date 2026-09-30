//! Client sync state (AGT-1393, README §Sync & hub "Client state"): what
//! a replica needs to talk to the hub without the hub protocol itself.
//!
//! - **Outbox.** `sync_state.pushed_through` is a marker into the local op
//!   log: every op with `seq` at or below it is known to the hub. The
//!   outbox is the ops after it, less any in `sync_pushed` — ops above the
//!   marker the hub already has, because they were acknowledged out of
//!   order ([`Store::mark_pushed`]) or pulled from the hub to begin with
//!   ([`Store::apply_pulled`]; a foreign op is never outbox). The marker
//!   advances over `sync_pushed` as soon as the gap below closes. A fresh
//!   or upgraded database starts at 0, so its whole existing log is the
//!   outbox — the first push's seed (README: "First push uploads the
//!   Studio's existing log").
//! - **Cursor.** `sync_state.pulled_seq`: the hub sequence number the last
//!   pull got through ([`Store::cursor`] / [`Store::set_cursor`]).
//! - **Pending numbers.** `pending_number` flags tickets created while a hub
//!   is configured: they read `AGT-?` until the hub's `field.set number`
//!   arrives, and [`Store::apply_pulled`] clears the flag once it has.
//!
//! All of it is bookkeeping written directly, like `backup_target`: not
//! derived from the op log, untouched by `pm doctor --rebuild`.

use pm_core::{DocApplyError, Op, Payload};
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use serde::Serialize;
use ulid::Ulid;

use crate::Store;
use crate::codec::ulid;
use crate::commit::commit_foreign_in;
use crate::error::{Result, StoreError};
use crate::project::{commit_doc_edit_in, is_known_doc};
use crate::query::read_ops;

/// What [`Store::apply_pulled`] did with a batch.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Pulled {
    /// Foreign ops committed into the log.
    pub applied: usize,
    /// Ops already present by `op_id` (this replica's own ops echoed back,
    /// a re-pulled batch, or a duplicate within the batch) — left alone.
    pub skipped: usize,
}

/// The sync state `pm doctor` reports.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct SyncStatus {
    /// Every local op with `seq` at or below this is known to the hub.
    pub pushed_through: i64,
    /// Ops the hub has not acknowledged ([`Store::outbox`]'s full length).
    pub outbox: u64,
    /// The hub sequence number the last pull got through; 0 = never.
    pub cursor: i64,
    /// Tickets still awaiting a hub-issued number.
    pub pending_numbers: u64,
}

impl Store {
    /// Up to `limit` ops the hub has not acknowledged, oldest (`seq`)
    /// first — the next batch to push.
    pub fn outbox(&self, limit: usize) -> Result<Vec<(i64, Op)>> {
        read_ops(
            &self.conn,
            "WHERE seq IN (SELECT seq FROM ops
                           WHERE seq > (SELECT pushed_through FROM sync_state)
                             AND seq NOT IN (SELECT seq FROM sync_pushed)
                           ORDER BY seq LIMIT ?1)",
            params![i64::try_from(limit).unwrap_or(i64::MAX)],
        )
    }

    /// How many ops are in the outbox.
    pub fn outbox_len(&self) -> Result<u64> {
        outbox_len(&self.conn)
    }

    /// Records that the hub has acknowledged `op_ids`: they leave the
    /// outbox, and the pushed-through marker advances over every
    /// contiguous acknowledged op. Idempotent; an id not in the log is
    /// ignored. Returns how many of `op_ids` are in the log.
    pub fn mark_pushed(&mut self, op_ids: &[Ulid]) -> Result<usize> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut found = 0;
        for op_id in op_ids {
            if let Some(seq) = seq_of(&tx, *op_id)? {
                mark_seq_pushed(&tx, seq)?;
                found += 1;
            }
        }
        fold_pushed(&tx)?;
        tx.commit()?;
        Ok(found)
    }

    /// The hub sequence number the last pull got through (0 = never
    /// pulled): what the next `pull since <seq>` asks from.
    pub fn cursor(&self) -> Result<i64> {
        Ok(self
            .conn
            .query_row("SELECT pulled_seq FROM sync_state", [], |r| r.get(0))?)
    }

    /// Sets the pull cursor to `seq` (the hub's sequence, which only ever
    /// grows; a caller resetting a replica may set it back to 0).
    pub fn set_cursor(&mut self, seq: i64) -> Result<()> {
        self.conn
            .execute("UPDATE sync_state SET pulled_seq = ?1", params![seq])?;
        Ok(())
    }

    /// Commits foreign ops pulled from the hub through the normal apply
    /// path — the same merge, materialization and hygiene checks as
    /// [`Store::commit`] / [`Store::commit_doc_edit`] — in **one**
    /// transaction. An op already present by `op_id` is skipped, so
    /// re-applying a batch (or receiving this replica's own ops back) is
    /// a no-op. Claims are not re-checked: the hub admitted them.
    ///
    /// Order-independent within the batch: an op whose ticket, relation
    /// target, document, project, project entity or state is not there
    /// yet is deferred
    /// and retried once the rest of the batch has landed, so a batch
    /// applies to the same state in any order (pm-core's merge is
    /// order-independent; this makes the existence checks so too). Ops
    /// are appended to the local log in the order they actually landed,
    /// so `pm doctor`'s replay in `seq` order reproduces the tables.
    ///
    /// Every applied op counts as already pushed (it came from the hub),
    /// and a ticket that now has a number leaves the pending-number set.
    /// Any failure — including a dependency nothing in the batch
    /// supplies — rolls the whole batch back ([`StoreError::Pull`]).
    pub fn apply_pulled(&mut self, ops: &[Op]) -> Result<Pulled> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut pulled = Pulled::default();
        let mut pending: Vec<&Op> = ops.iter().collect();
        while !pending.is_empty() {
            let mut deferred: Vec<&Op> = Vec::new();
            let mut first_blocker: Option<StoreError> = None;
            let attempted = pending.len();
            for op in pending {
                if let Some(seq) = seq_of(&tx, op.op_id)? {
                    mark_seq_pushed(&tx, seq)?;
                    pulled.skipped += 1;
                    continue;
                }
                tx.execute_batch("SAVEPOINT pulled_op")?;
                match apply_foreign(&tx, op) {
                    Ok(()) => {
                        tx.execute_batch("RELEASE pulled_op")?;
                        let seq = seq_of(&tx, op.op_id)?.expect("the op was just appended");
                        mark_seq_pushed(&tx, seq)?;
                        pulled.applied += 1;
                    }
                    Err(e) if is_dependency(&e) => {
                        tx.execute_batch("ROLLBACK TO pulled_op; RELEASE pulled_op")?;
                        deferred.push(op);
                        first_blocker.get_or_insert(pull_error(op, e));
                    }
                    Err(e) => return Err(pull_error(op, e)),
                }
            }
            if deferred.len() == attempted {
                // A full pass landed nothing: what the rest waits on is
                // not coming from this batch.
                return Err(first_blocker.expect("a deferred op recorded its error"));
            }
            pending = deferred;
        }
        tx.execute(
            "DELETE FROM pending_number
             WHERE ticket IN (SELECT id FROM ticket WHERE number IS NOT NULL)",
            [],
        )?;
        fold_pushed(&tx)?;
        tx.commit()?;
        Ok(pulled)
    }

    /// Flags `ticket` as awaiting a hub-issued number (created while a hub
    /// is configured). Idempotent.
    pub fn mark_pending_number(&mut self, ticket: Ulid) -> Result<()> {
        self.conn.execute(
            "INSERT OR IGNORE INTO pending_number (ticket) VALUES (?1)",
            params![ticket.to_string()],
        )?;
        Ok(())
    }

    /// Every ticket still awaiting a hub-issued number, by id.
    pub fn pending_numbers(&self) -> Result<Vec<Ulid>> {
        let mut stmt = self
            .conn
            .prepare("SELECT ticket FROM pending_number ORDER BY ticket")?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        rows.map(|row| ulid("pending_number.ticket", &row?))
            .collect()
    }

    /// The whole sync state in one read, for `pm doctor`.
    pub fn sync_status(&self) -> Result<SyncStatus> {
        sync_status(&self.conn)
    }
}

pub(crate) fn sync_status(conn: &Connection) -> Result<SyncStatus> {
    let (pushed_through, cursor) = conn.query_row(
        "SELECT pushed_through, pulled_seq FROM sync_state",
        [],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    let pending: i64 = conn.query_row("SELECT COUNT(*) FROM pending_number", [], |r| r.get(0))?;
    Ok(SyncStatus {
        pushed_through,
        outbox: outbox_len(conn)?,
        cursor,
        pending_numbers: pending as u64,
    })
}

fn outbox_len(conn: &Connection) -> Result<u64> {
    let n: i64 = conn.query_row(
        "SELECT COUNT(*) FROM ops
         WHERE seq > (SELECT pushed_through FROM sync_state)
           AND seq NOT IN (SELECT seq FROM sync_pushed)",
        [],
        |r| r.get(0),
    )?;
    Ok(n as u64)
}

/// Routes a foreign op to the commit path for the entity it targets. The
/// op log shares one entity namespace between tickets, project documents
/// and config entities; a `body.edit` whose entity is a known document
/// goes to the document path, and everything else — ticket kinds and the
/// config kinds (AGT-1384: `workspace.set`, `state.upsert`,
/// `actor.upsert`, `project.create`, `project.set`, `project.delete`,
/// `project.doc_add`, folded by [`crate::config`] since AGT-1385) — to
/// [`commit_foreign_in`], which dispatches on the kind. The match lists
/// every kind so a new one has to be routed here deliberately.
///
/// A document is "known" once any project has bound its id
/// (`project_doc_owner`, AGT-1413), so a `body.edit` that arrives before
/// the `project.create` / `project.doc_add` binding it falls through to the
/// ticket path, fails as [`StoreError::UnknownTicket`] and defers until
/// the binding lands; one whose binding lost to an earlier one, or whose
/// project was deleted, is still a document edit and lands in the log.
fn apply_foreign(tx: &Transaction<'_>, op: &Op) -> Result<()> {
    match &op.payload {
        Payload::BodyEdit(_) if is_known_doc(tx, op.entity)? => {
            commit_doc_edit_in(tx, op.entity, op)?;
        }
        Payload::WorkspaceSet(_)
        | Payload::StateUpsert(_)
        | Payload::ActorUpsert(_)
        | Payload::ProjectCreate(_)
        | Payload::ProjectSet(_)
        | Payload::ProjectDelete
        | Payload::ProjectDocAdd(_)
        | Payload::TicketCreate(_)
        | Payload::FieldSet(_)
        | Payload::LabelAdd(_)
        | Payload::LabelRemove(_)
        | Payload::RelationAdd(_)
        | Payload::RelationRemove(_)
        | Payload::CommentAdd(_)
        | Payload::StateTransition(_)
        | Payload::Claim(_)
        | Payload::HoldSet(_)
        | Payload::HoldClear
        | Payload::BodyEdit(_)
        | Payload::Tombstone => {
            commit_foreign_in(tx, op)?;
        }
    }
    Ok(())
}

/// Failures another op in the same batch may yet resolve: a missing
/// ticket, relation target, document, project (by slug — a ticket's
/// `project`, a project's `parent`), project entity (a `project.set` or
/// `project.doc_add` ahead of its `project.create`) or state, or a
/// document edit ahead of the edits it builds on (AGT-1413).
fn is_dependency(e: &StoreError) -> bool {
    matches!(
        e,
        StoreError::UnknownTicket { .. }
            | StoreError::UnknownRelationTarget { .. }
            | StoreError::UnknownDocument { .. }
            | StoreError::UnknownProject { .. }
            | StoreError::UnknownProjectEntity { .. }
            | StoreError::UnknownState { .. }
            | StoreError::DocApply(DocApplyError::MissingDependency { .. })
    )
}

fn pull_error(op: &Op, source: StoreError) -> StoreError {
    StoreError::Pull {
        op_id: op.op_id,
        kind: op.kind(),
        source: Box::new(source),
    }
}

fn seq_of(conn: &Connection, op_id: Ulid) -> Result<Option<i64>> {
    Ok(conn
        .query_row(
            "SELECT seq FROM ops WHERE op_id = ?1",
            params![op_id.to_string()],
            |r| r.get(0),
        )
        .optional()?)
}

fn mark_seq_pushed(conn: &Connection, seq: i64) -> Result<()> {
    conn.execute(
        "INSERT OR IGNORE INTO sync_pushed (seq)
         SELECT ?1 WHERE ?1 > (SELECT pushed_through FROM sync_state)",
        params![seq],
    )?;
    Ok(())
}

/// Advances `pushed_through` to just below the oldest op still in the
/// outbox (or to the end of the log when none is), then drops the
/// `sync_pushed` rows it has passed.
fn fold_pushed(conn: &Connection) -> Result<()> {
    conn.execute(
        "UPDATE sync_state SET pushed_through = MAX(pushed_through, COALESCE(
            (SELECT MIN(seq) - 1 FROM ops
             WHERE seq > pushed_through AND seq NOT IN (SELECT seq FROM sync_pushed)),
            (SELECT MAX(seq) FROM ops),
            pushed_through))",
        [],
    )?;
    conn.execute(
        "DELETE FROM sync_pushed WHERE seq <= (SELECT pushed_through FROM sync_state)",
        [],
    )?;
    Ok(())
}
