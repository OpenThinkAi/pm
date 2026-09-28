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
//! - `config` — workspace + states, projects + docs
//! - `check` — the snapshot `pm check` runs over ([`Store::check`])
//! - `doctor` — [`Store::doctor`] (verify) and [`Store::rebuild`] (replay the log)
//! - `backup` — [`Store::ops_since`] and per-target progress (AGT-1350)
//! - `project` — project document bodies as `body.edit` ops (AGT-1344):
//!   creation, named documents, deletion (refused with live tickets), and
//!   the doc-replay `doctor`/`rebuild` fold in
//! - `import` — the number allocator's floor and project upserts for
//!   `pm import vault` (AGT-1347)
//! - [`StoreError`] — typed failures (R2/R4/R5 violations, claim rejection, …)

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

use std::path::Path;
use std::time::Duration;

use rusqlite::{Connection, TransactionBehavior};

pub use backup::BackupStatus;
pub use doctor::{
    ColumnChange, Diff, ForeignKeyViolation, PROJECT_DOC_TABLES, Report, Row, RowChange,
    TICKET_TABLES, TableDiff,
};
pub use error::{Result, StoreError};
pub use query::TicketFilter;
pub use ready::ReadyQuery;

/// Embedded migrations, in order. Each runs once, inside its own
/// transaction, and is recorded in `schema_version`.
const MIGRATIONS: &[(u32, &str)] = &[
    (1, include_str!("../migrations/0001_schema_v1.sql")),
    (2, include_str!("../migrations/0002_backup_agt1350.sql")),
    (3, include_str!("../migrations/0003_project_doc_bodies.sql")),
    (4, include_str!("../migrations/0004_number_floor.sql")),
];

/// The newest schema version this build understands.
pub const SCHEMA_VERSION: u32 = 4;

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
            tx.execute(
                "INSERT INTO schema_version (version) VALUES (?1)",
                [version],
            )?;
            tx.commit()?;
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
