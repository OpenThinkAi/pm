//! `pm backup` (AGT-1350, projects/pm/README.md §Constraints: "Durability
//! before the hub exists: `pm backup` on a launchd timer (op-log JSONL
//! export to a private git repo). No state may exist only in one SQLite
//! file for more than a day.").
//!
//! Four verbs:
//! - `pm backup [--to <dir>]` appends ops committed since the last backup
//!   to `<dir>/ops/<prefix>/<NNNNNN>.jsonl` (one `pm_core::Op` per line,
//!   in `seq` order, sharded — see "Layout" below), rewrites
//!   `<dir>/ops/<prefix>.config.json` (the workspace + project snapshot),
//!   commits, and pushes if `<dir>` has a remote. Since AGT-1385 the
//!   workspace's config and project metadata are ops in the log too; the
//!   snapshot still carries what is not — document ids, the text of any
//!   document written outside the log — and lets a backup taken before
//!   that ticket restore.
//! - `pm backup --restore <dir>` reads those files and rebuilds a
//!   workspace at the resolved `--workspace` directory: first every
//!   config op in the JSONL (the states and projects everything else
//!   needs — a migrated log carries them *after* the ticket ops they
//!   configure, since migration 0007 appended them), then
//!   `init_workspace` + `put_project` from the snapshot (which commit
//!   nothing the log already said, and everything for a pre-AGT-1385
//!   backup), then `Store::commit_any` over every other op, in file order
//!   (append order = `seq` order). A project the log created but the
//!   snapshot no longer lists was deleted before the backup, and is
//!   deleted again.
//! - `pm backup install-timer` writes an hourly launchd job that runs `pm
//!   backup --workspace <resolved dir>` with the current binary's absolute
//!   path.
//! - `pm backup status` reports the last successful backup's age and
//!   exits 1 if it is missing or more than 24h old; it also lists every
//!   file of the target's layout with its size and warns about any over
//!   [`WARN_FILE_BYTES`].
//!
//! Backup progress (the high-water op `seq` sent to each target, and the
//! last success time) lives in the *source* workspace's own database
//! (`pm_store::Store::backup_*`), keyed by the target directory's absolute
//! path — not in the backup git repo itself, so `pm backup status` never
//! needs to touch git.
//!
//! # Layout (AGT-1378)
//!
//! GitHub rejects any file over 100 MB, and the first live backup of the
//! vault wrote an 87 MB `ops/agt.jsonl` in one go (Loro update bytes as
//! JSON arrays of integers), so an append-only single file was one modest
//! month from failing every hour. Two changes keep every file small:
//!
//! - Byte payloads are base64 (`pm_core::bytes`, ~1.33× instead of
//!   ~3.6×). A reader accepts both spellings, so an old backup restores.
//! - The log is **sharded by size**, in `seq` order: ops append to
//!   `ops/<prefix>/<NNNNNN>.jsonl`, and once a shard would grow past
//!   [`SHARD_ROTATE_BYTES`] the next op starts `<NNNNNN+1>.jsonl`. Only
//!   the newest shard is ever appended to; older ones never change again.
//!   Shards are cut by size rather than by HLC month because restore must
//!   replay in `seq` order and `pm import vault` back-dates HLCs to the
//!   vault's own timestamps — a month-keyed layout could put a ticket's
//!   `relation.add` in an earlier file than the `ticket.create` it needs.
//!   A single op larger than the threshold gets a shard to itself, so a
//!   shard is never larger than the threshold plus one line.
//! - The config snapshot omits the text of any document body the op log
//!   already carries (a doc with a `doc_id` whose cached text matches its
//!   replayed view): restore refills it from the `body.edit` ops, and the
//!   snapshot stops being a second copy of every project document (the
//!   live vault's was 20 MB, almost all one 17 MB document).
//!
//! A target written by a pre-AGT-1378 binary has a single
//! `ops/<prefix>.jsonl`. The first `pm backup` against it **migrates it
//! in place**: every line is re-read (either spelling), rewritten as
//! shards, and the legacy file is removed in the same commit — the ops
//! and their order are unchanged, so the target's high-water mark stays
//! valid and new ops simply append to the last shard. `--restore` reads
//! either layout (the legacy file, when present, wins: it is the source an
//! interrupted migration's shards were derived from).

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
/// single command's stdout. Still `1` after AGT-1378: the shape is
/// unchanged, only op-backed document bodies are now written empty.
const SNAPSHOT_SCHEMA: u32 = 1;

