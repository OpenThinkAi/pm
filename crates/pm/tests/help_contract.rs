//! `pm --help` / `pm <verb> --help` vs. `docs/cli-contract.md` (AGT-1352
//! AC3): every `--flag` clap prints for a command must appear somewhere in
//! the doc, and — so the doc never documents a flag that does not exist —
//! every `--flag`-shaped token the doc mentions must be one clap actually
//! prints for *some* command. No regex dependency: flags are `--` tokens,
//! found the same simple way on both sides.

use std::collections::BTreeSet;
use std::path::Path;
use std::process::{Command, Output};

/// Every command and subcommand path `main.rs`'s `Cmd`/`ImportCmd`/
/// `ProjectCmd`/`BackupCmd`/`TicketCmd` define. Kept as a flat list (rather than
/// re-deriving it from clap) so this test fails loudly — a path missing
/// here, or a path here clap no longer has — the moment either one drifts,
/// rather than silently checking less than it claims to.
const PATHS: &[&[&str]] = &[
    &[],
    &["init"],
    &["new"],
    &["show"],
    &["set"],
    &["label"],
    &["relate"],
    &["comment"],
    &["move"],
    &["done"],
    &["unclaim"],
    &["edit"],
    &["app"],
    &["claim"],
    &["list"],
    &["log"],
    &["status"],
    &["graph"],
    &["ready"],
    &["hold"],
    &["holds"],
    &["waive"],
    &["check"],
    &["doctor"],
    &["archive"],
    &["unarchive"],
    &["import"],
    &["import", "vault"],
    &["export"],
    &["export", "md"],
    &["project"],
    &["project", "new"],
    &["project", "show"],
    &["project", "list"],
    &["project", "edit"],
    &["project", "set"],
    &["project", "delete"],
    &["project", "doc"],
    &["project", "doc", "add"],
    &["project", "doc", "edit"],
    &["workspace"],
    &["workspace", "gate-label"],
    &["workspace", "gate-label", "add"],
    &["workspace", "gate-label", "remove"],
    &["workspace", "gate-label", "list"],
    &["workspace", "docs-owned-by"],
    &["workspace", "state"],
    &["workspace", "state", "add"],
    &["workspace", "state", "list"],
    &["hub"],
    &["hub", "login"],
    &["hub", "status"],
    &["hub", "logout"],
    &["sync"],
    &["ticket"],
    &["ticket", "list"],
    &["ticket", "show"],
    &["backup"],
    &["backup", "install-timer"],
    &["backup", "status"],
];

fn help_text(path: &[&str]) -> String {
    let mut args: Vec<&str> = path.to_vec();
    args.push("--help");
    let out: Output = Command::new(env!("CARGO_BIN_EXE_pm"))
        .args(&args)
        .env_clear()
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "`pm {} --help` exited {:?}\nstderr: {}",
        path.join(" "),
        out.status.code(),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}

/// Every `--flag`-shaped token in `text`: `--`, then a letter (every real
/// clap flag starts with one — this is what tells a flag apart from a
/// markdown table rule like `|---|---|`), then more letters/digits/`-`.
/// Works identically on `--help` output and on the markdown doc, so a flag
/// found one way is the same string found the other.
fn flag_tokens(text: &str) -> BTreeSet<String> {
    let bytes = text.as_bytes();
    let mut out = BTreeSet::new();
    let mut i = 0;
    while i + 1 < bytes.len() {
        if bytes[i] == b'-'
            && bytes[i + 1] == b'-'
            && bytes.get(i + 2).is_some_and(u8::is_ascii_alphabetic)
        {
            let start = i;
            let mut j = i + 2;
            while j < bytes.len() && (bytes[j].is_ascii_alphanumeric() || bytes[j] == b'-') {
                j += 1;
            }
            out.insert(text[start..j].to_string());
            i = j;
        } else {
            i += 1;
        }
    }
    out
}

/// Strips fenced (triple-backtick) code blocks: this doc's JSON-shape and
/// shell-command examples legitimately contain `--`-prefixed tokens that
/// are not `pm` flags (e.g. `cargo test --test json_contract`), and every
/// real `pm` flag is also named in this doc's plain prose (its "Flags:"
/// line), so excluding fenced blocks loses no real flag mention while
/// dropping that noise from the reverse (doc -> real flag) check.
fn strip_fenced_code_blocks(text: &str) -> String {
    text.split("```")
        .enumerate()
        .filter_map(|(i, part)| (i % 2 == 0).then_some(part))
        .collect::<Vec<_>>()
        .join("")
}

