//! Structured markers and `pm check` through the built binary (AGT-1342):
//! `pm hold/holds/waive`, `pm set not_before=/parked=`, the `pm show`
//! marker block, and `pm check`'s findings and exit codes.

use std::path::PathBuf;
use std::process::{Command, Output, Stdio};

use pm_core::op::RelationAdd;
use pm_core::{ActorId, Hlc, Op, Payload, Project, ProjectStatus, Relation, RelationKind};
use pm_store::Store;
use serde_json::Value;
use tempfile::TempDir;
use ulid::Ulid;

struct Sandbox {
    home: TempDir,
    ws: PathBuf,
}

impl Sandbox {
    fn new() -> Self {
        let home = tempfile::tempdir().unwrap();
        let ws = home.path().join("ws");
        let sb = Sandbox { home, ws };
        let ws = sb.ws.to_str().unwrap().to_string();
        assert_code(
            &sb.pm(&[
                "init",
                "--prefix",
                "AGT",
                "--preset",
                "saltline",
                "--workspace",
                &ws,
            ]),
            0,
        );
        sb.put_project("pm");
        sb
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

    fn put_project(&self, id: &str) {
        self.store()
            .put_project(
                &Project {
                    kind: Default::default(),
                    id: id.into(),
                    title: id.into(),
                    status: ProjectStatus::InProgress,
                    parent: None,
                    repos: Default::default(),
                    doc: String::new(),
                    documents: Default::default(),
                },
                &pm_core::ActorId::new("matt"),
            )
            .unwrap();
    }

    fn new_ticket(&self, extra: &[&str]) -> String {
        let mut args = vec!["new", "--title", "t"];
        args.extend_from_slice(extra);
        let out = self.pm(&args);
        assert_code(&out, 0);
        stdout(&out).trim().to_string()
    }

    fn show(&self, id: &str) -> Value {
        json(&self.pm(&["show", id, "--json"]))
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
    assert_code(out, 0);
    serde_json::from_slice(&out.stdout).unwrap()
}

// ---------------------------------------------------------------- AC1

#[test]
fn hold_sets_clears_and_lists() {
    let sb = Sandbox::new();
    let a = sb.new_ticket(&["--project", "pm"]);
    let b = sb.new_ticket(&["--project", "pm"]);

    assert_code(&sb.pm(&["hold", &a, "needs Matt's call"]), 0);
    let hold = &sb.show(&a)["hold"];
    assert_eq!(hold["reason"], "needs Matt's call");
    assert_eq!(hold["by"], "tester");
    assert!(hold["at"]["wall_ms"].as_u64().unwrap() > 0, "{hold}");

    let listed = json(&sb.pm(&["holds", "--json"]));
    let ids: Vec<&str> = listed["tickets"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, [a.as_str()]);
    let human = stdout(&sb.pm(&["holds", "--project", "pm"]));
    assert!(
        human.contains(&a) && human.contains("needs Matt's call"),
        "{human}"
    );
    assert!(!human.contains(&b), "{human}");
    assert_code(&sb.pm(&["holds", "--project", "nope"]), 3);

    assert_code(&sb.pm(&["hold", "--clear", &a]), 0);
    assert!(sb.show(&a)["hold"].is_null());
    assert_eq!(
        json(&sb.pm(&["holds", "--json"]))["tickets"],
        Value::Array(vec![])
    );
    // Clearing again is a no-op, not an error.
    assert_code(&sb.pm(&["hold", "--clear", &a]), 0);

    assert_code(&sb.pm(&["hold", &a]), 2);
    assert_code(&sb.pm(&["hold", &a, "why", "--clear"]), 2);
    assert_code(&sb.pm(&["hold", &a, "  "]), 2);
    assert_code(&sb.pm(&["hold", "AGT-999", "why"]), 3);
}

#[test]
fn holds_ids_scopes_like_ready_ids_and_unknown_id_is_not_found() {
    let sb = Sandbox::new();
    let a = sb.new_ticket(&["--project", "pm"]);
    let b = sb.new_ticket(&["--project", "pm"]);
    let c = sb.new_ticket(&["--project", "pm"]);
    assert_code(&sb.pm(&["hold", &a, "needs Matt's call"]), 0);
    assert_code(&sb.pm(&["hold", &b, "also needs Matt"]), 0);

    // Scoped to a and c: only a is held and in scope, so only it is
    // listed (b is held but out of scope; c is in scope but not held).
    let listed = json(&sb.pm(&["holds", "--ids", &format!("{a},{c}"), "--json"]));
    let ids: Vec<&str> = listed["tickets"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, [a.as_str()]);

    // An unknown id is exit 3, same as `pm ready --ids`.
    assert_code(&sb.pm(&["holds", "--ids", "AGT-999", "--json"]), 3);
    // `--project` and `--ids` conflict (clap), exit 2.
    assert_code(
        &sb.pm(&["holds", "--project", "pm", "--ids", &a, "--json"]),
        2,
    );
}

// ---------------------------------------------------------------- AC2

#[test]
fn set_accepts_strict_marker_dates() {
    let sb = Sandbox::new();
    let a = sb.new_ticket(&["--project", "pm"]);

    assert_code(
        &sb.pm(&["set", &a, "not_before=2026-10-01", "parked=forever"]),
        0,
    );
    let t = sb.show(&a);
    assert_eq!(t["not_before"]["date"], "2026-10-01");
    assert_eq!(t["parked"]["until"], "forever");
    assert!(t["ext"].as_object().unwrap().is_empty(), "{t}");

    assert_code(&sb.pm(&["set", &a, "parked=2027-02-28"]), 0);
    assert_eq!(sb.show(&a)["parked"]["until"], "2027-02-28");

    for bad in [
        "not_before=2026-02-30",
        "not_before=2026-10-1",
        "not_before=tomorrow",
        "parked=never",
        "parked=2026-13-01",
    ] {
        let out = sb.pm(&["set", &a, bad]);
        assert_code(&out, 2);
        assert!(
            stderr(&out).contains("YYYY-MM-DD"),
            "{bad}: {}",
            stderr(&out)
        );
    }
    // A bad date anywhere in the invocation lands nothing.
    assert_code(&sb.pm(&["set", &a, "parked=", "not_before=bad"]), 2);
    assert_eq!(sb.show(&a)["parked"]["until"], "2027-02-28");

    assert_code(&sb.pm(&["set", &a, "not_before=", "parked="]), 0);
    let t = sb.show(&a);
    assert!(t["not_before"].is_null() && t["parked"].is_null(), "{t}");
}

#[test]
fn waive_extends_the_waiver_list() {
    let sb = Sandbox::new();
    let a = sb.new_ticket(&[]);
    assert_code(&sb.pm(&["waive", &a, "r1", "standalone: one-off"]), 0);
    assert_code(&sb.pm(&["waive", &a, "R3", "hand-written"]), 0);
    assert_code(&sb.pm(&["waive", &a, "R1", "standalone: reworded"]), 0);
    assert_eq!(
        sb.show(&a)["waivers"],
        serde_json::json!([
            {"rule": "R1", "reason": "standalone: reworded"},
            {"rule": "R3", "reason": "hand-written"},
        ])
    );
    assert_code(&sb.pm(&["waive", &a, " ", "why"]), 2);
    assert_code(&sb.pm(&["waive", &a, "R1", ""]), 2);
}

// ---------------------------------------------------------------- AC4

#[test]
fn show_renders_markers_in_a_block_outside_the_description() {
    let sb = Sandbox::new();
    let a = sb.new_ticket(&["--project", "pm", "--description", "Body text.\n"]);
    let plain = stdout(&sb.pm(&["show", &a]));
    assert!(!plain.contains("markers:"), "{plain}");

    assert_code(&sb.pm(&["hold", &a, "waiting on design"]), 0);
    assert_code(
        &sb.pm(&["set", &a, "not_before=2026-10-01", "parked=forever"]),
        0,
    );
    assert_code(&sb.pm(&["waive", &a, "R3", "legacy"]), 0);
    let out = stdout(&sb.pm(&["show", &a]));
    let block = out.find("markers:").expect(&out);
    let body = out.find("Body text.").expect(&out);
    assert!(block < body, "{out}");
    let lines: Vec<&str> = out[block..body].lines().collect();
    assert!(
        lines[1].starts_with("  hold:        waiting on design (by tester, "),
        "{out}"
    );
    assert_eq!(
        &lines[2..5],
        [
            "  not-before:  2026-10-01",
            "  parked:      forever",
            "  waiver:      R3: legacy"
        ]
    );
    assert_eq!(sb.show(&a)["description"], "Body text.");
}

// ---------------------------------------------------------------- AC3

#[test]
fn check_is_clean_then_reports_every_finding() {
    let sb = Sandbox::new();
    let a = sb.new_ticket(&["--project", "pm"]);
    let b = sb.new_ticket(&["--project", "pm", "--blocked-by", &a]);
    let clean = sb.pm(&["check"]);
    assert_code(&clean, 0);
    assert!(stdout(&clean).contains("ok: no findings"));
    assert_eq!(json(&sb.pm(&["check", "--json"]))["ok"], true);

    // R1: no project; a waived one is fine.
    let bare = sb.new_ticket(&[]);
    let waived = sb.new_ticket(&[]);
    assert_code(&sb.pm(&["waive", &waived, "R1", "standalone: tooling"]), 0);
    // Held.
    assert_code(&sb.pm(&["hold", &a, "needs Matt"]), 0);
    // A blocker cycle: b blocks a, with a already blocking b.
    let mut store = sb.store();
    let ulid = |id: &str| -> Ulid { sb.show(id)["ulid"].as_str().unwrap().parse().unwrap() };
    let (ua, ub) = (ulid(&a), ulid(&b));
    let hlc = store.latest_hlc().unwrap();
    store
        .commit(&Op::new(
            Ulid::new(),
            Hlc::new(hlc.wall_ms + 1, 0),
            ActorId::new("tester"),
            ua,
            Payload::RelationAdd(RelationAdd {
                relation: Relation {
                    kind: RelationKind::Blocks,
                    from: ub,
                    to: ua,
                },
            }),
        ))
        .unwrap();

    let out = sb.pm(&["check", "--json"]);
    assert_code(&out, 1);
    let report: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(report["ok"], false);
    let found: Vec<(String, Vec<String>)> = report["findings"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| {
            (
                f["rule"].as_str().unwrap().to_string(),
                f["tickets"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|t| t.as_str().unwrap().to_string())
                    .collect(),
            )
        })
        .collect();
    let mut pair = vec![a.clone(), b.clone()];
    pair.sort_by_key(|id| if ulid(id) == ua.min(ub) { 0 } else { 1 });
    assert_eq!(
        found,
        [
            ("R1".to_string(), vec![bare.clone()]),
            ("held".to_string(), vec![a.clone()]),
            ("blocker-cycle".to_string(), pair),
        ]
    );
    assert_eq!(report["count"], 3);
    assert_eq!(report["findings"][1]["hold"]["reason"], "needs Matt");

    // Human output, and --project scoping (R1 has no project).
    let human = sb.pm(&["check", "--project", "pm"]);
    assert_code(&human, 1);
    let text = stdout(&human);
    assert!(
        text.contains("held") && text.contains("blocker-cycle"),
        "{text}"
    );
    assert!(!text.contains(&bare), "{text}");
    assert!(
        stderr(&human).contains("2 finding(s)"),
        "{}",
        stderr(&human)
    );
    sb.put_project("other");
    assert_code(&sb.pm(&["check", "--project", "other"]), 0);
    assert_code(&sb.pm(&["check", "--project", "nope"]), 3);

    // The database is still healthy.
    assert_code(&sb.pm(&["doctor"]), 0);
}

/// AGT-1575: a ticket parked `forever` is a `parked` finding (with its
/// age) until it is unparked or waived.
#[test]
fn check_reports_parked_forever_until_waived() {
    let sb = Sandbox::new();
    let t = sb.new_ticket(&["--project", "pm"]);
    let dated = sb.new_ticket(&["--project", "pm"]);
    assert_code(&sb.pm(&["set", &t, "parked=forever"]), 0);
    assert_code(&sb.pm(&["set", &dated, "parked=2999-01-01"]), 0);

    let out = sb.pm(&["check", "--json"]);
    assert_code(&out, 1);
    let report: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(report["count"], 1, "{report}");
    let f = &report["findings"][0];
    assert_eq!(f["rule"], "parked");
    assert_eq!(f["tickets"], serde_json::json!([t]));
    assert_eq!(f["days"], 0);
    assert_eq!(f["state"], "triage");

    let human = sb.pm(&["check", "--project", "pm"]);
    assert_code(&human, 1);
    assert!(
        stdout(&human).contains("parked forever for 0 days"),
        "{}",
        stdout(&human)
    );

    assert_code(&sb.pm(&["waive", &t, "parked", "reference only"]), 0);
    assert_code(&sb.pm(&["check"]), 0);
}

// ------------------------------------------- AGT-1576: bulk comment/hold/archive

fn comment_count(sb: &Sandbox, id: &str) -> usize {
    sb.show(id)["comments"].as_array().unwrap().len()
}

#[test]
fn bulk_forms_apply_to_every_id_repeated_or_comma_separated() {
    let sb = Sandbox::new();
    let a = sb.new_ticket(&[]);
    let b = sb.new_ticket(&[]);
    let c = sb.new_ticket(&[]);

    // Comment: repeat and comma-separate in one call; a ticket named twice
    // is commented once.
    let ab = format!("{a},{b}");
    let v = json(&sb.pm(&["comment", &ab, &c, &a, "provenance, noted", "--json"]));
    assert_eq!(v["schema"], 1);
    let results = v["results"].as_array().unwrap();
    let ids: Vec<&str> = results.iter().map(|r| r["id"].as_str().unwrap()).collect();
    assert_eq!(ids, [a.as_str(), b.as_str(), c.as_str()]);
    assert!(results.iter().all(|r| r["result"] == "commented"));
    assert_eq!(results[1]["ticket"]["id"], b.as_str());
    for id in [&a, &b, &c] {
        assert_eq!(comment_count(&sb, id), 1, "{id}");
        assert_eq!(sb.show(id)["comments"][0]["body"], "provenance, noted");
    }

    // Text output: one display id per line.
    let out = sb.pm(&["comment", &a, &b, "again"]);
    assert_code(&out, 0);
    assert_eq!(stdout(&out), format!("{a}\n{b}\n"));

    // Hold: every id held with its own op's HLC; then a mixed clear.
    let v = json(&sb.pm(&["hold", &a, &b, "needs Matt", "--json"]));
    assert!(
        v["results"]
            .as_array()
            .unwrap()
            .iter()
            .all(|r| r["result"] == "held")
    );
    for id in [&a, &b] {
        assert_eq!(sb.show(id)["hold"]["reason"], "needs Matt");
    }
    let out = sb.pm(&["hold", "--clear", &format!("{a},{c}"), "--json"]);
    let v = json(&out);
    let results: Vec<(&str, &str)> = v["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| (r["id"].as_str().unwrap(), r["result"].as_str().unwrap()))
        .collect();
    assert_eq!(results, [(a.as_str(), "cleared"), (c.as_str(), "not-held")]);
    assert!(stderr(&out).contains(&format!("{c} is not held")));
    assert!(sb.show(&a)["hold"].is_null());
    assert_eq!(sb.show(&b)["hold"]["reason"], "needs Matt");

    // Archive.
    let v = json(&sb.pm(&["archive", &a, &format!("{b},{c}"), "--force", "--json"]));
    assert_eq!(v["results"].as_array().unwrap().len(), 3);
    for id in [&a, &b, &c] {
        assert!(!sb.show(id)["archived_at"].is_null(), "{id}");
    }
}

#[test]
fn bulk_forms_write_nothing_when_any_id_is_unknown() {
    let sb = Sandbox::new();
    let a = sb.new_ticket(&[]);
    let b = sb.new_ticket(&[]);
    let ops_before = json(&sb.pm(&["log", &a, "--json"]));

    assert_code(&sb.pm(&["comment", &a, "AGT-999", &b, "x"]), 3);
    assert_code(&sb.pm(&["hold", &format!("{a},AGT-999"), "why"]), 3);
    assert_code(&sb.pm(&["archive", &a, &b, "AGT-999"]), 3);
    assert_code(&sb.pm(&["archive", &format!("{a},,{b}")]), 2);

    for id in [&a, &b] {
        let t = sb.show(id);
        assert_eq!(t["comments"].as_array().unwrap().len(), 0);
        assert!(t["hold"].is_null());
        assert!(t["archived_at"].is_null());
    }
    assert_eq!(json(&sb.pm(&["log", &a, "--json"])), ops_before);
}

#[test]
fn bulk_forms_keep_text_and_ids_apart() {
    let sb = Sandbox::new();
    let a = sb.new_ticket(&[]);
    let b = sb.new_ticket(&[]);
    // The text was forgotten: the last positional is an id, not a comment.
    let out = sb.pm(&["comment", &a, &b]);
    assert_code(&out, 2);
    assert!(stderr(&out).contains("looks like a ticket id"));
    assert_code(&sb.pm(&["hold", &a, &format!("{b},{a}")]), 2);
    // Text given twice: positional text next to --file / --clear.
    assert_code(&sb.pm(&["comment", &a, &b, "hi", "--file", "-"]), 2);
    assert_code(&sb.pm(&["hold", &a, &b, "why", "--clear"]), 2);
    // --auto takes no ids.
    assert_code(&sb.pm(&["archive", &a, &b, "--auto"]), 2);
    assert_eq!(comment_count(&sb, &a), 0);
    assert!(sb.show(&a)["hold"].is_null());
}