/// A shard that would grow past this many bytes with the next op is
/// closed and the op starts the next one. Well under
/// [`WARN_FILE_BYTES`] even after one oversized line.
pub const SHARD_ROTATE_BYTES: u64 = 16 * 1024 * 1024;

/// `pm backup status` (and `pm backup`) warn about any single file over
/// this: half of GitHub's 100 MB hard limit.
pub const WARN_FILE_BYTES: u64 = 50 * 1024 * 1024;

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

// ---------------------------------------------------------------- layout

/// Where one workspace's files live under a target directory.
struct Layout {
    /// `ops/<stem>.config.json`.
    config: PathBuf,
    /// `ops/<stem>.jsonl`: the pre-AGT-1378 single log file, if present.
    legacy: PathBuf,
    /// `ops/<stem>/`, holding `<NNNNNN>.jsonl` shards.
    shard_dir: PathBuf,
}

impl Layout {
    fn new(dir: &Path, stem: &str) -> Self {
        let ops_dir = dir.join("ops");
        Layout {
            config: ops_dir.join(format!("{stem}.config.json")),
            legacy: ops_dir.join(format!("{stem}.jsonl")),
            shard_dir: ops_dir.join(stem),
        }
    }

    /// Every existing shard, in order.
    fn shards(&self) -> Result<Vec<(u32, PathBuf)>> {
        list_shards(&self.shard_dir)
    }

    /// The files a restore replays, in replay order: the legacy log when
    /// it exists (see the module doc), else the shards.
    fn op_files(&self) -> Result<Vec<PathBuf>> {
        if self.legacy.is_file() {
            return Ok(vec![self.legacy.clone()]);
        }
        Ok(self.shards()?.into_iter().map(|(_, p)| p).collect())
    }

    /// Every file of this layout that exists, as `(path relative to the
    /// target, size in bytes)` — what `pm backup status` reports on.
    fn files(&self, dir: &Path) -> Result<Vec<(String, u64)>> {
        let mut out = Vec::new();
        let mut push = |path: &Path| -> Result<()> {
            if let Ok(meta) = fs::metadata(path) {
                let rel = path
                    .strip_prefix(dir)
                    .unwrap_or(path)
                    .to_string_lossy()
                    .into_owned();
                out.push((rel, meta.len()));
            }
            Ok(())
        };
        push(&self.config)?;
        push(&self.legacy)?;
        for (_, shard) in self.shards()? {
            push(&shard)?;
        }
        Ok(out)
    }
}

fn shard_name(n: u32) -> String {
    format!("{n:06}.jsonl")
}

fn list_shards(shard_dir: &Path) -> Result<Vec<(u32, PathBuf)>> {
    if !shard_dir.is_dir() {
        return Ok(Vec::new());
    }
    let mut shards: Vec<(u32, PathBuf)> = fs::read_dir(shard_dir)
        .with_context(|| format!("reading {}", shard_dir.display()))?
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter_map(|path| {
            let name = path.file_name()?.to_str()?;
            let n: u32 = name.strip_suffix(".jsonl")?.parse().ok()?;
            (shard_name(n) == name).then_some((n, path))
        })
        .collect();
    shards.sort();
    Ok(shards)
}

/// Appends lines to the newest shard under a directory, cutting a new
/// shard whenever the current one would grow past `rotate_at` bytes.
/// Lines are buffered per shard and written on rotation and on
/// [`ShardWriter::finish`].
struct ShardWriter {
    dir: PathBuf,
    rotate_at: u64,
    /// The shard being written, its size on disk plus what is buffered.
    current: u32,
    size: u64,
    buffer: String,
}

