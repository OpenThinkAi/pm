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
            .command(&["edit", id, "--view=editor"])
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
    let (ok, stdout, stderr) = admin(
        url,
        &["token", "create", name, "--workspace", workspace, "--any"],
    );
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
    // The first sync is the seed (AGT-1396); everything pushed comes
    // straight back on the pull and is skipped.
    assert_eq!(first["seed"]["pushed"], pushed, "{first}");
    assert_eq!(first["seed"]["resumed"], false);
    assert_eq!(first["seeded"], true);
    assert_eq!((pulled, applied, skipped), (pushed, 0, pushed), "{first}");
    assert_eq!(first["cursor"], first["head"]);
    assert_eq!(first["outbox"], 0);
    assert_eq!(first["workspace"], ws_id);
    assert_eq!(first["schema"], 1);
    let head_after_seed = first["head"].as_i64().unwrap();
    assert!(head_after_seed > 0);

    // A second sync with nothing new moves nothing, and does not seed.
    let again = alice.sync();
    assert_eq!(counts(&again), (0, 0, 0, 0), "{again}");
    assert_eq!(again["seed"], Value::Null);
    assert_eq!(again["cursor"].as_i64().unwrap(), head_after_seed);
    // The seed ended on the hub itself (AGT-1396): a direct end is the
    // 409 the helper accepts. From here the hub numbers tickets
    // (AGT-1398: with a hub configured, `pm new` files them as `T-?`).
    assert!(hub_seeded(port, &alice_token, &hub_ws));
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

// ======================================================= seeding (AGT-1396)

use std::collections::BTreeSet;

use pm_core::op::{
    BodyEdit, CommentAdd, FieldSet, LabelAdd, RelationAdd, StateTransition, TicketCreate,
};
use pm_core::{
    ActorId, Body, Clock, Op, Payload, Priority, Project, ProjectStatus, Relation, RelationKind,
};
use pm_store::Store;
use ulid::Ulid;

use hub::request;

impl Replica {
    /// `pm init --join <id>`: an empty replica of another workspace.
    fn join(id: &str, actor: &str) -> Self {
        let r = Replica::empty(actor);
        let out = r.pm(&["init", "--join", id]);
        assert!(out.status.success(), "init --join: {}", err(&out));
        r
    }

    /// `pm sync --json` with its exit code, stdout JSON (when any) and
    /// stderr, for the rounds whose failure or progress is the point.
    fn sync_raw(&self, extra: &[(&str, &str)]) -> (i32, Option<Value>, String) {
        let mut cmd = self.command(&["sync", "--json"]);
        for (k, v) in extra {
            cmd.env(k, v);
        }
        let out = cmd.output().unwrap();
        let json = serde_json::from_slice(&out.stdout).ok();
        (out.status.code().unwrap_or(-1), json, err(&out))
    }

    fn store(&self) -> Store {
        Store::open(self.ws.join("pm.sqlite")).unwrap()
    }

    /// Everything a reader can see, for comparing two replicas: every
    /// ticket (archived included) as `pm list --json` shows it, each
    /// project with its documents, and `pm doctor`'s table counts. The
    /// op log itself is compared separately, as a set of op ids.
    fn snapshot(&self) -> Value {
        let (code, list) = self.json(&["list", "--archived"]);
        assert_eq!(code, 0, "{list}");
        let (code, projects) = self.json(&["project", "list"]);
        assert_eq!(code, 0, "{projects}");
        let mut docs = serde_json::Map::new();
        for p in projects["projects"].as_array().unwrap() {
            let id = p["id"].as_str().unwrap();
            let (code, shown) = self.json(&["project", "show", id]);
            assert_eq!(code, 0, "{shown}");
            docs.insert(id.to_string(), shown);
        }
        let (code, doctor) = self.json(&["doctor"]);
        assert_eq!(code, 0, "doctor: {doctor}");
        assert_eq!(doctor["healthy"], true, "{doctor}");
        assert_eq!(doctor["drift"]["tables"], json!([]), "{doctor}");
        json!({
            "tickets": list,
            "projects": projects,
            "docs": docs,
            "tables": doctor["tables"],
            "op_count": doctor["op_count"],
        })
    }

