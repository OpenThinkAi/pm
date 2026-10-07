//! Each verb's `--json` top-level shape, stated in its `--help` (AGT-1574).
//!
//! Most verbs print one `{"schema": 1, ...}` object; `pm list` and `pm log`
//! print a bare JSON array. `docs/cli-contract.md` has always said so per
//! verb, but a caller only found out by a parse failure. [`annotate`] adds
//! one `--json shape:` line to every command's `--help` (after the options),
//! from the single table below, so the shape is discoverable from the
//! binary itself. `tests/help_contract.rs` checks every line against the
//! checked-in `tests/json/*.json` fixtures (array vs. object), and the unit
//! tests below check the table covers every command clap has.

use clap::Command;

/// The marker every shape line starts with — what scripts and the
/// contract test grep for.
pub const MARKER: &str = "--json shape:";

/// A single Ticket echo, shared by every ticket-mutating verb.
const TICKET: &str = r#"object: a Ticket {"schema": 1, "id": ..., ...}"#;

/// Command groups that do nothing on their own: the shape is per subcommand.
const GROUP: &str = "none of its own: each subcommand's --help states its shape";

/// `(command path, shape)`: one entry per command and subcommand
/// `Cli::command()` defines (the root excepted — its `--json` help points
/// here). A shape starts with `object` (one `{"schema": 1, ...}` object),
/// `bare JSON array` (no top-level envelope), or `none`.
const SHAPES: &[(&[&str], &str)] = &[
    (
        &["init"],
        r#"object {"schema": 1, "workspace": ..., "prefix": ..., ...}"#,
    ),
    (
        &["new"],
        r#"object: a Ticket {"schema": 1, "id": ..., ...}; with --batch, {"schema": 1, "tickets": [Ticket, ...], "refs": {...}}"#,
    ),
    (
        &["show"],
        r#"object: a Ticket {"schema": 1, ..., "comments": [...]}; --field/--section print {"schema": 1, <field>} / {"schema": 1, "section", "body"}"#,
    ),
    (&["set"], TICKET),
    (&["label"], TICKET),
    (&["relate"], TICKET),
    (&["comment"], TICKET),
    (&["move"], TICKET),
    (&["done"], TICKET),
    (&["unclaim"], TICKET),
    (&["edit"], TICKET),
    (
        &["app"],
        r#"object: one compact {"schema": 1, "url": ..., "token": ..., ...} line, then the server keeps running"#,
    ),
    (
        &["claim"],
        r#"object: a Ticket {"schema": 1, "id": ..., ...}; on exit 75, {"schema": 1, "id", "taken_by", "at", "state", "reason"}"#,
    ),
    (
        &["list"],
        r#"bare JSON array of Tickets [{"schema": 1, "id": ..., ...}, ...] with no top-level envelope"#,
    ),
    (
        &["log"],
        r#"bare JSON array of ops [{"schema": 1, "op_id": ..., "kind": ..., ...}, ...] with no top-level envelope"#,
    ),
    (&["status"], r#"object {"schema": 1, "states": [...], ...}"#),
    (&["graph"], r#"object {"schema": 1, "waves": [...], ...}"#),
    (
        &["ready"],
        r#"object {"schema": 1, "ready": [Ticket, ...], "excluded": [...], ...}"#,
    ),
    (&["hold"], TICKET),
    (
        &["holds"],
        r#"object {"schema": 1, "tickets": [Ticket, ...]}"#,
    ),
    (&["waive"], TICKET),
    (
        &["check"],
        r#"object {"schema": 1, "ok": ..., "count": ..., "findings": [...]}"#,
    ),
    (&["doctor"], r#"object {"schema": 1, "healthy": ..., ...}"#),
    (
        &["archive"],
        r#"object: a Ticket {"schema": 1, "id": ..., ...}; with --auto, {"schema": 1, "archived_tickets": [...], ...}"#,
    ),
    (&["unarchive"], TICKET),
    (&["import"], GROUP),
    (
        &["import", "vault"],
        r#"object {"schema": 1, "tickets": ..., ...}"#,
    ),
    (&["export"], GROUP),
    (
        &["export", "md"],
        r#"object {"schema": 1, "dir": ..., ...}"#,
    ),
    (&["project"], GROUP),
    (
        &["project", "new"],
        r#"object: a Project {"schema": 1, "id": ..., ...}"#,
    ),
    (
        &["project", "show"],
        r#"object: a Project {"schema": 1, ..., "documents": {name: body, ...}} (documents is an object, not an array), with "doc_version" and "document_versions": {name: version, ...}; with --doc, {"schema": 1, "project", "doc", "body", "version"}"#,
    ),
    (
        &["project", "list"],
        r#"object {"schema": 1, "projects": [Project, ...]}"#,
    ),
    (
        &["project", "edit"],
        r#"object: a Project {"schema": 1, "id": ..., ...}; on exit 4 (--if-version stale), {"schema": 1, "project", "doc", "expected_version", "version"}"#,
    ),
    (
        &["project", "set"],
        r#"object: a Project {"schema": 1, "id": ..., ...}"#,
    ),
    (
        &["project", "delete"],
        r#"object {"schema": 1, "id": ..., "deleted": ...}"#,
    ),
    (&["project", "doc"], GROUP),
    (
        &["project", "doc", "add"],
        r#"object {"schema": 1, "project": ..., "doc": ...}"#,
    ),
    (
        &["project", "doc", "edit"],
        r#"object: a Project {"schema": 1, "id": ..., ...}; on exit 4 (--if-version stale), {"schema": 1, "project", "doc", "expected_version", "version"}"#,
    ),
    (&["workspace"], GROUP),
    (&["workspace", "gate-label"], GROUP),
    (
        &["workspace", "gate-label", "add"],
        r#"object {"schema": 1, "gate_labels": [...]}"#,
    ),
    (
        &["workspace", "gate-label", "remove"],
        r#"object {"schema": 1, "gate_labels": [...]}"#,
    ),
    (
        &["workspace", "gate-label", "list"],
        r#"object {"schema": 1, "gate_labels": [...]}"#,
    ),
    (
        &["workspace", "docs-owned-by"],
        r#"object {"schema": 1, "docs_owned_by": ...}"#,
    ),
    (&["workspace", "state"], GROUP),
    (
        &["workspace", "state", "add"],
        r#"object {"schema": 1, "state": {"name": ..., "category": ..., ...}, ...}"#,
    ),
    (
        &["workspace", "state", "list"],
        r#"object {"schema": 1, "states": [...]}"#,
    ),
    (&["hub"], GROUP),
    (
        &["hub", "login"],
        r#"object {"schema": 1, "hub": ..., ...}"#,
    ),
    (
        &["hub", "status"],
        r#"object {"schema": 1, "configured": ..., ...}"#,
    ),
    (
        &["hub", "logout"],
        r#"object {"schema": 1, "hub_removed": ..., ...}"#,
    ),
    (
        &["sync"],
        r#"object {"schema": 1, "pushed": ..., ...}; with --watch, one such object per line per round"#,
    ),
    (&["ticket"], GROUP),
    (
        &["ticket", "list"],
        "none: --json is ignored, the output is always text",
    ),
    (
        &["ticket", "show"],
        "none: --json is ignored, the output is always text",
    ),
    (&["backup"], r#"object {"schema": 1, "target": ..., ...}"#),
    (
        &["backup", "install-timer"],
        r#"object {"schema": 1, "plist": ..., ...}"#,
    ),
    (
        &["backup", "status"],
        r#"object {"schema": 1, "healthy": ..., ...}"#,
    ),
];

fn shape(path: &[String]) -> Option<&'static str> {
    SHAPES
        .iter()
        .find(|(p, _)| p.iter().copied().eq(path.iter().map(String::as_str)))
        .map(|(_, s)| *s)
}

/// `cmd` with every (sub)command's `--json shape:` line set as its
/// `after_help`.
pub fn annotate(cmd: Command) -> Command {
    annotate_at(cmd, &mut Vec::new())
}

fn annotate_at(mut cmd: Command, path: &mut Vec<String>) -> Command {
    for sub in cmd.get_subcommands_mut() {
        path.push(sub.get_name().to_string());
        let owned = std::mem::take(sub);
        *sub = annotate_at(owned, path);
        path.pop();
    }
    match shape(path) {
        Some(s) => cmd.after_help(format!("{MARKER} {s}")),
        None => cmd,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    fn paths(cmd: &Command, prefix: &mut Vec<String>, out: &mut Vec<Vec<String>>) {
        for sub in cmd.get_subcommands() {
            prefix.push(sub.get_name().to_string());
            out.push(prefix.clone());
            paths(sub, prefix, out);
            prefix.pop();
        }
    }

    #[test]
    fn every_command_has_exactly_one_shape_and_every_shape_a_command() {
        let mut all = Vec::new();
        paths(&crate::Cli::command(), &mut Vec::new(), &mut all);
        for p in &all {
            let n = SHAPES
                .iter()
                .filter(|(s, _)| s.iter().copied().eq(p.iter().map(String::as_str)))
                .count();
            assert_eq!(n, 1, "`pm {}` has {n} --json shape entries", p.join(" "));
        }
        assert_eq!(all.len(), SHAPES.len(), "SHAPES names a command clap lacks");
    }

    #[test]
    fn every_shape_starts_with_a_known_kind_and_is_one_line() {
        for (p, s) in SHAPES {
            assert!(
                s.starts_with("object")
                    || s.starts_with("bare JSON array")
                    || s.starts_with("none"),
                "`pm {}`: {s}",
                p.join(" ")
            );
            assert!(!s.contains('\n'), "`pm {}` shape spans lines", p.join(" "));
        }
    }
}
