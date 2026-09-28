//! `pm init/new/show/set` driven through the built binary (AGT-1336).
//!
//! Every run gets a fresh temp HOME and a cleared environment, so nothing
//! reads or writes the real `~/.config/pm`, and stdin is `/dev/null`, so a
//! command that tried to prompt would read EOF rather than hang.

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use pm_core::{ActorId, Payload, Project, ProjectStatus, StateCategory};
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

    /// A sandbox with an initialized workspace at `self.ws`, recorded as
    /// the default in the sandbox's config.toml, and a `pm` project.
    fn initialized() -> Self {
        let sb = Sandbox::new();
        let out = sb.run(
            &["init", "--prefix", "AGT", "--workspace", sb.ws_str()],
            &[],
        );
        assert_ok(&out);
        sb.put_project("pm");
        sb
    }

    fn ws_str(&self) -> &str {
        self.ws.to_str().unwrap()
    }

    fn config_path(&self) -> PathBuf {
        self.home.path().join(".config/pm/config.toml")
    }

    /// `pm` with a cleared environment: HOME is the sandbox, USER is
    /// `tester`, plus `extra`.
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

    /// Projects have no CLI verb yet; they are config rows written
    /// directly, as `pm-store` intends.
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
    serde_json::from_slice(&out.stdout).unwrap()
}

fn ulid_of(sb: &Sandbox, id: &str) -> Ulid {
    let v = json(&sb.pm(&["show", id, "--json"]));
    v["ulid"].as_str().unwrap().parse().unwrap()
}

// ---------------------------------------------------------------- AC1

#[test]
fn init_creates_config_and_a_workspace_with_saltline_states() {
    let sb = Sandbox::new();
    let out = sb.pm(&["init", "--prefix", "AGT", "--workspace", sb.ws_str()]);
    assert_ok(&out);

    let config = std::fs::read_to_string(sb.config_path()).unwrap();
    let ws = std::fs::canonicalize(&sb.ws).unwrap();
    let parsed: toml::Table = toml::from_str(&config).unwrap();
    assert_eq!(
        Path::new(parsed["workspace"].as_str().unwrap()),
        ws.as_path()
    );

    let workspace = sb.store().workspace().unwrap().unwrap();
    assert_eq!(workspace.prefix, "AGT");
    let states: Vec<(&str, StateCategory)> = workspace
        .states
        .iter()
        .map(|s| (s.name.as_str(), s.category))
        .collect();
    assert_eq!(
        states,
        [
            ("triage", StateCategory::Unstarted),
            ("in-progress", StateCategory::Started),
            ("done", StateCategory::Completed),
        ]
    );
}

#[test]
fn init_without_workspace_uses_the_default_data_dir_and_config_finds_it() {
    let sb = Sandbox::new();
    let v = json(&sb.pm(&["init", "--prefix", "AGT", "--json"]));
    assert_eq!(v["schema"], 1);
    assert_eq!(v["config_written"], true);
    let expected = std::fs::canonicalize(sb.home.path())
        .unwrap()
        .join(".local/share/pm/agt");
    assert_eq!(Path::new(v["workspace"].as_str().unwrap()), expected);
    assert!(expected.join("pm.sqlite").is_file());

    // No flag, no PM_WORKSPACE: config.toml resolves the workspace.
    assert_ok(&sb.pm(&["new", "--title", "via config"]));
    assert_eq!(
        stdout(&sb.pm(&["show", "AGT-1", "--field", "title"])),
        "via config\n"
    );
}

#[test]
fn init_refuses_to_reinitialize_and_never_rewrites_config() {
    let sb = Sandbox::initialized();
    let config = std::fs::read_to_string(sb.config_path()).unwrap();

    let again = sb.pm(&["init", "--prefix", "AGT", "--workspace", sb.ws_str()]);
    assert_code(&again, 1);
    assert!(
        stderr(&again).contains("already a pm workspace"),
        "{}",
        stderr(&again)
    );

    let other = sb.home.path().join("other");
    let v = json(&sb.pm(&[
        "init",
        "--prefix",
        "OT",
        "--workspace",
        other.to_str().unwrap(),
        "--json",
    ]));
    assert_eq!(v["config_written"], false);
    assert_eq!(std::fs::read_to_string(sb.config_path()).unwrap(), config);
}

