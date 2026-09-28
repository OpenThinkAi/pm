//! Snapshot tests for the `--json` contract (AGT-1352,
//! `docs/cli-contract.md`): every verb's `--json` payload is compared
//! against a checked-in fixture under `tests/json/`, so a shape change
//! fails CI until the fixture *and* the doc are updated together.
//!
//! Non-deterministic values (ULIDs, HLC `wall_ms`/`counter`, wall-clock
//! timestamps, sandbox paths, the built binary's path) are normalized to
//! fixed placeholders before comparison — see [`normalize`]. Everything
//! else (field names, nesting, enum spellings, array vs. object shape,
//! deterministic counts and ids) is real and will fail a mismatched
//! fixture.
//!
//! To regenerate every fixture after an intentional shape change:
//! ```sh
//! UPDATE_JSON_FIXTURES=1 cargo test -p pm --test json_contract
//! ```
//! then diff `tests/json/*.json` and update `docs/cli-contract.md` to
//! match before committing — the doc and the fixtures describe the same
//! contract and must move together.
//!
//! `pm edit`/`pm project edit` are exercised here with a no-op `$EDITOR`
//! (exits 0, leaves the file untouched): "an unchanged save plans nothing
//! and commits nothing" (`crate::edit` module docs), so the printed ticket
//! / project is exactly what `pm show --json` / `pm project show --json`
//! already print and already fixture — this proves `pm edit --json`'s
//! shape without re-deriving the frontmatter render format here.
//!
//! `pm ticket list`/`pm ticket show` (the legacy markdown-vault reader)
//! have no fixture: `legacy_ticket` (`src/main.rs`) never looks at
//! `ctx.json`, so `--json` is accepted by clap but silently ignored and
//! the command always prints its fixed human text. `docs/cli-contract.md`
//! calls this out explicitly rather than fixturing an absence of JSON.

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use pm_core::{Project, ProjectStatus};
use pm_store::Store;
use serde_json::Value;
use tempfile::TempDir;

// ------------------------------------------------------------- sandbox

struct Sandbox {
    home: TempDir,
    ws: PathBuf,
}

impl Sandbox {
    fn new() -> Self {
        let home = tempfile::tempdir().unwrap();
        let ws = home.path().join("ws");
        Sandbox { home, ws }
    }

    fn ws_str(&self) -> &str {
        self.ws.to_str().unwrap()
    }

    fn path(&self, name: &str) -> PathBuf {
        self.home.path().join(name)
    }

    fn store(&self) -> Store {
        Store::open(self.ws.join("pm.sqlite")).unwrap()
    }

    /// `pm` with a cleared environment: HOME is the sandbox, USER is
    /// `tester`, stdin `/dev/null` (a command that tried to prompt would
    /// read EOF, not hang), plus `extra` env vars (e.g. `EDITOR`).
    fn run(&self, args: &[&str], extra: &[(&str, &str)]) -> Output {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_pm"));
        cmd.args(args)
            .env_clear()
            .env("HOME", self.home.path())
            .env("USER", "tester")
            .env("TMPDIR", self.home.path())
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .stdin(Stdio::null());
        for (k, v) in extra {
            cmd.env(k, v);
        }
        cmd.output().unwrap()
    }

    fn pm(&self, args: &[&str]) -> Output {
        self.run(args, &[])
    }

    fn fixture_input(&self, name: &str, contents: &str) -> PathBuf {
        let path = self.path(name);
        std::fs::write(&path, contents).unwrap();
        path
    }

    fn put_project(&self, id: &str) {
        let mut store = self.store();
        store
            .put_project(&Project {
                id: id.into(),
                title: id.into(),
                status: ProjectStatus::InProgress,
                parent: None,
                repos: Default::default(),
                doc: String::new(),
                documents: Default::default(),
            })
            .unwrap();
    }

