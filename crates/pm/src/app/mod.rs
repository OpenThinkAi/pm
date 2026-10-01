//! `pm app` (AGT-1401; projects/pm/README.md §Surfaces, decision A6): the
//! localhost API the ui-leaf views read and write through, so every
//! change they make is a pm op like any other. The HTTP contract is
//! `docs/app-api.md`; the JSON shapes are the CLI's (`docs/cli-contract.md`).
//!
//! **What it is.** An axum server inside the `pm` process, bound to
//! `127.0.0.1` on a random port. It serves the ticket, project, list and
//! ready JSON the CLI already prints (the same renderers: `ticket_json`,
//! `project_json`, `ready::compute`), serves ticket descriptions and
//! project documents as CRDT bodies, accepts field sets, label changes,
//! state moves, `body.edit` updates (on tickets and on project documents'
//! `doc_id`s) and new tickets (`pm new`'s own path, AGT-1405) as POSTs,
//! and streams every op that
//! lands in the log — from this API, from a CLI in another process, from
//! `pm sync` — over SSE (`events`). Writes take the CLI verbs' own paths
//! (`Stamper`, `Store::commit_batch`), so they land in the outbox and sync
//! exactly as a `pm set` would. Claims are not served: they need the hub
//! (AGT-1397), and a view that wants one runs `pm claim`.
//!
//! **Who may call it.** Three guards, in `auth`, before any handler:
//! - the `Host` header must be this server's own loopback address (a
//!   DNS-rebinding page reaches the port with its own hostname there);
//! - an `Origin` header, when present, must be one the launcher allowed
//!   with `--allow-origin` (the view's own origin); no other origin is
//!   ever answered, and no wildcard is ever sent;
//! - every request carries `Authorization: Bearer <token>`, the token
//!   minted once at start and printed once, to the launcher (AGT-1402),
//!   which hands it to the view. Nothing else ever learns it.
//!
//! **When it stops.** The launcher's view holds an event stream open for
//! as long as it is showing; when the last stream closes, the server
//! waits `--idle` seconds for a reconnect (a reload, a second view) and
//! exits. The same grace runs from start, so a launch nobody connects to
//! does not linger. `--idle 0` disables the exit for `curl`-driven use;
//! `Ctrl-C` (or the launcher killing the process) ends it at any time —
//! there is nothing to flush, every write was its own transaction.
//!
//! One `Store` per request, opened in a blocking task (`rusqlite` is not
//! `Sync`; the CLI opens one per invocation the same way). The event
//! watcher keeps its own connection and polls the log's head `seq` every
//! [`events::POLL`], nudged immediately after a local commit.

mod auth;
mod events;
mod initiatives;
pub(crate) mod launch;
mod routes;
mod views;

use std::net::Ipv4Addr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicUsize};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use anyhow::Context;
use pm_core::ActorId;
use tokio::net::TcpListener;
use tokio::sync::{Notify, broadcast};

use crate::exit::{CliError, Result};
use crate::verbs::{Ctx, SCHEMA};
use crate::workspace;
use launch::{Choice, Ended, Runtime, Target};

/// Every token starts with this, so a leaked one is recognisable.
const TOKEN_PREFIX: &str = "pma_";
const TOKEN_BYTES: usize = 32;

/// `--idle`'s default for a server nobody launched a view for
/// (`pm app --json`, or `pm app` without a display or ui-leaf).
pub const HEADLESS_IDLE_SECS: u64 = 30;
/// `--idle`'s default when pm launched the view: how long a closed window
/// may take to come back (a reload reconnects well inside it) before pm
/// ends. Short, because it is how long `pm edit` lingers after the window
/// closes.
pub const LAUNCHED_IDLE_SECS: u64 = 5;
/// When pm launched the view: how long it has to connect the first time
/// (a cold browser start plus ui-leaf compiling the view).
const LAUNCH_STARTUP_GRACE: Duration = Duration::from_secs(60);