    fn op_ids(&self) -> BTreeSet<Ulid> {
        self.store()
            .ops_since(0)
            .unwrap()
            .into_iter()
            .map(|(_, op)| op.op_id)
            .collect()
    }
}

/// Fills `r`'s workspace the way a long-lived one looks: an allocator
/// floor raised by `pm import vault`, `tickets` numbered tickets above it
/// with labels, comments, relations, transitions and descriptions, a
/// project with a design doc and a named document, one multi-megabyte
/// description (the Studio's log has a 23 MB `body.edit`), and one
/// ticket filed pending a hub number. Returns the ticket ids and the
/// pending one.
fn populate(r: &Replica, tickets: usize, floor: u64, big_bytes: usize) -> (Vec<Ulid>, Ulid) {
    let actor = ActorId::new("gen");
    let mut store = r.store();
    store.raise_number_floor(floor).unwrap();
    store
        .put_project(
            &Project {
                id: "big".into(),
                title: "Big project".into(),
                status: ProjectStatus::InProgress,
                parent: None,
                repos: ["OpenThinkAi/pm".to_string()].into_iter().collect(),
                doc: "# Big\n\nThe design doc.\n".into(),
                documents: [("notes".to_string(), "some notes\n".to_string())]
                    .into_iter()
                    .collect(),
            },
            &actor,
        )
        .unwrap();

    let mut clock = Clock::from_latest(store.latest_hlc().unwrap());
    let mut now = 1_790_000_000_000u64;
    let mut op = |entity: Ulid, payload: Payload| {
        now += 1;
        Op::new(Ulid::new(), clock.send(now), actor.clone(), entity, payload)
    };
    let mut ids: Vec<Ulid> = Vec::with_capacity(tickets);
    let mut batch: Vec<Op> = Vec::new();
    for i in 0..tickets {
        let id = Ulid::new();
        batch.push(op(
            id,
            Payload::TicketCreate(TicketCreate {
                title: format!("Ticket {i}"),
                state: "todo".into(),
                priority: Priority::default(),
                project: (i % 3 == 0).then(|| "big".to_string()),
                repo: (i % 4 == 0).then(|| "OpenThinkAi/pm".to_string()),
                source: None,
                ext: Default::default(),
            }),
        ));
        batch.push(op(
            id,
            Payload::FieldSet(FieldSet::Number(floor + 1 + i as u64)),
        ));
        batch.push(op(
            id,
            Payload::LabelAdd(LabelAdd {
                label: format!("l{}", i % 7),
            }),
        ));
        if i % 2 == 0 {
            batch.push(op(
                id,
                Payload::CommentAdd(CommentAdd {
                    body: format!("comment on ticket {i}"),
                }),
            ));
        }
        if i % 5 == 0 && i > 0 {
            batch.push(op(
                id,
                Payload::RelationAdd(RelationAdd {
                    relation: Relation {
                        kind: RelationKind::Blocks,
                        from: ids[i - 1],
                        to: id,
                    },
                }),
            ));
        }
        match i % 4 {
            1 => batch.push(op(
                id,
                Payload::StateTransition(StateTransition {
                    state: "in-progress".into(),
                }),
            )),
            2 => batch.push(op(
                id,
                Payload::StateTransition(StateTransition {
                    state: "done".into(),
                }),
            )),
            _ => {}
        }
        if i % 10 == 3 {
            let text = format!("## Problem\n\nTicket {i} needs doing.\n");
            let update = Body::new().diff_from_text(&text).unwrap();
            batch.push(op(
                id,
                Payload::BodyEdit(BodyEdit {
                    update: update.into_bytes(),
                }),
            ));
        }
        ids.push(id);
        if batch.len() >= 500 {
            store.commit_batch(&batch, &[]).unwrap();
            batch.clear();
        }
    }
    // One description of `big_bytes`: a single op that dwarfs the rest.
    let big = "lorem ipsum dolor ".repeat(big_bytes / 18 + 1);
    let update = Body::new().diff_from_text(&big).unwrap();
    batch.push(op(
        ids[0],
        Payload::BodyEdit(BodyEdit {
            update: update.into_bytes(),
        }),
    ));
    store.commit_batch(&batch, &[]).unwrap();

    // Filed after `pm hub login`, pending a hub number (AGT-1398's shape):
    // a create with no number op, flagged.
    let pending = Ulid::new();
    store
        .commit(&op(
            pending,
            Payload::TicketCreate(TicketCreate {
                title: "pending a hub number".into(),
                state: "todo".into(),
                priority: Priority::High,
                project: None,
                repo: None,
                source: None,
                ext: Default::default(),
            }),
        ))
        .unwrap();
    store.mark_pending_number(pending).unwrap();
    (ids, pending)
}

