//! `pm hub login|status|logout` (AGT-1394). Everything runs in a temp HOME
//! against a temp workspace, and the keychain service prefix is overridden
//! (`PM_HUB_KEYCHAIN_SERVICE_PREFIX=pm-hub-test.<random>`) so no real
//! keychain item is ever read or written. The "hub" is a throwaway loopback
//! server answering `/health` and `/w/<id>/whoami`.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::Value;
use tempfile::TempDir;

const TOKEN: &str = "pmh_test-Token_0123456789";

struct Sandbox {
    home: TempDir,
    ws: PathBuf,
    prefix: String,
    /// A throwaway keychain file (macOS): the real one is never touched.
    keychain: Option<PathBuf>,
}

impl Sandbox {
    fn new() -> Self {
        let home = tempfile::tempdir().unwrap();
        let ws = home.path().join("ws");
        let nonce = ulid::Ulid::new().to_string();
        let sb = Sandbox {
            home,
            ws,
            prefix: format!("pm-hub-test.{nonce}"),
            keychain: None,
        };
        let mut sb = sb;
        if cfg!(target_os = "macos") {
            let kc = sb.home.path().join("pm-hub-test.keychain-db");
            let run = |args: &[&str]| {
                let out = Command::new("security")
                    .args(args)
                    .env("HOME", sb.home.path())
                    .output()
                    .unwrap();
                assert!(out.status.success(), "security {args:?}: {}", err(&out));
            };
            run(&["create-keychain", "-p", "pw", kc.to_str().unwrap()]);
            run(&["unlock-keychain", "-p", "pw", kc.to_str().unwrap()]);
            sb.keychain = Some(kc);
        }
        let out = sb.pm(&["init", "--prefix", "T"], &[], None);
        assert!(out.status.success(), "init: {}", err(&out));
        sb
    }

    fn config(&self) -> PathBuf {
        self.home.path().join(".config/pm/config.toml")
    }

    fn config_text(&self) -> String {
        std::fs::read_to_string(self.config()).unwrap_or_default()
    }

    fn pm(&self, args: &[&str], extra: &[(&str, &str)], stdin: Option<&str>) -> Output {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_pm"));
        cmd.args(["--workspace", self.ws.to_str().unwrap()])
            .args(args)
            .env_clear()
            .env("HOME", self.home.path())
            .env("USER", "tester")
            .env("TMPDIR", self.home.path())
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("PM_HUB_KEYCHAIN_SERVICE_PREFIX", &self.prefix)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(kc) = &self.keychain {
            cmd.env("PM_HUB_KEYCHAIN", kc);
        }
        for (k, v) in extra {
            cmd.env(k, v);
        }
        let mut child = cmd.spawn().unwrap();
        let mut pipe = child.stdin.take().unwrap();
        if let Some(text) = stdin {
            pipe.write_all(text.as_bytes()).unwrap();
        }
        drop(pipe);
        child.wait_with_output().unwrap()
    }

    fn json(&self, args: &[&str], extra: &[(&str, &str)]) -> (i32, Value) {
        let mut a = args.to_vec();
        a.push("--json");
        let out = self.pm(&a, extra, None);
        let v = serde_json::from_slice(&out.stdout)
            .unwrap_or_else(|e| panic!("not JSON ({e}): {}\n{}", out_s(&out), err(&out)));
        (out.status.code().unwrap(), v)
    }
}

impl Drop for Sandbox {
    /// Whatever a test left in the keychain goes, via the real code path.
    fn drop(&mut self) {
        let _ = self.pm(&["hub", "logout"], &[], None);
        if let Some(kc) = &self.keychain {
            let _ = Command::new("security")
                .args(["delete-keychain", kc.to_str().unwrap()])
                .env("HOME", self.home.path())
                .output();
        }
    }
}

fn out_s(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}
fn err(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

/// A loopback hub. `accepted` counts connections; `token` is the one bearer
/// token whoami accepts (anything else gets the hub's bare 404).
struct FakeHub {
    url: String,
    accepted: Arc<AtomicUsize>,
    stop: Arc<AtomicBool>,
}

impl FakeHub {
    fn start(token: &'static str) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let accepted = Arc::new(AtomicUsize::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let (a, s) = (accepted.clone(), stop.clone());
        thread::spawn(move || {
            while !s.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        a.fetch_add(1, Ordering::SeqCst);
                        serve(stream, token);
                    }
                    Err(_) => thread::sleep(Duration::from_millis(5)),
                }
            }
        });
        FakeHub {
            url,
            accepted,
            stop,
        }
    }

    fn hits(&self) -> usize {
        self.accepted.load(Ordering::SeqCst)
    }
}

impl Drop for FakeHub {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}

