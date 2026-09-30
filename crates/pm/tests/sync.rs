//! `pm sync` (AGT-1395) and `pm claim` against a hub (AGT-1397) end to
//! end: two replicas of one workspace converging through the real `pm-hub`
//! on a throwaway Postgres (skipped, with a message, when neither docker
//! nor `PM_HUB_TEST_DATABASE_URL` is available), plus the failure paths
//! that need no hub at all.
//!
//! Nothing here touches the real `~/.config/pm/config.toml`, the login
//! keychain or a live workspace: every replica has its own temp HOME with
//! a hand-written config.toml, and the token travels in `PM_HUB_TOKEN`.
//! A second replica is a byte copy of the first workspace's database —
//! what a machine restored from a snapshot of the workspace looks like —
//! taken while no `pm` process has it open, so the WAL is checkpointed.

#[path = "../../pm-hub/tests/common/mod.rs"]
mod hub;

use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::thread::sleep;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tempfile::TempDir;

use hub::{admin, free_port, postgres_for, request_body, spawn_hub, wait_for_health};

/// One machine: its own HOME (config.toml), workspace directory and actor.
struct Replica {
    home: TempDir,
    ws: PathBuf,
    actor: String,
    token: Option<String>,
}

impl Replica {
    /// A fresh `pm init --prefix T` workspace.
    fn init(actor: &str) -> Self {
        let r = Replica::empty(actor);
        let out = r.pm(&["init", "--prefix", "T"]);
        assert!(out.status.success(), "init: {}", err(&out));
        r
    }

    fn empty(actor: &str) -> Self {
        let home = tempfile::tempdir().unwrap();
        let ws = home.path().join("ws");
        Replica {
            home,
            ws,
            actor: actor.to_string(),
            token: None,
        }
    }

    /// A second replica of `source`'s workspace: a copy of its database.
    fn clone_from(source: &Replica, actor: &str) -> Self {
        let r = Replica::empty(actor);
        std::fs::create_dir_all(&r.ws).unwrap();
        for name in ["pm.sqlite", "pm.sqlite-wal", "pm.sqlite-shm"] {
            let from = source.ws.join(name);
            if from.is_file() {
                std::fs::copy(&from, r.ws.join(name)).unwrap();
            }
        }
        r
    }

    /// `hub = "<url>"` in this replica's config.toml, written directly (no
    /// `pm hub login`, which on macOS would also write a keychain item).
    fn configure(&mut self, hub_url: &str, token: &str) {
        let path = self.home.path().join(".config/pm/config.toml");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, format!("hub = \"{hub_url}\"\n")).unwrap();
        self.token = Some(token.to_string());
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

    /// `pm sync --json`, asserting exit 0.
    fn sync(&self) -> Value {
        let out = self.pm(&["sync", "--json"]);
        assert!(out.status.success(), "sync: {}", err(&out));
        serde_json::from_slice(&out.stdout).unwrap()
    }

    fn show(&self, id: &str) -> Value {
        let (code, v) = self.json(&["show", id]);
        assert_eq!(code, 0, "{v}");
        v
    }

    /// `pm doctor --json`'s `sync` block: outbox, pushed_through, cursor,
    /// pending_numbers.
    fn sync_state(&self) -> Value {
        let (code, v) = self.json(&["doctor"]);
        assert_eq!(code, 0, "doctor: {v}");
        v["sync"].clone()
    }

    fn workspace_id(&self) -> String {
        let (code, v) = self.json(&["hub", "status"]);
        assert_eq!(code, 0, "{v}");
        v["workspace"]["id"].as_str().unwrap().to_string()
    }

