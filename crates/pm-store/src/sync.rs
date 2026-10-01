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
//! - **Seeded** (AGT-1396). `sync_state.seeded`: the hub is this
//!   workspace's authority — its seed ended (this replica ended it, or it
//!   joined a hub whose seed had ended). Until then `pm sync` runs the
//!   seed path; after, only push/pull ([`Store::mark_seeded`]).
//! - **Joining** (AGT-1396). [`Store::join_workspace`] makes the empty
//!   replica a second machine starts from: the `workspace` row with the
//!   hub's workspace id and no ops at all, so the first pull rebuilds
//!   everything — config included — from the hub's log.
//!
//! All of it is bookkeeping written directly, like `backup_target`: not
//! derived from the op log, untouched by `pm doctor --rebuild`.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};

use pm_core::{ApplyError, DocApplyError, Op, Payload};
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use serde::Serialize;
use ulid::Ulid;

use crate::Store;
use crate::codec::ulid;
use crate::commit::{Origin, commit_foreign_in, now_ms};
use crate::config::CONFIG_KINDS;
use crate::error::{Result, StoreError};
use crate::project::{commit_doc_edit_in, is_known_doc};
use crate::query::read_ops;

/// What [`Store::apply_pulled`] / [`Store::apply_pulled_page`] did with a
/// batch.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Pulled {
    /// Foreign ops committed into the log — this batch's, and any parked
    /// by an earlier page that landed in this one.
    pub applied: usize,
    /// Ops already present by `op_id` (this replica's own ops echoed back,
    /// a re-pulled batch, or a duplicate within the batch), already
    /// quarantined, or at or below the cursor — left alone.
    pub skipped: usize,
    /// Of `applied`, ops an earlier page had parked (AGT-1467).
    pub unparked: usize,
    /// This batch's ops still parked when it was committed, waiting on an
    /// op not pulled yet (AGT-1467; [`Store::apply_pulled_page`] only).
    pub parked: usize,
    /// Ops refused by this batch — this batch's, or parked ones given up
    /// on (AGT-1467; [`Store::apply_pulled_page`] only).
    pub refused: Vec<Quarantined>,
}

/// A pulled op kept out of the log (AGT-1467, [`Store::apply_pulled_page`]).
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Quarantined {
    pub op_id: Ulid,
    /// The op's seq in the hub log.
    pub hub_seq: i64,
    pub kind: String,
    pub entity: Ulid,
    pub status: QuarantineStatus,
    /// Retries it has had while parked.
    pub attempts: u32,
    /// Why it was parked (what it waits on) or refused.
    pub reason: String,
    /// When this replica recorded it (Unix ms); for a refused op, when it
    /// was refused.
    pub recorded_ms: u64,
    /// The op's content has been dropped (AGT-1482): a refused op older
    /// than [`QUARANTINE_RETENTION_MS`], or any refused op after `pm
    /// doctor --prune-quarantine`. Its id, seq, kind, entity and reason
    /// stay.
    pub pruned: bool,
}

/// How long a refused op's content stays in `sync_quarantine` (AGT-1482):
/// 30 days from its refusal. A refused op is never retried, so its body
/// only serves someone reading `pm doctor` about a recent sync; after that
/// it is another replica's text (comments, titles, description updates)
/// kept outside the op log for no one. Each pull drops what has aged out
/// ([`Store::apply_pulled_page`]); `pm doctor --prune-quarantine` drops
/// all of it now ([`Store::prune_quarantine`]). The row itself — op id,
/// hub seq, kind, entity, reason — stays: it is what keeps a re-served op
/// refused rather than applied.
pub const QUARANTINE_RETENTION_MS: u64 = 30 * 24 * 60 * 60 * 1000;

/// Where a quarantined op stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum QuarantineStatus {
    /// Waiting on an op not pulled yet; retried when one that could
    /// supply it lands.
    Parked,
    /// Can never apply here; kept for the record, never retried.
    Refused,
}

/// Most retries a parked op gets before it is refused (AGT-1467). Honest
/// logs park nothing — the hub serves every op after what it depends on —
/// so this only bounds the work an adversarial or corrupt log can cause.
pub const MAX_PARK_RETRIES: u32 = 32;

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
    /// The hub is this workspace's authority: its seed has ended
    /// (AGT-1396). `false` until the first `pm sync` seeds it, or joins
    /// a hub whose seed already ended.
    pub seeded: bool,
    /// Pulled ops parked, waiting on an op not pulled yet (AGT-1467).
    pub parked: u64,
    /// Pulled ops refused as inadmissible (AGT-1467).
    pub refused: u64,
}

impl Store {
    /// How many ops the log holds.
    pub fn op_count(&self) -> Result<u64> {
        let n: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM ops", [], |r| r.get(0))?;
        Ok(n as u64)
    }

