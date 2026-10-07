//! The CLI's exit-code contract (projects/pm/README.md §Surfaces): `0` ok,
//! `1` error, `2` usage, `3` not found, `4` stale, `75` taken/conflict,
//! `141` stdout closed early. Every verb returns [`Result`]; `main` prints
//! the message and exits with the code.
//! Clap's own parse failures already exit `2`.

use std::fmt;

use pm_store::StoreError;

pub const ERROR: u8 = 1;
pub const USAGE: u8 = 2;
pub const NOT_FOUND: u8 = 3;
/// A conditional document write found the document changed since the
/// version the caller read (`pm project edit --if-version`, AGT-1573).
pub const STALE: u8 = 4;
/// The authority refused a conditional op (`pm claim`: someone else holds
/// the ticket).
pub const TAKEN: u8 = 75;
/// Stdout's reader went away mid-output (`pm list | head -1`): `128 +
/// SIGPIPE`, exited quietly by [`crate::pipe`] (AGT-1634).
pub const BROKEN_PIPE: u8 = 141;

#[derive(Debug)]
pub struct CliError {
    pub code: u8,
    pub error: anyhow::Error,
}

impl CliError {
    pub fn usage(msg: impl fmt::Display) -> Self {
        CliError {
            code: USAGE,
            error: anyhow::anyhow!("{msg}"),
        }
    }

    pub fn not_found(msg: impl fmt::Display) -> Self {
        CliError {
            code: NOT_FOUND,
            error: anyhow::anyhow!("{msg}"),
        }
    }

    pub fn error(msg: impl fmt::Display) -> Self {
        CliError {
            code: ERROR,
            error: anyhow::anyhow!("{msg}"),
        }
    }
}

impl From<anyhow::Error> for CliError {
    fn from(error: anyhow::Error) -> Self {
        CliError { code: ERROR, error }
    }
}

/// A store failure that names something missing (a project, a ticket, a
/// state) is "not found", a refused claim is "taken", a re-parent that
/// would close a loop is "usage"; anything else is a plain error.
impl From<StoreError> for CliError {
    fn from(err: StoreError) -> Self {
        let code = match err {
            StoreError::UnknownProject { .. }
            | StoreError::UnknownTicket { .. }
            | StoreError::UnknownState { .. }
            | StoreError::UnknownRelationTarget { .. } => NOT_FOUND,
            StoreError::ClaimRejected(_) => TAKEN,
            StoreError::ProjectCycle { .. } | StoreError::InitiativeParent { .. } => USAGE,
            _ => ERROR,
        };
        CliError {
            code,
            error: err.into(),
        }
    }
}

pub type Result<T> = std::result::Result<T, CliError>;