/// `GET /w/<ws>/whoami`'s `seeded`, straight from the hub.
fn hub_seeded(port: u16, token: &str, hub_ws: &str) -> bool {
    let resp = request(
        port,
        "GET",
        &format!("/w/{hub_ws}/whoami"),
        &[&format!("Authorization: Bearer {token}")],
    );
    assert_eq!(resp.status, 200, "{}", resp.body);
    serde_json::from_str::<Value>(&resp.body).unwrap()["seeded"]
        .as_bool()
        .unwrap()
}

/// The `kind` of each of the hub's first `limit` ops, in seq order.
fn hub_kinds(port: u16, token: &str, hub_ws: &str, limit: usize) -> Vec<String> {
    let resp = request(
        port,
        "GET",
        &format!("/w/{hub_ws}/ops?since=0&limit={limit}"),
        &[&format!("Authorization: Bearer {token}")],
    );
    assert_eq!(resp.status, 200, "{}", resp.body);
    serde_json::from_str::<Value>(&resp.body).unwrap()["ops"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["op"]["kind"].as_str().unwrap().to_string())
        .collect()
}

fn assert_same_workspace(a: &Replica, b: &Replica) {
    let (sa, sb) = (a.snapshot(), b.snapshot());
    assert_eq!(sa["op_count"], sb["op_count"], "op counts differ");
    assert_eq!(sa["tables"], sb["tables"], "table counts differ");
    assert_eq!(sa["projects"], sb["projects"], "projects differ");
    assert_eq!(sa["docs"], sb["docs"], "project documents differ");
    let (ta, tb) = (
        sa["tickets"].as_array().unwrap(),
        sb["tickets"].as_array().unwrap(),
    );
    assert_eq!(ta.len(), tb.len(), "ticket counts differ");
    for (x, y) in ta.iter().zip(tb) {
        assert_eq!(x, y, "ticket {} differs", x["id"]);
    }
    assert_eq!(a.op_ids(), b.op_ids(), "op logs differ");
}

