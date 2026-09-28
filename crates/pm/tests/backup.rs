//! `pm backup` / `pm backup --restore` / `pm backup status` /
//! `pm backup install-timer` driven through the built binary (AGT-1350).
//!
//! Every run gets a fresh temp HOME and a cleared environment (same
//! pattern as `tests/cli.rs`), and every "remote" is a local bare repo —
//! nothing here touches the network, a real GitHub repo, or the real
//! `~/Library/LaunchAgents`.

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

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
            // `pm backup` shells out to `git`; keep PATH so it can find it
            // (everything else about the environment stays cleared/fake).
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

    /// `pm new --title <title>` against this sandbox's workspace; returns
    /// the created ticket's display id (`AGT-1`).
    fn new_ticket(&self, title: &str) -> String {
        let out = self.pm(&["new", "--title", title]);
        assert_ok(&out);
        stdout(&out).trim().to_string()
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
    serde_json::from_str(&stdout(out)).unwrap_or_else(|e| panic!("{e}: {}", stdout(out)))
}

/// A throwaway git command, run directly (not through `pm`) to build the
/// bare "remote" and inspect what `pm backup` pushed to it.
fn git(dir: &Path, args: &[&str]) -> Output {
    Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env_clear()
        .env("HOME", "/nonexistent") // never touch a real git identity/config
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .output()
        .unwrap()
}

/// A local bare repo standing in for the private GitHub backup repo
/// (README §Decisions A2 / AC4), plus a working clone with it as `origin`
/// — `pm backup` never clones or creates a remote itself (AC1: "push only
/// if a remote is configured"), so the test sets that up the way a human
/// bootstrapping a real target would.
struct RemoteBackup {
    _root: TempDir,
    bare: PathBuf,
    work: PathBuf,
}

impl RemoteBackup {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let bare = root.path().join("bare.git");
        let work = root.path().join("work");
        assert!(
            git(root.path(), &["init", "--quiet", "--bare", "bare.git"])
                .status
                .success()
        );
        std::fs::create_dir_all(&work).unwrap();
        assert!(git(&work, &["init", "--quiet"]).status.success());
        assert!(
            git(&work, &["remote", "add", "origin", bare.to_str().unwrap()])
                .status
                .success()
        );
        RemoteBackup {
            _root: root,
            bare,
            work,
        }
    }

    /// The bare repo's HEAD commit message, or `None` if nothing has been
    /// pushed yet.
    fn last_pushed_message(&self) -> Option<String> {
        let out = git(&self.bare, &["log", "-1", "--pretty=%s"]);
        out.status
            .success()
            .then(|| String::from_utf8(out.stdout).unwrap().trim().to_string())
    }
}

#[test]
fn backup_appends_only_new_ops_commits_and_pushes_when_a_remote_exists() {
    let sb = Sandbox::initialized();
    sb.new_ticket("first");
    sb.new_ticket("second");
    let remote = RemoteBackup::new();

    let first = json(&sb.pm(&["backup", "--to", remote.work.to_str().unwrap(), "--json"]));
    assert!(first["ops_appended"].as_u64().unwrap() > 0, "{first}");
    assert_eq!(first["committed"], true);
    assert_eq!(first["pushed"], true);
    assert!(remote.last_pushed_message().is_some(), "nothing was pushed");

    let jsonl = remote.work.join("ops/agt.jsonl");
    assert!(jsonl.is_file());
    let first_lines = std::fs::read_to_string(&jsonl).unwrap().lines().count();
    assert_eq!(first_lines as u64, first["ops_appended"].as_u64().unwrap());

    // A second backup with nothing new to say: no new ops, nothing staged,
    // still a healthy (committed: false) run — not an error.
    let second = json(&sb.pm(&["backup", "--to", remote.work.to_str().unwrap(), "--json"]));
    assert_eq!(second["ops_appended"], 0, "{second}");
    assert_eq!(second["committed"], false, "{second}");

    // A third ticket produces exactly one more backup's worth of ops,
    // appended (not rewritten) onto the same file.
    sb.new_ticket("third");
    let third = json(&sb.pm(&["backup", "--to", remote.work.to_str().unwrap(), "--json"]));
    assert!(third["ops_appended"].as_u64().unwrap() > 0, "{third}");
    assert_eq!(third["committed"], true);
    let total_lines = std::fs::read_to_string(&jsonl).unwrap().lines().count();
    assert_eq!(
        total_lines,
        first_lines + third["ops_appended"].as_u64().unwrap() as usize
    );
}

