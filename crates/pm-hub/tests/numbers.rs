//! End-to-end ticket-number allocation (AGT-1391): seed mode accepts a
//! client's numbers and allocates none; `POST /seeded` sets the floor,
//! numbers the stragglers and makes the hub the authority; after that
//! every pushed create is numbered in the push's transaction, a pushed
//! number is refused, a re-push allocates nothing, and 100 concurrent
//! pushes get 100 distinct contiguous numbers. See `common` for where
//! Postgres comes from.

mod common;

use std::collections::HashSet;
use std::thread;

use common::*;
use pm_core::op::{LabelAdd, TicketCreate};
use pm_core::{ActorId, Hlc, Op, Payload, Priority, TicketView, apply};
use serde_json::{Value, json};
use ulid::Ulid;

fn op(entity: Ulid, wall_ms: u64, payload: Payload) -> Op {
    Op::new(
        Ulid::new(),
        Hlc::new(wall_ms, 0),
        ActorId::new("studio"),
        entity,
        payload,
    )
}

fn create(entity: Ulid, wall_ms: u64) -> Op {
    op(
        entity,
        wall_ms,
        Payload::TicketCreate(TicketCreate {
            title: "t".into(),
            state: "triage".into(),
            priority: Priority::Medium,
            project: Some("pm".into()),
            repo: None,
            source: None,
            ext: Default::default(),
        }),
    )
}

fn number(entity: Ulid, wall_ms: u64, number: u64) -> Op {
    op(
        entity,
        wall_ms,
        Payload::FieldSet(pm_core::op::FieldSet::Number(number)),
    )
}