/// `pm app`'s flags.
pub struct AppArgs {
    /// Seconds without a connected event stream before the server exits;
    /// `0` never exits; `None` is [`HEADLESS_IDLE_SECS`] or
    /// [`LAUNCHED_IDLE_SECS`].
    pub idle: Option<u64>,
    /// Origins allowed to call the API from a browser context.
    pub allow_origin: Vec<String>,
}

/// What every handler shares.
pub(crate) struct AppState {
    /// The workspace directory every request opens.
    dir: PathBuf,
    /// The launching command's environment: `POST /tickets` numbers a new
    /// ticket the way `pm new` would on this machine
    /// (`hub::numbers_are_hub_assigned` reads config.toml through it).
    env: workspace::Env,
    /// The actor every op this server commits records.
    actor: ActorId,
    token: String,
    port: u16,
    /// `--allow-origin`, plus the launched view's origin once ui-leaf
    /// reports its port.
    allowed_origins: RwLock<Vec<String>>,
    /// Every op the watcher sees, fanned out to the event streams.
    events: broadcast::Sender<events::OpEvent>,
    /// The newest `seq` the watcher has read.
    head: AtomicI64,
    /// A handler committed: the watcher reads the log now, not at the
    /// next poll.
    nudge: Notify,
    /// Open event streams; the idle timer runs while this is zero.
    viewers: AtomicUsize,
    /// Whether any event stream has ever opened (the idle timer's startup
    /// grace ends then).
    connected_once: AtomicBool,
    viewers_changed: Notify,
}

impl AppState {
    fn origin_allowed(&self, origin: &str) -> bool {
        self.allowed_origins
            .read()
            .map(|list| list.iter().any(|a| a == origin))
            .unwrap_or(false)
    }

    /// Allows one more (already normalized) origin.
    fn allow_origin(&self, origin: String) {
        if let Ok(mut list) = self.allowed_origins.write()
            && !list.contains(&origin)
        {
            list.push(origin);
        }
    }

    fn origins(&self) -> Vec<String> {
        self.allowed_origins
            .read()
            .map(|list| list.clone())
            .unwrap_or_default()
    }
}

/// `pma_` + 32 CSPRNG bytes, hex: the one-shot bearer token.
fn mint_token() -> Result<String> {
    let mut bytes = [0u8; TOKEN_BYTES];
    getrandom::fill(&mut bytes)
        .map_err(|e| CliError::error(format!("minting the app token: {e}")))?;
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    Ok(format!("{TOKEN_PREFIX}{hex}"))
}

/// An `--allow-origin` value as browsers send `Origin`: `scheme://host[:port]`,
/// lowercase, no path, no trailing slash. Anything else — a wildcard, a
/// bare host, a URL with a path — is a usage error, because a value that
/// never matches a real `Origin` header would silently allow nothing.
fn parse_origin(raw: &str) -> Result<String> {
    let value = raw.trim().trim_end_matches('/').to_ascii_lowercase();
    let bad = || {
        CliError::usage(format!(
            "--allow-origin '{raw}': expected an origin like http://127.0.0.1:5173 (scheme, host, optional port; no path, no wildcard)"
        ))
    };
    let rest = value
        .strip_prefix("http://")
        .or_else(|| value.strip_prefix("https://"))
        .ok_or_else(bad)?;
    if rest.is_empty() || rest.contains(['/', '?', '#', '@', '*', ' ']) {
        return Err(bad());
    }
    Ok(value)
}

/// What the server does besides serving.
enum Mode {
    /// Announce the URL and token and serve until idle (`--json`, or no
    /// view could be launched).
    Headless,
    /// Launch `target` in `runtime` and serve until the view closes.
    View { runtime: Runtime, target: Target },
}

/// Everything a server run needs, resolved before anything listens.
struct Launch {
    dir: PathBuf,
    actor: ActorId,
    head: i64,
    allowed_origins: Vec<String>,
    idle: u64,
}

