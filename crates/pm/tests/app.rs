//! `pm app` (AGT-1401, `docs/app-api.md`) driven through the built binary
//! against a temp workspace (HOME is the sandbox; never the real config,
//! keychain or `~/.local/share/pm`). Each test launches its own server,
//! reads the launch line, and talks HTTP to it with `ureq`; the server is
//! killed when the test's `App` drops.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Shutdown, TcpStream};
use std::path::PathBuf;
use std::process::{Child, Command, Output, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use pm_core::{Body, BodyUpdate, Project, ProjectStatus};
use pm_store::Store;
use serde_json::{Value, json};
use tempfile::TempDir;

// ------------------------------------------------------------- sandbox

struct Sandbox {
    home: TempDir,
    ws: PathBuf,
}

impl Sandbox {
    /// A saltline workspace with projects `pm` and `other`, AGT-1 (project
    /// pm, labels a and b, a two-line description) and AGT-2 (blocked by
    /// AGT-1).
    fn new() -> Self {
        let home = tempfile::tempdir().unwrap();
        let ws = home.path().join("ws");
        let sb = Sandbox { home, ws };
        assert_ok(&sb.run(&["init", "--prefix", "AGT", "--preset", "saltline"], &[]));
        let mut store = sb.store();
        for id in ["pm", "other"] {
            store
                .put_project(
                    &Project {
                        id: id.into(),
                        title: id.into(),
                        status: ProjectStatus::InProgress,
                        parent: None,
                        repos: Default::default(),
                        doc: String::new(),
                        documents: Default::default(),
                    },
                    &pm_core::ActorId::new("matt"),
                )
                .unwrap();
        }
        assert_ok(&sb.run(
            &[
                "new",
                "--title",
                "Original title",
                "--project",
                "pm",
                "--label",
                "a,b",
                "--description",
                "line one\nline two",
            ],
            &[],
        ));
        assert_ok(&sb.run(
            &[
                "new",
                "--title",
                "Second",
                "--project",
                "pm",
                "--blocked-by",
                "AGT-1",
            ],
            &[],
        ));
        sb
    }

    fn ws_str(&self) -> &str {
        self.ws.to_str().unwrap()
    }

    fn store(&self) -> Store {
        Store::open(self.ws.join("pm.sqlite")).unwrap()
    }

    fn path(&self, name: &str) -> PathBuf {
        self.home.path().join(name)
    }

    /// A `pm` command with a cleared environment: HOME is the sandbox,
    /// USER is `tester`, stdin `/dev/null`.
    fn command(&self, args: &[&str], extra: &[(&str, &str)]) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_pm"));
        cmd.args(args)
            .arg("--workspace")
            .arg(&self.ws)
            .env_clear()
            .env("HOME", self.home.path())
            .env("USER", "tester")
            .env("TMPDIR", self.home.path())
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .stdin(Stdio::null());
        for (k, v) in extra {
            cmd.env(k, v);
        }
        cmd
    }

    fn run(&self, args: &[&str], extra: &[(&str, &str)]) -> Output {
        self.command(args, extra).output().unwrap()
    }

    /// `pm <args> --json`, parsed.
    fn json(&self, args: &[&str]) -> Value {
        let mut args = args.to_vec();
        args.push("--json");
        let out = self.run(&args, &[]);
        assert_ok(&out);
        serde_json::from_slice(&out.stdout).unwrap()
    }

    /// Every op kind on `id`, in log order.
    fn op_kinds(&self, id: &str) -> Vec<String> {
        self.json(&["log", id])
            .as_array()
            .unwrap()
            .iter()
            .map(|op| op["kind"].as_str().unwrap().to_string())
            .collect()
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

// ----------------------------------------------------------------- app

/// A running `pm app`, killed on drop.
struct App {
    child: Child,
    url: String,
    token: String,
    launch: Value,
}

impl App {
    fn start(sb: &Sandbox, extra_args: &[&str]) -> App {
        let mut args = vec!["app", "--json"];
        args.extend_from_slice(extra_args);
        let mut child = sb
            .command(&args, &[("PM_ACTOR", "tester:app")])
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let mut line = String::new();
        BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut line)
            .unwrap();
        let launch: Value =
            serde_json::from_str(&line).unwrap_or_else(|e| panic!("launch line {line:?}: {e}"));
        App {
            child,
            url: launch["url"].as_str().unwrap().to_string(),
            token: launch["token"].as_str().unwrap().to_string(),
            launch,
        }
    }

    fn port(&self) -> u16 {
        self.url.rsplit(':').next().unwrap().parse().unwrap()
    }

    fn agent() -> ureq::Agent {
        ureq::Agent::config_builder()
            .http_status_as_error(false)
            .timeout_global(Some(Duration::from_secs(10)))
            .build()
            .into()
    }

    fn read(resp: &mut ureq::http::Response<ureq::Body>) -> (u16, String) {
        let status = resp.status().as_u16();
        let body = resp.body_mut().read_to_string().unwrap();
        (status, body)
    }

    /// `GET path` with the bearer token: `(status, body)`.
    fn get_raw(&self, path: &str) -> (u16, String) {
        let mut resp = Self::agent()
            .get(format!("{}{path}", self.url))
            .header("Authorization", format!("Bearer {}", self.token))
            .call()
            .unwrap();
        Self::read(&mut resp)
    }

    fn get(&self, path: &str) -> (u16, Value) {
        let (status, body) = self.get_raw(path);
        (status, serde_json::from_str(&body).unwrap())
    }

    /// `POST path` with a JSON body and the bearer token.
    fn post(&self, path: &str, body: Value) -> (u16, Value) {
        let mut resp = Self::agent()
            .post(format!("{}{path}", self.url))
            .header("Authorization", format!("Bearer {}", self.token))
            .header("Content-Type", "application/json")
            .send(body.to_string())
            .unwrap();
        let (status, text) = Self::read(&mut resp);
        (status, serde_json::from_str(&text).unwrap())
    }

    /// A request written by hand on a raw socket, for headers `ureq`
    /// manages itself (`Host`): the status line's code and the whole
    /// response text.
    fn raw(&self, method: &str, path: &str, headers: &[String]) -> (u16, String) {
        let mut stream = TcpStream::connect(("127.0.0.1", self.port())).unwrap();
        let mut request = format!("{method} {path} HTTP/1.1\r\n");
        for h in headers {
            request.push_str(h);
            request.push_str("\r\n");
        }
        request.push_str("Connection: close\r\n\r\n");
        stream.write_all(request.as_bytes()).unwrap();
        let mut text = String::new();
        stream.read_to_string(&mut text).unwrap();
        let code = text
            .split_whitespace()
            .nth(1)
            .and_then(|c| c.parse().ok())
            .unwrap_or(0);
        (code, text)
    }

    fn bearer(&self) -> String {
        format!("Authorization: Bearer {}", self.token)
    }

    /// Opens `/events` on a raw socket — a view's connection — and
    /// streams every `data:` payload (parsed) from a reader thread.
    /// Dropping the [`EventStream`] closes the socket, which is how a
    /// view disconnects.
    fn events(&self) -> EventStream {
        let mut socket = TcpStream::connect(("127.0.0.1", self.port())).unwrap();
        write!(
            socket,
            "GET /events HTTP/1.1\r\nHost: 127.0.0.1:{}\r\n{}\r\nAccept: text/event-stream\r\n\r\n",
            self.port(),
            self.bearer()
        )
        .unwrap();
        // The status line and headers arrive at once; the body streams.
        // One BufReader from before the headers on, so a body byte it
        // buffered early is not lost when it moves to the thread.
        let mut reader = BufReader::new(socket.try_clone().unwrap());
        let mut status = String::new();
        reader.read_line(&mut status).unwrap();
        assert!(status.starts_with("HTTP/1.1 200"), "{status}");
        let mut headers = String::new();
        loop {
            let mut line = String::new();
            assert_ne!(reader.read_line(&mut line).unwrap(), 0, "headers cut short");
            if line == "\r\n" {
                break;
            }
            headers.push_str(&line);
        }
        assert!(
            headers
                .to_ascii_lowercase()
                .contains("content-type: text/event-stream"),
            "{headers}"
        );
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            // Chunk-size lines (the body is chunked) and `: keep-alive`
            // comments are simply not `data:` lines.
            for line in reader.lines() {
                let Ok(line) = line else { break };
                if let Some(data) = line.strip_prefix("data: ")
                    && let Ok(value) = serde_json::from_str::<Value>(data)
                    && tx.send(value).is_err()
                {
                    break;
                }
            }
        });
        EventStream { rx, socket }
    }

    /// Waits up to `within` for the server to exit; its exit code.
    fn wait_exit(&mut self, within: Duration) -> Option<i32> {
        let start = Instant::now();
        while start.elapsed() < within {
            if let Some(status) = self.child.try_wait().unwrap() {
                return status.code();
            }
            thread::sleep(Duration::from_millis(50));
        }
        None
    }
}

