//! `pm-hub`: the sync hub (projects/pm/README.md §Sync & hub). Today it
//! migrates its Postgres schema on start and serves `GET /health`; push,
//! pull, conditional ops and auth build on this.
//!
//! Environment: `DATABASE_URL` (required; a Postgres URL) and `PORT`
//! (default 8080; Railway sets it).

mod migrate;

use std::env;
use std::error::Error;
use std::net::Ipv6Addr;
use std::process::ExitCode;
use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::get;
use axum::{Json, Router};
use serde::Serialize;
use tokio_postgres::{Client, NoTls};

const DEFAULT_PORT: u16 = 8080;

#[tokio::main]
async fn main() -> ExitCode {
    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("pm-hub: {e}");
            ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<(), Box<dyn Error>> {
    let database_url = env::var("DATABASE_URL").map_err(|_| "DATABASE_URL is not set")?;
    let port = match env::var("PORT") {
        Ok(port) => port
            .parse::<u16>()
            .map_err(|_| format!("PORT must be a port number, got {port:?}"))?,
        Err(_) => DEFAULT_PORT,
    };

    let (mut client, connection) = tokio_postgres::connect(&database_url, NoTls).await?;
    // One connection for the process. If it drops, exit and let the
    // platform's restart policy reconnect rather than serve a hub that can
    // no longer reach its database.
    tokio::spawn(async move {
        if let Err(e) = connection.await {
            eprintln!("pm-hub: database connection: {e}");
        }
        eprintln!("pm-hub: database connection closed; exiting");
        std::process::exit(1);
    });

    let version = migrate::migrate(&mut client).await?;
    eprintln!("pm-hub: schema version {version}");

    let app = Router::new()
        .route("/health", get(health))
        .with_state(Arc::new(client));
    // `::` is dual-stack on Linux, so this serves both Railway's public
    // (IPv4) proxy and its private (IPv6) network.
    let listener = tokio::net::TcpListener::bind((Ipv6Addr::UNSPECIFIED, port)).await?;
    eprintln!("pm-hub: listening on port {port}");
    axum::serve(listener, app).await?;
    Ok(())
}

#[derive(Serialize)]
struct Health {
    status: &'static str,
    schema_version: i32,
    op_version: u16,
}

/// 200 with the schema version read live from the database (so a hub that
/// cannot reach Postgres fails its health check), 503 otherwise.
async fn health(State(db): State<Arc<Client>>) -> Result<Json<Health>, StatusCode> {
    let schema_version = migrate::schema_version(&db).await.map_err(|e| {
        eprintln!("pm-hub: health: {e}");
        StatusCode::SERVICE_UNAVAILABLE
    })?;
    Ok(Json(Health {
        status: "ok",
        schema_version,
        op_version: pm_core::OP_VERSION,
    }))
}
