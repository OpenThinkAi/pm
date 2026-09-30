//! `pm init/new/show/set` driven through the built binary (AGT-1336).
//!
//! Every run gets a fresh temp HOME and a cleared environment, so nothing
//! reads or writes the real `~/.config/pm`, and stdin is `/dev/null`, so a
//! command that tried to prompt would read EOF rather than hang.

use std::io::Write as _;
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
            &[
                "init",
                "--prefix",
                "AGT",
                "--preset",
                "saltline",
                "--workspace",
                sb.ws_str(),
            ],
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

    /// Like [`Sandbox::run`], but pipes `input` to the child's stdin
    /// instead of closing it (AGT-1346: `--description-file -`).
    fn run_with_stdin(&self, args: &[&str], input: &str) -> Output {
        let mut child = Command::new(env!("CARGO_BIN_EXE_pm"))
            .args(args)
            .env_clear()
            .env("HOME", self.home.path())
            .env("USER", "tester")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(input.as_bytes())
            .unwrap();
        child.wait_with_output().unwrap()
    }

    /// Writes `contents` to `name` under the sandbox's home dir and
    /// returns its path — a fixture file for `--from-file` / `--batch`.
    fn fixture(&self, name: &str, contents: &str) -> PathBuf {
        let path = self.home.path().join(name);
        std::fs::write(&path, contents).unwrap();
        path
    }

    /// Projects have no CLI verb yet; they are config rows written
    /// directly, as `pm-store` intends.
    fn put_project(&self, id: &str) {
        let mut store = Store::open(self.ws.join("pm.sqlite")).unwrap();
        store
            .put_project(
                &Project {
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
fn init_creates_config_and_a_workspace_with_the_default_states() {
    let sb = Sandbox::new();
    let out = sb.pm(&["init", "--workspace", sb.ws_str()]);
    assert_ok(&out);

    let config = std::fs::read_to_string(sb.config_path()).unwrap();
    let ws = std::fs::canonicalize(&sb.ws).unwrap();
    let parsed: toml::Table = toml::from_str(&config).unwrap();
    assert_eq!(
        Path::new(parsed["workspace"].as_str().unwrap()),
        ws.as_path()
    );

    let workspace = sb.store().workspace().unwrap().unwrap();
    assert_eq!(workspace.prefix, "PM");
    assert!(workspace.gate_labels.is_empty());
    assert!(workspace.model_labels.is_empty());
    let states: Vec<(&str, StateCategory)> = workspace
        .states
        .iter()
        .map(|s| (s.name.as_str(), s.category))
        .collect();
    assert_eq!(
        states,
        [
            ("backlog", StateCategory::Backlog),
            ("todo", StateCategory::Unstarted),
            ("in-progress", StateCategory::Started),
            ("done", StateCategory::Completed),
        ]
    );
}

#[test]
fn init_preset_saltline_reproduces_the_legacy_workspace() {
    let sb = Sandbox::new();
    let out = sb.pm(&[
        "init",
        "--prefix",
        "AGT",
        "--preset",
        "saltline",
        "--workspace",
        sb.ws_str(),
    ]);
    assert_ok(&out);

    let workspace = sb.store().workspace().unwrap().unwrap();
    assert_eq!(workspace.prefix, "AGT");
    assert_eq!(
        workspace.gate_labels,
        std::collections::BTreeSet::from(["manual".to_string()])
    );
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
fn init_preset_saltline_without_prefix_defaults_to_agt() {
    let sb = Sandbox::new();
    let v = json(&sb.pm(&[
        "init",
        "--preset",
        "saltline",
        "--workspace",
        sb.ws_str(),
        "--json",
    ]));
    assert_eq!(v["prefix"], "AGT");
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
    assert!(!sb.config_path().exists());
}

#[test]
fn init_rejects_an_unknown_preset_as_usage() {
    let sb = Sandbox::new();
    assert_code(&sb.pm(&["init", "--preset", "anglepoint"]), 2);
    assert!(!sb.config_path().exists());
}

/// AGT-1396: `pm init --join <ULID>` makes an empty replica — the
/// workspace row with that id, no states, no ops — that only a pull can
/// fill; it refuses a second join and a preset (the hub's config wins).
#[test]
fn init_join_makes_an_empty_replica_of_the_given_workspace() {
    let sb = Sandbox::new();
    let id = Ulid::new();
    let out = sb.pm(&[
        "init",
        "--join",
        &id.to_string(),
        "--workspace",
        sb.ws_str(),
    ]);
    assert_ok(&out);
    assert!(stdout(&out).contains(&format!("joined workspace {id}")));

    let store = sb.store();
    let workspace = store.workspace().unwrap().unwrap();
    assert_eq!(workspace.id, id);
    assert_eq!(workspace.prefix, "PM");
    assert!(workspace.states.is_empty());
    assert_eq!(store.op_count().unwrap(), 0);
    let status = store.sync_status().unwrap();
    assert_eq!((status.outbox, status.cursor, status.seeded), (0, 0, false));
    drop(store);

    // Healthy (nothing derived is missing), but nothing can be filed
    // until the states arrive from the hub.
    assert_ok(&sb.pm(&["doctor", "--workspace", sb.ws_str()]));
    assert_code(
        &sb.pm(&["new", "--title", "too early", "--workspace", sb.ws_str()]),
        1,
    );
    let v = json(&sb.pm(&["hub", "status", "--workspace", sb.ws_str(), "--json"]));
    assert_eq!(v["workspace"]["id"], id.to_string());

    // Already a workspace: init refuses, whichever way.
    assert_code(
        &sb.pm(&[
            "init",
            "--join",
            &id.to_string(),
            "--workspace",
            sb.ws_str(),
        ]),
        1,
    );
    assert_code(&sb.pm(&["init", "--workspace", sb.ws_str()]), 1);

    // `--join` takes its config from the hub, so a preset is a conflict;
    // a bad ULID is a usage error too.
    let other = sb.home.path().join("other");
    let other = other.to_str().unwrap();
    assert_code(
        &sb.pm(&[
            "init",
            "--join",
            &id.to_string(),
            "--preset",
            "saltline",
            "--workspace",
            other,
        ]),
        2,
    );
    assert_code(
        &sb.pm(&["init", "--join", "not-a-ulid", "--workspace", other]),
        2,
    );
    assert!(!Path::new(other).join("pm.sqlite").exists());

    // The prefix is a placeholder until the pull; `--prefix` sets it.
    let out = sb.pm(&[
        "init",
        "--join",
        &id.to_string(),
        "--prefix",
        "AGT",
        "--workspace",
        other,
        "--json",
    ]);
    assert_ok(&out);
    let v = json(&out);
    assert_eq!(v["prefix"], "AGT");
    assert_eq!(v["joined"], id.to_string());
    assert_eq!(v["states"], serde_json::json!([]));
    assert_eq!(v["config_written"], false);
}

/// AC1: `pm init` with no flags at all — the neutral default for outside
/// users, no `--prefix` required.
#[test]
fn init_with_no_flags_uses_the_default_preset() {
    let sb = Sandbox::new();
    let v = json(&sb.pm(&["init", "--workspace", sb.ws_str(), "--json"]));
    assert_eq!(v["prefix"], "PM");
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

/// AGT-1340 AC1: a key `pm set` does not know as a scalar field is not a
/// usage error — it lands under `ext.<key>`, with a warning on stderr.
#[test]
fn set_unknown_field_lands_in_ext_with_a_warning() {
    let sb = Sandbox::initialized();
    assert_ok(&sb.pm(&["new", "--title", "T"]));

    let out = sb.pm(&[
        "set",
        "AGT-1",
        "priority=high",
        "merged_sha=abc123",
        "--json",
    ]);
    assert_ok(&out);
    assert!(
        stderr(&out).contains("warning") && stderr(&out).contains("merged_sha"),
        "stderr: {}",
        stderr(&out)
    );
    let v = json(&out);
    assert_eq!(v["priority"], "high");
    assert_eq!(v["ext"]["merged_sha"], "abc123");

    // An empty value clears the ext key, same as an optional scalar field.
    let out = sb.pm(&["set", "AGT-1", "merged_sha=", "--json"]);
    let v = json(&out);
    assert!(v["ext"].as_object().unwrap().get("merged_sha").is_none());
}

/// AGT-1340 AC1: `pm set` now also covers assignee, linked-github,
/// linked-pr and linear (title/priority/project/repo were AGT-1336).
#[test]
fn set_covers_assignee_and_link_fields() {
    let sb = Sandbox::initialized();
    assert_ok(&sb.pm(&["new", "--title", "T"]));

    let out = sb.pm(&[
        "set",
        "AGT-1",
        "assignee=matt",
        "linked-github=https://github.com/OpenThinkAi/pm/issues/1",
        "linked-pr=https://github.com/OpenThinkAi/pm/pull/2",
        "linear=ANGL-1",
        "--json",
    ]);
    let v = json(&out);
    assert_eq!(v["assignee"], "matt");
    assert_eq!(
        v["linked_github"],
        "https://github.com/OpenThinkAi/pm/issues/1"
    );
    assert_eq!(v["linked_pr"], "https://github.com/OpenThinkAi/pm/pull/2");
    assert_eq!(v["linear"], "ANGL-1");

    // Empty clears, same as project/repo.
    let out = sb.pm(&["set", "AGT-1", "assignee=", "--json"]);
    let v = json(&out);
    assert!(v["assignee"].is_null());
}

// ---------------------------------------------- AGT-1340 AC1: pm label

#[test]
fn label_adds_and_removes() {
    let sb = Sandbox::initialized();
    assert_ok(&sb.pm(&["new", "--title", "T", "--label", "keep"]));

    // `changes` accepts hyphen-prefixed values (`-y`), so global flags like
    // `--json` must precede the subcommand rather than trail the list.
    let v = json(&sb.pm(&["--json", "label", "AGT-1", "+x", "+y"]));
    let mut labels: Vec<&str> = v["labels"]
        .as_array()
        .unwrap()
        .iter()
        .map(|l| l.as_str().unwrap())
        .collect();
    labels.sort();
    assert_eq!(labels, ["keep", "x", "y"]);

    let v = json(&sb.pm(&["--json", "label", "AGT-1", "-x", "+z"]));
    let mut labels: Vec<&str> = v["labels"]
        .as_array()
        .unwrap()
        .iter()
        .map(|l| l.as_str().unwrap())
        .collect();
    labels.sort();
    assert_eq!(labels, ["keep", "y", "z"]);
}

#[test]
fn label_rejects_a_token_without_a_sign_as_usage() {
    let sb = Sandbox::initialized();
    assert_ok(&sb.pm(&["new", "--title", "T"]));
    assert_code(&sb.pm(&["label", "AGT-1", "x"]), 2);
    assert_code(&sb.pm(&["label", "AGT-999", "+x"]), 3);
}

// -------------------------------------------- AGT-1340 AC2: pm comment

#[test]
fn comment_appends_with_actor_and_hlc() {
    let sb = Sandbox::initialized();
    assert_ok(&sb.pm(&["new", "--title", "T"]));
    assert_ok(&sb.pm(&["comment", "AGT-1", "hello there", "--as", "bob"]));

    let comments = sb.store().comments(ulid_of(&sb, "AGT-1")).unwrap();
    assert_eq!(comments.len(), 1);
    assert_eq!(comments[0].body, "hello there");
    assert_eq!(comments[0].author.as_str(), "bob");
}

#[test]
fn comment_file_dash_reads_stdin() {
    let sb = Sandbox::initialized();
    assert_ok(&sb.pm(&["new", "--title", "T"]));
    let out = sb.run_with_stdin(&["comment", "AGT-1", "--file", "-"], "from stdin\n");
    assert_ok(&out);
    let comments = sb.store().comments(ulid_of(&sb, "AGT-1")).unwrap();
    assert_eq!(comments[0].body, "from stdin");
}

#[test]
fn comment_requires_text_or_file_and_rejects_both() {
    let sb = Sandbox::initialized();
    assert_ok(&sb.pm(&["new", "--title", "T"]));
    assert_code(&sb.pm(&["comment", "AGT-1"]), 2);
    assert_code(&sb.pm(&["comment", "AGT-1", "hi", "--file", "-"]), 2);
    assert_code(&sb.pm(&["comment", "AGT-999", "hi"]), 3);
}

// -------------------------- AGT-1340 AC3: pm move / pm done / pm unclaim

#[test]
fn move_validates_the_state_exists() {
    let sb = Sandbox::initialized();
    assert_ok(&sb.pm(&["new", "--title", "T"]));

    let v = json(&sb.pm(&["move", "AGT-1", "in-progress", "--json"]));
    assert_eq!(v["state"], "in-progress");

    assert_code(&sb.pm(&["move", "AGT-1", "not-a-state"]), 3);
    assert_code(&sb.pm(&["move", "AGT-999", "in-progress"]), 3);
}

#[test]
fn done_transitions_and_records_note_and_links() {
    let sb = Sandbox::initialized();
    assert_ok(&sb.pm(&["new", "--title", "T"]));

    let v = json(&sb.pm(&[
        "done",
        "AGT-1",
        "--note",
        "shipped it",
        "--merged-sha",
        "abc123",
        "--pr",
        "https://github.com/OpenThinkAi/pm/pull/9",
        "--json",
    ]));
    assert_eq!(v["state"], "done");
    assert_eq!(v["linked_pr"], "https://github.com/OpenThinkAi/pm/pull/9");
    assert_eq!(v["ext"]["merged_sha"], "abc123");

    let comments = sb.store().comments(ulid_of(&sb, "AGT-1")).unwrap();
    assert_eq!(comments.len(), 1);
    assert_eq!(comments[0].body, "shipped it");
}

#[test]
fn done_without_flags_just_transitions() {
    let sb = Sandbox::initialized();
    assert_ok(&sb.pm(&["new", "--title", "T"]));
    let v = json(&sb.pm(&["done", "AGT-1", "--json"]));
    assert_eq!(v["state"], "done");
    assert!(v["ext"].as_object().unwrap().is_empty());
    assert_code(&sb.pm(&["done", "AGT-999"]), 3);
}

#[test]
fn unclaim_returns_a_started_ticket_to_unstarted_and_clears_assignee() {
    let sb = Sandbox::initialized();
    assert_ok(&sb.pm(&["new", "--title", "T"]));
    assert_ok(&sb.pm(&["move", "AGT-1", "in-progress"]));
    assert_ok(&sb.pm(&["set", "AGT-1", "assignee=matt"]));

    let v = json(&sb.pm(&["unclaim", "AGT-1", "--json"]));
    assert_eq!(v["state"], "triage");
    assert!(v["assignee"].is_null());
}

#[test]
fn unclaim_refuses_a_ticket_that_is_not_started() {
    let sb = Sandbox::initialized();
    assert_ok(&sb.pm(&["new", "--title", "T"]));
    // Still in triage: nothing to unclaim.
    assert_code(&sb.pm(&["unclaim", "AGT-1"]), 1);
    assert_code(&sb.pm(&["unclaim", "AGT-999"]), 3);
}

/// AGT-1379: an already-stranded ticket (unstarted/backlog but still
/// assigned, e.g. from `pm move --keep-assignee` or from before this
/// fix) has no state to un-start — `pm unclaim` clears just the stray
/// assignee, with a stderr note, and leaves the state alone.
#[test]
fn unclaim_on_an_already_unstarted_assigned_ticket_clears_only_the_assignee() {
    let sb = Sandbox::initialized();
    assert_ok(&sb.pm(&["new", "--title", "T"]));
    assert_ok(&sb.pm(&["set", "AGT-1", "assignee=matt"]));

    let out = sb.pm(&["unclaim", "AGT-1", "--json"]);
    assert_ok(&out);
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["state"], "triage");
    assert!(v["assignee"].is_null());
    assert!(
        stderr(&out).contains("already unstarted") && stderr(&out).contains("cleared"),
        "{}",
        stderr(&out)
    );

    // Now unassigned and unstarted: truly nothing to unclaim.
    assert_code(&sb.pm(&["unclaim", "AGT-1"]), 1);
}

// -------------------------------------------------- AGT-1379: pm move
// into an unstarted state must not strand an assigned ticket

#[test]
fn move_into_unstarted_clears_assignee_and_says_so_on_stderr() {
    let sb = Sandbox::initialized();
    assert_ok(&sb.pm(&["new", "--title", "T"]));
    assert_ok(&sb.pm(&["move", "AGT-1", "in-progress"]));
    assert_ok(&sb.pm(&["set", "AGT-1", "assignee=matt"]));

    let out = sb.pm(&["move", "AGT-1", "triage", "--json"]);
    assert_ok(&out);
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["state"], "triage");
    assert!(v["assignee"].is_null());
    assert!(
        stderr(&out).contains("cleared assignee"),
        "{}",
        stderr(&out)
    );
}

/// AC1 also covers the `backlog`-category state the default preset adds
/// (AGT-1373): moving into it clears the assignee just like moving into
/// an `unstarted`-category one.
#[test]
fn move_into_backlog_clears_assignee_on_the_default_preset() {
    let sb = Sandbox::new();
    assert_ok(&sb.pm(&["init", "--workspace", sb.ws_str()]));
    assert_ok(&sb.pm(&["new", "--title", "T"]));
    assert_ok(&sb.pm(&["move", "PM-1", "in-progress"]));
    assert_ok(&sb.pm(&["set", "PM-1", "assignee=matt"]));

    let out = sb.pm(&["move", "PM-1", "backlog", "--json"]);
    assert_ok(&out);
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["state"], "backlog");
    assert!(v["assignee"].is_null());
    assert!(
        stderr(&out).contains("cleared assignee"),
        "{}",
        stderr(&out)
    );
}

#[test]
fn move_keep_assignee_opts_out() {
    let sb = Sandbox::initialized();
    assert_ok(&sb.pm(&["new", "--title", "T"]));
    assert_ok(&sb.pm(&["move", "AGT-1", "in-progress"]));
    assert_ok(&sb.pm(&["set", "AGT-1", "assignee=matt"]));

    let out = sb.pm(&["move", "AGT-1", "triage", "--keep-assignee", "--json"]);
    assert_ok(&out);
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["state"], "triage");
    assert_eq!(v["assignee"], "matt");
    assert!(!stderr(&out).contains("cleared assignee"));
}

#[test]
fn move_without_an_assignee_is_silent_and_move_into_started_keeps_it() {
    let sb = Sandbox::initialized();
    assert_ok(&sb.pm(&["new", "--title", "T"]));

    // No assignee to clear: no stderr noise.
    let out = sb.pm(&["move", "AGT-1", "triage"]);
    assert_ok(&out);
    assert_eq!(stderr(&out), "");

    // Moving into a *started* state never touches the assignee.
    assert_ok(&sb.pm(&["move", "AGT-1", "in-progress"]));
    assert_ok(&sb.pm(&["set", "AGT-1", "assignee=matt"]));
    let out = sb.pm(&["move", "AGT-1", "in-progress", "--json"]);
    assert_ok(&out);
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["assignee"], "matt");
    assert_eq!(stderr(&out), "");
}

