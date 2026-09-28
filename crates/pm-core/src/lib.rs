//! `pm-core`: pure domain types, op types, merge rules and HLC logic,
//! shared by the `pm` CLI and (later) `pm-hub` so merge semantics are
//! identical everywhere.
//!
//! This crate must never perform IO: no `rusqlite`, no filesystem access,
//! no network. `crates/pm/tests/pm_core_purity.rs` enforces that
//! mechanically by checking this crate's `Cargo.toml` against a denylist
//! and grepping this crate's `src/` for filesystem use.
//!
//! Scaffolded empty in AGT-1333 (the cargo workspace restructure). Domain
//! types (ticket entities, the op log, HLC, merge rules) land in later
//! tickets per projects/pm/README.md §Architecture.

pub mod body;

pub use body::{Body, BodyError, BodyUpdate};

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
