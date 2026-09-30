//! `pm app` (AGT-1401; projects/pm/README.md §Surfaces, decision A6): the
//! localhost API the ui-leaf views read and write through, so every
//! change they make is a pm op like any other. The HTTP contract is
//! `docs/app-api.md`; the JSON shapes are the CLI's (`docs/cli-contract.md`).
//!
//! **What it is.** An axum server inside the `pm` process, bound to
//! `127.0.0.1` on a random port. It serves the ticket, project, list and
//! ready JSON the CLI already prints (the same renderers: `ticket_json`,
//! `project_json`, `ready::compute`), accepts field sets, label changes,
//! state moves and `body.edit` updates as POSTs, and streams every op that
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
mod routes;

use std::net::Ipv4Addr;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, AtomicUsize};
use std::time::Duration;

use anyhow::Context;
use pm_core::ActorId;
use tokio::net::TcpListener;
use tokio::sync::{Notify, broadcast};

use crate::exit::{CliError, Result};
use crate::verbs::{Ctx, SCHEMA};
use crate::workspace;

/// Every token starts with this, so a leaked one is recognisable.
const TOKEN_PREFIX: &str = "pma_";
const TOKEN_BYTES: usize = 32;

/// `pm app`'s flags.
pub struct AppArgs {
    /// Seconds without a connected event stream before the server exits;
    /// `0` never exits.
    pub idle: u64,
    /// Origins allowed to call the API from a browser context.
    pub allow_origin: Vec<String>,
}

/// What every handler shares.
pub(crate) struct AppState {
    /// The workspace directory every request opens.
    dir: PathBuf,
    /// The actor every op this server commits records.
    actor: ActorId,
    token: String,
    port: u16,
    allowed_origins: Vec<String>,
    /// Every op the watcher sees, fanned out to the event streams.
    events: broadcast::Sender<events::OpEvent>,
    /// The newest `seq` the watcher has read.
    head: AtomicI64,
    /// A handler committed: the watcher reads the log now, not at the
    /// next poll.
    nudge: Notify,
    /// Open event streams; the idle timer runs while this is zero.
    viewers: AtomicUsize,
    viewers_changed: Notify,
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

/// `pm app [--idle SECS] [--allow-origin ORIGIN]...`
pub fn app(ctx: &Ctx<'_>, args: AppArgs) -> Result<()> {
    let actor = ctx.actor()?;
    let dir = workspace::resolve(ctx.workspace, ctx.env)?;
    // Open once now so a missing workspace is exit 1 before anything
    // listens, and the event stream can start from the log's head.
    let (store, _ws) = workspace::open(&dir)?;
    let head = store.head_seq()?;
    drop(store);
    let allowed_origins = args
        .allow_origin
        .iter()
        .map(|o| parse_origin(o))
        .collect::<Result<Vec<_>>>()?;
    let token = mint_token()?;
    let idle = Duration::from_secs(args.idle);

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("starting the app runtime")?;
    runtime.block_on(async move {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .context("binding 127.0.0.1")?;
        let port = listener
            .local_addr()
            .context("reading the bound address")?
            .port();
        let (events, _) = broadcast::channel(events::BUFFER);
        let state = Arc::new(AppState {
            dir: dir.clone(),
            actor: actor.clone(),
            token: token.clone(),
            port,
            allowed_origins: allowed_origins.clone(),
            events,
            head: AtomicI64::new(head),
            nudge: Notify::new(),
            viewers: AtomicUsize::new(0),
            viewers_changed: Notify::new(),
        });

        announce(ctx, &state, args.idle);

        tokio::spawn(events::watch(state.clone()));
        let idle_state = state.clone();
        let shutdown = async move {
            events::idle(idle_state, idle).await;
            eprintln!("pm app: no view connected for {}s; exiting", args.idle);
        };
        axum::serve(listener, routes::router(state))
            .with_graceful_shutdown(shutdown)
            .await
            .context("serving the app API")?;
        Ok(())
    })
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
            "allowed_origins": state.allowed_origins,
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
