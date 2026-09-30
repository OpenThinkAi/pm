//! End-to-end trust checks on pushed ops (AGT-1450, oaudit 2026-09-30):
//! tokens bound to actor patterns, legacy tokens (minted before bindings)
//! staying unrestricted across the migration, the reserved `hub` actor,
//! stamp range / far-future checks, and the checked number floor; and
//! (AGT-1463) actors named in payloads, who may end a seed, and `token
//! create` needing `--actor` or `--any`. See `common` for where Postgres
//! comes from.

mod common;

use common::*;
use pm_core::op::{ActorUpsert, Claim, FieldSet, HoldSet, LabelAdd, TicketCreate};
use pm_core::{ActorId, ActorKind, Hlc, Hold, MAX_FUTURE_SKEW_MS, Op, Payload, Priority};
use serde_json::{Value, json};
use ulid::Ulid;

/// A well-formed stamp just behind the hub's wall clock.
fn recent() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
        - 60_000
}

fn label_as(actor: &str, hlc: Hlc) -> Op {
    Op::new(
        Ulid::new(),
        hlc,
        ActorId::new(actor),
        Ulid::new(),
        Payload::LabelAdd(LabelAdd {
            label: "l".to_string(),
        }),
    )
}

fn create_as(actor: &str, entity: Ulid, wall_ms: u64) -> Op {
    Op::new(
        Ulid::new(),
        Hlc::new(wall_ms, 0),
        ActorId::new(actor),
        entity,
        Payload::TicketCreate(TicketCreate {
            title: "t".into(),
            state: "triage".into(),
            priority: Priority::Medium,
            project: None,
            repo: None,
            source: None,
            ext: Default::default(),
        }),
    )
}

fn post(port: u16, token: &str, path: &str, body: &str) -> (u16, Value) {
    let resp = request_body(
        port,
        "POST",
        path,
        &[&format!("Authorization: Bearer {token}")],
        body.as_bytes(),
    );
    let body: Value = serde_json::from_str(&resp.body)
        .unwrap_or_else(|e| panic!("{} body {:?} is not JSON: {e}", resp.status, resp.body));
    (resp.status, body)
}

fn push_values(port: u16, token: &str, ops: &[Value]) -> (u16, Value) {
    post(
        port,
        token,
        "/w/saltline/ops",
        &json!({ "ops": ops }).to_string(),
    )
}

fn push(port: u16, token: &str, ops: &[&Op]) -> (u16, Value) {
    let ops: Vec<Value> = ops
        .iter()
        .map(|o| serde_json::to_value(o).unwrap())
        .collect();
    push_values(port, token, &ops)
}

fn stored(body: &Value) -> Vec<bool> {
    body["ops"]
        .as_array()
        .unwrap_or_else(|| panic!("no ops in {body}"))
        .iter()
        .map(|a| a["stored"].as_bool().unwrap())
        .collect()
}

fn count_ops(url: &str) -> i64 {
    query_rows(url, "SELECT count(*) FROM ops").unwrap()[0][0]
        .as_deref()
        .unwrap()
        .parse()
        .unwrap()
}

/// Mints a token with `extra` args: `(plaintext, id, stderr)`.
fn mint(url: &str, name: &str, extra: &[&str]) -> (String, String, String) {
    let mut args = vec!["token", "create", name, "--workspace", "saltline"];
    args.extend_from_slice(extra);
    let (ok, stdout, stderr) = admin(url, &args);
    assert!(ok, "token create failed: {stderr}");
    let id = stderr
        .lines()
        .find_map(|l| l.strip_prefix("token "))
        .and_then(|l| l.split_whitespace().next())
        .unwrap_or_else(|| panic!("no token id in {stderr:?}"))
        .to_string();
    (stdout.trim().to_string(), id, stderr)
}

/// The ACTORS column of `token list` for token `id`.
fn listed_actors(url: &str, id: &str) -> String {
    let (ok, stdout, stderr) = admin(url, &["token", "list"]);
    assert!(ok, "{stderr}");
    let header: Vec<&str> = stdout.lines().next().unwrap().split('\t').collect();
    assert_eq!(
        header,
        ["ID", "WORKSPACE", "NAME", "ACTORS", "CREATED", "REVOKED"]
    );
    stdout
        .lines()
        .map(|l| l.split('\t').collect::<Vec<_>>())
        .find(|cols| cols[0] == id)
        .unwrap_or_else(|| panic!("no token {id} in {stdout}"))[3]
        .to_string()
}