    /// Whether this replica has ever had an op acknowledged by the hub:
    /// the pushed-through marker has moved, or some op above it is known
    /// to the hub. `false` on a workspace that has never synced (and on a
    /// joined replica before its first pull).
    pub fn ever_pushed(&self) -> Result<bool> {
        Ok(self.conn.query_row(
            "SELECT (SELECT pushed_through FROM sync_state) > 0
                 OR EXISTS (SELECT 1 FROM sync_pushed)",
            [],
            |r| r.get(0),
        )?)
    }

    /// Which of `op_ids` the log does not hold, in `op_ids` order. The
    /// seed's probe (AGT-1396): a hub workspace whose ops are all in this
    /// log was seeded from it; one holding ops this log lacks was not.
    pub fn unknown_ops(&self, op_ids: &[Ulid]) -> Result<Vec<Ulid>> {
        if op_ids.is_empty() {
            return Ok(Vec::new());
        }
        // One query: the ids as a JSON array, joined against the log.
        let wanted =
            serde_json::to_string(&op_ids.iter().map(Ulid::to_string).collect::<Vec<String>>())
                .expect("strings serialize");
        let mut stmt = self.conn.prepare(
            "SELECT value FROM json_each(?1)
             WHERE value NOT IN (SELECT op_id FROM ops)",
        )?;
        let rows = stmt.query_map(params![wanted], |r| r.get::<_, String>(0))?;
        let unknown: BTreeSet<Ulid> = rows
            .map(|row| ulid("op_id", &row?))
            .collect::<Result<_>>()?;
        Ok(op_ids
            .iter()
            .filter(|id| unknown.contains(id))
            .copied()
            .collect())
    }

    /// Whether the hub is this workspace's authority (its seed has ended).
    pub fn seeded(&self) -> Result<bool> {
        Ok(self
            .conn
            .query_row("SELECT seeded FROM sync_state", [], |r| r.get(0))?)
    }

    /// Records that the workspace's seed has ended on the hub: from now on
    /// `pm sync` pushes and pulls without seeding. Idempotent.
    pub fn mark_seeded(&mut self) -> Result<()> {
        self.conn.execute("UPDATE sync_state SET seeded = 1", [])?;
        Ok(())
    }

    /// Makes this empty database a replica of workspace `id` that has yet
    /// to pull anything (`pm init --join`, AGT-1396): the `workspace` row
    /// with that id and `prefix`, no states, no ops. The row is a
    /// placeholder the first pull overwrites — the hub's log carries the
    /// workspace's `workspace.set` and `state.upsert` ops, and
    /// materializing them rewrites the row in place — so until that pull
    /// the workspace opens but has no states to file a ticket into.
    /// Refuses a database that already has a workspace
    /// ([`StoreError::ForeignWorkspace`] when the ids differ,
    /// [`StoreError::AlreadyJoined`] when they match) or any op, and a
    /// `prefix` that is not safe in a file path
    /// ([`StoreError::InvalidId`], AGT-1467 — the same rule every
    /// `workspace.set prefix` op is held to, since path-building verbs
    /// read the row before the first pull rewrites it).
    pub fn join_workspace(&mut self, id: Ulid, prefix: &str) -> Result<()> {
        pm_core::ids::check_component("workspace prefix", prefix)?;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let existing: Option<String> = tx
            .query_row("SELECT id FROM workspace", [], |r| r.get(0))
            .optional()?;
        if let Some(existing) = existing {
            let workspace = ulid("workspace.id", &existing)?;
            return Err(if workspace == id {
                StoreError::AlreadyJoined { workspace }
            } else {
                StoreError::ForeignWorkspace {
                    entity: id,
                    workspace,
                }
            });
        }
        let ops: i64 = tx.query_row("SELECT COUNT(*) FROM ops", [], |r| r.get(0))?;
        if ops > 0 {
            return Err(StoreError::NotEmpty { ops: ops as u64 });
        }
        tx.execute(
            "INSERT INTO workspace (singleton, id, prefix, gate_labels, model_labels, template_sections, stale_days)
             VALUES (1, ?1, ?2, ?3, ?4, ?5, ?6)",
            params![id.to_string(), prefix, "[]", "{}", "[]", 30u32],
        )?;
        tx.commit()?;
        Ok(())
    }

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