impl Launch {
    /// Resolves the actor and workspace (exit 1 before anything listens
    /// when it cannot be opened) and validates `allow_origin`.
    fn resolve(ctx: &Ctx<'_>, allow_origin: &[String], idle: u64) -> Result<Launch> {
        let actor = ctx.actor()?;
        let dir = workspace::resolve(ctx.workspace, ctx.env)?;
        // Open once now so a missing workspace is exit 1 before anything
        // listens, and the event stream can start from the log's head.
        let (store, _ws) = workspace::open(&dir)?;
        let head = store.head_seq()?;
        drop(store);
        let allowed_origins = allow_origin
            .iter()
            .map(|o| parse_origin(o))
            .collect::<Result<Vec<_>>>()?;
        Ok(Launch {
            dir,
            actor,
            head,
            allowed_origins,
            idle,
        })
    }
}

/// `pm app [--idle SECS] [--allow-origin ORIGIN]...`: with `--json`, the
/// headless server for tooling; otherwise the initiatives view (AGT-1492;
/// the board is its tab) in ui-leaf, falling
/// back to the headless server (with a note) when there is no display or
/// no pinned ui-leaf.
pub fn app(ctx: &Ctx<'_>, args: AppArgs) -> Result<()> {
    // The workspace and flags are checked before ui-leaf is probed.
    let mut launch = Launch::resolve(ctx, &args.allow_origin, 0)?;
    let choice = if ctx.json {
        Choice::Fallback(None)
    } else {
        launch::choose(ctx.env, true)?
    };
    let (mode, default_idle) = match choice {
        Choice::Launch(runtime) => (
            Mode::View {
                runtime,
                target: Target::Initiatives,
            },
            LAUNCHED_IDLE_SECS,
        ),
        Choice::Fallback(note) => {
            if let Some(note) = note {
                eprintln!(
                    "pm app: {}; serving the API only",
                    crate::text::inline(&note)
                );
            }
            (Mode::Headless, HEADLESS_IDLE_SECS)
        }
    };
    launch.idle = args.idle.unwrap_or(default_idle);
    match serve(ctx, launch, mode)? {
        Ended::Closed => Ok(()),
        Ended::Failed(why) => Err(CliError::error(format!("pm app: {why}"))),
    }
}

/// `pm edit <id>` in ui-leaf: the ticket view, until its window closes.
/// `id` must already be resolved: a ticket that exists, named by its ref
/// (the display id, or the ULID while its number is pending).
pub(crate) fn edit_ticket(ctx: &Ctx<'_>, runtime: Runtime, id: &str) -> Result<Ended> {
    let launch = Launch::resolve(ctx, &[], LAUNCHED_IDLE_SECS)?;
    serve(
        ctx,
        launch,
        Mode::View {
            runtime,
            target: Target::Ticket { id: id.to_string() },
        },
    )
}

/// `pm project edit <id>` in ui-leaf (AGT-1405): the project view — its
/// design doc and named documents in the CRDT editor, its tickets, and
/// "New ticket" — until its window closes. `id` must be a project that
/// exists.
pub(crate) fn edit_project(ctx: &Ctx<'_>, runtime: Runtime, id: &str) -> Result<Ended> {
    let launch = Launch::resolve(ctx, &[], LAUNCHED_IDLE_SECS)?;
    serve(
        ctx,
        launch,
        Mode::View {
            runtime,
            target: Target::Project { id: id.to_string() },
        },
    )
}