#[test]
fn bound_tokens_author_only_their_actors_and_legacy_tokens_survive_the_migration() {
    let Some((_container, url)) = postgres_for(
        "bound_tokens_author_only_their_actors_and_legacy_tokens_survive_the_migration",
    ) else {
        return;
    };
    let port = free_port();
    let mut hub = spawn_hub(&url, port);
    wait_for_health(&mut hub, port);
    let (_, _, _) = mint(&url, "bootstrap", &["--any"]);

    // --- a token from before bindings: rewind the schema to version 3
    // and insert the row the way the old hub's `token create` did.
    drop(hub);
    query_rows(
        &url,
        "ALTER TABLE tokens DROP COLUMN actors;
         DELETE FROM schema_version WHERE version = 4",
    )
    .unwrap();
    // The admin CLI refuses a schema older than it expects.
    let (ok, _, stderr) = admin(&url, &["token", "list"]);
    assert!(!ok);
    assert!(
        stderr.contains("deploy (start) the current hub"),
        "{stderr}"
    );
    let legacy = format!("pmh_{}", "L".repeat(43));
    query_rows(
        &url,
        &format!(
            "INSERT INTO tokens (workspace_id, label, token_hash)
             VALUES ('saltline', 'studio', sha256(convert_to('{legacy}', 'UTF8')))"
        ),
    )
    .unwrap();
    let legacy_id = query_rows(&url, "SELECT id FROM tokens WHERE label = 'studio'").unwrap()[0][0]
        .clone()
        .unwrap();
    let port = free_port();
    let mut hub = spawn_hub(&url, port);
    let (_, health) = wait_for_health(&mut hub, port);
    assert_eq!(health["schema_version"], expected_schema_version());
    let raw = query_rows(
        &url,
        &format!("SELECT actors IS NULL FROM tokens WHERE id = {legacy_id}"),
    )
    .unwrap();
    assert_eq!(
        raw[0][0].as_deref(),
        Some("t"),
        "the migration left it unbound"
    );
    assert_eq!(listed_actors(&url, &legacy_id), "any (legacy, unbound)");

    // The legacy token still pushes as every actor one machine carries
    // (README decision A7), as it did before the upgrade.
    let t = recent();
    let by_matt = label_as("matt", Hlc::new(t, 0));
    let by_sync = label_as("pm-sync", Hlc::new(t, 1));
    let by_agent = label_as("claude:pm-build", Hlc::new(t, 2));
    let by_alice = label_as("alice", Hlc::new(t, 3));
    let (status, body) = push(port, &legacy, &[&by_matt, &by_sync, &by_agent, &by_alice]);
    assert_eq!(status, 200, "{body}");
    assert_eq!(stored(&body), [true; 4]);

    // --- a new token bound to patterns.
    let (bound, bound_id, stderr) = mint(
        &url,
        "laptop",
        &["--actor", "matt,claude:*", "--actor", "pm-sync"],
    );
    assert!(
        stderr.contains("may author ops as: matt,claude:*,pm-sync"),
        "{stderr}"
    );
    assert_eq!(listed_actors(&url, &bound_id), "matt,claude:*,pm-sync");
    let mine = [
        label_as("matt", Hlc::new(t, 10)),
        label_as("claude:other-agent", Hlc::new(t, 11)),
        label_as("pm-sync", Hlc::new(t, 12)),
    ];
    let (status, body) = push(port, &bound, &mine.iter().collect::<Vec<_>>());
    assert_eq!(status, 200, "{body}");
    assert_eq!(stored(&body), [true; 3]);

    // Anyone else is a structured 400 naming the op, and the batch is
    // refused whole.
    let before = count_ops(&url);
    let fine = label_as("matt", Hlc::new(t, 20));
    let spoofed = label_as("alice", Hlc::new(t, 21));
    let (status, err) = push(port, &bound, &[&fine, &spoofed]);
    assert_eq!(status, 400, "{err}");
    assert_eq!(err["error"], "actor_not_allowed");
    assert_eq!(err["index"], 1);
    assert_eq!(err["op_id"], spoofed.op_id.to_string());
    let reason = err["reason"].as_str().unwrap();
    assert!(reason.contains("\"alice\""), "{reason}");
    assert!(reason.contains("matt,claude:*,pm-sync"), "{reason}");
    assert_eq!(count_ops(&url), before, "nothing stored");
    // `claude` alone is not `claude:*`.
    let (status, err) = push(port, &bound, &[&label_as("claude", Hlc::new(t, 22))]);
    assert_eq!(
        (status, err["error"].as_str()),
        (400, Some("actor_not_allowed"))
    );

    // An op the workspace already has is acknowledged whoever wrote it:
    // nothing is stored, so there is nothing to authorize.
    let (status, body) = push(port, &bound, &[&by_alice, &fine]);
    assert_eq!(status, 200, "{body}");
    assert_eq!(stored(&body), [false, true]);

    // `create` needs --actor or an explicit --any (AGT-1463): no default
    // any-actor token, and not both.
    let (ok, stdout, stderr) = admin(
        &url,
        &["token", "create", "plain", "--workspace", "saltline"],
    );
    assert!(!ok, "minted without --actor/--any: {stdout}");
    assert!(stdout.is_empty(), "no token printed: {stdout}");
    assert!(stderr.contains("--any"), "{stderr}");
    let (ok, _, _) = admin(
        &url,
        &[
            "token",
            "create",
            "both",
            "--workspace",
            "saltline",
            "--any",
            "--actor",
            "matt",
        ],
    );
    assert!(!ok, "--any with --actor");
    // --any is recorded as `*`, with a note.
    let (open, open_id, stderr) = mint(&url, "open", &["--any"]);
    assert!(stderr.contains("may author ops as any actor"), "{stderr}");
    assert!(
        stderr.contains(&format!("token bind {open_id}")),
        "{stderr}"
    );
    assert_eq!(listed_actors(&url, &open_id), "*");
    let (status, _) = push(port, &open, &[&label_as("anyone", Hlc::new(t, 30))]);
    assert_eq!(status, 200);

    // --- `token bind` restricts the legacy token after the fact; it takes
    // effect on the next request, no restart.
    let (ok, _, stderr) = admin(
        &url,
        &[
            "token",
            "bind",
            &legacy_id,
            "--actor",
            "matt",
            "--actor",
            "pm-sync,claude:*",
        ],
    );
    assert!(ok, "{stderr}");
    assert_eq!(listed_actors(&url, &legacy_id), "matt,pm-sync,claude:*");
    let (status, err) = push(port, &legacy, &[&label_as("alice", Hlc::new(t, 40))]);
    assert_eq!(
        (status, err["error"].as_str()),
        (400, Some("actor_not_allowed"))
    );
    let (status, _) = push(port, &legacy, &[&label_as("matt", Hlc::new(t, 41))]);
    assert_eq!(status, 200);
    // ...and `*` opens it again.
    let (ok, _, stderr) = admin(&url, &["token", "bind", &legacy_id, "--actor", "*"]);
    assert!(ok, "{stderr}");
    let (status, _) = push(port, &legacy, &[&label_as("alice", Hlc::new(t, 42))]);
    assert_eq!(status, 200);

    // Bind refuses bad patterns, missing and revoked tokens.
    for (args, want) in [
        (
            vec!["token", "bind", &bound_id, "--actor", "cl*ude"],
            "may only end",
        ),
        (vec!["token", "bind", &bound_id, "--actor", ""], "empty"),
        (
            vec!["token", "bind", "999999", "--actor", "matt"],
            "no token 999999",
        ),
    ] {
        let (ok, _, stderr) = admin(&url, &args);
        assert!(!ok, "{args:?}");
        assert!(stderr.contains(want), "{args:?}: {stderr}");
    }
    let (ok, _, stderr) = admin(&url, &["token", "bind", &bound_id]);
    assert!(!ok, "--actor is required: {stderr}");
    let (ok, _, _) = admin(&url, &["token", "revoke", &open_id]);
    assert!(ok);
    let (ok, _, stderr) = admin(&url, &["token", "bind", &open_id, "--actor", "matt"]);
    assert!(!ok);
    assert!(stderr.contains("is revoked"), "{stderr}");
    let (ok, _, stderr) = admin(
        &url,
        &[
            "token",
            "create",
            "x",
            "--workspace",
            "saltline",
            "--actor",
            "a b",
        ],
    );
    assert!(!ok);
    assert!(stderr.contains("whitespace"), "{stderr}");
}

