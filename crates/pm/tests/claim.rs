//! `pm claim` through the built binary (AGT-1341). Sandbox as in
//! `cli.rs`: temp HOME, cleared env, stdin closed. The race test spawns
//! twenty real `pm` processes against one workspace.

use std::path::PathBuf;
use std::process::{Command, Output, Stdio};

use pm_core::op::StateTransition;
use pm_core::{ActorId, Clock, Op, Payload, Project, ProjectStatus};
use pm_store::Store;
use serde_json::Value;
use tempfile::TempDir;
use ulid::Ulid;

struct Sandbox {
    home: TempDir,
    ws: PathBuf,
}

impl Sandbox {
    /// An initialized AGT workspace (the config default) with `pm` and
    /// `other` projects.
    fn new() -> Self {
        let home = tempfile::tempdir().unwrap();
        let ws = home.path().join("ws");
        let sb = Sandbox { home, ws };
        assert_code(
            &sb.pm(&[
                "init",
                "--prefix",
                "AGT",
                "--preset",
                "saltline",
                "--workspace",
                sb.ws_str(),
            ]),
            0,
        );
        for id in ["pm", "other"] {
            sb.store()
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
        sb
    }

    fn ws_str(&self) -> &str {
        self.ws.to_str().unwrap()
    }

    fn command(&self, args: &[&str], extra: &[(&str, &str)]) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_pm"));
        cmd.args(args)
            .env_clear()
            .env("HOME", self.home.path())
            .env("USER", "tester")
            .stdin(Stdio::null());
        for (k, v) in extra {
            cmd.env(k, v);
        }
        cmd
    }

    fn run(&self, args: &[&str], extra: &[(&str, &str)]) -> Output {
        self.command(args, extra).output().unwrap()
    }

    fn pm(&self, args: &[&str]) -> Output {
        self.run(args, &[])
    }

    fn store(&self) -> Store {
        Store::open(self.ws.join("pm.sqlite")).unwrap()
    }

    /// `pm new` with the given flags; returns the display id (`AGT-n`).
    fn new_ticket(&self, flags: &[&str]) -> String {
        let mut args = vec!["new", "--title", "t"];
        args.extend_from_slice(flags);
        let out = self.pm(&args);
        assert_code(&out, 0);
        stdout(&out).trim().to_string()
    }

    fn ulid_of(&self, id: &str) -> Ulid {
        let v = json(&self.pm(&["show", id, "--json"]));
        v["ulid"].as_str().unwrap().parse().unwrap()
    }

    /// Moves a ticket to `state` behind the CLI's back (no `pm move` yet).
    fn transition(&self, id: &str, state: &str) {
        let ulid = self.ulid_of(id);
        let mut store = self.store();
        let hlc = Clock::from_latest(store.latest_hlc().unwrap()).send(1);
        store
            .commit(&Op::new(
                Ulid::new(),
                hlc,
                ActorId::new("tester"),
                ulid,
                Payload::StateTransition(StateTransition {
                    state: state.into(),
                }),
            ))
            .unwrap();
    }

    fn op_count(&self) -> i64 {
        rusqlite::Connection::open(self.ws.join("pm.sqlite"))
            .unwrap()
            .query_row("SELECT COUNT(*) FROM ops", [], |r| r.get(0))
            .unwrap()
    }
}

fn stdout(out: &Output) -> String {
    String::from_utf8(out.stdout.clone()).unwrap()
}

fn stderr(out: &Output) -> String {
    String::from_utf8(out.stderr.clone()).unwrap()
}

fn assert_code(out: &Output, code: i32) {
    assert_eq!(
        out.status.code(),
        Some(code),
        "stdout: {}\nstderr: {}",
        stdout(out),
        stderr(out)
    );
}

fn json(out: &Output) -> Value {
    assert_code(out, 0);
    serde_json::from_slice(&out.stdout).unwrap()
}

// ---------------------------------------------------------------- AC1