#[test]
fn init_rejects_a_bad_prefix_as_usage() {
    let sb = Sandbox::new();
    assert_code(&sb.pm(&["init", "--prefix", "agt-1"]), 2);
    assert_code(&sb.pm(&["init"]), 2);
    assert!(!sb.config_path().exists());
}

// ---------------------------------------------------------------- AC2

#[test]
fn new_emits_create_and_label_ops_and_allocates_a_number() {
    let sb = Sandbox::initialized();
    let out = sb.pm(&[
        "new",
        "--title",
        "First",
        "--project",
        "pm",
        "--repo",
        "OpenThinkAi/pm",
        "--priority",
        "high",
        "--label",
        "x",
        "--label",
        "y,z",
    ]);
    assert_ok(&out);
    assert_eq!(stdout(&out), "AGT-1\n");

    let v = json(&sb.pm(&["new", "--title", "Second", "--json"]));
    assert_eq!(v["schema"], 1);
    assert_eq!(v["id"], "AGT-2");
    assert_eq!(v["number"], 2);
    assert_eq!(v["title"], "Second");
    assert_eq!(v["state"], "triage");
    assert_eq!(v["priority"], "medium");

    let id = ulid_of(&sb, "AGT-1");
    let kinds: Vec<String> = sb
        .store()
        .ops(id)
        .unwrap()
        .iter()
        .map(|op| op.kind().to_string())
        .collect();
    assert_eq!(
        kinds,
        [
            "ticket.create",
            "label.add",
            "label.add",
            "label.add",
            "field.set"
        ]
    );
}

#[test]
fn new_against_a_missing_project_is_not_found_and_writes_nothing() {
    let sb = Sandbox::initialized();
    let out = sb.pm(&["new", "--title", "T", "--project", "nope"]);
    assert_code(&out, 3);
    assert!(
        stderr(&out).contains("project 'nope' does not exist"),
        "{}",
        stderr(&out)
    );
    assert!(sb.store().tickets(&Default::default()).unwrap().is_empty());
}

#[test]
fn new_usage_errors_exit_2() {
    let sb = Sandbox::initialized();
    assert_code(&sb.pm(&["new"]), 2); // --title is required
    assert_code(&sb.pm(&["new", "--title", "  "]), 2);
    assert_code(&sb.pm(&["new", "--title", "T", "--priority", "urgent"]), 2);
    assert!(sb.store().tickets(&Default::default()).unwrap().is_empty());
}

// ---------------------------------------------------------------- AC3