#[test]
fn restore_rebuilds_a_workspace_that_doctor_reports_clean_and_show_matches() {
    let sb = Sandbox::initialized();
    let id = sb.new_ticket("round trips");
    assert_ok(&sb.pm(&["set", &id, "priority=high"]));
    let before = json(&sb.pm(&["show", &id, "--json"]));

    let remote = RemoteBackup::new();
    assert_ok(&sb.pm(&["backup", "--to", remote.work.to_str().unwrap()]));

    // Restore into a *different* sandbox/home, proving restore needs
    // nothing from the original workspace beyond what's in the backup dir.
    let restored = Sandbox::new();
    let out = restored.run(
        &[
            "backup",
            "--restore",
            remote.work.to_str().unwrap(),
            "--workspace",
            restored.ws_str(),
            "--json",
        ],
        &[],
    );
    let report = json(&out);
    assert_eq!(report["prefix"], "AGT");
    assert!(report["ops_replayed"].as_u64().unwrap() > 0, "{report}");

    assert_ok(&restored.pm(&["doctor", "--workspace", restored.ws_str()]));

    let after = json(&restored.pm(&["show", &id, "--workspace", restored.ws_str(), "--json"]));
    assert_eq!(after["title"], before["title"]);
    assert_eq!(after["priority"], before["priority"]);
    assert_eq!(after["id"], before["id"]);
    assert_eq!(after["ulid"], before["ulid"]);

    // Restoring into a directory that is already a workspace refuses
    // rather than silently clobbering it.
    let clobber = restored.run(
        &[
            "backup",
            "--restore",
            remote.work.to_str().unwrap(),
            "--workspace",
            restored.ws_str(),
        ],
        &[],
    );
    assert_code(&clobber, 1);
}

/// AGT-1344: a project's design doc is a `body.edit` op targeting a
/// `doc_id` that lives alongside ticket ops in the same JSONL. Restore
/// must reassign that id before replaying, and replay the op through
/// `Store::commit_any` rather than the ticket-only `Store::commit`
/// (backup.rs's `ConfigSnapshot::project_doc_ids`/`named_doc_ids`) — this
/// proves both halves of that fix, not just that restore doesn't crash.
#[test]
fn restore_preserves_a_projects_design_doc_and_its_edit_history() {
    let sb = Sandbox::initialized();
    assert_ok(&sb.pm(&["project", "new", "proj", "--title", "Proj"]));

    let script = sb.home.path().join("editor.sh");
    std::fs::write(&script, "#!/bin/sh\necho 'design v1' > \"$1\"\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    assert_ok(&sb.run(
        &["project", "edit", "proj"],
        &[("EDITOR", script.to_str().unwrap())],
    ));
    let before = json(&sb.pm(&["project", "show", "proj", "--json"]));
    assert_eq!(before["doc"], "design v1\n");

    let remote = RemoteBackup::new();
    assert_ok(&sb.pm(&["backup", "--to", remote.work.to_str().unwrap()]));

    let restored = Sandbox::new();
    let out = restored.run(
        &[
            "backup",
            "--restore",
            remote.work.to_str().unwrap(),
            "--workspace",
            restored.ws_str(),
            "--json",
        ],
        &[],
    );
    assert_ok(&out);

    assert_ok(&restored.pm(&["doctor", "--workspace", restored.ws_str()]));
    let after = json(&restored.pm(&[
        "project",
        "show",
        "proj",
        "--workspace",
        restored.ws_str(),
        "--json",
    ]));
    assert_eq!(after["doc"], "design v1\n");

    // The restored doc_id still accepts further edits (proving it's a
    // real, continuing CRDT replica, not just a copied string).
    let script2 = restored.home.path().join("editor2.sh");
    std::fs::write(&script2, "#!/bin/sh\necho 'design v2' > \"$1\"\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script2, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    assert_ok(&restored.run(
        &["project", "edit", "proj", "--workspace", restored.ws_str()],
        &[("EDITOR", script2.to_str().unwrap())],
    ));
    let edited_again = json(&restored.pm(&[
        "project",
        "show",
        "proj",
        "--workspace",
        restored.ws_str(),
        "--json",
    ]));
    assert_eq!(edited_again["doc"], "design v2\n");
}

#[test]
fn status_is_unhealthy_before_the_first_backup_and_healthy_right_after() {
    let sb = Sandbox::initialized();
    sb.new_ticket("t");
    let remote = RemoteBackup::new();

    let before = sb.pm(&["backup", "status", "--to", remote.work.to_str().unwrap()]);
    assert_code(&before, 1);

    assert_ok(&sb.pm(&["backup", "--to", remote.work.to_str().unwrap()]));

    let after = json(&sb.pm(&[
        "backup",
        "status",
        "--to",
        remote.work.to_str().unwrap(),
        "--json",
    ]));
    assert_eq!(after["healthy"], true, "{after}");
    assert!(after["last_success"].is_string(), "{after}");
}

#[test]
fn install_timer_to_a_custom_dir_never_loads_into_the_real_launchd() {
    let sb = Sandbox::initialized();
    let timer_dir = tempfile::tempdir().unwrap();

    let out = sb.pm(&[
        "backup",
        "install-timer",
        "--dir",
        timer_dir.path().to_str().unwrap(),
        "--no-load",
        "--json",
    ]);
    let report = json(&out);
    assert_eq!(report["loaded"], false, "{report}");

    let plist_path = PathBuf::from(report["plist"].as_str().unwrap());
    assert!(plist_path.starts_with(timer_dir.path()));
    let plist = std::fs::read_to_string(&plist_path).unwrap();
    assert!(plist.contains("com.openthink.pm-backup"));
    assert!(plist.contains("<string>backup</string>"));
    assert!(plist.contains(sb.ws_str()));
    assert!(plist.contains(env!("CARGO_BIN_EXE_pm")));
    assert!(plist.contains("<integer>3600</integer>"));
}
