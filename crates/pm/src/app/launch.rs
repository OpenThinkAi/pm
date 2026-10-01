//! Launching the ui-leaf app (AGT-1402; README §Surfaces, decision 6):
//! find a pinned ui-leaf runtime, decide whether a window can open here,
//! and drive one mount over ui-leaf's stdio protocol for as long as the
//! view is showing.
//!
//! **Finding the runtime.** `ui_leaf.path` in config.toml, else the first
//! `ui-leaf` on `PATH`. The npm package's `ui-leaf` is a Node shim beside
//! the native binary (`ui-leaf-bin`), so when that sibling exists pm runs
//! it directly — an explicit binary path, no Node in between.
//!
//! **Trust (AGT-1465).** pm hands the runtime its API bearer token, so a
//! random executable named `ui-leaf` earlier on `PATH` must not get it.
//! `ui_leaf.path` is the operator's explicit choice and is used as given.
//! A `PATH` hit is launched only when it is the npm-installed
//! `@openthink/ui-leaf` package: the shim's symlink is resolved and the
//! `package.json` at the package root (`<pkg>/bin/ui-leaf` → `<pkg>`) must
//! be named `@openthink/ui-leaf` ([`verify_npm_package`]). Anything else is
//! [`Missing::Untrusted`] — pm says so and uses `$EDITOR` — until the
//! operator names it in `ui_leaf.path`.
//!
//! **Integrity (AGT-1482).** Provenance alone is not integrity: a
//! compromised npm release, or a local write to the npm prefix, passes the
//! package check. So the binary a `PATH` hit resolves to (the native
//! `ui-leaf-bin` the postinstall downloaded, or the shim itself when there
//! is none) must also hash to one of [`PINNED_BUILDS`] — the SHA-256 of
//! every published build of the supported releases, per platform. An
//! unknown digest is [`Missing::Unverified`]: pm says so and uses
//! `$EDITOR`. `ui_leaf.path` stays the operator's explicit opt-in: a
//! configured binary is trusted as given, digest unchecked (a locally
//! built ui-leaf, or a release pm has not pinned yet).
//!
//! **Pinned** means: `<runtime> --version` reports a version in
//! [`PIN_MIN`]`..<`[`PIN_BELOW`] — exactly the verified minor (AGT-1468).
//! The runtime is handed pm's API token, so a release nobody has tested
//! (or a hijacked one) must not launch after a routine `npm i -g`: a new
//! minor is adopted only by bumping the pin (docs/app-api.md §Bumping the
//! ui-leaf pin). Anything else is treated as missing (with a note), never
//! launched.
//!
//! **A display.** ui-leaf's own `UI_LEAF_NO_OPEN` wins: truthy means no
//! window (pm uses `$EDITOR`), and `0`/`false`/`no` forces one. Otherwise
//! an SSH session (`SSH_CONNECTION`/`SSH_TTY`) has no display, and on
//! Linux and the BSDs neither does a session without `DISPLAY` or
//! `WAYLAND_DISPLAY`. macOS and Windows sessions that are not SSH are
//! taken to have one.
//!
//! **Passing the API.** The view never learns pm's URL or token from a URL
//! — not `view.url`, not a fragment (the insieme lesson: ui-leaf's `ready`
//! URL carries no token and its own token is ui-leaf's, not pm's). Nor
//! from the mount's `data`: ui-leaf inlines `data` into the page HTML, which
//! its server hands any local process without a token. Instead the mount
//! declares one mutation, `session`, and pm answers it — over ui-leaf's
//! token-gated `/mutate` channel — with `{schema, url, token}`. The mount's
//! CSP is ui-leaf's strict preset with pm's API origin added to
//! `connect-src` (and `'wasm-unsafe-eval'` to `script-src`, for the
//! editor's `loro-crdt`), and once ui-leaf reports its port the view's origin is
//! allowed on the API (what `--allow-origin` does for an external view).
//!
//! **Lifetime.** The view holds `GET /events` open while it shows. When the
//! window closes the stream drops, the server's idle grace runs out, and
//! pm tells ui-leaf to close; if ui-leaf exits first (Ctrl-C, a crash), the
//! server stops with it. ui-leaf's own `disconnected` event is ignored: it
//! is heartbeat silence, which a minimized window also produces.

use std::ffi::OsStr;
use std::fmt;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Context;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use super::AppState;
use crate::exit::Result;
use crate::verbs::SCHEMA;
use crate::workspace::{Config, Env};

/// The oldest ui-leaf pm launches: the release these views were built
/// and tested against (verified against 1.6.0).
pub(crate) const PIN_MIN: (u64, u64, u64) = (1, 6, 0);
/// The first ui-leaf version pm does not launch: the next minor.
pub(crate) const PIN_BELOW: (u64, u64, u64) = (1, 7, 0);

/// One published ui-leaf build pm launches from `PATH` (AGT-1482).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct PinnedBuild {
    pub version: &'static str,
    /// npm's `<platform>-<arch>`, as ui-leaf's postinstall names it.
    pub platform: &'static str,
    /// Lowercase hex SHA-256 of the native binary.
    pub sha256: &'static str,
}