/// AC3: a ticket claimed, then moved back to `triage` (the fix keeps it
/// from being stranded assigned-but-unstarted), is claimable again by a
/// different actor.
#[test]
fn claim_move_triage_then_claim_by_another_actor_succeeds() {
    let sb = Sandbox::initialized();
    assert_ok(&sb.pm(&["new", "--title", "T"]));
    assert_code(&sb.run(&["claim", "AGT-1"], &[("PM_ACTOR", "alice")]), 0);

    let out = sb.pm(&["move", "AGT-1", "triage"]);
    assert_ok(&out);

    let out = sb.run(&["claim", "AGT-1", "--json"], &[("PM_ACTOR", "bob")]);
    assert_ok(&out);
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["assignee"], "bob");
    assert_eq!(v["state"], "in-progress");
}

#[test]
fn check_reports_assigned_unstarted_and_ready_explain_names_it_distinctly() {
    let sb = Sandbox::initialized();
    assert_ok(&sb.pm(&["new", "--title", "T"]));
    // Simulate an already-stranded ticket from before this fix: assignee
    // set directly on an unstarted ticket, bypassing `pm move`'s clear.
    assert_ok(&sb.pm(&["set", "AGT-1", "assignee=matt"]));

    let out = sb.pm(&["check", "--json"]);
    assert_code(&out, 1);
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    let findings = v["findings"].as_array().unwrap();
    let found = findings
        .iter()
        .find(|f| f["rule"] == "assigned-unstarted")
        .unwrap_or_else(|| panic!("no assigned-unstarted finding in {findings:?}"));
    assert_eq!(found["tickets"], serde_json::json!(["AGT-1"]));
    assert_eq!(found["assignee"], "matt");
    assert_eq!(found["state"], "triage");

    // `pm ready --explain` names the same ticket distinctly from a
    // `started` exclusion (which never mentions "assigned").
    let v = json(&sb.pm(&["ready", "--json"]));
    let excluded = v["excluded"].as_array().unwrap();
    let reason = excluded
        .iter()
        .find(|e| e["id"] == "AGT-1")
        .unwrap_or_else(|| panic!("AGT-1 not excluded in {excluded:?}"));
    assert_eq!(reason["reason"], "assigned");
    assert!(
        reason["message"].as_str().unwrap().contains("assigned"),
        "{reason:?}"
    );

    assert_ok(&sb.pm(&["new", "--title", "started"]));
    assert_ok(&sb.pm(&["move", "AGT-2", "in-progress"]));
    let v = json(&sb.pm(&["ready", "--json"]));
    let excluded = v["excluded"].as_array().unwrap();
    let started_reason = excluded
        .iter()
        .find(|e| e["id"] == "AGT-2")
        .unwrap_or_else(|| panic!("AGT-2 not excluded in {excluded:?}"));
    assert_eq!(started_reason["reason"], "state");
    assert_ne!(started_reason["reason"], reason["reason"]);
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

// ------------------------------------------------------------- AGT-1346

/// The array's strings, sorted — `blocked_by`'s SQL order is by ULID, not
/// batch/file order, so tests compare it as a set.
fn sorted_strings(v: &Value) -> Vec<String> {
    let mut xs: Vec<String> = v
        .as_array()
        .unwrap()
        .iter()
        .map(|x| x.as_str().unwrap().to_string())
        .collect();
    xs.sort();
    xs
}

// AC1: --from-file parses a vault-format ticket file into a create op set.

#[test]
fn from_file_parses_a_real_shaped_vault_ticket() {
    let sb = Sandbox::initialized();
    let fixture = sb.fixture(
        "AGT-1003.md",
        r#"---
id: AGT-1003
title: "Decide the feature-flag contract before building it"
state: triage
created: 2026-08-15
updated: 2026-08-15
project: pm
repo: MicroMediaSites/bloom-cms
blocked-by: []
linked-github:
linked-pr:
priority: high
labels: [x, y]
source: { type: manual, url: "", id: "", fetched-at: "" }
team: engineering
---

## Problem Statement

The feature-flags project has four open design questions that change what
every other ticket builds.

## Acceptance Criteria

1. Decided, with a one-line reason.
"#,
    );
    let v = json(&sb.pm(&["new", "--from-file", fixture.to_str().unwrap(), "--json"]));
    assert_eq!(v["id"], "AGT-1");
    assert_eq!(
        v["title"],
        "Decide the feature-flag contract before building it"
    );
    assert_eq!(v["state"], "triage");
    assert_eq!(v["priority"], "high");
    assert_eq!(v["project"], "pm");
    assert_eq!(v["repo"], "MicroMediaSites/bloom-cms");
    assert_eq!(v["labels"], serde_json::json!(["x", "y"]));
    assert_eq!(v["linked_github"], Value::Null);
    assert_eq!(v["source"]["type"], "manual");
    assert_eq!(v["blocked_by"], serde_json::json!([]));
    // Unknown frontmatter keys land in ext (AC1); id/state/created/updated
    // are known keys pm computes itself and are dropped, not preserved.
    assert_eq!(v["ext"], serde_json::json!({"team": "engineering"}));
    let description = v["description"].as_str().unwrap();
    assert!(
        description.starts_with("## Problem Statement"),
        "{description}"
    );
    assert!(description.contains("Acceptance Criteria"), "{description}");
}

#[test]
fn from_file_requires_a_title_and_rejects_missing_fences() {
    let sb = Sandbox::initialized();
    let no_title = sb.fixture("no-title.md", "---\nproject: pm\n---\nbody\n");
    assert_code(
        &sb.pm(&["new", "--from-file", no_title.to_str().unwrap()]),
        2,
    );

    let no_fences = sb.fixture("no-fences.md", "title: T\n");
    assert_code(
        &sb.pm(&["new", "--from-file", no_fences.to_str().unwrap()]),
        2,
    );
    assert!(sb.store().tickets(&Default::default()).unwrap().is_empty());
}

#[test]
fn from_file_cannot_be_combined_with_batch_or_other_new_flags() {
    let sb = Sandbox::initialized();
    let fixture = sb.fixture("x.md", "---\ntitle: X\nproject: pm\n---\n");
    let empty_batch = sb.fixture("empty.yaml", "tickets: []\n");
    assert_code(
        &sb.pm(&[
            "new",
            "--from-file",
            fixture.to_str().unwrap(),
            "--batch",
            empty_batch.to_str().unwrap(),
        ]),
        2,
    );
    assert_code(
        &sb.pm(&[
            "new",
            "--from-file",
            fixture.to_str().unwrap(),
            "--title",
            "nope",
        ]),
        2,
    );
}

// AC2-4: --batch creates N tickets with symbolic @ref blockers, atomically.

#[test]
fn batch_creates_tickets_with_symbolic_refs_in_one_transaction() {
    let sb = Sandbox::initialized();
    let fixture = sb.fixture(
        "batch.yaml",
        r#"
tickets:
  - ref: core
    title: "Core domain types"
    project: pm
    priority: high
    labels: [x]
  - ref: schema
    title: "SQLite schema"
    project: pm
    blocked-by: ["@core"]
  - title: "CLI verbs"
    project: pm
    blocked-by: ["@core", "@schema"]
    linked-github: "https://github.com/OpenThinkAi/pm/pull/1"
    description: |
      Wires the ops to clap.
"#,
    );
    let v = json(&sb.pm(&["new", "--batch", fixture.to_str().unwrap(), "--json"]));
    assert_eq!(v["schema"], 1);
    assert_eq!(
        v["refs"],
        serde_json::json!({"@core": "AGT-1", "@schema": "AGT-2"})
    );
    let tickets = v["tickets"].as_array().unwrap();
    assert_eq!(tickets.len(), 3);
    assert_eq!(tickets[0]["id"], "AGT-1");
    assert_eq!(tickets[0]["priority"], "high");
    assert_eq!(tickets[0]["labels"], serde_json::json!(["x"]));
    assert_eq!(tickets[1]["id"], "AGT-2");
    assert_eq!(sorted_strings(&tickets[1]["blocked_by"]), ["AGT-1"]);
    assert_eq!(tickets[2]["id"], "AGT-3");
    assert_eq!(
        sorted_strings(&tickets[2]["blocked_by"]),
        ["AGT-1", "AGT-2"]
    );
    assert_eq!(
        tickets[2]["linked_github"],
        "https://github.com/OpenThinkAi/pm/pull/1"
    );
    assert!(
        tickets[2]["description"]
            .as_str()
            .unwrap()
            .contains("Wires the ops")
    );

    // pm show --json reflects it too (AC5's "reflects all", extended to
    // the batch path).
    let shown = json(&sb.pm(&["show", "AGT-3", "--json"]));
    assert_eq!(sorted_strings(&shown["blocked_by"]), ["AGT-1", "AGT-2"]);
}

#[test]
fn batch_with_an_unresolvable_ref_fails_and_creates_nothing() {
    let sb = Sandbox::initialized();
    let fixture = sb.fixture(
        "bad-ref.yaml",
        r#"
tickets:
  - ref: core
    title: "Core"
    project: pm
  - title: "Depends on a typo'd ref"
    project: pm
    blocked-by: ["@cor"]
"#,
    );
    let out = sb.pm(&["new", "--batch", fixture.to_str().unwrap()]);
    assert_code(&out, 2);
    assert!(stderr(&out).contains("@cor"), "{}", stderr(&out));
    assert!(
        sb.store().tickets(&Default::default()).unwrap().is_empty(),
        "a bad ref must create nothing"
    );
}

#[test]
fn batch_with_a_nonexistent_plain_blocker_id_fails_with_exit_2() {
    let sb = Sandbox::initialized();
    let fixture = sb.fixture(
        "bad-id.yaml",
        r#"
tickets:
  - title: "Blocked by nothing real"
    project: pm
    blocked-by: ["AGT-999"]
"#,
    );
    let out = sb.pm(&["new", "--batch", fixture.to_str().unwrap()]);
    assert_code(&out, 2);
    assert!(stderr(&out).contains("AGT-999"), "{}", stderr(&out));
    assert!(sb.store().tickets(&Default::default()).unwrap().is_empty());
}

#[test]
fn batch_rejects_duplicate_refs_and_missing_projects_without_creating_anything() {
    let sb = Sandbox::initialized();
    let dup = sb.fixture(
        "dup.yaml",
        "tickets:\n  - ref: core\n    title: A\n    project: pm\n  - ref: core\n    title: B\n    project: pm\n",
    );
    let out = sb.pm(&["new", "--batch", dup.to_str().unwrap()]);
    assert_code(&out, 2);
    assert!(stderr(&out).contains("core"), "{}", stderr(&out));

    let missing_project = sb.fixture(
        "missing-project.yaml",
        "tickets:\n  - title: A\n    project: pm\n  - title: B\n    project: nope\n",
    );
    let out = sb.pm(&["new", "--batch", missing_project.to_str().unwrap()]);
    assert_code(&out, 3);
    assert!(sb.store().tickets(&Default::default()).unwrap().is_empty());
}

#[test]
fn batch_with_no_tickets_is_a_usage_error() {
    let sb = Sandbox::initialized();
    let empty = sb.fixture("empty.yaml", "tickets: []\n");
    assert_code(&sb.pm(&["new", "--batch", empty.to_str().unwrap()]), 2);
}

// AC5: --description/--description-file/--blocked-by/--linked-github/--source

#[test]
fn description_file_dash_reads_stdin() {
    let sb = Sandbox::initialized();
    let out = sb.run_with_stdin(
        &["new", "--title", "Stdin desc", "--description-file", "-"],
        "Body from stdin.\n",
    );
    assert_ok(&out);
    let id = stdout(&out).trim().to_string();
    let v = json(&sb.pm(&["show", &id, "--json"]));
    assert_eq!(v["description"], "Body from stdin.");
}

#[test]
fn description_file_reads_a_path() {
    let sb = Sandbox::initialized();
    let path = sb.fixture("desc.md", "# Heading\n\nText from a file.\n");
    let out = sb.pm(&[
        "new",
        "--title",
        "From file",
        "--description-file",
        path.to_str().unwrap(),
    ]);
    assert_ok(&out);
    let v = json(&sb.pm(&["show", "AGT-1", "--json"]));
    assert_eq!(v["description"], "# Heading\n\nText from a file.");
}

#[test]
fn description_and_description_file_are_mutually_exclusive() {
    let sb = Sandbox::initialized();
    assert_code(
        &sb.pm(&[
            "new",
            "--title",
            "T",
            "--description",
            "a",
            "--description-file",
            "-",
        ]),
        2,
    );
}

#[test]
fn single_ticket_flags_cover_blocked_by_linked_github_and_source() {
    let sb = Sandbox::initialized();
    assert_ok(&sb.pm(&["new", "--title", "Blocker"]));
    let out = sb.pm(&[
        "new",
        "--title",
        "Blocked",
        "--blocked-by",
        "AGT-1",
        "--linked-github",
        "https://github.com/OpenThinkAi/pm/issues/9",
        "--source",
        "type=github,url=https://github.com/x,id=9",
        "--description",
        "Inline desc",
        "--json",
    ]);
    let v = json(&out);
    assert_eq!(v["id"], "AGT-2");
    assert_eq!(sorted_strings(&v["blocked_by"]), ["AGT-1"]);
    assert_eq!(
        v["linked_github"],
        "https://github.com/OpenThinkAi/pm/issues/9"
    );
    assert_eq!(v["source"]["type"], "github");
    assert_eq!(v["source"]["url"], "https://github.com/x");
    assert_eq!(v["source"]["id"], "9");
    assert_eq!(v["description"], "Inline desc");

    // pm show --json reflects it all too.
    let shown = json(&sb.pm(&["show", "AGT-2", "--json"]));
    assert_eq!(sorted_strings(&shown["blocked_by"]), ["AGT-1"]);
    assert_eq!(shown["source"]["type"], "github");
}

#[test]
fn blocked_by_names_an_unknown_ticket_as_not_found() {
    let sb = Sandbox::initialized();
    let out = sb.pm(&["new", "--title", "T", "--blocked-by", "AGT-99"]);
    assert_code(&out, 3);
    assert!(sb.store().tickets(&Default::default()).unwrap().is_empty());
}

#[test]
fn source_flag_requires_a_type() {
    let sb = Sandbox::initialized();
    assert_code(
        &sb.pm(&["new", "--title", "T", "--source", "url=https://x"]),
        2,
    );
}

// AGT-1383: `pm relate` edits blockers on existing tickets.
fn relate_sandbox() -> Sandbox {
    let sb = Sandbox::initialized();
    for title in ["A", "B", "C"] {
        assert_ok(&sb.pm(&["new", "--title", title]));
    }
    sb
}

#[test]
fn relate_adds_and_removes_blockers() {
    let sb = relate_sandbox();
    let v = json(&sb.pm(&["relate", "AGT-3", "--blocked-by", "AGT-1,AGT-2", "--json"]));
    assert_eq!(v["id"], "AGT-3");
    assert_eq!(sorted_strings(&v["blocked_by"]), ["AGT-1", "AGT-2"]);

    // Adding an existing blocker again is a no-op.
    let again = json(&sb.pm(&["relate", "AGT-3", "--blocked-by", "AGT-1", "--json"]));
    assert_eq!(sorted_strings(&again["blocked_by"]), ["AGT-1", "AGT-2"]);

    let v = json(&sb.pm(&["relate", "AGT-3", "--unblock", "AGT-1", "--json"]));
    assert_eq!(sorted_strings(&v["blocked_by"]), ["AGT-2"]);
    let shown = json(&sb.pm(&["show", "AGT-3", "--json"]));
    assert_eq!(sorted_strings(&shown["blocked_by"]), ["AGT-2"]);

    // Unblocking something that is not a blocker is a no-op too.
    assert_ok(&sb.pm(&["relate", "AGT-3", "--unblock", "AGT-1"]));

    // The op log records relation.add / relation.remove.
    let log = json(&sb.pm(&["log", "AGT-3", "--json"]));
    let text = log.to_string();
    assert!(text.contains("relation.add"), "{text}");
    assert!(text.contains("relation.remove"), "{text}");
}

#[test]
fn relate_can_add_and_remove_in_one_call() {
    let sb = relate_sandbox();
    assert_ok(&sb.pm(&["relate", "AGT-3", "--blocked-by", "AGT-1"]));
    let v = json(&sb.pm(&[
        "relate",
        "AGT-3",
        "--blocked-by",
        "AGT-2",
        "--unblock",
        "AGT-1",
        "--json",
    ]));
    assert_eq!(sorted_strings(&v["blocked_by"]), ["AGT-2"]);
}

#[test]
fn relate_unknown_ids_exit_3_and_write_nothing() {
    let sb = relate_sandbox();
    assert_code(&sb.pm(&["relate", "AGT-99", "--blocked-by", "AGT-1"]), 3);
    assert_code(
        &sb.pm(&["relate", "AGT-3", "--blocked-by", "AGT-1,AGT-99"]),
        3,
    );
    assert_code(&sb.pm(&["relate", "AGT-3", "--unblock", "AGT-99"]), 3);
    let v = json(&sb.pm(&["show", "AGT-3", "--json"]));
    assert_eq!(v["blocked_by"], serde_json::json!([]));
}

#[test]
fn relate_blocks_and_unblocks_set_outgoing_edges() {
    let sb = relate_sandbox();
    // `AGT-1 blocks AGT-2,AGT-3`; --json still returns the <id> ticket.
    let v = json(&sb.pm(&["relate", "AGT-1", "--blocks", "AGT-2,AGT-3", "--json"]));
    assert_eq!(v["id"], "AGT-1");
    assert_eq!(v["blocked_by"], serde_json::json!([]));
    for id in ["AGT-2", "AGT-3"] {
        let t = json(&sb.pm(&["show", id, "--json"]));
        assert_eq!(sorted_strings(&t["blocked_by"]), ["AGT-1"], "{id}");
    }

    // Duplicate edge: idempotent.
    assert_ok(&sb.pm(&["relate", "AGT-1", "--blocks", "AGT-2"]));
    let t = json(&sb.pm(&["show", "AGT-2", "--json"]));
    assert_eq!(sorted_strings(&t["blocked_by"]), ["AGT-1"]);

    assert_ok(&sb.pm(&["relate", "AGT-1", "--unblocks", "AGT-2"]));
    let t = json(&sb.pm(&["show", "AGT-2", "--json"]));
    assert_eq!(t["blocked_by"], serde_json::json!([]));
    let t = json(&sb.pm(&["show", "AGT-3", "--json"]));
    assert_eq!(sorted_strings(&t["blocked_by"]), ["AGT-1"]);
    // Removing an absent edge is a no-op.
    assert_ok(&sb.pm(&["relate", "AGT-1", "--unblocks", "AGT-2"]));

    // An edge created from the other side is removable from this one.
    assert_ok(&sb.pm(&["relate", "AGT-2", "--blocked-by", "AGT-3"]));
    assert_ok(&sb.pm(&["relate", "AGT-3", "--unblocks", "AGT-2"]));
    let t = json(&sb.pm(&["show", "AGT-2", "--json"]));
    assert_eq!(t["blocked_by"], serde_json::json!([]));
}

#[test]
fn relate_mixes_incoming_and_outgoing_flags_in_one_call() {
    let sb = relate_sandbox();
    assert_ok(&sb.pm(&["relate", "AGT-2", "--blocks", "AGT-3"]));
    // AGT-2: newly blocked by AGT-1, stops blocking AGT-3, starts blocking nothing else.
    let v = json(&sb.pm(&[
        "relate",
        "AGT-2",
        "--blocked-by",
        "AGT-1",
        "--unblocks",
        "AGT-3",
        "--json",
    ]));
    assert_eq!(sorted_strings(&v["blocked_by"]), ["AGT-1"]);
    let t = json(&sb.pm(&["show", "AGT-3", "--json"]));
    assert_eq!(t["blocked_by"], serde_json::json!([]));
    // One batch: the log for the single call is not split across ticket ops
    // that could land partially; unknown ids in any flag write nothing.
    assert_code(
        &sb.pm(&[
            "relate",
            "AGT-2",
            "--blocks",
            "AGT-3",
            "--unblocks",
            "AGT-99",
        ]),
        3,
    );
    let t = json(&sb.pm(&["show", "AGT-3", "--json"]));
    assert_eq!(t["blocked_by"], serde_json::json!([]));
}

#[test]
fn relate_blocks_refuses_self_cycles_and_duplicate_edges_with_exit_2() {
    let sb = relate_sandbox();
    assert_code(&sb.pm(&["relate", "AGT-1", "--blocks", "AGT-1"]), 2);
    assert_code(&sb.pm(&["relate", "AGT-1", "--blocks", "AGT-99"]), 3);
    // The same edge named twice, on both sides of the add/remove split.
    assert_code(
        &sb.pm(&[
            "relate",
            "AGT-1",
            "--blocks",
            "AGT-2",
            "--unblocks",
            "AGT-2",
        ]),
        2,
    );
    // ... including via the mirrored incoming flags.
    assert_code(
        &sb.pm(&[
            "relate",
            "AGT-2",
            "--blocked-by",
            "AGT-1",
            "--unblock",
            "AGT-1",
        ]),
        2,
    );
    assert_code(
        &sb.pm(&[
            "relate",
            "AGT-1",
            "--unblocks",
            "AGT-2",
            "--blocks",
            "AGT-2",
        ]),
        2,
    );
    let v = json(&sb.pm(&["show", "AGT-2", "--json"]));
    assert_eq!(v["blocked_by"], serde_json::json!([]));

    assert_ok(&sb.pm(&["relate", "AGT-1", "--blocks", "AGT-2"]));
    // AGT-1 blocks AGT-2 already; AGT-2 blocks AGT-3 -> AGT-3 blocking AGT-1 cycles.
    assert_ok(&sb.pm(&["relate", "AGT-2", "--blocks", "AGT-3"]));
    let out = sb.pm(&["relate", "AGT-3", "--blocks", "AGT-1"]);
    assert_code(&out, 2);
    assert!(String::from_utf8_lossy(&out.stderr).contains("cycle"));
    assert_code(&sb.pm(&["relate", "AGT-2", "--blocks", "AGT-1"]), 2);
    let v = json(&sb.pm(&["show", "AGT-1", "--json"]));
    assert_eq!(v["blocked_by"], serde_json::json!([]));
    // Removing the middle link in the same call makes the add legal.
    assert_ok(&sb.pm(&["relate", "AGT-3", "--blocks", "AGT-1", "--unblock", "AGT-2"]));
}

#[test]
fn relate_refuses_self_block_cycles_and_empty_calls_with_exit_2() {
    let sb = relate_sandbox();
    assert_code(&sb.pm(&["relate", "AGT-1", "--blocked-by", "AGT-1"]), 2);
    assert_code(&sb.pm(&["relate", "AGT-1"]), 2);
    assert_code(
        &sb.pm(&[
            "relate",
            "AGT-1",
            "--blocked-by",
            "AGT-2",
            "--unblock",
            "AGT-2",
        ]),
        2,
    );

    // AGT-2 blocked by AGT-1, AGT-3 blocked by AGT-2.
    assert_ok(&sb.pm(&["relate", "AGT-2", "--blocked-by", "AGT-1"]));
    assert_ok(&sb.pm(&["relate", "AGT-3", "--blocked-by", "AGT-2"]));
    // Direct and transitive cycles are refused, and write nothing.
    let out = sb.pm(&["relate", "AGT-1", "--blocked-by", "AGT-2"]);
    assert_code(&out, 2);
    assert!(String::from_utf8_lossy(&out.stderr).contains("cycle"));
    assert_code(&sb.pm(&["relate", "AGT-1", "--blocked-by", "AGT-3"]), 2);
    let v = json(&sb.pm(&["show", "AGT-1", "--json"]));
    assert_eq!(v["blocked_by"], serde_json::json!([]));

    // Removing the middle link in the same call makes the add legal.
    assert_ok(&sb.pm(&["relate", "AGT-3", "--unblock", "AGT-2"]));
    assert_ok(&sb.pm(&["relate", "AGT-1", "--blocked-by", "AGT-3"]));
}

// ------------------------------------------- AGT-1430: pm show comments

fn show_json(sb: &Sandbox, args: &[&str]) -> serde_json::Value {
    let out = sb.pm(args);
    assert_ok(&out);
    serde_json::from_slice(&out.stdout).unwrap()
}

#[test]
fn show_without_comments_is_unchanged_and_json_has_empty_array() {
    let sb = Sandbox::initialized();
    assert_ok(&sb.pm(&["new", "--title", "T", "--description", "the body"]));
    let out = sb.pm(&["show", "AGT-1"]);
    assert_ok(&out);
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(!text.contains("Comments"), "{text}");
    assert!(text.trim_end().ends_with("the body"), "{text}");
    let v = show_json(&sb, &["show", "AGT-1", "--json"]);
    assert_eq!(v["comments"], serde_json::json!([]));
}

#[test]
fn show_lists_one_comment_after_the_description() {
    let sb = Sandbox::initialized();
    assert_ok(&sb.pm(&["new", "--title", "T", "--description", "the body"]));
    assert_ok(&sb.pm(&["comment", "AGT-1", "hello there", "--as", "bob"]));
    let out = sb.pm(&["show", "AGT-1"]);
    assert_ok(&out);
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    let body_at = text.find("the body").unwrap();
    let head_at = text.find("Comments (1):").expect(&text);
    assert!(body_at < head_at, "{text}");
    assert!(text.contains(" — bob\n  hello there"), "{text}");
    let v = show_json(&sb, &["show", "AGT-1", "--json"]);
    let c = &v["comments"];
    assert_eq!(c.as_array().unwrap().len(), 1);
    assert_eq!(c[0]["author"], "bob");
    assert_eq!(c[0]["body"], "hello there");
    let at = c[0]["at"].as_str().unwrap();
    assert!(at.len() == 10 && at.as_bytes()[4] == b'-', "{at}");
}

#[test]
fn show_lists_several_comments_oldest_first_with_multiline_bodies() {
    let sb = Sandbox::initialized();
    assert_ok(&sb.pm(&["new", "--title", "T"]));
    assert_ok(&sb.pm(&["comment", "AGT-1", "first", "--as", "ann"]));
    let out = sb.run_with_stdin(
        &["comment", "AGT-1", "--file", "-", "--as", "bob"],
        "line one\n\nline three\n",
    );
    assert_ok(&out);
    assert_ok(&sb.pm(&["comment", "AGT-1", "third", "--as", "cy"]));

    let out = sb.pm(&["show", "AGT-1"]);
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(text.contains("Comments (3):"), "{text}");
    assert!(
        text.contains(" — bob\n  line one\n\n  line three\n"),
        "{text}"
    );
    let (a, b, c) = (
        text.find("ann").unwrap(),
        text.find("bob").unwrap(),
        text.find("cy\n").unwrap(),
    );
    assert!(a < b && b < c, "{text}");

    let v = show_json(&sb, &["show", "AGT-1", "--json"]);
    let authors: Vec<&str> = v["comments"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["author"].as_str().unwrap())
        .collect();
    assert_eq!(authors, ["ann", "bob", "cy"]);
    assert_eq!(v["comments"][1]["body"], "line one\n\nline three");
}

#[test]
fn list_json_omits_comments() {
    let sb = Sandbox::initialized();
    assert_ok(&sb.pm(&["new", "--title", "T"]));
    assert_ok(&sb.pm(&["comment", "AGT-1", "hi"]));
    let v = show_json(&sb, &["list", "--json"]);
    assert!(v[0].get("comments").is_none(), "{v}");
}