#[test]
fn show_prints_human_json_and_single_fields() {
    let sb = Sandbox::initialized();
    assert_ok(&sb.pm(&[
        "new",
        "--title",
        "Show me",
        "--project",
        "pm",
        "--repo",
        "r",
        "--label",
        "b,a",
    ]));

    let human = stdout(&sb.pm(&["show", "AGT-1"]));
    assert!(human.starts_with("AGT-1  Show me\n"), "{human}");
    assert!(human.contains("state:     triage\n"), "{human}");
    assert!(human.contains("labels:    a, b\n"), "{human}");

    let v = json(&sb.pm(&["show", "AGT-1", "--json"]));
    for key in [
        "schema",
        "id",
        "ulid",
        "number",
        "title",
        "state",
        "priority",
        "project",
        "repo",
        "assignee",
        "description",
        "labels",
        "created",
        "updated",
        "archived_at",
        "deleted",
        "linked_github",
        "linked_pr",
        "linear",
        "source",
        "hold",
        "waivers",
        "not_before",
        "parked",
        "ext",
    ] {
        assert!(v.get(key).is_some(), "missing {key}: {v}");
    }
    assert_eq!(v["labels"], serde_json::json!(["a", "b"]));

    // By ULID too, and the prefix is case-insensitive.
    let ulid = v["ulid"].as_str().unwrap();
    assert_eq!(json(&sb.pm(&["show", ulid, "--json"]))["id"], "AGT-1");
    assert_ok(&sb.pm(&["show", "agt-1"]));

    assert_eq!(
        stdout(&sb.pm(&["show", "AGT-1", "--field", "title"])),
        "Show me\n"
    );
    assert_eq!(
        stdout(&sb.pm(&["show", "AGT-1", "--field", "labels"])),
        "a\nb\n"
    );
    assert_eq!(
        stdout(&sb.pm(&["show", "AGT-1", "--field", "linked-pr"])),
        "\n"
    );
    assert_eq!(
        json(&sb.pm(&["show", "AGT-1", "--field", "title", "--json"])),
        serde_json::json!({"schema": 1, "title": "Show me"})
    );
    assert_code(&sb.pm(&["show", "AGT-1", "--field", "nope"]), 2);
}

#[test]
fn show_exit_codes() {
    let sb = Sandbox::initialized();
    assert_code(&sb.pm(&["show", "AGT-99"]), 3);
    assert_code(&sb.pm(&["show", "XYZ-1"]), 3);
    assert_code(&sb.pm(&["show", &Ulid::new().to_string()]), 3);
    assert_code(&sb.pm(&["show", "not-an-id"]), 2);
    assert_code(&sb.pm(&["show"]), 2);
}

// ---------------------------------------------------------------- AC4

#[test]
fn set_records_field_set_ops_that_show_reflects() {
    let sb = Sandbox::initialized();
    assert_ok(&sb.pm(&["new", "--title", "Old", "--repo", "r"]));
    let out = sb.pm(&[
        "set",
        "AGT-1",
        "title=New title",
        "priority=critical",
        "repo=",
    ]);
    assert_ok(&out);
    assert_eq!(stdout(&out), "AGT-1\n");

    assert_eq!(
        stdout(&sb.pm(&["show", "AGT-1", "--field", "title"])),
        "New title\n"
    );
    let v = json(&sb.pm(&["show", "AGT-1", "--json"]));
    assert_eq!(v["priority"], "critical");
    assert_eq!(v["repo"], Value::Null);

    let ops = sb.store().ops(ulid_of(&sb, "AGT-1")).unwrap();
    let sets: Vec<_> = ops
        .iter()
        .filter(|op| op.kind() == "field.set")
        .map(|op| serde_json::to_value(&op.payload).unwrap()["payload"]["field"].clone())
        .collect();
    assert_eq!(sets, ["number", "title", "priority", "repo"]);
}

#[test]
fn set_validates_every_assignment_before_writing() {
    let sb = Sandbox::initialized();
    assert_ok(&sb.pm(&["new", "--title", "Keep"]));
    let before = sb.store().ops(ulid_of(&sb, "AGT-1")).unwrap().len();

    assert_code(&sb.pm(&["set", "AGT-1", "title=X", "bogus=1"]), 2);
    assert_code(&sb.pm(&["set", "AGT-1", "title=X", "noequals"]), 2);
    assert_code(&sb.pm(&["set", "AGT-1", "title="]), 2);
    assert_code(&sb.pm(&["set", "AGT-1", "title=X", "project=missing"]), 3);
    assert_code(&sb.pm(&["set", "AGT-1"]), 2);
    assert_code(&sb.pm(&["set", "AGT-7", "title=X"]), 3);

    assert_eq!(sb.store().ops(ulid_of(&sb, "AGT-1")).unwrap().len(), before);
    assert_eq!(
        stdout(&sb.pm(&["show", "AGT-1", "--field", "title"])),
        "Keep\n"
    );
}

// ---------------------------------------------------------------- AC5

