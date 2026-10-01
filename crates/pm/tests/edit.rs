//! `pm edit` driven through the built binary with `EDITOR` set to a shell
//! script (AGT-1345). Each script appends a line to a counter file per
//! invocation, so a test can assert how many times the editor opened, and
//! acts on the invocation number: overwrite the file with a fixture, keep
//! a copy of what it was shown, or exit non-zero. stdin is `/dev/null`, so
//! nothing can wait on a terminal.

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use pm_core::{Project, ProjectStatus};
use pm_store::Store;
use serde_json::Value;
use tempfile::TempDir;

struct Sandbox {
    home: TempDir,
    ws: PathBuf,
}

impl Sandbox {
    /// An initialized workspace with projects `pm` and `other`, and one
    /// ticket AGT-1 (labels a, b; a two-line description).
    fn new() -> Self {
        let home = tempfile::tempdir().unwrap();
        let ws = home.path().join("ws");
        let sb = Sandbox { home, ws };
        let ws = sb.ws.to_str().unwrap().to_string();
        assert_ok(&sb.run(
            &[
                "init",
                "--prefix",
                "AGT",
                "--preset",
                "saltline",
                "--workspace",
                &ws,
            ],
            None,
        ));
        let mut store = sb.store();
        for id in ["pm", "other"] {
            store
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
        assert_ok(&sb.run(
            &[
                "new",
                "--title",
                "Original title",
                "--project",
                "pm",
                "--label",
                "a,b",
                "--description",
                "line one\nline two",
            ],
            None,
        ));
        sb
    }

    fn path(&self, name: &str) -> PathBuf {
        self.home.path().join(name)
    }

    fn store(&self) -> Store {
        Store::open(self.ws.join("pm.sqlite")).unwrap()
    }

    /// `pm` with a cleared environment (HOME = sandbox, USER = tester) and
    /// `EDITOR` = `editor`, if given. `PATH` is the system directories only,
    /// so no real ui-leaf is ever found (and no browser ever opens): the
    /// default view falls back to `$EDITOR`. `DISPLAY` is set so that holds
    /// on Linux for the same reason it does on macOS — ui-leaf is missing,
    /// not the display. `tests/launch.rs` covers a (fake) ui-leaf.
    fn run(&self, args: &[&str], editor: Option<&Path>) -> Output {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_pm"));
        cmd.args(args)
            .env_clear()
            .env("HOME", self.home.path())
            .env("USER", "tester")
            // Temp dirs go in the sandbox.
            .env("TMPDIR", self.home.path())
            .env("PATH", "/usr/bin:/bin")
            .env("DISPLAY", ":0")
            .stdin(Stdio::null());
        if let Some(editor) = editor {
            cmd.env("EDITOR", editor);
        }
        cmd.output().unwrap()
    }

    /// Writes an `EDITOR` script. `steps[i]` is the shell run on the i-th
    /// invocation (1-based `$n`; `$1` is the file); past the last step the
    /// script does nothing and exits 0.
    fn editor(&self, steps: &[&str]) -> PathBuf {
        let count = self.path("editor-count");
        let mut script = format!(
            "#!/bin/sh\necho x >> '{c}'\nn=$(wc -l < '{c}' | tr -d ' ')\ncase $n in\n",
            c = count.display()
        );
        for (i, step) in steps.iter().enumerate() {
            script.push_str(&format!("{}) {step} ;;\n", i + 1));
        }
        script.push_str("esac\n");
        let path = self.path("editor.sh");
        std::fs::write(&path, script).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        path
    }

    fn invocations(&self) -> usize {
        std::fs::read_to_string(self.path("editor-count"))
            .map(|s| s.lines().count())
            .unwrap_or(0)
    }

    /// A fixture file; `cp` it over `$1` from an editor step.
    fn fixture(&self, name: &str, contents: &str) -> String {
        let path = self.path(name);
        std::fs::write(&path, contents).unwrap();
        format!("cp '{}' \"$1\"", path.display())
    }

    /// Every op kind on AGT-1, in log order.
    fn op_kinds(&self) -> Vec<&'static str> {
        let store = self.store();
        let t = store.ticket_by_number(1).unwrap().unwrap();
        store
            .ops(t.id)
            .unwrap()
            .iter()
            .map(|o| o.payload.kind())
            .collect()
    }

