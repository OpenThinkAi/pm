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
//! - [`body`] — ticket/project-doc text as a Loro CRDT (AGT-1338)
//! - [`domain`] — entity types (`Ticket`, `Project`, markers, …)
//! - [`hlc`] — hybrid logical clock and the `(hlc, actor)` [`Stamp`]
//! - [`op`] — the [`Op`] envelope and every payload
//! - [`merge`] — LWW register, OR-set, append-only comment log
//! - [`view`] — [`TicketView`] and the pure [`apply`] function

pub mod body;
pub mod domain;
pub mod hlc;
pub mod merge;
pub mod op;
pub mod view;

pub use body::{Body, BodyError, BodyUpdate};
pub use domain::{
    Actor, ActorId, ActorKind, Comment, Hold, NotBefore, Parked, Priority, Project, ProjectStatus,
    Relation, RelationKind, Source, State, StateCategory, Ticket, Waiver, Workspace,
};
pub use hlc::{Clock, Hlc, Stamp};
pub use op::{OP_VERSION, Op, Payload};
pub use view::{ApplyError, ClaimRejected, TicketView, apply};

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