/// AC1 + AC3 on a large, realistic workspace: the first sync seeds the
/// hub with the whole log (client numbers accepted, the floor set, a
/// pending ticket numbered by the hub at the end), reports progress, and
/// only then marks the workspace hub-authoritative; a fresh replica
/// joined by ULID pulls from 0 and comes out identical; a never-synced
/// copy of the log is refused as a second seed.
#[test]
fn first_sync_seeds_a_large_workspace_and_a_joined_replica_rebuilds_it() {
    let Some((_container, db)) =
        postgres_for("first_sync_seeds_a_large_workspace_and_a_joined_replica_rebuilds_it")
    else {
        return;
    };
    let port = free_port();
    let mut hub = spawn_hub(&db, port);
    wait_for_health(&mut hub, port);
    let hub_url = format!("http://127.0.0.1:{port}");

    const TICKETS: usize = 1500;
    const FLOOR: u64 = 1380;
    let mut source = Replica::init("studio");
    let (ids, pending) = populate(&source, TICKETS, FLOOR, 3 * 1024 * 1024);
    let ws_id = source.workspace_id();
    let hub_ws = ws_id.to_ascii_lowercase();
    let before = source.snapshot();
    let op_count = before["op_count"].as_u64().unwrap();
    assert!(op_count > 6000, "{op_count} ops is not a large workspace");
    assert_eq!(source.sync_state()["pending_numbers"], 1);
    assert_eq!(source.sync_state()["seeded"], false);
    assert_eq!(source.sync_state()["outbox"], op_count);

    // A copy of the log from before any sync: the "second seed" below.
    let mut copy = Replica::clone_from(&source, "copy");

    let studio_token = create_token(&db, "studio", &hub_ws);
    source.configure(&hub_url, &studio_token);
    assert!(!hub_seeded(port, &studio_token, &hub_ws));

    // The seed.
    let (code, json, stderr) = source.sync_raw(&[]);
    assert_eq!(code, 0, "{stderr}");
    let first = json.unwrap();
    assert_eq!(first["seed"]["resumed"], false, "{first}");
    assert_eq!(first["seed"]["pushed"], op_count, "{first}");
    // The floor the hub adopted is the largest seeded number, above the
    // local floor; the pending ticket got the next one.
    let hub_floor = FLOOR + TICKETS as u64;
    assert_eq!(first["seed"]["number_floor"], hub_floor, "{first}");
    assert_eq!(first["seed"]["numbered"], 1, "{first}");
    assert_eq!(first["seeded"], true);
    assert_eq!(first["pushed"], op_count);
    // The pull brings the whole log back — plus the hub's number op,
    // already applied from the seed-end answer — and skips it all.
    assert_eq!(first["pulled"], op_count + 1, "{first}");
    assert_eq!(first["applied"], 0);
    assert_eq!(first["skipped"], op_count + 1);
    assert_eq!(first["outbox"], 0);
    assert_eq!(first["pending_numbers"], 0);
    assert_eq!(first["cursor"], first["head"]);
    assert!(
        stderr.contains(&format!(
            "seed: seeding hub workspace {hub_ws} with {op_count} op(s)"
        )),
        "{stderr}"
    );
    let batches = stderr
        .lines()
        .filter(|l| l.starts_with("seed: ") && l.contains("op(s) pushed ("))
        .count();
    assert!(batches >= 7, "{batches} progress lines:\n{stderr}");
    assert!(
        stderr.contains(&format!(
            "seed: ended; number floor {hub_floor}; the hub numbered 1 ticket(s)"
        )),
        "{stderr}"
    );
    assert!(hub_seeded(port, &studio_token, &hub_ws));
    assert_eq!(source.store().number_floor().unwrap(), hub_floor);
    // The hub's log starts with every config op, then the rest in local
    // order (the order `pm doctor` replays in), so a replica applying it
    // page by page finds each state, project and document binding first.
    let config_kinds = [
        "workspace.set",
        "state.upsert",
        "actor.upsert",
        "project.create",
        "project.set",
        "project.delete",
        "project.doc_add",
    ];
    let config_total = source
        .store()
        .ops_since(0)
        .unwrap()
        .iter()
        .filter(|(_, op)| config_kinds.contains(&op.kind()))
        .count();
    assert!(config_total >= 10, "{config_total}");
    let kinds = hub_kinds(port, &studio_token, &hub_ws, 1000);
    assert_eq!(kinds.len(), 1000);
    assert!(
        kinds[..config_total]
            .iter()
            .all(|k| config_kinds.contains(&k.as_str())),
        "config ops lead the hub's log: {:?}",
        &kinds[..config_total.min(20)]
    );
    assert!(
        !kinds[config_total..]
            .iter()
            .any(|k| config_kinds.contains(&k.as_str())),
        "no config op after the first {config_total}"
    );
    let numbered = source.show(&pending.to_string());
    assert_eq!(numbered["number"], hub_floor + 1, "{numbered}");
    assert_eq!(numbered["id"], format!("T-{}", hub_floor + 1));
    assert_eq!(
        source.show(&format!("T-{}", FLOOR + 1))["ulid"],
        ids[0].to_string()
    );
    let state = source.sync_state();
    assert_eq!(state["seeded"], true, "{state}");
    assert_eq!(state["outbox"], 0);
    assert_eq!(state["pending_numbers"], 0);
    // Healthy, and nothing but the hub's one op changed.
    let after = source.snapshot();
    assert_eq!(after["op_count"], op_count + 1);
    assert_eq!(after["tables"]["ticket"], before["tables"]["ticket"]);
    assert_eq!(after["tables"]["comment"], before["tables"]["comment"]);

    // Later syncs do not seed again.
    let again = source.sync();
    assert_eq!(counts(&again), (0, 0, 0, 0), "{again}");
    assert_eq!(again["seed"], Value::Null);
    assert_eq!(again["seeded"], true);

    // A second machine: an empty replica of the workspace, pulling from 0.
    let mut joined = Replica::join(&ws_id, "laptop");
    let joined_token = create_token(&db, "laptop", &hub_ws);
    joined.configure(&hub_url, &joined_token);
    assert_eq!(joined.sync_state()["seeded"], false);
    let (code, json, stderr) = joined.sync_raw(&[]);
    assert_eq!(code, 0, "{stderr}");
    let pulled = json.unwrap();
    assert_eq!(pulled["seed"], Value::Null, "{pulled}");
    assert_eq!(pulled["seeded"], true);
    assert_eq!(pulled["pushed"], 0);
    assert_eq!(pulled["pulled"], op_count + 1);
    assert_eq!(pulled["applied"], op_count + 1);
    assert_eq!(pulled["skipped"], 0);
    assert_eq!(pulled["cursor"], pulled["head"]);
    assert!(!stderr.contains("seed:"), "{stderr}");
    assert_eq!(joined.sync_state()["seeded"], true);
    // Identical: config (prefix, states), every ticket, project and
    // document, the same op log, and `pm doctor` clean on both.
    assert_same_workspace(&source, &joined);
    assert_eq!(joined.show(&pending.to_string()), numbered);
    let big = joined.show(&ids[0].to_string());
    assert!(
        big["description"].as_str().unwrap().len() >= 3 * 1024 * 1024,
        "the big description did not survive the round trip"
    );
    assert_eq!(big, source.show(&ids[0].to_string()));
    let (code, v) = joined.json(&["hub", "status"]);
    assert_eq!(code, 0, "{v}");
    assert_eq!(v["workspace"]["id"], ws_id);
    assert_eq!(v["workspace"]["prefix"], "T");

    // The copy taken before the seed has never synced and holds a log:
    // syncing it into the seeded hub would be a second seed — refused,
    // and nothing about it changed.
    let copy_token = create_token(&db, "copy", &hub_ws);
    copy.configure(&hub_url, &copy_token);
    let copy_state = copy.sync_state();
    let (code, json, stderr) = copy.sync_raw(&[]);
    assert_eq!(code, 1, "{stderr}");
    assert!(json.is_none(), "no partial JSON on refusal");
    assert!(
        stderr.contains("already seeded")
            && stderr.contains("never synced")
            && stderr.contains("pm init --join"),
        "{stderr}"
    );
    assert_eq!(copy.sync_state(), copy_state);
    assert!(copy.pm(&["doctor"]).status.success());
}