fn serve(mut stream: TcpStream, token: &str) {
    stream.set_nonblocking(false).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    let mut buf = [0u8; 4096];
    let n = stream.read(&mut buf).unwrap_or(0);
    let req = String::from_utf8_lossy(&buf[..n]).into_owned();
    let path = req.split_whitespace().nth(1).unwrap_or("");
    let authed = req.to_ascii_lowercase().contains(&format!(
        "authorization: bearer {}",
        token.to_ascii_lowercase()
    ));
    let (status, body) = if path == "/health" {
        (
            "200 OK",
            r#"{"status":"ok","schema_version":1,"op_version":5}"#.to_string(),
        )
    } else if path.starts_with("/w/") && path.ends_with("/whoami") && authed {
        (
            "200 OK",
            r#"{"workspace":"w","token_id":7,"name":"laptop"}"#.to_string(),
        )
    } else {
        ("404 Not Found", String::new())
    };
    let _ = write!(
        stream,
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
}

fn no_secret_leaked(sb: &Sandbox, outs: &[&Output]) {
    for o in outs {
        assert!(!out_s(o).contains(TOKEN), "token on stdout");
        assert!(!err(o).contains(TOKEN), "token on stderr");
    }
    assert!(!sb.config_text().contains(TOKEN), "token in config.toml");
}

#[test]
fn login_status_logout_roundtrip_on_env_token() {
    // PM_HUB_TOKEN path: no keychain involved on any platform.
    let hub = FakeHub::start(TOKEN);
    let sb = Sandbox::new();
    let login = sb.pm(
        &["hub", "login", &hub.url],
        &[("PM_HUB_TOKEN", TOKEN)],
        None,
    );
    // On macOS the env token is also stored in the (throwaway) keychain
    // item; logout below removes it.
    assert!(login.status.success(), "{}", err(&login));
    assert!(sb.config_text().contains(&format!("hub = \"{}\"", hub.url)));

    let (code, v) = sb.json(&["hub", "status"], &[("PM_HUB_TOKEN", TOKEN)]);
    assert_eq!(code, 0, "{v}");
    assert_eq!(v["reachable"], true);
    assert_eq!(v["health"]["schema_version"], 1);
    assert_eq!(v["token"]["present"], true);
    assert_eq!(v["token"]["source"], "env");
    assert_eq!(v["token_accepted"], true);
    assert_eq!(v["token_name"], "laptop");

    let text = sb.pm(&["hub", "status"], &[("PM_HUB_TOKEN", TOKEN)], None);
    no_secret_leaked(&sb, &[&login, &text]);

    let logout = sb.pm(&["hub", "logout"], &[], None);
    assert!(logout.status.success(), "{}", err(&logout));
    assert!(!sb.config_text().contains("hub ="));
    let (code, v) = sb.json(&["hub", "status"], &[]);
    assert_eq!(code, 0);
    assert_eq!(v["configured"], false);
    assert_eq!(v["token"]["present"], false);
}

#[cfg(target_os = "macos")]
#[test]
fn token_lives_in_the_keychain_not_config() {
    let hub = FakeHub::start(TOKEN);
    let sb = Sandbox::new();
    // Token on stdin, no PM_HUB_TOKEN.
    let login = sb.pm(
        &["hub", "login", &hub.url],
        &[],
        Some(&format!("{TOKEN}\n")),
    );
    assert!(login.status.success(), "{}", err(&login));
    no_secret_leaked(&sb, &[&login]);

    let (code, v) = sb.json(&["hub", "status"], &[]);
    assert_eq!(code, 0, "{v}");
    assert_eq!(v["token"]["source"], "keychain");
    assert_eq!(v["token_accepted"], true);
    let service = v["keychain_service"].as_str().unwrap().to_string();
    assert!(service.starts_with("pm-hub-test."), "{service}");

    let found = Command::new("security")
        .args(["find-generic-password", "-s", &service, "-a", "pm", "-w"])
        .arg(sb.keychain.as_ref().unwrap())
        .env("HOME", sb.home.path())
        .output()
        .unwrap();
    assert_eq!(out_s(&found).trim(), TOKEN);

    // Re-login updates in place.
    let login = sb.pm(&["hub", "login", &hub.url], &[], Some("second-token\n"));
    assert!(login.status.success(), "{}", err(&login));
    let found = Command::new("security")
        .args(["find-generic-password", "-s", &service, "-a", "pm", "-w"])
        .arg(sb.keychain.as_ref().unwrap())
        .env("HOME", sb.home.path())
        .output()
        .unwrap();
    assert_eq!(out_s(&found).trim(), "second-token");

    let (code, v) = sb.json(&["hub", "logout"], &[]);
    assert_eq!(code, 0);
    assert_eq!(v["token_removed"], true);
    assert_eq!(v["hub_removed"], true);
    let gone = Command::new("security")
        .args(["find-generic-password", "-s", &service, "-a", "pm", "-w"])
        .arg(sb.keychain.as_ref().unwrap())
        .env("HOME", sb.home.path())
        .output()
        .unwrap();
    assert!(!gone.status.success(), "item survived logout");
    // Logout twice is fine.
    let (code, v) = sb.json(&["hub", "logout"], &[]);
    assert_eq!(
        (code, &v["token_removed"], &v["hub_removed"]),
        (0, &Value::Bool(false), &Value::Bool(false))
    );
}

#[test]
fn login_preserves_other_config_keys() {
    let sb = Sandbox::new();
    std::fs::create_dir_all(sb.config().parent().unwrap()).unwrap();
    std::fs::write(
        sb.config(),
        "workspace = \"/w\"\n[edit]\nview = \"editor\"\n",
    )
    .unwrap();
    let out = sb.pm(
        &["hub", "login", "https://hub.example/"],
        &[("PM_HUB_TOKEN", TOKEN)],
        None,
    );
    assert!(out.status.success(), "{}", err(&out));
    let text = sb.config_text();
    assert!(text.contains("workspace = \"/w\""), "{text}");
    assert!(text.contains("view = \"editor\""), "{text}");
    assert!(text.contains("hub = \"https://hub.example\""), "{text}");
    let out = sb.pm(&["hub", "logout"], &[], None);
    assert!(out.status.success());
    let text = sb.config_text();
    assert!(
        text.contains("workspace = \"/w\"") && !text.contains("hub"),
        "{text}"
    );
}

#[test]
fn rejected_token_and_unreachable_hub_exit_1() {
    let hub = FakeHub::start(TOKEN);
    let sb = Sandbox::new();
    let out = sb.pm(
        &["hub", "login", &hub.url],
        &[("PM_HUB_TOKEN", TOKEN)],
        None,
    );
    assert!(out.status.success());

    let (code, v) = sb.json(&["hub", "status"], &[("PM_HUB_TOKEN", "wrong-token")]);
    assert_eq!(code, 1);
    assert_eq!(v["reachable"], true);
    assert_eq!(v["token_accepted"], false);

    drop(hub);
    thread::sleep(Duration::from_millis(50));
    // Nothing listens on this port now (the listener thread exited).
    let dead = "http://127.0.0.1:1";
    let out = sb.pm(&["hub", "login", dead], &[("PM_HUB_TOKEN", TOKEN)], None);
    assert!(out.status.success());
    let (code, v) = sb.json(&["hub", "status"], &[("PM_HUB_TOKEN", TOKEN)]);
    assert_eq!(code, 1);
    assert_eq!(v["reachable"], false);
    assert!(v["error"].is_string());
}

#[test]
fn bad_url_or_token_is_a_usage_error_and_writes_nothing() {
    let sb = Sandbox::new();
    for url in ["hub.example", "https://user:pw@hub.example", "ftp://x"] {
        let out = sb.pm(&["hub", "login", url], &[("PM_HUB_TOKEN", TOKEN)], None);
        assert_eq!(out.status.code(), Some(2), "{url}: {}", err(&out));
    }
    let out = sb.pm(
        &["hub", "login", "https://hub.example"],
        &[("PM_HUB_TOKEN", "has space\"quote")],
        None,
    );
    assert_eq!(out.status.code(), Some(2), "{}", err(&out));
    assert!(
        !sb.config_text().contains("hub"),
        "config written on failure"
    );
}

/// AC3: reads never contact the hub. A listener stands in for a configured
/// hub; every read verb runs; nothing may connect.
#[test]
fn reads_never_contact_the_hub() {
    let hub = FakeHub::start(TOKEN);
    let sb = Sandbox::new();
    let out = sb.pm(&["new", "--title", "a ticket"], &[], None);
    assert!(out.status.success(), "{}", err(&out));
    let out = sb.pm(
        &["hub", "login", &hub.url],
        &[("PM_HUB_TOKEN", TOKEN)],
        None,
    );
    assert!(out.status.success(), "{}", err(&out));
    assert_eq!(hub.hits(), 0, "login must not contact the hub");

    let started = Instant::now();
    for args in [
        &["list"][..],
        &["show", "T-1"],
        &["ready"],
        &["status"],
        &["graph"],
        &["log"],
        &["check"],
        &["holds"],
        &["doctor"],
    ] {
        let out = sb.pm(args, &[("PM_HUB_TOKEN", TOKEN)], None);
        assert!(
            out.status.success() || args[0] == "check",
            "pm {args:?}: {}",
            err(&out)
        );
    }
    assert!(started.elapsed() < Duration::from_secs(20));
    assert_eq!(hub.hits(), 0, "a read verb contacted the hub");

    // And, for contrast, status does.
    let out = sb.pm(&["hub", "status"], &[("PM_HUB_TOKEN", TOKEN)], None);
    assert!(out.status.success(), "{}", err(&out));
    assert!(hub.hits() >= 2);
}