    /// [`Store::outbox`] restricted to the config kinds (`workspace.set`,
    /// `state.upsert`, `actor.upsert`, `project.*`): what a seed pushes
    /// first (AGT-1396). Migrations 0007 and 0008 backfilled config ops
    /// for rows that predate them at the *end* of the log, after the
    /// ticket and document ops that depend on them; a replica applying
    /// the hub's log page by page needs every state, project and document
    /// binding before those, which is the order `pm doctor` replays in.
    pub fn outbox_config(&self, limit: usize) -> Result<Vec<(i64, Op)>> {
        read_ops(
            &self.conn,
            &format!(
                "WHERE seq IN (SELECT seq FROM ops
                               WHERE seq > (SELECT pushed_through FROM sync_state)
                                 AND seq NOT IN (SELECT seq FROM sync_pushed)
                                 AND kind IN ({CONFIG_KINDS})
                               ORDER BY seq LIMIT ?1)"
            ),
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

    /// Commits foreign ops through the normal apply path — the same
    /// merge, materialization and hygiene checks as [`Store::commit`] /
    /// [`Store::commit_doc_edit`] — in **one** transaction, all or
    /// nothing. The strict form, for ops the caller already knows the hub
    /// admitted (`pm claim`'s own claim, the seed end's number ops) and for
    /// tests; `pm sync`'s pull uses [`Store::apply_pulled_page`], which
    /// quarantines instead of failing. An op already present by `op_id`
    /// is skipped, so re-applying a batch (or receiving this replica's own
    /// ops back) is a no-op. Claims are not re-checked: the hub admitted
    /// them.
    ///
    /// Order-independent within the batch: an op whose ticket, relation
    /// target, document, project, project entity or state is not there
    /// yet — or a document or ticket-description edit whose predecessor
    /// edits are not — is parked and retried when an op that could
    /// supply it lands (the rules are [`Store::apply_pulled_page`]'s), so
    /// a batch applies to the same state in any order (pm-core's merge is
    /// order-independent; this makes the existence checks so too). Ops
    /// are appended to the local log in the order they actually landed,
    /// so `pm doctor`'s replay in `seq` order reproduces the tables.
    ///
    /// Every applied op counts as already pushed (it came from the hub),
    /// and a ticket that now has a number leaves the pending-number set.
    /// Any failure — an op [`Store::apply_pulled_page`] would refuse, or
    /// a dependency nothing in the batch supplies — rolls the whole batch
    /// back ([`StoreError::Pull`], naming the op).
    ///
    /// Every op's stamp is checked first (oaudit 2026-09-30): one out of
    /// the storable range, with a spent counter, or more than
    /// [`PULL_MAX_FUTURE_SKEW_MS`] ahead of this machine's clock fails the
    /// batch as [`StoreError::InvalidStamp`] (inside [`StoreError::Pull`])
    /// rather than panicking or poisoning the local clock. An op carrying
    /// a workspace prefix or project id unsafe in a file path fails it as
    /// [`StoreError::InvalidId`].
    pub fn apply_pulled(&mut self, ops: &[Op]) -> Result<Pulled> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut engine = Engine::new(&tx, Mode::Strict)?;
        for (index, op) in ops.iter().enumerate() {
            engine.process(index as i64, op)?;
        }
        let pulled = engine.finish()?;
        finish_pull(&tx)?;
        tx.commit()?;
        Ok(pulled)
    }

