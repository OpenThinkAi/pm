//! `pm ready` through the built binary (AGT-1343). Sandbox as in
//! `claim.rs`: temp HOME, cleared env, stdin closed.

use std::path::PathBuf;
use std::process::{Command, Output, Stdio};

use pm_core::op::FieldSet;
use pm_core::{ActorId, Clock, Op, Payload, Project, ProjectStatus};
use pm_store::Store;
use serde_json::Value;
use tempfile::TempDir;
use ulid::Ulid;

struct Sandbox {
    home: TempDir,
    ws: PathBuf,
}

impl Sandbox {
    /// An initialized AGT workspace (the config default) with `pm` and
    /// `other` projects.
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
        for id in ["pm", "other"] {
            sb.store()
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
        sb
    }

    /// The graph every test here starts from (numbered in this order):
    ///
    /// ```text
    /// AGT-1 base                      ready
    /// AGT-2 blocked by 1              wave 1
    /// AGT-3 held                      excluded: held
    /// AGT-4 blocked by 3              blocked by a held ticket
    /// AGT-5 blocked by 4              transitively blocked (via 4, root 3)
    /// AGT-6 manual                    excluded: label
    /// AGT-7 blocked by 6              blocked by a manual ticket
    /// AGT-8 model:sonnet-5, other     ready
    /// AGT-9 not_before 2999-01-01     excluded: not_before
    /// ```
    fn graph() -> Self {
        let sb = Sandbox::new();
        let spec = sb.home.path().join("batch.yaml");
        std::fs::write(
            &spec,
            "tickets:\n\
             \x20 - { ref: base, title: base, project: pm }\n\
             \x20 - { title: after base, project: pm, blocked-by: ['@base'] }\n\
             \x20 - { ref: held, title: held, project: pm }\n\
             \x20 - { ref: after-held, title: after held, project: pm, blocked-by: ['@held'] }\n\
             \x20 - { title: after after held, project: pm, blocked-by: ['@after-held'] }\n\
             \x20 - { ref: manual, title: manual, project: pm, labels: [manual] }\n\
             \x20 - { title: after manual, project: pm, blocked-by: ['@manual'] }\n\
             \x20 - { title: sonnet, project: other, labels: ['model:sonnet-5'] }\n\
             \x20 - { title: later, project: pm }\n",
        )
        .unwrap();
        assert_code(&sb.pm(&["new", "--batch", spec.to_str().unwrap()]), 0);
        assert_code(&sb.pm(&["hold", "AGT-3", "needs Matt"]), 0);
        assert_code(&sb.pm(&["set", "AGT-9", "not_before=2999-01-01"]), 0);
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

    fn ulid_of(&self, id: &str) -> Ulid {
        let v = json(&self.pm(&["show", id, "--json"]));
        v["ulid"].as_str().unwrap().parse().unwrap()
    }

    /// No `pm archive` yet (another ticket owns it): archives behind the
    /// CLI's back, as `read.rs` does.
    fn archive(&self, id: &str) {
        let ulid = self.ulid_of(id);
        let mut store = self.store();
        let hlc = Clock::from_latest(store.latest_hlc().unwrap()).send(1);
        store
            .commit(&Op::new(
                Ulid::new(),
                hlc,
                ActorId::new("tester"),
                ulid,
                Payload::FieldSet(FieldSet::ArchivedAt(Some(hlc))),
            ))
            .unwrap();
    }

    fn ready_json(&self, args: &[&str]) -> Value {
        let mut all = vec!["ready", "--json"];
        all.extend_from_slice(args);
        json(&self.pm(&all))
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
        "stdout:\n{}\nstderr:\n{}",
        stdout(out),
        stderr(out)
    );
}

fn json(out: &Output) -> Value {
    assert_code(out, 0);
    serde_json::from_str(&stdout(out)).unwrap_or_else(|e| panic!("{e}: {}", stdout(out)))
}

fn ids(v: &Value) -> Vec<&str> {
    v.as_array()
        .unwrap()
        .iter()
        .map(|t| t["id"].as_str().unwrap())
        .collect()
}

fn excluded<'a>(v: &'a Value, id: &str) -> &'a Value {
    v["excluded"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["id"] == id)
        .unwrap_or_else(|| panic!("{id} not in excluded: {}", v["excluded"]))
}

