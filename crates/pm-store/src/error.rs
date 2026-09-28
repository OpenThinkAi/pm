//! Typed failures. Constraint violations that the hygiene rules name
//! (R2, R4, R5) get their own variants; anything SQLite rejects that the
//! store did not anticipate surfaces as [`StoreError::Sqlite`].

use pm_core::{ApplyError, ClaimRejected};
use rusqlite::ErrorCode;
use ulid::Ulid;

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
    #[error("schema version {found} is newer than this build supports ({supported})")]
    SchemaTooNew { found: u32, supported: u32 },
    #[error("op {op_id} is already in the log")]
    DuplicateOp { op_id: Ulid },
    #[error("ticket {ticket} does not exist; only ticket.create can start a ticket")]
    UnknownTicket { ticket: Ulid },
    #[error("state '{state}' is not a workflow state of this workspace")]
    UnknownState { state: String },
    #[error("project '{project}' does not exist (R2)")]
    UnknownProject { project: String },
    #[error("related ticket {ticket} does not exist (R4)")]
    UnknownRelationTarget { ticket: Ulid },
    #[error("number {number} is already taken (R5)")]
    DuplicateNumber { number: u64 },
    #[error("ticket {ticket} already has number {number}")]
    AlreadyNumbered { ticket: Ulid, number: u64 },
    #[error("claim rejected: {0}")]
    ClaimRejected(#[from] ClaimRejected),
    #[error(transparent)]
    Apply(#[from] ApplyError),
    /// A stored column no longer decodes (a JSON blob or a ULID). Only a
    /// foreign writer or a schema bug can produce this.
    #[error("stored {what} is corrupt: {detail}")]
    Corrupt { what: &'static str, detail: String },
    /// `pm doctor --rebuild` could not re-apply a logged op; the tables
    /// were left as they were. Only a foreign writer (an edited op, a
    /// dropped project or state) can produce this.
    #[error("replaying op #{seq} ({op_id}, {kind}) failed: {source}")]
    Replay {
        seq: i64,
        op_id: Ulid,
        kind: &'static str,
        #[source]
        source: Box<StoreError>,
    },
}

impl StoreError {
    /// SQLite could not get the lock in time (`SQLITE_BUSY` / `SQLITE_LOCKED`).
    /// Nothing was written; the same call can simply be retried.
    pub fn is_busy(&self) -> bool {
        matches!(
            self,
            StoreError::Sqlite(rusqlite::Error::SqliteFailure(e, _))
                if e.code == ErrorCode::DatabaseBusy || e.code == ErrorCode::DatabaseLocked
        )
    }

    pub(crate) fn corrupt(what: &'static str) -> impl FnOnce(&dyn std::fmt::Display) -> Self {
        move |detail| StoreError::Corrupt {
            what,
            detail: detail.to_string(),
        }
    }
}

pub type Result<T> = std::result::Result<T, StoreError>;

/// Names the number when SQLite rejected a ticket row on
/// `ticket.number`'s UNIQUE — the one rule (R5) the store cannot pre-check
/// without racing another writer.
pub(crate) fn map_duplicate_number(err: rusqlite::Error, number: Option<u64>) -> StoreError {
    if let (rusqlite::Error::SqliteFailure(e, Some(msg)), Some(number)) = (&err, number)
        && e.code == ErrorCode::ConstraintViolation
        && msg.contains("ticket.number")
    {
        return StoreError::DuplicateNumber { number };
    }
    StoreError::Sqlite(err)
}