#[test]
fn the_hub_actor_is_reserved_and_seeding_history_still_lands() {
    let Some((_container, url)) =
        postgres_for("the_hub_actor_is_reserved_and_seeding_history_still_lands")
    else {
        return;
    };
    let port = free_port();
    let mut hub = spawn_hub(&url, port);
    wait_for_health(&mut hub, port);
    let (studio, _, _) = mint(&url, "studio", &["--any"]);
    let (agent, _, _) = mint(&url, "agent", &["--actor", "claude:*"]);

    // Seed mode: an unrestricted token uploads years-old history by many
    // actors — including a previous hub's number op (a reseed).
    let a = Ulid::new();
    let history = [
        create_as("matt", a, 1_000),
        label_as("claude:old-agent", Hlc::new(2_000, 0)),
        Op::new(
            Ulid::new(),
            Hlc::new(3_000, 0),
            ActorId::new("hub"),
            a,
            Payload::FieldSet(FieldSet::Number(7)),
        ),
    ];
    let (status, body) = push(port, &studio, &history.iter().collect::<Vec<_>>());
    assert_eq!(status, 200, "{body}");
    assert_eq!(stored(&body), [true; 3]);
    // A bound token may not speak as the hub, even while seeding.
    let forged = label_as("hub", Hlc::new(4_000, 0));
    let (status, err) = push(port, &agent, &[&forged]);
    assert_eq!(status, 400, "{err}");
    assert_eq!(err["error"], "reserved_actor");
    assert_eq!(err["op_id"], forged.op_id.to_string());

    // A floor past the allocator's range is a 400 and the seed can still
    // end; so is a seeded number past it.
    let (status, err) = post(
        port,
        &studio,
        "/w/saltline/seeded",
        &json!({ "number_floor": i64::MAX }).to_string(),
    );
    assert_eq!(status, 400, "{err}");
    assert_eq!(err["error"], "invalid_body");
    let huge = Op::new(
        Ulid::new(),
        Hlc::new(5_000, 0),
        ActorId::new("matt"),
        Ulid::new(),
        Payload::FieldSet(FieldSet::Number(i64::MAX as u64)),
    );
    let (status, err) = push(port, &studio, &[&huge]);
    assert_eq!(status, 400, "{err}");
    assert_eq!(err["error"], "invalid_op");
    assert!(
        err["reason"].as_str().unwrap().contains("out of range"),
        "{err}"
    );
    // Only an unrestricted token may end the seed (AGT-1463): a bound one
    // is refused and the workspace stays seeding.
    let (status, err) = post(
        port,
        &agent,
        "/w/saltline/seeded",
        &json!({ "number_floor": 9_007_199_254_740_990_u64 }).to_string(),
    );
    assert_eq!(status, 400, "{err}");
    assert_eq!(err["error"], "seed_not_allowed");
    assert!(err["reason"].as_str().unwrap().contains("(agent)"), "{err}");
    let seeded = query_rows(&url, "SELECT seeded_at IS NULL FROM workspaces").unwrap();
    assert_eq!(seeded[0][0].as_deref(), Some("t"), "still seeding");
    let (status, body) = post(
        port,
        &studio,
        "/w/saltline/seeded",
        &json!({ "number_floor": 100 }).to_string(),
    );
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["number_floor"], 100);
    // Once ended, every token hears it is done.
    let (status, err) = post(
        port,
        &agent,
        "/w/saltline/seeded",
        &json!({ "number_floor": 1 }).to_string(),
    );
    assert_eq!(status, 409, "{err}");
    assert_eq!(err["error"], "already_seeded");

    // Seeded: nobody speaks as the hub, not even an unrestricted token.
    let forged = label_as("hub", Hlc::new(recent(), 0));
    let (status, err) = push(port, &studio, &[&forged]);
    assert_eq!(status, 400, "{err}");
    assert_eq!(err["error"], "reserved_actor");
    // The hub's own ops keep flowing.
    let b = Ulid::new();
    let (status, body) = push(port, &agent, &[&create_as("claude:x", b, recent())]);
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["numbers"][0]["number"], 101);
    assert_eq!(body["numbers"][0]["op"]["actor"], "hub");
}

