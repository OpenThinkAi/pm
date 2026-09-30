//! Where the workspace lives (projects/pm/README.md §Surfaces): the
//! `--workspace` flag, then `PM_WORKSPACE`, then the `workspace` key of
//! `~/.config/pm/config.toml`. A workspace is a directory holding one
//! SQLite database, [`DB_FILE`].

use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::Context;
use pm_core::Workspace;
use pm_store::Store;
use serde::{Deserialize, Serialize};

use crate::exit::{CliError, Result};

/// The database file inside a workspace directory.
pub const DB_FILE: &str = "pm.sqlite";

/// The process environment a command depends on, read once in `main` so
/// every verb sees the same values and tests can reason about them. Blank
/// values count as unset.
#[derive(Clone, Debug, Default)]
pub struct Env {
    pub home: Option<PathBuf>,
    pub xdg_config_home: Option<PathBuf>,
    pub xdg_data_home: Option<PathBuf>,
    pub pm_workspace: Option<PathBuf>,
    pub pm_actor: Option<String>,
    pub user: Option<String>,
    /// `PM_HUB_TOKEN` (AGT-1394): the hub bearer token; wins over the keychain.
    pub hub_token: Option<String>,
    /// `PM_HUB_KEYCHAIN_SERVICE_PREFIX`: replaces `pm-hub` in the keychain
    /// service name so tests only touch throwaway items.
    pub hub_keychain_prefix: Option<String>,
    /// `PM_HUB_KEYCHAIN`: a keychain file for every keychain call instead of
    /// the default (login) keychain. Tests point it at a throwaway one.
    pub hub_keychain: Option<PathBuf>,
    /// `PM_SYNC_TEST_BATCH_OPS` (AGT-1396, a test hook): ops per push
    /// batch, below the hub's own cap, so a test can make a small log take
    /// many batches. Unset or unparsable means the real cap.
    pub sync_test_batch_ops: Option<usize>,
    /// `PM_SYNC_TEST_CRASH_AFTER_BATCHES` (AGT-1396, a test hook): exit
    /// the process after the hub has acknowledged this many push batches
    /// and *before* the last of them is marked pushed — the worst place a
    /// crash can land, which the next sync must recover from.
    pub sync_test_crash_after_batches: Option<usize>,
    /// `PATH` (AGT-1402): where `pm edit`/`pm app` look for the ui-leaf
    /// runtime when config.toml names none.
    pub path: Option<OsString>,
    /// `XDG_CACHE_HOME`: the bundled ui-leaf views are unpacked under
    /// `$XDG_CACHE_HOME/pm/views`, else `~/.cache/pm/views`.
    pub xdg_cache_home: Option<PathBuf>,
    /// `PM_VIEWS_DIR`: mount the views from this directory instead of the
    /// copy built into the binary (view development, AGT-1403..1405).
    pub pm_views_dir: Option<PathBuf>,
    /// `DISPLAY` / `WAYLAND_DISPLAY`: whether a Linux/BSD session has a
    /// display the ui-leaf window can open on.
    pub display: Option<String>,
    pub wayland_display: Option<String>,
    /// `SSH_CONNECTION` / `SSH_TTY`: a remote session, where a window
    /// would open on a desktop nobody at this terminal is looking at.
    pub ssh_connection: Option<String>,
    pub ssh_tty: Option<String>,
    /// `UI_LEAF_NO_OPEN`, ui-leaf's own switch: truthy means never open a
    /// window (so pm uses `$EDITOR`); `0`/`false`/`no` forces one even
    /// under SSH.
    pub ui_leaf_no_open: Option<String>,
}

