//! `pm doctor [--rebuild]` through the built binary (AGT-1337). A ticket
//! is exercised with the CLI (create, set) and the store (label, comment,
//! transition — no verbs for those yet); doctor is then healthy, a row
//! corrupted behind pm's back is reported (exit 1), and `--rebuild`
//! repairs it. Sandbox as in `cli.rs`: temp HOME, cleared env, no stdin.

use std::path::PathBuf;
use std::process::{Command, Output, Stdio};

use pm_core::op::{CommentAdd, LabelAdd, StateTransition};
use pm_core::{ActorId, Clock, Op, Payload, Project, ProjectStatus};
use pm_store::Store;
use rusqlite::Connection;
use serde_json::Value;
use tempfile::TempDir;
use ulid::Ulid;

struct Sandbox {
    home: TempDir,
    ws: PathBuf,
}

impl Sandbox {
    /// An initialized AGT workspace with a `pm` project.
    fn new() -> Self {
        let home = tempfile::tempdir().unwrap();
        let ws = home.path().join("ws");
        let sb = Sandbox { home, ws };
        assert_code(
            &sb.pm(&[
                "init",
                "--prefix",
                "AGT",
                "--preset",
                "saltline",
                "--workspace",
                sb.ws_str(),
            ]),
            0,
        );
        sb.store()
            .put_project(
                &Project {
                    kind: Default::default(),
                    id: "pm".into(),
                    title: "pm".into(),
                    status: ProjectStatus::InProgress,
                    parent: None,
                    repos: Default::default(),
                    doc: String::new(),
                    documents: Default::default(),
                },
                &pm_core::ActorId::new("matt"),
            )
            .unwrap();
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

    fn store(&self) -> Store {
        Store::open(self.ws.join("pm.sqlite")).unwrap()
    }

    fn raw(&self) -> Connection {
        Connection::open(self.ws.join("pm.sqlite")).unwrap()
    }

    /// Commits ops the CLI has no verb for yet, stamped after the log.
    fn commit(&self, ticket: Ulid, payloads: Vec<Payload>) {
        let mut store = self.store();
        let mut clock = Clock::from_latest(store.latest_hlc().unwrap());
        for payload in payloads {
            let hlc = clock.send(1_700_000_000_000);
            let op = Op::new(Ulid::new(), hlc, ActorId::new("tester"), ticket, payload);
            store.commit(&op).unwrap();
        }
    }
}

fn stdout(out: &Output) -> String {
    String::from_utf8(out.stdout.clone()).unwrap()
}

fn stderr(out: &Output) -> String {
    String::from_utf8(out.stderr.clone()).unwrap()
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
    serde_json::from_slice(&out.stdout).unwrap_or_else(|e| panic!("{e}: {}", stdout(out)))
}

/// create → set → label → comment → transition, through the CLI where a
/// verb exists and the store where not. Returns the ticket's ULID.
fn exercise(sb: &Sandbox) -> Ulid {
    assert_code(
        &sb.pm(&[
            "new",
            "--title",
            "First",
            "--project",
            "pm",
            "--label",
            "model:fable-5",
        ]),
        0,
    );
    assert_code(
        &sb.pm(&[
            "set",
            "AGT-1",
            "title=Renamed",
            "priority=high",
            "repo=OpenThinkAi/pm",
        ]),
        0,
    );
    let id: Ulid = json(&sb.pm(&["show", "AGT-1", "--json"]))["ulid"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    sb.commit(
        id,
        vec![
            Payload::LabelAdd(LabelAdd {
                label: "manual".into(),
            }),
            Payload::CommentAdd(CommentAdd {
                body: "on it".into(),
            }),
            Payload::StateTransition(StateTransition {
                state: "in-progress".into(),
            }),
        ],
    );
    id
}

// ---------------------------------------------------------------- AC1

#[test]
fn doctor_reports_counts_and_exits_0_on_a_healthy_database() {
    let sb = Sandbox::new();
    exercise(&sb);

    let out = sb.pm(&["doctor"]);
    assert_code(&out, 0);
    let text = stdout(&out);
    // Each of AGT-1350 (backup_target) and AGT-1344 (project doc bodies)
    // added a migration; the exact number here just needs to track
    // pm_store::SCHEMA_VERSION, not stay pinned to 1.
    assert!(
        text.contains(&format!("schema version  {}\n", pm_store::SCHEMA_VERSION)),
        "{text}"
    );
    assert!(text.contains("ops             18\n"), "{text}");
    assert!(text.contains("ticket 1"), "{text}");
    assert!(text.contains("ticket_label 2"), "{text}");
    assert!(text.contains("comment 1"), "{text}");
    assert!(text.contains("integrity       ok\n"), "{text}");
    assert!(text.contains("foreign keys    ok\n"), "{text}");
    assert!(text.contains("replay          ok"), "{text}");
    // AGT-1393: nothing has been pushed, so the whole log is the outbox.
    assert!(
        text.contains(
            "sync            outbox 18 op(s), pushed through seq 0, \
             cursor 0 (never pulled), 0 ticket(s) awaiting a hub number\n"
        ),
        "{text}"
    );

    let v = json(&sb.pm(&["doctor", "--json"]));
    assert_eq!(v["schema"], 1);
    assert_eq!(v["healthy"], true);
    assert_eq!(v["schema_version"], pm_store::SCHEMA_VERSION);
    assert_eq!(v["op_count"], 18);
    assert_eq!(v["tables"]["ticket"], 1);
    assert_eq!(v["tables"]["ops"], 18);
    assert_eq!(v["integrity"], serde_json::json!([]));
    assert_eq!(v["foreign_keys"], serde_json::json!([]));
    assert_eq!(v["replay_error"], Value::Null);
    assert_eq!(v["drift"]["tables"], serde_json::json!([]));
    assert_eq!(v["rebuilt"], Value::Null);
    assert_eq!(
        v["sync"],
        serde_json::json!({"outbox": 18, "pushed_through": 0, "cursor": 0, "pending_numbers": 0, "seeded": false, "parked": 0, "refused": 0})
    );
    // AGT-1467: nothing pulled, nothing quarantined.
    assert_eq!(v["quarantine"], serde_json::json!([]));
    assert!(text.contains("quarantine      none\n"), "{text}");
}

// ---------------------------------------------------------------- AC2

#[test]
fn rebuild_after_create_set_label_comment_transition_changes_nothing() {
    let sb = Sandbox::new();
    exercise(&sb);
    let before = json(&sb.pm(&["show", "AGT-1", "--json"]));

    let out = sb.pm(&["doctor", "--rebuild"]);
    assert_code(&out, 0);
    let text = stdout(&out);
    assert!(
        text.contains("rebuilt tables from 18 ops: no changes, they already matched\n"),
        "{text}"
    );
    assert!(text.contains("replay          ok"), "{text}");

    let v = json(&sb.pm(&["doctor", "--rebuild", "--json"]));
    assert_eq!(v["healthy"], true);
    assert_eq!(v["rebuilt"], serde_json::json!({ "tables": [] }));

    assert_eq!(json(&sb.pm(&["show", "AGT-1", "--json"])), before);
}

// ---------------------------------------------------------------- AC3

#[test]
fn a_corrupted_row_is_detected_by_doctor_and_repaired_by_rebuild() {
    let sb = Sandbox::new();
    let id = exercise(&sb);
    sb.raw()
        .execute("UPDATE ticket SET title = 'x'", [])
        .unwrap();
    assert_eq!(
        stdout(&sb.pm(&["show", "AGT-1", "--field", "title"])),
        "x\n",
        "reads now serve the corrupted row"
    );

    let out = sb.pm(&["doctor"]);
    assert_code(&out, 1);
    let text = stdout(&out);
    assert!(
        text.contains("replay          DRIFT: 1 row(s) differ from the op log\n"),
        "{text}"
    );
    assert!(
        text.contains(&format!("~ [{id}]  title: x -> Renamed")),
        "{text}"
    );
    assert!(
        stderr(&out).contains("pm doctor --rebuild"),
        "{}",
        stderr(&out)
    );

    let v = json(&sb.pm(&["doctor", "--json"]));
    assert_eq!(v["healthy"], false);
    assert_eq!(
        v["drift"]["tables"],
        serde_json::json!([{
            "table": "ticket",
            "missing": [],
            "extra": [],
            "changed": [{
                "key": [id.to_string()],
                "columns": [{ "column": "title", "before": "x", "after": "Renamed" }],
            }],
        }])
    );
    // `--json` exits 1 too: the code is the contract, the body the detail.
    assert_code(&sb.pm(&["doctor", "--json"]), 1);
    assert_eq!(
        stdout(&sb.pm(&["show", "AGT-1", "--field", "title"])),
        "x\n",
        "doctor without --rebuild changed nothing"
    );

    let out = sb.pm(&["doctor", "--rebuild"]);
    assert_code(&out, 0);
    let text = stdout(&out);
    assert!(
        text.contains("rebuilt tables from 18 ops: 1 row(s) changed\n"),
        "{text}"
    );
    assert!(
        text.contains(&format!("~ [{id}]  title: x -> Renamed")),
        "{text}"
    );
    assert!(text.contains("replay          ok"), "{text}");

    assert_code(&sb.pm(&["doctor"]), 0);
    assert_eq!(
        stdout(&sb.pm(&["show", "AGT-1", "--field", "title"])),
        "Renamed\n"
    );
}

#[test]
fn rebuild_json_reports_what_it_changed() {
    let sb = Sandbox::new();
    let id = exercise(&sb);
    sb.raw()
        .execute("DELETE FROM ticket_label WHERE label = 'manual'", [])
        .unwrap();

    let v = json(&sb.pm(&["doctor", "--rebuild", "--json"]));
    assert_eq!(v["healthy"], true);
    assert_eq!(v["rebuilt"]["tables"][0]["table"], "ticket_label");
    assert_eq!(
        v["rebuilt"]["tables"][0]["missing"][0]["key"],
        serde_json::json!([id.to_string(), "manual"])
    );
    assert_eq!(
        json(&sb.pm(&["show", "AGT-1", "--json"]))["labels"],
        serde_json::json!(["manual", "model:fable-5"])
    );
}

#[test]
fn doctor_exit_codes_outside_a_workspace() {
    let sb = Sandbox::new();
    let missing = sb.home.path().join("nope");
    let out = sb.pm(&["doctor", "--workspace", missing.to_str().unwrap()]);
    assert_code(&out, 1);
    assert!(
        stderr(&out).contains("not a pm workspace"),
        "{}",
        stderr(&out)
    );
    assert_code(&sb.pm(&["doctor", "--bogus"]), 2);
}

/// AGT-1482: `--prune-quarantine` drops a refused op's content now and
/// says so; the entry stays, marked `pruned`, and the database is healthy.
#[test]
fn prune_quarantine_drops_refused_content_and_keeps_the_entry() {
    let sb = Sandbox::new();
    let ticket = exercise(&sb);
    let bad = Op::new(
        Ulid::new(),
        pm_core::Hlc::new(1_700_000_000_000, 0),
        ActorId::new("peer"),
        ticket,
        Payload::Claim(pm_core::op::Claim {
            state: "../../x".into(),
            assignee: ActorId::new("peer"),
        }),
    );
    let pulled = sb
        .store()
        .apply_pulled_page(&[(7, bad.clone())], 7)
        .unwrap();
    assert_eq!(pulled.refused.len(), 1);

    let before = json(&sb.pm(&["doctor", "--json", "--workspace", sb.ws_str()]));
    assert!(before.get("pruned_quarantine").is_none(), "{before}");
    assert_eq!(before["quarantine"][0]["pruned"], false);

    let out = sb.pm(&[
        "doctor",
        "--json",
        "--prune-quarantine",
        "--workspace",
        sb.ws_str(),
    ]);
    assert_code(&out, 0);
    let after = json(&out);
    assert_eq!(after["pruned_quarantine"], 1);
    assert_eq!(after["healthy"], true);
    let entry = &after["quarantine"][0];
    assert_eq!(entry["op_id"], bad.op_id.to_string());
    assert_eq!(entry["status"], "refused");
    assert_eq!(entry["pruned"], true);
    assert!(entry["reason"].as_str().unwrap().contains("state name"));
    let content: String = sb
        .raw()
        .query_row("SELECT op FROM sync_quarantine", [], |r| r.get(0))
        .unwrap();
    assert_eq!(content, "");

    let out = sb.pm(&["doctor", "--prune-quarantine", "--workspace", sb.ws_str()]);
    assert_code(&out, 0);
    let text = stdout(&out);
    assert!(
        text.contains("pruned the content of 0 refused op(s)"),
        "{text}"
    );
    assert!(text.contains("[content pruned]"), "{text}");
}

// ------------------------------------------------- AGT-2521 (pm#2)

/// The design doc of project `pm`, its `doc_id` as bound by `put_project`.
fn design_doc_id(sb: &Sandbox) -> Ulid {
    let id: String = sb
        .raw()
        .query_row("SELECT doc_id FROM project WHERE id = 'pm'", [], |r| {
            r.get(0)
        })
        .unwrap();
    id.parse().unwrap()
}

/// Two writers editing the design doc, concurrently at one point, as the
/// `body.edit` updates they would push. Every update is a plain Loro text
/// edit; applied in this order they converge on one text.
fn two_writer_edits() -> Vec<pm_core::BodyUpdate> {
    let mut a = pm_core::Body::with_peer(11).unwrap();
    let mut b = pm_core::Body::with_peer(22).unwrap();
    let mut updates = Vec::new();
    for text in [
        "alpha\nbeta\n",
        "alpha\nbeta\ngamma\n",
        "alpha\nBETA\ngamma\n",
    ] {
        let u = a.diff_from_text(text).unwrap();
        b.apply(&u).unwrap();
        updates.push(u);
    }
    let ua = a
        .diff_from_text("alpha\nBETA\ngamma\ndelta from a\n")
        .unwrap();
    let ub = b.diff_from_text("zero\nalpha\nBETA\ngamma\n").unwrap();
    a.apply(&ub).unwrap();
    b.apply(&ua).unwrap();
    updates.push(ub);
    updates.push(ua);
    for i in 0..10 {
        let u = a
            .diff_from_text(&format!("{}line {i}\n", a.text()))
            .unwrap();
        b.apply(&u).unwrap();
        updates.push(u);
        let u = b.diff_from_text(&format!("x{i} {}", b.text())).unwrap();
        a.apply(&u).unwrap();
        updates.push(u);
    }
    assert_eq!(a.text(), b.text());
    updates
}

fn doc_view_row(sb: &Sandbox, doc: Ulid) -> String {
    sb.raw()
        .query_row(
            "SELECT view FROM project_doc_view WHERE doc_id = ?1",
            [doc.to_string()],
            |r| r.get(0),
        )
        .unwrap()
}

/// pm#2: a document built up one edit at a time (each commit restores the
/// stored snapshot, folds the edit in and stores a new snapshot) and the
/// same document replayed in one pass can serialize to different Loro
/// snapshot bytes while decoding to the same document. That is not drift.
#[test]
fn an_incrementally_built_doc_whose_snapshot_bytes_differ_from_the_replay_is_healthy() {
    let sb = Sandbox::new();
    let doc = design_doc_id(&sb);
    {
        let mut store = sb.store();
        let mut clock = Clock::from_latest(store.latest_hlc().unwrap());
        for update in two_writer_edits() {
            let hlc = clock.send(1_700_000_000_000);
            let op = Op::new(
                Ulid::new(),
                hlc,
                ActorId::new("tester"),
                doc,
                Payload::BodyEdit(pm_core::op::BodyEdit {
                    update: update.into_bytes(),
                }),
            );
            store.commit_doc_edit(doc, &op).unwrap();
        }
    }
    let incremental = doc_view_row(&sb, doc);

    // The premise: a one-pass replay really does write different bytes.
    let out = sb.pm(&["doctor", "--rebuild", "--json"]);
    assert_code(&out, 0);
    let replayed = doc_view_row(&sb, doc);
    assert_ne!(
        incremental, replayed,
        "the incremental and replayed snapshots should differ in bytes"
    );
    // Put the incremental row back: the database as it was before the
    // rebuild, which doctor must call healthy.
    sb.raw()
        .execute(
            "UPDATE project_doc_view SET view = ?2 WHERE doc_id = ?1",
            [doc.to_string(), incremental],
        )
        .unwrap();

    let out = sb.pm(&["doctor"]);
    assert_code(&out, 0);
    assert!(
        stdout(&out).contains("replay          ok"),
        "{}",
        stdout(&out)
    );
    let v = json(&sb.pm(&["doctor", "--json"]));
    assert_eq!(v["healthy"], true);
    assert_eq!(v["drift"]["tables"], serde_json::json!([]));
}

/// The other half of pm#2: a stored snapshot that decodes to a different
/// document — text the log never produced — is still drift.
#[test]
fn a_doc_snapshot_with_different_content_is_still_drift() {
    let sb = Sandbox::new();
    let doc = design_doc_id(&sb);
    assert_code(
        &sb.pm(&["project", "edit", "pm", "--from-file", &{
            let path = sb.home.path().join("design.md");
            std::fs::write(&path, "the real design\n").unwrap();
            path.to_str().unwrap().to_string()
        }]),
        0,
    );
    assert_code(&sb.pm(&["doctor"]), 0);

    // Same id and stamp, but a body that says something else.
    let mut view: pm_core::DocView = serde_json::from_str(&doc_view_row(&sb, doc)).unwrap();
    let mut forged = pm_core::Body::with_peer(99).unwrap();
    let update = forged.diff_from_text("a forged design\n").unwrap();
    view.body = pm_core::BodyState::new();
    view.body.apply(&update).unwrap();
    sb.raw()
        .execute(
            "UPDATE project_doc_view SET view = ?2 WHERE doc_id = ?1",
            [doc.to_string(), serde_json::to_string(&view).unwrap()],
        )
        .unwrap();

    let out = sb.pm(&["doctor"]);
    assert_code(&out, 1);
    let text = stdout(&out);
    assert!(
        text.contains("replay          DRIFT: 1 row(s) differ from the op log\n"),
        "{text}"
    );
    let v = json(&sb.pm(&["doctor", "--json"]));
    assert_eq!(v["drift"]["tables"][0]["table"], "project_doc_view");
    assert_eq!(
        v["drift"]["tables"][0]["changed"][0]["key"],
        serde_json::json!([doc.to_string()])
    );
}
