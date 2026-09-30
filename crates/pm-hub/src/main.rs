//! `pm-hub`: the sync hub (projects/pm/README.md §Sync & hub). With no
//! subcommand it migrates its Postgres schema on start and serves:
//!
//! - `GET /health` (open): status and schema version;
//! - `GET /w/{workspace}/whoami` (bearer token): the token's workspace and
//!   name, so a client can check a token before syncing with it;
//! - `POST /w/{workspace}/ops` (bearer token): push a batch of ops and get
//!   their seqs (see `ops`) — and, once the workspace is seeded, the
//!   numbers the hub allocated for its creates (see `numbers`);
//! - `GET /w/{workspace}/ops?since=&limit=` (bearer token): pull ops past
//!   a seq, a page at a time (see `pull`);
//! - `POST /w/{workspace}/seeded` (bearer token): end the workspace's
//!   seed and make the hub its number authority (see `numbers`).
//!
//! Claims are arbitrated inside the push, against the hub's materialized
//! views (see `views`, AGT-1392). `pm-hub token create|list|bind|revoke` manage
//! bearer tokens and the actors each may author ops as (see `admin`). The HTTP contract is `docs/hub-api.md`.
//!
//! Environment: `DATABASE_URL` (required; a Postgres URL) and `PORT`
//! (default 8080; Railway sets it).

mod admin;
mod auth;
mod migrate;
mod numbers;
mod ops;
mod pull;
mod views;

use std::env;
use std::error::Error;
use std::net::Ipv6Addr;
use std::process::ExitCode;
use std::sync::Arc;

use axum::extract::{DefaultBodyLimit, FromRef, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router, middleware};
use clap::{Parser, Subcommand};
use serde::Serialize;
use tokio::sync::Mutex;
use tokio_postgres::{Client, NoTls};

const DEFAULT_PORT: u16 = 8080;

/// pm-hub - the pm sync hub. With no subcommand, serve.
#[derive(Parser, Debug)]
#[command(version)]
struct Cli {
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Manage bearer tokens (run where DATABASE_URL reaches the hub's Postgres)
    #[command(subcommand)]
    Token(TokenCmd),
}

#[derive(Subcommand, Debug)]
enum TokenCmd {
    /// Mint a token for a machine or agent; prints it once on stdout (creates the workspace if new)
    Create {
        /// What holds the token, e.g. `studio` or `claude:pm-build`
        name: String,
        /// Workspace the token grants access to
        #[arg(long, value_name = "ID")]
        workspace: String,
        /// Actors the token may author ops as: an actor (`matt`), a prefix
        /// ending in `*` (`claude:*`) or `*`; repeat or comma-separate.
        /// Without it the token may act as any actor
        #[arg(long = "actor", value_name = "PATTERN")]
        actors: Vec<String>,
    },
    /// Restrict (or re-bind) the actors a token may author ops as, by the id `token list` shows
    Bind {
        id: i64,
        /// Actor patterns, as for `create --actor` (`*` = any actor)
        #[arg(long = "actor", value_name = "PATTERN", required = true)]
        actors: Vec<String>,
    },
    /// List tokens (never their secrets)
    List {
        /// Only this workspace's tokens
        #[arg(long, value_name = "ID")]
        workspace: Option<String>,
    },
    /// Revoke a token by the id `token list` shows
    Revoke { id: i64 },
}

/// The server's two database connections. `reader` answers auth, health
/// and pulls concurrently (tokio-postgres pipelines them); `writer` is
/// the one connection pushes run their transactions on, one at a time
/// (`ops`). Cloned into every handler.
#[derive(Clone)]
struct Db {
    reader: Arc<Client>,
    writer: Arc<Mutex<Client>>,
}

impl FromRef<Db> for Arc<Client> {
    fn from_ref(db: &Db) -> Self {
        db.reader.clone()
    }
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    let result = match cli.cmd {
        None => run().await,
        Some(Cmd::Token(cmd)) => token(cmd).await,
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("pm-hub: {e}");
            ExitCode::FAILURE
        }
    }
}

fn database_url() -> Result<String, Box<dyn Error>> {
    Ok(env::var("DATABASE_URL").map_err(|_| "DATABASE_URL is not set")?)
}

async fn token(cmd: TokenCmd) -> Result<(), Box<dyn Error>> {
    let (mut client, connection) = tokio_postgres::connect(&database_url()?, NoTls).await?;
    tokio::spawn(async move {
        if let Err(e) = connection.await {
            eprintln!("pm-hub: database connection: {e}");
        }
    });
    match cmd {
        TokenCmd::Create {
            name,
            workspace,
            actors,
        } => admin::token_create(&mut client, &name, &workspace, &actors).await,
        TokenCmd::Bind { id, actors } => admin::token_bind(&client, id, &actors).await,
        TokenCmd::List { workspace } => admin::token_list(&client, workspace.as_deref()).await,
        TokenCmd::Revoke { id } => admin::token_revoke(&client, id).await,
    }
}