/// The SHA-256 of every ui-leaf build inside the pin ([`PIN_MIN`]..
/// [`PIN_BELOW`]) that pm launches from `PATH`: each platform's native
/// binary, exactly as the release publishes it and ui-leaf's postinstall
/// installs it (`bin/ui-leaf-bin`, byte for byte). 1.6.0 is the only
/// 1.6.x release (npm, 2026-09-30).
///
/// Where the digests come from (2026-09-30): the npm tarball ships only a
/// stub — the postinstall downloads the binary from the GitHub release
/// `v<version>` and checks it against that release's `checksums.txt` —
/// so the npm tarball holds no binary to hash. Each digest below is the
/// release asset's SHA-256 as GitHub reports it, and it matches the
/// release's `checksums.txt`; darwin-arm64 was also hashed from an
/// installed copy. **Bumping** (a new 1.6.x, or with [`PIN_MIN`] /
/// [`PIN_BELOW`] a new minor) is docs/app-api.md §Bumping the ui-leaf pin:
/// add one entry per platform from that release, read-only
/// (`gh release view v<version> --repo OpenThinkAi/ui-leaf --json assets`
/// and `gh release download v<version> --pattern checksums.txt`), and
/// check at least one against a real install.
pub(crate) const PINNED_BUILDS: &[PinnedBuild] = &[
    PinnedBuild {
        version: "1.6.0",
        platform: "darwin-arm64",
        sha256: "4c4e27358b4db17511721cc287fe61d1381c08f19247ebef352f5e09d23f1cf4",
    },
    PinnedBuild {
        version: "1.6.0",
        platform: "darwin-x64",
        sha256: "5d822c04636ad0f7bf933fa12d72c13489c391b5199147c4a38c666bc19d6474",
    },
    PinnedBuild {
        version: "1.6.0",
        platform: "linux-arm64",
        sha256: "f0ce0beeec8c72200afabb73746ae6ecbfb3cd23826dcb32d51b2f98b71c293b",
    },
    PinnedBuild {
        version: "1.6.0",
        platform: "linux-x64",
        sha256: "e98d2c6c9af7768640d6d9ef83935d329445bca0323930c1c5c3b5ab790a6293",
    },
    PinnedBuild {
        version: "1.6.0",
        platform: "win32-x64",
        sha256: "4702a23265e81d36773b68c722fc60a057ca0e481f76eeccadf52e92dc9d800b",
    },
];

/// How long `ui-leaf --version` may take (the npm shim starts Node).
const VERSION_TIMEOUT: Duration = Duration::from_secs(10);
/// How long ui-leaf gets to close after pm asks before it is killed.
const CLOSE_TIMEOUT: Duration = Duration::from_secs(5);
/// The one mutation a pm view may call.
const SESSION: &str = "session";

// -------------------------------------------------------------- runtime

/// A ui-leaf binary that passed the pin.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Runtime {
    pub path: PathBuf,
    pub version: String,
}

/// Why there is no runtime to launch. `Display` is the one-line note pm
/// prints when it falls back.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Missing {
    /// Neither `ui_leaf.path` nor `PATH` has one.
    NotFound,
    /// `ui_leaf.path` names something that is not an executable file.
    NotExecutable(PathBuf),
    /// Found on `PATH`, but it is not the npm-installed
    /// `@openthink/ui-leaf` package (AGT-1465).
    Untrusted { path: PathBuf, why: String },
    /// Found on `PATH` in the npm package, but its binary is not one of
    /// [`PINNED_BUILDS`] (AGT-1482): `why` gives its digest, or why it
    /// could not be hashed.
    Unverified { path: PathBuf, why: String },
    /// `--version` failed or printed something that is not a version.
    Unrunnable { path: PathBuf, why: String },
    /// A version outside the pin.
    Unpinned { path: PathBuf, version: String },
}

impl fmt::Display for Missing {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Missing::NotFound => write!(f, "ui-leaf not found (config ui_leaf.path, then PATH)"),
            Missing::NotExecutable(path) => {
                write!(
                    f,
                    "ui_leaf.path {} is not an executable file",
                    path.display()
                )
            }
            Missing::Untrusted { path, why } => write!(
                f,
                "ui-leaf on PATH at {} is not the npm @openthink/ui-leaf package ({why}); \
                 not launched. Set ui_leaf.path in config.toml to use it anyway",
                path.display()
            ),
            Missing::Unverified { path, why } => write!(
                f,
                "ui-leaf at {} is not a build pm has pinned ({why}); not launched. \
                 Set ui_leaf.path in config.toml to trust it anyway",
                path.display()
            ),
            Missing::Unrunnable { path, why } => {
                write!(
                    f,
                    "ui-leaf at {} did not report a version ({why})",
                    path.display()
                )
            }
            Missing::Unpinned { path, version } => write!(
                f,
                "ui-leaf {version} at {} is outside the supported range >={}.{}.{}, <{}.{}.{}",
                path.display(),
                PIN_MIN.0,
                PIN_MIN.1,
                PIN_MIN.2,
                PIN_BELOW.0,
                PIN_BELOW.1,
                PIN_BELOW.2
            ),
        }
    }
}

/// `x.y.z` (a leading `v`, a `-pre`/`+build` suffix and surrounding text
/// on the line are tolerated) from `ui-leaf --version`'s first line.
pub(crate) fn parse_version(output: &str) -> Option<(u64, u64, u64)> {
    let line = output.lines().next()?.trim();
    let word = line.split_whitespace().last()?;
    let core = word.trim_start_matches('v');
    let core = core.split(['-', '+']).next()?;
    let mut parts = core.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = parts.next()?.parse().ok()?;
    if parts.next().is_some() {
        return None;
    }
    Some((major, minor, patch))
}

