//! `pm backup` (AGT-1350, projects/pm/README.md §Constraints: "Durability
//! before the hub exists: `pm backup` on a launchd timer (op-log JSONL
//! export to a private git repo). No state may exist only in one SQLite
//! file for more than a day.").
//!
//! Four verbs:
//! - `pm backup [--to <dir>]` appends ops committed since the last backup
//!   to `<dir>/ops/<prefix>.jsonl` (one `pm_core::Op` per line, in `seq`
//!   order), rewrites `<dir>/ops/<prefix>.config.json` (the workspace +
//!   project snapshot a restore needs — config tables are not op-logged,
//!   README §Op log), commits, and pushes if `<dir>` has a remote.
//! - `pm backup --restore <dir>` reads those two files and rebuilds a
//!   workspace at the resolved `--workspace` directory: `init_workspace` +
//!   `put_project` from the config snapshot, then `Store::commit` over
//!   every op in the JSONL, in file order (append order = `seq` order).
//! - `pm backup install-timer` writes an hourly launchd job that runs `pm
//!   backup --workspace <resolved dir>` with the current binary's absolute
//!   path.
//! - `pm backup status` reports the last successful backup's age and
//!   exits 1 if it is missing or more than 24h old.
//!
//! Backup progress (the high-water op `seq` sent to each target, and the
//! last success time) lives in the *source* workspace's own database
//! (`pm_store::Store::backup_*`), keyed by the target directory's absolute
//! path — not in the backup git repo itself, so `pm backup status` never
//! needs to touch git.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::Context;
use pm_core::{Op, Project, Workspace};
use pm_store::Store;
use serde::{Deserialize, Serialize};
use serde_json::json;
use ulid::Ulid;

use crate::exit::{CliError, Result};
use crate::verbs::{Ctx, SCHEMA, print_json};
use crate::workspace::{self, DB_FILE};

/// On-disk shape of `<dir>/ops/<prefix>.config.json` — versioned
/// separately from the CLI's `--json` contract ([`SCHEMA`]), since this
/// file is a durable artifact another `pm` binary reads back, not a
/// single command's stdout.
const SNAPSHOT_SCHEMA: u32 = 1;

#[derive(Debug, Serialize, Deserialize)]
struct ConfigSnapshot {
    schema: u32,
    workspace: Workspace,
    projects: Vec<Project>,
    /// A project's design-doc id, by project id (AGT-1344): present only
    /// for a project created through `pm project new`, whose design doc's
    /// `body.edit` ops in the JSONL target it. Restore must reassign the
    /// same id to the recreated row *before* those ops replay
    /// ([`Store::commit_any`]'s only way to recognize them as document
    /// edits, not ticket ops) — `#[serde(default)]` so a backup written
    /// before this ticket restores fine, just without any doc history.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    project_doc_ids: BTreeMap<String, Ulid>,
    /// A named document's id, by project id then document name.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    named_doc_ids: BTreeMap<String, BTreeMap<String, Ulid>>,
}

/// The absolute form of `path`, without requiring it (or any ancestor) to
/// exist — `fs::canonicalize` needs the path to exist, but a backup
/// target's high-water mark must be keyable before its directory is ever
/// created, and consistently after.
fn absolute(path: &Path) -> Result<PathBuf> {
    std::path::absolute(path)
        .with_context(|| format!("resolving {}", path.display()))
        .map_err(Into::into)
}

/// `--to <dir>`, else `backup.repo` from `~/.config/pm/config.toml`.
fn resolve_target(to: Option<PathBuf>, env: &workspace::Env) -> Result<PathBuf> {
    if let Some(dir) = to {
        return Ok(dir);
    }
    let config_path = env.config_path()?;
    match workspace::Config::load(&config_path)?
        .and_then(|c| c.backup)
        .and_then(|b| b.repo)
    {
        Some(dir) => Ok(dir),
        None => Err(CliError::usage(format!(
            "no backup target: pass --to <dir> or set `backup.repo` in {}",
            config_path.display()
        ))),
    }
}

