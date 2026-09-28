//! `pm workspace gate-label add|remove|list` driven through the built
//! binary (AGT-1380 AC2). Sandbox as in `tests/cli.rs`: temp HOME, cleared
//! env, stdin closed. This file keeps its own small `Sandbox` rather than
//! sharing another test file's (Rust integration tests are separate
//! binaries and can't share private items across `tests/*.rs` files
//! without a `tests/common/mod.rs`).

use std::path::PathBuf;
use std::process::{Command, Output, Stdio};

use serde_json::Value;
use tempfile::TempDir;

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

    /// A `--preset saltline` workspace: gate label `manual` seeded, same
    /// as the real workspaces this ticket's build loops run against.
    fn initialized() -> Self {
        let sb = Sandbox::new();
        assert_ok(&sb.pm(&[
            "init",
            "--prefix",
            "AGT",
            "--preset",
            "saltline",
            "--workspace",
            sb.ws_str(),
        ]));
        sb
    }

    fn ws_str(&self) -> &str {
        self.ws.to_str().unwrap()
    }

    fn pm(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_pm"))
            .args(args)
            .env_clear()
            .env("HOME", self.home.path())
            .env("USER", "tester")
            .stdin(Stdio::null())
            .output()
            .unwrap()
    }

    /// `pm new --title <title>` against this sandbox's workspace; returns
    /// the created ticket's display id (`AGT-1`).
    fn new_ticket(&self, extra: &[&str]) -> String {
        let mut args = vec!["new", "--title", "t"];
        args.extend_from_slice(extra);
        let out = self.pm(&args);
        assert_ok(&out);
        stdout(&out).trim().to_string()
    }
}

fn stdout(out: &Output) -> String {
    String::from_utf8(out.stdout.clone()).unwrap()
}

fn stderr(out: &Output) -> String {
    String::from_utf8(out.stderr.clone()).unwrap()
}

fn assert_ok(out: &Output) {
    assert_eq!(
        out.status.code(),
        Some(0),
        "stdout: {}\nstderr: {}",
        stdout(out),
        stderr(out)
    );
}

fn assert_code(out: &Output, code: i32) {
    assert_eq!(
        out.status.code(),
        Some(code),
        "stdout: {}\nstderr: {}",
        stdout(out),
        stderr(out)
    );
}

fn json(out: &Output) -> Value {
    assert_ok(out);
    serde_json::from_str(&stdout(out)).unwrap()
}

fn ready_ids(out: &Value) -> Vec<&str> {
    out["ready"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["id"].as_str().unwrap())
        .collect()
}

#[test]
fn gate_label_add_list_remove_round_trips_and_json_shape() {
    let sb = Sandbox::initialized();

    // The saltline preset seeds `manual`.
    let listed = json(&sb.pm(&["workspace", "gate-label", "list", "--json"]));
    assert_eq!(
        listed,
        serde_json::json!({"schema": 1, "gate_labels": ["manual"]})
    );
    let human = stdout(&sb.pm(&["workspace", "gate-label", "list"]));
    assert_eq!(human.trim(), "manual");

    let added = json(&sb.pm(&["workspace", "gate-label", "add", "matt-gated", "--json"]));
    assert_eq!(
        added["gate_labels"],
        serde_json::json!(["manual", "matt-gated"])
    );

    // Adding an already-present label is a no-op, not an error (a set).
    assert_ok(&sb.pm(&["workspace", "gate-label", "add", "matt-gated"]));
    let listed = json(&sb.pm(&["workspace", "gate-label", "list", "--json"]));
    assert_eq!(
        listed["gate_labels"],
        serde_json::json!(["manual", "matt-gated"])
    );

    let removed = json(&sb.pm(&["workspace", "gate-label", "remove", "manual", "--json"]));
    assert_eq!(removed["gate_labels"], serde_json::json!(["matt-gated"]));

    // Removing a label that isn't there is a no-op too.
    assert_ok(&sb.pm(&["workspace", "gate-label", "remove", "nope"]));
    assert_eq!(
        json(&sb.pm(&["workspace", "gate-label", "list", "--json"]))["gate_labels"],
        serde_json::json!(["matt-gated"])
    );

    assert_code(&sb.pm(&["workspace", "gate-label", "add", "  "]), 2);
}

#[test]
fn a_project_gate_label_excludes_a_ticket_from_ready_transitively_like_manual() {
    let sb = Sandbox::initialized();
    assert_ok(&sb.pm(&["project", "new", "pm", "--title", "pm"]));

    let gated = sb.new_ticket(&["--project", "pm"]);
    let dependent = sb.new_ticket(&["--project", "pm", "--blocked-by", &gated]);

    // Before the label exists as a gate, both are ready (blocked-by keeps
    // `dependent` out until `gated` resolves, so check `gated` alone).
    let out = json(&sb.pm(&["ready", "--json"]));
    assert_eq!(ready_ids(&out), [gated.as_str()]);

    assert_ok(&sb.pm(&["label", &gated, "+matt-gated"]));
    // The label isn't a gate label yet, so it changes nothing.
    let out = json(&sb.pm(&["ready", "--json"]));
    assert_eq!(ready_ids(&out), [gated.as_str()]);

    assert_ok(&sb.pm(&["workspace", "gate-label", "add", "matt-gated"]));
    // Now `gated` is excluded, and — like `manual` — everything it
    // transitively blocks is excluded too, without needing
    // `--exclude-label` or a `pm hold` (the problem this ticket exists to
    // fix).
    let out = json(&sb.pm(&["ready", "--json"]));
    assert_eq!(ready_ids(&out), Vec::<&str>::new(), "{out}");
    // `pm claim --ready` sees the same exclusion.
    assert_code(&sb.pm(&["claim", "--ready"]), 3);

    // Removing the gate label restores the frontier.
    assert_ok(&sb.pm(&["workspace", "gate-label", "remove", "matt-gated"]));
    let out = json(&sb.pm(&["ready", "--json"]));
    assert_eq!(ready_ids(&out), [gated.as_str()]);
    let _ = dependent; // still blocked by `gated`; not itself under test here
}