/// Inside the pin: `>= PIN_MIN, < PIN_BELOW`.
pub(crate) fn pinned(version: (u64, u64, u64)) -> bool {
    version >= PIN_MIN && version < PIN_BELOW
}

fn is_executable(path: &Path) -> bool {
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    if !meta.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        meta.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

/// The first `ui-leaf` executable on `path_var`.
fn search_path(path_var: &OsStr) -> Option<PathBuf> {
    let name = if cfg!(windows) {
        "ui-leaf.exe"
    } else {
        "ui-leaf"
    };
    std::env::split_paths(path_var)
        .filter(|dir| dir.is_absolute())
        .map(|dir| dir.join(name))
        .find(|p| is_executable(p))
}

/// The npm package's native binary when `found` is its Node shim
/// (`…/@openthink/ui-leaf/bin/ui-leaf` beside `ui-leaf-bin`), else `found`.
fn native_binary(found: &Path) -> PathBuf {
    let Ok(real) = std::fs::canonicalize(found) else {
        return found.to_path_buf();
    };
    let native = real.with_file_name(if cfg!(windows) {
        "ui-leaf-bin.exe"
    } else {
        "ui-leaf-bin"
    });
    if is_executable(&native) {
        native
    } else {
        found.to_path_buf()
    }
}

/// The npm package name the `PATH` shim must belong to.
const NPM_PACKAGE: &str = "@openthink/ui-leaf";

/// Whether `found` (a `PATH` hit) is the npm package's shim: its symlink
/// resolved, `package.json` one directory up from the shim (`bin/`) — or
/// beside it — names [`NPM_PACKAGE`]. `Err` says what is wrong.
fn verify_npm_package(found: &Path) -> std::result::Result<(), String> {
    let real = std::fs::canonicalize(found).map_err(|e| format!("cannot resolve it: {e}"))?;
    let bin_dir = real.parent().ok_or("it has no parent directory")?;
    for dir in [bin_dir.parent(), Some(bin_dir)].into_iter().flatten() {
        let Ok(text) = std::fs::read_to_string(dir.join("package.json")) else {
            continue;
        };
        let name = serde_json::from_str::<Value>(&text)
            .ok()
            .and_then(|v| v.get("name").and_then(Value::as_str).map(str::to_string));
        return match name.as_deref() {
            Some(NPM_PACKAGE) => Ok(()),
            Some(other) => Err(format!("its package.json is named {other:?}")),
            None => Err("its package.json has no name".into()),
        };
    }
    Err("no package.json beside it".into())
}

/// Where the runtime should be: config first, then `PATH`.
fn locate(
    configured: Option<PathBuf>,
    path_var: Option<&OsStr>,
) -> std::result::Result<PathBuf, Missing> {
    if let Some(path) = configured {
        return if is_executable(&path) {
            Ok(native_binary(&path))
        } else {
            Err(Missing::NotExecutable(path))
        };
    }
    let found = path_var.and_then(search_path).ok_or(Missing::NotFound)?;
    verify_npm_package(&found).map_err(|why| Missing::Untrusted {
        path: found.clone(),
        why,
    })?;
    Ok(native_binary(&found))
}

/// Lowercase hex SHA-256 of the file at `path`, streamed.
fn sha256_file(path: &Path) -> std::io::Result<String> {
    use sha2::{Digest, Sha256};
    use std::fmt::Write as _;
    use std::io::Read;
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher.finalize().iter().fold(String::new(), |mut hex, b| {
        let _ = write!(hex, "{b:02x}");
        hex
    }))
}

/// Checks that `path` hashes to one of `trusted` (AGT-1482).
fn verify_digest(path: &Path, trusted: &[&str]) -> std::result::Result<(), Missing> {
    let unverified = |why: String| Missing::Unverified {
        path: path.to_path_buf(),
        why,
    };
    let digest = sha256_file(path).map_err(|e| unverified(format!("cannot hash it: {e}")))?;
    if trusted.iter().any(|t| t.eq_ignore_ascii_case(&digest)) {
        Ok(())
    } else {
        Err(unverified(format!(
            "sha256 {digest} is not a pinned {}.{}.x build",
            PIN_MIN.0, PIN_MIN.1
        )))
    }
}

/// The digests a `PATH` hit may have: [`PINNED_BUILDS`] — plus, in a
/// debug build only, `PM_UI_LEAF_TEST_SHA256`, so the test suite's fake
/// runtime can stand in for a real one. A release build ignores it.
fn trusted_digests(env: &Env) -> Vec<&str> {
    let mut trusted: Vec<&str> = PINNED_BUILDS.iter().map(|b| b.sha256).collect();
    if cfg!(debug_assertions)
        && let Some(test) = env.ui_leaf_test_sha256.as_deref()
    {
        trusted.push(test);
    }
    trusted
}

/// [`locate`], then — for a `PATH` hit, never for `ui_leaf.path` — the
/// digest check.
fn resolve(
    configured: Option<PathBuf>,
    path_var: Option<&OsStr>,
    trusted: &[&str],
) -> std::result::Result<PathBuf, Missing> {
    let explicit = configured.is_some();
    let path = locate(configured, path_var)?;
    if !explicit {
        verify_digest(&path, trusted)?;
    }
    Ok(path)
}