    /// An `EDITOR` that appends `line` to whatever file it is given.
    fn appending_editor(&self, line: &str) -> PathBuf {
        let path = self.home.path().join("editor.sh");
        std::fs::write(
            &path,
            format!("#!/bin/sh\nprintf '%s\\n' '{line}' >> \"$1\"\n"),
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        path
    }

    fn edit_description(&self, id: &str, line: &str) {
        let editor = self.appending_editor(line);
        let out = self
            .command(&["edit", id])
            .env("EDITOR", &editor)
            .output()
            .unwrap();
        assert!(out.status.success(), "edit: {}", err(&out));
    }

    /// `pm log <id> --json`: every op on the ticket, in log order.
    fn log(&self, id: &str) -> Vec<Value> {
        let (code, v) = self.json(&["log", id]);
        assert_eq!(code, 0, "log: {v}");
        v.as_array().unwrap().clone()
    }

    /// The stamp of `actor`'s admitted `claim` on `id` in this log — what
    /// the hub reports as `at` to a claim it refuses on that ticket.
    fn claim_hlc(&self, id: &str, actor: &str) -> Value {
        self.log(id)
            .into_iter()
            .find(|op| op["kind"] == "claim" && op["actor"] == actor)
            .unwrap_or_else(|| panic!("no claim by {actor} on {id}"))["hlc"]
            .clone()
    }

    /// Moves this replica's pull cursor `by` hub seqs behind the hub's
    /// back. Pushed past the hub's head, the next pull asks from beyond
    /// everything and brings nothing: a replica whose view stays stale
    /// even though it syncs. Run only while no `pm` process has the
    /// database open.
    fn shift_cursor(&self, by: i64) {
        rusqlite::Connection::open(self.ws.join("pm.sqlite"))
            .unwrap()
            .execute("UPDATE sync_state SET pulled_seq = pulled_seq + ?1", [by])
            .unwrap();
    }
}

fn out_s(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}
fn err(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

fn create_token(url: &str, name: &str, workspace: &str) -> String {
    let (ok, stdout, stderr) = admin(url, &["token", "create", name, "--workspace", workspace]);
    assert!(ok, "token create failed: {stderr}");
    stdout.trim().to_string()
}

/// `POST /w/{ws}/seeded {"number_floor": 0}` straight at the hub: ends
/// seed mode, so the hub numbers every `ticket.create` it is pushed from
/// here on (AGT-1391) — which, with a hub configured, `pm new` no longer
/// does locally (AGT-1398). `409 already_seeded` is fine too: the seed
/// already ended (the first sync does this itself once AGT-1396 lands).
fn end_seed(port: u16, hub_ws: &str, token: &str) {
    let auth = format!("Authorization: Bearer {token}");
    let resp = request_body(
        port,
        "POST",
        &format!("/w/{hub_ws}/seeded"),
        &[auth.as_str(), "Content-Type: application/json"],
        b"{\"number_floor\": 0}",
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

/// AC1 + AC3: two replicas, concurrent edits to one ticket's title,
/// labels and description, converging through the hub; a sync with
/// nothing new moves nothing; a rejected token changes nothing locally;
/// `--watch` picks up the other replica's changes.
#[test]
fn two_replicas_converge_through_a_hub() {
    let Some((_container, db)) = postgres_for("two_replicas_converge_through_a_hub") else {
        return;
    };
    let port = free_port();
    let mut hub = spawn_hub(&db, port);
    wait_for_health(&mut hub, port);
    let hub_url = format!("http://127.0.0.1:{port}");

    // Alice's machine: the workspace, seeded to the hub.
    let mut alice = Replica::init("alice");
    let ws_id = alice.workspace_id();
    // The hub keys the workspace by the ULID's lowercase form.
    let hub_ws = ws_id.to_ascii_lowercase();
    let alice_token = create_token(&db, "alice", &hub_ws);
    let bob_token = create_token(&db, "bob", &hub_ws);
    alice.configure(&hub_url, &alice_token);

    let first = alice.sync();
    let (pushed, pulled, applied, skipped) = counts(&first);
    assert!(
        pushed > 0,
        "the init config ops are the first outbox: {first}"
    );
    // Everything pushed comes straight back on the pull and is skipped.
    assert_eq!((pulled, applied, skipped), (pushed, 0, pushed), "{first}");
    assert_eq!(first["cursor"], first["head"]);
    assert_eq!(first["outbox"], 0);
    assert_eq!(first["workspace"], ws_id);
    assert_eq!(first["schema"], 1);
    let head_after_seed = first["head"].as_i64().unwrap();
    assert!(head_after_seed > 0);

    // A second sync with nothing new moves nothing.
    let again = alice.sync();
    assert_eq!(counts(&again), (0, 0, 0, 0), "{again}");
    assert_eq!(again["cursor"].as_i64().unwrap(), head_after_seed);
    // The seed is in; from here the hub numbers tickets (AGT-1398: with a
    // hub configured, `pm new` files them as `T-?` and waits for it).
    end_seed(port, &hub_ws, &alice_token);

    // Bob's machine: a copy of the workspace as of now, synced.
    let mut bob = Replica::clone_from(&alice, "bob");
    bob.configure(&hub_url, &bob_token);
    assert_eq!(counts(&bob.sync()), (0, 0, 0, 0));

    // Alice files a ticket (`T-?` until the hub numbers it); the sync
    // pushes it and pulls the hub's `field.set number` back — the one
    // foreign op in an otherwise all-echo pull. Bob pulls all of it.
    let out = alice.ok(&["new", "--title", "shared", "--description", "line one"]);
    assert!(out_s(&out).starts_with("T-?  "), "{}", out_s(&out));
    assert_eq!(alice.sync_state()["pending_numbers"], 1);
    let created = alice.sync();
    let (pushed, pulled, applied, skipped) = counts(&created);
    assert!(pushed >= 2, "{created}");
    assert_eq!(
        (pulled, applied, skipped),
        (pushed + 1, 1, pushed),
        "{created}"
    );
    assert_eq!(created["pending_numbers"], 0, "{created}");
    let got = bob.sync();
    let (pushed, pulled, applied, skipped) = counts(&got);
    assert_eq!((pushed, applied, skipped), (0, pulled, 0), "{got}");
    assert_eq!(
        pulled,
        counts(&created).0 + 1,
        "Alice's ops plus the hub's number"
    );
    assert_eq!(bob.show("T-1"), alice.show("T-1"));
    assert_eq!(bob.show("T-1")["title"], "shared");

    // Concurrent edits on both sides, before either syncs.
    alice.ok(&["set", "T-1", "title=from alice"]);
    alice.ok(&["label", "T-1", "+alice"]);
    alice.edit_description("T-1", "alice line");
    bob.ok(&["set", "T-1", "title=from bob"]);
    bob.ok(&["label", "T-1", "+bob"]);
    bob.edit_description("T-1", "bob line");
    assert_ne!(alice.show("T-1"), bob.show("T-1"));

    // Alice pushes; Bob pushes and takes Alice's; Alice takes Bob's.
    let a1 = alice.sync();
    assert!(counts(&a1).0 >= 3, "{a1}");
    assert_eq!(counts(&a1).2, 0);
    let b1 = bob.sync();
    assert!(counts(&b1).0 >= 3, "{b1}");
    assert_eq!(
        counts(&b1).2,
        counts(&a1).0,
        "Bob applies exactly Alice's ops: {b1}"
    );
    assert_eq!(counts(&b1).3, counts(&b1).0, "and skips his own: {b1}");
    let a2 = alice.sync();
    assert_eq!(counts(&a2).0, 0);
    assert_eq!(
        counts(&a2).2,
        counts(&b1).0,
        "Alice applies exactly Bob's ops: {a2}"
    );

    // Converged: identical tickets, both histories present, healthy.
    let a = alice.show("T-1");
    let b = bob.show("T-1");
    assert_eq!(a, b, "replicas differ:\n{a}\n{b}");
    assert!(
        a["title"] == "from alice" || a["title"] == "from bob",
        "{}",
        a["title"]
    );
    let labels: Vec<&str> = a["labels"]
        .as_array()
        .unwrap()
        .iter()
        .map(|l| l.as_str().unwrap())
        .collect();
    assert_eq!(labels, ["alice", "bob"]);
    let description = a["description"].as_str().unwrap();
    assert!(
        description.contains("line one")
            && description.contains("alice line")
            && description.contains("bob line"),
        "{description:?}"
    );
    for r in [&alice, &bob] {
        let done = r.sync();
        assert_eq!(counts(&done), (0, 0, 0, 0), "{done}");
        assert_eq!(done["cursor"], done["head"]);
        let state = r.sync_state();
        assert_eq!(state["outbox"], 0, "{state}");
        assert_eq!(state["cursor"], done["head"]);
        let out = r.pm(&["doctor"]);
        assert!(out.status.success(), "{}: {}", r.actor, out_s(&out));
    }
    assert_eq!(alice.sync_state(), bob.sync_state());

    // A rejected token (the hub's bare 404) changes nothing locally.
    alice.ok(&["comment", "T-1", "a comment"]);
    let before = alice.sync_state();
    assert_eq!(before["outbox"], 1, "{before}");
    let real = alice.token.replace("pmh_wrong-token".to_string()).unwrap();
    let out = alice.pm(&["sync"]);
    assert_eq!(out.status.code(), Some(1), "{}", err(&out));
    assert!(
        err(&out).contains("rejected the token") && err(&out).contains("pm hub status"),
        "{}",
        err(&out)
    );
    assert_eq!(alice.sync_state(), before);
    alice.token = Some(real);
    assert_eq!(counts(&alice.sync()).0, 1);

    // `--watch`: Bob's watcher picks up what Alice syncs while it runs.
    let mut watcher = bob
        .command(&["sync", "--watch", "1", "--json"])
        .spawn()
        .unwrap();
    alice.ok(&["label", "T-1", "+watched"]);
    alice.sync();
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let labels = bob.show("T-1")["labels"].clone();
        if labels.as_array().unwrap().iter().any(|l| l == "watched") {
            break;
        }
        assert!(
            watcher.try_wait().unwrap().is_none(),
            "the watcher exited on its own"
        );
        assert!(
            Instant::now() < deadline,
            "Bob's watcher never pulled the label"
        );
        sleep(Duration::from_millis(200));
    }
    assert!(watcher.try_wait().unwrap().is_none());
    watcher.kill().unwrap();
    let out = watcher.wait_with_output().unwrap();
    let rounds: Vec<Value> = out_s(&out)
        .lines()
        .map(|l| serde_json::from_str(l).unwrap_or_else(|e| panic!("{l:?}: {e}")))
        .collect();
    assert!(!rounds.is_empty(), "no rounds reported: {}", err(&out));
    assert!(
        rounds.iter().any(|r| r["applied"].as_u64().unwrap() > 0),
        "{rounds:?}"
    );
    // Killed mid-loop, the replica is still consistent.
    assert!(bob.pm(&["doctor"]).status.success());
    assert_eq!(alice.show("T-1"), bob.show("T-1"));
}

/// AC2: the hub cannot be reached — exit 1, a clear message, and the
/// outbox and cursor exactly as they were.
#[test]
fn unreachable_hub_exits_1_and_leaves_local_state_untouched() {
    let mut r = Replica::init("alice");
    // Port 1: nothing listens, the connection is refused at once.
    r.configure("http://127.0.0.1:1", "pmh_some-token");
    r.ok(&["new", "--title", "offline"]);
    let before = r.sync_state();
    assert!(before["outbox"].as_u64().unwrap() > 0, "{before}");
    assert_eq!(before["cursor"], 0);

    let out = r.pm(&["sync"]);
    assert_eq!(out.status.code(), Some(1), "{}", err(&out));
    assert!(
        err(&out).contains("cannot reach the hub at http://127.0.0.1:1"),
        "{}",
        err(&out)
    );
    assert!(out_s(&out).is_empty());
    let out = r.pm(&["sync", "--json"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(out_s(&out).is_empty(), "no partial JSON on failure");

    assert_eq!(r.sync_state(), before);
    assert!(r.pm(&["doctor"]).status.success());
}

#[test]
fn no_hub_configured_is_an_error_that_names_login() {
    let r = Replica::init("alice");
    let out = r.pm(&["sync"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(err(&out).contains("pm hub login"), "{}", err(&out));
}

/// `--watch` keeps looping through failed rounds (the hub is down) until
/// it is interrupted, and `--watch 0` is a usage error.
#[test]
fn watch_keeps_looping_through_failures() {
    let mut r = Replica::init("alice");
    r.configure("http://127.0.0.1:1", "pmh_some-token");
    let out = r.pm(&["sync", "--watch", "0"]);
    assert_eq!(out.status.code(), Some(2), "{}", err(&out));

    let mut watcher = r.command(&["sync", "--watch", "1"]).spawn().unwrap();
    sleep(Duration::from_millis(2500));
    assert!(
        watcher.try_wait().unwrap().is_none(),
        "the watcher gave up after a failed round"
    );
    watcher.kill().unwrap();
    let out = watcher.wait_with_output().unwrap();
    let failures = err(&out)
        .lines()
        .filter(|l| l.contains("sync failed") && l.contains("cannot reach the hub"))
        .count();
    assert!(failures >= 2, "{}", err(&out));
    assert!(r.pm(&["doctor"]).status.success());
}

// ------------------------------------------------ pm claim via the hub (AGT-1397)

/// AC1 + AC3: once the workspace is seeded the hub arbitrates claims. Two
/// replicas racing for one ticket get exactly one `0` and one `75` naming
/// the winner (with `at` the admitted claim's stamp), the loser logs
/// nothing, and they agree once synced. A replica whose view is stale
/// gets the same `75` for a ticket it still sees as free, and `--ready`
/// skips such a candidate for the next one. Before the seed ends, a
/// configured hub leaves the decision to the local database and the claim
/// goes up with the seed. (The unreachable-hub path: `tests/claim.rs`.)
#[test]
fn claims_are_arbitrated_by_the_hub_once_seeded() {
    let Some((_container, db)) = postgres_for("claims_are_arbitrated_by_the_hub_once_seeded")
    else {
        return;
    };
    let port = free_port();
    let mut hub = spawn_hub(&db, port);
    wait_for_health(&mut hub, port);
    let hub_url = format!("http://127.0.0.1:{port}");

    let mut alice = Replica::init("alice");
    let hub_ws = alice.workspace_id().to_ascii_lowercase();
    let alice_token = create_token(&db, "alice", &hub_ws);
    let bob_token = create_token(&db, "bob", &hub_ws);
    // Numbered locally: filed before the hub is configured.
    for title in ["one", "two", "three", "four"] {
        alice.ok(&["new", "--title", title]);
    }
    alice.configure(&hub_url, &alice_token);

    // Seed mode: the hub is configured but not yet the authority, so the
    // claim is decided here and waits in the outbox with the rest of the
    // seed; nothing has gone to the hub's log yet.
    alice.ok(&["claim", "T-1", "--branch", "seed/one"]);
    let one = alice.show("T-1");
    assert_eq!(
        (&one["assignee"], &one["ext"]["branch"]),
        (&json!("alice"), &json!("seed/one"))
    );
    let state = alice.sync_state();
    assert!(state["outbox"].as_u64().unwrap() > 0, "{state}");
    assert_eq!(state["cursor"], 0, "{state}");

    alice.sync();
    end_seed(port, &hub_ws, &alice_token);
    let mut bob = Replica::clone_from(&alice, "bob");
    bob.configure(&hub_url, &bob_token);

    // AC3: both claim T-2 at once.
    let a = alice
        .command(&["claim", "T-2", "--branch", "race/alice", "--json"])
        .spawn()
        .unwrap();
    let b = bob
        .command(&["claim", "T-2", "--branch", "race/bob", "--json"])
        .spawn()
        .unwrap();
    let a = a.wait_with_output().unwrap();
    let b = b.wait_with_output().unwrap();
    let mut codes = [a.status.code().unwrap(), b.status.code().unwrap()];
    codes.sort_unstable();
    assert_eq!(
        codes,
        [0, 75],
        "alice: {} {}\nbob: {} {}",
        out_s(&a),
        err(&a),
        out_s(&b),
        err(&b)
    );
    let (winner, won, loser, lost) = if a.status.success() {
        (&alice, &a, &bob, &b)
    } else {
        (&bob, &b, &alice, &a)
    };
    let ticket: Value = serde_json::from_slice(&won.stdout).unwrap();
    assert_eq!(ticket["id"], "T-2");
    assert_eq!(ticket["assignee"], winner.actor);
    assert_eq!(ticket["ext"]["branch"], format!("race/{}", winner.actor));
    let taken: Value = serde_json::from_slice(&lost.stdout).unwrap();
    assert_eq!(taken["schema"], 1);
    assert_eq!(taken["id"], "T-2");
    assert_eq!(taken["ulid"], ticket["ulid"]);
    assert_eq!(taken["taken_by"], winner.actor, "{taken}");
    assert_eq!(taken["state"], ticket["state"]);
    assert_eq!(
        taken["at"],
        winner.claim_hlc("T-2", &winner.actor),
        "at is the admitted claim's stamp"
    );
    assert!(
        taken["reason"].as_str().unwrap().contains("not unstarted"),
        "{taken}"
    );
    assert!(
        err(lost).contains(&format!("taken by {}", winner.actor)),
        "{}",
        err(lost)
    );
    // The loser logged nothing of its own on the ticket.
    let loser_ops = loser.log("T-2");
    assert!(
        !loser_ops
            .iter()
            .any(|op| op["kind"] == "claim" && op["actor"] == loser.actor),
        "{loser_ops:?}"
    );
    assert_ne!(
        loser.show("T-2")["ext"]["branch"],
        format!("race/{}", loser.actor)
    );

    // Synced, both agree on the winner; the seed's claim stands.
    alice.sync();
    bob.sync();
    alice.sync();
    assert_eq!(alice.show("T-2"), bob.show("T-2"));
    assert_eq!(bob.show("T-2")["assignee"], winner.actor);
    assert_eq!(bob.show("T-1")["assignee"], "alice");

    // A stale replica: Alice claims T-3 through the hub, and Bob's cursor
    // is moved past the hub's head behind his back, so his syncs pull
    // nothing and T-3 still reads free to him. The hub says otherwise.
    alice.ok(&["claim", "T-3"]);
    bob.shift_cursor(1_000_000);
    let (code, taken) = bob.json(&["claim", "T-3"]);
    assert_eq!(code, 75, "{taken}");
    assert_eq!(taken["taken_by"], "alice");
    assert_eq!(taken["at"], alice.claim_hlc("T-3", "alice"));
    assert_eq!(bob.show("T-3")["assignee"], Value::Null, "nothing written");
    // `--ready`: T-3 is Bob's lowest-numbered ready ticket; the hub's 75
    // skips it and T-4 is admitted.
    let (code, ticket) = bob.json(&["claim", "--ready"]);
    assert_eq!(code, 0, "{ticket}");
    assert_eq!(ticket["id"], "T-4");
    assert_eq!(ticket["assignee"], "bob");
    bob.shift_cursor(-1_000_000);

    bob.sync();
    alice.sync();
    for id in ["T-1", "T-2", "T-3", "T-4"] {
        assert_eq!(alice.show(id), bob.show(id), "{id}");
    }
    assert_eq!(alice.show("T-3")["assignee"], "alice");
    assert_eq!(alice.show("T-4")["assignee"], "bob");
    for r in [&alice, &bob] {
        let state = r.sync_state();
        assert_eq!(state["outbox"], 0, "{}: {state}", r.actor);
        assert!(r.pm(&["doctor"]).status.success(), "{}", r.actor);
    }
}

/// A claim the local database admitted while it was the authority (Bob
/// claimed with no hub configured, after Alice's seed) that the hub
/// refuses when it finally goes up: `pm sync` reads the `seq: null` ack
/// (AGT-1392's shape, which AGT-1395's parser could not), marks the claim
/// pushed, reports it, and logs the compensating writes in the same round,
/// so both replicas converge on the hub's answer.
#[test]
fn sync_reconciles_a_claim_the_hub_refuses() {
    let Some((_container, db)) = postgres_for("sync_reconciles_a_claim_the_hub_refuses") else {
        return;
    };
    let port = free_port();
    let mut hub = spawn_hub(&db, port);
    wait_for_health(&mut hub, port);
    let hub_url = format!("http://127.0.0.1:{port}");

    let mut alice = Replica::init("alice");
    let hub_ws = alice.workspace_id().to_ascii_lowercase();
    let alice_token = create_token(&db, "alice", &hub_ws);
    let bob_token = create_token(&db, "bob", &hub_ws);
    alice.ok(&["new", "--title", "contested"]);
    alice.configure(&hub_url, &alice_token);
    alice.sync();
    end_seed(port, &hub_ws, &alice_token);

    // Bob's copy was taken after the seed but has no hub configured: his
    // own database admits his claim, which waits in his outbox.
    let mut bob = Replica::clone_from(&alice, "bob");
    bob.ok(&["claim", "T-1"]);
    assert_eq!(bob.show("T-1")["assignee"], "bob");
    let claim_op = bob
        .log("T-1")
        .into_iter()
        .find(|op| op["kind"] == "claim")
        .unwrap();
    assert_eq!(bob.sync_state()["outbox"], 1);
    // Meanwhile the hub admits Alice's.
    alice.ok(&["claim", "T-1"]);

    // Bob joins: his claim goes up alone and the hub refuses it.
    bob.configure(&hub_url, &bob_token);
    let out = bob.pm(&["sync", "--json"]);
    assert!(out.status.success(), "{}", err(&out));
    let round: Value = serde_json::from_slice(&out.stdout).unwrap();
    let rejected = round["rejected"].as_array().unwrap();
    assert_eq!(rejected.len(), 1, "{round}");
    assert_eq!(rejected[0]["op_id"], claim_op["op_id"]);
    assert_eq!(rejected[0]["ticket"], bob.show("T-1")["ulid"]);
    assert_eq!(rejected[0]["taken_by"], "alice");
    assert_eq!(rejected[0]["code"], "not_unstarted");
    assert_eq!(rejected[0]["state"], alice.show("T-1")["state"]);
    assert_eq!(rejected[0]["at"], alice.claim_hlc("T-1", "alice"));
    assert!(
        err(&out).contains("refused claim") && err(&out).contains("taken by alice"),
        "{}",
        err(&out)
    );
    // The claim was acknowledged, and the two compensating writes it
    // triggered went up in the same round.
    assert_eq!(round["pushed"], 3, "{round}");
    assert_eq!(round["outbox"], 0, "{round}");
    assert_eq!(bob.show("T-1")["assignee"], "alice", "reconciled");

    // Alice pulls the compensation; the replicas agree, and a further
    // round moves nothing and refuses nothing.
    let a = alice.sync();
    assert_eq!(counts(&a).2, 2, "{a}");
    assert_eq!(alice.show("T-1"), bob.show("T-1"));
    assert_eq!(alice.show("T-1")["assignee"], "alice");
    let again = bob.sync();
    assert_eq!(counts(&again), (0, 0, 0, 0), "{again}");
    assert_eq!(again["rejected"], json!([]));
    for r in [&alice, &bob] {
        assert!(r.pm(&["doctor"]).status.success(), "{}", r.actor);
    }
}
