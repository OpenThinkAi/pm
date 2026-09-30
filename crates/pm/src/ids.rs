//! Validation of the two operator-chosen identifiers that end up in
//! filesystem paths: the workspace prefix and project ids (AGT-1453).
//!
//! Two tiers. The strict `validate_*` functions run where an id is
//! *created* (`pm init --prefix`, `pm project new`). [`safe_component`] is
//! the defensive check at every place an id reaches the filesystem
//! (export, backup, editor temp files), because a prefix or project id can
//! also arrive through a synced op from the hub or another replica. It is
//! deliberately looser than the creation rules so every id that ever
//! existed still passes; it only refuses what could escape a directory.

use crate::exit::{CliError, Result};

/// Longest workspace prefix.
pub const PREFIX_MAX: usize = 16;
/// Longest project id, and the longest path component we accept.
pub const ID_MAX: usize = pm_core::ids::ID_MAX;

/// A workspace prefix: 1-16 uppercase letters or digits, starting with a
/// letter. Exit 2 otherwise.
pub fn validate_prefix(prefix: &str) -> Result<()> {
    let mut chars = prefix.chars();
    let ok = prefix.len() <= PREFIX_MAX
        && chars.next().is_some_and(|c| c.is_ascii_uppercase())
        && chars.all(|c| c.is_ascii_uppercase() || c.is_ascii_digit());
    if ok {
        Ok(())
    } else {
        Err(CliError::usage(format!(
            "invalid prefix '{}': use 1-16 uppercase letters or digits, starting with a letter (e.g. PM)",
            printable(prefix)
        )))
    }
}

/// A project id (README §Data model: "project — id (kebab)"): lowercase
/// letters, digits and single hyphens, never leading, trailing or doubled.
/// Exit 2 otherwise.
pub fn validate_project_id(id: &str) -> Result<()> {
    let ok = !id.is_empty()
        && id.len() <= ID_MAX
        && id
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        && !id.starts_with('-')
        && !id.ends_with('-')
        && !id.contains("--");
    if ok {
        Ok(())
    } else {
        Err(CliError::usage(format!(
            "invalid project id '{}': use lowercase letters, digits and single hyphens \
             (e.g. pm-project-verbs)",
            printable(id)
        )))
    }
}

/// Defensive check before an id is used as a path component: ASCII
/// letters, digits, `-`, `_` and `.`, at most [`ID_MAX`] bytes, not
/// starting with `.` (so never `.` or `..`). No separators, NUL or other
/// control characters. Exit 2 otherwise.
pub fn safe_component<'a>(value: &'a str, what: &str) -> Result<&'a str> {
    // One rule with the trust-boundary check (`pm_core::ids`, AGT-1450).
    if pm_core::ids::is_safe_component(value) {
        Ok(value)
    } else {
        Err(CliError::usage(format!(
            "{what} '{}' is not safe to use in a file path (letters, digits, `-`, `_`, `.`; \
             at most {ID_MAX} characters; no separators or `..`)",
            printable(value)
        )))
    }
}

/// `value` with control characters escaped, for error messages.
fn printable(value: &str) -> String {
    value.chars().flat_map(char::escape_default).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const BAD: &[&str] = &[
        "", ".", "..", "../x", "a/b", "a\\b", "/abs", "a\0b", "a\nb", "a\x1bb", "a b", ".hidden",
        "é",
    ];

    #[test]
    fn live_shapes_pass_every_tier() {
        for id in [
            "pm",
            "think-3",
            "ui-leaf-v1",
            "open-audit-may2026-hardening",
        ] {
            validate_project_id(id).unwrap();
            safe_component(id, "project id").unwrap();
        }
        for p in ["AGT", "PM", "A", "SALTLINE2"] {
            validate_prefix(p).unwrap();
            safe_component(p, "prefix").unwrap();
        }
    }

    #[test]
    fn bad_input_is_rejected_everywhere() {
        for bad in BAD {
            assert!(validate_prefix(bad).is_err(), "prefix {bad:?}");
            assert!(validate_project_id(bad).is_err(), "project {bad:?}");
            assert!(safe_component(bad, "x").is_err(), "component {bad:?}");
        }
    }

    #[test]
    fn length_is_capped() {
        assert!(validate_prefix(&"A".repeat(17)).is_err());
        assert!(validate_project_id(&"a".repeat(65)).is_err());
        assert!(safe_component(&"a".repeat(65), "x").is_err());
        safe_component(&"a".repeat(64), "x").unwrap();
    }

    #[test]
    fn errors_are_usage_and_escape_controls() {
        let err = safe_component("a\0b", "prefix").unwrap_err();
        assert_eq!(err.code, 2);
        assert!(!err.error.to_string().contains('\0'));
    }
}