#[test]
fn pushed_stamps_are_range_checked_and_bounded_in_the_future() {
    let Some((_container, url)) =
        postgres_for("pushed_stamps_are_range_checked_and_bounded_in_the_future")
    else {
        return;
    };
    let port = free_port();
    let mut hub = spawn_hub(&url, port);
    wait_for_health(&mut hub, port);
    let (studio, _, _) = mint(&url, "studio", &["--any"]);
    let now = recent() + 60_000;

    let ok = label_as("matt", Hlc::new(1_000, 0));
    let cases: Vec<(&str, Hlc, &str)> = vec![
        (
            "wall_ms above u64 storage",
            Hlc::new(u64::MAX, 0),
            "invalid_stamp",
        ),
        (
            "wall_ms just above i64::MAX",
            Hlc::new(i64::MAX as u64 + 1, 0),
            "invalid_stamp",
        ),
        (
            "spent counter",
            Hlc::new(now - 60_000, u32::MAX),
            "invalid_stamp",
        ),
        (
            "two days ahead",
            Hlc::new(now + 2 * MAX_FUTURE_SKEW_MS, 0),
            "future_stamp",
        ),
        ("i64::MAX", Hlc::new(i64::MAX as u64, 0), "future_stamp"),
    ];
    for (case, hlc, want) in cases {
        let bad = label_as("matt", hlc);
        let (status, err) = push(port, &studio, &[&ok, &bad]);
        assert_eq!(status, 400, "{case}: {err}");
        assert_eq!(err["error"], want, "{case}: {err}");
        assert_eq!(err["index"], 1, "{case}");
        assert_eq!(err["op_id"], bad.op_id.to_string(), "{case}");
        assert_eq!(count_ops(&url), 0, "{case}: batch refused whole");
    }

    // A prefix or project id unsafe in a file path (AGT-1450).
    let ws = Ulid::new();
    let bad_prefix = Op::new(
        Ulid::new(),
        Hlc::new(1_000, 1),
        ActorId::new("matt"),
        ws,
        Payload::WorkspaceSet(pm_core::op::WorkspaceSet::Prefix("../../etc".into())),
    );
    let (status, err) = push(port, &studio, &[&ok, &bad_prefix]);
    assert_eq!(status, 400, "{err}");
    assert_eq!(err["error"], "invalid_id");
    assert_eq!(err["index"], 1);
    assert!(
        err["reason"].as_str().unwrap().contains("workspace prefix"),
        "{err}"
    );
    assert_eq!(count_ops(&url), 0);

    // A few hours ahead (a fast clock) and ancient history are fine.
    let ahead = label_as("matt", Hlc::new(now + 3 * 60 * 60 * 1000, u32::MAX - 1));
    let (status, body) = push(port, &studio, &[&ok, &ahead]);
    assert_eq!(status, 200, "{body}");
    assert_eq!(stored(&body), [true, true]);
}

