//! End-to-end: run the real `pm-hub` binary against a throwaway Postgres
//! and check that it migrates on start and serves `GET /health` (AGT-1387).
//!
//! Postgres comes from, in order:
//! 1. `PM_HUB_TEST_DATABASE_URL` — a disposable database you provide (the
//!    hub will create its tables in it);
//! 2. a `postgres` container started with docker (removed afterwards);
//! 3. neither — the database-backed test is skipped with a message.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::thread::sleep;
use std::time::{Duration, Instant};

const POSTGRES_IMAGE: &str = "postgres:17-alpine";
const PASSWORD: &str = "pm-hub-test";

/// A docker container removed when dropped.
struct Container(String);

impl Drop for Container {
    fn drop(&mut self) {
        let _ = Command::new("docker")
            .args(["rm", "-f", &self.0])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

/// A running hub process, killed when dropped.
struct Hub(Child);

impl Drop for Hub {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn docker_available() -> bool {
    Command::new("docker")
        .arg("info")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// Starts a Postgres container on a random host port and waits until it
/// accepts queries over TCP.
fn start_postgres() -> (Container, String) {
    let out = Command::new("docker")
        .args([
            "run",
            "-d",
            "--rm",
            "-e",
            &format!("POSTGRES_PASSWORD={PASSWORD}"),
            "-p",
            "127.0.0.1::5432",
            POSTGRES_IMAGE,
        ])
        .output()
        .expect("running docker");
    assert!(
        out.status.success(),
        "docker run failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let container = Container(String::from_utf8(out.stdout).unwrap().trim().to_string());
    let out = Command::new("docker")
        .args(["port", &container.0, "5432/tcp"])
        .output()
        .expect("running docker port");
    let mapping = String::from_utf8(out.stdout).unwrap();
    let port = mapping
        .lines()
        .next()
        .and_then(|l| l.rsplit(':').next())
        .unwrap_or_else(|| panic!("no port mapping in {mapping:?}"))
        .trim()
        .to_string();
    let url = format!("postgres://postgres:{PASSWORD}@127.0.0.1:{port}/postgres");
    // The image's entrypoint runs a socket-only server for init, then
    // restarts on TCP, so a successful TCP query means it is really up.
    let deadline = Instant::now() + Duration::from_secs(60);
    while query_rows(&url, "SELECT 1").is_err() {
        assert!(Instant::now() < deadline, "postgres did not become ready");
        sleep(Duration::from_millis(250));
    }
    (container, url)
}

/// Runs `sql` and returns each row's columns as optional strings.
fn query_rows(url: &str, sql: &str) -> Result<Vec<Vec<Option<String>>>, tokio_postgres::Error> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let (client, connection) = tokio_postgres::connect(url, tokio_postgres::NoTls).await?;
        tokio::spawn(connection);
        let rows = client.simple_query(sql).await?;
        Ok(rows
            .into_iter()
            .filter_map(|m| match m {
                tokio_postgres::SimpleQueryMessage::Row(row) => Some(
                    (0..row.len())
                        .map(|i| row.get(i).map(str::to_string))
                        .collect(),
                ),
                _ => None,
            })
            .collect())
    })
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn spawn_hub(database_url: &str, port: u16) -> Hub {
    Hub(Command::new(env!("CARGO_BIN_EXE_pm-hub"))
        .env("DATABASE_URL", database_url)
        .env("PORT", port.to_string())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("spawning pm-hub"))
}

/// A minimal HTTP/1.1 GET: `(status, body)`, or `None` if nothing is
/// listening yet.
fn get(port: u16, path: &str) -> Option<(u16, String)> {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).ok()?;
    write!(
        stream,
        "GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n"
    )
    .ok()?;
    let mut response = String::new();
    stream.read_to_string(&mut response).ok()?;
    let status = response.split_whitespace().nth(1)?.parse().ok()?;
    let body = response.split_once("\r\n\r\n")?.1.to_string();
    Some((status, body))
}

/// Polls `/health` until the hub answers, failing if it exits first.
fn wait_for_health(hub: &mut Hub, port: u16) -> (u16, serde_json::Value) {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if let Some(status) = hub.0.try_wait().unwrap() {
            panic!("pm-hub exited before serving /health: {status}");
        }
        if let Some((status, body)) = get(port, "/health") {
            let json = serde_json::from_str(&body)
                .unwrap_or_else(|e| panic!("/health body {body:?} is not JSON: {e}"));
            return (status, json);
        }
        assert!(Instant::now() < deadline, "pm-hub never served /health");
        sleep(Duration::from_millis(100));
    }
}

/// One migration file per schema version, so the newest version is the
/// number of `.sql` files in `migrations/`.
fn expected_schema_version() -> u64 {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("migrations");
    std::fs::read_dir(dir)
        .unwrap()
        .filter(|e| {
            e.as_ref()
                .unwrap()
                .path()
                .extension()
                .is_some_and(|x| x == "sql")
        })
        .count() as u64
}

#[test]
fn migrates_on_start_and_serves_health() {
    let (_container, url) = match std::env::var("PM_HUB_TEST_DATABASE_URL") {
        Ok(url) => (None, url),
        Err(_) if docker_available() => {
            let (container, url) = start_postgres();
            (Some(container), url)
        }
        Err(_) => {
            eprintln!(
                "SKIPPED migrates_on_start_and_serves_health: no Postgres available \
                 (set PM_HUB_TEST_DATABASE_URL to a disposable database, or start docker)"
            );
            return;
        }
    };

    // First start applies every migration.
    let port = free_port();
    let mut hub = spawn_hub(&url, port);
    let (status, health) = wait_for_health(&mut hub, port);
    assert_eq!(status, 200, "{health}");
    assert_eq!(health["status"], "ok");
    assert_eq!(health["schema_version"], expected_schema_version());
    assert_eq!(health["op_version"], pm_core::OP_VERSION);
    drop(hub);

    let tables = query_rows(
        &url,
        "SELECT table_name FROM information_schema.tables
         WHERE table_schema = 'public' ORDER BY table_name",
    )
    .unwrap();
    let tables: Vec<&str> = tables.iter().filter_map(|r| r[0].as_deref()).collect();
    assert_eq!(tables, ["ops", "schema_version", "tokens", "workspaces"]);
    let seq = query_rows(
        &url,
        "SELECT data_type, column_default FROM information_schema.columns
         WHERE table_name = 'ops' AND column_name = 'seq'",
    )
    .unwrap();
    assert_eq!(seq[0][0].as_deref(), Some("bigint"));
    assert!(
        seq[0][1]
            .as_deref()
            .is_some_and(|d| d.starts_with("nextval(")),
        "ops.seq is not a bigserial: {seq:?}"
    );

    // A restart against the migrated database is a no-op migration.
    let port = free_port();
    let mut hub = spawn_hub(&url, port);
    let (status, health) = wait_for_health(&mut hub, port);
    assert_eq!(status, 200, "{health}");
    assert_eq!(health["schema_version"], expected_schema_version());
    let applied = query_rows(&url, "SELECT count(*) FROM schema_version").unwrap();
    assert_eq!(
        applied[0][0].as_deref(),
        Some(expected_schema_version().to_string().as_str())
    );
}

#[test]
fn refuses_to_start_without_database_url() {
    let out = Command::new(env!("CARGO_BIN_EXE_pm-hub"))
        .env_remove("DATABASE_URL")
        .output()
        .expect("running pm-hub");
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("DATABASE_URL is not set"), "{stderr}");
}