/// Binds, serves, and — in [`Mode::View`] — drives the ui-leaf mount;
/// returns when the server went idle or the view closed.
fn serve(ctx: &Ctx<'_>, launch: Launch, mode: Mode) -> Result<Ended> {
    let views_root = match &mode {
        Mode::View { .. } => Some(views::root(ctx.env)?),
        Mode::Headless => None,
    };
    let token = mint_token()?;
    let idle_secs = launch.idle;
    let grace = Duration::from_secs(idle_secs);
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("starting the app runtime")?;
    let ended = runtime.block_on(async move {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .context("binding 127.0.0.1")?;
        let port = listener
            .local_addr()
            .context("reading the bound address")?
            .port();
        let (events, _) = broadcast::channel(events::BUFFER);
        let state = Arc::new(AppState {
            dir: launch.dir.clone(),
            env: ctx.env.clone(),
            actor: launch.actor.clone(),
            token: token.clone(),
            port,
            allowed_origins: RwLock::new(launch.allowed_origins.clone()),
            events,
            head: AtomicI64::new(launch.head),
            nudge: Notify::new(),
            viewers: AtomicUsize::new(0),
            connected_once: AtomicBool::new(false),
            viewers_changed: Notify::new(),
        });
        tokio::spawn(events::watch(state.clone()));

        match mode {
            Mode::Headless => {
                announce(ctx, &state, idle_secs);
                let idle_state = state.clone();
                let shutdown = async move {
                    events::idle(idle_state, grace, grace).await;
                    eprintln!("pm app: no view connected for {idle_secs}s; exiting");
                };
                axum::serve(listener, routes::router(state))
                    .with_graceful_shutdown(shutdown)
                    .await
                    .context("serving the app API")?;
                Ok(Ended::Closed)
            }
            Mode::View { runtime, target } => {
                let config = launch::mount_config(
                    &target,
                    views_root.as_deref().unwrap_or(std::path::Path::new(".")),
                    &format!("http://127.0.0.1:{port}"),
                );
                let server = tokio::spawn(
                    axum::serve(listener, routes::router(state.clone())).into_future(),
                );
                let startup = if grace.is_zero() {
                    grace
                } else {
                    LAUNCH_STARTUP_GRACE.max(grace)
                };
                let stop = events::idle(state.clone(), startup, grace);
                let ended = launch::drive(&runtime, config, state, stop).await;
                // The view is gone: stop answering. A stream the closed
                // page left open must not hold the process, so this is an
                // abort, not a graceful drain.
                server.abort();
                ended
            }
        }
    });
    // Nothing is left to flush (every write was its own transaction); do
    // not wait on a blocking read a request left behind.
    runtime.shutdown_timeout(Duration::from_secs(1));
    ended
}

/// The launch line: with `--json`, one compact line a launcher can read
/// before the server's lifetime ends (`docs/cli-contract.md` §`pm app`);
/// otherwise the URL and the token on their own lines.
fn announce(ctx: &Ctx<'_>, state: &AppState, idle: u64) {
    use std::io::Write as _;
    let url = format!("http://127.0.0.1:{}", state.port);
    let mut out = std::io::stdout().lock();
    if ctx.json {
        let line = serde_json::json!({
            "schema": SCHEMA,
            "url": url,
            "token": state.token,
            "pid": std::process::id(),
            "workspace": state.dir,
            "actor": state.actor.as_str(),
            "idle_secs": idle,
            "allowed_origins": state.origins(),
        });
        let _ = writeln!(out, "{line}");
    } else {
        let _ = writeln!(out, "url:   {url}");
        let _ = writeln!(out, "token: {}", state.token);
    }
    let _ = out.flush();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_are_prefixed_hex_and_distinct() {
        let a = mint_token().unwrap();
        let b = mint_token().unwrap();
        assert!(a.starts_with(TOKEN_PREFIX));
        assert_eq!(a.len(), TOKEN_PREFIX.len() + TOKEN_BYTES * 2);
        assert!(
            a[TOKEN_PREFIX.len()..]
                .bytes()
                .all(|b| b.is_ascii_hexdigit())
        );
        assert_ne!(a, b);
    }

    #[test]
    fn origins_are_normalized_and_validated() {
        assert_eq!(
            parse_origin("HTTP://Localhost:5173/").unwrap(),
            "http://localhost:5173"
        );
        assert_eq!(
            parse_origin("https://127.0.0.1").unwrap(),
            "https://127.0.0.1"
        );
        for bad in [
            "*",
            "localhost:5173",
            "http://",
            "http://a/b",
            "http://a b",
            "file://x",
        ] {
            assert!(parse_origin(bad).is_err(), "{bad}");
        }
    }
}
