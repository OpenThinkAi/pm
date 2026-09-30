//! `pm-core`: pure domain types, op types, merge rules and HLC logic,
//! shared by the `pm` CLI and (later) `pm-hub` so merge semantics are
//! identical everywhere.
//!
//! This crate must never perform IO: no `rusqlite`, no filesystem access,
//! no network, no system clock (the HLC takes `now_ms` as an argument).
//! `crates/pm/tests/pm_core_purity.rs` enforces that mechanically by
//! checking this crate's `Cargo.toml` against a denylist and scanning
//! this crate's `src/` for IO modules.
//!
//! Layout (projects/pm/README.md §Data model, §Op log, §Conflict semantics):
//! - [`archive`] — selection logic for `pm archive --auto` (AGT-1351)
//! - [`body`] — ticket/project-doc text as a Loro CRDT (AGT-1338)
//! - [`check`] — the `pm check` invariants, over a ticket snapshot (AGT-1342)
//! - [`config`] — [`WorkspaceView`] and [`ProjectView`]: workspace config,
//!   states, actors and project metadata folded from the config op kinds
//!   (AGT-1384, decision A4)
//! - [`doc`] — [`DocView`], the same text CRDT keyed for a project document
//!   rather than a ticket (AGT-1344)
//! - [`domain`] — entity types (`Ticket`, `Project`, markers, …)
//! - [`markers`] — strict marker dates, waiver rules (AGT-1342)
//! - [`hlc`] — hybrid logical clock and the `(hlc, actor)` [`Stamp`]
//! - [`op`] — the [`Op`] envelope and every payload
//! - [`bytes`] — byte payloads as base64 on the wire, legacy arrays on read (AGT-1378)
//! - [`merge`] — LWW register, OR-set, append-only comment log
//! - [`ready`] — the ready frontier: verdicts, reasons and waves (AGT-1343)
//! - [`view`] — [`TicketView`] and the pure [`apply`] function

pub mod archive;
pub mod body;
pub mod bytes;
pub mod check;
pub mod config;
pub mod doc;
pub mod domain;
pub mod hlc;
pub mod markers;
pub mod merge;
pub mod op;
pub mod ready;
pub mod view;

pub use archive::{month_key, project_idle, ticket_archivable};
pub use body::{Body, BodyError, BodyUpdate};
pub use check::Finding;
pub use config::{
    ConfigApplyError, DocClaims, ProjectView, WorkspaceView, apply_project, apply_workspace,
};
pub use doc::{DocApplyError, DocView, apply_doc, apply_doc_persisted};
pub use domain::{
    Actor, ActorId, ActorKind, Comment, DocsOwner, Hold, NotBefore, Parked, Priority, Project,
    ProjectStatus, Relation, RelationKind, Source, State, StateCategory, Ticket, Waiver, Workspace,
};
pub use hlc::{Clock, Hlc, Stamp};
pub use op::{OP_VERSION, Op, Payload};
pub use view::{ApplyError, BodyState, ClaimRejected, TicketView, apply, apply_persisted};

/// This crate's own version, exposed so dependents can sanity-check they
/// are linked against the crate they expect.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_is_not_empty() {
        assert!(!VERSION.is_empty());
    }
}