    fn show(&self) -> Value {
        let out = self.run(&["show", "AGT-1", "--json"], None);
        assert_ok(&out);
        serde_json::from_slice(&out.stdout).unwrap()
    }
}

fn assert_code(out: &Output, code: i32) {
    assert_eq!(
        out.status.code(),
        Some(code),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

fn assert_ok(out: &Output) {
    assert_code(out, 0);
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

const EDITED: &str = "---\n\
title: New title\n\
priority: high\n\
project: other\n\
repo:\n\
assignee:\n\
labels: [a, c]\n\
linked-github:\n\
linked-pr:\n\
linear:\n\
---\n\
\n\
line one\n\
line two, edited\n";

#[test]
fn a_save_becomes_field_label_and_body_ops() {
    let sb = Sandbox::new();
    let shown = sb.path("shown.md");
    let editor = sb.editor(&[&format!(
        "cp \"$1\" '{}'; {}",
        shown.display(),
        sb.fixture("edited.md", EDITED)
    )]);
    let before = sb.op_kinds().len();
    let out = sb.run(&["edit", "AGT-1", "--view=editor"], Some(&editor));
    assert_ok(&out);
    assert_eq!(sb.invocations(), 1);

    // The editor saw the ticket as frontmatter + markdown.
    let shown = std::fs::read_to_string(shown).unwrap();
    assert!(
        shown.starts_with("# pm edit AGT-1 (state: triage"),
        "{shown}"
    );
    assert!(shown.contains("title: Original title\n"), "{shown}");
    assert!(shown.ends_with("---\n\nline one\nline two\n"), "{shown}");

    let new: Vec<&str> = sb.op_kinds()[before..].to_vec();
    assert_eq!(
        new,
        [
            "field.set",
            "field.set",
            "field.set",
            "label.add",
            "label.remove",
            "body.edit"
        ]
    );
    let t = sb.show();
    assert_eq!(t["title"], "New title");
    assert_eq!(t["priority"], "high");
    assert_eq!(t["project"], "other");
    assert_eq!(t["labels"], serde_json::json!(["a", "c"]));
    assert_eq!(t["description"], "line one\nline two, edited");

    // The ticket tables still replay from the log.
    assert_ok(&sb.run(&["doctor"], None));
}

#[test]
fn an_unchanged_save_emits_zero_ops() {
    let sb = Sandbox::new();
    let editor = sb.editor(&[":"]);
    let before = sb.op_kinds();
    let out = sb.run(&["edit", "AGT-1"], Some(&editor));
    assert_ok(&out);
    assert!(stderr(&out).contains("no changes"), "{}", stderr(&out));
    assert_eq!(sb.op_kinds(), before);
}

#[test]
fn a_parse_error_reopens_with_the_error_and_an_unchanged_error_file_aborts() {
    let sb = Sandbox::new();
    let reshown = sb.path("reshown.md");
    let editor = sb.editor(&[
        &sb.fixture("broken.md", "---\ntitle: x\npriority: urgent\n---\nbody\n"),
        &format!("cp \"$1\" '{}'", reshown.display()),
    ]);
    let before = sb.op_kinds();
    let out = sb.run(&["edit", "AGT-1"], Some(&editor));
    assert_code(&out, 1);
    assert!(stderr(&out).contains("aborted"), "{}", stderr(&out));
    assert_eq!(sb.invocations(), 2);
    assert_eq!(sb.op_kinds(), before, "an abort writes nothing");

    let reshown = std::fs::read_to_string(reshown).unwrap();
    assert!(
        reshown.starts_with("# pm edit AGT-1: could not save:\n#   "),
        "{reshown}"
    );
    assert!(
        reshown.contains("urgent"),
        "the error names the bad value: {reshown}"
    );
    assert!(
        reshown.contains("---\ntitle: x\npriority: urgent\n---\n"),
        "{reshown}"
    );
}

#[test]
fn a_fixed_save_after_a_parse_error_applies() {
    let sb = Sandbox::new();
    let editor = sb.editor(&[
        &sb.fixture(
            "broken.md",
            "---\ntitle: x\nproject: nope\npriority: low\n---\n",
        ),
        &sb.fixture("edited.md", EDITED),
    ]);
    let out = sb.run(&["edit", "AGT-1"], Some(&editor));
    assert_ok(&out);
    assert_eq!(sb.invocations(), 2);
    assert_eq!(sb.show()["title"], "New title");
}

#[test]
fn an_editor_exiting_non_zero_aborts() {
    let sb = Sandbox::new();
    let editor = sb.editor(&[&format!("{}; exit 3", sb.fixture("edited.md", EDITED))]);
    let before = sb.op_kinds();
    let out = sb.run(&["edit", "AGT-1"], Some(&editor));
    assert_code(&out, 1);
    assert!(stderr(&out).contains("non-zero"), "{}", stderr(&out));
    assert_eq!(sb.op_kinds(), before);
    assert_eq!(sb.show()["title"], "Original title");
}

#[test]
fn an_editor_that_keeps_saving_garbage_is_bounded() {
    let sb = Sandbox::new();
    // Every invocation writes a different broken file, so none counts as
    // "unchanged"; the reopen cap still ends the loop.
    let editor = sb.editor(&[]);
    std::fs::write(
        &editor,
        format!(
            "#!/bin/sh\necho x >> '{c}'\nwc -l < '{c}' > \"$1\"\n",
            c = sb.path("editor-count").display()
        ),
    )
    .unwrap();
    let out = sb.run(&["edit", "AGT-1"], Some(&editor));
    assert_code(&out, 1);
    assert!(stderr(&out).contains("gave up"), "{}", stderr(&out));
    // The last text is echoed, and no temp directory outlives the command.
    assert!(
        stderr(&out).contains("the text you saved last:"),
        "{}",
        stderr(&out)
    );
    let leftovers = std::fs::read_dir(sb.home.path())
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().starts_with("pm-edit-"))
        .count();
    assert_eq!(leftovers, 0);
    assert_eq!(sb.invocations(), 11);
}

#[test]
fn without_ui_leaf_every_view_choice_uses_the_editor() {
    let sb = Sandbox::new();
    let editor = sb.editor(&[]);
    let note = "ui-leaf not found";

    // The default (ui-leaf) run non-interactively, as here, is the editor
    // flow without a word.
    let out = sb.run(&["edit", "AGT-1"], Some(&editor));
    assert_ok(&out);
    assert!(!stderr(&out).contains("ui-leaf"), "{}", stderr(&out));
    assert_eq!(sb.invocations(), 1);

    // Asked for by flag: falls back with one note, no longer "not yet
    // available".
    let out = sb.run(&["edit", "AGT-1", "--view=ui-leaf"], Some(&editor));
    assert_ok(&out);
    assert!(stderr(&out).contains(note), "{}", stderr(&out));
    assert_eq!(
        stderr(&out)
            .lines()
            .filter(|l| l.contains("ui-leaf"))
            .count(),
        1,
        "{}",
        stderr(&out)
    );
    assert!(!stderr(&out).contains("not yet available"));
    assert_eq!(sb.invocations(), 2);

    // Asked for by config: same.
    let config = sb.path(".config/pm/config.toml");
    let base = std::fs::read_to_string(&config).unwrap();
    std::fs::write(&config, format!("{base}\n[edit]\nview = \"ui-leaf\"\n")).unwrap();
    let out = sb.run(&["edit", "AGT-1"], Some(&editor));
    assert_ok(&out);
    assert!(stderr(&out).contains(note), "{}", stderr(&out));
    assert_eq!(sb.invocations(), 3);

    // --view editor (overriding config) and edit.view = editor: the
    // $EDITOR flow with no ui-leaf note at all.
    let out = sb.run(&["edit", "AGT-1", "--view", "editor"], Some(&editor));
    assert_ok(&out);
    assert!(!stderr(&out).contains("ui-leaf"), "{}", stderr(&out));
    std::fs::write(&config, format!("{base}\n[edit]\nview = \"editor\"\n")).unwrap();
    let out = sb.run(&["edit", "AGT-1"], Some(&editor));
    assert_ok(&out);
    assert!(!stderr(&out).contains("ui-leaf"), "{}", stderr(&out));
    assert_eq!(sb.invocations(), 5);

    std::fs::write(&config, format!("{base}\n[edit]\nview = \"emacs\"\n")).unwrap();
    assert_code(&sb.run(&["edit", "AGT-1"], Some(&editor)), 2);
    assert_eq!(sb.invocations(), 5, "a bad edit.view never opens an editor");
}

#[test]
fn a_body_edit_merges_with_a_concurrent_edit() {
    // While the editor is open, a second `pm edit` (another actor, its own
    // editor script) rewrites line two and a `pm set` changes priority.
    // The outer save edits line one; nothing the others did is reverted.
    let sb = Sandbox::new();
    let inner = sb.path("inner.sh");
    std::fs::write(
        &inner,
        "#!/bin/sh\nperl -pi -e 's/line two/LINE TWO/' \"$1\"\n",
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&inner, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let concurrent = format!(
        "HOME='{home}' USER=other EDITOR='{inner}' '{bin}' edit AGT-1 >/dev/null && \
         HOME='{home}' USER=other '{bin}' set AGT-1 priority=critical >/dev/null || exit 9",
        home = sb.home.path().display(),
        inner = inner.display(),
        bin = env!("CARGO_BIN_EXE_pm"),
    );
    let editor = sb.editor(&[&format!(
        "{concurrent}; perl -pi -e 's/line one/LINE ONE/' \"$1\""
    )]);
    let out = sb.run(&["edit", "AGT-1"], Some(&editor));
    assert_ok(&out);
    let t = sb.show();
    assert_eq!(t["description"], "LINE ONE\nLINE TWO");
    assert_eq!(
        t["priority"], "critical",
        "an untouched field is not reverted"
    );
    assert_ok(&sb.run(&["doctor"], None));
}

impl Sandbox {
    /// A second replica of this workspace: a byte copy of its database in
    /// a fresh HOME, taken while no `pm` process has it open.
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
        // `pm init` pointed this HOME's config.toml at the workspace; point
        // the copy's at its own.
        let config = |home: &Path| home.join(".config/pm/config.toml");
        let text = std::fs::read_to_string(config(self.home.path())).unwrap();
        let text = text.replace(self.ws.to_str().unwrap(), ws.to_str().unwrap());
        std::fs::create_dir_all(config(home.path()).parent().unwrap()).unwrap();
        std::fs::write(config(home.path()), text).unwrap();
        Sandbox { home, ws }
    }

    /// Every op on AGT-1 in this replica's log.
    fn ticket_ops(&self) -> Vec<pm_core::Op> {
        let store = self.store();
        let t = store.ticket_by_number(1).unwrap().unwrap();
        store.ops(t.id).unwrap()
    }
}

/// P3 gate finding 1 (AGT-1429): two replicas `pm edit` the same
/// description offline — one edits line A and appends C, the other edits
/// line B and appends D — and exchange ops. Each edit must land on its own
/// line; the character-level diff put ", x" on line C.
#[test]
fn offline_description_edits_merge_line_faithfully() {
    let sb = Sandbox::new();
    let seed = sb.editor(&["perl -pi -e 's/^line one$/A./; s/^line two$/B./' \"$1\""]);
    assert_ok(&sb.run(&["edit", "AGT-1", "--view=editor"], Some(&seed)));
    assert_eq!(sb.show()["description"], "A.\nB.");
    std::fs::remove_file(sb.path("editor-count")).unwrap();

    let other = sb.replica();
    let left = sb.editor(&["perl -pi -e 's/^A\\.$/A, y./' \"$1\"; printf 'C.\\n' >> \"$1\""]);
    let right = other.editor(&["perl -pi -e 's/^B\\.$/B, x./' \"$1\"; printf 'D.\\n' >> \"$1\""]);
    assert_ok(&sb.run(&["edit", "AGT-1", "--view=editor"], Some(&left)));
    assert_ok(&other.run(&["edit", "AGT-1", "--view=editor"], Some(&right)));
    assert_eq!(sb.show()["description"], "A, y.\nB.\nC.");
    assert_eq!(other.show()["description"], "A.\nB, x.\nD.");

    // Reconnect: each replica pulls the other's log (already-known ops are
    // skipped by op id).
    let (from_sb, from_other) = (sb.ticket_ops(), other.ticket_ops());
    sb.store().apply_pulled(&from_other).unwrap();
    other.store().apply_pulled(&from_sb).unwrap();

    let merged = sb.show()["description"].as_str().unwrap().to_string();
    assert_eq!(
        other.show()["description"],
        merged.as_str(),
        "replicas converge"
    );
    assert!(
        merged == "A, y.\nB, x.\nC.\nD." || merged == "A, y.\nB, x.\nD.\nC.",
        "{merged:?}"
    );
    assert_ok(&sb.run(&["doctor"], None));
    assert_ok(&other.run(&["doctor"], None));
}

// ------------------------------------------------ AGT-1480: --from-file

impl Sandbox {
    /// A plain file with `contents` (unlike `fixture`, which is a `cp` step).
    fn file(&self, name: &str, contents: &str) -> String {
        let path = self.path(name);
        std::fs::write(&path, contents).unwrap();
        path.to_str().unwrap().to_string()
    }

    fn run_stdin(&self, args: &[&str], input: &str) -> Output {
        use std::io::Write;
        let mut child = Command::new(env!("CARGO_BIN_EXE_pm"))
            .args(args)
            .env_clear()
            .env("HOME", self.home.path())
            .env("USER", "tester")
            .env("PATH", "/usr/bin:/bin")
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
}

#[test]
fn from_file_replaces_only_the_description() {
    let sb = Sandbox::new();
    let f = sb.file("d.md", "new one\nline two\n\n## More\n");
    let out = sb.run(&["edit", "AGT-1", "--from-file", &f, "--json"], None);
    assert_ok(&out);
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["description"], "new one\nline two\n\n## More\n");
    let t = sb.show();
    assert_eq!(t["title"], "Original title");
    assert_eq!(t["labels"], serde_json::json!(["a", "b"]));
    assert_eq!(sb.op_kinds().last(), Some(&"body.edit"));
}

#[test]
fn from_file_unchanged_commits_nothing_and_stdin_works() {
    let sb = Sandbox::new();
    let before = sb.op_kinds().len();
    let out = sb.run_stdin(&["edit", "AGT-1", "--from-file", "-"], "line one\nline two");
    assert_ok(&out);
    assert_eq!(sb.op_kinds().len(), before, "same text: no op");
    assert_ok(&sb.run_stdin(&["edit", "AGT-1", "--from-file", "-"], "piped\n"));
    assert_eq!(sb.show()["description"], "piped\n");
}

#[test]
fn from_file_by_ulid_unknown_ticket_and_view_conflict() {
    let sb = Sandbox::new();
    let f = sb.file("d.md", "x\n");
    let ulid = sb
        .store()
        .ticket_by_number(1)
        .unwrap()
        .unwrap()
        .id
        .to_string();
    assert_ok(&sb.run(&["edit", &ulid, "--from-file", &f], None));
    assert_eq!(sb.show()["description"], "x\n");
    assert_code(&sb.run(&["edit", "AGT-99", "--from-file", &f], None), 3);
    assert_code(
        &sb.run(&["edit", "AGT-1", "--from-file", &f, "--view=editor"], None),
        2,
    );
}

#[test]
fn from_file_merges_line_faithfully_with_a_concurrent_replica() {
    let sb = Sandbox::new();
    let other = sb.replica();
    let l = sb.file("l.md", "line ONE\nline two");
    let r = other.file("r.md", "line one\nline TWO");
    assert_ok(&sb.run(&["edit", "AGT-1", "--from-file", &l], None));
    assert_ok(&other.run(&["edit", "AGT-1", "--from-file", &r], None));
    let (a, b) = (sb.ticket_ops(), other.ticket_ops());
    sb.store().apply_pulled(&b).unwrap();
    other.store().apply_pulled(&a).unwrap();
    assert_eq!(sb.show()["description"], "line ONE\nline TWO");
    assert_eq!(other.show()["description"], "line ONE\nline TWO");
    assert_ok(&sb.run(&["doctor"], None));
}
