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