impl Env {
    pub fn from_process() -> Self {
        fn path(key: &str) -> Option<PathBuf> {
            std::env::var_os(key)
                .filter(|v| !v.is_empty())
                .map(PathBuf::from)
        }
        // The XDG base-dir spec says relative values are invalid and must
        // be ignored.
        fn xdg(key: &str) -> Option<PathBuf> {
            path(key).filter(|p| p.is_absolute())
        }
        fn text(key: &str) -> Option<String> {
            std::env::var_os(key)
                .map(OsString::into_string)
                .and_then(|v| v.ok())
                .filter(|v| !v.trim().is_empty())
        }
        Env {
            home: path("HOME"),
            xdg_config_home: xdg("XDG_CONFIG_HOME"),
            xdg_data_home: xdg("XDG_DATA_HOME"),
            pm_workspace: path("PM_WORKSPACE"),
            pm_actor: text("PM_ACTOR"),
            user: text("USER"),
            hub_token: text("PM_HUB_TOKEN"),
            hub_keychain_prefix: text("PM_HUB_KEYCHAIN_SERVICE_PREFIX"),
            hub_keychain: path("PM_HUB_KEYCHAIN"),
            sync_test_batch_ops: text("PM_SYNC_TEST_BATCH_OPS").and_then(|v| v.parse().ok()),
            sync_test_crash_after_batches: text("PM_SYNC_TEST_CRASH_AFTER_BATCHES")
                .and_then(|v| v.parse().ok()),
            path: std::env::var_os("PATH").filter(|v| !v.is_empty()),
            xdg_cache_home: xdg("XDG_CACHE_HOME"),
            pm_views_dir: path("PM_VIEWS_DIR"),
            display: text("DISPLAY"),
            wayland_display: text("WAYLAND_DISPLAY"),
            ssh_connection: text("SSH_CONNECTION"),
            ssh_tty: text("SSH_TTY"),
            ui_leaf_no_open: text("UI_LEAF_NO_OPEN"),
        }
    }

    /// `$XDG_CONFIG_HOME/pm/config.toml`, else `~/.config/pm/config.toml`.
    pub fn config_path(&self) -> Result<PathBuf> {
        let base = match (&self.xdg_config_home, &self.home) {
            (Some(xdg), _) => xdg.clone(),
            (None, Some(home)) => home.join(".config"),
            (None, None) => {
                return Err(CliError::error(
                    "cannot locate ~/.config/pm/config.toml: HOME is not set",
                ));
            }
        };
        Ok(base.join("pm").join("config.toml"))
    }

    /// Where `pm init` puts a workspace nobody named:
    /// `$XDG_DATA_HOME/pm/<prefix>`, else `~/.local/share/pm/<prefix>`.
    pub fn default_workspace_dir(&self, prefix: &str) -> Result<PathBuf> {
        let base = match (&self.xdg_data_home, &self.home) {
            (Some(xdg), _) => xdg.clone(),
            (None, Some(home)) => home.join(".local").join("share"),
            (None, None) => {
                return Err(CliError::error(
                    "cannot choose a workspace directory: HOME is not set; pass --workspace",
                ));
            }
        };
        Ok(base.join("pm").join(prefix.to_ascii_lowercase()))
    }

    /// `$XDG_CACHE_HOME/pm`, else `~/.cache/pm`: rebuildable files only
    /// (the unpacked ui-leaf views).
    pub fn cache_dir(&self) -> Result<PathBuf> {
        let base = match (&self.xdg_cache_home, &self.home) {
            (Some(xdg), _) => xdg.clone(),
            (None, Some(home)) => home.join(".cache"),
            (None, None) => {
                return Err(CliError::error(
                    "cannot locate ~/.cache/pm: HOME is not set (or set PM_VIEWS_DIR)",
                ));
            }
        };
        Ok(base.join("pm"))
    }
}