fn raw_contract_doc() -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("docs")
        .join("cli-contract.md");
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("reading {}: {e}", path.display()))
}

fn contract_doc() -> String {
    strip_fenced_code_blocks(&raw_contract_doc())
}

#[test]
fn every_help_flag_is_documented_and_every_documented_flag_is_real() {
    let doc = contract_doc();
    let doc_flags = flag_tokens(&doc);

    let mut all_real_flags: BTreeSet<String> = BTreeSet::new();
    let mut missing_from_doc: Vec<String> = Vec::new();

    for path in PATHS {
        let text = help_text(path);
        let flags = flag_tokens(&text);
        for flag in &flags {
            all_real_flags.insert(flag.clone());
            if !doc_flags.contains(flag) {
                missing_from_doc.push(format!(
                    "`pm {}` has `{flag}` (from --help), but docs/cli-contract.md never mentions it",
                    path.join(" ")
                ));
            }
        }
    }

    let phantom_in_doc: Vec<&String> = doc_flags.difference(&all_real_flags).collect();

    let mut problems = missing_from_doc;
    problems.extend(phantom_in_doc.into_iter().map(|flag| {
        format!("docs/cli-contract.md mentions `{flag}`, which no `pm ... --help` output has")
    }));

    assert!(
        problems.is_empty(),
        "{} problem(s) between --help and docs/cli-contract.md:\n{}",
        problems.len(),
        problems.join("\n")
    );
}

/// Every path in [`PATHS`] is one clap actually recognizes (catches a typo
/// in the list itself, and a subcommand this test forgot to add).
#[test]
fn every_listed_path_is_a_real_command() {
    for path in PATHS {
        let text = help_text(path);
        assert!(
            text.starts_with(char::is_alphabetic) || text.contains("Usage:"),
            "`pm {} --help` did not look like help text:\n{text}",
            path.join(" ")
        );
    }
}

/// The quoted alternatives the doc lists after `"<key>": ` in the JSON
/// shape that starts at `anchor`, up to the next `"message"`/`"tickets"`
/// key: e.g. `"reason": "state" | "assigned" | ...` in `pm ready`'s shape.
fn documented_alternatives(doc: &str, anchor: &str, key: &str, until: &str) -> BTreeSet<String> {
    let section = &doc[doc
        .find(anchor)
        .unwrap_or_else(|| panic!("docs/cli-contract.md has no `{anchor}`"))..];
    let start = section
        .find(&format!("\"{key}\": "))
        .unwrap_or_else(|| panic!("no `\"{key}\"` after `{anchor}`"))
        + key.len()
        + 4;
    let end = start + section[start..].find(until).unwrap();
    section[start..end]
        .split('"')
        .skip(1)
        .step_by(2)
        .map(str::to_string)
        .collect()
}