fn label(entity: Ulid, wall_ms: u64) -> Op {
    op(
        entity,
        wall_ms,
        Payload::LabelAdd(LabelAdd {
            label: "l".to_string(),
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

fn seeded(port: u16, token: &str, floor: u64) -> (u16, Value) {
    post(
        port,
        token,
        "/w/saltline/seeded",
        &json!({ "number_floor": floor }).to_string(),
    )
}

fn whoami_seeded(port: u16, token: &str) -> bool {
    let resp = request(
        port,
        "GET",
        "/w/saltline/whoami",
        &[&format!("Authorization: Bearer {token}")],
    );
    assert_eq!(resp.status, 200, "{resp:?}");
    let json: Value = serde_json::from_str(&resp.body).unwrap();
    json["seeded"]
        .as_bool()
        .unwrap_or_else(|| panic!("no seeded in {json}"))
}

/// `(entity, number, seq, allocated, op)` per entry of a `numbers` array.
fn numbers(body: &Value) -> Vec<(String, i64, i64, bool, Op)> {
    body["numbers"]
        .as_array()
        .unwrap_or_else(|| panic!("no numbers in {body}"))
        .iter()
        .map(|n| {
            (
                n["entity"].as_str().unwrap().to_string(),
                n["number"].as_i64().unwrap(),
                n["seq"].as_i64().unwrap(),
                n["allocated"].as_bool().unwrap(),
                serde_json::from_value(n["op"].clone())
                    .unwrap_or_else(|e| panic!("{} is not a pm op: {e}", n["op"])),
            )
        })
        .collect()
}

fn seqs(body: &Value) -> Vec<(i64, bool)> {
    body["ops"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| (a["seq"].as_i64().unwrap(), a["stored"].as_bool().unwrap()))
        .collect()
}

fn count(url: &str, sql: &str) -> i64 {
    query_rows(url, sql).unwrap()[0][0]
        .as_deref()
        .unwrap()
        .parse()
        .unwrap()
}

fn create_token(url: &str, name: &str, workspace: &str) -> String {
    let (ok, stdout, stderr) = admin(
        url,
        &["token", "create", name, "--workspace", workspace, "--any"],
    );
    assert!(ok, "token create failed: {stderr}");
    stdout.trim().to_string()
}

/// The hub's number op for `entity`, as stored: `(seq, actor, op)`.
fn stored_number_op(url: &str, entity: Ulid) -> (i64, String, Op) {
    let rows = query_rows(
        url,
        &format!(
            "SELECT seq, actor, op::text FROM ops
             WHERE entity = '{entity}' AND kind = 'field.set' ORDER BY seq"
        ),
    )
    .unwrap();
    assert_eq!(rows.len(), 1, "{rows:?}");
    let row = &rows[0];
    (
        row[0].as_deref().unwrap().parse().unwrap(),
        row[1].clone().unwrap(),
        serde_json::from_str(row[2].as_deref().unwrap()).unwrap(),
    )
}

#[test]
fn seed_accepts_client_numbers_then_the_hub_allocates_from_the_floor() {
    let Some((_container, url)) =
        postgres_for("seed_accepts_client_numbers_then_the_hub_allocates_from_the_floor")
    else {
        return;
    };
    let port = free_port();
    let mut hub = spawn_hub(&url, port);
    wait_for_health(&mut hub, port);
    let studio = create_token(&url, "studio", "saltline");
    let other = create_token(&url, "elsewhere", "other");
    assert!(!whoami_seeded(port, &studio), "a new workspace is seeding");

    // --- seed mode: the Studio's log carries its own numbers ---
    let (a, b, c) = (Ulid::new(), Ulid::new(), Ulid::new());
    let a_create = create(a, 100);
    let a_number = number(a, 101, 1300);
    let b_create = create(b, 200);
    let b_number = number(b, 201, 1376);
    // c was created after `pm hub login`: pending a hub number, so the
    // seed has no number op for it.
    let c_create = create(c, 300);
    let seed = [&a_create, &a_number, &b_create, &b_number, &c_create];
    let (status, body) = push(port, &studio, &seed);
    assert_eq!(status, 200, "{body}");
    let acked = seqs(&body);
    assert!(acked.iter().all(|s| s.1), "{acked:?}");
    let reported = numbers(&body);
    assert_eq!(reported.len(), 2, "{body}");
    assert_eq!(reported[0].0, a.to_string());
    assert_eq!(
        (reported[0].1, reported[0].2, reported[0].3),
        (1300, acked[1].0, false)
    );
    assert_eq!(reported[0].4, a_number, "the seeded op is served back");
    assert_eq!((reported[1].1, reported[1].3), (1376, false));
    assert_eq!(count(&url, "SELECT count(*) FROM ops"), 5, "no hub op yet");
    assert_eq!(count(&url, "SELECT count(*) FROM numbers"), 2);

    // A seeded number may not collide with one already issued, nor
    // re-number a ticket; the batch is refused whole and names the op.
    let d = Ulid::new();
    let d_create = create(d, 400);
    let d_number = number(d, 401, 1376);
    let (status, err) = push(port, &studio, &[&d_create, &d_number]);
    assert_eq!(status, 400, "{err}");
    assert_eq!(err["error"], "duplicate_number");
    assert_eq!(err["index"], 1);
    assert_eq!(err["op_id"], d_number.op_id.to_string());
    assert!(err["reason"].as_str().unwrap().contains("1376"), "{err}");
    let a_again = number(a, 102, 5);
    let (status, err) = push(port, &studio, &[&a_again]);
    assert_eq!(status, 400, "{err}");
    assert_eq!(err["error"], "duplicate_number");
    assert!(err["reason"].as_str().unwrap().contains("1300"), "{err}");
    // Two tickets claiming one number inside a batch, too.
    let (e, f) = (Ulid::new(), Ulid::new());
    let (status, err) = push(port, &studio, &[&number(e, 1, 9), &number(f, 2, 9)]);
    assert_eq!(status, 400, "{err}");
    assert_eq!(err["error"], "duplicate_number");
    assert_eq!(err["index"], 1);
    assert_eq!(count(&url, "SELECT count(*) FROM ops"), 5);
    assert_eq!(count(&url, "SELECT count(*) FROM numbers"), 2);

    // Re-pushing the seed stores nothing and still reports the numbers.
    let (status, body) = push(port, &studio, &seed);
    assert_eq!(status, 200, "{body}");
    assert!(seqs(&body).iter().all(|s| !s.1), "{body}");
    let again = numbers(&body);
    assert_eq!(
        again.iter().map(|n| (n.1, n.2)).collect::<Vec<_>>(),
        reported.iter().map(|n| (n.1, n.2)).collect::<Vec<_>>()
    );

    // --- the seed ends: floor, stragglers, authority ---
    let (status, err) = post(port, &studio, "/w/saltline/seeded", "{}");
    assert_eq!(status, 400, "{err}");
    assert_eq!(err["error"], "invalid_body");
    let unknown = request(port, "GET", "/no/such/route", &[]);
    let resp = request_body(
        port,
        "POST",
        "/w/saltline/seeded",
        &[&format!("Authorization: Bearer {other}")],
        br#"{"number_floor": 1}"#,
    );
    assert_eq!(resp, unknown, "another workspace's token");
    assert!(!whoami_seeded(port, &studio));

    // The client's floor (1300) is below the largest seeded number
    // (1376): the hub adopts 1376 and numbers c right after it.
    let (status, body) = seeded(port, &studio, 1300);
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["number_floor"], 1376);
    let stragglers = numbers(&body);
    assert_eq!(stragglers.len(), 1, "{body}");
    let (entity, n, seq, allocated, hub_op) = &stragglers[0];
    assert_eq!(
        (entity.as_str(), *n, *allocated),
        (c.to_string().as_str(), 1377, true)
    );
    assert_eq!(*seq, 6, "appended after the seed");
    assert_eq!(hub_op.actor.as_str(), "hub");
    assert_eq!(hub_op.entity, c);
    assert!(
        hub_op.hlc > c_create.hlc,
        "{} after {}",
        hub_op.hlc,
        c_create.hlc
    );
    let (stored_seq, actor, stored) = stored_number_op(&url, c);
    assert_eq!((stored_seq, actor.as_str()), (6, "hub"));
    assert_eq!(&stored, hub_op);
    assert!(whoami_seeded(port, &studio));
    let row = &query_rows(
        &url,
        "SELECT number_floor, next_number, seeded_at IS NOT NULL FROM workspaces
         WHERE id = 'saltline'",
    )
    .unwrap()[0];
    assert_eq!(
        row.iter()
            .map(|c| c.as_deref().unwrap())
            .collect::<Vec<_>>(),
        ["1376", "1378", "t"]
    );
    let (status, err) = seeded(port, &studio, 1300);
    assert_eq!(status, 409, "{err}");
    assert_eq!(err["error"], "already_seeded");

    // --- authoritative: only the hub numbers tickets ---
    let g = Ulid::new();
    let g_create = create(g, 500);
    let g_number = number(g, 501, 1378);
    let (status, err) = push(port, &studio, &[&g_create, &g_number]);
    assert_eq!(status, 400, "{err}");
    assert_eq!(err["error"], "number_not_allowed");
    assert_eq!(err["index"], 1);
    assert_eq!(err["op_id"], g_number.op_id.to_string());
    assert_eq!(count(&url, "SELECT count(*) FROM ops"), 6, "nothing stored");
    // Nor may a client speak as the hub.
    let mut forged = number(g, 502, 1378);
    forged.actor = ActorId::new("hub");
    let (status, err) = push(port, &studio, &[&forged]);
    assert_eq!(status, 400, "{err}");
    assert_eq!(err["error"], "number_not_allowed");

    // A numberless create is numbered in the same push: the hub's op
    // follows the batch (contiguous seqs), is stamped after every op in
    // it, and comes back in `numbers` so the client need not wait for a
    // pull.
    let g_label = label(g, 9_000);
    let (status, body) = push(port, &studio, &[&g_create, &g_label]);
    assert_eq!(status, 200, "{body}");
    let acked = seqs(&body);
    assert_eq!(acked, [(7, true), (8, true)]);
    let got = numbers(&body);
    assert_eq!(got.len(), 1, "{body}");
    let (entity, n, seq, allocated, hub_op) = &got[0];
    assert_eq!(
        (entity.as_str(), *n, *seq, *allocated),
        (g.to_string().as_str(), 1378, 9, true)
    );
    assert_eq!(hub_op.actor.as_str(), "hub");
    assert!(
        hub_op.hlc > g_label.hlc,
        "{} after {}",
        hub_op.hlc,
        g_label.hlc
    );
    assert_eq!(hub_op.kind(), "field.set");
    let (stored_seq, actor, stored) = stored_number_op(&url, g);
    assert_eq!((stored_seq, actor.as_str()), (9, "hub"));
    assert_eq!(&stored, hub_op);
    assert_eq!(count(&url, "SELECT count(*) FROM ops"), 9);

    // pm-core folds the create and the hub's op into the numbered ticket.
    let mut view = TicketView::new(g);
    apply(&mut view, &g_create).unwrap();
    apply(&mut view, hub_op).unwrap();
    let ticket = view.snapshot();
    assert_eq!(ticket.number, Some(1378));
    assert_eq!(ticket.title, "t");

    // Re-pushing the create allocates nothing: same number, same op.
    let (status, body) = push(port, &studio, &[&g_create]);
    assert_eq!(status, 200, "{body}");
    assert_eq!(seqs(&body), [(7, false)]);
    let again = numbers(&body);
    assert_eq!(again.len(), 1);
    assert_eq!((again[0].1, again[0].2, again[0].3), (1378, 9, false));
    assert_eq!(&again[0].4, hub_op);
    assert_eq!(count(&url, "SELECT count(*) FROM ops"), 9);
    assert_eq!(count(&url, "SELECT count(*) FROM numbers"), 4);
    let next: i64 = count(
        &url,
        "SELECT next_number FROM workspaces WHERE id = 'saltline'",
    );
    assert_eq!(next, 1379);

    // Two creates in one batch: numbered in batch order, both ops after
    // the batch; a non-create batch reports no numbers.
    let (h, i) = (Ulid::new(), Ulid::new());
    let (status, body) = push(
        port,
        &studio,
        &[&create(h, 600), &label(h, 601), &create(i, 602)],
    );
    assert_eq!(status, 200, "{body}");
    assert_eq!(seqs(&body), [(10, true), (11, true), (12, true)]);
    let got = numbers(&body);
    assert_eq!(
        got.iter()
            .map(|n| (n.0.as_str(), n.1, n.2))
            .collect::<Vec<_>>(),
        [
            (h.to_string().as_str(), 1379, 13),
            (i.to_string().as_str(), 1380, 14)
        ]
    );
    assert!(got[0].4.hlc < got[1].4.hlc);
    let (status, body) = push(port, &studio, &[&label(h, 700)]);
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["numbers"], json!([]));
}

#[test]
fn concurrent_pushes_never_issue_a_number_twice() {
    let Some((_container, url)) = postgres_for("concurrent_pushes_never_issue_a_number_twice")
    else {
        return;
    };
    let port = free_port();
    let mut hub = spawn_hub(&url, port);
    wait_for_health(&mut hub, port);
    let token = create_token(&url, "studio", "saltline");
    // An empty seed: the floor is the client's.
    let (status, body) = seeded(port, &token, 500);
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["number_floor"], 500);
    assert_eq!(body["numbers"], json!([]));

    const PUSHERS: usize = 100;
    let creates: Vec<Op> = (0..PUSHERS)
        .map(|i| create(Ulid::new(), 1_000 + i as u64))
        .collect();
    let results: Vec<(Ulid, i64, i64, i64)> = thread::scope(|s| {
        let handles: Vec<_> = creates
            .iter()
            .map(|c| {
                let token = &token;
                s.spawn(move || {
                    let (status, body) = push(port, token, &[c]);
                    assert_eq!(status, 200, "{body}");
                    let acked = seqs(&body);
                    assert_eq!(acked.len(), 1);
                    let got = numbers(&body);
                    assert_eq!(got.len(), 1, "{body}");
                    assert_eq!(got[0].0, c.entity.to_string());
                    assert!(got[0].3);
                    assert_eq!(got[0].4.entity, c.entity);
                    (c.entity, got[0].1, acked[0].0, got[0].2)
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });

    // 100 distinct numbers, contiguous from the floor.
    let mut issued: Vec<i64> = results.iter().map(|r| r.1).collect();
    issued.sort_unstable();
    assert_eq!(issued, (501..=600).collect::<Vec<_>>());
    // Each push's hub op directly follows its create: pushes to one
    // workspace run one at a time, allocation included.
    for (entity, _, create_seq, number_seq) in &results {
        assert_eq!(*number_seq, create_seq + 1, "{entity}");
    }
    let mut all_seqs: Vec<i64> = results.iter().flat_map(|r| [r.2, r.3]).collect();
    all_seqs.sort_unstable();
    assert_eq!(all_seqs, (1..=2 * PUSHERS as i64).collect::<Vec<_>>());

    // The database agrees.
    assert_eq!(count(&url, "SELECT count(*) FROM ops"), 2 * PUSHERS as i64);
    assert_eq!(
        count(&url, "SELECT count(DISTINCT number) FROM numbers"),
        PUSHERS as i64
    );
    assert_eq!(
        count(
            &url,
            "SELECT count(*) FROM ops WHERE actor = 'hub' AND kind = 'field.set'"
        ),
        PUSHERS as i64
    );
    let stored: HashSet<i64> = query_rows(
        &url,
        "SELECT (op->'payload'->>'value')::bigint FROM ops WHERE actor = 'hub'",
    )
    .unwrap()
    .into_iter()
    .map(|r| r[0].as_deref().unwrap().parse().unwrap())
    .collect();
    assert_eq!(stored, issued.into_iter().collect::<HashSet<_>>());
    assert_eq!(
        count(
            &url,
            "SELECT next_number FROM workspaces WHERE id = 'saltline'"
        ),
        601
    );
}