/// `~/.config/pm/config.toml`. Unknown keys are ignored so later tickets
/// can add settings without breaking older binaries.
#[derive(Debug, Default, Deserialize, Serialize)]
pub struct Config {
    /// The default workspace directory. A relative path is relative to
    /// the directory holding config.toml.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<PathBuf>,
    /// `[backup]` (AGT-1350): where `pm backup` writes and pushes from
    /// when `--to` is not given.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backup: Option<BackupConfig>,
    /// `[edit]` (AGT-1345): `pm edit` defaults.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub edit: Option<EditConfig>,
    /// The pm-hub that arbitrates claims and numbers (README §Authority),
    /// e.g. `https://pm-hub.example`. Unset means this machine's database
    /// is the authority (phases 1–2). Written by `pm hub login` (AGT-1394). Until the hub protocol lands (P3),
    /// setting it makes `pm claim` refuse rather than claim unconfirmed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hub: Option<String>,
    /// `[ui_leaf]` (AGT-1402): the ui-leaf runtime `pm edit`/`pm app` use.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ui_leaf: Option<UiLeafConfig>,
}

/// `[ui_leaf]` in config.toml.
#[derive(Debug, Default, Deserialize, Serialize)]
pub struct UiLeafConfig {
    /// `ui_leaf.path`: the ui-leaf binary to run, instead of the first
    /// `ui-leaf` on `PATH`. A relative path is relative to the directory
    /// holding config.toml.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<PathBuf>,
}

/// `[edit]` in config.toml.
#[derive(Debug, Default, Deserialize, Serialize)]
pub struct EditConfig {
    /// `edit.view = "editor" | "ui-leaf"`: the view `pm edit` opens when
    /// `--view` is not given. Validated by `edit::parse_view`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub view: Option<String>,
}

/// `pm backup`'s config section (AGT-1350 AC1: "default dir from config
/// `backup.repo`").
#[derive(Debug, Default, Deserialize, Serialize)]
pub struct BackupConfig {
    /// The backup target: a git working directory `pm backup` writes
    /// `ops/<prefix>.jsonl` into, commits, and pushes (if it has a
    /// remote). A relative path is relative to the directory holding
    /// config.toml, same as `workspace`. Default: a clone of the private
    /// `OpenThinkAi/pm-backup-saltline` repo (README §Decisions A2).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo: Option<PathBuf>,
}

impl Config {
    /// The config at `path`, or `None` when the file does not exist.
    pub fn load(path: &Path) -> Result<Option<Config>> {
        let text = match fs::read_to_string(path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => {
                return Err(anyhow::Error::new(e)
                    .context(format!("reading {}", path.display()))
                    .into());
            }
        };
        let mut config: Config =
            toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        if let (Some(ws), Some(dir)) = (&config.workspace, path.parent())
            && ws.is_relative()
        {
            config.workspace = Some(dir.join(ws));
        }
        if let (Some(backup), Some(dir)) = (&mut config.backup, path.parent())
            && let Some(repo) = &backup.repo
            && repo.is_relative()
        {
            backup.repo = Some(dir.join(repo));
        }
        if let (Some(ui_leaf), Some(dir)) = (&mut config.ui_leaf, path.parent())
            && let Some(bin) = &ui_leaf.path
            && bin.is_relative()
        {
            ui_leaf.path = Some(dir.join(bin));
        }
        Ok(Some(config))
    }

    pub fn write(&self, path: &Path) -> Result<()> {
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        let text = toml::to_string(self).context("serializing config.toml")?;
        fs::write(path, text).with_context(|| format!("writing {}", path.display()))?;
        Ok(())
    }
}

/// The workspace directory a non-`init` command operates on:
/// `--workspace`, then `PM_WORKSPACE`, then config.toml.
pub fn resolve(flag: Option<&Path>, env: &Env) -> Result<PathBuf> {
    if let Some(dir) = flag.or(env.pm_workspace.as_deref()) {
        return Ok(dir.to_path_buf());
    }
    let config_path = env.config_path()?;
    match Config::load(&config_path)?.and_then(|c| c.workspace) {
        Some(dir) => Ok(dir),
        None => Err(CliError::error(format!(
            "no workspace: pass --workspace <dir>, set PM_WORKSPACE, or run `pm init --prefix <PREFIX>` \
             (looked for `workspace` in {})",
            config_path.display()
        ))),
    }
}

