//! Hub-assigned ticket numbers (AGT-1398). With a `hub` configured, `pm
//! new` allocates nothing locally: the ticket is `T-?` (`number: null`)
//! and named by its ULID until a `pm sync` brings the hub's `field.set
//! number`. Without a hub nothing changes. The two-replica AC3 test runs
//! the real `pm-hub` on a throwaway Postgres through pm-hub's own harness
//! (skipped, with a message, when neither docker nor
//! `PM_HUB_TEST_DATABASE_URL` is available); everything else needs no hub
//! at all — `pm new` never contacts one.
//!
//! Every replica has its own temp HOME with a hand-written config.toml,
//! and the token travels in `PM_HUB_TOKEN`: the real `~/.config/pm`, the
//! login keychain and any live workspace are never touched.

#[path = "../../pm-hub/tests/common/mod.rs"]
mod hub;

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};

use serde_json::Value;
use tempfile::TempDir;

use hub::{admin, free_port, postgres_for, request_body, spawn_hub, wait_for_health};

/// Nothing listens on port 1: a connection is refused at once, which is
/// what "the hub is unreachable" looks like to `pm sync`.
const DEAD_HUB: &str = "http://127.0.0.1:1";

/// One machine: its own HOME (config.toml), workspace directory and actor.
struct Replica {
    home: TempDir,
    ws: PathBuf,
    actor: String,
    token: Option<String>,
}

impl Replica {
    /// A fresh `pm init --prefix T` workspace, no hub configured.
    fn init(actor: &str) -> Self {
        let home = tempfile::tempdir().unwrap();
        let ws = home.path().join("ws");
        let r = Replica {
            home,
            ws,
            actor: actor.to_string(),
            token: None,
        };
        r.ok(&["init", "--prefix", "T"]);
        r
    }

    /// A second replica of `source`'s workspace: a byte copy of its
    /// database, taken while no `pm` process has it open.
    fn clone_from(source: &Replica, actor: &str) -> Self {
        let home = tempfile::tempdir().unwrap();
        let ws = home.path().join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        for name in ["pm.sqlite", "pm.sqlite-wal", "pm.sqlite-shm"] {
            let from = source.ws.join(name);
            if from.is_file() {
                std::fs::copy(&from, ws.join(name)).unwrap();
            }
        }
        Replica {
            home,
            ws,
            actor: actor.to_string(),
            token: None,
        }
    }

    fn config_path(&self) -> PathBuf {
        self.home.path().join(".config/pm/config.toml")
    }