impl ShardWriter {
    /// Continues the newest existing shard under `dir` (or starts
    /// `000001.jsonl`).
    fn open(dir: &Path, rotate_at: u64) -> Result<Self> {
        fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        let (current, size) = match list_shards(dir)?.last() {
            Some((n, path)) => (
                *n,
                fs::metadata(path)
                    .with_context(|| format!("reading {}", path.display()))?
                    .len(),
            ),
            None => (1, 0),
        };
        Ok(ShardWriter {
            dir: dir.to_path_buf(),
            rotate_at,
            current,
            size,
            buffer: String::new(),
        })
    }

    fn path(&self) -> PathBuf {
        self.dir.join(shard_name(self.current))
    }

    fn append(&mut self, line: &str) -> Result<()> {
        let bytes = line.len() as u64 + 1;
        if self.size > 0 && self.size + bytes > self.rotate_at {
            self.flush()?;
            self.current += 1;
            self.size = 0;
        }
        self.buffer.push_str(line);
        self.buffer.push('\n');
        self.size += bytes;
        Ok(())
    }

    fn flush(&mut self) -> Result<()> {
        if self.buffer.is_empty() {
            return Ok(());
        }
        let path = self.path();
        let mut file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .with_context(|| format!("opening {}", path.display()))?;
        file.write_all(self.buffer.as_bytes())
            .with_context(|| format!("writing {}", path.display()))?;
        self.buffer.clear();
        Ok(())
    }

    /// Writes what is buffered; returns the number of the newest shard.
    fn finish(mut self) -> Result<u32> {
        self.flush()?;
        Ok(self.current)
    }
}

/// Reads one JSONL log file's ops, in file order; `lineno` in errors is
/// 1-based.
fn read_jsonl(path: &Path) -> Result<Vec<Op>> {
    let text = fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let mut ops = Vec::new();
    for (lineno, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let op: Op = serde_json::from_str(line)
            .with_context(|| format!("parsing {}:{}", path.display(), lineno + 1))?;
        ops.push(op);
    }
    Ok(ops)
}

/// Rewrites a pre-AGT-1378 `ops/<stem>.jsonl` as shards under
/// `layout.shard_dir` and removes it. Any shards already there are from
/// an earlier, interrupted migration of the same file (a completed one
/// removes the legacy file) and are rebuilt from scratch. Returns the
/// number of ops carried over.
fn migrate_legacy(layout: &Layout) -> Result<usize> {
    let ops = read_jsonl(&layout.legacy)?;
    for (_, stale) in layout.shards()? {
        fs::remove_file(&stale).with_context(|| format!("removing {}", stale.display()))?;
    }
    let mut writer = ShardWriter::open(&layout.shard_dir, SHARD_ROTATE_BYTES)?;
    for op in &ops {
        writer.append(&serde_json::to_string(op).expect("an op serializes"))?;
    }
    writer.finish()?;
    fs::remove_file(&layout.legacy)
        .with_context(|| format!("removing {}", layout.legacy.display()))?;
    Ok(ops.len())
}

fn megabytes(bytes: u64) -> f64 {
    bytes as f64 / (1024.0 * 1024.0)
}

