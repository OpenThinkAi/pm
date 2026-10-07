//! `pm-store`: the SQLite op log and materialized tables behind `pm`
//! (projects/pm/README.md §Op log; AGT-1335).
//!
//! Every mutation is a [`pm_core::Op`]. [`Store::commit`] appends it to
//! `ops` and rewrites the ticket's rows in **one transaction**, so reads are
//! plain queries and the log and the tables can never disagree. The merge
//! rules run in pm-core: the store loads the ticket's persisted
//! [`pm_core::TicketView`], calls [`pm_core::apply`], and writes the
//! snapshot back — nothing CRDT-shaped lives in SQL.
//!
//! Layout:
//! - `migrations/*.sql` — versioned schema, applied by [`Store::open`]
//! - `commit` — [`Store::commit`], [`Store::allocate_number`], materialization
//! - `query` — by id / number / filtered list, comments, relations, ops
//! - `ready` — [`Store::ready`] / [`Store::frontier`], the ready frontier (`pm claim --ready`, `pm ready`)
//! - `config` — workspace + states + actors and project metadata as
//!   config ops (AGT-1385): the commit/replay path for `workspace.set`,
//!   `state.upsert`, `actor.upsert`, `project.create`, `project.set`, and
//!   the diff-based writers (`init_workspace`, `put_project`, …)
//! - `backfill` — migrations 0007's and 0008's one-offs: config ops for
//!   every row a pre-AGT-1385 database wrote directly, and `project.doc_add`
//!   ops for every document identity a pre-AGT-1413 one did
//! - `check` — the snapshot `pm check` runs over ([`Store::check`])
//! - `doctor` — [`Store::doctor`] (verify) and [`Store::rebuild`] (replay the log)
//! - `backup` — [`Store::ops_since`] and per-target progress (AGT-1350)
//! - `project` — project document bodies as `body.edit` ops (AGT-1344):
//!   creation, named documents, deletion (refused with live tickets), and
//!   the doc-replay `doctor`/`rebuild` fold in
//! - `import` — the number allocator's floor and project upserts for
//!   `pm import vault` (AGT-1347)
//! - `reencode` — migration 0005's in-place rewrite of byte payloads from
//!   JSON arrays to base64 (AGT-1378)
//! - `sync` — client sync state (AGT-1393): the outbox of ops the hub has
//!   not acknowledged, the pull cursor, [`Store::apply_pulled`] for foreign
//!   ops, the pending-number marker for tickets awaiting a hub number, and
//!   (AGT-1396) the seeded flag plus the seed's helpers — which of a set of
//!   op ids the log lacks, and [`Store::join_workspace`], the empty replica
//!   a second machine starts from; and (AGT-1467) the quarantine of pulled
//!   ops that cannot apply ([`Store::apply_pulled_page`])
//! - [`StoreError`] — typed failures (R2/R4/R5 violations, claim rejection, …)

mod backfill;
mod backup;
mod check;
mod codec;
mod commit;
mod config;
mod doctor;
mod error;
mod import;
mod project;
mod query;
mod ready;
mod reencode;
mod sync;

use std::path::Path;
use std::time::Duration;

use rusqlite::{Connection, TransactionBehavior};

pub use backfill::MIGRATE_ACTOR;
pub use backup::BackupStatus;
pub use commit::LOCAL_MAX_FUTURE_SKEW_MS;
pub use config::{DocIds, project_diff, workspace_diff};
pub use doctor::{
    CONFIG_TABLES, ColumnChange, Diff, ForeignKeyViolation, PROJECT_DOC_TABLES, Report, Row,
    RowChange, TICKET_TABLES, TableDiff,
};
pub use error::{Result, StoreError};
pub use query::TicketFilter;
pub use ready::ReadyQuery;
pub use sync::{
    MAX_PARK_RETRIES, PULL_MAX_FUTURE_SKEW_MS, Pulled, QUARANTINE_RETENTION_MS, QuarantineStatus,
    Quarantined, SyncStatus,
};

/// Embedded migrations, in order. Each runs once, inside its own
/// transaction, and is recorded in `schema_version`.
const MIGRATIONS: &[(u32, &str)] = &[
    (1, include_str!("../migrations/0001_schema_v1.sql")),
    (2, include_str!("../migrations/0002_backup_agt1350.sql")),
    (3, include_str!("../migrations/0003_project_doc_bodies.sql")),
    (4, include_str!("../migrations/0004_number_floor.sql")),
    (5, include_str!("../migrations/0005_compact_bytes.sql")),
    (6, include_str!("../migrations/0006_sync_state.sql")),
    (7, include_str!("../migrations/0007_config_ops.sql")),
    (8, include_str!("../migrations/0008_doc_identity.sql")),
    (9, include_str!("../migrations/0009_sync_seeded.sql")),
    (10, include_str!("../migrations/0010_docs_owned_by.sql")),
    (11, include_str!("../migrations/0011_reserialize_views.sql")),
    (12, include_str!("../migrations/0012_sync_quarantine.sql")),
    (13, include_str!("../migrations/0013_project_kind.sql")),
    (14, include_str!("../migrations/0014_project_parked.sql")),
];

/// The newest schema version this build understands.
pub const SCHEMA_VERSION: u32 = 14;