/// One of the server's connections. If it drops, exit and let the
/// platform's restart policy reconnect rather than serve a hub that can
/// no longer reach its database.
async fn connect(database_url: &str) -> Result<Client, tokio_postgres::Error> {
    let (client, connection) = tokio_postgres::connect(database_url, NoTls).await?;
    tokio::spawn(async move {
        if let Err(e) = connection.await {
            eprintln!("pm-hub: database connection: {e}");
        }
        eprintln!("pm-hub: database connection closed; exiting");
        std::process::exit(1);
    });
    Ok(client)
}

async fn run() -> Result<(), Box<dyn Error>> {
    let database_url = database_url()?;
    let port = match env::var("PORT") {
        Ok(port) => port
            .parse::<u16>()
            .map_err(|_| format!("PORT must be a port number, got {port:?}"))?,
        Err(_) => DEFAULT_PORT,
    };

    let mut reader = connect(&database_url).await?;
    let version = migrate::migrate(&mut reader).await?;
    eprintln!("pm-hub: schema version {version}");
    let writer = connect(&database_url).await?;

    let app = app(Db {
        reader: Arc::new(reader),
        writer: Arc::new(Mutex::new(writer)),
    });
    // `::` is dual-stack on Linux, so this serves both Railway's public
    // (IPv4) proxy and its private (IPv6) network.
    let listener = tokio::net::TcpListener::bind((Ipv6Addr::UNSPECIFIED, port)).await?;
    eprintln!("pm-hub: listening on port {port}");
    axum::serve(listener, app).await?;
    Ok(())
}

/// The HTTP surface. Routes added above `route_layer` require a bearer
/// token for the `{workspace}` in their path; `/health` is added after it
/// and stays open. Unknown paths, wrong methods and auth failures all get
/// the same bare 404 (`auth::not_found`). Only the push route accepts a
/// body larger than axum's 2 MB default (`ops::MAX_BODY_BYTES`); axum
/// merges the two method routers registered for `/ops`, so the pull is
/// on the same path without that limit (a GET carries no body).
fn app(db: Db) -> Router {
    let routes = Router::new()
        .route("/w/{workspace}/whoami", get(whoami))
        .route(
            "/w/{workspace}/ops",
            post(ops::push).layer(DefaultBodyLimit::max(ops::MAX_BODY_BYTES)),
        )
        .route("/w/{workspace}/ops", get(pull::pull))
        .route(
            "/w/{workspace}/seeded",
            post(numbers::finish_seed).layer(DefaultBodyLimit::max(numbers::MAX_SEED_BODY_BYTES)),
        )
        .route_layer(middleware::from_fn_with_state(
            db.clone(),
            auth::require_auth,
        ))
        .route("/health", get(health))
        .fallback(auth::not_found)
        .method_not_allowed_fallback(auth::not_found)
        .with_state(db);
    // Axum sets `Allow` outside any per-route layer, so strip it from a
    // wrapper around the whole router.
    Router::new()
        .fallback_service(routes)
        .layer(middleware::map_response(auth::strip_allow))
}

#[derive(Serialize)]
struct Whoami {
    workspace: String,
    token_id: i64,
    /// The token's name from `token create <name>`, never its secret.
    name: String,
    /// Whether the workspace's seed has ended, making the hub its number
    /// authority (`numbers`): `false` means the first sync (AGT-1396)
    /// still has to seed it, or finish seeding it.
    seeded: bool,
}

/// The authenticated caller and the workspace's sync mode. A tiny probe
/// for clients (`pm hub login`, AGT-1394; the first sync, AGT-1396) and
/// the auth tests; the sync routes reuse the same layer.
async fn whoami(State(db): State<Db>, caller: auth::Authed) -> Result<Json<Whoami>, Response> {
    let seeded = numbers::is_seeded(&db.reader, &caller.workspace)
        .await
        .map_err(|e| {
            eprintln!("pm-hub: whoami: {e}");
            StatusCode::SERVICE_UNAVAILABLE.into_response()
        })?
        // The token authenticated a moment ago; answer as auth does when
        // the workspace is gone.
        .ok_or_else(|| StatusCode::NOT_FOUND.into_response())?;
    Ok(Json(Whoami {
        workspace: caller.workspace,
        token_id: caller.token_id,
        name: caller.token_label,
        seeded,
    }))
}

#[derive(Serialize)]
struct Health {
    status: &'static str,
    schema_version: i32,
    op_version: u16,
    /// Git sha this binary was built from (`PM_HUB_BUILD_SHA` at build
    /// time, see scripts/deploy-hub.sh), or "unknown".
    build: &'static str,
}

/// The build's git sha, baked in at compile time.
const BUILD_SHA: &str = match option_env!("PM_HUB_BUILD_SHA") {
    Some(sha) if !sha.is_empty() => sha,
    _ => "unknown",
};

/// 200 with the schema version read live from the database (so a hub that
/// cannot reach Postgres fails its health check), 503 otherwise.
async fn health(State(db): State<Db>) -> Result<Json<Health>, StatusCode> {
    let schema_version = migrate::schema_version(&db.reader).await.map_err(|e| {
        eprintln!("pm-hub: health: {e}");
        StatusCode::SERVICE_UNAVAILABLE
    })?;
    Ok(Json(Health {
        status: "ok",
        schema_version,
        op_version: pm_core::OP_VERSION,
        build: BUILD_SHA,
    }))
}