/// One warning per file over [`WARN_FILE_BYTES`].
fn size_warnings(files: &[(String, u64)]) -> Vec<String> {
    files
        .iter()
        .filter(|(_, bytes)| *bytes > WARN_FILE_BYTES)
        .map(|(path, bytes)| {
            format!(
                "{path} is {:.1} MB, over the {:.0} MB warning threshold (GitHub rejects files over 100 MB)",
                megabytes(*bytes),
                megabytes(WARN_FILE_BYTES)
            )
        })
        .collect()
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
    let layout = Layout::new(&dir, &ws.prefix.to_ascii_lowercase());

    // A target last written by a pre-AGT-1378 binary: same ops, sharded.
    let legacy_migrated = if layout.legacy.is_file() {
        Some(migrate_legacy(&layout)?)
    } else {
        None
    };

    let last_seq = store.backup_last_seq(&target)?;
    let new_ops = store.ops_since(last_seq)?;
    let through_seq = new_ops.last().map_or(last_seq, |(seq, _)| *seq);

    let mut writer = ShardWriter::open(&layout.shard_dir, SHARD_ROTATE_BYTES)?;
    for (_, op) in &new_ops {
        writer.append(&serde_json::to_string(op).expect("an op serializes"))?;
    }
    let shards = writer.finish()?;

    // Rewritten every backup, whether or not there were new ops: what
    // the snapshot carries beyond the (now op-logged, AGT-1385) config —
    // document ids, the text of any document written outside the log —
    // changes independently of the op count, and a restore of a backup
    // whose log predates config ops learns its workspace from it.
    let snapshot = config_snapshot(&store, &ws)?;
    fs::write(
        &layout.config,
        serde_json::to_string_pretty(&snapshot).expect("a snapshot serializes"),
    )
    .with_context(|| format!("writing {}", layout.config.display()))?;

    // `-A` so a migrated-away legacy file is staged as a removal too.
    run_git(&dir, &["add", "-A", "--", "ops/"])?;
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

    let warnings = size_warnings(&layout.files(&dir)?);
    if ctx.json {
        print_json(&json!({
            "schema": SCHEMA,
            "target": target,
            "ops_appended": new_ops.len(),
            "last_seq": through_seq,
            "shards": shards,
            "legacy_migrated": legacy_migrated,
            "committed": committed,
            "pushed": pushed,
            "remote": remote,
            "warnings": warnings,
        }));
    } else {
        println!(
            "backed up {} to {target}: {} new op(s), through seq {through_seq}",
            ws.prefix,
            new_ops.len()
        );
        if let Some(n) = legacy_migrated {
            println!(
                "  migrated {n} op(s) from {} into shards",
                layout.legacy.display()
            );
        }
        println!(
            "  shards: {shards} under {}, committed: {committed}, pushed: {}",
            layout.shard_dir.display(),
            match &remote {
                Some(name) if pushed => format!("yes ({name})"),
                _ => "no (no remote configured)".to_string(),
            }
        );
        for warning in &warnings {
            eprintln!("warning: {warning}");
        }
    }
    Ok(())
}

/// The workspace + project snapshot, with the text of every op-backed
/// document body left empty (module doc, "Layout"): a body whose cached
/// text is exactly what its `body.edit` ops replay to is carried by the
/// log, and restore refills it. A body with no `doc_id`, or whose cached
/// text has drifted from its view, keeps its text — restore has no other
/// source for it.
fn config_snapshot(store: &Store, ws: &Workspace) -> Result<ConfigSnapshot> {
    let mut projects = store.projects()?;
    let mut project_doc_ids = BTreeMap::new();
    let mut named_doc_ids: BTreeMap<String, BTreeMap<String, Ulid>> = BTreeMap::new();
    let op_backed = |doc_id: Ulid, text: &str| -> Result<bool> {
        Ok(store
            .doc_view(doc_id)?
            .is_some_and(|view| view.text() == text))
    };
    for project in &mut projects {
        if let Some(doc_id) = store.design_doc_id(&project.id)? {
            project_doc_ids.insert(project.id.clone(), doc_id);
            if op_backed(doc_id, &project.doc)? {
                project.doc.clear();
            }
        }
        let mut names = BTreeMap::new();
        for (name, body) in &mut project.documents {
            if let Some(doc_id) = store.named_doc_id(&project.id, name)? {
                names.insert(name.clone(), doc_id);
                if op_backed(doc_id, body)? {
                    body.clear();
                }
            }
        }
        if !names.is_empty() {
            named_doc_ids.insert(project.id.clone(), names);
        }
    }
    Ok(ConfigSnapshot {
        schema: SNAPSHOT_SCHEMA,
        workspace: ws.clone(),
        projects,
        project_doc_ids,
        named_doc_ids,
    })
}

// --------------------------------------------------------- pm backup --restore