#[test]
fn claim_moves_the_ticket_to_started_assigns_the_actor_and_records_the_branch() {
    let sb = Sandbox::new();
    let id = sb.new_ticket(&["--project", "pm"]);
    let before = sb.op_count();

    let out = sb.run(
        &["claim", &id, "--branch", "pm-claim"],
        &[("PM_ACTOR", "claude:pm-build")],
    );
    assert_code(&out, 0);
    assert_eq!(stdout(&out), format!("{id}\n"));

    let v = json(&sb.pm(&["show", &id, "--json"]));
    assert_eq!(v["state"], "in-progress");
    assert_eq!(v["assignee"], "claude:pm-build");
    assert_eq!(v["ext"]["branch"], "pm-claim");

    // One claim op plus the branch ext write, both by the claiming actor.
    let ops = sb.store().ops(sb.ulid_of(&id)).unwrap();
    let tail: Vec<&Op> = ops.iter().skip(ops.len() - 2).collect();
    assert_eq!(sb.op_count(), before + 2);
    assert!(
        matches!(&tail[0].payload, Payload::Claim(c) if c.state == "in-progress"
        && c.assignee.as_str() == "claude:pm-build")
    );
    assert!(matches!(&tail[1].payload, Payload::FieldSet(_)));
    assert!(tail.iter().all(|op| op.actor.as_str() == "claude:pm-build"));
}

#[test]
fn claim_json_prints_the_ticket_and_needs_no_branch() {
    let sb = Sandbox::new();
    let id = sb.new_ticket(&[]);
    let v = json(&sb.pm(&["claim", &id, "--json"]));
    assert_eq!(v["schema"], 1);
    assert_eq!(v["id"], id);
    assert_eq!(v["state"], "in-progress");
    assert_eq!(v["assignee"], "tester");
    assert!(v["ext"].get("branch").is_none(), "{v}");
}

// ---------------------------------------------------------------- AC2

#[test]
fn a_second_claim_exits_75_with_taken_by_and_at_and_writes_nothing() {
    let sb = Sandbox::new();
    let id = sb.new_ticket(&[]);
    assert_code(&sb.run(&["claim", &id], &[("PM_ACTOR", "alice")]), 0);
    let claim_hlc = sb
        .store()
        .ops(sb.ulid_of(&id))
        .unwrap()
        .into_iter()
        .find(|op| matches!(op.payload, Payload::Claim(_)))
        .unwrap()
        .hlc;
    let before = sb.op_count();

    let out = sb.run(&["claim", &id, "--json"], &[("PM_ACTOR", "bob")]);
    assert_code(&out, 75);
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["schema"], 1);
    assert_eq!(v["id"], id);
    assert_eq!(v["taken_by"], "alice");
    assert_eq!(v["at"], serde_json::to_value(claim_hlc).unwrap());
    assert!(stderr(&out).contains("taken by alice"), "{}", stderr(&out));
    assert_eq!(sb.op_count(), before, "a rejected claim appends nothing");

    // Without --json: same exit code, message on stderr, empty stdout.
    let out = sb.run(&["claim", &id], &[("PM_ACTOR", "bob")]);
    assert_code(&out, 75);
    assert!(stdout(&out).is_empty());
    // AGT-1449: the holder's claim time reads as a UTC date-time, never
    // the raw `<wall_ms>.<counter>` HLC.
    let text = stderr(&out);
    let since = text.split("(since ").nth(1).unwrap_or_default();
    let stamp = &since[..since.find(')').unwrap_or(0)];
    let b = stamp.as_bytes();
    assert!(
        stamp.len() == 20
            && stamp.ends_with(" UTC")
            && b[4] == b'-'
            && b[7] == b'-'
            && b[10] == b' '
            && b[13] == b':',
        "{text}"
    );

    // The holder cannot re-claim either: a claim is once per ticket.
    assert_code(&sb.run(&["claim", &id], &[("PM_ACTOR", "alice")]), 75);
}

#[test]
fn a_ticket_that_is_no_longer_unstarted_is_taken_even_without_an_assignee() {
    let sb = Sandbox::new();
    let id = sb.new_ticket(&[]);
    sb.transition(&id, "done");
    let out = sb.pm(&["claim", &id, "--json"]);
    assert_code(&out, 75);
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["taken_by"], Value::Null);
    assert_eq!(v["state"], "done");
    assert!(stderr(&out).contains("in state 'done'"), "{}", stderr(&out));
}

