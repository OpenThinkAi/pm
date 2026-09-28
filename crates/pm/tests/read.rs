//! `pm list`, `pm log`, `pm status`, `pm graph` and `pm show --section`
//! (AGT-1339), driven through the built binary — same pattern as
//! `tests/cli.rs` (a fresh temp HOME + workspace per run, stdin closed).
//! This file keeps its own small `Sandbox` rather than sharing `cli.rs`'s
//! (Rust integration tests are separate binaries and can't share private
//! items across `tests/*.rs` files without a `tests/common/mod.rs`), which
//! also keeps this ticket's diff out of a file other in-flight tickets may
//! be touching.

use std::path::PathBuf;
use std::process::{Command, Output, Stdio};

use pm_core::op::StateTransition;
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
    fn new() -> Self {
        let home = tempfile::tempdir().unwrap();
        let ws = home.path().join("ws");
        Sandbox { home, ws }
    }

    fn initialized() -> Self {
        let sb = Sandbox::new();
        assert_ok(&sb.run(
            &["init", "--prefix", "AGT", "--workspace", sb.ws_str()],
            &[],
        ));
        sb.put_project("pm");
        sb
    }

    fn ws_str(&self) -> &str {
        self.ws.to_str().unwrap()
    }

    fn run(&self, args: &[&str], extra: &[(&str, &str)]) -> Output {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_pm"));
        cmd.args(args)
            .env_clear()
            .env("HOME", self.home.path())
            .env("USER", "tester")
            .stdin(Stdio::null());
        for (k, v) in extra {
            cmd.env(k, v);
        }
        cmd.output().unwrap()
    }

    fn pm(&self, args: &[&str]) -> Output {
        self.run(args, &[])
    }

    fn put_project(&self, id: &str) {
        let mut store = Store::open(self.ws.join("pm.sqlite")).unwrap();
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

    fn store(&self) -> Store {
        Store::open(self.ws.join("pm.sqlite")).unwrap()
    }

    /// Neither `pm archive` nor a state-mutating verb has landed yet
    /// (other in-flight tickets own those); this reaches into the store
    /// directly, the same way `cli.rs`'s `Sandbox::put_project` does for
    /// projects.
    fn transition(&self, ticket: Ulid, state: &str) {
        let mut store = self.store();
        let hlc = Clock::from_latest(store.latest_hlc().unwrap()).send(now_ms());
        store
            .commit(&Op::new(
                Ulid::new(),
                hlc,
                ActorId::new("tester"),
                ticket,
                Payload::StateTransition(StateTransition {
                    state: state.to_string(),
                }),
            ))
            .unwrap();
    }

    fn archive(&self, ticket: Ulid) {
        let mut store = self.store();
        let hlc = Clock::from_latest(store.latest_hlc().unwrap()).send(now_ms());
        store
            .commit(&Op::new(
                Ulid::new(),
                hlc,
                ActorId::new("tester"),
                ticket,
                Payload::FieldSet(pm_core::op::FieldSet::ArchivedAt(Some(hlc))),
            ))
            .unwrap();
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

fn stdout(out: &Output) -> String {
    String::from_utf8(out.stdout.clone()).unwrap()
}

fn assert_ok(out: &Output) {
    assert_eq!(
        out.status.code(),
        Some(0),
        "stdout: {}\nstderr: {}",
        stdout(out),
        String::from_utf8(out.stderr.clone()).unwrap()
    );
}

fn assert_code(out: &Output, code: i32) {
    assert_eq!(
        out.status.code(),
        Some(code),
        "stdout: {}\nstderr: {}",
        stdout(out),
        String::from_utf8(out.stderr.clone()).unwrap()
    );
}

fn json(out: &Output) -> Value {
    assert_ok(out);
    serde_json::from_str(&stdout(out)).unwrap()
}

fn ulid_of(sb: &Sandbox, id: &str) -> Ulid {
    json(&sb.pm(&["show", id, "--json"]))["ulid"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap()
}

// ---------------------------------------------------------------- pm list

#[test]
fn list_and_combines_filters_and_prints_a_table_or_json() {
    let sb = Sandbox::initialized();
    sb.put_project("other");
    assert_ok(&sb.pm(&[
        "new",
        "--title",
        "Fix the login bug",
        "--project",
        "pm",
        "--repo",
        "OpenThinkAi/pm",
        "--label",
        "model:fable-5",
    ]));
    assert_ok(&sb.pm(&[
        "new",
        "--title",
        "Write docs",
        "--project",
        "other",
        "--repo",
        "OpenThinkAi/other",
    ]));
    assert_ok(&sb.pm(&[
        "new",
        "--title",
        "Another pm ticket",
        "--project",
        "pm",
        "--linked-github",
        "https://github.com/OpenThinkAi/pm/issues/9",
    ]));

    // No filters: every live ticket, numbered order.
    let all = json(&sb.pm(&["list", "--json"]));
    assert_eq!(all.as_array().unwrap().len(), 3);
    for t in all.as_array().unwrap() {
        assert!(t.get("schema").is_some());
        assert!(t.get("id").is_some());
    }

    // --project (AND with nothing else here).
    let pm_only = json(&sb.pm(&["list", "--project", "pm", "--json"]));
    let ids: Vec<&str> = pm_only
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, ["AGT-1", "AGT-3"]);

    // --project and --search AND together.
    let combo = json(&sb.pm(&["list", "--project", "pm", "--search", "login", "--json"]));
    assert_eq!(combo.as_array().unwrap().len(), 1);
    assert_eq!(combo[0]["id"], "AGT-1");

    // Comma-separated values OR within one flag (AC1).
    let multi = json(&sb.pm(&["list", "--project", "pm,other", "--json"]));
    assert_eq!(multi.as_array().unwrap().len(), 3);

    // --label.
    let by_label = json(&sb.pm(&["list", "--label", "model:fable-5", "--json"]));
    assert_eq!(by_label.as_array().unwrap().len(), 1);
    assert_eq!(by_label[0]["id"], "AGT-1");

    // --repo.
    let by_repo = json(&sb.pm(&["list", "--repo", "OpenThinkAi/other", "--json"]));
    assert_eq!(by_repo.as_array().unwrap().len(), 1);

    // --github.
    let by_gh = json(&sb.pm(&[
        "list",
        "--github",
        "https://github.com/OpenThinkAi/pm/issues/9",
        "--json",
    ]));
    assert_eq!(by_gh.as_array().unwrap().len(), 1);
    assert_eq!(by_gh[0]["id"], "AGT-3");

    // --linked-github is an alias for --github, matching `pm new
    // --linked-github`'s flag name for the same field.
    let by_gh_alias = json(&sb.pm(&[
        "list",
        "--linked-github",
        "https://github.com/OpenThinkAi/pm/issues/9",
        "--json",
    ]));
    assert_eq!(by_gh_alias, by_gh);

    // --state (every ticket starts triage).
    let by_state = json(&sb.pm(&["list", "--state", "triage", "--json"]));
    assert_eq!(by_state.as_array().unwrap().len(), 3);
    assert!(
        json(&sb.pm(&["list", "--state", "done", "--json"]))
            .as_array()
            .unwrap()
            .is_empty()
    );

    // Human table: a header-free, non-empty line per ticket.
    let human = stdout(&sb.pm(&["list"]));
    assert_eq!(human.lines().count(), 3, "{human}");
    assert!(human.contains("AGT-1"), "{human}");
    assert!(human.contains("Fix the login bug"), "{human}");
}

#[test]
fn list_archived_is_excluded_unless_asked_for() {
    let sb = Sandbox::initialized();
    assert_ok(&sb.pm(&["new", "--title", "Old ticket", "--project", "pm"]));
    let id = ulid_of(&sb, "AGT-1");
    sb.archive(id);

    assert!(
        json(&sb.pm(&["list", "--json"]))
            .as_array()
            .unwrap()
            .is_empty(),
        "archived tickets are excluded by default"
    );
    let with_archived = json(&sb.pm(&["list", "--archived", "--json"]));
    assert_eq!(with_archived.as_array().unwrap().len(), 1);
}

#[test]
fn list_with_no_matches_exits_0_with_no_stdout() {
    let sb = Sandbox::initialized();
    let out = sb.pm(&["list", "--project", "nope"]);
    assert_ok(&out);
    assert_eq!(stdout(&out), "");
}

// ----------------------------------------------------------------- pm log

#[test]
fn log_lists_ops_oldest_first_with_hlc_actor_kind_and_summary() {
    let sb = Sandbox::initialized();
    assert_ok(&sb.pm(&["new", "--title", "Loggable", "--project", "pm"]));
    assert_ok(&sb.pm(&["set", "AGT-1", "title=Renamed"]));

    let ops = json(&sb.pm(&["log", "AGT-1", "--json"]));
    let ops = ops.as_array().unwrap();
    assert!(ops.len() >= 2, "{ops:?}");
    assert_eq!(ops[0]["kind"], "ticket.create");
    assert!(ops[0]["summary"].as_str().unwrap().contains("Loggable"));
    for op in ops {
        assert!(op.get("schema").is_some());
        assert!(op.get("hlc").is_some());
        assert!(op.get("actor").is_some());
        assert!(op.get("summary").is_some());
    }
    let last = ops.last().unwrap();
    assert_eq!(last["kind"], "field.set");
    assert!(last["summary"].as_str().unwrap().contains("Renamed"));

    // hlc order is non-decreasing (oldest first).
    let walls: Vec<u64> = ops
        .iter()
        .map(|o| o["hlc"]["wall_ms"].as_u64().unwrap())
        .collect();
    assert!(walls.windows(2).all(|w| w[0] <= w[1]), "{walls:?}");

    let human = stdout(&sb.pm(&["log", "AGT-1"]));
    assert!(human.contains("ticket.create"), "{human}");
    assert!(human.contains("field.set"), "{human}");
    assert!(human.contains("tester"), "{human}");
}

#[test]
fn log_of_an_unknown_ticket_is_not_found() {
    let sb = Sandbox::initialized();
    assert_code(&sb.pm(&["log", "AGT-99"]), 3);
}

// -------------------------------------------------------------- pm status

#[test]
fn status_counts_per_state_plus_held_and_parked() {
    let sb = Sandbox::initialized();
    assert_ok(&sb.pm(&["new", "--title", "A", "--project", "pm"]));
    assert_ok(&sb.pm(&["new", "--title", "B", "--project", "pm"]));
    let b = ulid_of(&sb, "AGT-2");
    sb.transition(b, "in-progress");

    let out = json(&sb.pm(&["status", "--project", "pm", "--json"]));
    assert_eq!(out["schema"], 1);
    assert_eq!(out["project"], "pm");
    // AGT-1352: `states` is an ordered array (workflow order), not a map —
    // keys used to sort alphabetically (AGT-1339 review).
    assert_eq!(
        out["states"],
        serde_json::json!([
            {"name": "triage", "category": "unstarted", "count": 1},
            {"name": "in-progress", "category": "started", "count": 1},
            {"name": "done", "category": "completed", "count": 0},
        ])
    );
    assert_eq!(out["held"], 0);
    assert_eq!(out["parked"], 0);

    let human = stdout(&sb.pm(&["status"]));
    assert!(human.contains("triage"), "{human}");
    assert!(human.contains("held"), "{human}");
    assert!(human.contains("parked"), "{human}");
}

// --------------------------------------------------------------- pm graph

#[test]
fn graph_groups_into_waves_by_blockers_and_reports_done() {
    let sb = Sandbox::initialized();
    assert_ok(&sb.pm(&["new", "--title", "Base", "--project", "pm"]));
    assert_ok(&sb.pm(&[
        "new",
        "--title",
        "Depends on base",
        "--project",
        "pm",
        "--blocked-by",
        "AGT-1",
    ]));

    let out = json(&sb.pm(&["graph", "--project", "pm", "--json"]));
    assert_eq!(out["done"], false);
    let waves = out["waves"].as_array().unwrap();
    assert_eq!(waves.len(), 2, "{waves:?}");
    assert_eq!(waves[0], serde_json::json!(["AGT-1"]));
    assert_eq!(waves[1], serde_json::json!(["AGT-2"]));

    // Finishing the blocker collapses AGT-2 into wave 0; the project isn't
    // done until every ticket is (README §CLI verbs: "waves, done flag").
    let base = ulid_of(&sb, "AGT-1");
    sb.transition(base, "done");
    let out = json(&sb.pm(&["graph", "--project", "pm", "--json"]));
    assert_eq!(out["done"], false);
    assert_eq!(out["waves"], serde_json::json!([["AGT-2"]]));

    let dependent = ulid_of(&sb, "AGT-2");
    sb.transition(dependent, "done");
    let out = json(&sb.pm(&["graph", "--project", "pm", "--json"]));
    assert_eq!(out["done"], true);
    assert_eq!(out["waves"], serde_json::json!([]));

    let human = stdout(&sb.pm(&["graph", "--project", "pm"]));
    assert!(human.contains("done: true"), "{human}");
}

#[test]
fn graph_treats_an_archived_blocker_as_done() {
    let sb = Sandbox::initialized();
    assert_ok(&sb.pm(&["new", "--title", "Base", "--project", "pm"]));
    assert_ok(&sb.pm(&[
        "new",
        "--title",
        "Depends on base",
        "--project",
        "pm",
        "--blocked-by",
        "AGT-1",
    ]));
    // Archived straight from `triage`: it leaves the graph and no longer
    // holds AGT-2 back (AGT-1343: archive-as-done).
    sb.archive(ulid_of(&sb, "AGT-1"));
    let out = json(&sb.pm(&["graph", "--project", "pm", "--json"]));
    assert_eq!(out["waves"], serde_json::json!([["AGT-2"]]));
    assert_eq!(out["done"], false);
}

// ---------------------------------------------------------- pm show --section

#[test]
fn show_section_prints_one_body_section() {
    let sb = Sandbox::initialized();
    let body = "## Problem Statement\n\nSomething is broken.\n\n\
                ## Acceptance Criteria\n\n1. It works\n2. Tests pass\n\n\
                ## Comments\n\nnone yet";
    assert_ok(&sb.pm(&[
        "new",
        "--title",
        "Sectioned",
        "--project",
        "pm",
        "--description",
        body,
    ]));

    let out = stdout(&sb.pm(&["show", "AGT-1", "--section", "Acceptance Criteria"]));
    assert_eq!(out, "1. It works\n2. Tests pass\n");

    // Case-insensitive.
    let out = stdout(&sb.pm(&["show", "AGT-1", "--section", "acceptance criteria"]));
    assert_eq!(out, "1. It works\n2. Tests pass\n");

    let v = json(&sb.pm(&["show", "AGT-1", "--section", "Comments", "--json"]));
    assert_eq!(v["schema"], 1);
    assert_eq!(v["section"], "Comments");
    assert_eq!(v["body"], "none yet");

    assert_code(&sb.pm(&["show", "AGT-1", "--section", "Nope"]), 3);
    assert_code(
        &sb.pm(&["show", "AGT-1", "--field", "title", "--section", "Comments"]),
        2,
    );
}
