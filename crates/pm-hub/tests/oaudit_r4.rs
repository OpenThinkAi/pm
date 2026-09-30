//! oaudit round 4 at the hub's push (AGT-1482): only an unrestricted
//! token may push a seed's ticket numbers; once seeded, an actor-bound
//! token's `field.set assignee` is arbitrated like a `claim` (a restating
//! reconcile write still lands); a claim's state name, stamps carried in
//! payloads and every payload's size are checked before anything is
//! stored. See `common` for where Postgres comes from.

mod common;

use common::*;
use pm_core::op::{
    Claim, CommentAdd, FieldSet, HoldSet, StateTransition, StateUpsert, TicketCreate,
};
use pm_core::{ActorId, Hlc, Hold, MAX_FUTURE_SKEW_MS, Op, Payload, Priority, StateCategory};
use serde_json::{Value, json};
use ulid::Ulid;

fn op(entity: Ulid, wall_ms: u64, actor: &str, payload: Payload) -> Op {
    Op::new(
        Ulid::new(),
        Hlc::new(wall_ms, 0),
        ActorId::new(actor),
        entity,
        payload,
    )
}

fn create(entity: Ulid, wall_ms: u64, actor: &str) -> Op {
    op(
        entity,
        wall_ms,
        actor,
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

fn number(entity: Ulid, wall_ms: u64, actor: &str, n: u64) -> Op {
    op(
        entity,
        wall_ms,
        actor,
        Payload::FieldSet(FieldSet::Number(n)),
    )
}

fn claim(entity: Ulid, wall_ms: u64, actor: &str, state: &str) -> Op {
    op(
        entity,
        wall_ms,
        actor,
        Payload::Claim(Claim {
            state: state.into(),
            assignee: ActorId::new(actor),
        }),
    )
}

fn assign(entity: Ulid, wall_ms: u64, actor: &str, to: Option<&str>) -> Op {
    op(
        entity,
        wall_ms,
        actor,
        Payload::FieldSet(FieldSet::Assignee(to.map(ActorId::new))),
    )
}

fn state_upsert(ws: Ulid, wall_ms: u64, name: &str, category: StateCategory, position: u32) -> Op {
    op(
        ws,
        wall_ms,
        "matt",
        Payload::StateUpsert(StateUpsert {
            name: name.into(),
            category,
            position,
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

fn push(port: u16, token: &str, ops: &[&Op]) -> (u16, Value) {
    let ops: Vec<Value> = ops
        .iter()
        .map(|o| serde_json::to_value(o).unwrap())
        .collect();
    post(
        port,
        token,
        "/w/saltline/ops",
        &json!({ "ops": ops }).to_string(),
    )
}

fn push_ok(port: u16, token: &str, ops: &[&Op]) -> Value {
    let (status, body) = push(port, token, ops);
    assert_eq!(status, 200, "{body}");
    for ack in body["ops"].as_array().unwrap() {
        assert!(ack.get("rejected").is_none(), "{body}");
    }
    body
}

fn mint(url: &str, name: &str, extra: &[&str]) -> String {
    let mut args = vec!["token", "create", name, "--workspace", "saltline"];
    args.extend_from_slice(extra);
    let (ok, stdout, stderr) = admin(url, &args);
    assert!(ok, "token create failed: {stderr}");
    stdout.trim().to_string()
}

fn count_ops(url: &str) -> i64 {
    query_rows(url, "SELECT count(*) FROM ops").unwrap()[0][0]
        .as_deref()
        .unwrap()
        .parse()
        .unwrap()
}

fn recent() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
        - 60_000
}

/// Seed numbers: a bound token's is refused (it would set the floor the
/// seed's end adopts, or take a number the real seed carries); the
/// unrestricted token's lands. Payload checks: a claim's state name, a
/// payload stamp and a payload's size.
#[test]
fn seed_numbers_need_an_unrestricted_token_and_payloads_are_checked() {
    let Some((_container, url)) =
        postgres_for("seed_numbers_need_an_unrestricted_token_and_payloads_are_checked")
    else {
        return;
    };
    let port = free_port();
    let mut hub = spawn_hub(&url, port);
    wait_for_health(&mut hub, port);
    let studio = mint(&url, "studio", &["--any"]);
    let agent = mint(&url, "agent", &["--actor", "claude:*"]);

    let (a, b) = (Ulid::new(), Ulid::new());
    push_ok(port, &agent, &[&create(a, 1_000, "claude:x")]);
    // The allocator-exhausting number, and an ordinary one, from a bound
    // token: refused whole, nothing stored.
    for n in [(1_u64 << 53) - 1, 7] {
        let before = count_ops(&url);
        let (status, err) = push(port, &agent, &[&number(a, 1_001, "claude:x", n)]);
        assert_eq!(status, 400, "{err}");
        assert_eq!(err["error"], "number_not_allowed", "{err}");
        assert_eq!(err["index"], 0);
        let reason = err["reason"].as_str().unwrap();
        assert!(
            reason.contains("(agent)") && reason.contains("unrestricted"),
            "{reason}"
        );
        assert_eq!(count_ops(&url), before);
    }
    let numbers = query_rows(&url, "SELECT count(*) FROM numbers").unwrap();
    assert_eq!(numbers[0][0].as_deref(), Some("0"));
    // The unrestricted token seeds numbers, as before.
    push_ok(
        port,
        &studio,
        &[
            &create(b, 1_002, "matt"),
            &number(a, 1_003, "matt", 7),
            &number(b, 1_004, "matt", 8),
        ],
    );
    let (status, body) = post(
        port,
        &studio,
        "/w/saltline/seeded",
        &json!({ "number_floor": 0 }).to_string(),
    );
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["number_floor"], 8);

    // A claim's state name is a path segment like any other state's.
    let t = recent();
    for bad in ["../../x", "a/b", "NUL", "C:x"] {
        let (status, err) = push(port, &studio, &[&claim(a, t, "matt", bad)]);
        assert_eq!(status, 400, "{bad}: {err}");
        assert_eq!(err["error"], "invalid_id", "{bad}: {err}");
        assert!(
            err["reason"].as_str().unwrap().contains("state name"),
            "{bad}: {err}"
        );
    }
    // Payload stamps: out of range, or more than a day past their op.
    let hold_at = |at: Hlc| {
        op(
            a,
            t,
            "matt",
            Payload::HoldSet(HoldSet {
                hold: Hold {
                    reason: "r".into(),
                    by: ActorId::new("matt"),
                    at,
                },
            }),
        )
    };
    let archived_at = |at: Hlc| {
        op(
            a,
            t,
            "matt",
            Payload::FieldSet(FieldSet::ArchivedAt(Some(at))),
        )
    };
    for (case, bad, want) in [
        (
            "hold.at out of range",
            hold_at(Hlc::new(u64::MAX, 0)),
            "invalid_stamp",
        ),
        (
            "hold.at spent counter",
            hold_at(Hlc::new(t, u32::MAX)),
            "invalid_stamp",
        ),
        (
            "hold.at two days ahead",
            hold_at(Hlc::new(t + 2 * MAX_FUTURE_SKEW_MS, 0)),
            "future_stamp",
        ),
        (
            "archived_at out of range",
            archived_at(Hlc::new(i64::MAX as u64 + 1, 0)),
            "invalid_stamp",
        ),
        (
            "archived_at far ahead",
            archived_at(Hlc::new(i64::MAX as u64, 0)),
            "future_stamp",
        ),
    ] {
        let (status, err) = push(port, &studio, &[&bad]);
        assert_eq!(status, 400, "{case}: {err}");
        assert_eq!(err["error"], want, "{case}: {err}");
        assert!(
            err["reason"].as_str().unwrap().starts_with("payload "),
            "{case}: {err}"
        );
    }
    // Stamped with the op itself (what pm writes), or an archive month
    // older than today: fine.
    push_ok(
        port,
        &studio,
        &[&hold_at(Hlc::new(t, 0)), &archived_at(Hlc::new(1_000, 0))],
    );
    // Any payload over the megabyte, not only a body.edit.
    let big = op(
        a,
        t,
        "matt",
        Payload::CommentAdd(CommentAdd {
            body: "x".repeat(pm_core::MAX_OP_PAYLOAD_BYTES),
        }),
    );
    let (status, err) = push(port, &studio, &[&big]);
    assert_eq!(status, 400, "{err}");
    assert_eq!(err["error"], "op_too_large", "{err}");
    assert!(
        err["reason"]
            .as_str()
            .unwrap()
            .contains("comment.add payload"),
        "{err}"
    );
}

/// Once seeded, a bound token's assignee write is judged like a claim: it
/// may take an unstarted, unassigned ticket, but not one someone else won
/// (the refusal is a claim's: `rejected`, nothing stored); restating the
/// current assignee — `pm sync`'s reconcile — and unassigning still land;
/// an unrestricted token reassigns freely.
#[test]
fn a_bound_tokens_assignee_write_cannot_bypass_claim_arbitration() {
    let Some((_container, url)) =
        postgres_for("a_bound_tokens_assignee_write_cannot_bypass_claim_arbitration")
    else {
        return;
    };
    let port = free_port();
    let mut hub = spawn_hub(&url, port);
    wait_for_health(&mut hub, port);
    let studio = mint(&url, "studio", &["--any"]);
    let agent = mint(&url, "agent", &["--actor", "claude:*"]);
    let ws = Ulid::new();
    push_ok(
        port,
        &studio,
        &[
            &state_upsert(ws, 1, "triage", StateCategory::Unstarted, 0),
            &state_upsert(ws, 2, "in-progress", StateCategory::Started, 1),
        ],
    );
    let (status, body) = post(
        port,
        &studio,
        "/w/saltline/seeded",
        &json!({ "number_floor": 0 }).to_string(),
    );
    assert_eq!(status, 200, "{body}");

    let t = recent();
    let won = Ulid::new();
    push_ok(port, &studio, &[&create(won, t, "matt")]);
    // claude:a wins the claim.
    push_ok(
        port,
        &agent,
        &[&claim(won, t + 1, "claude:a", "in-progress")],
    );

    // claude:b skips the claim and writes itself in, with a transition
    // riding along: the assignee write is refused like a losing claim;
    // the rest of the batch lands.
    let before = count_ops(&url);
    let takeover = assign(won, t + 2, "claude:b", Some("claude:b"));
    let along = op(
        won,
        t + 3,
        "claude:b",
        Payload::StateTransition(StateTransition {
            state: "in-progress".into(),
        }),
    );
    let (status, body) = push(port, &agent, &[&takeover, &along]);
    assert_eq!(status, 200, "{body}");
    let ack = &body["ops"][0];
    assert_eq!(ack["op_id"], takeover.op_id.to_string());
    assert_eq!(ack["seq"], Value::Null, "{body}");
    assert_eq!(ack["stored"], false, "{body}");
    assert_eq!(ack["rejected"]["taken_by"], "claude:a", "{body}");
    assert_eq!(ack["rejected"]["code"], "not_unstarted", "{body}");
    assert_eq!(ack["rejected"]["state"], "in-progress", "{body}");
    assert_eq!(body["ops"][1]["stored"], true, "{body}");
    assert_eq!(count_ops(&url), before + 1);

    // The reconcile write a client logs after that refusal restates the
    // winner: accepted from the bound token.
    push_ok(
        port,
        &agent,
        &[&assign(won, t + 4, "claude:b", Some("claude:a"))],
    );
    // The winner restating itself, and unassigning, land too.
    push_ok(
        port,
        &agent,
        &[&assign(won, t + 5, "claude:a", Some("claude:a"))],
    );

    // An unstarted, unassigned ticket: the bound token's assignee write is
    // admitted (it is a claim in all but state), and a later claim by
    // another agent then loses to it.
    let open = Ulid::new();
    push_ok(port, &studio, &[&create(open, t + 10, "matt")]);
    push_ok(
        port,
        &agent,
        &[&assign(open, t + 11, "claude:c", Some("claude:c"))],
    );
    let (status, body) = push(
        port,
        &agent,
        &[&claim(open, t + 12, "claude:d", "in-progress")],
    );
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        body["ops"][0]["rejected"]["code"], "already_assigned",
        "{body}"
    );
    assert_eq!(body["ops"][0]["rejected"]["taken_by"], "claude:c", "{body}");
    // ...nor may another bound actor overwrite it.
    let (status, body) = push(
        port,
        &agent,
        &[&assign(open, t + 13, "claude:d", Some("claude:d"))],
    );
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        body["ops"][0]["rejected"]["code"], "already_assigned",
        "{body}"
    );
    // Unassigning names nobody and is never arbitrated.
    push_ok(port, &agent, &[&assign(open, t + 14, "claude:c", None)]);

    // The operator's unrestricted token reassigns a won ticket freely.
    push_ok(
        port,
        &studio,
        &[&assign(won, t + 20, "matt", Some("claude:z"))],
    );
    let view = query_rows(
        &url,
        &format!(
            "SELECT view::json->'assignee'->>'value' FROM ticket_views WHERE entity = '{won}'"
        ),
    )
    .unwrap();
    assert_eq!(view[0][0].as_deref(), Some("claude:z"));
}