fn on(ticket: Ulid, actor: &str, hlc: Hlc, payload: Payload) -> Op {
    Op::new(Ulid::new(), hlc, ActorId::new(actor), ticket, payload)
}

fn assign(ticket: Ulid, actor: &str, hlc: Hlc, to: Option<&str>) -> Op {
    on(
        ticket,
        actor,
        hlc,
        Payload::FieldSet(FieldSet::Assignee(to.map(ActorId::new))),
    )
}

#[test]
fn actors_named_in_payloads_are_held_to_the_token_binding() {
    let Some((_container, url)) =
        postgres_for("actors_named_in_payloads_are_held_to_the_token_binding")
    else {
        return;
    };
    let port = free_port();
    let mut hub = spawn_hub(&url, port);
    wait_for_health(&mut hub, port);
    let (studio, _, _) = mint(&url, "studio", &["--any"]);
    let (agent, _, _) = mint(&url, "agent", &["--actor", "claude:*"]);
    let t = recent();
    let ticket = Ulid::new();
    let (status, body) = push(port, &studio, &[&create_as("matt", ticket, t)]);
    assert_eq!(status, 200, "{body}");

    // A bound token may not name an actor outside its patterns in a
    // claim, a hold, an assignment or an actor registration: a structured
    // 400 naming the field, and the batch is refused whole.
    let claim_as_matt = on(
        ticket,
        "claude:x",
        Hlc::new(t, 1),
        Payload::Claim(Claim {
            state: "in-progress".into(),
            assignee: ActorId::new("matt"),
        }),
    );
    let hold_by_matt = on(
        ticket,
        "claude:x",
        Hlc::new(t, 2),
        Payload::HoldSet(HoldSet {
            hold: Hold {
                reason: "r".into(),
                by: ActorId::new("matt"),
                at: Hlc::new(t, 2),
            },
        }),
    );
    let assign_matt = assign(ticket, "claude:x", Hlc::new(t, 3), Some("matt"));
    let register_matt = on(
        Ulid::new(),
        "claude:x",
        Hlc::new(t, 4),
        Payload::ActorUpsert(ActorUpsert {
            id: ActorId::new("matt"),
            kind: ActorKind::Human,
        }),
    );
    for (bad, field) in [
        (&claim_as_matt, "claim op's assignee"),
        (&hold_by_matt, "hold.set op's hold.by"),
        (&assign_matt, "field.set op's assignee"),
        (&register_matt, "actor.upsert op's id"),
    ] {
        let before = count_ops(&url);
        let fine = label_as("claude:x", Hlc::new(t, 5));
        let (status, err) = push(port, &agent, &[&fine, bad]);
        assert_eq!(status, 400, "{field}: {err}");
        assert_eq!(err["error"], "actor_not_allowed", "{field}");
        assert_eq!(err["index"], 1, "{field}");
        assert_eq!(err["op_id"], bad.op_id.to_string(), "{field}");
        let reason = err["reason"].as_str().unwrap();
        assert!(reason.contains(field), "{field}: {reason}");
        assert!(reason.contains("\"matt\""), "{field}: {reason}");
        assert_eq!(count_ops(&url), before, "{field}: nothing stored");
    }
    // The reserved actor, named in a payload, is refused like an author.
    let claim_as_hub = on(
        ticket,
        "claude:x",
        Hlc::new(t, 6),
        Payload::Claim(Claim {
            state: "in-progress".into(),
            assignee: ActorId::new("hub"),
        }),
    );
    let (status, err) = push(port, &agent, &[&claim_as_hub]);
    assert_eq!(
        (status, err["error"].as_str()),
        (400, Some("reserved_actor")),
        "{err}"
    );

    // Its own actors, and unassigning, are fine.
    let own = [
        assign(ticket, "claude:x", Hlc::new(t, 10), Some("claude:x")),
        assign(ticket, "claude:x", Hlc::new(t, 11), None),
        on(
            ticket,
            "claude:x",
            Hlc::new(t, 12),
            Payload::HoldSet(HoldSet {
                hold: Hold {
                    reason: "r".into(),
                    by: ActorId::new("claude:x"),
                    at: Hlc::new(t, 12),
                },
            }),
        ),
    ];
    let (status, body) = push(port, &agent, &own.iter().collect::<Vec<_>>());
    assert_eq!(status, 200, "{body}");
    assert_eq!(stored(&body), [true; 3]);

    // Restating the ticket's current assignee is accepted from any token
    // (a client reconciling after a refused claim logs exactly this);
    // naming someone new is not.
    let (status, body) = push(
        port,
        &studio,
        &[&assign(ticket, "matt", Hlc::new(t, 20), Some("matt"))],
    );
    assert_eq!(status, 200, "{body}");
    let restate = assign(ticket, "claude:x", Hlc::new(t, 21), Some("matt"));
    let (status, body) = push(port, &agent, &[&restate]);
    assert_eq!(status, 200, "{body}");
    assert_eq!(stored(&body), [true]);
    let (status, err) = push(
        port,
        &agent,
        &[&assign(ticket, "claude:x", Hlc::new(t, 22), Some("alice"))],
    );
    assert_eq!(
        (status, err["error"].as_str()),
        (400, Some("actor_not_allowed")),
        "{err}"
    );
    // ...and "current" is read at that point in the batch: once the
    // batch itself reassigns, restating the old assignee is naming them.
    let (status, err) = push(
        port,
        &agent,
        &[
            &assign(ticket, "claude:x", Hlc::new(t, 23), Some("claude:x")),
            &assign(ticket, "claude:x", Hlc::new(t, 24), Some("matt")),
        ],
    );
    assert_eq!(status, 400, "{err}");
    assert_eq!(err["index"], 1);

    // An unrestricted token names anyone, as before.
    let (status, body) = push(
        port,
        &studio,
        &[&claim_as_matt, &hold_by_matt, &register_matt],
    );
    assert_eq!(status, 200, "{body}");
    assert_eq!(stored(&body), [true; 3]);
}