/// AC2: a seed interrupted at the worst point — the hub committed a
/// batch and the process died before marking it — resumes on the next
/// sync from what the hub already holds, twice over, and a replica
/// joined afterwards still rebuilds the source exactly.
#[test]
fn an_interrupted_seed_resumes_from_where_it_stopped() {
    let Some((_container, db)) = postgres_for("an_interrupted_seed_resumes_from_where_it_stopped")
    else {
        return;
    };
    let port = free_port();
    let mut hub = spawn_hub(&db, port);
    wait_for_health(&mut hub, port);
    let hub_url = format!("http://127.0.0.1:{port}");

    let mut source = Replica::init("studio");
    let (_, pending) = populate(&source, 80, 200, 64 * 1024);
    let ws_id = source.workspace_id();
    let hub_ws = ws_id.to_ascii_lowercase();
    let op_count = source.sync_state()["outbox"].as_u64().unwrap();
    assert!(op_count > 250, "{op_count}");
    // The seed's first batch is the config pass (fewer than a batch).
    let config = source.store().outbox_config(1000).unwrap().len() as u64;
    assert!(config > 0 && config < 50, "{config}");
    let token = create_token(&db, "studio", &hub_ws);
    source.configure(&hub_url, &token);
    const BATCH: &str = "50";
    let small = [("PM_SYNC_TEST_BATCH_OPS", BATCH)];

    // Crash after the first batch (the config ops): the hub has them,
    // this replica has never marked anything — the seed looks fresh from
    // here except that the hub is not empty.
    let (code, json, stderr) =
        source.sync_raw(&[small[0], ("PM_SYNC_TEST_CRASH_AFTER_BATCHES", "1")]);
    assert_eq!(code, 1, "{stderr}");
    assert!(json.is_none());
    assert!(stderr.contains("exiting after batch 1"), "{stderr}");
    let state = source.sync_state();
    assert_eq!(state["pushed_through"], 0, "{state}");
    assert_eq!(state["outbox"], op_count);
    assert_eq!(state["seeded"], false);
    assert!(!hub_seeded(port, &token, &hub_ws));

    // Resume, and crash again after two more batches (the probe marked
    // the config ops as pushed; an empty config pass costs no request,
    // so batches 1 and 2 of the rest went up, 2 unmarked).
    let (code, _, stderr) = source.sync_raw(&[small[0], ("PM_SYNC_TEST_CRASH_AFTER_BATCHES", "2")]);
    assert_eq!(code, 1, "{stderr}");
    assert!(
        stderr.contains(&format!(
            "seed: resuming the seed of hub workspace {hub_ws}: {} of {op_count} op(s) still to push",
            op_count - config
        )),
        "{stderr}"
    );
    assert!(stderr.contains("exiting after batch 2"), "{stderr}");
    let state = source.sync_state();
    assert_eq!(state["outbox"], op_count - config - 50, "{state}");
    assert_eq!(state["seeded"], false);
    assert!(!hub_seeded(port, &token, &hub_ws));

    // The third run finishes: the probe finds everything pushed so far
    // on the hub (the unmarked batch included) and marks it, the rest
    // goes up, the seed ends.
    let on_hub = config + 100;
    let (code, json, stderr) = source.sync_raw(&small);
    assert_eq!(code, 0, "{stderr}");
    let done = json.unwrap();
    assert_eq!(done["seed"]["resumed"], true, "{done}");
    assert_eq!(done["seed"]["pushed"], op_count - on_hub, "{done}");
    assert_eq!(done["seed"]["number_floor"], 280);
    assert_eq!(done["seed"]["numbered"], 1);
    assert_eq!(done["seeded"], true);
    assert_eq!(done["pulled"], op_count + 1);
    assert_eq!(done["skipped"], op_count + 1);
    assert_eq!(done["outbox"], 0);
    assert_eq!(done["pending_numbers"], 0);
    assert!(
        stderr.contains(&format!(
            "seed: resuming the seed of hub workspace {hub_ws}: {} of {op_count} op(s) still to push",
            op_count - on_hub
        )),
        "{stderr}"
    );
    assert!(hub_seeded(port, &token, &hub_ws));
    assert_eq!(source.show(&pending.to_string())["number"], 281);
    assert_eq!(counts(&source.sync()), (0, 0, 0, 0));

    // A replica joined now sees exactly the source.
    let mut joined = Replica::join(&ws_id, "laptop");
    joined.configure(&hub_url, &create_token(&db, "laptop", &hub_ws));
    let got = joined.sync();
    assert_eq!(got["applied"], op_count + 1, "{got}");
    assert_same_workspace(&source, &joined);
}