fn run_git(dir: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .with_context(|| format!("running git -C {} {}", dir.display(), args.join(" ")))?;
    if !output.status.success() {
        return Err(CliError::error(format!(
            "git -C {} {} failed: {}",
            dir.display(),
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// `true` if `git <args>` exits zero, without treating a non-zero exit as
/// an error (used for the plain boolean checks `diff --cached --quiet`
/// and `rev-parse --verify` want).
fn git_ok(dir: &Path, args: &[&str]) -> Result<bool> {
    let status = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .with_context(|| format!("running git -C {} {}", dir.display(), args.join(" ")))?;
    Ok(status.success())
}

/// Initializes `dir` as a git repository if it is not one already. Never
/// touches remotes — a target with no `origin` just never gets pushed to
/// (AC1: "push only if a remote is configured"), which is exactly how the
/// tests and smoke run this against a bare local "remote" without hitting
/// the network or a real GitHub repo.
fn ensure_git_dir(dir: &Path) -> Result<()> {
    fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    if !dir.join(".git").is_dir() {
        run_git(dir, &["init", "--quiet"])?;
    }
    Ok(())
}

// ------------------------------------------------------------- pm backup

/// `pm backup [--to <dir>]` (AC1).
pub fn run(ctx: &Ctx<'_>, to: Option<PathBuf>) -> Result<()> {
    let (mut store, ws) = workspace::open(&workspace::resolve(ctx.workspace, ctx.env)?)?;

    let dir = absolute(&resolve_target(to, ctx.env)?)?;
    ensure_git_dir(&dir)?;
    let target = dir.to_string_lossy().into_owned();

    let ops_dir = dir.join("ops");
    fs::create_dir_all(&ops_dir).with_context(|| format!("creating {}", ops_dir.display()))?;
    let stem = ws.prefix.to_ascii_lowercase();
    let jsonl_name = format!("{stem}.jsonl");
    let config_name = format!("{stem}.config.json");
    let jsonl_path = ops_dir.join(&jsonl_name);
    let config_path = ops_dir.join(&config_name);

    let last_seq = store.backup_last_seq(&target)?;
    let new_ops = store.ops_since(last_seq)?;
    let through_seq = new_ops.last().map_or(last_seq, |(seq, _)| *seq);

    if !new_ops.is_empty() {
        let mut lines = String::new();
        for (_, op) in &new_ops {
            lines.push_str(&serde_json::to_string(op).expect("an op serializes"));
            lines.push('\n');
        }
        let mut file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&jsonl_path)
            .with_context(|| format!("opening {}", jsonl_path.display()))?;
        file.write_all(lines.as_bytes())
            .with_context(|| format!("writing {}", jsonl_path.display()))?;
    }

    // Rewritten every backup, whether or not there were new ops: config
    // tables (workspace settings, project docs) are not op-logged
    // (README §Op log), so this snapshot is the only way `--restore`
    // learns them, and it changes independently of the op count.
    let projects = store.projects()?;
    let mut project_doc_ids = BTreeMap::new();
    let mut named_doc_ids: BTreeMap<String, BTreeMap<String, Ulid>> = BTreeMap::new();
    for project in &projects {
        if let Some(doc_id) = store.design_doc_id(&project.id)? {
            project_doc_ids.insert(project.id.clone(), doc_id);
        }
        let mut names = BTreeMap::new();
        for name in project.documents.keys() {
            if let Some(doc_id) = store.named_doc_id(&project.id, name)? {
                names.insert(name.clone(), doc_id);
            }
        }
        if !names.is_empty() {
            named_doc_ids.insert(project.id.clone(), names);
        }
    }
    let snapshot = ConfigSnapshot {
        schema: SNAPSHOT_SCHEMA,
        workspace: ws.clone(),
        projects,
        project_doc_ids,
        named_doc_ids,
    };
    fs::write(
        &config_path,
        serde_json::to_string_pretty(&snapshot).expect("a snapshot serializes"),
    )
    .with_context(|| format!("writing {}", config_path.display()))?;

    run_git(
        &dir,
        &[
            "add",
            "--",
            &format!("ops/{jsonl_name}"),
            &format!("ops/{config_name}"),
        ],
    )?;
    let has_staged = !git_ok(&dir, &["diff", "--cached", "--quiet"])?;
    let mut committed = false;
    if has_staged {
        let message = format!(
            "pm backup: {} op(s) through seq {through_seq} ({})",
            new_ops.len(),
            ws.prefix
        );
        run_git(
            &dir,
            &[
                "-c",
                "user.name=pm backup",
                "-c",
                "user.email=pm-backup@localhost",
                "commit",
                "--quiet",
                "-m",
                &message,
            ],
        )?;
        committed = true;
    }

    let mut pushed = false;
    let mut remote: Option<String> = None;
    if git_ok(&dir, &["rev-parse", "--verify", "--quiet", "HEAD"])? {
        let remotes = run_git(&dir, &["remote"])?;
        if let Some(name) = remotes.lines().next() {
            let branch = run_git(&dir, &["rev-parse", "--abbrev-ref", "HEAD"])?;
            run_git(
                &dir,
                &["push", "--quiet", name, &format!("{branch}:{branch}")],
            )?;
            pushed = true;
            remote = Some(name.to_string());
        }
    }

    store.backup_record(&target, through_seq)?;

    if ctx.json {
        print_json(&json!({
            "schema": SCHEMA,
            "target": target,
            "ops_appended": new_ops.len(),
            "last_seq": through_seq,
            "committed": committed,
            "pushed": pushed,
            "remote": remote,
        }));
    } else {
        println!(
            "backed up {} to {target}: {} new op(s), through seq {through_seq}",
            ws.prefix,
            new_ops.len()
        );
        println!(
            "  committed: {committed}, pushed: {}",
            match &remote {
                Some(name) if pushed => format!("yes ({name})"),
                _ => "no (no remote configured)".to_string(),
            }
        );
    }
    Ok(())
}

// --------------------------------------------------------- pm backup --restore

/// The one `ops/*.jsonl` file under `dir` and its sibling
/// `*.config.json`. `pm backup` writes one workspace's pair per target
/// today, so "exactly one" is the expected case; more than one names them
/// so a future `--prefix` flag has something to disambiguate.
fn find_backup_files(dir: &Path) -> Result<(PathBuf, PathBuf)> {
    let ops_dir = dir.join("ops");
    let mut jsonl_files: Vec<PathBuf> = fs::read_dir(&ops_dir)
        .with_context(|| format!("reading {}", ops_dir.display()))?
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|p| p.extension().is_some_and(|e| e == "jsonl"))
        .collect();
    jsonl_files.sort();
    match jsonl_files.as_slice() {
        [] => Err(CliError::not_found(format!(
            "no ops/*.jsonl found under {}",
            dir.display()
        ))),
        [jsonl] => {
            let stem = jsonl.file_stem().and_then(|s| s.to_str()).unwrap_or("");
            let config = ops_dir.join(format!("{stem}.config.json"));
            if !config.is_file() {
                return Err(CliError::not_found(format!(
                    "{} not found alongside {}",
                    config.display(),
                    jsonl.display()
                )));
            }
            Ok((jsonl.clone(), config))
        }
        many => {
            let names: Vec<&str> = many
                .iter()
                .filter_map(|p| p.file_stem().and_then(|s| s.to_str()))
                .collect();
            Err(CliError::usage(format!(
                "{} backs up more than one workspace ({}); restore does not \
                 disambiguate between them yet",
                ops_dir.display(),
                names.join(", ")
            )))
        }
    }
}

/// Projects in an order `Store::put_project` accepts: a project's parent
/// (R2's project-existence check applies to `project.parent` too) always
/// lands before it.
fn topo_sorted(mut projects: Vec<Project>) -> Result<Vec<Project>> {
    let mut sorted = Vec::with_capacity(projects.len());
    let mut placed: BTreeSet<String> = BTreeSet::new();
    while !projects.is_empty() {
        let before = projects.len();
        let mut remaining = Vec::new();
        for project in projects {
            let ready = project
                .parent
                .as_ref()
                .is_none_or(|parent| placed.contains(parent));
            if ready {
                placed.insert(project.id.clone());
                sorted.push(project);
            } else {
                remaining.push(project);
            }
        }
        if remaining.len() == before {
            return Err(CliError::error(
                "backup config snapshot has a project with a missing or cyclic parent",
            ));
        }
        projects = remaining;
    }
    Ok(sorted)
}

/// `pm backup --restore <dir>` (AC2): rebuilds a workspace at the
/// resolved `--workspace` directory. Refuses to touch a directory that is
/// already a workspace.
pub fn restore(ctx: &Ctx<'_>, dir: &Path) -> Result<()> {
    let dir = absolute(dir)?;
    let (jsonl_path, config_path) = find_backup_files(&dir)?;

    let snapshot: ConfigSnapshot = {
        let text = fs::read_to_string(&config_path)
            .with_context(|| format!("reading {}", config_path.display()))?;
        serde_json::from_str(&text).with_context(|| format!("parsing {}", config_path.display()))?
    };

    let ws_dir = workspace::resolve(ctx.workspace, ctx.env)?;
    fs::create_dir_all(&ws_dir).with_context(|| format!("creating {}", ws_dir.display()))?;
    let db_path = ws_dir.join(DB_FILE);
    if db_path.is_file() {
        return Err(CliError::error(format!(
            "{} is already a pm workspace; restore refuses to overwrite it \
             (point --workspace at an empty directory)",
            ws_dir.display()
        )));
    }

    let mut store = Store::open(&db_path)?;
    store.init_workspace(&snapshot.workspace)?;
    for project in topo_sorted(snapshot.projects)? {
        store.put_project(&project)?;
    }
    // Reassign each document's original doc_id before any op replays
    // (AGT-1344): a `body.edit` in the JSONL below targets it, and
    // `Store::commit_any` only recognizes a document edit by finding its
    // doc_id already on a `project`/`project_doc` row.
    for (project, doc_id) in &snapshot.project_doc_ids {
        store.set_design_doc_id(project, *doc_id)?;
    }
    for (project, names) in &snapshot.named_doc_ids {
        for (name, doc_id) in names {
            store.set_named_doc_id(project, name, *doc_id)?;
        }
    }

    let text = fs::read_to_string(&jsonl_path)
        .with_context(|| format!("reading {}", jsonl_path.display()))?;
    let mut replayed: u64 = 0;
    for (lineno, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let op: Op = serde_json::from_str(line)
            .with_context(|| format!("parsing {}:{}", jsonl_path.display(), lineno + 1))?;
        // A document's body.edit (AGT-1344) and a ticket op both live in
        // this one log; commit_any tells them apart by entity.
        store.commit_any(&op)?;
        replayed += 1;
    }

    if ctx.json {
        print_json(&json!({
            "schema": SCHEMA,
            "workspace": ws_dir,
            "prefix": snapshot.workspace.prefix,
            "ops_replayed": replayed,
        }));
    } else {
        println!(
            "restored {} into {}: {replayed} op(s) replayed",
            snapshot.workspace.prefix,
            ws_dir.display()
        );
    }
    Ok(())
}

// ------------------------------------------------------------- pm backup status

const STALE_AFTER_SECONDS: f64 = 24.0 * 60.0 * 60.0;

/// `pm backup status [--to <dir>]` (AC3): exits 1 when the target has
/// never backed up successfully, or its last success is older than 24h.
pub fn status(ctx: &Ctx<'_>, to: Option<PathBuf>) -> Result<()> {
    let (store, _ws) = workspace::open(&workspace::resolve(ctx.workspace, ctx.env)?)?;
    let target = absolute(&resolve_target(to, ctx.env)?)?
        .to_string_lossy()
        .into_owned();
    let found = store.backup_status(&target)?;
    let healthy = found
        .as_ref()
        .and_then(|s| s.age_seconds)
        .is_some_and(|age| age < STALE_AFTER_SECONDS);

    if ctx.json {
        print_json(&json!({
            "schema": SCHEMA,
            "target": target,
            "last_seq": found.as_ref().map_or(0, |s| s.last_seq),
            "last_success": found.as_ref().and_then(|s| s.last_success.clone()),
            "age_seconds": found.as_ref().and_then(|s| s.age_seconds),
            "healthy": healthy,
        }));
    } else {
        println!("target:       {target}");
        match &found {
            Some(s) => {
                println!("last seq:     {}", s.last_seq);
                println!(
                    "last success: {}",
                    s.last_success.as_deref().unwrap_or("never")
                );
                if let Some(age) = s.age_seconds {
                    println!("age:          {age:.0}s");
                }
            }
            None => println!("last success: never"),
        }
        println!("healthy:      {healthy}");
    }

    if healthy {
        Ok(())
    } else {
        Err(CliError::error(
            "backup is unhealthy: it has never succeeded, or not in the last 24h",
        ))
    }
}

// ------------------------------------------------------- pm backup install-timer

/// XML-escapes the handful of characters plist string values can contain
/// (a filesystem path here — never `<`/`&`/`"` in practice, but a plist
/// with an unescaped one is silently invalid rather than a build error).
fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn render_plist(label: &str, pm_bin: &Path, ws_dir: &Path, log_path: &Path) -> String {
    let label = xml_escape(label);
    let pm_bin = xml_escape(&pm_bin.display().to_string());
    let ws_dir = xml_escape(&ws_dir.display().to_string());
    let log = xml_escape(&log_path.display().to_string());
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{label}</string>
    <key>ProgramArguments</key>
    <array>
        <string>{pm_bin}</string>
        <string>backup</string>
        <string>--workspace</string>
        <string>{ws_dir}</string>
    </array>
    <key>StartInterval</key>
    <integer>3600</integer>
    <key>RunAtLoad</key>
    <false/>
    <key>StandardOutPath</key>
    <string>{log}</string>
    <key>StandardErrorPath</key>
    <string>{log}</string>
</dict>
</plist>
"#
    )
}

/// `pm backup install-timer [--dir <dir>] [--no-load]` (AC3). Writing to a
/// custom `--dir` never loads the job into the real launchd, regardless of
/// `--no-load` — the one way this command is safe to run in tests and
/// smoke without touching `~/Library/LaunchAgents` or a live agent.
pub fn install_timer(ctx: &Ctx<'_>, dir: Option<PathBuf>, no_load: bool) -> Result<()> {
    let ws_dir = absolute(&workspace::resolve(ctx.workspace, ctx.env)?)?;
    // Fail early (and clearly) if this workspace does not actually exist,
    // rather than installing a timer that will just error hourly.
    workspace::open(&ws_dir)?;

    let pm_bin =
        absolute(&std::env::current_exe().context("resolving the current pm binary's path")?)?;

    let (plist_dir, log_path) = match &dir {
        Some(custom) => (custom.clone(), custom.join("pm-backup.log")),
        None => {
            let home = ctx.env.home.clone().ok_or_else(|| {
                CliError::error("cannot locate ~/Library/LaunchAgents: HOME is not set")
            })?;
            (
                home.join("Library").join("LaunchAgents"),
                home.join("Library").join("Logs").join("pm-backup.log"),
            )
        }
    };
    fs::create_dir_all(&plist_dir).with_context(|| format!("creating {}", plist_dir.display()))?;
    if let Some(log_dir) = log_path.parent() {
        fs::create_dir_all(log_dir).with_context(|| format!("creating {}", log_dir.display()))?;
    }

    const LABEL: &str = "com.openthink.pm-backup";
    let plist_path = plist_dir.join(format!("{LABEL}.plist"));
    fs::write(
        &plist_path,
        render_plist(LABEL, &pm_bin, &ws_dir, &log_path),
    )
    .with_context(|| format!("writing {}", plist_path.display()))?;

    // Never load a plist written to a custom directory — only the default,
    // real LaunchAgents path is ever a candidate, and only when the caller
    // did not ask us not to.
    let should_load = dir.is_none() && !no_load;
    if should_load {
        let output = Command::new("launchctl")
            .args(["load", "-w"])
            .arg(&plist_path)
            .output()
            .context("running launchctl load")?;
        if !output.status.success() {
            return Err(CliError::error(format!(
                "launchctl load -w {} failed: {}",
                plist_path.display(),
                String::from_utf8_lossy(&output.stderr).trim()
            )));
        }
    }

    if ctx.json {
        print_json(&json!({
            "schema": SCHEMA,
            "plist": plist_path,
            "workspace": ws_dir,
            "pm_bin": pm_bin,
            "loaded": should_load,
        }));
    } else {
        println!("wrote {}", plist_path.display());
        println!("workspace: {}, pm: {}", ws_dir.display(), pm_bin.display());
        println!(
            "loaded: {should_load}{}",
            if should_load { "" } else { " (not loaded)" }
        );
    }
    Ok(())
}
