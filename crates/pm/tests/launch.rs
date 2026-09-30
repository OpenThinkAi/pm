//! The ui-leaf launcher (AGT-1402): `pm edit` and `pm app` against a
//! **fake** ui-leaf — a shell script that speaks ui-leaf's stdio protocol
//! and records what pm gave it (argv, environment, the mount config, the
//! `session` reply). No test here, or anywhere, runs the real ui-leaf or a
//! browser: `PATH` is the sandbox's fake directory plus the system
//! directories, and HOME is a temp dir (so is the workspace).

use std::io::{BufRead, BufReader, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tempfile::TempDir;

/// The fake: `--version` prints `$FAKE_UI_LEAF_VERSION` (default 1.6.0);
/// `mount` records, reports ready on port 45678, asks for `session`, asks
/// for an undeclared mutation, then either waits for pm's `close`
/// (default), exits on its own once `quit` appears (`FAKE_UI_LEAF_MODE=
/// quit`), or fails before ready (`FAKE_UI_LEAF_MODE=fail`).
const FAKE: &str = r#"#!/bin/sh
rec="__REC__"
if [ "$1" = "--version" ]; then echo "${FAKE_UI_LEAF_VERSION:-1.6.0}"; exit 0; fi
printf '%s\n' "$@" > "$rec/argv"
env > "$rec/env"
IFS= read -r config
printf '%s\n' "$config" > "$rec/config.json"
if [ "$FAKE_UI_LEAF_MODE" = fail ]; then
  echo '{"version":"1","type":"error","message":"fake: view failed to compile"}'
  exit 1
fi
echo '{"version":"1","type":"ready","url":"http://127.0.0.1:45678","port":45678}'
echo '{"version":"1","type":"mutate","id":7,"name":"session","args":{}}'
IFS= read -r reply
printf '%s\n' "$reply" > "$rec/session.tmp"
echo '{"version":"1","type":"mutate","id":8,"name":"delete-everything","args":{}}'
IFS= read -r other
printf '%s\n' "$other" > "$rec/other.json"
mv "$rec/session.tmp" "$rec/session.json"
if [ "$FAKE_UI_LEAF_MODE" = quit ]; then
  while [ ! -f "$rec/quit" ]; do sleep 0.1; done
  echo '{"version":"1","type":"closed","reason":"signal"}'
  exit 0
fi
while IFS= read -r line; do
  printf '%s\n' "$line" >> "$rec/stdin.log"
  case "$line" in
    *'"close"'*) echo '{"version":"1","type":"closed","reason":"caller"}'; exit 0 ;;
  esac
done
exit 0
"#;

/// The view's origin, as the fake's `ready` port makes it.
const VIEW_ORIGIN: &str = "http://127.0.0.1:45678";

struct Sandbox {
    home: TempDir,
    ws: PathBuf,
}

impl Sandbox {
    /// A workspace with AGT-1, and the fake installed as `bin/ui-leaf`.
    fn new() -> Self {
        let home = tempfile::tempdir().unwrap();
        let ws = home.path().join("ws");
        let sb = Sandbox { home, ws };
        std::fs::create_dir_all(sb.rec()).unwrap();
        std::fs::create_dir_all(sb.path("bin")).unwrap();
        sb.install_fake(&sb.path("bin/ui-leaf"));
        assert_ok(&sb.run(&["init", "--prefix", "AGT", "--preset", "saltline"], &[]));
        assert_ok(&sb.run(&["new", "--title", "Original title"], &[]));
        sb
    }

    fn path(&self, name: &str) -> PathBuf {
        self.home.path().join(name)
    }

    /// Where the fake records.
    fn rec(&self) -> PathBuf {
        self.path("rec")
    }

    fn install_fake(&self, at: &Path) {
        let script = FAKE.replace("__REC__", self.rec().to_str().unwrap());
        write_executable(at, &script);
    }