/// The refusals (exit 1, a clear message, nothing changed locally): a
/// joined replica against a hub nobody has seeded; a hub still seeding
/// that holds another workspace's ops; a replica that is seeded locally
/// while the hub says it is not (a hub restored from before the seed).
#[test]
fn seed_refusals_exit_1_and_change_nothing() {
    let Some((_container, db)) = postgres_for("seed_refusals_exit_1_and_change_nothing") else {
        return;
    };
    let port = free_port();
    let mut hub = spawn_hub(&db, port);
    wait_for_health(&mut hub, port);
    let hub_url = format!("http://127.0.0.1:{port}");

    // Workspace X: seeds, but dies after the first batch.
    let mut x = Replica::init("x");
    populate(&x, 30, 0, 1024);
    let ws_id = x.workspace_id();
    let hub_ws = ws_id.to_ascii_lowercase();
    let x_token = create_token(&db, "x", &hub_ws);
    x.configure(&hub_url, &x_token);

    // Nothing on the hub yet: a joined replica has nothing to seed with.
    let mut joined = Replica::join(&ws_id, "joined");
    joined.configure(&hub_url, &create_token(&db, "joined", &hub_ws));
    let (code, json, stderr) = joined.sync_raw(&[]);
    assert_eq!(code, 1, "{stderr}");
    assert!(json.is_none());
    assert!(
        stderr.contains("has not been seeded yet") && stderr.contains("no log to seed it with"),
        "{stderr}"
    );
    assert_eq!(joined.sync_state()["seeded"], false);

    let (code, _, stderr) = x.sync_raw(&[
        ("PM_SYNC_TEST_BATCH_OPS", "20"),
        ("PM_SYNC_TEST_CRASH_AFTER_BATCHES", "1"),
    ]);
    assert_eq!(code, 1, "{stderr}");
    assert!(!hub_seeded(port, &x_token, &hub_ws));

    // Workspace Y claims the same id but grew its own log (a replica
    // joined by id, then configured locally instead of pulling). The hub
    // holds X's ops, none of which are Y's: refused, untouched.
    let mut y = Replica::join(&ws_id, "y");
    {
        let mut store = y.store();
        let ws = pm_core::Workspace {
            id: ws_id.parse().unwrap(),
            prefix: "Y".into(),
            states: vec![pm_core::State {
                name: "todo".into(),
                category: pm_core::StateCategory::Unstarted,
                position: 0,
            }],
            gate_labels: Default::default(),
            model_labels: Default::default(),
            template_sections: Vec::new(),
            stale_days: 30,
            docs_owned_by: Default::default(),
        };
        store.init_workspace(&ws, &ActorId::new("y")).unwrap();
    }
    y.configure(&hub_url, &create_token(&db, "y", &hub_ws));
    let y_state = y.sync_state();
    assert!(y_state["outbox"].as_u64().unwrap() > 0);
    let (code, json, stderr) = y.sync_raw(&[]);
    assert_eq!(code, 1, "{stderr}");
    assert!(json.is_none());
    assert!(
        stderr.contains("still seeding")
            && stderr.contains("seeded from another workspace")
            && stderr.contains(&hub_ws),
        "{stderr}"
    );
    assert_eq!(y.sync_state(), y_state);
    assert!(!hub_seeded(port, &x_token, &hub_ws));

    // X finishes its seed normally.
    let (code, json, stderr) = x.sync_raw(&[]);
    assert_eq!(code, 0, "{stderr}");
    assert_eq!(json.unwrap()["seed"]["resumed"], true);
    assert!(hub_seeded(port, &x_token, &hub_ws));
    assert_eq!(x.sync_state()["seeded"], true);

    // The hub loses the seed (restored from an older backup): X, seeded
    // locally, refuses to seed on top of it rather than doing so twice.
    hub::query_rows(
        &db,
        &format!("UPDATE workspaces SET seeded_at = NULL WHERE id = '{hub_ws}'"),
    )
    .unwrap();
    assert!(!hub_seeded(port, &x_token, &hub_ws));
    let x_state = x.sync_state();
    let (code, json, stderr) = x.sync_raw(&[]);
    assert_eq!(code, 1, "{stderr}");
    assert!(json.is_none());
    assert!(
        stderr.contains("in seed mode") && stderr.contains("restored from before the seed"),
        "{stderr}"
    );
    assert_eq!(x.sync_state(), x_state);
    assert!(x.pm(&["doctor"]).status.success());
}