#[test]
fn ready_is_the_frontier_and_explain_gives_each_exclusion_its_first_reason() {
    let sb = Sandbox::graph();

    let out = sb.pm(&["ready"]);
    assert_code(&out, 0);
    let text = stdout(&out);
    let rows: Vec<&str> = text.lines().collect();
    assert_eq!(rows.len(), 2, "{text}");
    assert!(
        rows[0].starts_with("AGT-1  ") && rows[0].contains("  -  "),
        "{text}"
    );
    assert!(
        rows[1].starts_with("AGT-8  ") && rows[1].contains("sonnet-5"),
        "{text}"
    );
    assert!(!text.contains("excluded:"));

    let text = stdout(&sb.pm(&["ready", "--explain"]));
    let line = |id: &str| {
        text.lines()
            .find(|l| l.trim_start().starts_with(&format!("{id} ")))
            .unwrap_or_else(|| panic!("no line for {id} in:\n{text}"))
            .to_string()
    };
    assert!(text.contains("excluded:"), "{text}");
    assert!(line("AGT-2").ends_with("blocked by AGT-1"), "{text}");
    assert!(line("AGT-3").contains("held: needs Matt"), "{text}");
    assert!(
        line("AGT-4").contains("blocked by AGT-3 (held: needs Matt"),
        "{text}"
    );
    assert!(
        line("AGT-5").contains("transitively blocked by AGT-3 (held: needs Matt")
            && line("AGT-5").ends_with("via AGT-4"),
        "{text}"
    );
    assert!(line("AGT-6").ends_with("label manual"), "{text}");
    assert!(
        line("AGT-7").ends_with("blocked by AGT-6 (label manual)"),
        "{text}"
    );
    assert!(line("AGT-9").ends_with("not_before 2999-01-01"), "{text}");

    let v = sb.ready_json(&[]);
    assert_eq!(v["schema"], 1);
    assert_eq!(ids(&v["ready"]), ["AGT-1", "AGT-8"]);
    assert_eq!(v["ready"][0]["title"], "base");
    assert_eq!(excluded(&v, "AGT-2")["reason"], "blocked-by");
    assert_eq!(excluded(&v, "AGT-2")["blocker"], "AGT-1");
    assert_eq!(excluded(&v, "AGT-2")["gate"], Value::Null);
    assert_eq!(excluded(&v, "AGT-3")["reason"], "held");
    assert_eq!(excluded(&v, "AGT-4")["gate"]["kind"], "held");
    assert_eq!(excluded(&v, "AGT-5")["reason"], "transitively-blocked");
    assert_eq!(excluded(&v, "AGT-5")["via"], "AGT-4");
    assert_eq!(excluded(&v, "AGT-5")["root"], "AGT-3");
    assert_eq!(excluded(&v, "AGT-6")["reason"], "label");
    assert_eq!(excluded(&v, "AGT-7")["gate"]["label"], "manual");
    assert_eq!(excluded(&v, "AGT-9")["reason"], "not-before");
    // Waves: the frontier, then what building it unblocks; nothing behind
    // the held or manual ticket, nothing date-gated.
    assert_eq!(
        v["waves"],
        serde_json::json!([["AGT-1", "AGT-8"], ["AGT-2"]])
    );
}

#[test]
fn an_archived_blocker_is_done_and_releasing_a_hold_reopens_its_chain() {
    let sb = Sandbox::graph();
    sb.archive("AGT-1");
    let v = sb.ready_json(&["--project", "pm"]);
    assert_eq!(ids(&v["ready"]), ["AGT-2"], "{}", v["excluded"]);
    assert_eq!(v["waves"], serde_json::json!([["AGT-2"]]));

    assert_code(&sb.pm(&["hold", "AGT-3", "--clear"]), 0);
    let v = sb.ready_json(&["--project", "pm"]);
    assert_eq!(ids(&v["ready"]), ["AGT-2", "AGT-3"]);
    assert_eq!(
        v["waves"],
        serde_json::json!([["AGT-2", "AGT-3"], ["AGT-4"], ["AGT-5"]])
    );
}