    /// `hub = "<url>"` in this replica's config.toml, written directly (no
    /// `pm hub login`, which on macOS would also write a keychain item).
    fn configure(&mut self, hub_url: &str, token: &str) {
        let path = self.config_path();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, format!("hub = \"{hub_url}\"\n")).unwrap();
        self.token = Some(token.to_string());
    }

    /// Back to no hub at all.
    fn unconfigure(&mut self) {
        let _ = std::fs::remove_file(self.config_path());
        self.token = None;
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_pm"));
        cmd.args(["--workspace", self.ws.to_str().unwrap()])
            .args(args)
            .env_clear()
            .env("HOME", self.home.path())
            .env("USER", "tester")
            .env("PM_ACTOR", &self.actor)
            .env("TMPDIR", self.home.path())
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(t) = &self.token {
            cmd.env("PM_HUB_TOKEN", t);
        }
        cmd
    }

    fn pm(&self, args: &[&str]) -> Output {
        self.command(args).output().unwrap()
    }

    fn ok(&self, args: &[&str]) -> Output {
        let out = self.pm(args);
        assert!(out.status.success(), "pm {args:?}: {}", err(&out));
        out
    }

    fn json(&self, args: &[&str]) -> (i32, Value) {
        let mut a = args.to_vec();
        a.push("--json");
        let out = self.pm(&a);
        let v = serde_json::from_slice(&out.stdout)
            .unwrap_or_else(|e| panic!("not JSON ({e}): {}\n{}", out_s(&out), err(&out)));
        (out.status.code().unwrap(), v)
    }

    fn json_ok(&self, args: &[&str]) -> Value {
        let (code, v) = self.json(args);
        assert_eq!(code, 0, "pm {args:?}: {v}");
        v
    }

    /// `pm new --title <title>` in text mode, asserting the pending shape
    /// (`T-?  <ULID>`) and returning the ULID.
    fn new_pending(&self, title: &str) -> String {
        let out = self.ok(&["new", "--title", title]);
        let line = out_s(&out);
        let (id, ulid) = line
            .trim_end()
            .split_once("  ")
            .unwrap_or_else(|| panic!("not `T-?  <ULID>`: {line:?}"));
        assert_eq!(id, "T-?", "{line:?}");
        assert!(looks_like_ulid(ulid), "{line:?}");
        assert!(
            line.ends_with('\n') && line.lines().count() == 1,
            "{line:?}"
        );
        ulid.to_string()
    }

    fn show(&self, id: &str) -> Value {
        self.json_ok(&["show", id])
    }

    /// `pm doctor --json`'s `sync` block: outbox, pushed_through, cursor,
    /// pending_numbers.
    fn sync_state(&self) -> Value {
        self.json_ok(&["doctor"])["sync"].clone()
    }

    fn workspace_id(&self) -> String {
        self.json_ok(&["hub", "status"])["workspace"]["id"]
            .as_str()
            .unwrap()
            .to_string()
    }

    /// `pm sync --json`, asserting exit 0.
    fn sync(&self) -> Value {
        self.json_ok(&["sync"])
    }
}

