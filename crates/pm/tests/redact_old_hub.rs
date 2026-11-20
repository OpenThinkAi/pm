//! AGT-2551: before `pm sync` pushes a `redact`, it asks the hub's open
//! `GET /health` for its `schema_version` (once a round) and refuses with
//! an "upgrade the hub" error when the hub is older than schema 6, rather
//! than sending the op and reading the hub's parse error. Only when
//! `/health` gives no answer does the push go out as before, the parse
//! error being the fallback.
//!
//! The hub here is a loopback fake that records what it is sent; each
//! replica has its own temp HOME and takes its token from `PM_HUB_TOKEN`,
//! so no real config, keychain or workspace is touched.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;

use serde_json::{Value, json};
use tempfile::TempDir;

/// What the fake hub's `/health` does.
#[derive(Clone, Copy)]
enum Health {
    /// Reports `schema_version` 5: a hub from before redaction.
    Old,
    /// Reports `schema_version` 6: redaction is in.
    New,
    /// Hangs up without answering: `/health` is unreachable. The hub then
    /// behaves as a pre-redaction one, refusing a redact with a parse error.
    Silent,
}

/// A loopback hub: seed mode until `POST …/seeded`, every pushed op
/// acknowledged except a `redact` when it is too old to know one (the
/// serde error a pre-redaction hub answers), empty pull pages.
struct FakeHub {
    url: String,
    /// The `kind` of every op a `POST …/ops` carried, in order.
    pushed: Arc<Mutex<Vec<String>>>,
    health_hits: Arc<AtomicUsize>,
}

impl FakeHub {
    fn start(health: Health) -> FakeHub {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let pushed = Arc::new(Mutex::new(Vec::new()));
        let health_hits = Arc::new(AtomicUsize::new(0));
        let seeded = Arc::new(AtomicBool::new(false));
        let (p, h) = (pushed.clone(), health_hits.clone());
        thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let (method, path, body) = read_request(&mut stream);
                let (status, reply) = if path == "/health" {
                    h.fetch_add(1, Ordering::SeqCst);
                    match health {
                        Health::Old => (
                            200,
                            r#"{"status":"ok","schema_version":5,"op_version":1}"#.to_string(),
                        ),
                        Health::New => (
                            200,
                            r#"{"status":"ok","schema_version":6,"op_version":1}"#.to_string(),
                        ),
                        Health::Silent => continue,
                    }
                } else if method == "GET" && path.ends_with("/whoami") {
                    let s = seeded.load(Ordering::SeqCst);
                    (
                        200,
                        json!({"workspace": "w", "token_id": 1, "name": "t", "seeded": s})
                            .to_string(),
                    )
                } else if method == "POST" && path.ends_with("/seeded") {
                    seeded.store(true, Ordering::SeqCst);
                    let req: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
                    (
                        200,
                        json!({"number_floor": req["number_floor"], "numbers": []}).to_string(),
                    )
                } else if method == "POST" && path.ends_with("/ops") {
                    let batch: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
                    let ops = batch["ops"].as_array().cloned().unwrap_or_default();
                    let kinds: Vec<String> = ops
                        .iter()
                        .map(|op| op["kind"].as_str().unwrap_or("").to_string())
                        .collect();
                    let has_redact = kinds.iter().any(|k| k == "redact");
                    p.lock().unwrap().extend(kinds);
                    if has_redact && !matches!(health, Health::New) {
                        (
                            400,
                            json!({"error": "invalid_json", "message": "ops[0]: unknown variant `redact`, expected one of `ticket.create`"}).to_string(),
                        )
                    } else {
                        let acks: Vec<Value> = ops
                            .iter()
                            .enumerate()
                            .map(|(i, op)| json!({"op_id": op["op_id"], "seq": i + 1, "stored": true}))
                            .collect();
                        (200, json!({"ops": acks}).to_string())
                    }
                } else if method == "GET" && path.contains("/ops") {
                    let since: i64 = path
                        .split_once('?')
                        .map_or("", |(_, q)| q)
                        .split('&')
                        .find_map(|kv| kv.strip_prefix("since="))
                        .and_then(|v| v.parse().ok())
                        .unwrap_or(0);
                    (
                        200,
                        json!({"ops": [], "next": since, "head": since}).to_string(),
                    )
                } else {
                    (404, String::new())
                };
                let _ = write!(
                    stream,
                    "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}",
                    reply.len()
                );
            }
        });
        FakeHub {
            url,
            pushed,
            health_hits,
        }
    }

    fn pushed_redacts(&self) -> usize {
        self.pushed
            .lock()
            .unwrap()
            .iter()
            .filter(|k| *k == "redact")
            .count()
    }

    fn health_hits(&self) -> usize {
        self.health_hits.load(Ordering::SeqCst)
    }
}