    /// Applies one page of the hub's log — `ops` as `(hub seq, op)` —
    /// and moves the pull cursor to `next`, in **one** transaction, so a
    /// page is never applied twice (AGT-1467). This is `pm sync`'s pull.
    ///
    /// Unlike [`Store::apply_pulled`], one op that cannot apply does not
    /// fail the page — which, since the hub serves the page again from the
    /// same cursor, would fail every later pull too. Each op, in hub seq
    /// order, ends up:
    ///
    /// - **applied** — committed into the log, as by `apply_pulled`;
    /// - **skipped** — already in the log (this replica's own op echoed
    ///   back), already quarantined, or at or below the cursor;
    /// - **parked** — it waits on something not here yet (a
    ///   [dependency](Store::apply_pulled)). It is kept out of the log in
    ///   `sync_quarantine` and retried, in hub seq order, whenever an op
    ///   that could supply it lands: any config op wakes every parked op,
    ///   and any other op wakes those waiting on its entity (a missing
    ///   ticket or relation target, the edits a `body.edit` builds on) or
    ///   targeting it. A parked op still blocked after
    ///   [`MAX_PARK_RETRIES`] retries is refused;
    /// - **refused** — it can never apply: an unsafe id, a stamp out of
    ///   range, an oversized `body.edit`, a duplicate `project.create`, a
    ///   document or ticket id already in use, a number already taken, an
    ///   update that does not decode. Recorded with its reason in
    ///   `sync_quarantine` for `pm doctor`, and never retried.
    ///
    /// **Convergence.** Every replica must end in the same state whatever
    /// it quarantines. That holds because what happens to an op is a pure
    /// function of the hub's log up to it: ops are processed one at a time
    /// in hub seq order whatever the page boundaries (the parked set, with
    /// each op's wake key and retry count, is persisted between pages, and
    /// the cursor moves in the same transaction); applying an op, and each
    /// retry it gets, is decided by the store's state, which is itself
    /// the result of that same processing; and an op this replica already
    /// holds (its own, echoed back) still wakes parked ops at its hub
    /// position, exactly as landing it does on every other replica. So
    /// two replicas that pulled the same log hold the same ops, parked
    /// the same ops and refused the same ops, and pm-core's
    /// order-independent merge gives them the same tables. Two inputs are
    /// deliberately *not* quarantined, because they are not functions of
    /// the log: a stamp more than [`PULL_MAX_FUTURE_SKEW_MS`] ahead of
    /// this machine's clock (whether it is depends on the clock; the pull
    /// fails, as before, and succeeds once the clock is right), and
    /// anything the store cannot write at all (a busy or corrupt
    /// database) — both fail the page and leave the cursor where it was.
    /// This replica's own ops are the one local input, and an honest log
    /// never lets them matter: an op naming an entity follows that
    /// entity's creation in the hub log (nobody else can know a fresh id
    /// before the hub serves it), so nothing is parked on an op this
    /// replica already holds. Only a log that names an entity before its
    /// creation reached the hub — a hostile one — can have ops parked on
    /// it here; retries against the local copy then decide them sooner
    /// than elsewhere, which changes retry counts and, in a contrived log
    /// that also holds a conflicting op, which of the two is refused.
    ///
    /// Bounded work (AGT-1467): each op is tried once on arrival and at
    /// most [`MAX_PARK_RETRIES`] times after, so a page costs at most
    /// `1 + MAX_PARK_RETRIES` attempts per op whatever order it arrives
    /// in (the old pass-until-nothing-lands loop was quadratic).
    ///
    /// `ops` may come in any order; it is sorted by hub seq. `next` is
    /// the page's `next`: the cursor becomes the greater of it and the
    /// current cursor.
    pub fn apply_pulled_page(&mut self, ops: &[(i64, Op)], next: i64) -> Result<Pulled> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let cursor: i64 = tx.query_row("SELECT pulled_seq FROM sync_state", [], |r| r.get(0))?;
        let mut page: Vec<&(i64, Op)> = ops.iter().collect();
        page.sort_by_key(|(seq, _)| *seq);
        let mut engine = Engine::new(&tx, Mode::Quarantine)?;
        let mut last = cursor;
        for (seq, op) in page {
            if *seq <= last {
                // At or below the cursor (processed by an earlier page), or
                // a repeat within this one.
                engine.pulled.skipped += 1;
                continue;
            }
            last = *seq;
            engine.process(*seq, op)?;
        }
        let pulled = engine.finish()?;
        tx.execute(
            "UPDATE sync_state SET pulled_seq = MAX(pulled_seq, ?1)",
            params![next],
        )?;
        finish_pull(&tx)?;
        prune_quarantine(&tx, now_ms().saturating_sub(QUARANTINE_RETENTION_MS))?;
        tx.commit()?;
        Ok(pulled)
    }

    /// Drops the content of every refused op in `sync_quarantine` now
    /// (`pm doctor --prune-quarantine`, AGT-1482), keeping each row's op
    /// id, hub seq, kind, entity and reason. Parked ops keep theirs: they
    /// are still retried. Returns how many rows it pruned. Changes nothing
    /// a pull decides: a refused op is never retried, and a re-served one
    /// is recognised by its `op_id` alone.
    pub fn prune_quarantine(&mut self) -> Result<u64> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let pruned = prune_quarantine(&tx, u64::MAX)?;
        tx.commit()?;
        Ok(pruned)
    }

    /// Every pulled op this replica has parked or refused
    /// ([`Store::apply_pulled_page`]), in hub seq order — for `pm doctor`.
    pub fn quarantine(&self) -> Result<Vec<Quarantined>> {
        quarantine(&self.conn)
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

/// Migration 0009's one step (AGT-1396): adds `sync_state.seeded` unless
/// the table already has it — SQLite has no `ADD COLUMN IF NOT EXISTS`,
/// and migrations from 0005 on must be re-runnable (`compact_bytes` test).
pub(crate) fn add_seeded_column(conn: &Connection) -> Result<()> {
    let has_seeded: bool = conn.query_row(
        "SELECT COUNT(*) > 0 FROM pragma_table_info('sync_state') WHERE name = 'seeded'",
        [],
        |r| r.get(0),
    )?;
    if !has_seeded {
        conn.execute_batch(
            "ALTER TABLE sync_state ADD COLUMN seeded INTEGER NOT NULL DEFAULT 0 CHECK (seeded IN (0, 1))",
        )?;
    }
    Ok(())
}

pub(crate) fn sync_status(conn: &Connection) -> Result<SyncStatus> {
    let (pushed_through, cursor, seeded) = conn.query_row(
        "SELECT pushed_through, pulled_seq, seeded FROM sync_state",
        [],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )?;
    let pending: i64 = conn.query_row("SELECT COUNT(*) FROM pending_number", [], |r| r.get(0))?;
    let (parked, refused): (i64, i64) = conn.query_row(
        "SELECT COUNT(*) FILTER (WHERE status = 'parked'),
                COUNT(*) FILTER (WHERE status = 'refused')
         FROM sync_quarantine",
        [],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    Ok(SyncStatus {
        pushed_through,
        outbox: outbox_len(conn)?,
        cursor,
        pending_numbers: pending as u64,
        seeded,
        parked: parked as u64,
        refused: refused as u64,
    })
}

/// Drops the content (`op`, set to `''`) of every refused op recorded at
/// or before `cutoff_ms` (AGT-1482); returns how many rows changed.
fn prune_quarantine(conn: &Connection, cutoff_ms: u64) -> Result<u64> {
    let cutoff = i64::try_from(cutoff_ms).unwrap_or(i64::MAX);
    let pruned = conn.execute(
        "UPDATE sync_quarantine SET op = ''
         WHERE status = 'refused' AND op <> '' AND recorded_ms <= ?1",
        params![cutoff],
    )?;
    Ok(pruned as u64)
}

pub(crate) fn quarantine(conn: &Connection) -> Result<Vec<Quarantined>> {
    let mut stmt = conn.prepare(
        "SELECT op_id, hub_seq, kind, entity, status, attempts, reason, recorded_ms, op = ''
         FROM sync_quarantine ORDER BY hub_seq, op_id",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok((
            (
                r.get::<_, String>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, String>(4)?,
            ),
            r.get::<_, u32>(5)?,
            r.get::<_, String>(6)?,
            r.get::<_, i64>(7)?,
            r.get::<_, bool>(8)?,
        ))
    })?;
    rows.map(|row| {
        let ((op_id, hub_seq, kind, entity, status), attempts, reason, recorded_ms, pruned) = row?;
        Ok(Quarantined {
            op_id: ulid("sync_quarantine.op_id", &op_id)?,
            hub_seq,
            kind,
            entity: ulid("sync_quarantine.entity", &entity)?,
            status: match status.as_str() {
                "parked" => QuarantineStatus::Parked,
                _ => QuarantineStatus::Refused,
            },
            attempts,
            reason,
            recorded_ms: recorded_ms.max(0) as u64,
            pruned,
        })
    })
    .collect()
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
    check_foreign_stamp(op, now_ms())?;
    match &op.payload {
        Payload::BodyEdit(_) if is_known_doc(tx, op.entity)? => {
            commit_doc_edit_in(tx, op.entity, op, Origin::Pulled)?;
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

/// How far ahead of this machine's clock a pulled op's stamp may be. Far
/// more lenient than the hub's push bound ([`pm_core::MAX_FUTURE_SKEW_MS`],
/// one day): the hub already refuses far-future pushes, and a replica
/// whose own clock runs behind must still pull honest ops. This only
/// stops an op the hub stored before it checked (or a hostile hub) from
/// dragging this replica's clock years ahead.
pub const PULL_MAX_FUTURE_SKEW_MS: u64 = 365 * pm_core::MAX_FUTURE_SKEW_MS;

/// The trust-boundary check on a foreign op (oaudit 2026-09-30): its
/// stamp storable, its counter not spent, and not absurdly far in the
/// future; and every identifier it carries safe in a file path (AGT-1450,
/// AGT-1453, AGT-1464). Refused as [`StoreError::InvalidStamp`] /
/// [`StoreError::InvalidId`] before the op reaches the log, a view or a
/// clock; the pull rolls back and names the op. The same check every
/// local commit and `pm backup --restore` runs
/// ([`crate::commit::check_ingest`]).
fn check_foreign_stamp(op: &Op, now_ms: u64) -> Result<()> {
    crate::commit::check_ingest_at(op, now_ms, Origin::Pulled)
}

/// What a pull does with an op that failed to apply (AGT-1467).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Disposition {
    /// Something another op may yet supply is missing: park it, to be
    /// woken as [`Wake`] says.
    Park(Wake),
    /// It can never apply: refuse it.
    Refuse,
    /// Not a property of the op: fail the whole page.
    Fail,
}

/// What a parked op waits for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Wake {
    /// Any config op (a project, state, or document binding).
    Config,
    /// The next op landed on this entity (a ticket's creation, or the
    /// edits a `body.edit` builds on).
    Entity(Ulid),
}