/// AGT-1464: the push refuses a hostile document name, a second
/// (backdated) `project.create` for a stored project and a `ticket.create`
/// under a bound document's id — ops every replica would refuse on pull.
#[test]
fn pushed_project_identity_is_admission_checked() {
    let Some((_container, url)) = postgres_for("pushed_project_identity_is_admission_checked")
    else {
        return;
    };
    let port = free_port();
    let mut hub = spawn_hub(&url, port);
    wait_for_health(&mut hub, port);
    let (studio, _, _) = mint(&url, "studio", &["--any"]);
    let (project, design, notes) = (Ulid::new(), Ulid::new(), Ulid::new());
    let at = |wall_ms: u64, entity: Ulid, payload: Payload| {
        Op::new(
            Ulid::new(),
            Hlc::new(wall_ms, 0),
            ActorId::new("matt"),
            entity,
            payload,
        )
    };
    let create = |wall_ms: u64| {
        at(
            wall_ms,
            project,
            Payload::ProjectCreate(pm_core::op::ProjectCreate {
                id: "pm".into(),
                title: "pm".into(),
                status: pm_core::ProjectStatus::InProgress,
                parent: None,
                doc_id: Some(design),
            }),
        )
    };
    let doc_add = |name: &str, doc_id: Ulid| {
        at(
            2_000,
            project,
            Payload::ProjectDocAdd(pm_core::op::ProjectDocAdd {
                name: Some(name.into()),
                doc_id,
            }),
        )
    };
    let (status, body) = push(port, &studio, &[&create(1_000), &doc_add("notes", notes)]);
    assert_eq!(status, 200, "{body}");
    assert_eq!(count_ops(&url), 2);

    let (status, err) = push(port, &studio, &[&doc_add("../x", Ulid::new())]);
    assert_eq!(
        (status, err["error"].as_str()),
        (400, Some("invalid_id")),
        "{err}"
    );

    let (status, err) = push(port, &studio, &[&create(1)]);
    assert_eq!(
        (status, err["error"].as_str()),
        (400, Some("invalid_op")),
        "{err}"
    );
    assert!(
        err["reason"]
            .as_str()
            .unwrap()
            .contains("already has a project.create"),
        "{err}"
    );

    for doc in [design, notes] {
        let (status, err) = push(port, &studio, &[&create_as("matt", doc, 3_000)]);
        assert_eq!(
            (status, err["error"].as_str()),
            (400, Some("invalid_op")),
            "{err}"
        );
        assert!(
            err["reason"]
                .as_str()
                .unwrap()
                .contains("already a project document"),
            "{err}"
        );
    }
    assert_eq!(count_ops(&url), 2, "nothing more stored");

    // A re-push of the stored create (a lost ack) is still idempotent.
    let (status, body) = push_values(
        port,
        &studio,
        &[serde_json::from_str(
            &query_rows(&url, "SELECT op::text FROM ops ORDER BY seq LIMIT 1").unwrap()[0][0]
                .clone()
                .unwrap(),
        )
        .unwrap()],
    );
    assert_eq!(status, 200, "{body}");
    assert_eq!(stored(&body), [false]);
}