#[test]
fn twenty_concurrent_claims_yield_exactly_one_winner_and_nineteen_75s() {
    let sb = Sandbox::new();
    let id = sb.new_ticket(&["--project", "pm"]);
    let before = sb.op_count();

    let children: Vec<_> = (0..20)
        .map(|i| {
            let actor = format!("claude:loop-{i}");
            sb.command(&["claim", &id, "--json"], &[("PM_ACTOR", &actor)])
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap()
        })
        .collect();
    let outputs: Vec<Output> = children
        .into_iter()
        .map(|c| c.wait_with_output().unwrap())
        .collect();

    let codes: Vec<i32> = outputs.iter().map(|o| o.status.code().unwrap()).collect();
    let winners: Vec<&Output> = outputs
        .iter()
        .filter(|o| o.status.code() == Some(0))
        .collect();
    assert_eq!(winners.len(), 1, "codes: {codes:?}");
    assert_eq!(
        codes.iter().filter(|c| **c == 75).count(),
        19,
        "codes: {codes:?}"
    );

    let winner: Value = serde_json::from_slice(&winners[0].stdout).unwrap();
    let winner_actor = winner["assignee"].as_str().unwrap().to_string();
    assert!(winner_actor.starts_with("claude:loop-"), "{winner}");
    let claim_hlc = sb
        .store()
        .ops(sb.ulid_of(&id))
        .unwrap()
        .into_iter()
        .find(|op| matches!(op.payload, Payload::Claim(_)))
        .unwrap()
        .hlc;
    for loser in outputs.iter().filter(|o| o.status.code() == Some(75)) {
        let v: Value = serde_json::from_slice(&loser.stdout).unwrap();
        assert_eq!(v["taken_by"], winner_actor, "{v}");
        assert_eq!(v["at"], serde_json::to_value(claim_hlc).unwrap(), "{v}");
    }

    // Exactly one claim landed, the tables agree with the log, and the
    // database is healthy afterwards.
    assert_eq!(sb.op_count(), before + 1);
    let v = json(&sb.pm(&["show", &id, "--json"]));
    assert_eq!(v["assignee"], winner_actor);
    assert_eq!(v["state"], "in-progress");
    assert_code(&sb.pm(&["doctor"]), 0);
}

// ---------------------------------------------------------------- AC3

/// AGT-1397 AC2: with a hub configured the hub decides, so one that
/// cannot answer — unreachable, or no token to ask with — is exit 1 and
/// writes nothing. (Claims through a reachable hub: `tests/sync.rs`.)
#[test]
fn claim_needs_the_hub_when_one_is_configured() {
    let sb = Sandbox::new();
    let id = sb.new_ticket(&[]);
    let config = sb.home.path().join(".config/pm/config.toml");
    let mut text = std::fs::read_to_string(&config).unwrap();
    // Port 1: nothing listens, the connection is refused at once.
    text.insert_str(0, "hub = \"http://127.0.0.1:1\"\n");
    std::fs::write(&config, text).unwrap();
    let before = sb.op_count();
    let token = [("PM_HUB_TOKEN", "pmh_some-token")];

    for args in [
        vec!["claim", id.as_str()],
        vec!["claim", id.as_str(), "--json"],
        vec!["claim", "--ready"],
    ] {
        let out = sb.run(&args, &token);
        assert_code(&out, 1);
        assert!(
            stderr(&out).contains("claims require the hub")
                && stderr(&out).contains("cannot reach the hub at http://127.0.0.1:1"),
            "{}",
            stderr(&out)
        );
        assert!(
            stdout(&out).is_empty(),
            "no partial output: {}",
            stdout(&out)
        );
    }
    // Without a token the hub could not have answered either.
    let out = sb.pm(&["claim", &id]);
    assert_code(&out, 1);
    assert!(
        stderr(&out).contains("claims require the hub") && stderr(&out).contains("no hub token"),
        "{}",
        stderr(&out)
    );

    assert_eq!(sb.op_count(), before);
    assert_eq!(
        json(&sb.pm(&["show", &id, "--json"]))["assignee"],
        Value::Null
    );
    // Reads and other writes are unaffected: only claims need the authority.
    assert_code(&sb.pm(&["set", &id, "title=still fine"]), 0);
}