/// `(method, path, body)`: headers, then exactly Content-Length body bytes.
fn read_request(stream: &mut std::net::TcpStream) -> (String, String, Vec<u8>) {
    let mut raw = Vec::new();
    let mut buf = [0u8; 8192];
    let (head_len, body_len) = loop {
        let n = stream.read(&mut buf).unwrap_or(0);
        if n == 0 {
            break (raw.len(), 0);
        }
        raw.extend_from_slice(&buf[..n]);
        if let Some(pos) = raw.windows(4).position(|w| w == b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&raw[..pos]).into_owned();
            let len = head
                .lines()
                .find_map(|l| {
                    l.split_once(':')
                        .filter(|(k, _)| k.eq_ignore_ascii_case("content-length"))
                })
                .and_then(|(_, v)| v.trim().parse::<usize>().ok())
                .unwrap_or(0);
            break (pos + 4, len);
        }
    };
    while raw.len() < head_len + body_len {
        let n = stream.read(&mut buf).unwrap_or(0);
        if n == 0 {
            break;
        }
        raw.extend_from_slice(&buf[..n]);
    }
    let head = String::from_utf8_lossy(&raw[..head_len]).into_owned();
    let mut line = head.lines().next().unwrap_or("").split_whitespace();
    let method = line.next().unwrap_or("").to_string();
    let path = line.next().unwrap_or("").to_string();
    let end = (head_len + body_len).min(raw.len());
    (method, path, raw[head_len..end].to_vec())
}

/// One machine with its own HOME and workspace.
struct Replica {
    home: TempDir,
    ws: PathBuf,
    hub: Option<String>,
}

impl Replica {
    fn init() -> Replica {
        let home = tempfile::tempdir().unwrap();
        let ws = home.path().join("ws");
        let r = Replica {
            home,
            ws,
            hub: None,
        };
        r.ok(&["init", "--prefix", "T"]);
        r
    }

    fn configure(&mut self, hub_url: &str) {
        let path = self.home.path().join(".config/pm/config.toml");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, format!("hub = \"{hub_url}\"\n")).unwrap();
        self.hub = Some(hub_url.to_string());
    }

    fn pm(&self, args: &[&str]) -> Output {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_pm"));
        cmd.args(["--workspace", self.ws.to_str().unwrap()])
            .args(args)
            .env_clear()
            .env("HOME", self.home.path())
            .env("USER", "tester")
            .env("PM_ACTOR", "alice")
            .env("TMPDIR", self.home.path())
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .stdin(Stdio::null());
        if self.hub.is_some() {
            cmd.env("PM_HUB_TOKEN", "pmh_test-token");
        }
        cmd.output().unwrap()
    }

    fn ok(&self, args: &[&str]) -> Output {
        let out = self.pm(args);
        assert!(out.status.success(), "pm {args:?}: {}", err(&out));
        out
    }
}

fn err(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

/// A replica seeded into a fresh `hub`, with a redact of its one comment
/// waiting in the outbox. The seed round pushed no redact, so it never
/// asked `/health`.
fn replica_with_pending_redact(hub: &FakeHub) -> Replica {
    let mut r = Replica::init();
    r.ok(&["new", "--title", "shared"]);
    r.ok(&["comment", "T-1", "a secret"]);
    r.configure(&hub.url);
    r.ok(&["sync"]);
    assert_eq!(hub.health_hits(), 0, "no redact to push, no /health probe");
    r.ok(&["redact", "T-1", "--comment", "1"]);
    r
}

#[test]
fn an_old_hub_schema_refuses_the_redact_before_it_is_pushed() {
    let hub = FakeHub::start(Health::Old);
    let r = replica_with_pending_redact(&hub);
    let out = r.pm(&["sync"]);
    assert!(!out.status.success());
    let e = err(&out);
    assert!(
        e.contains("predates redaction (it reports schema 5)"),
        "{e}"
    );
    assert!(e.contains("upgrade pm-hub (schema 6 or later)"), "{e}");
    assert_eq!(
        hub.pushed_redacts(),
        0,
        "the redact must not reach an old hub"
    );
    assert_eq!(hub.health_hits(), 1);
}

#[test]
fn a_new_hub_schema_lets_the_redact_go_up() {
    let hub = FakeHub::start(Health::New);
    let r = replica_with_pending_redact(&hub);
    // Two more comments' redacts, so one round pushes several: /health is
    // still asked once.
    r.ok(&["comment", "T-1", "another"]);
    r.ok(&["redact", "T-1", "--comment", "2"]);
    r.ok(&["sync"]);
    assert_eq!(hub.pushed_redacts(), 2);
    assert_eq!(hub.health_hits(), 1, "probed once per round");
}

#[test]
fn an_unreachable_health_falls_back_to_the_hubs_parse_error() {
    let hub = FakeHub::start(Health::Silent);
    let r = replica_with_pending_redact(&hub);
    let out = r.pm(&["sync"]);
    assert!(!out.status.success());
    let e = err(&out);
    assert!(
        e.contains("the hub predates redaction and cannot store a redact op"),
        "{e}"
    );
    assert!(!e.contains("it reports schema"), "{e}");
    assert_eq!(hub.pushed_redacts(), 1, "pushed as before /health existed");
    assert_eq!(hub.health_hits(), 1);
}