/// `<path> --version`'s stdout, bounded by [`VERSION_TIMEOUT`].
fn run_version(path: &Path) -> std::result::Result<String, String> {
    let mut child = Command::new(path)
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| e.to_string())?;
    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if started.elapsed() > VERSION_TIMEOUT => {
                let _ = child.kill();
                let _ = child.wait();
                return Err("--version timed out".into());
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            Err(e) => return Err(e.to_string()),
        }
    }
    let output = child.wait_with_output().map_err(|e| e.to_string())?;
    if !output.status.success() {
        return Err(format!("--version exited {}", output.status));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Checks the pin on `path`.
fn probe(path: PathBuf) -> std::result::Result<Runtime, Missing> {
    let output = run_version(&path).map_err(|why| Missing::Unrunnable {
        path: path.clone(),
        why,
    })?;
    let Some(version) = parse_version(&output) else {
        return Err(Missing::Unrunnable {
            path,
            why: format!("--version printed {:?}", output.trim()),
        });
    };
    let shown = format!("{}.{}.{}", version.0, version.1, version.2);
    if !pinned(version) {
        return Err(Missing::Unpinned {
            path,
            version: shown,
        });
    }
    Ok(Runtime {
        path,
        version: shown,
    })
}

/// The pinned runtime this environment names, or why there is none.
pub(crate) fn find(env: &Env) -> Result<std::result::Result<Runtime, Missing>> {
    let configured = Config::load(&env.config_path()?)?
        .and_then(|c| c.ui_leaf)
        .and_then(|u| u.path);
    let trusted = trusted_digests(env);
    Ok(resolve(configured, env.path.as_deref(), &trusted).and_then(probe))
}

// -------------------------------------------------------------- display

/// Why no window can open here, or `None` when one can. `macos_or_windows`
/// is the platform question, a parameter so every branch is testable
/// anywhere.
fn headless_on(env: &Env, macos_or_windows: bool) -> Option<String> {
    if let Some(value) = &env.ui_leaf_no_open {
        return match value.trim().to_ascii_lowercase().as_str() {
            "0" | "false" | "no" => None,
            _ => Some("UI_LEAF_NO_OPEN is set".into()),
        };
    }
    if env.ssh_connection.is_some() || env.ssh_tty.is_some() {
        return Some("SSH session".into());
    }
    if !macos_or_windows && env.display.is_none() && env.wayland_display.is_none() {
        return Some("no DISPLAY or WAYLAND_DISPLAY".into());
    }
    None
}

/// Why no window can open in this session, or `None` when one can.
pub(crate) fn headless(env: &Env) -> Option<String> {
    headless_on(env, cfg!(any(target_os = "macos", windows)))
}

// --------------------------------------------------------------- choice

/// What a command that prefers the ui-leaf view does.
#[derive(Debug, PartialEq)]
pub(crate) enum Choice {
    /// Launch this runtime.
    Launch(Runtime),
    /// Don't; print the note (if any) and use the fallback.
    Fallback(Option<String>),
}

/// The decision, given the facts: no display falls back (noted only when
/// the view was asked for explicitly — a headless default is expected);
/// no pinned runtime always falls back with the reason.
pub(crate) fn decide(
    headless: Option<String>,
    runtime: impl FnOnce() -> Result<std::result::Result<Runtime, Missing>>,
    explicit: bool,
) -> Result<Choice> {
    if let Some(reason) = headless {
        return Ok(Choice::Fallback(
            explicit.then(|| format!("no display for ui-leaf ({reason})")),
        ));
    }
    Ok(match runtime()? {
        Ok(runtime) => Choice::Launch(runtime),
        Err(missing) => Choice::Fallback(Some(missing.to_string())),
    })
}

/// [`decide`] for this environment; the runtime is only probed when a
/// display exists.
pub(crate) fn choose(env: &Env, explicit: bool) -> Result<Choice> {
    decide(headless(env), || find(env), explicit)
}

// ---------------------------------------------------------------- mount

/// Which view to open.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Target {
    /// `pm app`'s entry view (AGT-1492): initiatives → projects → a
    /// project, with the board as a tab.
    Initiatives,
    Ticket {
        id: String,
    },
    /// The project view (AGT-1405), by project id.
    Project {
        id: String,
    },
}

/// ui-leaf's strict CSP preset with `api` added to `connect-src`, and
/// `'wasm-unsafe-eval'` added to `script-src`: the ticket editor's
/// `loro-crdt` compiles its WebAssembly module from bytes inlined in the
/// page (`views/vendor/loro.js`), which a CSP without that source refuses.
/// It allows WebAssembly compilation only — not `eval` or `new Function` —
/// and every other directive is the preset's.
///
/// `'unsafe-inline'` stays in `script-src` (AGT-1452): ui-leaf serves a view
/// as one HTML page whose bootstrap and compiled view are inline `<script>`
/// elements (`packages/cli/src/compile.ts`), with no nonce or hash support
/// in its CSP config, so dropping it would blank the page. Revisit if
/// ui-leaf gains nonce/hash injection.
fn csp(api: &str) -> String {
    [
        "default-src 'self'".to_string(),
        format!("connect-src 'self' {api}"),
        "form-action 'self'".to_string(),
        "img-src 'self' data: https:".to_string(),
        "font-src 'self' https: data:".to_string(),
        "style-src 'self' 'unsafe-inline' https:".to_string(),
        "script-src 'self' 'unsafe-inline' 'wasm-unsafe-eval'".to_string(),
    ]
    .join("; ")
}

