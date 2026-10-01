//! `pm project new/show/list/edit/doc/delete` driven through the built
//! binary (AGT-1344). Sandbox as in `tests/cli.rs`: temp HOME, cleared
//! env, no stdin — a command that tried to prompt would read EOF rather
//! than hang. `pm project edit` launches `$EDITOR` the same way `pm edit`
//! does (`crate::edit`, AGT-1345), so PATH is kept (needed for the `sh -c`
//! it shells through) the way `tests/edit.rs`'s sandbox keeps it.

use std::path::PathBuf;
use std::process::{Command, Output, Stdio};

use pm_store::Store;
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

    fn initialized() -> Self {
        let sb = Sandbox::new();
        assert_ok(&sb.run(
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
        ));
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
            // `pm project edit` shells out to `sh -c` to launch $EDITOR
            // (crate::edit::run_editor); keep PATH so `sh` resolves.
            .env("PATH", std::env::var("PATH").unwrap_or_default())
            .stdin(Stdio::null());
        for (k, v) in extra {
            cmd.env(k, v);
        }
        cmd.output().unwrap()
    }

    fn pm(&self, args: &[&str]) -> Output {
        self.run(args, &[])
    }

    fn store(&self) -> Store {
        Store::open(self.ws.join("pm.sqlite")).unwrap()
    }

    /// An executable script standing in for `$EDITOR`: overwrites whatever
    /// file it's given with `content`, non-interactively (README
    /// §Constraints: no command may prompt without a TTY, and this test
    /// harness's stdin is always closed).
    fn editor_script(&self, name: &str, content: &str) -> PathBuf {
        let path = self.home.path().join(name);
        std::fs::write(
            &path,
            format!("#!/bin/sh\ncat > \"$1\" <<'PM_EOF'\n{content}\nPM_EOF\n"),
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        path
    }

    fn fixture(&self, name: &str, contents: &str) -> PathBuf {
        let path = self.home.path().join(name);
        std::fs::write(&path, contents).unwrap();
        path
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

// -------------------------------------------------------------------- AC1

#[test]
fn new_creates_a_project_show_and_list_reflect_it() {
    let sb = Sandbox::initialized();
    let out = sb.pm(&[
        "project",
        "new",
        "pm",
        "--title",
        "pm",
        "--repo",
        "OpenThinkAi/pm",
    ]);
    assert_ok(&out);

    let v = json(&sb.pm(&["project", "show", "pm", "--json"]));
    assert_eq!(v["schema"], 1);
    assert_eq!(v["id"], "pm");
    assert_eq!(v["title"], "pm");
    assert_eq!(v["status"], "in-progress");
    assert_eq!(v["repos"], serde_json::json!(["OpenThinkAi/pm"]));
    assert_eq!(v["doc"], "");
    assert_eq!(v["documents"], serde_json::json!({}));

    let human = stdout(&sb.pm(&["project", "show", "pm"]));
    assert!(human.starts_with("pm  pm\n"), "{human}");
    assert!(human.contains("status:  in-progress\n"), "{human}");

    let listed = json(&sb.pm(&["project", "list", "--json"]));
    let ids: Vec<&str> = listed["projects"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, ["pm"]);
}

#[test]
fn new_rejects_a_bad_id_or_a_missing_title() {
    let sb = Sandbox::initialized();
    assert_code(&sb.pm(&["project", "new", "PM", "--title", "x"]), 2);
    assert_code(&sb.pm(&["project", "new", "-pm", "--title", "x"]), 2);
    assert_code(&sb.pm(&["project", "new", "pm-", "--title", "x"]), 2);
    assert_code(&sb.pm(&["project", "new", "pm"]), 2); // --title is required
    assert!(sb.store().projects().unwrap().is_empty());
}

#[test]
fn new_rejects_a_duplicate_id_and_checks_the_parent_exists() {
    let sb = Sandbox::initialized();
    assert_ok(&sb.pm(&["project", "new", "pm", "--title", "pm"]));
    let dup = sb.pm(&["project", "new", "pm", "--title", "again"]);
    assert_code(&dup, 1);
    assert!(stderr(&dup).contains("already exists"), "{}", stderr(&dup));

    let out = sb.pm(&[
        "project", "new", "child", "--title", "c", "--parent", "nope",
    ]);
    assert_code(&out, 3);

    assert_ok(&sb.pm(&["project", "new", "child", "--title", "c", "--parent", "pm"]));
    let v = json(&sb.pm(&["project", "show", "child", "--json"]));
    assert_eq!(v["parent"], "pm");
}

/// AGT-1488: `--kind initiative` creates an initiative, no `--kind` a
/// plain project; `list --kind` filters; an initiative cannot take
/// `--parent` (exit 2, nothing created); the kind survives a rebuild and
/// names itself in `pm log`.
#[test]
fn kind_is_set_at_creation_filtered_by_list_and_initiatives_have_no_parent() {
    let sb = Sandbox::initialized();
    let v = json(&sb.pm(&[
        "project",
        "new",
        "q4",
        "--title",
        "Q4",
        "--kind",
        "initiative",
        "--json",
    ]));
    assert_eq!(v["kind"], "initiative");
    let v = json(&sb.pm(&["project", "new", "pm", "--title", "pm", "--json"]));
    assert_eq!(v["kind"], "project");
    assert_ok(&sb.pm(&[
        "project", "new", "pm-app", "--title", "app", "--kind", "project", "--parent", "q4",
    ]));

    let ids = |args: &[&str]| -> Vec<String> {
        json(&sb.pm(args))["projects"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| p["id"].as_str().unwrap().to_string())
            .collect()
    };
    assert_eq!(
        ids(&["project", "list", "--kind", "initiative", "--json"]),
        ["q4"]
    );
    assert_eq!(
        ids(&["project", "list", "--kind", "project", "--json"]),
        ["pm", "pm-app"]
    );
    assert_eq!(ids(&["project", "list", "--json"]).len(), 3);
    assert_code(&sb.pm(&["project", "list", "--kind", "epic"]), 2);

    let out = sb.pm(&[
        "project",
        "new",
        "z",
        "--title",
        "z",
        "--kind",
        "initiative",
        "--parent",
        "pm",
    ]);
    assert_code(&out, 2);
    assert!(stderr(&out).contains("initiative"), "{}", stderr(&out));
    assert_code(
        &sb.pm(&["project", "new", "z", "--title", "z", "--kind", "epic"]),
        2,
    );
    assert!(sb.store().project("z").unwrap().is_none());

    let human = stdout(&sb.pm(&["project", "show", "q4"]));
    assert!(human.contains("kind:    initiative\n"), "{human}");
    let log = json(&sb.pm(&["log", "--json"]));
    assert!(
        log.as_array().unwrap().iter().any(|o| o["summary"]
            .as_str()
            .unwrap()
            .contains("created initiative 'q4'")),
        "{log}"
    );

    let v = json(&sb.pm(&["doctor", "--rebuild", "--json"]));
    assert_eq!(v["healthy"], true);
    assert_eq!(v["rebuilt"]["tables"], serde_json::json!([]));
    let v = json(&sb.pm(&["project", "show", "q4", "--json"]));
    assert_eq!(v["kind"], "initiative");
}

// -------------------------------------------------------------------- AC2

/// `run_editor` (crate::edit, AGT-1345) is `$VISUAL`, then `$EDITOR`, then
/// `vi` — pm itself never prompts, so there is no separate "$EDITOR unset"
/// usage error to test here (mirroring `tests/edit.rs`, which does not
/// test the `vi` fallback either: it depends on what's installed). What
/// *is* deterministic, and the same abort path a missing $EDITOR that
/// somehow launched something broken would hit, is a non-zero exit.
#[test]
fn edit_aborts_when_the_editor_exits_non_zero() {
    let sb = Sandbox::initialized();
    assert_ok(&sb.pm(&["project", "new", "pm", "--title", "pm"]));
    let editor = sb.home.path().join("failing-editor.sh");
    std::fs::write(&editor, "#!/bin/sh\nexit 3\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&editor, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let out = sb.run(
        &["project", "edit", "pm"],
        &[("EDITOR", editor.to_str().unwrap())],
    );
    assert_code(&out, 1);
    assert!(stderr(&out).contains("non-zero"), "{}", stderr(&out));
    assert!(sb.store().design_doc_id("pm").unwrap().is_some());
    let v = json(&sb.pm(&["project", "show", "pm", "--json"]));
    assert_eq!(v["doc"], "", "a non-zero exit commits nothing");
}

#[test]
fn edit_twice_commits_body_edit_ops_that_show_reflects() {
    let sb = Sandbox::initialized();
    assert_ok(&sb.pm(&["project", "new", "pm", "--title", "pm"]));

    let first = sb.editor_script("editor1.sh", "# pm\n\nFirst draft.");
    assert_ok(&sb.run(
        &["project", "edit", "pm"],
        &[("EDITOR", first.to_str().unwrap())],
    ));
    let v = json(&sb.pm(&["project", "show", "pm", "--json"]));
    assert_eq!(v["doc"], "# pm\n\nFirst draft.\n");

    let second = sb.editor_script("editor2.sh", "# pm\n\nSecond draft.");
    assert_ok(&sb.run(
        &["project", "edit", "pm"],
        &[("EDITOR", second.to_str().unwrap())],
    ));
    let v = json(&sb.pm(&["project", "show", "pm", "--json"]));
    assert_eq!(v["doc"], "# pm\n\nSecond draft.\n");

    // Two body.edit ops landed in the log, targeting the design doc's
    // doc_id — never the ticket-shaped id namespace, and never each
    // other's stale content (this is a real CRDT edit, not an overwrite).
    let store = sb.store();
    let doc_id = store.design_doc_id("pm").unwrap().unwrap();
    let ops = store.ops(doc_id).unwrap();
    let kinds: Vec<&str> = ops.iter().map(|op| op.kind()).collect();
    assert_eq!(kinds, ["body.edit", "body.edit"]);

    // A no-op edit (editor writes the same text back) commits nothing new.
    let noop = sb.editor_script("editor3.sh", "# pm\n\nSecond draft.");
    assert_ok(&sb.run(
        &["project", "edit", "pm"],
        &[("EDITOR", noop.to_str().unwrap())],
    ));
    assert_eq!(sb.store().ops(doc_id).unwrap().len(), 2);
}

#[test]
fn edit_rejects_an_unknown_project() {
    let sb = Sandbox::initialized();
    let editor = sb.editor_script("editor.sh", "text");
    let out = sb.run(
        &["project", "edit", "nope"],
        &[("EDITOR", editor.to_str().unwrap())],
    );
    assert_code(&out, 3);
}

// -------------------------------------------------------------------- AC3

#[test]
fn doc_add_creates_a_named_document_and_show_doc_prints_it() {
    let sb = Sandbox::initialized();
    assert_ok(&sb.pm(&["project", "new", "pm", "--title", "pm"]));
    let fixture = sb.fixture("spike.md", "# Spike notes\n\nLoro wins.\n");

    let out = sb.pm(&[
        "project",
        "doc",
        "add",
        "pm",
        "research/spike",
        "--from-file",
        fixture.to_str().unwrap(),
    ]);
    assert_ok(&out);

    let printed = stdout(&sb.pm(&["project", "show", "pm", "--doc", "research/spike"]));
    assert_eq!(printed, "# Spike notes\n\nLoro wins.\n");

    let v = json(&sb.pm(&["project", "show", "pm", "--json"]));
    assert_eq!(
        v["documents"]["research/spike"],
        "# Spike notes\n\nLoro wins.\n"
    );

    // The design doc itself is untouched by adding a named document.
    assert_eq!(v["doc"], "");

    let store = sb.store();
    let doc_id = store.named_doc_id("pm", "research/spike").unwrap().unwrap();
    assert_eq!(store.ops(doc_id).unwrap().len(), 1);
}

#[test]
fn doc_add_rejects_a_duplicate_name_and_show_doc_rejects_an_unknown_one() {
    let sb = Sandbox::initialized();
    assert_ok(&sb.pm(&["project", "new", "pm", "--title", "pm"]));
    let fixture = sb.fixture("notes.md", "notes\n");
    assert_ok(&sb.pm(&[
        "project",
        "doc",
        "add",
        "pm",
        "notes",
        "--from-file",
        fixture.to_str().unwrap(),
    ]));
    let dup = sb.pm(&[
        "project",
        "doc",
        "add",
        "pm",
        "notes",
        "--from-file",
        fixture.to_str().unwrap(),
    ]);
    assert_code(&dup, 1);

    assert_code(&sb.pm(&["project", "show", "pm", "--doc", "nope"]), 3);
}

// -------------------------------------------------------------------- AC4

#[test]
fn delete_is_refused_while_the_project_has_live_tickets() {
    let sb = Sandbox::initialized();
    assert_ok(&sb.pm(&["project", "new", "pm", "--title", "pm"]));
    assert_ok(&sb.pm(&["new", "--title", "t", "--project", "pm"]));

    let out = sb.pm(&["project", "delete", "pm"]);
    assert_code(&out, 1);
    assert!(stderr(&out).contains("tickets"), "{}", stderr(&out));
    assert!(sb.store().project("pm").unwrap().is_some());
}

#[test]
fn delete_removes_an_empty_project() {
    let sb = Sandbox::initialized();
    assert_ok(&sb.pm(&["project", "new", "pm", "--title", "pm"]));
    assert_ok(&sb.pm(&["project", "delete", "pm"]));
    assert!(sb.store().project("pm").unwrap().is_none());
    assert_code(&sb.pm(&["project", "show", "pm"]), 3);
}

#[test]
fn delete_is_refused_while_it_has_a_child_project() {
    let sb = Sandbox::initialized();
    assert_ok(&sb.pm(&["project", "new", "parent", "--title", "p"]));
    assert_ok(&sb.pm(&[
        "project", "new", "child", "--title", "c", "--parent", "parent",
    ]));
    let out = sb.pm(&["project", "delete", "parent"]);
    assert_code(&out, 1);
    assert!(stderr(&out).contains("child"), "{}", stderr(&out));
}

// -------------------------------------------------------------- pm doctor

#[test]
fn doctor_stays_clean_through_a_full_project_workflow() {
    let sb = Sandbox::initialized();
    assert_ok(&sb.pm(&["project", "new", "pm", "--title", "pm"]));
    let editor = sb.editor_script("editor.sh", "design text");
    assert_ok(&sb.run(
        &["project", "edit", "pm"],
        &[("EDITOR", editor.to_str().unwrap())],
    ));
    let fixture = sb.fixture("notes.md", "notes\n");
    assert_ok(&sb.pm(&[
        "project",
        "doc",
        "add",
        "pm",
        "notes",
        "--from-file",
        fixture.to_str().unwrap(),
    ]));

    assert_ok(&sb.pm(&["doctor"]));
    let v = json(&sb.pm(&["doctor", "--rebuild", "--json"]));
    assert_eq!(v["healthy"], true);
    assert_eq!(v["rebuilt"]["tables"], serde_json::json!([]));
}

/// Round-3 review finding: a project's `body.edit` ops stay in the log
/// after `pm project delete` (the log is never pruned), and `pm doctor` /
/// `--rebuild` must not choke on them — they used to be misrouted into
/// the ticket replay path and fail with `UnknownTicket`.
#[test]
fn doctor_survives_a_deleted_project_that_had_document_edits() {
    let sb = Sandbox::initialized();
    assert_ok(&sb.pm(&["project", "new", "pm", "--title", "pm"]));
    let editor = sb.editor_script("editor.sh", "design text");
    assert_ok(&sb.run(
        &["project", "edit", "pm"],
        &[("EDITOR", editor.to_str().unwrap())],
    ));
    assert_ok(&sb.pm(&["project", "delete", "pm"]));

    assert_ok(&sb.pm(&["doctor"]));
    let v = json(&sb.pm(&["doctor", "--rebuild", "--json"]));
    assert_eq!(v["healthy"], true, "{v}");
    assert_eq!(v["rebuilt"]["tables"], serde_json::json!([]));
}

#[test]
fn project_new_rejects_path_shaped_ids_with_usage_exit_and_accepts_live_ones() {
    let sb = Sandbox::initialized();
    for bad in [
        "../x",
        "a/b",
        "..",
        "a\\b",
        "a\u{1}b",
        "Upper",
        &"a".repeat(65),
    ] {
        let out = sb.pm(&[
            "project",
            "new",
            bad,
            "--title",
            "T",
            "--workspace",
            sb.ws_str(),
        ]);
        assert_eq!(out.status.code(), Some(2), "{bad:?}: {}", stderr(&out));
    }
    for ok in ["pm", "think-3", "ui-leaf-v1"] {
        assert_ok(&sb.pm(&[
            "project",
            "new",
            ok,
            "--title",
            "T",
            "--workspace",
            sb.ws_str(),
        ]));
    }
}

#[test]
fn init_rejects_path_shaped_prefixes_with_usage_exit() {
    for bad in ["../X", "A/B", "a", "A\u{1}B", "", &"A".repeat(17)] {
        let sb = Sandbox::new();
        let out = sb.pm(&["init", "--prefix", bad, "--workspace", sb.ws_str()]);
        assert_eq!(out.status.code(), Some(2), "{bad:?}: {}", stderr(&out));
        assert!(!sb.ws.exists(), "{bad:?} created a workspace");
    }
}

// ------------------------------------------------ AGT-1480: --from-file

impl Sandbox {
    /// `pm` with `input` on stdin.
    fn pm_stdin(&self, args: &[&str], input: &str) -> Output {
        use std::io::Write;
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_pm"));
        cmd.args(args)
            .env_clear()
            .env("HOME", self.home.path())
            .env("USER", "tester")
            .env("PATH", "/usr/bin:/bin")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = cmd.spawn().unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(input.as_bytes())
            .unwrap();
        child.wait_with_output().unwrap()
    }

    fn doc_of(&self, id: &str) -> String {
        json(&self.pm(&["project", "show", id, "--json"]))["doc"]
            .as_str()
            .unwrap()
            .to_string()
    }

    fn replica(&self) -> Sandbox {
        let home = tempfile::tempdir().unwrap();
        let ws = home.path().join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        for name in ["pm.sqlite", "pm.sqlite-wal", "pm.sqlite-shm"] {
            let from = self.ws.join(name);
            if from.is_file() {
                std::fs::copy(&from, ws.join(name)).unwrap();
            }
        }
        let config = |home: &std::path::Path| home.join(".config/pm/config.toml");
        let text = std::fs::read_to_string(config(self.home.path())).unwrap();
        let text = text.replace(self.ws.to_str().unwrap(), ws.to_str().unwrap());
        std::fs::create_dir_all(config(home.path()).parent().unwrap()).unwrap();
        std::fs::write(config(home.path()), text).unwrap();
        Sandbox { home, ws }
    }
}

#[test]
fn from_file_replaces_the_design_doc_and_unchanged_is_a_no_op() {
    let sb = Sandbox::initialized();
    assert_ok(&sb.pm(&["project", "new", "pm", "--title", "pm"]));
    let f = sb.fixture("spec.md", "# pm\n\nFirst.\n");
    let v = json(&sb.pm(&[
        "project",
        "edit",
        "pm",
        "--from-file",
        f.to_str().unwrap(),
        "--json",
    ]));
    assert_eq!(v["doc"], "# pm\n\nFirst.\n");
    assert_eq!(sb.doc_of("pm"), "# pm\n\nFirst.\n");
    let doc_id = sb.store().design_doc_id("pm").unwrap().unwrap();
    assert_eq!(sb.store().ops(doc_id).unwrap().len(), 1);

    assert_ok(&sb.pm(&["project", "edit", "pm", "--from-file", f.to_str().unwrap()]));
    assert_eq!(sb.store().ops(doc_id).unwrap().len(), 1, "unchanged: no op");

    let g = sb.fixture("spec2.md", "# pm\n\nSecond.\n");
    assert_ok(&sb.pm(&["project", "edit", "pm", "--from-file", g.to_str().unwrap()]));
    assert_eq!(sb.doc_of("pm"), "# pm\n\nSecond.\n");
    assert_eq!(sb.store().ops(doc_id).unwrap().len(), 2);
}

#[test]
fn from_file_reads_stdin_with_a_dash() {
    let sb = Sandbox::initialized();
    assert_ok(&sb.pm(&["project", "new", "pm", "--title", "pm"]));
    assert_ok(&sb.pm_stdin(
        &["project", "edit", "pm", "--from-file", "-"],
        "piped body\n",
    ));
    assert_eq!(sb.doc_of("pm"), "piped body\n");
}

#[test]
fn from_file_writes_a_named_doc_and_doc_add_points_there_on_a_duplicate() {
    let sb = Sandbox::initialized();
    assert_ok(&sb.pm(&["project", "new", "pm", "--title", "pm"]));
    let f = sb.fixture("a.md", "one\n");
    assert_ok(&sb.pm(&[
        "project",
        "doc",
        "add",
        "pm",
        "notes",
        "--from-file",
        f.to_str().unwrap(),
    ]));

    let out = sb.pm(&[
        "project",
        "doc",
        "add",
        "pm",
        "notes",
        "--from-file",
        f.to_str().unwrap(),
    ]);
    assert_code(&out, 1);
    assert!(stderr(&out).contains("project edit pm --doc notes --from-file"));

    assert_ok(&sb.pm_stdin(
        &[
            "project",
            "edit",
            "pm",
            "--doc",
            "notes",
            "--from-file",
            "-",
        ],
        "two\n",
    ));
    let v = json(&sb.pm(&["project", "show", "pm", "--doc", "notes", "--json"]));
    assert_eq!(v["body"], "two\n");
    assert_eq!(sb.doc_of("pm"), "", "the design doc is untouched");
}

#[test]
fn from_file_unknown_project_or_doc_exits_3() {
    let sb = Sandbox::initialized();
    assert_ok(&sb.pm(&["project", "new", "pm", "--title", "pm"]));
    let f = sb.fixture("a.md", "x\n");
    let p = f.to_str().unwrap();
    assert_code(&sb.pm(&["project", "edit", "nope", "--from-file", p]), 3);
    assert_code(
        &sb.pm(&["project", "edit", "pm", "--doc", "nope", "--from-file", p]),
        3,
    );
}

#[test]
fn from_file_conflicts_with_view_and_doc_needs_from_file() {
    let sb = Sandbox::initialized();
    assert_ok(&sb.pm(&["project", "new", "pm", "--title", "pm"]));
    let f = sb.fixture("a.md", "x\n");
    let p = f.to_str().unwrap();
    assert_code(
        &sb.pm(&[
            "project",
            "edit",
            "pm",
            "--from-file",
            p,
            "--view",
            "editor",
        ]),
        2,
    );
    assert_code(&sb.pm(&["project", "edit", "pm", "--doc", "n"]), 2);
}

#[test]
fn from_file_edits_merge_with_a_concurrent_replica_edit() {
    let sb = Sandbox::initialized();
    assert_ok(&sb.pm(&["project", "new", "pm", "--title", "pm"]));
    let base = sb.fixture("base.md", "line A\nline B\n");
    assert_ok(&sb.pm(&[
        "project",
        "edit",
        "pm",
        "--from-file",
        base.to_str().unwrap(),
    ]));

    let other = sb.replica();
    let left = sb.fixture("l.md", "line A left\nline B\n");
    let right = other.fixture("r.md", "line A\nline B right\n");
    assert_ok(&sb.pm(&[
        "project",
        "edit",
        "pm",
        "--from-file",
        left.to_str().unwrap(),
    ]));
    assert_ok(&other.pm(&[
        "project",
        "edit",
        "pm",
        "--from-file",
        right.to_str().unwrap(),
    ]));

    let doc_id = sb.store().design_doc_id("pm").unwrap().unwrap();
    let (a, b) = (
        sb.store().ops(doc_id).unwrap(),
        other.store().ops(doc_id).unwrap(),
    );
    sb.store().apply_pulled(&b).unwrap();
    other.store().apply_pulled(&a).unwrap();
    let merged = sb.doc_of("pm");
    assert_eq!(merged, "line A left\nline B right\n");
    assert_eq!(other.doc_of("pm"), merged);
}

#[test]
fn doc_add_reserves_names_that_read_as_the_design_doc() {
    let sb = Sandbox::initialized();
    assert_ok(&sb.pm(&["project", "new", "pm", "--title", "pm"]));
    let f = sb.fixture("a.md", "one\n");
    for name in ["design", "Design", "README", "readme"] {
        let out = sb.pm(&[
            "project",
            "doc",
            "add",
            "pm",
            name,
            "--from-file",
            f.to_str().unwrap(),
        ]);
        assert_code(&out, 2);
        assert!(stderr(&out).contains("project edit pm --from-file"));
    }
    let v = json(&sb.pm(&["project", "show", "pm", "--json"]));
    assert!(v["documents"].as_object().is_none_or(|d| d.is_empty()));
}

#[test]
fn text_output_says_design_doc_versus_named_doc() {
    let sb = Sandbox::initialized();
    assert_ok(&sb.pm(&["project", "new", "pm", "--title", "pm"]));
    let f = sb.fixture("a.md", "one\n");
    let p = f.to_str().unwrap();
    assert_ok(&sb.pm(&["project", "doc", "add", "pm", "notes", "--from-file", p]));

    let out = sb.pm(&["project", "edit", "pm", "--from-file", p]);
    assert_ok(&out);
    assert_eq!(stdout(&out).trim(), "pm: design doc updated");
    let out = sb.pm(&["project", "edit", "pm", "--from-file", p]);
    assert_eq!(stdout(&out).trim(), "pm: design doc unchanged");
    let out = sb.pm(&["project", "edit", "pm", "--doc", "notes", "--from-file", p]);
    assert_ok(&out);
    assert_eq!(stdout(&out).trim(), "pm: named doc 'notes' unchanged");

    let shown = sb.pm(&["project", "show", "pm"]);
    let text = stdout(&shown);
    assert!(text.contains("named docs: notes"), "{text}");
    assert!(text.contains("design doc:"), "{text}");
    let named = sb.pm(&["project", "show", "pm", "--doc", "notes"]);
    assert_eq!(stdout(&named), "one\n", "stdout stays the bare body");
    assert!(stderr(&named).contains("pm: named doc 'notes'"));
}

// ------------------------------------------------- pm project set (AGT-1489)

/// How many `project.set` ops the workspace log holds.
fn project_set_ops(sb: &Sandbox) -> usize {
    let v = json(&sb.pm(&["log", "--json"]));
    v.to_string().matches("\"project.set\"").count()
}

#[test]
fn set_changes_title_status_and_parent_and_show_reflects_it() {
    let sb = Sandbox::initialized();
    assert_ok(&sb.pm(&["project", "new", "a", "--title", "A"]));
    assert_ok(&sb.pm(&["project", "new", "b", "--title", "B"]));
    let before = project_set_ops(&sb);

    let v = json(&sb.pm(&[
        "project",
        "set",
        "b",
        "title=Bravo",
        "status=complete",
        "parent=a",
        "--json",
    ]));
    assert_eq!(v["title"], "Bravo");
    assert_eq!(v["status"], "complete");
    assert_eq!(v["parent"], "a");
    assert_eq!(
        project_set_ops(&sb),
        before + 3,
        "one project.set per field"
    );

    let v = json(&sb.pm(&["project", "show", "b", "--json"]));
    assert_eq!(
        (&v["title"], &v["status"], &v["parent"]),
        (&"Bravo".into(), &"complete".into(), &"a".into())
    );
    let text = stdout(&sb.pm(&["project", "show", "b"]));
    assert!(text.contains("parent:  a"), "{text}");

    // An unchanged value commits nothing.
    assert_ok(&sb.pm(&["project", "set", "b", "title=Bravo", "parent=a"]));
    assert_eq!(project_set_ops(&sb), before + 3);

    // AC2: `parent=-` clears it.
    let v = json(&sb.pm(&["project", "set", "b", "parent=-", "--json"]));
    assert_eq!(v["parent"], Value::Null);
    let v = json(&sb.pm(&["project", "show", "b", "--json"]));
    assert_eq!(v["parent"], Value::Null);

    assert_ok(&sb.pm(&["doctor"]));
    let v = json(&sb.pm(&["doctor", "--rebuild", "--json"]));
    assert_eq!(v["healthy"], true);
}

#[test]
fn set_refuses_a_parent_that_closes_a_cycle() {
    let sb = Sandbox::initialized();
    for (id, parent) in [("a", None), ("b", Some("a")), ("c", Some("b"))] {
        let mut args = vec!["project", "new", id, "--title", id];
        if let Some(p) = parent {
            args.extend(["--parent", p]);
        }
        assert_ok(&sb.pm(&args));
    }
    let before = project_set_ops(&sb);

    let out = sb.pm(&["project", "set", "a", "parent=a"]);
    assert_code(&out, 2);
    assert!(stderr(&out).contains("a -> a"), "{}", stderr(&out));

    let out = sb.pm(&["project", "set", "a", "parent=b"]);
    assert_code(&out, 2);
    assert!(stderr(&out).contains("a -> b -> a"), "{}", stderr(&out));

    // A grandchild: the message names the whole loop.
    let out = sb.pm(&["project", "set", "a", "parent=c", "title=renamed"]);
    assert_code(&out, 2);
    assert!(
        stderr(&out).contains("a -> c -> b -> a"),
        "{}",
        stderr(&out)
    );

    assert_eq!(project_set_ops(&sb), before, "a refusal commits nothing");
    let v = json(&sb.pm(&["project", "show", "a", "--json"]));
    assert_eq!((&v["parent"], &v["title"]), (&Value::Null, &"a".into()));

    // Moving a sub-tree under a sibling (not a descendant) is fine.
    assert_ok(&sb.pm(&["project", "new", "d", "--title", "d"]));
    assert_ok(&sb.pm(&["project", "set", "b", "parent=d"]));
    assert_ok(&sb.pm(&["project", "set", "d", "parent=a"]));
}

#[test]
fn set_refuses_a_parent_on_an_initiative() {
    let sb = Sandbox::initialized();
    assert_ok(&sb.pm(&[
        "project",
        "new",
        "i",
        "--title",
        "I",
        "--kind",
        "initiative",
    ]));
    assert_ok(&sb.pm(&["project", "new", "p", "--title", "P"]));
    let before = project_set_ops(&sb);

    // AGT-1489 AC5: an initiative has no parent, so setting one exits 2.
    let out = sb.pm(&["project", "set", "i", "parent=p"]);
    assert_code(&out, 2);
    assert!(stderr(&out).contains("initiative"), "{}", stderr(&out));
    assert_eq!(project_set_ops(&sb), before, "a refusal commits nothing");

    // Clearing it, other keys, and filing a project under it still work.
    assert_ok(&sb.pm(&["project", "set", "i", "parent=-", "title=Init"]));
    assert_ok(&sb.pm(&["project", "set", "p", "parent=i"]));
    let v = json(&sb.pm(&["project", "show", "p", "--json"]));
    assert_eq!(v["parent"], "i");
}

#[test]
fn set_rejects_unknown_parents_projects_and_bad_assignments() {
    let sb = Sandbox::initialized();
    assert_ok(&sb.pm(&["project", "new", "a", "--title", "A"]));
    let before = project_set_ops(&sb);

    assert_code(&sb.pm(&["project", "set", "a", "parent=nope"]), 3);
    assert_code(&sb.pm(&["project", "set", "nope", "title=x"]), 3);
    for bad in [
        "colour=red",
        "title",
        "title=",
        "status=bogus",
        "parent=",
        "repo=OpenThinkAi/pm",
    ] {
        let out = sb.pm(&["project", "set", "a", bad]);
        assert_code(&out, 2);
        assert!(!stderr(&out).is_empty(), "{bad}");
    }
    // Every assignment parses before anything commits.
    assert_code(&sb.pm(&["project", "set", "a", "title=ok", "bogus=1"]), 2);
    assert_code(&sb.pm(&["project", "set", "a", "title=x", "title=y"]), 2);
    assert_code(&sb.pm(&["project", "set", "a"]), 2);
    assert_eq!(project_set_ops(&sb), before);
    let v = json(&sb.pm(&["project", "show", "a", "--json"]));
    assert_eq!(v["title"], "A");
}