fn actors(sb: &Sandbox, id: &str) -> Vec<ActorId> {
    sb.store()
        .ops(ulid_of(sb, id))
        .unwrap()
        .into_iter()
        .map(|op| op.actor)
        .collect()
}

#[test]
fn every_op_records_pm_actor_then_as_then_user() {
    let sb = Sandbox::initialized();

    // $USER only.
    assert_ok(&sb.pm(&["new", "--title", "T", "--label", "x"]));
    assert!(actors(&sb, "AGT-1").iter().all(|a| a.as_str() == "tester"));

    // --as beats $USER.
    assert_ok(&sb.pm(&["set", "AGT-1", "title=by bob", "--as", "bob"]));
    assert_eq!(actors(&sb, "AGT-1").last().unwrap().as_str(), "bob");

    // PM_ACTOR beats --as, on every op a command emits.
    assert_ok(&sb.run(
        &["new", "--title", "agent", "--label", "a,b", "--as", "bob"],
        &[("PM_ACTOR", "claude:pm-build")],
    ));
    let agent = actors(&sb, "AGT-2");
    assert_eq!(agent.len(), 4); // create + 2 labels + number
    assert!(
        agent.iter().all(|a| a.as_str() == "claude:pm-build"),
        "{agent:?}"
    );

    let create = &sb.store().ops(ulid_of(&sb, "AGT-2")).unwrap()[0];
    assert!(matches!(create.payload, Payload::TicketCreate(_)));
}

#[test]
fn no_actor_at_all_is_a_usage_error() {
    let sb = Sandbox::initialized();
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_pm"));
    let out = cmd
        .args(["new", "--title", "T"])
        .env_clear()
        .env("HOME", sb.home.path())
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert_code(&out, 2);
    assert!(stderr(&out).contains("no actor"), "{}", stderr(&out));
}

#[test]
fn workspace_resolution_flag_then_env_then_config() {
    let sb = Sandbox::initialized(); // config.toml -> sb.ws
    let second = sb.home.path().join("second");
    assert_ok(&sb.pm(&[
        "init",
        "--prefix",
        "SEC",
        "--workspace",
        second.to_str().unwrap(),
    ]));
    let third = sb.home.path().join("third");
    assert_ok(&sb.pm(&[
        "init",
        "--prefix",
        "THR",
        "--workspace",
        third.to_str().unwrap(),
    ]));

    let env = [("PM_WORKSPACE", second.to_str().unwrap())];
    assert_eq!(stdout(&sb.run(&["new", "--title", "env"], &env)), "SEC-1\n");
    assert_eq!(
        stdout(&sb.run(
            &[
                "new",
                "--title",
                "flag",
                "--workspace",
                third.to_str().unwrap()
            ],
            &env
        )),
        "THR-1\n"
    );
    assert_eq!(stdout(&sb.pm(&["new", "--title", "config"])), "AGT-1\n");
}

#[test]
fn a_missing_workspace_is_an_error_not_a_new_database() {
    let sb = Sandbox::new();
    let out = sb.pm(&["show", "AGT-1"]);
    assert_code(&out, 1);
    assert!(stderr(&out).contains("no workspace"), "{}", stderr(&out));

    let out = sb.pm(&["new", "--title", "T", "--workspace", sb.ws_str()]);
    assert_code(&out, 1);
    assert!(
        stderr(&out).contains("not a pm workspace"),
        "{}",
        stderr(&out)
    );
    assert!(!sb.ws.join("pm.sqlite").exists());
}

#[test]
fn legacy_ticket_commands_still_read_markdown() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("AGT-5-x.md"),
        "---\nid: AGT-5\ntitle: \"Legacy\"\nstate: triage\npriority: high\nproject: pm\n---\n",
    )
    .unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_pm"))
        .args(["ticket", "show"])
        .arg(dir.path().join("AGT-5-x.md"))
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert_ok(&out);
    assert!(stdout(&out).contains("Legacy"), "{}", stdout(&out));
}