    fn recorded(&self, name: &str) -> Option<String> {
        std::fs::read_to_string(self.rec().join(name)).ok()
    }

    fn was_mounted(&self) -> bool {
        self.rec().join("argv").exists()
    }

    /// A `pm` command: cleared environment, HOME the sandbox, a display,
    /// `PATH` the fake's directory then the system's (never a real
    /// ui-leaf), stdin `/dev/null`.
    fn command(&self, args: &[&str], extra: &[(&str, &str)]) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_pm"));
        cmd.args(args)
            .arg("--workspace")
            .arg(&self.ws)
            .env_clear()
            .env("HOME", self.home.path())
            .env("USER", "tester")
            .env("TMPDIR", self.home.path())
            .env("DISPLAY", ":0")
            .env(
                "PATH",
                format!("{}:/usr/bin:/bin", self.path("bin").display()),
            )
            .stdin(Stdio::null());
        for (k, v) in extra {
            cmd.env(k, v);
        }
        cmd
    }

    fn run(&self, args: &[&str], extra: &[(&str, &str)]) -> Output {
        self.command(args, extra).output().unwrap()
    }

    fn spawn(&self, args: &[&str], extra: &[(&str, &str)]) -> Child {
        self.command(args, extra)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap()
    }

    /// An `EDITOR` that counts its runs in `editor-count` and saves the
    /// file unchanged.
    fn editor(&self) -> PathBuf {
        let path = self.path("editor.sh");
        write_executable(
            &path,
            &format!(
                "#!/bin/sh\necho x >> '{}'\n",
                self.path("editor-count").display()
            ),
        );
        path
    }

    fn editor_runs(&self) -> usize {
        std::fs::read_to_string(self.path("editor-count"))
            .map(|s| s.lines().count())
            .unwrap_or(0)
    }

    /// Blocks until the fake has recorded pm's `session` reply.
    fn wait_for_session(&self, child: &mut Child) -> Value {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if let Some(text) = self.recorded("session.json") {
                return serde_json::from_str(text.trim()).unwrap();
            }
            if let Some(status) = child.try_wait().unwrap() {
                let out = child_output(child);
                panic!("pm exited {status} before the session reply\n{out}");
            }
            assert!(Instant::now() < deadline, "no session reply");
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

fn write_executable(path: &Path, text: &str) {
    std::fs::write(path, text).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
}

fn child_output(child: &mut Child) -> String {
    let mut text = String::new();
    if let Some(mut out) = child.stdout.take() {
        let _ = std::io::Read::read_to_string(&mut out, &mut text);
    }
    if let Some(mut err) = child.stderr.take() {
        text.push_str("\nstderr: ");
        let _ = std::io::Read::read_to_string(&mut err, &mut text);
    }
    text
}

/// Waits for `child` (at most `secs`), returning its exit code and output.
fn wait(mut child: Child, secs: u64) -> (Option<i32>, String, String) {
    let deadline = Instant::now() + Duration::from_secs(secs);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            let out = child.wait_with_output().unwrap();
            return (
                status.code(),
                String::from_utf8_lossy(&out.stdout).into_owned(),
                String::from_utf8_lossy(&out.stderr).into_owned(),
            );
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            let text = child_output(&mut child);
            panic!("pm did not exit within {secs}s\n{text}");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn assert_ok(out: &Output) {
    assert_eq!(
        out.status.code(),
        Some(0),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_global(Some(Duration::from_secs(10)))
        .build()
        .into()
}

/// What pm handed the view: the API's URL and token.
struct Session {
    url: String,
    token: String,
}

impl Session {
    fn from_reply(reply: &Value) -> Session {
        assert_eq!(reply["version"], "1");
        assert_eq!(reply["type"], "result");
        assert_eq!(reply["id"], 7, "the reply answers the session request");
        let value = &reply["value"];
        assert_eq!(value["schema"], 1);
        Session {
            url: value["url"].as_str().unwrap().to_string(),
            token: value["token"].as_str().unwrap().to_string(),
        }
    }

    fn port(&self) -> u16 {
        self.url.rsplit(':').next().unwrap().parse().unwrap()
    }

    /// `GET path` as the view makes it: bearer token and the view's
    /// `Origin`. `(status, Access-Control-Allow-Origin, body)`.
    fn get(&self, path: &str, origin: &str) -> (u16, Option<String>, String) {
        let mut resp = agent()
            .get(format!("{}{path}", self.url))
            .header("Authorization", format!("Bearer {}", self.token))
            .header("Origin", origin)
            .call()
            .unwrap();
        let allow = resp
            .headers()
            .get("access-control-allow-origin")
            .map(|v| v.to_str().unwrap().to_string());
        let status = resp.status().as_u16();
        (status, allow, resp.body_mut().read_to_string().unwrap())
    }

    fn post(&self, path: &str, body: Value) -> u16 {
        agent()
            .post(format!("{}{path}", self.url))
            .header("Authorization", format!("Bearer {}", self.token))
            .header("Origin", VIEW_ORIGIN)
            .header("Content-Type", "application/json")
            .send(body.to_string())
            .unwrap()
            .status()
            .as_u16()
    }

    /// Opens `GET /events` — the view's connection — and returns once the
    /// stream is up. Dropping the socket is the view closing.
    fn open_events(&self) -> TcpStream {
        let mut socket = TcpStream::connect(("127.0.0.1", self.port())).unwrap();
        write!(
            socket,
            "GET /events HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nAuthorization: Bearer {}\r\nOrigin: {VIEW_ORIGIN}\r\n\r\n",
            self.port(),
            self.token
        )
        .unwrap();
        let mut status = String::new();
        BufReader::new(socket.try_clone().unwrap())
            .read_line(&mut status)
            .unwrap();
        assert!(status.starts_with("HTTP/1.1 200"), "{status}");
        socket
    }
}

/// Everything the fake recorded about how pm launched it: the API URL and
/// token reach the view only through the `session` mutation — never in
/// argv, the environment, the mount config (whose `data` ui-leaf serves
/// without a token), or a URL fragment.
fn assert_passed_explicitly(sb: &Sandbox, session: &Session) {
    assert!(
        session.url.starts_with("http://127.0.0.1:"),
        "{}",
        session.url
    );
    assert!(!session.url.contains('#'), "{}", session.url);
    assert!(session.token.starts_with("pma_"), "{}", session.token);
    assert_eq!(sb.recorded("argv").unwrap(), "mount\n");
    let env = sb.recorded("env").unwrap();
    let config = sb.recorded("config.json").unwrap();
    for (what, text) in [("env", &env), ("config", &config)] {
        assert!(!text.contains(&session.token), "token leaked into {what}");
        assert!(!text.contains("#token"), "fragment in {what}");
    }
    let config: Value = serde_json::from_str(config.trim()).unwrap();
    assert!(
        config["data"].get("url").is_none() && config["data"].get("token").is_none(),
        "{config}"
    );
    // The CSP lets the view reach the API and nothing else new.
    let csp = config["csp"].as_str().unwrap();
    assert!(
        csp.contains(&format!("connect-src 'self' {};", session.url)),
        "{csp}"
    );
    // The undeclared mutation was refused.
    let other: Value = serde_json::from_str(sb.recorded("other.json").unwrap().trim()).unwrap();
    assert_eq!(other["type"], "error");
    assert_eq!(other["id"], 8);
}

/// The API answers the view's origin (allowed once ui-leaf reported its
/// port) and no other.
fn assert_api_reachable(session: &Session) {
    let (status, allow, body) = session.get("/workspace", VIEW_ORIGIN);
    assert_eq!(status, 200, "{body}");
    assert_eq!(allow.as_deref(), Some(VIEW_ORIGIN));
    let ws: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(ws["prefix"], "AGT");
    let (status, _, _) = session.get("/workspace", "http://127.0.0.1:45679");
    assert_eq!(status, 403, "only the view's origin is allowed");
}

// ---------------------------------------------------------------- tests

#[test]
fn pm_edit_opens_the_ticket_view_and_returns_when_it_closes() {
    let sb = Sandbox::new();
    let editor = sb.editor();
    let mut child = sb.spawn(
        &["edit", "AGT-1", "--view=ui-leaf", "--json"],
        &[("EDITOR", editor.to_str().unwrap())],
    );
    let session = Session::from_reply(&sb.wait_for_session(&mut child));
    assert_passed_explicitly(&sb, &session);
    assert_api_reachable(&session);

    let config: Value = serde_json::from_str(sb.recorded("config.json").unwrap().trim()).unwrap();
    assert_eq!(config["version"], "1");
    assert_eq!(config["view"], "ticket");
    assert_eq!(
        config["data"],
        json!({"schema": 1, "view": "ticket", "ticket": "AGT-1"})
    );
    assert_eq!(config["mutations"], json!(["session"]));
    let root = PathBuf::from(config["viewsRoot"].as_str().unwrap());
    assert!(root.is_absolute(), "{}", root.display());
    assert!(
        root.starts_with(sb.path(".cache/pm/views")),
        "{}",
        root.display()
    );
    for file in ["ticket.tsx", "board.tsx", "lib/pm.ts"] {
        assert!(root.join(file).is_file(), "{file} not unpacked");
    }

    // The view writes through the API while it is open…
    assert_eq!(
        session.post("/tickets/AGT-1/fields", json!({"title": "From the view"})),
        200
    );
    // …holds its event stream, then closes: the stream drops, the grace
    // runs out, pm closes ui-leaf and returns.
    let events = session.open_events();
    std::thread::sleep(Duration::from_millis(300));
    drop(events);
    let (code, stdout, stderr) = wait(child, 30);
    assert_eq!(code, Some(0), "{stderr}");
    assert!(
        sb.recorded("stdin.log")
            .unwrap()
            .contains(r#""type":"close""#),
        "pm asked ui-leaf to close"
    );
    let ticket: Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(ticket["id"], "AGT-1");
    assert_eq!(ticket["title"], "From the view");
    assert_eq!(sb.editor_runs(), 0, "$EDITOR never ran");
}

#[test]
fn pm_app_opens_the_board_and_ends_with_ui_leaf() {
    let sb = Sandbox::new();
    let mut child = sb.spawn(&["app", "--idle", "0"], &[("FAKE_UI_LEAF_MODE", "quit")]);
    let session = Session::from_reply(&sb.wait_for_session(&mut child));
    assert_passed_explicitly(&sb, &session);
    assert_api_reachable(&session);
    let config: Value = serde_json::from_str(sb.recorded("config.json").unwrap().trim()).unwrap();
    assert_eq!(config["view"], "board");
    assert_eq!(config["data"], json!({"schema": 1, "view": "board"}));

    // ui-leaf exiting (the window closed, Ctrl-C) ends pm app, even with
    // --idle 0.
    std::fs::write(sb.rec().join("quit"), "").unwrap();
    let (code, stdout, stderr) = wait(child, 30);
    assert_eq!(code, Some(0), "{stderr}");
    assert!(
        !stdout.contains(&session.token) && !stderr.contains(&session.token),
        "the launched board never prints the token"
    );
}

#[test]
fn ui_leaf_from_config_wins_over_path() {
    let sb = Sandbox::new();
    // PATH's fake is broken; config names a working one elsewhere.
    write_executable(&sb.path("bin/ui-leaf"), "#!/bin/sh\nexit 9\n");
    let custom = sb.path("tools/my-ui-leaf");
    std::fs::create_dir_all(custom.parent().unwrap()).unwrap();
    sb.install_fake(&custom);
    let config = sb.path(".config/pm/config.toml");
    let base = std::fs::read_to_string(&config).unwrap_or_default();
    std::fs::write(
        &config,
        format!("{base}\n[ui_leaf]\npath = \"{}\"\n", custom.display()),
    )
    .unwrap();
    let mut child = sb.spawn(&["app", "--idle", "0"], &[("FAKE_UI_LEAF_MODE", "quit")]);
    let session = Session::from_reply(&sb.wait_for_session(&mut child));
    assert_api_reachable(&session);
    std::fs::write(sb.rec().join("quit"), "").unwrap();
    let (code, _, stderr) = wait(child, 30);
    assert_eq!(code, Some(0), "{stderr}");
}

#[test]
fn a_non_interactive_default_is_the_editor_even_with_ui_leaf_present() {
    // stdin is /dev/null and stdout a pipe: a scripted `pm edit` (an agent)
    // never opens a window unless it asked for one.
    let sb = Sandbox::new();
    let editor = sb.editor();
    let out = sb.run(&["edit", "AGT-1"], &[("EDITOR", editor.to_str().unwrap())]);
    assert_ok(&out);
    assert!(
        !String::from_utf8_lossy(&out.stderr).contains("ui-leaf"),
        "silently"
    );
    assert!(!sb.was_mounted(), "the fake on PATH was never run");
    assert_eq!(sb.editor_runs(), 1);

    // edit.view = ui-leaf in config is a request too: it launches.
    let config = sb.path(".config/pm/config.toml");
    let base = std::fs::read_to_string(&config).unwrap_or_default();
    std::fs::write(&config, format!("{base}\n[edit]\nview = \"ui-leaf\"\n")).unwrap();
    let mut child = sb.spawn(
        &["edit", "AGT-1"],
        &[
            ("EDITOR", editor.to_str().unwrap()),
            ("FAKE_UI_LEAF_MODE", "quit"),
        ],
    );
    sb.wait_for_session(&mut child);
    std::fs::write(sb.rec().join("quit"), "").unwrap();
    let (code, _, stderr) = wait(child, 30);
    assert_eq!(code, Some(0), "{stderr}");
    assert_eq!(sb.editor_runs(), 1, "$EDITOR did not run again");
}

#[test]
fn without_ui_leaf_an_explicit_request_is_the_editor_flow_with_one_note() {
    let sb = Sandbox::new();
    std::fs::remove_file(sb.path("bin/ui-leaf")).unwrap();
    let editor = sb.editor();
    let out = sb.run(
        &["edit", "AGT-1", "--view=ui-leaf"],
        &[("EDITOR", editor.to_str().unwrap())],
    );
    assert_ok(&out);
    let stderr = String::from_utf8_lossy(&out.stderr);
    let notes: Vec<_> = stderr.lines().filter(|l| l.contains("ui-leaf")).collect();
    assert_eq!(notes.len(), 1, "{stderr}");
    assert!(notes[0].contains("ui-leaf not found"), "{stderr}");
    assert_eq!(sb.editor_runs(), 1);
}

#[test]
fn edit_view_editor_never_looks_for_ui_leaf() {
    let sb = Sandbox::new();
    let editor = sb.editor();
    let config = sb.path(".config/pm/config.toml");
    let base = std::fs::read_to_string(&config).unwrap_or_default();
    std::fs::write(&config, format!("{base}\n[edit]\nview = \"editor\"\n")).unwrap();
    let out = sb.run(&["edit", "AGT-1"], &[("EDITOR", editor.to_str().unwrap())]);
    assert_ok(&out);
    assert!(!String::from_utf8_lossy(&out.stderr).contains("ui-leaf"));
    assert!(!sb.was_mounted());
    assert_eq!(sb.editor_runs(), 1);
    // --view editor likewise.
    let out = sb.run(
        &["edit", "AGT-1", "--view=editor"],
        &[("EDITOR", editor.to_str().unwrap())],
    );
    assert_ok(&out);
    assert!(!sb.was_mounted());
}

#[test]
fn no_display_uses_the_editor_quietly_unless_ui_leaf_was_asked_for() {
    let sb = Sandbox::new();
    let editor = sb.editor();
    let ssh = [
        ("EDITOR", editor.to_str().unwrap()),
        ("SSH_CONNECTION", "10.0.0.2 50000 10.0.0.1 22"),
    ];
    let out = sb.run(&["edit", "AGT-1"], &ssh);
    assert_ok(&out);
    assert!(!String::from_utf8_lossy(&out.stderr).contains("ui-leaf"));
    let out = sb.run(&["edit", "AGT-1", "--view=ui-leaf"], &ssh);
    assert_ok(&out);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("no display") && stderr.contains("SSH"),
        "{stderr}"
    );
    // ui-leaf's own switch counts as no display too.
    let out = sb.run(
        &["edit", "AGT-1", "--view=ui-leaf"],
        &[
            ("EDITOR", editor.to_str().unwrap()),
            ("UI_LEAF_NO_OPEN", "1"),
        ],
    );
    assert_ok(&out);
    assert!(!sb.was_mounted(), "the runtime never ran without a display");
    assert_eq!(sb.editor_runs(), 3);
}

#[test]
fn an_unpinned_ui_leaf_is_not_launched() {
    let sb = Sandbox::new();
    let editor = sb.editor();
    for version in ["2.0.0", "1.5.1"] {
        let out = sb.run(
            &["edit", "AGT-1", "--view=ui-leaf"],
            &[
                ("EDITOR", editor.to_str().unwrap()),
                ("FAKE_UI_LEAF_VERSION", version),
            ],
        );
        assert_ok(&out);
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            stderr.contains(version) && stderr.contains("outside the supported range"),
            "{stderr}"
        );
    }
    assert!(!sb.was_mounted());
    assert_eq!(sb.editor_runs(), 2);
}

#[test]
fn a_mount_that_fails_before_ready_falls_back_to_the_editor() {
    let sb = Sandbox::new();
    let editor = sb.editor();
    let out = sb.run(
        &["edit", "AGT-1", "--view=ui-leaf"],
        &[
            ("EDITOR", editor.to_str().unwrap()),
            ("FAKE_UI_LEAF_MODE", "fail"),
        ],
    );
    assert_ok(&out);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("could not open AGT-1") && stderr.contains("failed to compile"),
        "{stderr}"
    );
    assert_eq!(sb.editor_runs(), 1);

    // pm app has no editor to fall back to: exit 1 with the reason.
    let out = sb.run(&["app"], &[("FAKE_UI_LEAF_MODE", "fail")]);
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("failed to compile"));
}

#[test]
fn a_missing_ticket_is_not_found_before_any_window() {
    let sb = Sandbox::new();
    let out = sb.run(&["edit", "AGT-99", "--view=ui-leaf"], &[]);
    assert_eq!(out.status.code(), Some(3));
    assert!(!sb.was_mounted());
}

#[test]
fn pm_app_without_ui_leaf_serves_headless() {
    let sb = Sandbox::new();
    std::fs::remove_file(sb.path("bin/ui-leaf")).unwrap();
    let out = sb.run(&["app", "--idle", "1"], &[]);
    assert_ok(&out);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("url:") && stdout.contains("token: pma_"),
        "{stdout}"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("ui-leaf not found") && stderr.contains("serving the API only"),
        "{stderr}"
    );
}

#[test]
fn pm_app_json_is_headless_even_with_ui_leaf() {
    let sb = Sandbox::new();
    let out = sb.run(&["app", "--json", "--idle", "1"], &[]);
    assert_ok(&out);
    let line: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(line["idle_secs"], 1);
    assert!(!sb.was_mounted());
}
