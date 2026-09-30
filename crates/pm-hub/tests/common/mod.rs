//! Shared harness for the pm-hub integration tests: a throwaway Postgres
//! and the real `pm-hub` binary.
//!
//! Postgres comes from, in order:
//! 1. `PM_HUB_TEST_DATABASE_URL` — a disposable database you provide (the
//!    hub will create its tables in it);
//! 2. a `postgres` container started with docker (removed afterwards);
//! 3. neither — database-backed tests are skipped with a message.

#![allow(dead_code)] // each test binary uses a different subset

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::thread::sleep;
use std::time::{Duration, Instant};

const POSTGRES_IMAGE: &str = "postgres:17-alpine";
const PASSWORD: &str = "pm-hub-test";

/// A docker container removed when dropped.
pub struct Container(String);

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
pub struct Hub(pub Child);

impl Drop for Hub {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

pub fn docker_available() -> bool {
    Command::new("docker")
        .arg("info")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// Starts a Postgres container on a random host port and waits until it
/// accepts queries over TCP.
pub fn start_postgres() -> (Container, String) {
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
pub fn query_rows(url: &str, sql: &str) -> Result<Vec<Vec<Option<String>>>, tokio_postgres::Error> {
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

pub fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

pub fn spawn_hub(database_url: &str, port: u16) -> Hub {
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
pub fn get(port: u16, path: &str) -> Option<(u16, String)> {
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
pub fn wait_for_health(hub: &mut Hub, port: u16) -> (u16, serde_json::Value) {
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
pub fn expected_schema_version() -> u64 {
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

/// A disposable Postgres URL (and the container backing it, if any), or
/// `None` after printing why `test` is skipped.
pub fn postgres_for(test: &str) -> Option<(Option<Container>, String)> {
    match std::env::var("PM_HUB_TEST_DATABASE_URL") {
        Ok(url) => Some((None, url)),
        Err(_) if docker_available() => {
            let (container, url) = start_postgres();
            Some((Some(container), url))
        }
        Err(_) => {
            eprintln!(
                "SKIPPED {test}: no Postgres available \
                 (set PM_HUB_TEST_DATABASE_URL to a disposable database, or start docker)"
            );
            None
        }
    }
}

/// A raw HTTP/1.1 response with the `date` header dropped, so two
/// responses can be compared byte for byte.
#[derive(Debug, PartialEq, Eq)]
pub struct Response {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: String,
}

/// One HTTP/1.1 request with extra header lines (`"Name: value"`).
pub fn request(port: u16, method: &str, path: &str, headers: &[&str]) -> Response {
    request_body(port, method, path, headers, b"")
}

/// [`request`] with a body (`Content-Length` set from it on non-GETs).
pub fn request_body(
    port: u16,
    method: &str,
    path: &str,
    headers: &[&str],
    body: &[u8],
) -> Response {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("connecting to pm-hub");
    let mut req = format!("{method} {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n");
    for h in headers {
        req.push_str(h);
        req.push_str("\r\n");
    }
    if method != "GET" {
        req.push_str(&format!("Content-Length: {}\r\n", body.len()));
    }
    req.push_str("\r\n");
    stream.write_all(req.as_bytes()).unwrap();
    stream.write_all(body).unwrap();
    let mut raw = String::new();
    stream.read_to_string(&mut raw).unwrap();
    let (head, body) = raw.split_once("\r\n\r\n").expect("HTTP response");
    let mut lines = head.lines();
    let status = lines
        .next()
        .unwrap()
        .split_whitespace()
        .nth(1)
        .unwrap()
        .parse()
        .unwrap();
    let headers = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_string()))
        .filter(|(k, _)| k != "date")
        .collect();
    Response {
        status,
        headers,
        body: body.to_string(),
    }
}

/// Runs `pm-hub <args>` against `database_url`: `(success, stdout, stderr)`.
pub fn admin(database_url: &str, args: &[&str]) -> (bool, String, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_pm-hub"))
        .env("DATABASE_URL", database_url)
        .args(args)
        .output()
        .expect("running pm-hub");
    (
        out.status.success(),
        String::from_utf8(out.stdout).unwrap(),
        String::from_utf8(out.stderr).unwrap(),
    )
}
