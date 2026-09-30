//! `pm archive` / `pm unarchive` through the built binary (AGT-1351).
//! Sandbox as in `claim.rs`/`ready.rs`: temp HOME, cleared env, stdin
//! closed.
//!
//! `--auto`'s "completion month is before the current one" needs a ticket
//! that actually completed in the past, and `pm` only ever stamps "now" —
//! there is no flag to backdate a `pm done`. So these tests do exactly
//! what the ticket's own notes say to: create and complete a ticket
//! normally, then reach into the op log with `rusqlite` (a dev-dependency
//! already, for the same reason `store.rs` and `edit.rs` use it: pm-store
//! has no "corrupt a row for a test" surface) and move its `ticket.create`
//! and `state.transition` ops' `hlc_wall_ms` into the past, oldest first so
//! the LWW `state` register still resolves to `done`. `pm doctor --rebuild`
//! then re-materializes the ticket tables from the edited log, exactly as
//! it would after a real backdated sync.

use std::path::PathBuf;
use std::process::{Command, Output, Stdio};

use pm_core::{Project, ProjectStatus};
use pm_store::Store;
use rusqlite::{Connection, params};
use serde_json::Value;
use tempfile::TempDir;

struct Sandbox {
    home: TempDir,
    ws: PathBuf,
}