/// The mount config (ui-leaf stdin line 1). Holds no secret: `data` is
/// served to anyone who asks ui-leaf's `GET /`.
pub(crate) fn mount_config(target: &Target, views_root: &Path, api: &str) -> Value {
    let (view, title, data, size) = match target {
        Target::Initiatives => (
            "initiatives",
            "pm — initiatives".to_string(),
            json!({"schema": SCHEMA, "view": "initiatives"}),
            (1280, 900),
        ),
        Target::Ticket { id } => (
            "ticket",
            format!("pm — {id}"),
            json!({"schema": SCHEMA, "view": "ticket", "ticket": id}),
            (900, 900),
        ),
        Target::Project { id } => (
            "project",
            format!("pm — project {id}"),
            json!({"schema": SCHEMA, "view": "project", "project": id}),
            (1280, 900),
        ),
    };
    json!({
        "version": "1",
        "view": view,
        "viewsRoot": views_root,
        "data": data,
        "mutations": [SESSION],
        "title": title,
        "port": 0,
        "shell": "app",
        "windowSize": {"width": size.0, "height": size.1},
        "csp": csp(api),
        // A hidden window's timers are clamped; pm ignores `disconnected`
        // anyway, this only keeps ui-leaf from reporting it spuriously.
        "heartbeatTimeoutMs": 90_000,
    })
}

/// How a mount ended.
#[derive(Debug, PartialEq)]
pub(crate) enum Ended {
    /// The view was up and has closed.
    Closed,
    /// ui-leaf exited before it was ready: nothing was shown.
    Failed(String),
}

/// A line to ui-leaf's stdin.
fn message(value: Value) -> String {
    let mut line = value.to_string();
    line.push('\n');
    line
}

