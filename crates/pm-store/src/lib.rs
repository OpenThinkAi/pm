//! `pm-store`: SQLite-backed op log and materialized tables (op append +
//! materialize in one transaction, per projects/pm/README.md §Op log).
//!
//! Stubbed empty in AGT-1333 (the cargo workspace restructure); schema,
//! migrations and `rusqlite` land in a later ticket. Depends on `pm-core`
//! now to prove the workspace wiring, ahead of real usage.

pub use pm_core::VERSION as CORE_VERSION;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn links_against_pm_core() {
        assert!(!CORE_VERSION.is_empty());
    }
}