impl Sandbox {
    /// An initialized AGT workspace (the config default) with a `pm` project.
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
        sb
    }

    fn ws_str(&self) -> &str {
        self.ws.to_str().unwrap()
    }

    fn pm(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_pm"))
            .args(args)
            .env_clear()
            .env("HOME", self.home.path())
            .env("USER", "tester")
            .stdin(Stdio::null())
            .output()
            .unwrap()
    }

    fn project(&self, id: &str, title: &str) {
        Store::open(self.ws.join("pm.sqlite"))
            .unwrap()
            .put_project(
                &Project {
                    id: id.into(),
                    title: title.into(),
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

    fn ulid_of(&self, id: &str) -> String {
        json(&self.pm(&["show", id, "--json"]))["ulid"]
            .as_str()
            .unwrap()
            .to_string()
    }

    /// Moves every op of `kind` against `ticket_ulid` to `wall_ms`, then
    /// replays the log so the materialized tables (and every cached view)
    /// agree with the edited history — see the module doc for why this is
    /// how these tests reach a ticket "into the past".
    fn backdate(&self, ticket_ulid: &str, kind: &str, wall_ms: i64) {
        let conn = Connection::open(self.ws.join("pm.sqlite")).unwrap();
        let changed = conn
            .execute(
                "UPDATE ops SET hlc_wall_ms = ?1 WHERE entity = ?2 AND kind = ?3",
                params![wall_ms, ticket_ulid, kind],
            )
            .unwrap();
        assert!(changed > 0, "no '{kind}' op found for {ticket_ulid}");
        assert_code(&self.pm(&["doctor", "--rebuild"]), 0);
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
        "stdout:\n{}\nstderr:\n{}",
        stdout(out),
        stderr(out)
    );
}

fn json(out: &Output) -> Value {
    assert_code(out, 0);
    serde_json::from_str(&stdout(out)).unwrap_or_else(|e| panic!("{e}: {}", stdout(out)))
}

fn list_ids(v: &Value) -> Vec<&str> {
    v.as_array()
        .unwrap()
        .iter()
        .map(|t| t["id"].as_str().unwrap())
        .collect()
}

// 2026-08-15T00:00:00Z and 2026-08-01T00:00:00Z: both well before whatever
// "now" the test machine's clock reads (this repo's earliest plausible
// build date is 2026), so the ticket's completion month is always in the
// past relative to `--auto`'s `now_ms`.
const AUGUST_CREATE_MS: i64 = 1_785_542_400_000;
const AUGUST_DONE_MS: i64 = 1_786_752_000_000;

#[test]
fn archive_and_unarchive_round_trip_a_single_ticket() {
    let sb = Sandbox::new();
    sb.project("pm", "pm");
    assert_code(&sb.pm(&["new", "--title", "t", "--project", "pm"]), 0);

    let v = json(&sb.pm(&["archive", "AGT-1", "--json"]));
    assert!(v["archived_at"].is_object(), "{v}");
    assert_eq!(
        list_ids(&json(&sb.pm(&["list", "--json"]))),
        Vec::<&str>::new()
    );
    assert_eq!(
        list_ids(&json(&sb.pm(&["list", "--archived", "--json"]))),
        ["AGT-1"]
    );

    let v = json(&sb.pm(&["unarchive", "AGT-1", "--json"]));
    assert_eq!(v["archived_at"], Value::Null);
    assert_eq!(
        list_ids(&json(&sb.pm(&["list", "--json"]))),
        ["AGT-1"],
        "unarchive puts it back in the default list"
    );
}

#[test]
fn archive_requires_an_id_or_auto_and_rejects_both() {
    let sb = Sandbox::new();
    assert_code(&sb.pm(&["archive"]), 2);
    sb.project("pm", "pm");
    assert_code(&sb.pm(&["new", "--title", "t", "--project", "pm"]), 0);
    assert_code(&sb.pm(&["archive", "AGT-1", "--auto"]), 2);
    assert_code(&sb.pm(&["archive", "--dry-run"]), 2);
}

#[test]
fn auto_archives_a_ticket_completed_last_month_and_skips_the_current_one() {
    let sb = Sandbox::new();
    sb.project("pm", "pm");
    assert_code(
        &sb.pm(&["new", "--title", "completed in august", "--project", "pm"]),
        0,
    );
    assert_code(
        &sb.pm(&["new", "--title", "completed just now", "--project", "pm"]),
        0,
    );
    assert_code(&sb.pm(&["done", "AGT-1"]), 0);
    assert_code(&sb.pm(&["done", "AGT-2"]), 0);
    let old = sb.ulid_of("AGT-1");
    sb.backdate(&old, "ticket.create", AUGUST_CREATE_MS);
    sb.backdate(&old, "state.transition", AUGUST_DONE_MS);

    // --dry-run: reports AGT-1, writes nothing.
    let v = json(&sb.pm(&["archive", "--auto", "--dry-run", "--json"]));
    assert_eq!(v["dry_run"], true);
    assert_eq!(v["archived_tickets"], serde_json::json!(["AGT-1"]));
    let show = json(&sb.pm(&["show", "AGT-1", "--json"]));
    assert_eq!(
        show["archived_at"],
        Value::Null,
        "dry-run must not write: {show}"
    );

    // The real run archives exactly AGT-1; AGT-2 (this month) is untouched.
    let v = json(&sb.pm(&["archive", "--auto", "--json"]));
    assert_eq!(v["dry_run"], false);
    assert_eq!(v["archived_tickets"], serde_json::json!(["AGT-1"]));
    assert_eq!(
        list_ids(&json(&sb.pm(&["list", "--json"]))),
        ["AGT-2"],
        "AGT-1 archived out of the default list"
    );
    assert_eq!(
        list_ids(&json(&sb.pm(&["list", "--archived", "--json"]))),
        ["AGT-1", "AGT-2"]
    );

    // Idempotent: nothing left to archive on a second run.
    let v = json(&sb.pm(&["archive", "--auto", "--json"]));
    assert_eq!(v["archived_tickets"], serde_json::json!([]));
}

#[test]
fn auto_retires_an_idle_project_but_leaves_an_active_one_alone() {
    let sb = Sandbox::new();
    sb.project("idle-empty", "Idle Empty");
    sb.project("active", "Active");
    assert_code(
        &sb.pm(&["new", "--title", "still open", "--project", "active"]),
        0,
    );

    let v = json(&sb.pm(&["archive", "--auto", "--json"]));
    assert_eq!(
        v["completed_projects"],
        serde_json::json!(["idle-empty"]),
        "zero tickets and no doc edit ever recorded: stale"
    );

    let idle = json(&sb.pm(&["project", "show", "idle-empty", "--json"]));
    assert_eq!(idle["status"], "complete");
    let active = json(&sb.pm(&["project", "show", "active", "--json"]));
    assert_eq!(
        active["status"], "in-progress",
        "a live ticket keeps it open"
    );
}

#[test]
fn a_ticket_archived_by_pm_archive_still_does_not_block_pm_ready() {
    // AC2, proven at the level AGT-1351 owns: the real `pm archive`
    // command, not a hand-built op (AGT-1343's own `pm ready` tests already
    // cover the readiness rule itself independent of how archived_at got
    // set). A `done` blocker already resolves before archiving
    // (`pm_core::ready::Graph::done` treats a completed state as done on
    // its own); the AC1351 guarantee is that `pm archive` does not
    // *regress* that — AGT-2 stays ready across the archive.
    let sb = Sandbox::new();
    sb.project("pm", "pm");
    assert_code(&sb.pm(&["new", "--title", "blocker", "--project", "pm"]), 0);
    assert_code(
        &sb.pm(&[
            "new",
            "--title",
            "blocked",
            "--project",
            "pm",
            "--blocked-by",
            "AGT-1",
        ]),
        0,
    );
    assert_code(&sb.pm(&["done", "AGT-1"]), 0);

    let before = json(&sb.pm(&["ready", "--json"]));
    assert_eq!(
        list_ids(&before["ready"]),
        ["AGT-2"],
        "a done, un-archived blocker already resolves it"
    );

    assert_code(&sb.pm(&["archive", "AGT-1"]), 0);
    let after = json(&sb.pm(&["ready", "--json"]));
    assert_eq!(
        list_ids(&after["ready"]),
        ["AGT-2"],
        "archiving the done blocker must not make it block"
    );
}

#[test]
fn doctor_stays_clean_through_a_full_archive_workflow() {
    let sb = Sandbox::new();
    sb.project("pm", "pm");
    assert_code(&sb.pm(&["new", "--title", "a", "--project", "pm"]), 0);
    assert_code(&sb.pm(&["new", "--title", "b", "--project", "pm"]), 0);
    assert_code(&sb.pm(&["done", "AGT-1"]), 0);
    let old = sb.ulid_of("AGT-1");
    sb.backdate(&old, "ticket.create", AUGUST_CREATE_MS);
    sb.backdate(&old, "state.transition", AUGUST_DONE_MS);
    assert_code(&sb.pm(&["archive", "--auto"]), 0);
    assert_code(&sb.pm(&["archive", "AGT-2"]), 0);
    assert_code(&sb.pm(&["unarchive", "AGT-2"]), 0);

    assert_code(&sb.pm(&["doctor"]), 0);
    let v = json(&sb.pm(&["doctor", "--rebuild", "--json"]));
    assert_eq!(v["healthy"], true, "{v}");
    assert_eq!(
        v["rebuilt"],
        serde_json::json!({ "tables": [] }),
        "the rebuild changes nothing: {v}"
    );
}
