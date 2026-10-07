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

// ------------------------------------------------ workflow states (AGT-1518)

fn state_names(v: &Value) -> Vec<(String, String, u64)> {
    v["states"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| {
            (
                s["name"].as_str().unwrap().to_string(),
                s["category"].as_str().unwrap().to_string(),
                s["position"].as_u64().unwrap(),
            )
        })
        .collect()
}

#[test]
fn state_add_is_usable_at_once_and_an_explicit_position_inserts() {
    let sb = Sandbox::initialized();
    let added = json(&sb.pm(&[
        "workspace",
        "state",
        "add",
        "qa",
        "--category",
        "started",
        "--position",
        "2",
        "--json",
    ]));
    assert_eq!(added["created"], true);
    assert_eq!(added["changed"], true);
    // `done` held position 2: the insert shifted it after `qa`.
    assert_eq!(
        state_names(&added),
        vec![
            ("triage".into(), "unstarted".into(), 0),
            ("in-progress".into(), "started".into(), 1),
            ("qa".into(), "started".into(), 2),
            ("done".into(), "completed".into(), 3),
            ("canceled".into(), "canceled".into(), 4),
        ]
    );
    // No --position: after the last state.
    let added = json(&sb.pm(&[
        "workspace",
        "state",
        "add",
        "dropped",
        "--category",
        "canceled",
        "--json",
    ]));
    assert_eq!(added["state"]["position"], 5);

    // pm move / list --state / status see it immediately.
    let id = sb.new_ticket(&[]);
    assert_ok(&sb.pm(&["move", &id, "qa"]));
    let listed = json(&sb.pm(&["list", "--state", "qa", "--json"]));
    assert_eq!(listed.as_array().unwrap().len(), 1);
    let status = stdout(&sb.pm(&["status"]));
    assert!(status.contains("qa"), "{status}");

    // `list` prints name, category, position in workflow order.
    let text = stdout(&sb.pm(&["workspace", "state", "list"]));
    assert_eq!(
        text,
        "triage\tunstarted\t0\nin-progress\tstarted\t1\nqa\tstarted\t2\ndone\tcompleted\t3\ncanceled\tcanceled\t4\ndropped\tcanceled\t5\n"
    );

    // One state.upsert per state the write changed, in the config log.
    let log = stdout(&sb.pm(&["log"]));
    assert!(
        log.contains("set state 'qa' (started, position 2)"),
        "{log}"
    );
    assert!(
        log.contains("set state 'done' (completed, position 3)"),
        "{log}"
    );
}

#[test]
fn state_add_upserts_an_existing_state_and_ready_follows_its_category() {
    let sb = Sandbox::initialized();
    assert_ok(&sb.pm(&[
        "workspace",
        "state",
        "add",
        "refined",
        "--category",
        "backlog",
    ]));
    let id = sb.new_ticket(&[]);
    assert_ok(&sb.pm(&["move", &id, "refined"]));
    let ready = json(&sb.pm(&["ready", "--json"]));
    assert!(ready_ids(&ready).is_empty(), "{ready}");

    // Re-categorise: only the category changes, the position stays.
    let changed = json(&sb.pm(&[
        "workspace",
        "state",
        "add",
        "refined",
        "--category",
        "unstarted",
        "--json",
    ]));
    assert_eq!(changed["created"], false);
    assert_eq!(changed["changed"], true);
    assert_eq!(changed["state"]["position"], 4);
    let ready = json(&sb.pm(&["ready", "--json"]));
    assert_eq!(ready_ids(&ready), vec![id.as_str()]);
    let claimed = sb.pm(&["claim", &id]);
    assert_ok(&claimed);

    // The same record again commits nothing.
    let same = json(&sb.pm(&[
        "workspace",
        "state",
        "add",
        "refined",
        "--category",
        "unstarted",
        "--json",
    ]));
    assert_eq!(same["changed"], false);
}

#[test]
fn state_add_refuses_what_would_break_the_workflow() {
    let sb = Sandbox::initialized();
    // A new state needs a category.
    let out = sb.pm(&["workspace", "state", "add", "qa"]);
    assert_code(&out, 2);
    assert!(stderr(&out).contains("--category is required"));
    // Not a path-safe name.
    assert_code(
        &sb.pm(&["workspace", "state", "add", "a/b", "--category", "started"]),
        2,
    );
    assert_code(
        &sb.pm(&["workspace", "state", "add", "qa", "--category", "nope"]),
        2,
    );
    // The only unstarted / completed state keeps its category.
    let out = sb.pm(&[
        "workspace",
        "state",
        "add",
        "triage",
        "--category",
        "backlog",
    ]);
    assert_code(&out, 2);
    assert!(
        stderr(&out).contains("only unstarted state"),
        "{}",
        stderr(&out)
    );
    assert_code(
        &sb.pm(&[
            "workspace",
            "state",
            "add",
            "done",
            "--category",
            "canceled",
        ]),
        2,
    );
    // With a second completed state, `done` may change.
    assert_ok(&sb.pm(&[
        "workspace",
        "state",
        "add",
        "shipped",
        "--category",
        "completed",
    ]));
    assert_ok(&sb.pm(&[
        "workspace",
        "state",
        "add",
        "done",
        "--category",
        "canceled",
    ]));
}

#[test]
fn init_seeds_a_custom_state_list() {
    let sb = Sandbox::new();
    let out = json(&sb.pm(&[
        "init",
        "--prefix",
        "AGT",
        "--preset",
        "saltline",
        "--state",
        "triage:unstarted,refined:unstarted",
        "--state",
        "in-progress:started,qa:started,done:completed",
        "--workspace",
        sb.ws_str(),
        "--json",
    ]));
    assert_eq!(
        state_names(&out),
        vec![
            ("triage".into(), "unstarted".into(), 0),
            ("refined".into(), "unstarted".into(), 1),
            ("in-progress".into(), "started".into(), 2),
            ("qa".into(), "started".into(), 3),
            ("done".into(), "completed".into(), 4),
        ]
    );
    // The preset still supplies the gate labels.
    let labels = json(&sb.pm(&["workspace", "gate-label", "list", "--json"]));
    assert_eq!(labels["gate_labels"], serde_json::json!(["manual"]));
    assert_eq!(sb.new_ticket(&[]), "AGT-1");
}

#[test]
fn init_refuses_a_state_list_without_unstarted_or_completed() {
    for spec in [
        "doing:started,done:completed",
        "todo:unstarted,doing:started",
        "todo:unstarted,todo:completed",
        "todo",
        "todo:later",
    ] {
        let sb = Sandbox::new();
        let out = sb.pm(&["init", "--state", spec, "--workspace", sb.ws_str()]);
        assert_code(&out, 2);
        assert!(
            !sb.ws.join("pm.sqlite").exists(),
            "{spec}: a refused init must not create the database"
        );
    }
}