impl Drop for App {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// One open `/events` connection; the socket closes on drop.
struct EventStream {
    rx: mpsc::Receiver<Value>,
    socket: TcpStream,
}

impl EventStream {
    fn next(&self, within: Duration) -> Value {
        self.rx
            .recv_timeout(within)
            .unwrap_or_else(|e| panic!("no event within {within:?}: {e}"))
    }

    fn try_next(&self) -> Option<Value> {
        self.rx.try_recv().ok()
    }
}

impl Drop for EventStream {
    fn drop(&mut self) {
        let _ = self.socket.shutdown(Shutdown::Both);
    }
}

// --------------------------------------------------------------- tests

#[test]
fn the_launch_line_names_a_loopback_url_and_a_token() {
    let sb = Sandbox::new();
    let app = App::start(
        &sb,
        &["--idle", "0", "--allow-origin", "HTTP://Localhost:5173/"],
    );
    assert_eq!(app.launch["schema"], 1);
    assert!(app.url.starts_with("http://127.0.0.1:"), "{}", app.url);
    assert!(
        app.token.starts_with("pma_") && app.token.len() == 68,
        "{}",
        app.token
    );
    assert_eq!(app.launch["actor"], "tester:app");
    assert_eq!(app.launch["idle_secs"], 0);
    assert_eq!(
        app.launch["allowed_origins"],
        json!(["http://localhost:5173"])
    );
    assert!(app.launch["pid"].as_u64().unwrap() > 0);
    assert_eq!(app.launch["workspace"].as_str().unwrap(), sb.ws_str());

    let (status, ws) = app.get("/workspace");
    assert_eq!(status, 200);
    assert_eq!(ws["prefix"], "AGT");
    assert_eq!(ws["actor"], "tester:app");
    assert_eq!(ws["states"][0]["name"], "triage");
    assert_eq!(ws["gate_labels"], json!(["manual"]));
}

#[test]
fn a_bad_allow_origin_is_a_usage_error() {
    let sb = Sandbox::new();
    let out = sb.run(&["app", "--allow-origin", "*"], &[]);
    assert_eq!(out.status.code(), Some(2));
    let out = sb.run(&["app", "--allow-origin", "localhost:5173"], &[]);
    assert_eq!(out.status.code(), Some(2));
}

#[test]
fn every_request_needs_the_token() {
    let sb = Sandbox::new();
    let app = App::start(&sb, &["--idle", "0"]);
    let host = format!("Host: 127.0.0.1:{}", app.port());

    let (code, text) = app.raw("GET", "/tickets/AGT-1", std::slice::from_ref(&host));
    assert_eq!(code, 401, "{text}");
    assert!(text.contains("www-authenticate: Bearer"), "{text}");
    assert!(text.contains(r#""error""#), "{text}");

    let wrong = format!("Authorization: Bearer {}x", app.token);
    let (code, _) = app.raw("GET", "/tickets/AGT-1", &[host.clone(), wrong]);
    assert_eq!(code, 401);
    let (code, _) = app.raw(
        "GET",
        "/tickets/AGT-1",
        &[host.clone(), format!("Authorization: Basic {}", app.token)],
    );
    assert_eq!(code, 401);

    // A write without the token writes nothing.
    let before = sb.op_kinds("AGT-1");
    let (code, _) = app.raw(
        "POST",
        "/tickets/AGT-1/fields",
        &[
            host.clone(),
            "Content-Type: application/json".into(),
            "Content-Length: 17".into(),
        ],
    );
    assert_eq!(code, 401);
    assert_eq!(sb.op_kinds("AGT-1"), before);

    let (code, _) = app.raw("GET", "/tickets/AGT-1", &[host, app.bearer()]);
    assert_eq!(code, 200);
    // An unknown route, even with the token, is a JSON 404.
    let (status, body) = app.get("/nope");
    assert_eq!(status, 404);
    assert_eq!(body["schema"], 1);
    assert!(body["error"].is_string());
}

#[test]
fn a_foreign_host_or_origin_is_refused() {
    let sb = Sandbox::new();
    let app = App::start(
        &sb,
        &["--idle", "0", "--allow-origin", "http://127.0.0.1:5173"],
    );
    let port = app.port();

    // DNS rebinding: the request reaches the port under another name.
    for host in [
        "Host: evil.example".to_string(),
        format!("Host: evil.example:{port}"),
        format!("Host: 127.0.0.1:{}", port.wrapping_add(1)),
        "Host: 127.0.0.1".to_string(),
    ] {
        let (code, text) = app.raw("GET", "/workspace", &[host.clone(), app.bearer()]);
        assert_eq!(code, 403, "{host}: {text}");
    }
    for host in [
        format!("Host: 127.0.0.1:{port}"),
        format!("Host: LOCALHOST:{port}"),
    ] {
        let (code, text) = app.raw("GET", "/workspace", &[host.clone(), app.bearer()]);
        assert_eq!(code, 200, "{host}: {text}");
    }

    let host = format!("Host: 127.0.0.1:{port}");
    // An origin that was not allow-listed, token or not.
    for origin in ["http://127.0.0.1:5174", "http://evil.example", "null"] {
        let (code, text) = app.raw(
            "GET",
            "/workspace",
            &[host.clone(), app.bearer(), format!("Origin: {origin}")],
        );
        assert_eq!(code, 403, "{origin}: {text}");
        assert!(
            !text
                .to_ascii_lowercase()
                .contains("access-control-allow-origin"),
            "{origin}: {text}"
        );
        let (code, _) = app.raw(
            "OPTIONS",
            "/workspace",
            &[host.clone(), format!("Origin: {origin}")],
        );
        assert_eq!(code, 403, "preflight for {origin}");
    }
    // The allowed origin: CORS headers, that origin only, no wildcard.
    let (code, text) = app.raw(
        "GET",
        "/workspace",
        &[
            host.clone(),
            app.bearer(),
            "Origin: http://127.0.0.1:5173".into(),
        ],
    );
    assert_eq!(code, 200, "{text}");
    let lower = text.to_ascii_lowercase();
    assert!(
        lower.contains("access-control-allow-origin: http://127.0.0.1:5173"),
        "{text}"
    );
    assert!(!lower.contains("access-control-allow-origin: *"), "{text}");
    assert!(
        !lower.contains("access-control-allow-credentials"),
        "{text}"
    );
    // Its preflight needs no token.
    let (code, text) = app.raw(
        "OPTIONS",
        "/tickets/AGT-1/fields",
        &[
            host.clone(),
            "Origin: http://127.0.0.1:5173".into(),
            "Access-Control-Request-Method: POST".into(),
            "Access-Control-Request-Headers: authorization, content-type".into(),
        ],
    );
    assert_eq!(code, 204, "{text}");
    let lower = text.to_ascii_lowercase();
    assert!(
        lower.contains("access-control-allow-headers: authorization, content-type"),
        "{text}"
    );
    assert!(
        lower.contains("access-control-allow-methods: get, post, options"),
        "{text}"
    );
    // But the real request still does.
    let (code, _) = app.raw(
        "GET",
        "/workspace",
        &[host, "Origin: http://127.0.0.1:5173".into()],
    );
    assert_eq!(code, 401);
}

#[test]
fn reads_are_the_clis_json_shapes() {
    let sb = Sandbox::new();
    assert_ok(&sb.run(&["hold", "AGT-2", "waiting"], &[]));
    // A comment: `pm show --json` carries `comments` (AGT-1430), `pm list`
    // does not, and the API follows each.
    assert_ok(&sb.run(&["comment", "AGT-1", "a note"], &[]));
    let app = App::start(&sb, &["--idle", "0"]);

    for (path, cli) in [
        ("/tickets/AGT-1", vec!["show", "AGT-1"]),
        ("/tickets/AGT-2", vec!["show", "AGT-2"]),
        ("/tickets", vec!["list"]),
        (
            "/tickets?project=pm&state=triage,in-progress",
            vec!["list", "--project", "pm", "--state", "triage,in-progress"],
        ),
        ("/tickets?label=a", vec!["list", "--label", "a"]),
        ("/tickets?held=true", vec!["list", "--held"]),
        ("/tickets?search=second", vec!["list", "--search", "second"]),
        ("/projects", vec!["project", "list"]),
        (
            "/projects?status=complete",
            vec!["project", "list", "--status", "complete"],
        ),
        ("/projects/pm", vec!["project", "show", "pm"]),
        ("/ready", vec!["ready"]),
        (
            "/ready?project=pm&limit=1",
            vec!["ready", "--project", "pm", "--limit", "1"],
        ),
        (
            "/ready?ids=AGT-1,AGT-2&exclude_label=a",
            vec!["ready", "--ids", "AGT-1,AGT-2", "--exclude-label", "a"],
        ),
    ] {
        let (status, body) = app.get(path);
        assert_eq!(status, 200, "{path}: {body}");
        assert_eq!(
            body,
            sb.json(&cli),
            "{path} differs from `pm {}`",
            cli.join(" ")
        );
    }
    let (_, one) = app.get("/tickets/AGT-1");
    assert_eq!(one["comments"][0]["body"], "a note", "{one}");
    let (_, listed) = app.get("/tickets");
    assert!(listed[0].get("comments").is_none(), "{listed}");
    // The ULID works as the id, as it does on the command line.
    let ulid = sb.json(&["show", "AGT-1"])["ulid"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(
        app.get(&format!("/tickets/{ulid}")).1,
        sb.json(&["show", "AGT-1"])
    );

    // The CLI's exit codes, as statuses.
    assert_eq!(app.get("/tickets/AGT-99").0, 404);
    assert_eq!(app.get("/tickets/AGT-?").0, 400);
    assert_eq!(app.get("/tickets/nonsense").0, 400);
    assert_eq!(app.get("/projects/nope").0, 404);
    assert_eq!(app.get("/ready?project=nope").0, 404);
    assert_eq!(app.get("/ready?limit=0").0, 400);
    assert_eq!(app.get("/projects?status=bogus").0, 400);

    // The body endpoint: the description and a Loro snapshot that
    // rebuilds it.
    let (status, body) = app.get("/tickets/AGT-1/body");
    assert_eq!(status, 200);
    assert_eq!(body["id"], "AGT-1");
    assert_eq!(body["ulid"], ulid);
    assert_eq!(body["text"], "line one\nline two");
    let snapshot = pm_core::bytes::decode(body["snapshot"].as_str().unwrap()).unwrap();
    let mut doc = Body::new();
    doc.apply(&BodyUpdate::from_bytes(snapshot)).unwrap();
    assert_eq!(doc.text(), "line one\nline two");
}

#[test]
fn every_post_commits_an_op_the_cli_can_see() {
    let sb = Sandbox::new();
    let app = App::start(&sb, &["--idle", "0"]);
    let before = sb.op_kinds("AGT-1").len();

    let (status, t) = app.post(
        "/tickets/AGT-1/fields",
        json!({"title": "Renamed", "priority": "high", "project": "other", "repo": null, "team": "eng"}),
    );
    assert_eq!(status, 200, "{t}");
    assert_eq!(t["title"], "Renamed");
    assert_eq!(t["priority"], "high");
    assert_eq!(t["project"], "other");
    assert_eq!(t["ext"]["team"], "eng");
    assert_eq!(
        t,
        sb.json(&["show", "AGT-1"]),
        "the answer is `pm show --json`"
    );

    let (status, t) = app.post(
        "/tickets/AGT-1/labels",
        json!({"add": ["c"], "remove": ["b"]}),
    );
    assert_eq!(status, 200, "{t}");
    assert_eq!(t["labels"], json!(["a", "c"]));

    let (status, t) = app.post("/tickets/AGT-1/state", json!({"state": "in-progress"}));
    assert_eq!(status, 200, "{t}");
    assert_eq!(t["state"], "in-progress");

    let (status, t) = app.post(
        "/tickets/AGT-1/body",
        json!({"text": "line one\nline two\nline three"}),
    );
    assert_eq!(status, 200, "{t}");
    assert_eq!(t["description"], "line one\nline two\nline three");

    let kinds: Vec<String> = sb.op_kinds("AGT-1")[before..].to_vec();
    assert_eq!(
        kinds,
        [
            "field.set",
            "field.set",
            "field.set",
            "field.set",
            "field.set",
            "label.add",
            "label.remove",
            "state.transition",
            "body.edit",
        ]
    );
    let log = sb.json(&["log", "AGT-1"]);
    let actors: Vec<&str> = log.as_array().unwrap()[before..]
        .iter()
        .map(|op| op["actor"].as_str().unwrap())
        .collect();
    assert!(actors.iter().all(|a| *a == "tester:app"), "{actors:?}");
    // The ops are ordinary: they sit in the outbox for `pm sync`, and the
    // tables still replay from the log.
    assert_eq!(
        sb.store().outbox_len().unwrap() as usize,
        sb.op_kinds("AGT-1").len()
            + sb.op_kinds("AGT-2").len()
            + sb.json(&["log"]).as_array().unwrap().len()
    );
    assert_ok(&sb.run(&["doctor"], &[]));

    // Moving back to an unstarted state clears the assignee like `pm move`.
    assert_ok(&sb.run(&["set", "AGT-1", "assignee=someone"], &[]));
    let (status, t) = app.post("/tickets/AGT-1/state", json!({"state": "triage"}));
    assert_eq!(status, 200, "{t}");
    assert_eq!(t["state"], "triage");
    assert!(t["assignee"].is_null(), "{t}");

    // Bad writes write nothing.
    let before = sb.op_kinds("AGT-1");
    assert_eq!(app.post("/tickets/AGT-1/fields", json!({})).0, 400);
    assert_eq!(
        app.post("/tickets/AGT-1/fields", json!({"title": ""})).0,
        400
    );
    assert_eq!(
        app.post("/tickets/AGT-1/fields", json!({"priority": "urgent"}))
            .0,
        400
    );
    assert_eq!(
        app.post("/tickets/AGT-1/fields", json!({"project": "nope"}))
            .0,
        404
    );
    assert_eq!(
        app.post("/tickets/AGT-1/fields", json!({"title": ["x"]})).0,
        400
    );
    assert_eq!(app.post("/tickets/AGT-1/fields", json!([])).0, 400);
    assert_eq!(app.post("/tickets/AGT-1/labels", json!({})).0, 400);
    assert_eq!(
        app.post("/tickets/AGT-1/labels", json!({"add": [""]})).0,
        400
    );
    assert_eq!(
        app.post("/tickets/AGT-1/state", json!({"state": "nope"})).0,
        404
    );
    assert_eq!(app.post("/tickets/AGT-1/state", json!({})).0, 400);
    assert_eq!(app.post("/tickets/AGT-1/body", json!({})).0, 400);
    assert_eq!(
        app.post("/tickets/AGT-1/body", json!({"update": "not base64!"}))
            .0,
        400
    );
    assert_eq!(
        app.post("/tickets/AGT-1/body", json!({"update": "aGVsbG8="}))
            .0,
        400
    );
    assert_eq!(
        app.post("/tickets/AGT-99/fields", json!({"title": "x"})).0,
        404
    );
    // A no-op body edit is not an op.
    let text = sb.json(&["show", "AGT-1"])["description"].clone();
    assert_eq!(
        app.post("/tickets/AGT-1/body", json!({"text": text})).0,
        200
    );
    assert_eq!(sb.op_kinds("AGT-1"), before);
    // Malformed JSON is a JSON 400 too.
    let mut resp = App::agent()
        .post(format!("{}/tickets/AGT-1/fields", app.url))
        .header("Authorization", format!("Bearer {}", app.token))
        .send("{not json")
        .unwrap();
    let (status, body) = App::read(&mut resp);
    assert_eq!(status, 400);
    assert!(body.contains(r#""error""#), "{body}");
    assert_eq!(sb.op_kinds("AGT-1"), before);
}

#[test]
fn a_loro_update_from_an_editor_merges_like_pm_edit() {
    let sb = Sandbox::new();
    let app = App::start(&sb, &["--idle", "0"]);

    // The editor binds its own document (its own random peer) to the
    // body's snapshot, as a loro-crdt view would.
    let (_, body) = app.get("/tickets/AGT-1/body");
    let snapshot = pm_core::bytes::decode(body["snapshot"].as_str().unwrap()).unwrap();
    let mut doc = Body::new();
    doc.apply(&BodyUpdate::from_bytes(snapshot)).unwrap();

    // Meanwhile `pm edit` in a terminal rewrites line two.
    let editor = sb.path("editor.sh");
    std::fs::write(
        &editor,
        "#!/bin/sh\nperl -pi -e 's/line two/LINE TWO/' \"$1\"\n",
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&editor, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    assert_ok(&sb.run(
        &["edit", "AGT-1", "--view=editor"],
        &[("EDITOR", editor.to_str().unwrap())],
    ));

    // The editor's edit of line one, as the update bytes its document
    // produced, lands on top: neither edit is lost.
    let update = doc.diff_from_text("LINE ONE\nline two").unwrap();
    let (status, t) = app.post(
        "/tickets/AGT-1/body",
        json!({"update": pm_core::bytes::encode(update.as_bytes())}),
    );
    assert_eq!(status, 200, "{t}");
    assert_eq!(t["description"], "LINE ONE\nLINE TWO");
    assert_eq!(
        sb.json(&["show", "AGT-1"])["description"],
        "LINE ONE\nLINE TWO"
    );
    assert_eq!(
        sb.op_kinds("AGT-1").last().map(String::as_str),
        Some("body.edit")
    );
    assert_ok(&sb.run(&["doctor"], &[]));

    // The same update again is idempotent: applied, nothing changes.
    let (status, t) = app.post(
        "/tickets/AGT-1/body",
        json!({"update": pm_core::bytes::encode(update.as_bytes())}),
    );
    assert_eq!(status, 200, "{t}");
    assert_eq!(t["description"], "LINE ONE\nLINE TWO");

    // An update whose history this replica lacks is refused, not queued.
    let mut stranger = Body::new();
    stranger.diff_from_text("unrelated").unwrap();
    let second = stranger.diff_from_text("unrelated, more").unwrap();
    let before = sb.op_kinds("AGT-1");
    let (status, err) = app.post(
        "/tickets/AGT-1/body",
        json!({"update": pm_core::bytes::encode(second.as_bytes())}),
    );
    assert_eq!(status, 400, "{err}");
    assert!(err["error"].as_str().unwrap().contains("history"), "{err}");
    assert_eq!(sb.op_kinds("AGT-1"), before);
}

#[test]
fn a_cli_write_in_another_process_reaches_the_stream_within_a_second() {
    let sb = Sandbox::new();
    let app = App::start(&sb, &["--idle", "0"]);
    let events = app.events();
    let hello = events.next(Duration::from_secs(5));
    assert_eq!(hello["schema"], 1);
    let head = hello["seq"].as_i64().unwrap();
    assert!(head > 0, "{hello}");

    // A write from another process: the CLI.
    assert_ok(&sb.run(&["set", "AGT-1", "repo=o/r"], &[]));
    let started = Instant::now();
    let op = events.next(Duration::from_secs(1));
    assert!(started.elapsed() < Duration::from_secs(1));
    assert_eq!(op["kind"], "field.set");
    assert_eq!(op["id"], "AGT-1");
    assert_eq!(op["actor"], "tester");
    assert_eq!(op["seq"].as_i64().unwrap(), head + 1);
    assert!(op["hlc"]["wall_ms"].as_u64().unwrap() > 0);
    assert!(op.get("payload").is_none(), "{op}");
    let ulid = sb.json(&["show", "AGT-1"])["ulid"].clone();
    assert_eq!(op["entity"], ulid);

    // And a write through the API, which nudges the watcher.
    let (status, _) = app.post("/tickets/AGT-1/labels", json!({"add": ["z"]}));
    assert_eq!(status, 200);
    let op = events.next(Duration::from_secs(1));
    assert_eq!(op["kind"], "label.add");
    assert_eq!(op["actor"], "tester:app");
    assert_eq!(op["seq"].as_i64().unwrap(), head + 2);

    // A config op streams too, with no ticket id.
    assert_ok(&sb.run(&["workspace", "gate-label", "add", "wip"], &[]));
    let op = events.next(Duration::from_secs(1));
    assert_eq!(op["kind"], "workspace.set");
    assert!(op["id"].is_null(), "{op}");

    // A second stream starts from the head as it now stands.
    let second = app.events();
    let hello2 = second.next(Duration::from_secs(5));
    assert_eq!(hello2["seq"].as_i64().unwrap(), head + 3);
    assert!(events.try_next().is_none(), "nothing else was committed");
}

#[test]
fn the_server_exits_once_the_last_view_has_been_gone_for_the_grace() {
    let sb = Sandbox::new();
    let mut app = App::start(&sb, &["--idle", "1"]);
    assert_eq!(app.launch["idle_secs"], 1);

    // A connected view keeps it alive past the grace.
    let view = app.events();
    view.next(Duration::from_secs(5));
    thread::sleep(Duration::from_millis(1500));
    assert!(
        app.child.try_wait().unwrap().is_none(),
        "exited with a view connected"
    );
    assert_eq!(app.get("/workspace").0, 200);

    // The view goes away: the stream is dropped, and the grace runs out.
    drop(view);
    let code = app.wait_exit(Duration::from_secs(10));
    assert_eq!(code, Some(0), "did not exit after the last view left");
}

#[test]
fn a_launch_nobody_connects_to_exits_after_the_grace() {
    let sb = Sandbox::new();
    let mut app = App::start(&sb, &["--idle", "1"]);
    assert_eq!(
        app.get("/workspace").0,
        200,
        "plain requests do not count as a view"
    );
    let code = app.wait_exit(Duration::from_secs(10));
    assert_eq!(code, Some(0));
}

#[test]
fn a_missing_workspace_is_exit_1_before_anything_listens() {
    let home = tempfile::tempdir().unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_pm"))
        .args(["app", "--workspace"])
        .arg(home.path().join("nowhere"))
        .env_clear()
        .env("HOME", home.path())
        .env("USER", "tester")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert!(
        out.stdout.is_empty(),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
}
