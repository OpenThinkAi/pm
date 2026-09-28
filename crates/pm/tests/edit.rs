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
    /// `EDITOR` = `editor`, if given.
    fn run(&self, args: &[&str], editor: Option<&Path>) -> Output {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_pm"));
        cmd.args(args)
            .env_clear()
            .env("HOME", self.home.path())
            .env("USER", "tester")
            // Aborted edits keep their temp file; keep it in the sandbox.
            .env("TMPDIR", self.home.path())
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
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
    assert_eq!(sb.invocations(), 11);
}

#[test]
fn ui_leaf_is_not_yet_available_by_flag_or_config() {
    let sb = Sandbox::new();
    let editor = sb.editor(&[]);
    let out = sb.run(&["edit", "AGT-1", "--view=ui-leaf"], Some(&editor));
    assert_code(&out, 1);
    assert!(
        stderr(&out).contains("not yet available"),
        "{}",
        stderr(&out)
    );

    let config = sb.path(".config/pm/config.toml");
    let base = std::fs::read_to_string(&config).unwrap();
    std::fs::write(&config, format!("{base}\n[edit]\nview = \"ui-leaf\"\n")).unwrap();
    let out = sb.run(&["edit", "AGT-1"], Some(&editor));
    assert_code(&out, 1);
    assert!(
        stderr(&out).contains("not yet available"),
        "{}",
        stderr(&out)
    );
    // The flag overrides config.
    assert_ok(&sb.run(&["edit", "AGT-1", "--view", "editor"], Some(&editor)));

    std::fs::write(&config, format!("{base}\n[edit]\nview = \"emacs\"\n")).unwrap();
    assert_code(&sb.run(&["edit", "AGT-1"], Some(&editor)), 2);
    assert_eq!(sb.invocations(), 1, "the editor ran only for --view editor");
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
