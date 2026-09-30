//! Typed failures. Constraint violations that the hygiene rules name
//! (R2, R4, R5) get their own variants; anything SQLite rejects that the
//! store did not anticipate surfaces as [`StoreError::Sqlite`].

use pm_core::{ApplyError, ClaimRejected, ConfigApplyError, DocApplyError};
use rusqlite::ErrorCode;
use ulid::Ulid;

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
    #[error("schema version {found} is newer than this build supports ({supported})")]
    SchemaTooNew { found: u32, supported: u32 },
    #[error("the database has no workspace; run `pm init`")]
    NoWorkspace,
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
    #[error(transparent)]
    DocApply(#[from] DocApplyError),
    #[error(transparent)]
    ConfigApply(#[from] ConfigApplyError),
    /// A `project.set` for a project Ulid with no `project.create` yet
    /// (AGT-1385). Within a pulled batch this defers until the create
    /// lands; on its own it is a caller bug or a foreign writer.
    #[error("project {project} does not exist; only project.create can start a project")]
    UnknownProjectEntity { project: Ulid },
    /// A workspace op (or `init_workspace`) for a workspace id other than
    /// this database's — one workspace per database (AGT-1385).
    #[error("op targets workspace {entity}, but this database is workspace {workspace}")]
    ForeignWorkspace { entity: Ulid, workspace: Ulid },
    /// `join_workspace` (AGT-1396) on a database that already is that
    /// workspace.
    #[error("this database already is workspace {workspace}")]
    AlreadyJoined { workspace: Ulid },
    /// `join_workspace` on a database that already holds ops: a joined
    /// replica starts empty and takes its whole log from the hub.
    #[error("this database already holds {ops} op(s); a joined replica must start empty")]
    NotEmpty { ops: u64 },
    /// A ticket op reached the config path. Only a routing bug in this
    /// crate can produce it.
    #[error("op {op_id}: '{kind}' is not a config op")]
    NotAConfigOp { op_id: Ulid, kind: &'static str },
    /// AGT-1344 AC1: `pm project new` against an id that already exists.
    #[error("project '{id}' already exists")]
    DuplicateProject { id: String },
    /// AGT-1344 AC3: `pm project doc add` against a name already taken.
    #[error("document '{name}' already exists on project '{project}'")]
    DuplicateDocument { project: String, name: String },
    /// A `body.edit` targeting a `doc_id` no project has ever bound
    /// (`project_doc_owner`, AGT-1413). Within a pulled batch this defers
    /// until the `project.create` / `project.doc_add` binding it lands.
    #[error("document {doc_id} does not belong to any project")]
    UnknownDocument { doc_id: Ulid },
    /// A `project.create` / `project.doc_add` binding a `doc_id` another
    /// project or document already has (AGT-1413). Only a foreign writer
    /// reuses one: every writer mints a fresh id.
    #[error("document id {doc_id} is already bound to another document")]
    DocIdInUse { doc_id: Ulid },
    /// A document's text could not be turned into a `body.edit`.
    #[error(transparent)]
    Body(#[from] pm_core::BodyError),
    /// AGT-1344 AC4 (R-style FK): a project cannot be deleted while a
    /// ticket still references it.
    #[error("project '{project}' has tickets; move or delete them first")]
    ProjectHasTickets { project: String },
    /// Same rule, for a child project's `parent` reference.
    #[error("project '{project}' has child projects; reparent or delete them first")]
    ProjectHasChildren { project: String },
    /// A stamp that is not admissible: out of the storable range, or (on
    /// a pull) too far ahead of this machine's clock (oaudit 2026-09-30,
    /// see [`crate::Store::apply_pulled`]).
    #[error("invalid stamp: {0}")]
    InvalidStamp(#[from] pm_core::StampError),
    /// A foreign op carrying a workspace prefix or project id that is not
    /// safe in a file path (`pm_core::ids`, AGT-1450).
    #[error(transparent)]
    InvalidId(#[from] pm_core::ids::IdError),
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
    /// [`crate::Store::apply_pulled`] could not commit a foreign op; the
    /// whole pulled batch was rolled back. A dependency error (an unknown
    /// ticket, relation target, document, project, project entity or
    /// state) here means nothing in the batch — or already in the store —
    /// supplied it.
    #[error("applying pulled op {op_id} ({kind}) failed: {source}")]
    Pull {
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
