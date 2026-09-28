//! `pm backup` / `pm backup --restore` / `pm backup status` /
//! `pm backup install-timer` driven through the built binary (AGT-1350),
//! plus the sharded, base64 layout and the in-place migration of a
//! pre-AGT-1378 single-file target (AGT-1378).
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
    assert_ok(&sb.pm(&[
        "new",
        "--title",
        "first",
        "--description",
        "# First\n\nBody.",
    ]));
    sb.new_ticket("second");
    let remote = RemoteBackup::new();

    let first = json(&sb.pm(&["backup", "--to", remote.work.to_str().unwrap(), "--json"]));
    assert!(first["ops_appended"].as_u64().unwrap() > 0, "{first}");
    assert_eq!(first["committed"], true);
    assert_eq!(first["pushed"], true);
    assert!(remote.last_pushed_message().is_some(), "nothing was pushed");

    // AGT-1378: the log is sharded under ops/<prefix>/, and byte payloads
    // travel as base64 strings, never JSON arrays of integers.
    assert_eq!(first["shards"], 1, "{first}");
    assert_eq!(first["legacy_migrated"], Value::Null, "{first}");
    assert_eq!(first["warnings"], serde_json::json!([]), "{first}");
    assert!(!remote.work.join("ops/agt.jsonl").exists());
    let jsonl = remote.work.join("ops/agt/000001.jsonl");
    assert!(jsonl.is_file());
    let text = std::fs::read_to_string(&jsonl).unwrap();
    let first_lines = text.lines().count();
    assert_eq!(first_lines as u64, first["ops_appended"].as_u64().unwrap());
    let body_edits: Vec<Value> = text
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .filter(|op: &Value| op["kind"] == "body.edit")
        .collect();
    assert!(!body_edits.is_empty(), "{text}");
    for op in &body_edits {
        assert!(op["payload"]["update"].is_string(), "{op}");
    }

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

    // AGT-1378: the snapshot carries the doc's id but not its text — the
    // op log has it, and restore replays it back.
    let snapshot: Value = serde_json::from_str(
        &std::fs::read_to_string(remote.work.join("ops/agt.config.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(snapshot["projects"][0]["id"], "proj", "{snapshot}");
    assert_eq!(snapshot["projects"][0]["doc"], "", "{snapshot}");
    assert!(
        snapshot["project_doc_ids"]["proj"].is_string(),
        "{snapshot}"
    );

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

/// Turns a base64 byte payload at `key` back into the JSON array of
/// integers a pre-AGT-1378 binary wrote.
fn to_legacy_bytes(value: &mut Value, key: &str) {
    let encoded = value[key].as_str().expect("current form is a string");
    let bytes = pm_core::bytes::decode(encoded).unwrap();
    value[key] = Value::Array(bytes.into_iter().map(Value::from).collect());
}

/// Rewrites a freshly written target into the layout a pre-AGT-1378
/// binary produced: one `ops/<prefix>.jsonl` with byte payloads as
/// integer arrays, no shard directory, and every document body's text in
/// the config snapshot. Committed, so the target's git state is exactly
/// what the live backup repo looked like before the upgrade.
fn downgrade_target(work: &Path, doc_text: &str) {
    let shard_dir = work.join("ops/agt");
    let mut lines = Vec::new();
    let mut shards: Vec<PathBuf> = std::fs::read_dir(&shard_dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    shards.sort();
    for shard in shards {
        for line in std::fs::read_to_string(&shard).unwrap().lines() {
            let mut op: Value = serde_json::from_str(line).unwrap();
            if op["kind"] == "body.edit" {
                to_legacy_bytes(&mut op["payload"], "update");
            }
            lines.push(op.to_string());
        }
    }
    std::fs::remove_dir_all(&shard_dir).unwrap();
    std::fs::write(work.join("ops/agt.jsonl"), lines.join("\n") + "\n").unwrap();

    let config_path = work.join("ops/agt.config.json");
    let mut snapshot: Value =
        serde_json::from_str(&std::fs::read_to_string(&config_path).unwrap()).unwrap();
    snapshot["projects"][0]["doc"] = Value::String(doc_text.to_string());
    std::fs::write(
        &config_path,
        serde_json::to_string_pretty(&snapshot).unwrap(),
    )
    .unwrap();

    assert!(git(work, &["add", "-A", "--", "ops/"]).status.success());
    assert!(
        git(
            work,
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@localhost",
                "commit",
                "--quiet",
                "-m",
                "legacy layout"
            ]
        )
        .status
        .success()
    );
}

fn restore_into(remote: &RemoteBackup) -> Sandbox {
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
    restored
}

/// AGT-1378 AC1/AC3: a backup written before this ticket — one JSONL
/// file, byte arrays, full doc text in the snapshot — still restores; and
/// the first `pm backup` against it migrates it in place to shards
/// without losing, duplicating or reordering an op.
#[test]
fn a_legacy_single_file_backup_restores_and_is_migrated_to_shards_in_place() {
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
    assert_ok(&sb.pm(&[
        "new",
        "--title",
        "first",
        "--project",
        "proj",
        "--description",
        "# First\n\nA body with **markdown** and unicode: héllo — 你好.",
    ]));
    let first_show = json(&sb.pm(&["show", "AGT-1", "--json"]));

    let remote = RemoteBackup::new();
    let initial = json(&sb.pm(&["backup", "--to", remote.work.to_str().unwrap(), "--json"]));
    let initial_ops = initial["ops_appended"].as_u64().unwrap();
    downgrade_target(&remote.work, "design v1\n");
    let legacy = remote.work.join("ops/agt.jsonl");
    assert!(legacy.is_file());
    assert!(!remote.work.join("ops/agt").exists());
    assert!(
        std::fs::read_to_string(&legacy)
            .unwrap()
            .contains("\"update\":["),
        "the fixture must carry the legacy array spelling"
    );

    // (1) The legacy layout restores as-is.
    let from_legacy = restore_into(&remote);
    let show = json(&from_legacy.pm(&[
        "show",
        "AGT-1",
        "--workspace",
        from_legacy.ws_str(),
        "--json",
    ]));
    assert_eq!(show["description"], first_show["description"]);
    assert_eq!(show["title"], first_show["title"]);
    let proj = json(&from_legacy.pm(&[
        "project",
        "show",
        "proj",
        "--workspace",
        from_legacy.ws_str(),
        "--json",
    ]));
    assert_eq!(proj["doc"], "design v1\n");

    // (2) The next backup migrates the target in place and appends the
    // new ops after the migrated ones.
    sb.new_ticket("second");
    let migrated = json(&sb.pm(&["backup", "--to", remote.work.to_str().unwrap(), "--json"]));
    assert_eq!(migrated["legacy_migrated"], initial_ops, "{migrated}");
    assert!(migrated["ops_appended"].as_u64().unwrap() > 0, "{migrated}");
    assert_eq!(migrated["shards"], 1, "{migrated}");
    assert_eq!(migrated["committed"], true, "{migrated}");
    assert!(
        !legacy.exists(),
        "the legacy file is removed in the same commit"
    );
    let shard = remote.work.join("ops/agt/000001.jsonl");
    let text = std::fs::read_to_string(&shard).unwrap();
    assert_eq!(
        text.lines().count() as u64,
        initial_ops + migrated["ops_appended"].as_u64().unwrap(),
        "every legacy op once, then the new ones"
    );
    assert!(
        !text.contains("\"update\":["),
        "migrated lines are re-encoded"
    );
    let status = git(&remote.work, &["status", "--porcelain"]);
    assert_eq!(
        String::from_utf8(status.stdout).unwrap().trim(),
        "",
        "nothing left unstaged"
    );
    assert_eq!(
        remote.last_pushed_message().as_deref(),
        Some(
            format!(
                "pm backup: {} op(s) through seq {} (AGT)",
                migrated["ops_appended"], migrated["last_seq"]
            )
            .as_str()
        )
    );

    // (3) And the migrated target restores to the same tickets plus the
    // new one; a third backup is a plain no-op append.
    let from_shards = restore_into(&remote);
    let show = json(&from_shards.pm(&[
        "show",
        "AGT-1",
        "--workspace",
        from_shards.ws_str(),
        "--json",
    ]));
    assert_eq!(show["description"], first_show["description"]);
    let second = json(&from_shards.pm(&[
        "show",
        "AGT-2",
        "--workspace",
        from_shards.ws_str(),
        "--json",
    ]));
    assert_eq!(second["title"], "second");
    let again = json(&sb.pm(&["backup", "--to", remote.work.to_str().unwrap(), "--json"]));
    assert_eq!(again["ops_appended"], 0, "{again}");
    assert_eq!(again["legacy_migrated"], Value::Null, "{again}");
}

/// AGT-1378 AC3: any single file over 50 MB is called out by `pm backup
/// status` (and by `pm backup`), without turning a fresh backup
/// unhealthy.
#[test]
fn status_lists_files_and_warns_above_50_mb() {
    let sb = Sandbox::initialized();
    sb.new_ticket("t");
    let remote = RemoteBackup::new();
    assert_ok(&sb.pm(&["backup", "--to", remote.work.to_str().unwrap()]));

    let clean = json(&sb.pm(&[
        "backup",
        "status",
        "--to",
        remote.work.to_str().unwrap(),
        "--json",
    ]));
    let paths: Vec<&str> = clean["files"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["path"].as_str().unwrap())
        .collect();
    assert_eq!(
        paths,
        ["ops/agt.config.json", "ops/agt/000001.jsonl"],
        "{clean}"
    );
    assert_eq!(clean["warnings"], serde_json::json!([]), "{clean}");

    // A shard that has somehow grown past the threshold.
    let big = remote.work.join("ops/agt/000002.jsonl");
    std::fs::write(&big, vec![b'x'; 51 * 1024 * 1024]).unwrap();
    let out = sb.pm(&[
        "backup",
        "status",
        "--to",
        remote.work.to_str().unwrap(),
        "--json",
    ]);
    let warned = json(&out);
    assert_eq!(warned["healthy"], true, "{warned}");
    let warnings = warned["warnings"].as_array().unwrap();
    assert_eq!(warnings.len(), 1, "{warned}");
    let warning = warnings[0].as_str().unwrap();
    assert!(
        warning.starts_with("ops/agt/000002.jsonl is 51.0 MB, over the 50 MB"),
        "{warning}"
    );
    assert!(warning.contains("100 MB"), "{warning}");

    let human = sb.pm(&["backup", "status", "--to", remote.work.to_str().unwrap()]);
    assert_ok(&human);
    assert!(
        stdout(&human).contains("warning:      ops/agt/000002.jsonl is 51.0 MB"),
        "{}",
        stdout(&human)
    );
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