/// The one workspace backed up under `dir`, found by its
/// `ops/<stem>.config.json`. `pm backup` writes one workspace per target
/// today, so "exactly one" is the expected case; more than one names them
/// so a future `--prefix` flag has something to disambiguate.
fn find_backup(dir: &Path) -> Result<Layout> {
    let ops_dir = dir.join("ops");
    let mut stems: Vec<String> = fs::read_dir(&ops_dir)
        .with_context(|| format!("reading {}", ops_dir.display()))?
        .filter_map(|entry| entry.ok())
        .filter_map(|entry| {
            let name = entry.file_name();
            let name = name.to_str()?;
            Some(name.strip_suffix(".config.json")?.to_string())
        })
        .collect();
    stems.sort();
    match stems.as_slice() {
        [] => Err(CliError::not_found(format!(
            "no ops/*.config.json found under {}",
            dir.display()
        ))),
        [stem] => {
            let layout = Layout::new(dir, stem);
            if layout.op_files()?.is_empty() {
                return Err(CliError::not_found(format!(
                    "no ops/{stem}.jsonl or ops/{stem}/*.jsonl found alongside {}",
                    layout.config.display()
                )));
            }
            Ok(layout)
        }
        many => Err(CliError::usage(format!(
            "{} backs up more than one workspace ({}); restore does not \
             disambiguate between them yet",
            ops_dir.display(),
            many.join(", ")
        ))),
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
    let layout = find_backup(&dir)?;

    let snapshot: ConfigSnapshot = {
        let text = fs::read_to_string(&layout.config)
            .with_context(|| format!("reading {}", layout.config.display()))?;
        serde_json::from_str(&text)
            .with_context(|| format!("parsing {}", layout.config.display()))?
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

    let actor = ctx.actor()?;
    let mut store = Store::open(&db_path)?;
    let op_files = layout.op_files()?;

    // Pass 1 (AGT-1385): the config ops, which everything else depends
    // on. Order-independent among themselves (pm-core's config folds are
    // CRDTs), so file order is fine.
    let mut replayed: u64 = 0;
    for path in &op_files {
        for op in read_jsonl(path)? {
            if op.payload.is_config() {
                store.commit(&op)?;
                replayed += 1;
            }
        }
    }

    // The snapshot: a no-op on what pass 1 already established, the
    // whole configuration for a backup written before config was
    // op-logged, and in both cases the document text of any document the
    // log does not carry (`put_project`'s direct doc writes).
    store.init_workspace(&snapshot.workspace, &actor)?;
    let listed: BTreeSet<String> = snapshot.projects.iter().map(|p| p.id.clone()).collect();
    for project in topo_sorted(snapshot.projects)? {
        store.put_project(&project, &actor)?;
    }
    for project in store.projects()? {
        if !listed.contains(&project.id) {
            // Created by the log, gone from the snapshot: deleted before
            // the backup (`pm project delete` removes the row, never the
            // ops). Nothing references it, or the delete was refused.
            store.delete_project(&project.id)?;
        }
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

    // Pass 2: everything else, in file order.
    for path in &op_files {
        for op in read_jsonl(path)? {
            if op.payload.is_config() {
                continue;
            }
            // A document's body.edit (AGT-1344) and a ticket op both live
            // in this one log; commit_any tells them apart by entity.
            store.commit_any(&op)?;
            replayed += 1;
        }
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
/// Also lists the target's files and warns about any over
/// [`WARN_FILE_BYTES`] (AGT-1378) — a warning alone never fails the
/// check.
pub fn status(ctx: &Ctx<'_>, to: Option<PathBuf>) -> Result<()> {
    let (store, ws) = workspace::open(&workspace::resolve(ctx.workspace, ctx.env)?)?;
    let dir = absolute(&resolve_target(to, ctx.env)?)?;
    let target = dir.to_string_lossy().into_owned();
    let found = store.backup_status(&target)?;
    let healthy = found
        .as_ref()
        .and_then(|s| s.age_seconds)
        .is_some_and(|age| age < STALE_AFTER_SECONDS);
    let files = Layout::new(&dir, &ws.prefix.to_ascii_lowercase()).files(&dir)?;
    let warnings = size_warnings(&files);

    if ctx.json {
        print_json(&json!({
            "schema": SCHEMA,
            "target": target,
            "last_seq": found.as_ref().map_or(0, |s| s.last_seq),
            "last_success": found.as_ref().and_then(|s| s.last_success.clone()),
            "age_seconds": found.as_ref().and_then(|s| s.age_seconds),
            "healthy": healthy,
            "files": files
                .iter()
                .map(|(path, bytes)| json!({"path": path, "bytes": bytes}))
                .collect::<Vec<_>>(),
            "warnings": warnings,
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
        for (path, bytes) in &files {
            println!("file:         {path} ({:.1} MB)", megabytes(*bytes));
        }
        for warning in &warnings {
            println!("warning:      {warning}");
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn sizes(dir: &Path) -> Vec<(u32, u64)> {
        list_shards(dir)
            .unwrap()
            .into_iter()
            .map(|(n, p)| (n, fs::metadata(p).unwrap().len()))
            .collect()
    }

    #[test]
    fn shard_writer_rotates_by_size_in_order_and_continues_the_newest_shard() {
        let dir = tempfile::tempdir().unwrap();
        // 10-byte lines (9 + newline) against a 25-byte threshold: two
        // per shard.
        let mut w = ShardWriter::open(dir.path(), 25).unwrap();
        for i in 0..5 {
            w.append(&format!("line-{i:04}")).unwrap();
        }
        assert_eq!(w.finish().unwrap(), 3);
        assert_eq!(sizes(dir.path()), vec![(1, 20), (2, 20), (3, 10)]);

        // Reopening appends to the newest shard and rotates from there.
        let mut w = ShardWriter::open(dir.path(), 25).unwrap();
        w.append("line-0005").unwrap();
        w.append("line-0006").unwrap();
        assert_eq!(w.finish().unwrap(), 4);
        assert_eq!(sizes(dir.path()), vec![(1, 20), (2, 20), (3, 20), (4, 10)]);
        assert_eq!(
            fs::read_to_string(dir.path().join("000003.jsonl")).unwrap(),
            "line-0004\nline-0005\n"
        );
    }

    #[test]
    fn a_line_over_the_threshold_gets_a_shard_to_itself() {
        let dir = tempfile::tempdir().unwrap();
        let mut w = ShardWriter::open(dir.path(), 25).unwrap();
        w.append("small").unwrap();
        w.append(&"x".repeat(40)).unwrap();
        w.append("small").unwrap();
        assert_eq!(w.finish().unwrap(), 3);
        assert_eq!(sizes(dir.path()), vec![(1, 6), (2, 41), (3, 6)]);
    }

    #[test]
    fn list_shards_ignores_files_that_are_not_shards() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("000002.jsonl"), "b\n").unwrap();
        fs::write(dir.path().join("000001.jsonl"), "a\n").unwrap();
        fs::write(dir.path().join("1.jsonl"), "not zero-padded\n").unwrap();
        fs::write(dir.path().join("README.md"), "neither\n").unwrap();
        let names: Vec<String> = list_shards(dir.path())
            .unwrap()
            .into_iter()
            .map(|(_, p)| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, ["000001.jsonl", "000002.jsonl"]);
        assert!(list_shards(&dir.path().join("missing")).unwrap().is_empty());
    }

    #[test]
    fn size_warnings_name_only_files_over_the_threshold() {
        let files = vec![
            ("ops/agt.config.json".to_string(), 1024),
            ("ops/agt/000001.jsonl".to_string(), WARN_FILE_BYTES + 1),
        ];
        let warnings = size_warnings(&files);
        assert_eq!(warnings.len(), 1);
        assert!(
            warnings[0].starts_with("ops/agt/000001.jsonl is 50.0 MB, over the 50 MB"),
            "{}",
            warnings[0]
        );
    }
}