/// Opens an initialized workspace. Never creates one: a missing database
/// is an error pointing at `pm init`, not a silently empty workspace.
pub fn open(dir: &Path) -> Result<(Store, Workspace)> {
    let db = dir.join(DB_FILE);
    let not_a_workspace = || {
        CliError::error(format!(
            "{} is not a pm workspace; run `pm init --prefix <PREFIX> --workspace {}`",
            dir.display(),
            dir.display()
        ))
    };
    if !db.is_file() {
        return Err(not_a_workspace());
    }
    let store = Store::open(&db)?;
    let workspace = store.workspace()?.ok_or_else(not_a_workspace)?;
    Ok((store, workspace))
}

// ----------------------------------------------------------- pm workspace

/// `pm workspace gate-label add|remove|list <label>` (AGT-1380 AC2): the
/// workspace's gate labels (`Rules::gate_labels`, `Workspace::gate_labels`)
/// are `workspace.set gate_label_add` / `gate_label_remove` config ops
/// (AGT-1385; `pm_store::Store::set_gate_labels` diffs the wanted set
/// against the workspace's view and commits exactly those). `pm ready`/`pm
/// claim --ready` already read `Workspace::gate_labels` fresh on every
/// call and exclude a gate-labelled ticket transitively (everything it
/// blocks too), the same way they treat `manual`, so a label added here
/// takes effect immediately with no other code change. Being ops, the
/// labels replay under `pm doctor --rebuild` and travel in `pm backup`'s
/// op log (and the `Workspace` snapshot it also writes).
#[derive(clap::Subcommand, Debug)]
pub enum WorkspaceCmd {
    /// Manage the workspace's gate labels
    GateLabel {
        #[command(subcommand)]
        cmd: GateLabelCmd,
    },
}

#[derive(clap::Subcommand, Debug)]
pub enum GateLabelCmd {
    /// Add a label (excludes it from `pm ready`/`pm claim --ready`, transitively)
    Add { label: String },
    /// Remove a label
    Remove { label: String },
    /// List every gate label
    List,
}

pub fn run(ctx: &crate::verbs::Ctx<'_>, cmd: WorkspaceCmd) -> Result<()> {
    match cmd {
        WorkspaceCmd::GateLabel { cmd } => gate_label(ctx, cmd),
    }
}

fn gate_label(ctx: &crate::verbs::Ctx<'_>, cmd: GateLabelCmd) -> Result<()> {
    match cmd {
        GateLabelCmd::Add { label } => {
            let label = crate::verbs::non_empty("gate label", &label)?;
            let actor = ctx.actor()?;
            let (mut store, ws) = ctx.open()?;
            let mut labels = ws.gate_labels;
            labels.insert(label);
            store.set_gate_labels(&labels, &actor)?;
            print_gate_labels(ctx, &labels)
        }
        GateLabelCmd::Remove { label } => {
            let label = crate::verbs::non_empty("gate label", &label)?;
            let actor = ctx.actor()?;
            let (mut store, ws) = ctx.open()?;
            let mut labels = ws.gate_labels;
            labels.remove(&label);
            store.set_gate_labels(&labels, &actor)?;
            print_gate_labels(ctx, &labels)
        }
        GateLabelCmd::List => {
            let (_store, ws) = ctx.open()?;
            print_gate_labels(ctx, &ws.gate_labels)
        }
    }
}

fn print_gate_labels(
    ctx: &crate::verbs::Ctx<'_>,
    labels: &std::collections::BTreeSet<String>,
) -> Result<()> {
    if ctx.json {
        crate::verbs::print_json(&serde_json::json!({
            "schema": crate::verbs::SCHEMA,
            "gate_labels": labels,
        }));
    } else if labels.is_empty() {
        eprintln!("no gate labels");
    } else {
        for l in labels {
            println!("{l}");
        }
    }
    Ok(())
}