impl Wake {
    fn column(self) -> String {
        match self {
            Wake::Config => "*".to_string(),
            Wake::Entity(entity) => entity.to_string(),
        }
    }
}

/// Classifies a failed op. Exhaustive on purpose: a new [`StoreError`]
/// has to be placed deliberately, since parking or refusing an op that is
/// really a local failure (or failing on one that is the op's fault)
/// breaks either convergence or liveness. Dependencies wait on: a missing
/// ticket or relation target (its `ticket.create` — also how a `body.edit`
/// for a document not yet bound shows up, whose binding is a config op),
/// a document, project, project entity or state (config ops), or a
/// document (AGT-1413) or ticket-description (AGT-1415) edit ahead of the
/// edits it builds on (the next edit of that entity).
fn disposition(e: &StoreError, op: &Op) -> Disposition {
    use StoreError as E;
    match e {
        E::UnknownTicket { ticket } | E::UnknownRelationTarget { ticket } => {
            Disposition::Park(Wake::Entity(*ticket))
        }
        E::DocApply(DocApplyError::MissingDependency { .. })
        | E::Apply(ApplyError::MissingDependency { .. }) => {
            Disposition::Park(Wake::Entity(op.entity))
        }
        E::UnknownDocument { .. }
        | E::UnknownProject { .. }
        | E::UnknownProjectEntity { .. }
        | E::UnknownState { .. } => Disposition::Park(Wake::Config),
        // How far ahead is too far depends on this machine's clock, not on
        // the op: quarantining would make two replicas decide differently.
        E::InvalidStamp(
            pm_core::StampError::FarFuture { .. } | pm_core::StampError::PayloadFarFuture { .. },
        ) => Disposition::Fail,
        E::InvalidStamp(_)
        | E::InvalidId(_)
        | E::OpTooLarge(_)
        | E::NotADocumentEdit { .. }
        | E::DuplicateNumber { .. }
        | E::AlreadyNumbered { .. }
        | E::ClaimRejected(_)
        | E::Apply(_)
        | E::DocApply(_)
        | E::ConfigApply(_)
        | E::ForeignWorkspace { .. }
        | E::DuplicateProject { .. }
        | E::DuplicateDocument { .. }
        | E::DocIdInUse { .. }
        | E::EntityInUse { .. }
        | E::DuplicateProjectCreate { .. }
        | E::Body(_)
        | E::ProjectHasTickets { .. }
        | E::ProjectHasChildren { .. }
        // Only `Store::set_project` raises it, never an ingest path; were
        // it ever to, it is the op's own content, like the FK refusals.
        | E::ProjectCycle { .. } => Disposition::Refuse,
        // A constraint the op's own content violates is as deterministic
        // as a typed refusal; any other SQLite failure is the database's.
        E::Sqlite(rusqlite::Error::SqliteFailure(f, _))
            if f.code == rusqlite::ErrorCode::ConstraintViolation =>
        {
            Disposition::Refuse
        }
        E::Sqlite(_)
        | E::SchemaTooNew { .. }
        | E::NoWorkspace
        | E::DuplicateOp { .. }
        | E::AlreadyJoined { .. }
        | E::NotEmpty { .. }
        | E::NotAConfigOp { .. }
        | E::Corrupt { .. }
        | E::Replay { .. }
        | E::Pull { .. } => Disposition::Fail,
    }
}