    /// A no-op `$EDITOR`: exits 0 without touching the file, so `pm edit`
    /// / `pm project edit` plan and commit nothing (module docs).
    fn noop_editor(&self) -> PathBuf {
        let path = self.path("noop-editor.sh");
        std::fs::write(&path, "#!/bin/sh\nexit 0\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        path
    }
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

// -------------------------------------------------------- normalization

/// Crockford base32 ULID shape: 26 characters, first restricted to `0`-`7`
/// (a 48-bit ms timestamp never sets a higher bit).
fn looks_like_ulid(s: &str) -> bool {
    const ALPHABET: &str = "0123456789ABCDEFGHJKMNPQRSTVWXYZ";
    let bytes: Vec<u8> = s.bytes().collect();
    bytes.len() == 26
        && matches!(bytes[0], b'0'..=b'7')
        && bytes
            .iter()
            .all(|b| ALPHABET.contains(b.to_ascii_uppercase() as char))
}

fn normalize_value(v: &mut Value) {
    match v {
        Value::Object(map) => {
            for (key, val) in map.iter_mut() {
                match key.as_str() {
                    // Hlc {wall_ms, counter}: wall-clock and op-ordering
                    // dependent, never reproducible across runs.
                    "wall_ms" | "counter" => {
                        *val = Value::from(0);
                        continue;
                    }
                    // `pm backup status`: age since the backup this test
                    // just ran.
                    "age_seconds" => {
                        *val = Value::from(0);
                        continue;
                    }
                    // `pm backup status`: an RFC 3339 timestamp of "now".
                    "last_success" if val.is_string() => {
                        *val = Value::String("<TIMESTAMP>".into());
                        continue;
                    }
                    // `pm ready`: today's date.
                    "today" if val.is_string() => {
                        *val = Value::String("<TODAY>".into());
                        continue;
                    }
                    // `pm import vault`: how long the run took.
                    "elapsed_ms" => {
                        *val = Value::from(0);
                        continue;
                    }
                    _ => {}
                }
                normalize_value(val);
            }
        }
        Value::Array(items) => {
            for item in items {
                normalize_value(item);
            }
        }
        Value::String(s) if looks_like_ulid(s) => {
            *v = Value::String("<ULID>".into());
        }
        _ => {}
    }
}

/// Collapses everything about a `--json` payload that legitimately varies
/// run to run — ULIDs, HLC timestamps, wall-clock dates, and every
/// sandbox/binary path baked into the output — to fixed placeholders, so
/// what remains is exactly the shape the contract promises.
fn normalize(raw: &str, sb: &Sandbox) -> String {
    let mut text = raw.to_string();

    // Path substitution happens on the raw text, before JSON parsing, so
    // it catches a path wherever it landed (any key) and in whichever of
    // its two spellings shows up: the sandbox's raw tempdir path, and
    // `fs::canonicalize`'s resolved form (on macOS, `/var/folders/...` ->
    // `/private/var/folders/...`).
    let home_raw = sb.home.path().to_string_lossy().into_owned();
    let home_canon = std::fs::canonicalize(sb.home.path())
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| home_raw.clone());
    text = text.replace(&home_canon, "<HOME>");
    text = text.replace(&home_raw, "<HOME>");

    let pm_bin = env!("CARGO_BIN_EXE_pm");
    text = text.replace(pm_bin, "<PM_BIN>");
    if let Ok(canon) = std::fs::canonicalize(pm_bin) {
        text = text.replace(&canon.to_string_lossy().into_owned(), "<PM_BIN>");
    }

    let mut value: Value = serde_json::from_str(&text)
        .unwrap_or_else(|e| panic!("normalizing non-JSON output: {e}\n---\n{text}"));
    normalize_value(&mut value);
    serde_json::to_string_pretty(&value).unwrap()
}

// -------------------------------------------------------------- fixtures

fn fixture_path(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("json")
        .join(format!("{name}.json"))
}

/// Compares `actual` (already normalized) against the checked-in fixture
/// `name`, or writes it when `UPDATE_JSON_FIXTURES=1` is set. Mismatches
/// are collected into `failures` rather than panicking immediately, so one
/// run reports every verb that drifted, not just the first.
fn check_fixture(name: &str, actual: &str, failures: &mut Vec<String>) {
    let path = fixture_path(name);
    if std::env::var_os("UPDATE_JSON_FIXTURES").is_some() {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, format!("{actual}\n")).unwrap();
        return;
    }
    let expected = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(_) => {
            failures.push(format!(
                "{name}: no fixture at {} (run with UPDATE_JSON_FIXTURES=1 to create it)",
                path.display()
            ));
            return;
        }
    };
    if actual.trim_end() != expected.trim_end() {
        failures.push(format!(
            "{name}: --json output does not match {}\n--- expected ---\n{expected}\n--- actual ---\n{actual}\n",
            path.display()
        ));
    }
}