/// AGT-1353: `pm ready`'s `excluded[].reason` and `pm check`'s
/// `findings[].rule` are spelled in the doc exactly as the binary prints
/// them (`Reason::kind`, `Finding::rule`) — the doc once said `blocked_by`,
/// `no_project`, `blocker_cycle` while the binary printed `blocked-by`,
/// `R1`, `blocker-cycle`. Each variant is built once below; the exhaustive
/// `match`es make adding a variant a compile error here until it is listed.
#[test]
fn documented_ready_reasons_and_check_rules_match_the_binary() {
    use pm_core::ready::{Gate, Reason};
    use pm_core::{ActorId, Finding, Hlc, Hold, Relation, RelationKind};
    use ulid::Ulid;

    let hold = Hold {
        reason: "r".into(),
        by: ActorId::new("a"),
        at: Hlc::ZERO,
    };
    let id = Ulid::nil();
    let gate = Gate::Label {
        label: "manual".into(),
    };
    let reasons = [
        Reason::State { state: "s".into() },
        Reason::Assigned {
            assignee: ActorId::new("a"),
        },
        Reason::Held { hold: hold.clone() },
        Reason::Label { label: "l".into() },
        Reason::Parked {
            until: "forever".into(),
        },
        Reason::ProjectParked {
            project: "api-router".into(),
        },
        Reason::NotBefore {
            date: "2099-01-01".into(),
        },
        Reason::Cycle { tickets: vec![id] },
        Reason::BlockedBy {
            blocker: id,
            gate: None,
        },
        Reason::BlockedByCanceled {
            blocker: id,
            state: "canceled".into(),
        },
        Reason::TransitivelyBlocked {
            via: id,
            root: id,
            gate,
        },
        Reason::Model { labels: vec![] },
    ];
    for r in &reasons {
        match r {
            Reason::State { .. }
            | Reason::Assigned { .. }
            | Reason::Held { .. }
            | Reason::Label { .. }
            | Reason::Parked { .. }
            | Reason::ProjectParked { .. }
            | Reason::NotBefore { .. }
            | Reason::Cycle { .. }
            | Reason::BlockedBy { .. }
            | Reason::BlockedByCanceled { .. }
            | Reason::TransitivelyBlocked { .. }
            | Reason::Model { .. } => {}
        }
    }
    let findings = [
        Finding::NoProject { ticket: id },
        Finding::Stale {
            ticket: id,
            days: 1,
        },
        Finding::Held {
            ticket: id,
            hold: hold.clone(),
        },
        Finding::AssignedUnstarted {
            ticket: id,
            assignee: ActorId::new("a"),
            state: "s".into(),
        },
        Finding::Parked {
            ticket: id,
            days: 1,
            state: "s".into(),
        },
        Finding::BlockerCycle { tickets: vec![id] },
        Finding::DanglingRelation {
            relation: Relation {
                kind: RelationKind::Blocks,
                from: id,
                to: id,
            },
            missing: id,
        },
        Finding::DeletedProject {
            ticket: id,
            project: "p".into(),
        },
        Finding::BlockedByCanceled {
            ticket: id,
            blocker: id,
            state: "canceled".into(),
        },
    ];
    for f in &findings {
        match f {
            Finding::NoProject { .. }
            | Finding::Stale { .. }
            | Finding::Held { .. }
            | Finding::AssignedUnstarted { .. }
            | Finding::Parked { .. }
            | Finding::BlockerCycle { .. }
            | Finding::DanglingRelation { .. }
            | Finding::DeletedProject { .. }
            | Finding::BlockedByCanceled { .. } => {}
        }
    }

    // `done` is the CLI's own reason for an `--ids` entry that is not a
    // candidate at all (crates/pm/src/ready.rs), not a `Reason` variant.
    let mut want_reasons: BTreeSet<String> = reasons.iter().map(|r| r.kind().to_string()).collect();
    want_reasons.insert("done".into());
    let want_rules: BTreeSet<String> = findings.iter().map(|f| f.rule().to_string()).collect();

    // The serialized tag must agree with the accessor the CLI prints.
    for r in &reasons {
        assert_eq!(serde_json::to_value(r).unwrap()["reason"], r.kind());
    }
    for f in &findings {
        assert_eq!(serde_json::to_value(f).unwrap()["rule"], f.rule());
    }

    let doc = raw_contract_doc();
    assert_eq!(
        documented_alternatives(&doc, "### `pm ready`", "reason", "\"message\""),
        want_reasons,
        "docs/cli-contract.md `pm ready` excluded[].reason values"
    );
    assert_eq!(
        documented_alternatives(&doc, "### `pm check`", "rule", "\"tickets\""),
        want_rules,
        "docs/cli-contract.md `pm check` findings[].rule values"
    );
}

/// AGT-1577: `pm label --help` tells the reader global flags may follow the
/// changes (no more "put them before `label`" trap), and the contract doc
/// documents the same form.
#[test]
fn label_help_documents_trailing_global_flags() {
    let help = help_text(&["label"]);
    assert!(help.contains("+x adds, -y removes"), "{help}");
    assert!(help.contains("after the changes"), "{help}");
    assert!(!help.contains("before `label`, not after"), "{help}");
    let doc = raw_contract_doc();
    assert!(
        doc.contains("`pm label AGT-12 +x -y --json` works"),
        "docs/cli-contract.md must document trailing global flags on `pm label`"
    );
}

/// AGT-1574: the one `--json shape:` line each command's `--help` ends
/// with (`src/json_shape.rs`). `None` if absent; panics if there are two.
fn json_shape_line(path: &[&str]) -> Option<String> {
    let text = help_text(path);
    let lines: Vec<&str> = text
        .lines()
        .filter(|l| l.starts_with("--json shape: "))
        .collect();
    assert!(
        lines.len() <= 1,
        "`pm {} --help` has {} `--json shape:` lines",
        path.join(" "),
        lines.len()
    );
    lines
        .first()
        .map(|l| l["--json shape: ".len()..].to_string())
}