/// Runs one mount of `runtime` with `config` until the view closes:
/// `stop` resolving (the API's idle grace ran out) asks ui-leaf to close;
/// ui-leaf exiting on its own ends it too.
pub(crate) async fn drive(
    runtime: &Runtime,
    config: Value,
    state: Arc<AppState>,
    stop: impl Future<Output = ()>,
) -> Result<Ended> {
    let mut child = tokio::process::Command::new(&runtime.path)
        .arg("mount")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .with_context(|| format!("starting ui-leaf ({})", runtime.path.display()))?;
    let mut stdin = child.stdin.take().context("ui-leaf stdin")?;
    let mut lines = BufReader::new(child.stdout.take().context("ui-leaf stdout")?).lines();
    // A write failing means ui-leaf is gone; its exit is handled below.
    let _ = stdin.write_all(message(config).as_bytes()).await;
    let _ = stdin.flush().await;

    let api = format!("http://127.0.0.1:{}", state.port);
    let mut ready = false;
    let mut fatal: Option<String> = None;
    tokio::pin!(stop);
    let stopped = loop {
        tokio::select! {
            line = lines.next_line() => {
                let Ok(Some(line)) = line else { break false };
                let Ok(event) = serde_json::from_str::<Value>(&line) else { continue };
                match event["type"].as_str() {
                    Some("ready") => {
                        if let Some(port) = event["port"].as_u64() {
                            state.allow_origin(format!("http://127.0.0.1:{port}"));
                            state.allow_origin(format!("http://localhost:{port}"));
                        }
                        ready = true;
                    }
                    Some("mutate") => {
                        let id = event["id"].clone();
                        let reply = if event["name"] == SESSION {
                            json!({"version": "1", "type": "result", "id": id, "value": {
                                "schema": SCHEMA, "url": api, "token": state.token,
                            }})
                        } else {
                            json!({"version": "1", "type": "error", "id": id,
                                "message": "pm views declare only the session mutation; write through the API"})
                        };
                        let _ = stdin.write_all(message(reply).as_bytes()).await;
                        let _ = stdin.flush().await;
                    }
                    Some("error") => {
                        let text = event["message"].as_str().unwrap_or("(no message)");
                        match event["phase"].as_str() {
                            // Fatal: ui-leaf exits next; say why then.
                            None => fatal = Some(text.to_string()),
                            // A view that does not compile (build) or a
                            // runtime failure: surface it, it is not ours
                            // to hide.
                            Some(phase) => eprintln!("pm: ui-leaf {phase} error: {text}"),
                        }
                    }
                    Some("closed") => break false,
                    _ => {}
                }
            }
            () = &mut stop => break true,
        }
    };
    if stopped {
        let _ = stdin
            .write_all(message(json!({"version": "1", "type": "close"})).as_bytes())
            .await;
        let _ = stdin.flush().await;
    }
    drop(stdin);
    match tokio::time::timeout(CLOSE_TIMEOUT, child.wait()).await {
        Ok(status) => {
            let status = status.context("waiting for ui-leaf")?;
            if !ready {
                return Ok(Ended::Failed(fatal.unwrap_or_else(|| {
                    format!("ui-leaf exited ({status}) before its view was ready")
                })));
            }
        }
        Err(_) => {
            let _ = child.kill().await;
            if !ready {
                return Ok(Ended::Failed("ui-leaf never became ready".into()));
            }
        }
    }
    Ok(Ended::Closed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_parse_from_the_first_line() {
        assert_eq!(parse_version("1.6.0\n"), Some((1, 6, 0)));
        assert_eq!(parse_version("v1.7.2-rc.1"), Some((1, 7, 2)));
        assert_eq!(parse_version("ui-leaf 1.6.3+abc\nmore"), Some((1, 6, 3)));
        for bad in ["", "ui-leaf", "1.6", "1.6.0.1", "one.two.three"] {
            assert_eq!(parse_version(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn the_pin_is_exactly_the_verified_minor() {
        assert!(pinned(PIN_MIN));
        assert!(pinned((1, 6, 1)));
        assert!(pinned((1, 6, 99)));
        assert!(!pinned((1, 7, 0)));
        assert!(!pinned((1, 99, 0)));
        assert!(!pinned((1, 5, 9)));
        assert!(!pinned((0, 8, 4)));
        assert!(!pinned((2, 0, 0)));
    }

    #[cfg(unix)]
    #[test]
    fn a_path_hit_must_be_the_npm_package() {
        let tmp = tempfile::tempdir().unwrap();
        let bin = tmp.path().join("bin");
        // A bare executable named ui-leaf: no package, no launch.
        std::fs::create_dir_all(&bin).unwrap();
        script(&bin, "ui-leaf", "echo 1.6.0");
        let path_var = bin.as_os_str().to_owned();
        assert!(matches!(
            locate(None, Some(&path_var)),
            Err(Missing::Untrusted { why, .. }) if why.contains("no package.json")
        ));
        // ...a symlink into somebody else's package...
        std::fs::remove_file(bin.join("ui-leaf")).unwrap();
        npm_install(tmp.path(), "evil", "not-ui-leaf", &bin);
        let err = locate(None, Some(&path_var)).unwrap_err();
        assert!(
            matches!(&err, Missing::Untrusted { why, .. } if why.contains("not-ui-leaf")),
            "{err:?}"
        );
        assert!(err.to_string().contains("ui_leaf.path"), "{err}");
        // ...an unreadable/nameless package.json...
        let pkg = tmp.path().join("evil/lib/node_modules/@openthink/ui-leaf");
        std::fs::write(pkg.join("package.json"), "{}").unwrap();
        assert!(matches!(
            locate(None, Some(&path_var)),
            Err(Missing::Untrusted { why, .. }) if why.contains("no name")
        ));
        // ...but the real package, and an explicit config path, launch.
        std::fs::write(pkg.join("package.json"), r#"{"name":"@openthink/ui-leaf"}"#).unwrap();
        assert!(locate(None, Some(&path_var)).is_ok());
        let bare = script(tmp.path(), "mine", "echo 1.6.0");
        assert_eq!(locate(Some(bare.clone()), None), Ok(bare));
    }

    fn script(dir: &Path, name: &str, body: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        path
    }

    /// An npm-global-style install under `root/<tag>` named `name`, and
    /// its `ui-leaf` symlinked into `bin` (as `npm i -g` does).
    #[cfg(unix)]
    fn npm_install(root: &Path, tag: &str, name: &str, bin: &Path) -> PathBuf {
        let pkg = root.join(tag).join("lib/node_modules/@openthink/ui-leaf");
        std::fs::create_dir_all(pkg.join("bin")).unwrap();
        std::fs::write(pkg.join("package.json"), format!("{{\"name\":\"{name}\"}}")).unwrap();
        let shim = script(&pkg.join("bin"), "ui-leaf", "echo 1.6.0");
        std::fs::create_dir_all(bin).unwrap();
        let link = bin.join("ui-leaf");
        std::os::unix::fs::symlink(&shim, &link).unwrap();
        link
    }

    #[cfg(unix)]
    #[test]
    fn config_wins_over_path_and_path_is_searched_in_order() {
        let tmp = tempfile::tempdir().unwrap();
        let (a, b) = (tmp.path().join("a"), tmp.path().join("b"));
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        let in_b = npm_install(tmp.path(), "nb", NPM_PACKAGE, &b);
        let path_var = std::env::join_paths([a.clone(), b.clone()]).unwrap();
        assert_eq!(locate(None, Some(&path_var)), Ok(in_b.clone()));
        let in_a = npm_install(tmp.path(), "na", NPM_PACKAGE, &a);
        assert_eq!(locate(None, Some(&path_var)), Ok(in_a));

        let configured = script(tmp.path(), "custom-ui-leaf", "echo 1.6.0");
        assert_eq!(
            locate(Some(configured.clone()), Some(&path_var)),
            Ok(configured)
        );
        // A configured path that is wrong is reported, not skipped.
        let wrong = tmp.path().join("nope");
        assert_eq!(
            locate(Some(wrong.clone()), Some(&path_var)),
            Err(Missing::NotExecutable(wrong))
        );
        assert_eq!(locate(None, None), Err(Missing::NotFound));
        let empty = tmp.path().join("empty");
        std::fs::create_dir_all(&empty).unwrap();
        assert_eq!(
            locate(None, Some(empty.as_os_str())),
            Err(Missing::NotFound)
        );
    }

    /// AGT-1482: a `PATH` hit launches only when its binary hashes to a
    /// trusted digest; `ui_leaf.path` is the explicit opt-in and is never
    /// hashed.
    #[cfg(unix)]
    #[test]
    fn a_path_hit_must_hash_to_a_pinned_build() {
        let tmp = tempfile::tempdir().unwrap();
        let bin = tmp.path().join("bin");
        let link = npm_install(tmp.path(), "ok", NPM_PACKAGE, &bin);
        let path_var = bin.as_os_str().to_owned();
        let shim = std::fs::canonicalize(&link).unwrap();
        let digest = sha256_file(&shim).unwrap();
        assert_eq!(digest.len(), 64);

        // The real pins do not include a shell script.
        let env = Env::default();
        let pinned = trusted_digests(&env);
        let err = resolve(None, Some(&path_var), &pinned).unwrap_err();
        assert!(
            matches!(&err, Missing::Unverified { why, .. } if why.contains(&digest)),
            "{err:?}"
        );
        let note = err.to_string();
        assert!(note.contains("not a build pm has pinned"), "{note}");
        assert!(note.contains("ui_leaf.path"), "{note}");

        // Trusted by digest (either case): launched.
        assert_eq!(resolve(None, Some(&path_var), &[&digest]), Ok(link.clone()));
        let upper = digest.to_uppercase();
        assert_eq!(resolve(None, Some(&path_var), &[&upper]), Ok(link));

        // With a native sibling, the sibling is what is hashed: the shim's
        // digest no longer vouches for it.
        let native = script(shim.parent().unwrap(), "ui-leaf-bin", "echo native 1.6.0");
        let native = std::fs::canonicalize(native).unwrap();
        assert!(matches!(
            resolve(None, Some(&path_var), &[&digest]),
            Err(Missing::Unverified { .. })
        ));
        let native_digest = sha256_file(&native).unwrap();
        assert_eq!(
            resolve(None, Some(&path_var), &[&native_digest]),
            Ok(native)
        );

        // ui_leaf.path: trusted as given, whatever it hashes to.
        let mine = script(tmp.path(), "mine", "echo 1.6.0");
        assert_eq!(resolve(Some(mine.clone()), None, &[]), Ok(mine));
    }

    #[test]
    fn pinned_builds_are_well_formed_and_inside_the_pin() {
        let mut seen = std::collections::HashSet::new();
        for b in PINNED_BUILDS {
            assert_eq!(b.sha256.len(), 64, "{b:?}");
            assert!(
                b.sha256
                    .chars()
                    .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()),
                "{b:?}"
            );
            assert!(seen.insert(b.sha256), "duplicate {b:?}");
            let version = parse_version(b.version).expect("a version");
            assert!(pinned(version), "{b:?} is outside the pin");
        }
        // Every platform ui-leaf's postinstall supports, for the pin's
        // first release.
        for platform in [
            "darwin-arm64",
            "darwin-x64",
            "linux-arm64",
            "linux-x64",
            "win32-x64",
        ] {
            assert!(
                PINNED_BUILDS
                    .iter()
                    .any(|b| b.platform == platform && b.version == "1.6.0"),
                "{platform}"
            );
        }
    }

    /// The test hook is honoured in a debug build only.
    #[test]
    fn the_test_digest_is_a_debug_build_hook() {
        let env = Env {
            ui_leaf_test_sha256: Some("ab".repeat(32)),
            ..Env::default()
        };
        let trusted = trusted_digests(&env);
        assert_eq!(
            trusted.contains(&"ab".repeat(32).as_str()),
            cfg!(debug_assertions)
        );
        assert_eq!(trusted_digests(&Env::default()).len(), PINNED_BUILDS.len());
    }

    #[cfg(unix)]
    #[test]
    fn the_npm_shim_resolves_to_its_native_sibling() {
        let tmp = tempfile::tempdir().unwrap();
        let pkg = tmp.path().join("pkg");
        let bin = tmp.path().join("bin");
        std::fs::create_dir_all(&pkg).unwrap();
        std::fs::create_dir_all(&bin).unwrap();
        let shim = script(&pkg, "ui-leaf", "echo 1.6.0");
        std::os::unix::fs::symlink(&shim, bin.join("ui-leaf")).unwrap();
        // No sibling: the shim itself.
        assert_eq!(native_binary(&bin.join("ui-leaf")), bin.join("ui-leaf"));
        let native = script(&pkg, "ui-leaf-bin", "echo 1.6.0");
        assert_eq!(
            native_binary(&bin.join("ui-leaf")),
            std::fs::canonicalize(native).unwrap()
        );
    }

    #[cfg(unix)]
    #[test]
    fn probing_checks_the_pin() {
        let tmp = tempfile::tempdir().unwrap();
        let good = script(tmp.path(), "good", "echo 1.6.0");
        assert_eq!(
            probe(good.clone()),
            Ok(Runtime {
                path: good,
                version: "1.6.0".into()
            })
        );
        let old = script(tmp.path(), "old", "echo 1.5.1");
        assert!(matches!(probe(old), Err(Missing::Unpinned { version, .. }) if version == "1.5.1"));
        let next = script(tmp.path(), "next", "echo 1.7.0");
        assert!(matches!(probe(next), Err(Missing::Unpinned { .. })));
        let garbage = script(tmp.path(), "garbage", "echo hello");
        assert!(matches!(probe(garbage), Err(Missing::Unrunnable { .. })));
        let failing = script(tmp.path(), "failing", "exit 3");
        assert!(matches!(probe(failing), Err(Missing::Unrunnable { .. })));
    }

    #[test]
    fn display_detection() {
        let env = |f: &dyn Fn(&mut Env)| {
            let mut e = Env::default();
            f(&mut e);
            e
        };
        let bare = Env::default();
        assert_eq!(headless_on(&bare, true), None, "a local macOS session");
        assert!(headless_on(&bare, false).is_some(), "Linux without DISPLAY");
        let x11 = env(&|e| e.display = Some(":0".into()));
        assert_eq!(headless_on(&x11, false), None);
        let wayland = env(&|e| e.wayland_display = Some("wayland-0".into()));
        assert_eq!(headless_on(&wayland, false), None);
        let ssh = env(&|e| {
            e.display = Some(":0".into());
            e.ssh_connection = Some("1.2.3.4 5 6.7.8.9 22".into());
        });
        assert_eq!(headless_on(&ssh, true), Some("SSH session".into()));
        assert_eq!(headless_on(&ssh, false), Some("SSH session".into()));
        let tty = env(&|e| e.ssh_tty = Some("/dev/ttys001".into()));
        assert!(headless_on(&tty, true).is_some());
        let no_open = env(&|e| e.ui_leaf_no_open = Some("1".into()));
        assert!(headless_on(&no_open, true).is_some());
        // ui-leaf's own override: 0/false/no forces a window, even over SSH.
        for forced in ["0", "false", "No"] {
            let e = env(&|e| {
                e.ssh_tty = Some("/dev/ttys001".into());
                e.ui_leaf_no_open = Some(forced.into());
            });
            assert_eq!(headless_on(&e, false), None, "{forced}");
        }
    }

    fn rt() -> Runtime {
        Runtime {
            path: "/x/ui-leaf".into(),
            version: "1.6.0".into(),
        }
    }

    #[test]
    fn fallback_decisions() {
        // A display and a runtime: launch.
        assert_eq!(
            decide(None, || Ok(Ok(rt())), false).unwrap(),
            Choice::Launch(rt())
        );
        // Headless: never probe; note only when asked for explicitly.
        let never = || -> Result<std::result::Result<Runtime, Missing>> {
            panic!("probed a runtime without a display")
        };
        assert_eq!(
            decide(Some("SSH session".into()), never, false).unwrap(),
            Choice::Fallback(None)
        );
        let Choice::Fallback(Some(note)) = decide(Some("SSH session".into()), never, true).unwrap()
        else {
            panic!("explicit + headless must note");
        };
        assert!(note.contains("SSH session"), "{note}");
        // Missing: always noted, one line.
        for explicit in [false, true] {
            let Choice::Fallback(Some(note)) =
                decide(None, || Ok(Err(Missing::NotFound)), explicit).unwrap()
            else {
                panic!("missing must fall back with a note");
            };
            assert!(note.contains("ui-leaf not found"), "{note}");
            assert!(!note.contains('\n'));
        }
        let unpinned = Missing::Unpinned {
            path: "/x/ui-leaf".into(),
            version: "2.1.0".into(),
        };
        let Choice::Fallback(Some(note)) = decide(None, || Ok(Err(unpinned)), false).unwrap()
        else {
            panic!()
        };
        assert!(
            note.contains("2.1.0") && note.contains(">=1.6.0, <1.7.0"),
            "{note}"
        );
    }

    #[test]
    fn the_mount_config_carries_no_secret_and_opens_the_api_in_csp() {
        let config = mount_config(
            &Target::Ticket { id: "AGT-7".into() },
            Path::new("/views"),
            "http://127.0.0.1:4242",
        );
        assert_eq!(config["version"], "1");
        assert_eq!(config["view"], "ticket");
        assert_eq!(config["viewsRoot"], "/views");
        assert_eq!(config["data"]["ticket"], "AGT-7");
        assert_eq!(config["mutations"], json!(["session"]));
        assert_eq!(config["port"], 0);
        let csp = config["csp"].as_str().unwrap();
        assert!(
            csp.contains("connect-src 'self' http://127.0.0.1:4242;"),
            "{csp}"
        );
        // WebAssembly for loro-crdt, and nothing looser: no `unsafe-eval`.
        assert!(
            csp.contains("script-src 'self' 'unsafe-inline' 'wasm-unsafe-eval'"),
            "{csp}"
        );
        assert!(!csp.contains("'unsafe-eval'"), "{csp}");
        let text = config.to_string();
        assert!(!text.contains("pma_") && !text.contains("token"), "{text}");
        let app = mount_config(
            &Target::Initiatives,
            Path::new("/views"),
            "http://127.0.0.1:1",
        );
        assert_eq!(app["view"], "initiatives");
        assert_eq!(app["data"], json!({"schema": 1, "view": "initiatives"}));
        let project = mount_config(
            &Target::Project { id: "pm".into() },
            Path::new("/views"),
            "http://127.0.0.1:1",
        );
        assert_eq!(project["view"], "project");
        assert_eq!(
            project["data"],
            json!({"schema": 1, "view": "project", "project": "pm"})
        );
    }
}