// ---------------------------------------------------------------- AC4

#[test]
fn claim_ready_takes_the_lowest_numbered_ready_ticket_in_the_project() {
    let sb = Sandbox::new();
    let gate = sb.new_ticket(&["--project", "pm", "--label", "manual"]); // AGT-1: gated
    let blocked = sb.new_ticket(&["--project", "pm", "--blocked-by", &gate]); // AGT-2: blocked
    let elsewhere = sb.new_ticket(&["--project", "other"]); // AGT-3
    let first = sb.new_ticket(&["--project", "pm"]); // AGT-4
    let second = sb.new_ticket(&["--project", "pm"]); // AGT-5
    assert_eq!((gate.as_str(), blocked.as_str()), ("AGT-1", "AGT-2"));
    assert_eq!(elsewhere, "AGT-3");

    let out = sb.run(
        &["claim", "--ready", "--project", "pm", "--branch", "b1"],
        &[("PM_ACTOR", "claude:a")],
    );
    assert_code(&out, 0);
    assert_eq!(stdout(&out), format!("{first}\n"));
    let v = json(&sb.pm(&["show", &first, "--json"]));
    assert_eq!(v["assignee"], "claude:a");
    assert_eq!(v["ext"]["branch"], "b1");

    // Next call: the next lowest. The claimed one is no longer ready.
    let v = json(&sb.run(
        &["claim", "--ready", "--project", "pm", "--json"],
        &[("PM_ACTOR", "claude:b")],
    ));
    assert_eq!(v["id"], second);
    assert_eq!(v["assignee"], "claude:b");

    // Nothing left in pm: AGT-1 is gated, AGT-2 blocked by it -> exit 3.
    let out = sb.pm(&["claim", "--ready", "--project", "pm"]);
    assert_code(&out, 3);
    assert!(
        stderr(&out).contains("no ready ticket in project 'pm'"),
        "{}",
        stderr(&out)
    );
    // Any project: AGT-3 is still there.
    assert_eq!(
        stdout(&sb.pm(&["claim", "--ready"])),
        format!("{elsewhere}\n")
    );
    assert_code(&sb.pm(&["claim", "--ready"]), 3);

    // Resolving the blocker frees AGT-2 (the gate label only holds AGT-1).
    sb.transition(&gate, "done");
    assert_eq!(
        stdout(&sb.pm(&["claim", "--ready", "--project", "pm"])),
        format!("{blocked}\n")
    );
}

#[test]
fn claim_ready_against_a_missing_project_is_not_found() {
    let sb = Sandbox::new();
    sb.new_ticket(&[]);
    let out = sb.pm(&["claim", "--ready", "--project", "nope"]);
    assert_code(&out, 3);
    assert!(
        stderr(&out).contains("project 'nope' does not exist"),
        "{}",
        stderr(&out)
    );
}

// ---------------------------------------------------------------- exit codes

#[test]
fn claim_usage_and_not_found_exit_codes() {
    let sb = Sandbox::new();
    assert_code(&sb.pm(&["claim"]), 2);
    assert_code(&sb.pm(&["claim", "AGT-1", "--ready"]), 2);
    assert_code(&sb.pm(&["claim", "AGT-1", "--project", "pm"]), 2);
    assert_code(&sb.pm(&["claim", "AGT-1", "--branch", " "]), 2);
    assert_code(&sb.pm(&["claim", "AGT-1"]), 3);

    let id = sb.new_ticket(&[]);
    let ulid = sb.ulid_of(&id);
    let mut store = sb.store();
    let hlc = Clock::from_latest(store.latest_hlc().unwrap()).send(1);
    store
        .commit(&Op::new(
            Ulid::new(),
            hlc,
            ActorId::new("tester"),
            ulid,
            Payload::Tombstone,
        ))
        .unwrap();
    let out = sb.pm(&["claim", &id]);
    assert_code(&out, 3);
    assert!(stderr(&out).contains("deleted"), "{}", stderr(&out));
}