/// How a pull treats an op that cannot apply.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    /// [`Store::apply_pulled`]: fail the batch.
    Strict,
    /// [`Store::apply_pulled_page`]: park or refuse it in
    /// `sync_quarantine` and carry on.
    Quarantine,
}

/// One parked op, as the engine tracks it; its body is in `Engine::ops`
/// or, parked by an earlier page, in `sync_quarantine`.
struct Parked {
    op_id: Ulid,
    entity: Ulid,
    wake: Wake,
    attempts: u32,
    /// Parked by an earlier page (loaded from `sync_quarantine`).
    earlier: bool,
}

/// What one attempt at an op did.
enum Attempt {
    Landed,
    Blocked(StoreError, Wake),
    Refused(StoreError),
}

/// The pull's apply loop (AGT-1467): each op in order, parking what waits
/// on a dependency and retrying it when something that could supply it
/// lands. Orders are hub seqs ([`Mode::Quarantine`]) or batch positions
/// ([`Mode::Strict`]).
struct Engine<'a, 'c> {
    tx: &'a Transaction<'c>,
    mode: Mode,
    /// The parked set, by order.
    parked: BTreeMap<i64, Parked>,
    /// Orders of the parked ops each entity may wake: their own entity,
    /// and the one a [`Wake::Entity`] names.
    by_entity: HashMap<Ulid, BTreeSet<i64>>,
    /// The parked ops' ids.
    parked_ids: HashSet<Ulid>,
    /// Parked ops' bodies, for those parked in this call.
    ops: HashMap<i64, Op>,
    /// [`Mode::Strict`]: why each parked op is blocked, for the error.
    blocked: HashMap<i64, StoreError>,
    pulled: Pulled,
}

