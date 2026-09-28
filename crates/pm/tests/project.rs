//! `pm project new/show/list/edit/doc/delete` driven through the built
//! binary (AGT-1344). Sandbox as in `tests/cli.rs`: temp HOME, cleared
//! env, no stdin — a command that tried to prompt would read EOF rather
//! than hang, and `pm project edit` never even tries (it requires $EDITOR
//! instead of falling back to a default terminal editor).

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
            &["init", "--prefix", "AGT", "--workspace", sb.ws_str()],
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

// -------------------------------------------------------------------- AC2

#[test]
fn edit_requires_editor_set() {
    let sb = Sandbox::initialized();
    assert_ok(&sb.pm(&["project", "new", "pm", "--title", "pm"]));
    let out = sb.run(&["project", "edit", "pm"], &[("EDITOR", "")]);
    assert_code(&out, 2);
    assert!(stderr(&out).contains("$EDITOR"), "{}", stderr(&out));
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