fn out_s(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}
fn err(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

/// Crockford base32 ULID shape: 26 characters, first `0`-`7`.
fn looks_like_ulid(s: &str) -> bool {
    const ALPHABET: &str = "0123456789ABCDEFGHJKMNPQRSTVWXYZ";
    s.len() == 26
        && matches!(s.as_bytes()[0], b'0'..=b'7')
        && s.bytes().all(|b| ALPHABET.contains(b as char))
}

fn strings(v: &Value) -> Vec<String> {
    v.as_array()
        .unwrap_or_else(|| panic!("not an array: {v}"))
        .iter()
        .map(|s| s.as_str().unwrap().to_string())
        .collect()
}

// ------------------------------------------------------------ no hub

/// Without a hub configured nothing changes: `pm new` numbers locally, no
/// ticket is ever pending, and `T-?` is not an id.
#[test]
fn without_a_hub_pm_new_numbers_locally() {
    let r = Replica::init("alice");
    let out = r.ok(&["new", "--title", "local"]);
    assert_eq!(out_s(&out), "T-1\n");
    let t = r.json_ok(&["new", "--title", "second"]);
    assert_eq!(t["id"], "T-2");
    assert_eq!(t["number"], 2);
    assert_eq!(r.sync_state()["pending_numbers"], 0);

    let batch = r.home.path().join("batch.yaml");
    std::fs::write(
        &batch,
        "tickets:\n  - ref: a\n    title: A\n  - title: B\n    blocked-by: [\"@a\"]\n",
    )
    .unwrap();
    let b = r.json_ok(&["new", "--batch", batch.to_str().unwrap()]);
    assert_eq!(b["refs"]["@a"], "T-3");
    assert_eq!(b["tickets"][1]["blocked_by"], serde_json::json!(["T-3"]));

    // `T-?` never names a ticket; the message says what does.
    let out = r.pm(&["show", "T-?"]);
    assert_eq!(out.status.code(), Some(2), "{}", err(&out));
    assert!(err(&out).contains("ULID"), "{}", err(&out));
    assert!(r.pm(&["doctor"]).status.success());
}

// ------------------------------------------------- hub configured, offline

/// AC1 + AC2 without a hub in reach: with `hub` configured, `pm new`
/// (single, `--from-file`, `--batch`) never allocates locally. The ticket
/// reads `T-?` / `number: null`, every reference to it is its ULID, and
/// every verb takes that ULID.
#[test]
fn with_a_hub_configured_pm_new_is_pending_and_addressed_by_ulid() {
    let mut r = Replica::init("alice");
    // One ticket numbered the old way, before the hub was configured.
    assert_eq!(out_s(&r.ok(&["new", "--title", "before"])), "T-1\n");
    r.configure(DEAD_HUB, "pmh_some-token");

    // ---- single: `T-?  <ULID>` on stdout, `T-?` / null in JSON.
    let a = r.new_pending("first pending");
    let shown = r.show(&a);
    assert_eq!(shown["id"], "T-?");
    assert!(shown["number"].is_null(), "{shown}");
    assert_eq!(shown["ulid"], a);
    assert_eq!(shown["title"], "first pending");
    let out = r.ok(&["show", &a]);
    assert!(
        out_s(&out).starts_with("T-?  first pending\n"),
        "{}",
        out_s(&out)
    );
    assert!(out_s(&out).contains(&format!("ulid:      {a}\n")));
    let (code, v) = r.json(&["new", "--title", "json pending"]);
    assert_eq!(code, 0);
    assert_eq!(v["id"], "T-?");
    assert!(v["number"].is_null());
    let b = v["ulid"].as_str().unwrap().to_string();
    // No `field.set number` was logged for either.
    for id in [&a, &b] {
        let log = r.json_ok(&["log", id]);
        assert!(
            log.as_array()
                .unwrap()
                .iter()
                .all(|op| op["kind"] != "field.set"),
            "{log}"
        );
    }
    let log = r.json_ok(&["log", "T-1"]);
    assert!(
        log.as_array()
            .unwrap()
            .iter()
            .any(|op| op["summary"] == "numbered 1"),
        "{log}"
    );

    // ---- `T-?` is not an id; the next number is not a ticket either.
    let out = r.pm(&["show", "T-?"]);
    assert_eq!(out.status.code(), Some(2), "{}", err(&out));
    assert!(
        err(&out).contains("awaiting its number") && err(&out).contains("pm sync"),
        "{}",
        err(&out)
    );
    let out = r.pm(&["show", "T-2"]);
    assert_eq!(out.status.code(), Some(3), "{}", err(&out));
    assert!(r.pm(&["show", "t-1"]).status.success());

    // ---- list: numbered first, then pending in creation order.
    let out = r.ok(&["list"]);
    let text = out_s(&out);
    let rows: Vec<&str> = text
        .lines()
        .map(|l| l.split("  ").next().unwrap())
        .collect();
    assert_eq!(rows, ["T-1", "T-?", "T-?"], "{text}");
    let list = r.json_ok(&["list"]);
    let list = list.as_array().unwrap();
    assert_eq!(list.len(), 3);
    assert_eq!(list[0]["id"], "T-1");
    assert_eq!(list[1]["ulid"], a);
    assert!(list[1]["number"].is_null());
    assert_eq!(list[2]["ulid"], b);

    // ---- --blocked-by takes a pending ticket's ULID next to a numbered
    // id; `blocked_by` names the pending blocker by ULID (AC2).
    let c = r.json_ok(&[
        "new",
        "--title",
        "blocked",
        "--blocked-by",
        &format!("{a},T-1"),
    ]);
    assert_eq!(c["id"], "T-?");
    let c_ulid = c["ulid"].as_str().unwrap().to_string();
    assert_eq!(
        strings(&c["blocked_by"])
            .into_iter()
            .collect::<BTreeSet<_>>(),
        ["T-1".to_string(), a.clone()].into_iter().collect()
    );
    // A `T-?` in --blocked-by is a usage error, not "not found".
    let out = r.pm(&["new", "--title", "x", "--blocked-by", "T-?"]);
    assert_eq!(out.status.code(), Some(2), "{}", err(&out));

    // ---- relate, set, label, comment, move, hold: all by ULID.
    let v = r.json_ok(&["relate", &c_ulid, "--unblock", &a]);
    assert_eq!(v["blocked_by"], serde_json::json!(["T-1"]));
    let v = r.json_ok(&["relate", &c_ulid, "--blocked-by", &b, "--blocks", &a]);
    assert_eq!(
        strings(&v["blocked_by"])
            .into_iter()
            .collect::<BTreeSet<_>>(),
        ["T-1".to_string(), b.clone()].into_iter().collect()
    );
    assert_eq!(r.show(&a)["blocked_by"], serde_json::json!([c_ulid]));
    assert_eq!(out_s(&r.ok(&["set", &a, "title=renamed"])), "T-?\n");
    assert_eq!(r.show(&a)["title"], "renamed");
    r.ok(&["label", &a, "+x"]);
    r.ok(&["comment", &a, "a note"]);
    r.ok(&["hold", &a, "waiting"]);
    assert_eq!(r.json_ok(&["holds"])["tickets"][0]["ulid"], a);
    r.ok(&["hold", &a, "--clear"]);
    let v = r.json_ok(&["move", &a, "in-progress"]);
    assert_eq!(v["state"], "in-progress");
    r.ok(&["move", &a, "todo"]);

    // ---- --from-file: pending too, blocked-by a ULID from the file.
    let file = r.home.path().join("ticket.md");
    std::fs::write(
        &file,
        format!("---\ntitle: From file\nblocked-by: [{a}]\n---\n\nBody.\n"),
    )
    .unwrap();
    let out = r.ok(&["new", "--from-file", file.to_str().unwrap()]);
    let line = out_s(&out);
    assert!(line.starts_with("T-?  "), "{line}");
    let f = r.json_ok(&["new", "--from-file", file.to_str().unwrap()]);
    assert_eq!(f["id"], "T-?");
    assert!(f["number"].is_null());
    assert_eq!(f["blocked_by"], serde_json::json!([a]));

    // ---- --batch: refs resolve to ULIDs, and those ULIDs work (AC2).
    let batch = r.home.path().join("batch.yaml");
    std::fs::write(
        &batch,
        format!(
            "tickets:\n  - ref: core\n    title: Core\n  - ref: leaf\n    title: Leaf\n    blocked-by: [\"@core\", \"{a}\", T-1]\n"
        ),
    )
    .unwrap();
    let out = r.ok(&["new", "--batch", batch.to_str().unwrap()]);
    let text = out_s(&out);
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 5, "{text}");
    for (line, title) in lines[..2].iter().zip(["Core", "Leaf"]) {
        let parts: Vec<&str> = line.split("  ").collect();
        assert_eq!(parts.len(), 3, "{line:?}");
        assert_eq!(parts[0], "T-?");
        assert!(looks_like_ulid(parts[1]), "{line:?}");
        assert_eq!(parts[2], title);
    }
    assert_eq!(lines[2], "refs:");
    let core = lines[3].strip_prefix("  @core -> ").unwrap();
    assert!(looks_like_ulid(core), "{text}");
    assert_eq!(r.show(core)["title"], "Core");
    let bt = r.json_ok(&["new", "--batch", batch.to_str().unwrap()]);
    let core2 = bt["refs"]["@core"].as_str().unwrap();
    assert!(looks_like_ulid(core2), "{bt}");
    assert_eq!(bt["refs"]["@leaf"], bt["tickets"][1]["ulid"]);
    assert_eq!(bt["tickets"][0]["id"], "T-?");
    assert!(bt["tickets"][0]["number"].is_null());
    assert_eq!(
        strings(&bt["tickets"][1]["blocked_by"])
            .into_iter()
            .collect::<BTreeSet<_>>(),
        ["T-1".to_string(), a.clone(), core2.to_string()]
            .into_iter()
            .collect()
    );

    // ---- graph / ready / check name a pending ticket by ULID.
    let g = r.json_ok(&["graph"]);
    let waves: Vec<String> = g["waves"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(strings)
        .collect();
    assert!(
        waves.contains(&"T-1".to_string()) && waves.contains(&a),
        "{g}"
    );
    assert!(!waves.iter().any(|w| w == "T-?"), "{g}");
    let g = r.json_ok(&["graph", "--ids", &format!("{a},{c_ulid}")]);
    assert_eq!(g["ids"], serde_json::json!([a, c_ulid]));
    let out = r.ok(&["graph"]);
    assert!(
        out_s(&out).contains(&a) && !out_s(&out).contains("T-?"),
        "{}",
        out_s(&out)
    );
    let rd = r.json_ok(&["ready"]);
    let ready: Vec<&Value> = rd["ready"].as_array().unwrap().iter().collect();
    // `b` is unblocked; `a` is blocked (by `c`, above) and so lands in a
    // later wave, named by ULID there.
    assert!(
        ready.iter().any(|t| t["ulid"] == b && t["id"] == "T-?"),
        "{rd}"
    );
    assert!(!ready.iter().any(|t| t["ulid"] == a), "{rd}");
    let waves: Vec<String> = rd["waves"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(strings)
        .collect();
    assert!(
        waves.contains(&a) && !waves.iter().any(|w| w == "T-?"),
        "{rd}"
    );
    let rd = r.json_ok(&["ready", "--ids", &c_ulid, "--explain"]);
    assert_eq!(rd["ids"], serde_json::json!([c_ulid]));
    assert_eq!(rd["excluded"][0]["id"], c_ulid);
    let (code, ck) = r.json(&["check"]);
    assert_eq!(code, 1, "{ck}");
    let named: BTreeSet<String> = ck["findings"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|f| strings(&f["tickets"]))
        .collect();
    assert!(named.contains("T-1") && named.contains(&a), "{ck}");
    assert!(!named.contains("T-?"), "{ck}");

    // ---- the pending set is exactly what was created, and healthy.
    let state = r.sync_state();
    assert_eq!(state["pending_numbers"], 9, "{state}");
    assert!(r.pm(&["doctor"]).status.success());
    // A sync cannot reach the hub; nothing changes, the tickets stay pending.
    let out = r.pm(&["sync"]);
    assert_eq!(out.status.code(), Some(1), "{}", err(&out));
    assert_eq!(r.sync_state(), state);
    assert_eq!(r.show(&a)["id"], "T-?");

    // ---- with the hub gone from config, local numbering is back (the
    // pending tickets stay pending: only the hub can number them).
    r.unconfigure();
    assert_eq!(out_s(&r.ok(&["new", "--title", "local again"])), "T-2\n");
    assert_eq!(r.show(&a)["id"], "T-?");
    assert_eq!(r.sync_state()["pending_numbers"], 9);
}

// ---------------------------------------------------------- AC3: two replicas

fn create_token(url: &str, name: &str, workspace: &str) -> String {
    let (ok, stdout, stderr) = admin(
        url,
        &["token", "create", name, "--workspace", workspace, "--any"],
    );
    assert!(ok, "token create failed: {stderr}");
    stdout.trim().to_string()
}

/// `POST /w/{ws}/seeded {"number_floor": floor}` straight at the hub:
/// ends seed mode so the hub numbers every create from here on (AGT-1391).
/// `409 already_seeded` is fine too — the seed already ended (the first
/// sync will do this itself once AGT-1396 lands).
fn end_seed(port: u16, hub_ws: &str, token: &str, floor: u64) {
    let auth = format!("Authorization: Bearer {token}");
    let resp = request_body(
        port,
        "POST",
        &format!("/w/{hub_ws}/seeded"),
        &[auth.as_str(), "Content-Type: application/json"],
        format!("{{\"number_floor\": {floor}}}").as_bytes(),
    );
    assert!(
        resp.status == 200 || (resp.status == 409 && resp.body.contains("already_seeded")),
        "POST /seeded: {} {}",
        resp.status,
        resp.body
    );
}

fn counts(v: &Value) -> (u64, u64, u64, u64) {
    (
        v["pushed"].as_u64().unwrap(),
        v["pulled"].as_u64().unwrap(),
        v["applied"].as_u64().unwrap(),
        v["skipped"].as_u64().unwrap(),
    )
}

/// AC3: two offline replicas each create 50 tickets (all `T-?`); after
/// both sync, all 100 carry unique hub numbers, the pending flags are
/// clear, `pm show T-N` resolves on both, and both are healthy.
#[test]
fn fifty_offline_tickets_on_each_of_two_replicas_all_get_unique_numbers() {
    const PER_REPLICA: usize = 50;
    let Some((_container, db)) =
        postgres_for("fifty_offline_tickets_on_each_of_two_replicas_all_get_unique_numbers")
    else {
        return;
    };
    let port = free_port();
    let mut hub = spawn_hub(&db, port);
    wait_for_health(&mut hub, port);
    let hub_url = format!("http://127.0.0.1:{port}");

    // Alice's machine: the workspace, seeded to the hub and past seeding.
    let mut alice = Replica::init("alice");
    let hub_ws = alice.workspace_id().to_ascii_lowercase();
    let alice_token = create_token(&db, "alice", &hub_ws);
    let bob_token = create_token(&db, "bob", &hub_ws);
    alice.configure(&hub_url, &alice_token);
    let first = alice.sync();
    assert_eq!(first["outbox"], 0, "{first}");
    end_seed(port, &hub_ws, &alice_token, 0);

    // Bob's machine: a copy of the workspace, in sync.
    let mut bob = Replica::clone_from(&alice, "bob");
    bob.configure(&hub_url, &bob_token);
    assert_eq!(counts(&bob.sync()), (0, 0, 0, 0));

    // Both lose the hub and keep filing.
    alice.configure(DEAD_HUB, &alice_token);
    bob.configure(DEAD_HUB, &bob_token);
    let mut ulids: Vec<(String, String)> = Vec::new(); // (actor, ulid)
    for i in 0..PER_REPLICA {
        ulids.push(("alice".into(), alice.new_pending(&format!("alice {i}"))));
        ulids.push(("bob".into(), bob.new_pending(&format!("bob {i}"))));
    }
    for r in [&alice, &bob] {
        let out = r.pm(&["sync"]);
        assert_eq!(out.status.code(), Some(1), "{}", err(&out));
        let state = r.sync_state();
        assert_eq!(
            state["pending_numbers"], PER_REPLICA,
            "{}: {state}",
            r.actor
        );
        assert_eq!(state["outbox"], PER_REPLICA, "{}: {state}", r.actor);
        let list = r.json_ok(&["list"]);
        assert_eq!(list.as_array().unwrap().len(), PER_REPLICA);
        assert!(
            list.as_array()
                .unwrap()
                .iter()
                .all(|t| t["id"] == "T-?" && t["number"].is_null()),
            "{list}"
        );
        let out = r.ok(&["list"]);
        assert_eq!(out_s(&out).matches("T-?").count(), PER_REPLICA);
    }

    // The hub is back. Alice: her 50 creates go up, the hub's 50 numbers
    // come down (and her own creates echo back, skipped).
    alice.configure(&hub_url, &alice_token);
    bob.configure(&hub_url, &bob_token);
    let a1 = alice.sync();
    assert_eq!(
        counts(&a1),
        (
            PER_REPLICA as u64,
            2 * PER_REPLICA as u64,
            PER_REPLICA as u64,
            PER_REPLICA as u64
        ),
        "{a1}"
    );
    assert_eq!(a1["pending_numbers"], 0, "{a1}");
    assert_eq!(a1["outbox"], 0, "{a1}");
    // Bob: his 50 go up; Alice's 50 creates + 50 numbers and his own 50
    // numbers come down.
    let b1 = bob.sync();
    assert_eq!(
        counts(&b1),
        (
            PER_REPLICA as u64,
            4 * PER_REPLICA as u64,
            3 * PER_REPLICA as u64,
            PER_REPLICA as u64
        ),
        "{b1}"
    );
    assert_eq!(b1["pending_numbers"], 0, "{b1}");
    // Alice takes Bob's 50 creates and their numbers.
    let a2 = alice.sync();
    assert_eq!(
        counts(&a2),
        (0, 2 * PER_REPLICA as u64, 2 * PER_REPLICA as u64, 0),
        "{a2}"
    );
    let out = alice.pm(&["sync"]);
    assert!(out.status.success(), "{}", err(&out));
    assert!(!out_s(&out).contains("awaiting"), "{}", out_s(&out));

    // Converged: 100 tickets, numbers exactly 1..=100, none pending.
    let total = 2 * PER_REPLICA;
    let a_list = alice.json_ok(&["list"]);
    let b_list = bob.json_ok(&["list"]);
    assert_eq!(a_list, b_list, "replicas differ");
    let tickets = a_list.as_array().unwrap();
    assert_eq!(tickets.len(), total);
    let numbers: BTreeSet<u64> = tickets
        .iter()
        .map(|t| t["number"].as_u64().unwrap())
        .collect();
    assert_eq!(numbers.len(), total, "numbers are unique");
    assert_eq!(numbers, (1..=total as u64).collect());
    assert!(tickets.iter().all(|t| t["id"] != "T-?"));
    // Alice pushed first, so her tickets took 1..=50 and Bob's 51..=100,
    // each in creation order.
    for (i, (actor, ulid)) in ulids.iter().enumerate() {
        let expected = if actor == "alice" {
            i / 2 + 1
        } else {
            PER_REPLICA + i / 2 + 1
        };
        for r in [&alice, &bob] {
            let by_ulid = r.show(ulid);
            assert_eq!(
                by_ulid["id"],
                format!("T-{expected}"),
                "{}: {by_ulid}",
                r.actor
            );
            assert_eq!(by_ulid["number"], expected);
            let by_number = r.show(&format!("T-{expected}"));
            assert_eq!(by_number, by_ulid);
            assert_eq!(by_number["title"], format!("{actor} {}", i / 2));
        }
    }
    // The number came from the hub, not from either replica.
    let log = alice.json_ok(&["log", "T-1"]);
    let numbered: Vec<&Value> = log
        .as_array()
        .unwrap()
        .iter()
        .filter(|op| op["summary"] == "numbered 1")
        .collect();
    assert_eq!(numbered.len(), 1, "{log}");
    assert_eq!(numbered[0]["actor"], "hub");
    for r in [&alice, &bob] {
        let state = r.sync_state();
        assert_eq!(state["pending_numbers"], 0, "{}: {state}", r.actor);
        assert_eq!(state["outbox"], 0, "{}: {state}", r.actor);
        let out = r.pm(&["doctor"]);
        assert!(out.status.success(), "{}: {}", r.actor, out_s(&out));
        let out = r.pm(&["show", "T-?"]);
        assert_eq!(out.status.code(), Some(2));
    }

    // And a ticket filed now, hub in reach, is still pending until it
    // syncs: `pm new` never talks to the hub itself.
    let late = alice.new_pending("late");
    assert_eq!(alice.sync_state()["pending_numbers"], 1);
    alice.sync();
    assert_eq!(alice.show(&late)["id"], format!("T-{}", total + 1));
    assert_eq!(alice.sync_state()["pending_numbers"], 0);
}