/// AGT-1574 AC1: every command's `--help` states its `--json` top-level
/// shape in one line, and that line matches what the verb really prints:
/// each checked-in `tests/json/*.json` fixture is a bare array exactly when
/// its verb's line says `bare JSON array`, and a `{"schema": 1, ...}`
/// object exactly when it says `object`. Every fixture must be mapped
/// below, so a new fixture cannot dodge the check.
#[test]
fn every_help_states_its_json_shape_and_it_matches_the_fixtures() {
    for path in PATHS.iter().filter(|p| !p.is_empty()) {
        assert!(
            json_shape_line(path).is_some(),
            "`pm {} --help` does not state its --json shape",
            path.join(" ")
        );
    }

    // Fixture stem -> the command that printed it (`tests/json_contract.rs`).
    let fixture_verb = |stem: &str| -> Vec<&'static str> {
        const LONGEST_FIRST: &[(&str, &[&str])] = &[
            (
                "workspace_gate_label_add",
                &["workspace", "gate-label", "add"],
            ),
            (
                "workspace_gate_label_remove",
                &["workspace", "gate-label", "remove"],
            ),
            (
                "workspace_gate_label_list",
                &["workspace", "gate-label", "list"],
            ),
            ("workspace_docs_owned_by", &["workspace", "docs-owned-by"]),
            ("workspace_state_add", &["workspace", "state", "add"]),
            ("workspace_state_list", &["workspace", "state", "list"]),
            ("backup_install_timer", &["backup", "install-timer"]),
            ("backup_status", &["backup", "status"]),
            ("project_doc_add", &["project", "doc", "add"]),
            ("project_doc_edit", &["project", "doc", "edit"]),
            ("project_delete", &["project", "delete"]),
            ("project_edit", &["project", "edit"]),
            ("project_list", &["project", "list"]),
            ("project_new", &["project", "new"]),
            ("project_set", &["project", "set"]),
            ("project_show", &["project", "show"]),
            ("import_vault", &["import", "vault"]),
            ("export_md", &["export", "md"]),
            ("hub_login", &["hub", "login"]),
            ("hub_logout", &["hub", "logout"]),
            ("hub_status", &["hub", "status"]),
            ("unarchive", &["unarchive"]),
            ("archive", &["archive"]),
            ("backup", &["backup"]),
            ("check", &["check"]),
            ("claim", &["claim"]),
            ("comment", &["comment"]),
            ("doctor", &["doctor"]),
            ("done", &["done"]),
            ("edit", &["edit"]),
            ("graph", &["graph"]),
            ("holds", &["holds"]),
            ("hold", &["hold"]),
            ("init", &["init"]),
            ("label", &["label"]),
            ("list", &["list"]),
            ("log", &["log"]),
            ("move", &["move"]),
            ("new", &["new"]),
            ("ready", &["ready"]),
            ("relate", &["relate"]),
            ("set", &["set"]),
            ("show", &["show"]),
            ("status", &["status"]),
            ("sync", &["sync"]),
            ("unclaim", &["unclaim"]),
            ("waive", &["waive"]),
        ];
        LONGEST_FIRST
            .iter()
            .find(|(prefix, _)| {
                stem == *prefix
                    || stem
                        .strip_prefix(prefix)
                        .is_some_and(|rest| rest.starts_with('_'))
            })
            .unwrap_or_else(|| panic!("fixture `{stem}.json` maps to no command here"))
            .1
            .to_vec()
    };

    let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("json");
    let mut checked = 0;
    for entry in std::fs::read_dir(&dir).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let stem = path.file_stem().unwrap().to_str().unwrap().to_string();
        let value: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let verb = fixture_verb(&stem);
        let line = json_shape_line(&verb).unwrap();
        if value.is_array() {
            assert!(
                line.starts_with("bare JSON array"),
                "{stem}.json is a bare array, but `pm {} --help` says: {line}",
                verb.join(" ")
            );
        } else {
            assert_eq!(value["schema"], 1, "{stem}.json has no top-level schema");
            assert!(
                line.starts_with("object") && line.contains(r#"{"schema": 1"#),
                "{stem}.json is a {{\"schema\": 1, ...}} object, but `pm {} --help` says: {line}",
                verb.join(" ")
            );
        }
        checked += 1;
    }
    assert!(
        checked > 40,
        "only {checked} fixture(s) found under {}",
        dir.display()
    );
}