/// Runs `pm <args>`, asserts its exit code, and returns its normalized
/// stdout — or records a failure and returns `None` so one bad command
/// doesn't abort every fixture after it.
fn capture(
    sb: &Sandbox,
    name: &str,
    args: &[&str],
    expect_code: i32,
    failures: &mut Vec<String>,
) -> Option<String> {
    let out = sb.pm(args);
    if out.status.code() != Some(expect_code) {
        failures.push(format!(
            "{name}: `pm {}` exited {:?}, expected {expect_code}\nstdout: {}\nstderr: {}",
            args.join(" "),
            out.status.code(),
            stdout(&out),
            stderr(&out)
        ));
        return None;
    }
    let text = normalize(&stdout(&out), sb);
    check_fixture(name, &text, failures);
    Some(text)
}

const BATCH_YAML: &str = "\
tickets:
  - ref: batchA
    title: \"Batch A\"
    project: pm
    priority: medium
    labels: [batch]
  - ref: batchB
    title: \"Batch B\"
    project: pm
    blocked-by: [\"@batchA\"]
    labels: [batch]
";

const FROM_FILE_MD: &str = "\
---
title: Imported ticket
project: pm
priority: low
labels: [imported]
---

## Problem Statement

Filed from a vault-format file.
";

/// A miniature vault for `pm import vault`: one ticket in `tickets/triage`
/// naming a project with a README and a sibling document.
const MINI_VAULT: &[(&str, &str)] = &[
    (
        "tickets/triage/AGT-900-vaulted.md",
        "---\nid: AGT-900\ntitle: Vaulted ticket\nstate: triage\ncreated: 2026-09-01\nupdated: 2026-09-02\nproject: vaulted\nrepo: \nblocked-by: []\nlinked-github: \nlinked-pr: \npriority: medium\nlabels: [contract]\nsource: { type: manual, url: \"\", id: \"\", fetched-at: \"\" }\nteam: product\n---\n\n## Problem Statement\n\nImported from a vault.\n\n## Comments\n\n### 2026-09-02 — Filed\nwaived: standalone — contract fixture\n",
    ),
    (
        "projects/vaulted/README.md",
        "---\nid: vaulted\ntitle: \"Vaulted\"\nstatus: active\nparent-project:\nrepos: []\n---\n\n# vaulted\n",
    ),
    ("projects/vaulted/NOTES.md", "# Notes\n"),
];

/// Drives every `--json`-producing verb once, in dependency order, against
/// one sandbox — later steps rely on tickets/projects earlier ones
/// created — and snapshot-compares each output.
#[test]
fn every_verbs_json_output_matches_its_fixture() {
    let sb = Sandbox::new();
    let mut failures: Vec<String> = Vec::new();
    let cap = |name: &str, args: &[&str], code: i32, failures: &mut Vec<String>| {
        capture(&sb, name, args, code, failures)
    };

    // ---- pm init ----
    cap(
        "init",
        &[
            "init",
            "--prefix",
            "AGT",
            "--preset",
            "saltline",
            "--workspace",
            sb.ws_str(),
            "--json",
        ],
        0,
        &mut failures,
    );

    // ---- pm project new/list/show/doc ----
    cap(
        "project_new",
        &[
            "project",
            "new",
            "pm",
            "--title",
            "pm",
            "--repo",
            "OpenThinkAi/pm",
            "--json",
        ],
        0,
        &mut failures,
    );
    cap(
        "project_list",
        &["project", "list", "--json"],
        0,
        &mut failures,
    );
    cap(
        "project_show",
        &["project", "show", "pm", "--json"],
        0,
        &mut failures,
    );
    let notes = sb.fixture_input("notes.md", "Some project notes.\n");
    cap(
        "project_doc_add",
        &[
            "project",
            "doc",
            "add",
            "pm",
            "notes",
            "--from-file",
            notes.to_str().unwrap(),
            "--json",
        ],
        0,
        &mut failures,
    );
    cap(
        "project_show_doc",
        &["project", "show", "pm", "--doc", "notes", "--json"],
        0,
        &mut failures,
    );
    let editor = sb.noop_editor();
    {
        let out = sb.run(
            &["project", "edit", "pm", "--json"],
            &[("EDITOR", editor.to_str().unwrap())],
        );
        if out.status.code() != Some(0) {
            failures.push(format!(
                "project_edit: exited {:?}\nstderr: {}",
                out.status.code(),
                stderr(&out)
            ));
        } else {
            check_fixture(
                "project_edit",
                &normalize(&stdout(&out), &sb),
                &mut failures,
            );
        }
    }

    // ---- pm new (single, --blocked-by, --batch, --from-file) ----
    cap(
        "new",
        &[
            "new",
            "--title",
            "Ticket A",
            "--project",
            "pm",
            "--repo",
            "OpenThinkAi/pm",
            "--priority",
            "high",
            "--label",
            "foo",
            "--description",
            "## Section\n\nBody text for the section.\n",
            "--json",
        ],
        0,
        &mut failures,
    ); // AGT-1
    cap(
        "new_blocked_by",
        &[
            "new",
            "--title",
            "Ticket B",
            "--project",
            "pm",
            "--blocked-by",
            "AGT-1",
            "--json",
        ],
        0,
        &mut failures,
    ); // AGT-2
    let batch = sb.fixture_input("batch.yaml", BATCH_YAML);
    cap(
        "new_batch",
        &["new", "--batch", batch.to_str().unwrap(), "--json"],
        0,
        &mut failures,
    ); // AGT-3 (batchA), AGT-4 (batchB, blocked by AGT-3)

    // ---- pm archive/unarchive (single ticket; round-tripped so later
    // steps see AGT-4 in its normal, non-archived state) ----
    cap("archive", &["archive", "AGT-4", "--json"], 0, &mut failures);
    cap(
        "unarchive",
        &["unarchive", "AGT-4", "--json"],
        0,
        &mut failures,
    );

    // ---- pm show ----
    cap("show", &["show", "AGT-1", "--json"], 0, &mut failures);
    cap(
        "show_field",
        &["show", "AGT-1", "--field", "title", "--json"],
        0,
        &mut failures,
    );
    cap(
        "show_section",
        &["show", "AGT-1", "--section", "Section", "--json"],
        0,
        &mut failures,
    );

    // ---- pm list ----
    cap(
        "list",
        &["list", "--project", "pm", "--json"],
        0,
        &mut failures,
    );

    // ---- pm set/label/comment/move ----
    cap(
        "set",
        &["set", "AGT-1", "priority=critical", "--json"],
        0,
        &mut failures,
    );
    // `-y` looks like a flag to clap, so global flags (`--json`) go before
    // `label`, not after (`pm label --help`'s own warning).
    cap(
        "label",
        &["--json", "label", "AGT-1", "+bar", "-foo"],
        0,
        &mut failures,
    );
    cap(
        "comment",
        &["comment", "AGT-1", "hello from the contract test", "--json"],
        0,
        &mut failures,
    );
    cap(
        "move",
        &["move", "AGT-1", "in-progress", "--json"],
        0,
        &mut failures,
    );

    // ---- pm log/status/graph/ready ----
    cap("log", &["log", "AGT-1", "--json"], 0, &mut failures);
    cap(
        "status",
        &["status", "--project", "pm", "--json"],
        0,
        &mut failures,
    );
    cap(
        "graph",
        &["graph", "--project", "pm", "--json"],
        0,
        &mut failures,
    );
    cap(
        "ready",
        &["ready", "--project", "pm", "--json"],
        0,
        &mut failures,
    );

    // ---- pm hold/holds/waive/check ----
    cap(
        "hold",
        &["hold", "AGT-2", "needs review", "--json"],
        0,
        &mut failures,
    );
    cap(
        "holds",
        &["holds", "--project", "pm", "--json"],
        0,
        &mut failures,
    );
    cap(
        "waive",
        &[
            "waive",
            "AGT-2",
            "R1",
            "standalone: contract test",
            "--json",
        ],
        0,
        &mut failures,
    );
    // AGT-2 is held, so `pm check` finds exactly that and exits 1.
    cap(
        "check",
        &["check", "--project", "pm", "--json"],
        1,
        &mut failures,
    );

    // ---- pm claim/unclaim ----
    cap(
        "claim",
        &["claim", "AGT-3", "--branch", "feature/contract", "--json"],
        0,
        &mut failures,
    );
    // Already claimed: exit 75, `{taken_by, at, ...}`.
    cap(
        "claim_taken",
        &["claim", "AGT-3", "--json"],
        75,
        &mut failures,
    );
    cap("unclaim", &["unclaim", "AGT-3", "--json"], 0, &mut failures);

    // ---- pm done ----
    cap(
        "done",
        &[
            "done",
            "AGT-1",
            "--note",
            "shipped",
            "--merged-sha",
            "deadbeef",
            "--pr",
            "https://github.com/OpenThinkAi/pm/pull/9",
            "--json",
        ],
        0,
        &mut failures,
    );

    // ---- pm edit (no-op save; same shape as `pm show --json`) ----
    {
        let out = sb.run(
            &["edit", "AGT-2", "--json"],
            &[("EDITOR", editor.to_str().unwrap())],
        );
        if out.status.code() != Some(0) {
            failures.push(format!(
                "edit: exited {:?}\nstderr: {}",
                out.status.code(),
                stderr(&out)
            ));
        } else {
            check_fixture("edit", &normalize(&stdout(&out), &sb), &mut failures);
        }
    }

    // ---- pm archive --auto --dry-run ---- (every ticket here completed,
    // if at all, this same month, so nothing is eligible yet — this still
    // exercises the --auto/--dry-run shape, with empty result arrays)
    cap(
        "archive_auto",
        &["archive", "--auto", "--dry-run", "--json"],
        0,
        &mut failures,
    );

    // ---- pm doctor ----
    cap("doctor", &["doctor", "--json"], 0, &mut failures);
    cap(
        "doctor_rebuild",
        &["doctor", "--rebuild", "--json"],
        0,
        &mut failures,
    );

    // ---- pm backup ----
    let backup_dir = sb.path("backup-target");
    std::fs::create_dir_all(&backup_dir).unwrap();
    cap(
        "backup",
        &["backup", "--to", backup_dir.to_str().unwrap(), "--json"],
        0,
        &mut failures,
    );
    cap(
        "backup_status",
        &[
            "backup",
            "status",
            "--to",
            backup_dir.to_str().unwrap(),
            "--json",
        ],
        0,
        &mut failures,
    );
    let timer_dir = sb.path("timer");
    cap(
        "backup_install_timer",
        &[
            "backup",
            "install-timer",
            "--dir",
            timer_dir.to_str().unwrap(),
            "--no-load",
            "--json",
        ],
        0,
        &mut failures,
    );

    // ---- pm project delete ----
    // The creation here is only scaffolding for the delete below (already
    // covered by the "project_new" fixture above), so it runs unfixtured.
    {
        let out = sb.pm(&["project", "new", "scratch", "--title", "scratch", "--json"]);
        if out.status.code() != Some(0) {
            failures.push(format!(
                "project_delete setup: `pm project new scratch` exited {:?}\nstderr: {}",
                out.status.code(),
                stderr(&out)
            ));
        }
    }
    cap(
        "project_delete",
        &["project", "delete", "scratch", "--json"],
        0,
        &mut failures,
    );

    // ---- pm new --from-file ----
    let from_file = sb.fixture_input("import.md", FROM_FILE_MD);
    cap(
        "new_from_file",
        &["new", "--from-file", from_file.to_str().unwrap(), "--json"],
        0,
        &mut failures,
    );

    // ---- pm import vault ----
    // Last: the import raises the number allocator floor to the vault's
    // maximum, which would renumber everything filed after it. A two-file
    // vault in the sandbox (one ticket with a marker, one project with a
    // named doc); the real fixture vault is exercised in tests/import.rs.
    let vault = sb.path("mini-vault");
    for (rel, text) in MINI_VAULT {
        let path = vault.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }
    cap(
        "import_vault_dry_run",
        &[
            "import",
            "vault",
            vault.to_str().unwrap(),
            "--dry-run",
            "--json",
        ],
        0,
        &mut failures,
    );
    cap(
        "import_vault",
        &["import", "vault", vault.to_str().unwrap(), "--json"],
        0,
        &mut failures,
    );
    // ---- pm import vault --report / pm export md (AGT-1348) ----
    let parity = sb.path("parity.md");
    cap(
        "import_vault_report",
        &[
            "import",
            "vault",
            vault.to_str().unwrap(),
            "--report",
            parity.to_str().unwrap(),
            "--json",
        ],
        0,
        &mut failures,
    );
    let export_dir = sb.path("export");
    cap(
        "export_md",
        &[
            "export",
            "md",
            export_dir.to_str().unwrap(),
            "--legacy-markers",
            "--json",
        ],
        0,
        &mut failures,
    );

    assert!(
        failures.is_empty(),
        "{} verb(s) drifted from their fixture:\n\n{}",
        failures.len(),
        failures.join("\n\n")
    );
}

/// `pm ticket list`/`pm ticket show` never look at `ctx.json` (they predate
/// the `--json` contract and read markdown files directly) — `--json` is
/// accepted but silently ignored, never JSON. This is a contract fact
/// worth pinning down, not a shape to fixture.
#[test]
fn legacy_ticket_verbs_ignore_the_json_flag() {
    let sb = Sandbox::new();
    let dir = sb.path("vault");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("AGT-1.md"),
        "---\nid: AGT-1\ntitle: Legacy\nstate: triage\npriority: medium\n---\n",
    )
    .unwrap();

    let out = sb.pm(&["ticket", "list", dir.to_str().unwrap(), "--json"]);
    assert_eq!(out.status.code(), Some(0));
    assert!(
        serde_json::from_slice::<Value>(&out.stdout).is_err(),
        "`pm ticket list --json` printed JSON; docs/cli-contract.md says it never does: {}",
        stdout(&out)
    );
    assert!(stdout(&out).contains("AGT-1"), "{}", stdout(&out));
}

/// Exercises the AGT-1339-review fix directly: `pm status --json`'s
/// `states` is an ordered array in workflow order, never a map (whose keys
/// would sort alphabetically — `triage` was already in front of
/// `in-progress` under the old shape, but `done` sorted before both).
#[test]
fn status_states_is_an_array_in_workflow_order_not_an_alphabetical_map() {
    let sb = Sandbox::new();
    assert_eq!(
        sb.pm(&[
            "init",
            "--prefix",
            "AGT",
            "--preset",
            "saltline",
            "--workspace",
            sb.ws_str(),
        ])
        .status
        .code(),
        Some(0)
    );
    sb.put_project("pm");
    assert_eq!(
        sb.pm(&["new", "--title", "t", "--project", "pm"])
            .status
            .code(),
        Some(0)
    );

    let out = sb.pm(&["status", "--json"]);
    assert_eq!(out.status.code(), Some(0));
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    let states = v["states"].as_array().expect("states is an array");
    let names: Vec<&str> = states.iter().map(|s| s["name"].as_str().unwrap()).collect();
    assert_eq!(names, ["triage", "in-progress", "done"]);
    assert_eq!(states[0]["category"], "unstarted");
    assert_eq!(states[0]["count"], 1);
}
