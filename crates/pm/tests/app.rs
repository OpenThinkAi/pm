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
                        kind: Default::default(),
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
            // Never a real ui-leaf: a non-`--json` `pm app` here must not
            // open a browser (tests/launch.rs drives a fake one).
            .env("PATH", "/usr/bin:/bin")
            .env("UI_LEAF_NO_OPEN", "1")
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
        (
            "/projects?status=parked",
            vec!["project", "list", "--status", "parked"],
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

/// The board's drop (AGT-1404, `planMove` in crates/pm/views/lib/board.ts)
/// sends exactly `{"state", "keep_assignee": false}` addressed by the
/// ticket's ref — the ULID when it has no number yet — and that is one
/// `state.transition` (plus `pm move`'s assignee clear into an unstarted
/// state), streamed to every open board.
#[test]
fn a_board_move_is_a_state_transition_by_ref() {
    let sb = Sandbox::new();
    assert_ok(&sb.run(&["set", "AGT-1", "assignee=matt"], &[]));
    let ulid = sb.json(&["show", "AGT-1"])["ulid"]
        .as_str()
        .unwrap()
        .to_string();
    let app = App::start(&sb, &["--idle", "0"]);
    let events = app.events();
    events.next(Duration::from_secs(5)); // hello

    let before = sb.op_kinds("AGT-1").len();
    let (status, t) = app.post(
        &format!("/tickets/{ulid}/state"),
        json!({"state": "done", "keep_assignee": false}),
    );
    assert_eq!(status, 200, "{t}");
    assert_eq!(t["state"], "done");
    assert_eq!(t["assignee"], "matt", "a completed move keeps the assignee");
    assert_eq!(sb.op_kinds("AGT-1")[before..], ["state.transition"]);
    let op = events.next(Duration::from_secs(1));
    assert_eq!(op["kind"], "state.transition");
    assert_eq!(op["id"], "AGT-1");

    let (status, t) = app.post(
        &format!("/tickets/{ulid}/state"),
        json!({"state": "triage", "keep_assignee": false}),
    );
    assert_eq!(status, 200, "{t}");
    assert_eq!(t["state"], "triage");
    assert!(t["assignee"].is_null(), "back to unstarted un-assigns: {t}");
    assert_eq!(sb.op_kinds("AGT-1")[before + 1], "state.transition");
    assert_eq!(
        t,
        sb.json(&["show", "AGT-1"]),
        "the answer is `pm show --json`"
    );
}

/// `GET /tickets/AGT-1/body?since=…` for an encoded version vector,
/// percent-encoded as a view's `encodeURIComponent` does.
fn since_path(version: &[u8]) -> String {
    let b64 = pm_core::bytes::encode(version)
        .replace('+', "%2B")
        .replace('/', "%2F")
        .replace('=', "%3D");
    format!("/tickets/AGT-1/body?since={b64}")
}

#[test]
fn body_since_a_version_ships_only_what_the_editor_lacks() {
    let sb = Sandbox::new();
    let app = App::start(&sb, &["--idle", "0"]);

    // An editor bound to the snapshot, with an edit of its own the server
    // has not seen yet.
    let (_, body) = app.get("/tickets/AGT-1/body");
    let mut doc = Body::new();
    doc.apply(&BodyUpdate::from_bytes(
        pm_core::bytes::decode(body["snapshot"].as_str().unwrap()).unwrap(),
    ))
    .unwrap();
    doc.diff_from_text("LINE ONE\nline two").unwrap();

    // Another window's edit lands through the API.
    let (status, _) = app.post(
        "/tickets/AGT-1/body",
        json!({"text": "line one\nline two\nline three"}),
    );
    assert_eq!(status, 200);

    // The editor asks for what it lacks: exactly that edit, no snapshot.
    let (status, caught_up) = app.get(&since_path(&doc.version()));
    assert_eq!(status, 200, "{caught_up}");
    assert_eq!(caught_up["id"], "AGT-1");
    assert_eq!(caught_up["text"], "line one\nline two\nline three");
    assert!(caught_up.get("snapshot").is_none(), "{caught_up}");
    let update = pm_core::bytes::decode(caught_up["update"].as_str().unwrap()).unwrap();
    doc.apply(&BodyUpdate::from_bytes(update)).unwrap();
    assert_eq!(doc.text(), "LINE ONE\nline two\nline three");

    // Caught up: the next answer changes nothing.
    let (_, again) = app.get(&since_path(&doc.version()));
    let nothing = pm_core::bytes::decode(again["update"].as_str().unwrap()).unwrap();
    doc.apply(&BodyUpdate::from_bytes(nothing)).unwrap();
    assert_eq!(doc.text(), "LINE ONE\nline two\nline three");

    // An empty version is "everything"; garbage is a 400.
    let (status, all) = app.get("/tickets/AGT-1/body?since=");
    assert_eq!(status, 200, "{all}");
    let mut fresh = Body::new();
    fresh
        .apply(&BodyUpdate::from_bytes(
            pm_core::bytes::decode(all["update"].as_str().unwrap()).unwrap(),
        ))
        .unwrap();
    assert_eq!(fresh.text(), "line one\nline two\nline three");
    assert_eq!(app.get("/tickets/AGT-1/body?since=not%20base64!").0, 400);
    assert_eq!(app.get("/tickets/AGT-1/body?since=%2F%2F%2F%2F").0, 400);
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

// ---------------------------------------------------- project documents

/// A **Ticket** minus what differs between two tickets filed the same way:
/// identity, number and stamps.
fn filed_shape(t: &Value) -> Value {
    let mut t = t.clone();
    for key in ["id", "ulid", "number", "created", "updated"] {
        t.as_object_mut().unwrap().remove(key);
    }
    t
}

/// `GET <path>?since=…` for an encoded version vector, percent-encoded.
fn since(path: &str, version: &[u8]) -> String {
    let b64 = pm_core::bytes::encode(version)
        .replace('+', "%2B")
        .replace('/', "%2F")
        .replace('=', "%3D");
    format!("{path}?since={b64}")
}

/// An editor document bound to a body endpoint's snapshot.
fn bind(answer: &Value) -> Body {
    let mut doc = Body::new();
    doc.apply(&BodyUpdate::from_bytes(
        pm_core::bytes::decode(answer["snapshot"].as_str().unwrap()).unwrap(),
    ))
    .unwrap();
    doc
}

/// Every op on `entity` in the log, by kind (a document's `body.edit`s
/// are not in `pm log`, which lists tickets and config ops).
fn entity_op_kinds(sb: &Sandbox, entity: &str) -> Vec<String> {
    sb.store()
        .ops(entity.parse().unwrap())
        .unwrap()
        .iter()
        .map(|op| op.payload.kind().to_string())
        .collect()
}

/// AGT-1405: the design doc and a named document are CRDT bodies served
/// and written exactly like a ticket's description — snapshot, `since`,
/// `{"update"}` and `{"text"}` — and every write is one `body.edit` on the
/// document's own `doc_id`, visible to `pm project show` and rebuilt by
/// `pm doctor --rebuild`.
#[test]
fn project_documents_are_crdt_bodies_on_their_doc_ids() {
    let sb = Sandbox::new();
    assert_ok(&sb.run(&["project", "new", "design", "--title", "Design"], &[]));
    let notes = sb.path("notes.md");
    std::fs::write(&notes, "first note\n").unwrap();
    assert_ok(&sb.run(
        &[
            "project",
            "doc",
            "add",
            "design",
            "run notes",
            "--from-file",
            notes.to_str().unwrap(),
        ],
        &[],
    ));
    let app = App::start(&sb, &["--idle", "0"]);
    let events = app.events();
    events.next(Duration::from_secs(5)); // hello

    // The design doc: empty so far, with its own doc_id.
    let (status, design) = app.get("/projects/design/body");
    assert_eq!(status, 200, "{design}");
    assert_eq!(design["project"], "design");
    assert!(design["doc"].is_null(), "{design}");
    assert_eq!(design["text"], "");
    let design_id = design["doc_id"].as_str().unwrap().to_string();
    assert_ne!(design_id, "");

    // An editor's own Loro update lands as one body.edit on that doc_id.
    let mut doc = bind(&design);
    let update = doc.diff_from_text("# Design\n\nThe plan.\n").unwrap();
    let (status, written) = app.post(
        "/projects/design/body",
        json!({"update": pm_core::bytes::encode(update.as_bytes())}),
    );
    assert_eq!(status, 200, "{written}");
    assert_eq!(written["text"], "# Design\n\nThe plan.\n");
    assert_eq!(written["doc_id"], design_id);
    assert_eq!(
        sb.json(&["project", "show", "design"])["doc"],
        "# Design\n\nThe plan.\n"
    );
    assert_eq!(entity_op_kinds(&sb, &design_id), ["body.edit"]);
    let op = events.next(Duration::from_secs(2));
    assert_eq!(op["kind"], "body.edit");
    assert_eq!(op["entity"], design_id.as_str());
    assert!(op["id"].is_null(), "a document op names no ticket: {op}");

    // Another window's whole-text save merges with the editor's unsent
    // edit; `since` ships only what the editor lacks.
    doc.diff_from_text("# Design!\n\nThe plan.\n").unwrap();
    let (status, _) = app.post(
        "/projects/design/body",
        json!({"text": "# Design\n\nThe plan.\n\nMore.\n"}),
    );
    assert_eq!(status, 200);
    let (status, caught_up) = app.get(&since("/projects/design/body", &doc.version()));
    assert_eq!(status, 200, "{caught_up}");
    assert!(caught_up.get("snapshot").is_none(), "{caught_up}");
    doc.apply(&BodyUpdate::from_bytes(
        pm_core::bytes::decode(caught_up["update"].as_str().unwrap()).unwrap(),
    ))
    .unwrap();
    assert_eq!(doc.text(), "# Design!\n\nThe plan.\n\nMore.\n");
    let mine = doc.updates_since(&[]).unwrap();
    let (status, merged) = app.post(
        "/projects/design/body",
        json!({"update": pm_core::bytes::encode(mine.as_bytes())}),
    );
    assert_eq!(status, 200, "{merged}");
    assert_eq!(merged["text"], "# Design!\n\nThe plan.\n\nMore.\n");
    // Same text again commits nothing.
    let before = entity_op_kinds(&sb, &design_id).len();
    let (status, _) = app.post(
        "/projects/design/body",
        json!({"text": "# Design!\n\nThe plan.\n\nMore.\n"}),
    );
    assert_eq!(status, 200);
    assert_eq!(entity_op_kinds(&sb, &design_id).len(), before);

    // A named document, by name (percent-encoded), the same way.
    let (status, named) = app.get("/projects/design/docs/run%20notes/body");
    assert_eq!(status, 200, "{named}");
    assert_eq!(named["doc"], "run notes");
    assert_eq!(named["text"], "first note\n");
    let named_id = named["doc_id"].as_str().unwrap().to_string();
    assert_ne!(named_id, design_id);
    let mut notes_doc = bind(&named);
    let update = notes_doc
        .diff_from_text("first note\nsecond note\n")
        .unwrap();
    let (status, written) = app.post(
        "/projects/design/docs/run%20notes/body",
        json!({"update": pm_core::bytes::encode(update.as_bytes())}),
    );
    assert_eq!(status, 200, "{written}");
    assert_eq!(
        sb.json(&["project", "show", "design", "--doc", "run notes"])["body"],
        "first note\nsecond note\n"
    );
    assert_eq!(entity_op_kinds(&sb, &named_id), ["body.edit", "body.edit"]);

    // Refusals, with nothing written.
    let kinds = entity_op_kinds(&sb, &design_id);
    assert_eq!(app.get("/projects/nope/body").0, 404);
    assert_eq!(app.get("/projects/design/docs/nope/body").0, 404);
    assert_eq!(app.post("/projects/nope/body", json!({"text": "x"})).0, 404);
    assert_eq!(app.get("/projects/design/body?since=%2F%2F%2F%2F").0, 400);
    let mut stranger = Body::new();
    stranger.diff_from_text("unrelated").unwrap();
    let orphan = stranger.diff_from_text("unrelated, more").unwrap();
    let (status, err) = app.post(
        "/projects/design/body",
        json!({"update": pm_core::bytes::encode(orphan.as_bytes())}),
    );
    assert_eq!(status, 400, "{err}");
    assert_eq!(
        app.post("/projects/design/body", json!({"update": "", "text": ""}))
            .0,
        400
    );
    assert_eq!(entity_op_kinds(&sb, &design_id), kinds);

    // It is ordinary history: a rebuild from the log reproduces both
    // documents, and the database is healthy.
    drop(events);
    drop(app);
    let rebuilt = sb.json(&["doctor", "--rebuild"]);
    assert_eq!(rebuilt["healthy"], true, "{rebuilt}");
    let project = sb.json(&["project", "show", "design"]);
    assert_eq!(project["doc"], "# Design!\n\nThe plan.\n\nMore.\n");
    assert_eq!(
        project["documents"]["run notes"],
        "first note\nsecond note\n"
    );
    assert_ok(&sb.run(&["doctor"], &[]));
}

/// AGT-1405: "New ticket" is `POST /tickets`, which files exactly as
/// `pm new` with the same flags — same ops, same Ticket — and answers 201
/// with `pm new --json`'s shape.
#[test]
fn post_tickets_files_exactly_as_pm_new() {
    let sb = Sandbox::new();
    let app = App::start(&sb, &["--idle", "0"]);
    let events = app.events();
    events.next(Duration::from_secs(5)); // hello

    let (status, made) = app.post(
        "/tickets",
        json!({
            "title": "  Filed from the view ",
            "project": "pm",
            "priority": "high",
            "labels": ["x", "y"],
            "description": "why\n",
            "blocked_by": ["AGT-1"],
        }),
    );
    assert_eq!(status, 201, "{made}");
    assert_eq!(made["id"], "AGT-3");
    assert_eq!(made["title"], "Filed from the view");
    assert!(made.get("comments").is_none(), "pm new --json has none");
    assert_eq!(
        made,
        sb.json(&["show", "AGT-3"])
            .as_object()
            .map(|o| {
                let mut o = o.clone();
                o.remove("comments");
                Value::Object(o)
            })
            .unwrap()
    );
    let op = events.next(Duration::from_secs(2));
    assert_eq!(op["kind"], "ticket.create");
    assert_eq!(op["id"], "AGT-3");

    let cli = sb.json(&[
        "new",
        "--title",
        "Filed from the view",
        "--project",
        "pm",
        "--priority",
        "high",
        "--label",
        "x,y",
        "--description",
        "why\n",
        "--blocked-by",
        "AGT-1",
    ]);
    assert_eq!(cli["id"], "AGT-4");
    assert_eq!(filed_shape(&made), filed_shape(&cli));
    assert_eq!(sb.op_kinds("AGT-3"), sb.op_kinds("AGT-4"));

    // Just a title and a project, as the project view sends it.
    let (status, bare) = app.post("/tickets", json!({"title": "Bare", "project": "pm"}));
    assert_eq!(status, 201, "{bare}");
    let cli = sb.json(&["new", "--title", "Bare", "--project", "pm"]);
    assert_eq!(filed_shape(&bare), filed_shape(&cli));

    // pm new's refusals, and nothing filed.
    let count = sb.json(&["list"]).as_array().unwrap().len();
    for (body, want) in [
        (json!({"project": "pm"}), 400),
        (json!({"title": "  "}), 400),
        (json!({"title": "x", "project": "nope"}), 404),
        (json!({"title": "x", "blocked_by": ["AGT-99"]}), 404),
        (json!({"title": "x", "priority": "urgent"}), 400),
        (json!({"title": "x", "state": "done"}), 400),
        (json!({"title": "x", "labels": "a"}), 400),
        (json!(["title"]), 400),
    ] {
        let (status, err) = app.post("/tickets", body.clone());
        assert_eq!(status, want, "{body}: {err}");
    }
    assert_eq!(sb.json(&["list"]).as_array().unwrap().len(), count);
}

/// With a hub configured — even one that cannot be reached — `POST
/// /tickets` numbers nothing locally, exactly as `pm new` does
/// (AGT-1398): the ticket is `AGT-?`, pending, and named by its ULID,
/// which is what the project view opens it by.
#[test]
fn post_tickets_leaves_the_number_to_a_configured_hub() {
    let sb = Sandbox::new();
    let config = sb.path(".config/pm/config.toml");
    let base = std::fs::read_to_string(&config).unwrap();
    std::fs::write(&config, format!("hub = \"http://127.0.0.1:1\"\n{base}")).unwrap();
    let app = App::start(&sb, &["--idle", "0"]);

    let (status, made) = app.post("/tickets", json!({"title": "Pending", "project": "pm"}));
    assert_eq!(status, 201, "{made}");
    assert_eq!(made["id"], "AGT-?");
    assert!(made["number"].is_null(), "{made}");
    let ulid = made["ulid"].as_str().unwrap().to_string();
    let cli = sb.json(&["new", "--title", "Pending", "--project", "pm"]);
    assert_eq!(cli["id"], "AGT-?");
    assert_eq!(filed_shape(&made), filed_shape(&cli));
    let doctor = sb.json(&["doctor"]);
    assert_eq!(doctor["sync"]["pending_numbers"], 2, "{doctor}");

    // The editor opens it by ULID: reads and writes work, AGT-? does not.
    let (status, t) = app.get(&format!("/tickets/{ulid}"));
    assert_eq!(status, 200, "{t}");
    assert_eq!(t["title"], "Pending");
    let (status, _) = app.get(&format!("/tickets/{ulid}/body"));
    assert_eq!(status, 200);
    let (status, t) = app.post(
        &format!("/tickets/{ulid}/body"),
        json!({"text": "filled in"}),
    );
    assert_eq!(status, 200, "{t}");
    assert_eq!(t["description"], "filled in");
    assert_eq!(app.get("/tickets/AGT-?").0, 400);
}

/// `{backlog, unstarted, started, completed, canceled}` as the API spells
/// it.
fn cats(unstarted: u64, started: u64, completed: u64) -> Value {
    json!({
        "backlog": 0,
        "unstarted": unstarted,
        "started": started,
        "completed": completed,
        "canceled": 0,
    })
}

#[test]
fn initiatives_are_a_project_tree_with_ticket_rollups() {
    let sb = Sandbox::new();
    for args in [
        vec![
            "project",
            "new",
            "ini",
            "--title",
            "Initiative",
            "--kind",
            "initiative",
        ],
        vec![
            "project", "new", "child", "--title", "Child", "--parent", "ini",
        ],
        vec![
            "project", "new", "sub", "--title", "Sub", "--parent", "child",
        ],
    ] {
        assert_ok(&sb.run(&args, &[]));
    }
    // AGT-3 on the initiative itself, started; AGT-4 on child, done;
    // AGT-5 on sub, triage; AGT-6 on sub, archived (counts nowhere).
    for (n, (project, state)) in [
        ("ini", "in-progress"),
        ("child", "done"),
        ("sub", "triage"),
        ("sub", "done"),
    ]
    .into_iter()
    .enumerate()
    {
        assert_ok(&sb.run(&["new", "--title", "t", "--project", project], &[]));
        if state != "triage" {
            let id = format!("AGT-{}", n + 3);
            assert_ok(&sb.run(&["move", &id, state], &[]));
        }
    }
    assert_ok(&sb.run(&["archive", "AGT-6"], &[]));
    // A parent cycle only a concurrent sync could produce: put straight
    // into the store, past `pm project set`'s guard.
    {
        let mut store = sb.store();
        for (id, parent) in [
            ("cy-a", None),
            ("cy-b", Some("cy-a")),
            ("cy-a", Some("cy-b")),
        ] {
            store
                .put_project(
                    &Project {
                        kind: Default::default(),
                        id: id.into(),
                        title: id.into(),
                        status: ProjectStatus::InProgress,
                        parent: parent.map(str::to_string),
                        repos: Default::default(),
                        doc: "a doc body the tree leaves out".into(),
                        documents: Default::default(),
                    },
                    &pm_core::ActorId::new("matt"),
                )
                .unwrap();
        }
    }
    assert_eq!(sb.json(&["project", "show", "cy-a"])["parent"], "cy-b");
    assert_eq!(sb.json(&["project", "show", "cy-b"])["parent"], "cy-a");
    assert_ok(&sb.run(&["new", "--title", "t", "--project", "cy-b"], &[]));
    assert_ok(&sb.run(&["move", "AGT-7", "in-progress"], &[]));
    let app = App::start(&sb, &["--idle", "0"]);

    let (status, tree) = app.get("/initiatives");
    assert_eq!(status, 200, "{tree}");
    assert_eq!(
        tree,
        json!({
            "schema": 1,
            "initiatives": [{
                "id": "ini",
                "title": "Initiative",
                "kind": "initiative",
                "status": "in-progress",
                "tickets": cats(0, 1, 0),
                "total": cats(1, 1, 1),
                "children": [{
                    "id": "child",
                    "title": "Child",
                    "kind": "project",
                    "status": "in-progress",
                    "tickets": cats(0, 0, 1),
                    "total": cats(1, 0, 1),
                    "children": [{
                        "id": "sub",
                        "title": "Sub",
                        "kind": "project",
                        "status": "in-progress",
                        "tickets": cats(1, 0, 0),
                        "total": cats(1, 0, 0),
                        "children": [],
                    }],
                }],
            }],
            "unfiled": {
                "total": cats(2, 1, 0),
                "projects": [
                    // The cycle is cut at its smallest id; each member once.
                    {
                        "id": "cy-a",
                        "title": "cy-a",
                        "kind": "project",
                        "status": "in-progress",
                        "tickets": cats(0, 0, 0),
                        "total": cats(0, 1, 0),
                        "children": [{
                            "id": "cy-b",
                            "title": "cy-b",
                            "kind": "project",
                            "status": "in-progress",
                            "tickets": cats(0, 1, 0),
                            "total": cats(0, 1, 0),
                            "children": [],
                        }],
                    },
                    {
                        "id": "other",
                        "title": "other",
                        "kind": "project",
                        "status": "in-progress",
                        "tickets": cats(0, 0, 0),
                        "total": cats(0, 0, 0),
                        "children": [],
                    },
                    {
                        "id": "pm",
                        "title": "pm",
                        "kind": "project",
                        "status": "in-progress",
                        "tickets": cats(2, 0, 0),
                        "total": cats(2, 0, 0),
                        "children": [],
                    },
                ],
            },
        }),
    );

    // `?status=` filters as `GET /projects` does: a completed child drops
    // out and its sub-project hangs under the initiative instead.
    assert_ok(&sb.run(&["project", "set", "child", "status=complete"], &[]));
    let (status, tree) = app.get("/initiatives?status=in-progress");
    assert_eq!(status, 200, "{tree}");
    let ini = &tree["initiatives"][0];
    assert_eq!(ini["children"][0]["id"], "sub", "{tree}");
    assert_eq!(ini["total"], cats(1, 1, 0), "{tree}");
    let (status, tree) = app.get("/initiatives?status=complete");
    assert_eq!(status, 200, "{tree}");
    assert_eq!(tree["initiatives"], json!([]), "{tree}");
    // `child` is filed under `ini`, which is not shown: not Unfiled.
    assert_eq!(tree["unfiled"]["projects"], json!([]), "{tree}");
    assert_eq!(app.get("/initiatives?status=bogus").0, 400);
    // AGT-1635: a parked project carries its status in the tree, and
    // `?status=parked` is a valid filter (it keeps only `sub`, filed
    // under an initiative it does not show, so neither group lists it).
    assert_ok(&sb.run(&["project", "set", "sub", "status=parked"], &[]));
    let (status, tree) = app.get("/initiatives?status=parked");
    assert_eq!(status, 200, "{tree}");
    assert_eq!(tree["initiatives"], json!([]), "{tree}");
    assert_eq!(tree["unfiled"]["projects"], json!([]), "{tree}");
    let (_, tree) = app.get("/initiatives");
    let sub = &tree["initiatives"][0]["children"][0]["children"][0];
    assert_eq!(
        (&sub["id"], &sub["status"]),
        (&json!("sub"), &json!("parked")),
        "{tree}"
    );

    // The same guard as every route.
    let host = format!("Host: 127.0.0.1:{}", app.port());
    let (code, _) = app.raw("GET", "/initiatives", std::slice::from_ref(&host));
    assert_eq!(code, 401);
    let (code, _) = app.raw(
        "GET",
        "/initiatives",
        &["Host: evil.example".into(), app.bearer()],
    );
    assert_eq!(code, 403);
    let (code, _) = app.raw(
        "GET",
        "/initiatives",
        &[host, app.bearer(), "Origin: http://evil.example".into()],
    );
    assert_eq!(code, 403);
}