impl<'a, 'c> Engine<'a, 'c> {
    fn new(tx: &'a Transaction<'c>, mode: Mode) -> Result<Self> {
        let mut engine = Engine {
            tx,
            mode,
            parked: BTreeMap::new(),
            by_entity: HashMap::new(),
            parked_ids: HashSet::new(),
            ops: HashMap::new(),
            blocked: HashMap::new(),
            pulled: Pulled::default(),
        };
        if mode == Mode::Quarantine {
            let mut stmt = tx.prepare(
                "SELECT hub_seq, op_id, entity, wake, attempts FROM sync_quarantine
                 WHERE status = 'parked'",
            )?;
            let rows = stmt.query_map([], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, Option<String>>(3)?,
                    r.get::<_, u32>(4)?,
                ))
            })?;
            for row in rows {
                let (order, op_id, entity, wake, attempts) = row?;
                let wake = match wake.as_deref() {
                    None | Some("*") => Wake::Config,
                    Some(entity) => Wake::Entity(ulid("sync_quarantine.wake", entity)?),
                };
                engine.index(
                    order,
                    Parked {
                        op_id: ulid("sync_quarantine.op_id", &op_id)?,
                        entity: ulid("sync_quarantine.entity", &entity)?,
                        wake,
                        attempts,
                        earlier: true,
                    },
                );
            }
        }
        Ok(engine)
    }

    fn index(&mut self, order: i64, parked: Parked) {
        self.by_entity
            .entry(parked.entity)
            .or_default()
            .insert(order);
        if let Wake::Entity(entity) = parked.wake {
            self.by_entity.entry(entity).or_default().insert(order);
        }
        self.parked_ids.insert(parked.op_id);
        self.parked.insert(order, parked);
    }

    fn unindex(&mut self, order: i64) -> Option<Parked> {
        let parked = self.parked.remove(&order)?;
        self.parked_ids.remove(&parked.op_id);
        let keys = match parked.wake {
            Wake::Entity(entity) => vec![parked.entity, entity],
            Wake::Config => vec![parked.entity],
        };
        for key in keys {
            if let Some(orders) = self.by_entity.get_mut(&key) {
                orders.remove(&order);
                if orders.is_empty() {
                    self.by_entity.remove(&key);
                }
            }
        }
        self.ops.remove(&order);
        self.blocked.remove(&order);
        Some(parked)
    }

    /// One incoming op at `order`.
    fn process(&mut self, order: i64, op: &Op) -> Result<()> {
        if let Some(seq) = seq_of(self.tx, op.op_id)? {
            // Already in the log (this replica's own, echoed back): on
            // every other replica it lands here, so it wakes here too.
            mark_seq_pushed(self.tx, seq)?;
            self.pulled.skipped += 1;
            return self.cascade(op);
        }
        if self.is_quarantined(op.op_id)? {
            self.pulled.skipped += 1;
            return Ok(());
        }
        match self.attempt(op)? {
            Attempt::Landed => {
                self.pulled.applied += 1;
                self.cascade(op)
            }
            Attempt::Blocked(e, wake) => self.park(order, op, wake, e),
            Attempt::Refused(e) => self.refuse(order, op, 0, e.to_string(), e),
        }
    }

    fn is_quarantined(&self, op_id: Ulid) -> Result<bool> {
        if self.parked_ids.contains(&op_id) {
            return Ok(true);
        }
        Ok(self.mode == Mode::Quarantine
            && crate::commit::exists(
                self.tx,
                "SELECT 1 FROM sync_quarantine WHERE op_id = ?1",
                &op_id.to_string(),
            )?)
    }

    /// Tries `op` inside a savepoint: landed (and marked pushed), blocked
    /// on a dependency, or refused — or the page's error.
    fn attempt(&self, op: &Op) -> Result<Attempt> {
        self.tx.execute_batch("SAVEPOINT pulled_op")?;
        match apply_foreign(self.tx, op) {
            Ok(()) => {
                self.tx.execute_batch("RELEASE pulled_op")?;
                let seq = seq_of(self.tx, op.op_id)?.expect("the op was just appended");
                mark_seq_pushed(self.tx, seq)?;
                Ok(Attempt::Landed)
            }
            Err(e) => {
                self.tx
                    .execute_batch("ROLLBACK TO pulled_op; RELEASE pulled_op")?;
                match disposition(&e, op) {
                    Disposition::Park(wake) => Ok(Attempt::Blocked(e, wake)),
                    Disposition::Refuse => Ok(Attempt::Refused(e)),
                    Disposition::Fail => Err(pull_error(op, e)),
                }
            }
        }
    }

    fn park(&mut self, order: i64, op: &Op, wake: Wake, e: StoreError) -> Result<()> {
        if self.mode == Mode::Quarantine {
            self.tx.execute(
                "INSERT INTO sync_quarantine
                     (op_id, hub_seq, kind, entity, status, wake, attempts, reason, recorded_ms, op)
                 VALUES (?1, ?2, ?3, ?4, 'parked', ?5, 0, ?6, ?7, ?8)",
                params![
                    op.op_id.to_string(),
                    order,
                    op.kind(),
                    op.entity.to_string(),
                    wake.column(),
                    e.to_string(),
                    now_ms() as i64,
                    serde_json::to_string(op).expect("an op serializes"),
                ],
            )?;
        } else {
            self.blocked.insert(order, e);
        }
        self.ops.insert(order, op.clone());
        self.index(
            order,
            Parked {
                op_id: op.op_id,
                entity: op.entity,
                wake,
                attempts: 0,
                earlier: false,
            },
        );
        Ok(())
    }

    /// Refuses `op` for good: fails the batch ([`Mode::Strict`]), or
    /// records it ([`Mode::Quarantine`]).
    fn refuse(
        &mut self,
        order: i64,
        op: &Op,
        attempts: u32,
        reason: String,
        e: StoreError,
    ) -> Result<()> {
        if self.mode == Mode::Strict {
            return Err(pull_error(op, e));
        }
        let recorded_ms = now_ms();
        self.tx.execute(
            "INSERT INTO sync_quarantine
                 (op_id, hub_seq, kind, entity, status, wake, attempts, reason, recorded_ms, op)
             VALUES (?1, ?2, ?3, ?4, 'refused', NULL, ?5, ?6, ?7, ?8)
             ON CONFLICT(op_id) DO UPDATE SET
                 status = 'refused', wake = NULL, attempts = excluded.attempts,
                 reason = excluded.reason, recorded_ms = excluded.recorded_ms",
            params![
                op.op_id.to_string(),
                order,
                op.kind(),
                op.entity.to_string(),
                attempts,
                reason,
                recorded_ms as i64,
                serde_json::to_string(op).expect("an op serializes"),
            ],
        )?;
        self.pulled.refused.push(Quarantined {
            op_id: op.op_id,
            hub_seq: order,
            kind: op.kind().to_string(),
            entity: op.entity,
            status: QuarantineStatus::Refused,
            attempts,
            reason,
            recorded_ms,
            pruned: false,
        });
        Ok(())
    }

    /// The parked op at `order`'s body.
    fn load(&self, order: i64, op_id: Ulid) -> Result<Op> {
        if let Some(op) = self.ops.get(&order) {
            return Ok(op.clone());
        }
        let text: String = self.tx.query_row(
            "SELECT op FROM sync_quarantine WHERE op_id = ?1",
            params![op_id.to_string()],
            |r| r.get(0),
        )?;
        serde_json::from_str(&text).map_err(|e| StoreError::corrupt("sync_quarantine.op")(&e))
    }

    /// `landed` is now in the log: retry every parked op it may unblock,
    /// in order, and whatever those unblock in turn.
    fn cascade(&mut self, landed: &Op) -> Result<()> {
        if self.parked.is_empty() {
            return Ok(());
        }
        let mut queue: VecDeque<(bool, Ulid)> =
            VecDeque::from([(landed.payload.is_config(), landed.entity)]);
        while let Some((config, entity)) = queue.pop_front() {
            let woken: Vec<i64> = if config {
                self.parked.keys().copied().collect()
            } else {
                self.by_entity
                    .get(&entity)
                    .map(|orders| orders.iter().copied().collect())
                    .unwrap_or_default()
            };
            for order in woken {
                let Some(parked) = self.parked.get_mut(&order) else {
                    continue; // landed or refused earlier in this cascade
                };
                parked.attempts += 1;
                let (op_id, attempts, earlier) = (parked.op_id, parked.attempts, parked.earlier);
                let op = self.load(order, op_id)?;
                let outcome = match seq_of(self.tx, op_id)? {
                    // Landed some other way meanwhile (`pm claim`'s strict
                    // apply): it is in the log, so it is no longer parked.
                    Some(seq) => {
                        mark_seq_pushed(self.tx, seq)?;
                        None
                    }
                    None => Some(self.attempt(&op)?),
                };
                match outcome {
                    None | Some(Attempt::Landed) => {
                        self.unindex(order);
                        self.forget(op_id)?;
                        if outcome.is_some() {
                            self.pulled.applied += 1;
                            if earlier {
                                self.pulled.unparked += 1;
                            }
                        }
                        queue.push_back((op.payload.is_config(), op.entity));
                    }
                    Some(Attempt::Blocked(e, _)) if attempts >= MAX_PARK_RETRIES => {
                        self.unindex(order);
                        let reason = format!("still waiting after {attempts} retries: {e}");
                        self.refuse(order, &op, attempts, reason, e)?;
                    }
                    Some(Attempt::Blocked(e, wake)) => {
                        let mut parked = self.unindex(order).expect("it was parked");
                        parked.wake = wake;
                        if self.mode == Mode::Quarantine {
                            self.tx.execute(
                                "UPDATE sync_quarantine SET wake = ?2, attempts = ?3, reason = ?4
                                 WHERE op_id = ?1",
                                params![op_id.to_string(), wake.column(), attempts, e.to_string()],
                            )?;
                        } else {
                            self.blocked.insert(order, e);
                        }
                        self.ops.insert(order, op);
                        self.index(order, parked);
                    }
                    Some(Attempt::Refused(e)) => {
                        self.unindex(order);
                        self.refuse(order, &op, attempts, e.to_string(), e)?;
                    }
                }
            }
        }
        Ok(())
    }

    fn forget(&self, op_id: Ulid) -> Result<()> {
        if self.mode == Mode::Quarantine {
            self.tx.execute(
                "DELETE FROM sync_quarantine WHERE op_id = ?1",
                params![op_id.to_string()],
            )?;
        }
        Ok(())
    }

    /// What the batch did. [`Mode::Strict`]: an op still parked is the
    /// batch's error (the first, in batch order).
    fn finish(mut self) -> Result<Pulled> {
        if self.mode == Mode::Strict
            && let Some((&order, _)) = self.parked.iter().next()
        {
            let op = self.ops.remove(&order).expect("a strict parked op is held");
            let e = self
                .blocked
                .remove(&order)
                .expect("a strict parked op records its blocker");
            return Err(pull_error(&op, e));
        }
        self.pulled.parked = self.parked.values().filter(|p| !p.earlier).count();
        Ok(self.pulled)
    }
}

/// What every pull does once its ops are in: tickets that now have a
/// number leave the pending set, and the pushed marker catches up.
fn finish_pull(tx: &Transaction<'_>) -> Result<()> {
    tx.execute(
        "DELETE FROM pending_number
         WHERE ticket IN (SELECT id FROM ticket WHERE number IS NOT NULL)",
        [],
    )?;
    fold_pushed(tx)
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