#[test]
fn model_filter_matches_the_label_and_unlabelled_tickets_default_to_opus() {
    let sb = Sandbox::graph();
    let v = sb.ready_json(&["--model", "sonnet-5"]);
    assert_eq!(ids(&v["ready"]), ["AGT-8"]);
    assert_eq!(v["model"], "sonnet-5");
    let plain = excluded(&v, "AGT-1");
    assert_eq!(plain["reason"], "model");
    assert_eq!(
        plain["message"],
        "no model label (opus-5 by default), not sonnet-5"
    );
    // Waves are model-agnostic.
    assert_eq!(v["waves"][0], serde_json::json!(["AGT-1", "AGT-8"]));

    let v = sb.ready_json(&["--model", "opus-5"]);
    assert_eq!(ids(&v["ready"]), ["AGT-1"], "no label means opus-5");
    assert_eq!(
        excluded(&v, "AGT-8")["message"],
        "model sonnet-5, not opus-5"
    );
    let v = sb.ready_json(&["--model", "model:sonnet-5"]);
    assert_eq!(
        ids(&v["ready"]),
        ["AGT-8"],
        "the full label is accepted too"
    );

    let text = stdout(&sb.pm(&["ready", "--model", "opus-5", "--explain"]));
    assert!(text.contains("AGT-8  model sonnet-5, not opus-5"), "{text}");
}

#[test]
fn limit_ids_and_exclude_label_narrow_the_answer() {
    let sb = Sandbox::graph();
    let v = sb.ready_json(&["--limit", "1"]);
    assert_eq!(ids(&v["ready"]), ["AGT-1"]);
    assert_eq!(v["limit"], 1);
    assert_eq!(
        v["waves"][0],
        serde_json::json!(["AGT-1", "AGT-8"]),
        "--limit never cuts the waves"
    );

    // --ids: verdicts only for those, blockers still resolved through the
    // whole graph, and a non-candidate gets a `done` line.
    assert_code(&sb.pm(&["move", "AGT-1", "done"]), 0);
    let v = sb.ready_json(&["--ids", "AGT-1,AGT-2", "--ids", "AGT-5"]);
    assert_eq!(v["ids"], serde_json::json!(["AGT-1", "AGT-2", "AGT-5"]));
    assert_eq!(ids(&v["ready"]), ["AGT-2"]);
    assert_eq!(excluded(&v, "AGT-5")["reason"], "transitively-blocked");
    assert_eq!(excluded(&v, "AGT-1")["reason"], "done");
    assert_eq!(excluded(&v, "AGT-1")["message"], "done: in state 'done'");
    assert_eq!(v["waves"], serde_json::json!([["AGT-2"]]));
    let text = stdout(&sb.pm(&["ready", "--ids", "AGT-1", "--explain"]));
    assert!(text.contains("AGT-1  done: in state 'done'"), "{text}");

    // --exclude-label gates like `manual`, transitively.
    assert_code(&sb.pm(&["label", "AGT-2", "+live-session"]), 0);
    let v = sb.ready_json(&["--exclude-label", "live-session", "--project", "pm"]);
    assert_eq!(ids(&v["ready"]), Vec::<&str>::new());
    assert_eq!(excluded(&v, "AGT-2")["label"], "live-session");
    assert_eq!(v["waves"], serde_json::json!([]));
}

#[test]
fn claim_ready_takes_the_first_ticket_pm_ready_lists() {
    let sb = Sandbox::graph();
    let first = sb.ready_json(&["--project", "pm"])["ready"][0]["id"]
        .as_str()
        .unwrap()
        .to_string();
    let out = sb.pm(&["claim", "--ready", "--project", "pm"]);
    assert_code(&out, 0);
    assert_eq!(stdout(&out).trim(), first);
    // Claimed: started and assigned, so out of the frontier; AGT-2 waits
    // on it in wave 0 of what comes next.
    let v = sb.ready_json(&["--project", "pm"]);
    assert_eq!(ids(&v["ready"]), Vec::<&str>::new());
    let claimed = excluded(&v, &first);
    assert_eq!(claimed["reason"], "state");
    assert_eq!(claimed["state"], "in-progress");
    assert_eq!(v["waves"], serde_json::json!([["AGT-2"]]));
}

#[test]
fn empty_frontier_exits_0_and_bad_arguments_are_usage_or_not_found() {
    let sb = Sandbox::new();
    let out = sb.pm(&["ready"]);
    assert_code(&out, 0);
    assert_eq!(stdout(&out), "");
    assert!(stderr(&out).contains("no ready tickets"));
    let v = sb.ready_json(&[]);
    assert_eq!(v["ready"], serde_json::json!([]));
    assert_eq!(v["waves"], serde_json::json!([]));

    assert_code(&sb.pm(&["ready", "--project", "nope"]), 3);
    assert_code(&sb.pm(&["ready", "--ids", "AGT-99"]), 3);
    assert_code(&sb.pm(&["ready", "--limit", "0"]), 2);
    assert_code(&sb.pm(&["ready", "--project", "pm", "--ids", "AGT-1"]), 2);
}