/// The migration whose work is Rust, not SQL: after its (comment-only)
/// SQL file runs, [`reencode::run`] rewrites every stored byte payload in
/// the same transaction (AGT-1378).
const COMPACT_BYTES_VERSION: u32 = 5;

/// Likewise for the config backfill: after its SQL adds the view tables
/// and `project.ulid`, [`backfill::run`] appends a config op for every
/// existing workspace/state/actor/project row (AGT-1385).
const CONFIG_OPS_VERSION: u32 = 7;

/// And for document identity: after its SQL adds `project_doc_owner`,
/// [`backfill::doc_identity`] appends a `project.doc_add` for every
/// existing project document (AGT-1413).
const DOC_IDENTITY_VERSION: u32 = 8;

/// And for the seeded flag (AGT-1396): its (comment-only) SQL file is
/// followed by [`sync::add_seeded_column`], which adds
/// `sync_state.seeded` only if it is not there yet, so the migration can
/// run again against a database that already has it.
const SYNC_SEEDED_VERSION: u32 = 9;

/// And for `workspace.docs_owned_by` (AGT-1406): the column is added by
/// [`config::add_docs_owned_by_column`], idempotently.
const DOCS_OWNED_BY_VERSION: u32 = 10;

/// And for stored view rows (AGT-1436): its (comment-only) SQL file is
/// followed by [`reencode::views`], which re-serializes every view row
/// through the current types; a no-op on rows already in that form.
const RESERIALIZE_VIEWS_VERSION: u32 = 11;

/// And for `project.kind` (AGT-1488): the column is added by
/// [`config::add_project_kind_column`], idempotently.
const PROJECT_KIND_VERSION: u32 = 13;

/// And for `parked` (AGT-1635): `project.status`'s CHECK is widened by
/// [`config::allow_parked_project_status`], idempotently.
const PROJECT_PARKED_VERSION: u32 = 14;

/// How long a writer waits for the database lock before giving up. Sized
/// for many concurrent CLI invocations (build loops fan out), not for a
/// server.
const BUSY_TIMEOUT: Duration = Duration::from_secs(10);

/// One open database. Not `Sync`: each thread opens its own `Store`.
pub struct Store {
    conn: Connection,
}

impl Store {
    /// Opens (creating if needed) the database at `path` and brings its
    /// schema up to [`SCHEMA_VERSION`]. The connection runs in WAL mode
    /// with foreign keys enforced and a busy timeout, so concurrent
    /// writers queue instead of failing.
    pub fn open(path: impl AsRef<Path>) -> Result<Store> {
        let conn = Connection::open(path)?;
        conn.busy_timeout(BUSY_TIMEOUT)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        let mut store = Store { conn };
        store.migrate()?;
        Ok(store)
    }

    /// The schema version recorded in the database (0 before the first
    /// migration).
    pub fn schema_version(&self) -> Result<u32> {
        Ok(self.conn.query_row(
            "SELECT COALESCE(MAX(version), 0) FROM schema_version",
            [],
            |r| r.get(0),
        )?)
    }

    fn migrate(&mut self) -> Result<()> {
        self.conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS schema_version (
                version    INTEGER PRIMARY KEY,
                applied_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
            )",
        )?;
        let current = self.schema_version()?;
        if current > SCHEMA_VERSION {
            return Err(StoreError::SchemaTooNew {
                found: current,
                supported: SCHEMA_VERSION,
            });
        }
        for (version, sql) in MIGRATIONS.iter().filter(|(v, _)| *v > current) {
            let tx = self
                .conn
                .transaction_with_behavior(TransactionBehavior::Immediate)?;
            tx.execute_batch(sql)?;
            let mut rewrote_rows = false;
            if *version == COMPACT_BYTES_VERSION {
                rewrote_rows = reencode::run(&tx)? != reencode::Rewritten::default();
            }
            if *version == CONFIG_OPS_VERSION {
                backfill::run(&tx)?;
            }
            if *version == DOC_IDENTITY_VERSION {
                backfill::doc_identity(&tx)?;
            }
            if *version == SYNC_SEEDED_VERSION {
                sync::add_seeded_column(&tx)?;
            }
            if *version == DOCS_OWNED_BY_VERSION {
                config::add_docs_owned_by_column(&tx)?;
            }
            if *version == RESERIALIZE_VIEWS_VERSION {
                reencode::views(&tx)?;
            }
            if *version == PROJECT_KIND_VERSION {
                config::add_project_kind_column(&tx)?;
            }
            if *version == PROJECT_PARKED_VERSION {
                config::allow_parked_project_status(&tx)?;
            }
            tx.execute(
                "INSERT INTO schema_version (version) VALUES (?1)",
                [version],
            )?;
            tx.commit()?;
            if rewrote_rows {
                // The rewrite shrank every touched row but SQLite keeps
                // the freed pages (the live 154 MB database came out of
                // it at 174 MB); VACUUM cannot run inside the migration's
                // transaction, so it follows it. Skipped when nothing was
                // rewritten (every fresh database), where it would only
                // cost a file rewrite.
                self.conn.execute_batch("VACUUM")?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migrations_are_ordered_and_end_at_the_supported_version() {
        let versions: Vec<u32> = MIGRATIONS.iter().map(|(v, _)| *v).collect();
        assert!(versions.windows(2).all(|w| w[0] < w[1]), "{versions:?}");
        assert_eq!(versions.last(), Some(&SCHEMA_VERSION));
    }
}
