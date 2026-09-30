//! End-to-end claim arbitration (AGT-1392): the hub's materialized views
//! equal a client's `pm doctor --rebuild` of the same ops (built with
//! pm-store, pushed as a seed, compared ticket by ticket); once seeded, of
//! 20 concurrent claims on one ticket exactly one is admitted and the rest
//! get the structured rejection with nothing stored; a re-pushed admitted
//! claim is idempotent; unclaim / done then claim again; config-defined
//! states are respected; a mixed batch stores everything but the refused
//! claim; and the migration backfills the views from an existing log. See
//! `common` for where Postgres comes from.

mod common;

use std::collections::{BTreeSet, HashSet};
use std::thread;

use common::*;
use pm_core::op::{
    Claim, CommentAdd, FieldSet, LabelAdd, StateTransition, StateUpsert, TicketCreate,
};
use pm_core::{ActorId, Hlc, Op, Payload, Priority, State, StateCategory, TicketView, Workspace};
use pm_store::Store;
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

fn create(entity: Ulid, wall_ms: u64, state: &str) -> Op {
    op(
        entity,
        wall_ms,
        "matt",
        Payload::TicketCreate(TicketCreate {
            title: "t".into(),
            state: state.into(),
            priority: Priority::Medium,
            project: None,
            repo: None,
            source: None,
            ext: Default::default(),
        }),
    )
}

fn claim(entity: Ulid, wall_ms: u64, actor: &str) -> Op {
    op(
        entity,
        wall_ms,
        actor,
        Payload::Claim(Claim {
            state: "in-progress".into(),
            assignee: ActorId::new(actor),
        }),
    )
}

fn transition(entity: Ulid, wall_ms: u64, actor: &str, state: &str) -> Op {
    op(
        entity,
        wall_ms,
        actor,
        Payload::StateTransition(StateTransition {
            state: state.into(),
        }),
    )
}

fn assignee(entity: Ulid, wall_ms: u64, actor: &str, to: Option<&str>) -> Op {
    op(
        entity,
        wall_ms,
        actor,
        Payload::FieldSet(FieldSet::Assignee(to.map(ActorId::new))),
    )
}

fn label(entity: Ulid, wall_ms: u64, label: &str) -> Op {
    op(
        entity,
        wall_ms,
        "matt",
        Payload::LabelAdd(LabelAdd {
            label: label.into(),
        }),
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

fn workspace() -> Workspace {
    Workspace {
        id: Ulid::new(),
        prefix: "AGT".into(),
        states: vec![
            State {
                name: "triage".into(),
                category: StateCategory::Unstarted,
                position: 0,
            },
            State {
                name: "in-progress".into(),
                category: StateCategory::Started,
                position: 1,
            },
            State {
                name: "done".into(),
                category: StateCategory::Completed,
                position: 2,
            },
        ],
        gate_labels: Default::default(),
        model_labels: Default::default(),
        template_sections: Vec::new(),
        stale_days: 30,
    }
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

/// Pushes and asserts every op was stored.
fn push_ok(port: u16, token: &str, ops: &[&Op]) -> Value {
    let (status, body) = push(port, token, ops);
    assert_eq!(status, 200, "{body}");
    for ack in body["ops"].as_array().unwrap() {
        assert_eq!(ack["stored"], true, "{body}");
        assert!(ack.get("rejected").is_none(), "{body}");
    }
    body
}

fn seeded(port: u16, token: &str, floor: u64) -> (u16, Value) {
    post(
        port,
        token,
        "/w/saltline/seeded",
        &json!({ "number_floor": floor }).to_string(),
    )
}

fn create_token(url: &str, name: &str, workspace: &str) -> String {
    let (ok, stdout, stderr) = admin(url, &["token", "create", name, "--workspace", workspace]);
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

/// The hub's ticket views for `saltline`, by entity.
fn hub_views(url: &str) -> Vec<(String, TicketView)> {
    let mut rows: Vec<(String, TicketView)> = query_rows(
        url,
        "SELECT entity, view FROM ticket_views WHERE workspace_id = 'saltline'",
    )
    .unwrap()
    .into_iter()
    .map(|r| {
        let entity = r[0].clone().unwrap();
        let view: TicketView = serde_json::from_str(r[1].as_deref().unwrap()).unwrap();
        assert_eq!(view.id.to_string(), entity);
        (entity, view)
    })
    .collect();
    rows.sort_by(|a, b| a.0.cmp(&b.0));
    rows
}

fn hub_view(url: &str, entity: Ulid) -> TicketView {
    hub_views(url)
        .into_iter()
        .find(|(e, _)| *e == entity.to_string())
        .unwrap_or_else(|| panic!("no hub view for {entity}"))
        .1
}

/// The fields the arbitration reads, plus the ones a client shows.
fn essentials(view: &TicketView) -> (String, Option<ActorId>, bool, Option<u64>, BTreeSet<String>) {
    let t = view.snapshot();
    (t.state, t.assignee, t.deleted, t.number, t.labels)
}

/// The hub's `workspace_views` states for `saltline`, sorted as
/// `Workspace::states` are.
fn hub_states(url: &str) -> Vec<State> {
    let rows = query_rows(
        url,
        "SELECT view FROM workspace_views WHERE workspace_id = 'saltline'",
    )
    .unwrap();
    assert_eq!(rows.len(), 1, "{rows:?}");
    let view: pm_core::WorkspaceView =
        serde_json::from_str(rows[0][0].as_deref().unwrap()).unwrap();
    view.snapshot().states
}

#[test]
fn hub_views_equal_a_client_rebuild_and_the_migration_backfills_them() {
    let Some((_container, url)) =
        postgres_for("hub_views_equal_a_client_rebuild_and_the_migration_backfills_them")
    else {
        return;
    };
    let port = free_port();
    let mut hub = spawn_hub(&url, port);
    wait_for_health(&mut hub, port);
    let studio = create_token(&url, "studio", "saltline");

    // A client's log, built the way `pm` builds one: config ops from
    // `init_workspace`, numbered creates, claims judged by the local
    // authority (the second claim on `a` is refused and never logged),
    // transitions, an unclaim, a tombstone, a label, a comment.
    let dir = tempfile::tempdir().unwrap();
    let mut store = Store::open(dir.path().join("pm.sqlite")).unwrap();
    let matt = ActorId::new("matt");
    store.init_workspace(&workspace(), &matt).unwrap();
    let (a, b, c) = (Ulid::new(), Ulid::new(), Ulid::new());
    let mut t = 1_790_000_000_000u64;
    let mut next = || {
        t += 1;
        t
    };
    store
        .commit_batch(&[create(a, next(), "triage")], &[(a, matt.clone())])
        .unwrap();
    store
        .commit_batch(&[create(b, next(), "triage")], &[(b, matt.clone())])
        .unwrap();
    store.commit(&create(c, next(), "triage")).unwrap();
    store.commit(&claim(a, next(), "alice")).unwrap();
    let refused = store.commit(&claim(a, next(), "bob"));
    assert!(
        matches!(refused, Err(pm_store::StoreError::ClaimRejected(_))),
        "{refused:?}"
    );
    store
        .commit(&transition(a, next(), "alice", "done"))
        .unwrap();
    store.commit(&claim(b, next(), "bob")).unwrap();
    store
        .commit(&transition(b, next(), "bob", "triage"))
        .unwrap();
    store.commit(&assignee(b, next(), "bob", None)).unwrap();
    store.commit(&claim(b, next(), "carol")).unwrap();
    store
        .commit(&op(c, next(), "matt", Payload::Tombstone))
        .unwrap();
    store.commit(&label(a, next(), "model:fable-5")).unwrap();
    store
        .commit(&op(
            b,
            next(),
            "bob",
            Payload::CommentAdd(CommentAdd { body: "hi".into() }),
        ))
        .unwrap();

    // Seed the hub with the whole outbox, in two batches, in log order.
    let outbox: Vec<Op> = store
        .outbox(10_000)
        .unwrap()
        .into_iter()
        .map(|(_, op)| op)
        .collect();
    assert!(outbox.len() > 8, "{}", outbox.len());
    let (first, second) = outbox.split_at(outbox.len() / 2);
    push_ok(port, &studio, &first.iter().collect::<Vec<_>>());
    push_ok(port, &studio, &second.iter().collect::<Vec<_>>());
    let (status, body) = seeded(port, &studio, 2);
    assert_eq!(status, 200, "{body}");
    // `c` was never numbered locally: the seed end numbers it (3), and
    // that hub op is in the view too. The client applies it as a pull
    // would.
    let hub_ops: Vec<Op> = body["numbers"]
        .as_array()
        .unwrap()
        .iter()
        .map(|n| serde_json::from_value(n["op"].clone()).unwrap())
        .collect();
    assert_eq!(hub_ops.len(), 1, "{body}");
    store.apply_pulled(&hub_ops).unwrap();

    // The client's rebuild and the hub's views agree, ticket by ticket.
    let diff = store.rebuild().unwrap();
    assert!(diff.is_empty(), "{diff:#?}");
    for id in [a, b, c] {
        let client = store.ticket(id).unwrap().unwrap();
        let hub = hub_view(&url, id).snapshot();
        assert_eq!(
            (
                &hub.state,
                &hub.assignee,
                hub.deleted,
                hub.number,
                &hub.labels,
                &hub.title,
                hub.created,
            ),
            (
                &client.state,
                &client.assignee,
                client.deleted,
                client.number,
                &client.labels,
                &client.title,
                client.created,
            ),
            "ticket {id}"
        );
    }
    assert_eq!(hub_view(&url, a).snapshot().state, "done");
    assert_eq!(
        hub_view(&url, b).snapshot().assignee,
        Some(ActorId::new("carol"))
    );
    assert!(hub_view(&url, c).snapshot().deleted);
    assert_eq!(hub_view(&url, c).snapshot().number, Some(3));
    assert_eq!(hub_states(&url), store.workspace().unwrap().unwrap().states);

    // Backfill: drop the views and their migration, restart, and the hub
    // rebuilds them from the log to the same state.
    let before: Vec<_> = hub_views(&url)
        .iter()
        .map(|(e, v)| (e.clone(), essentials(v)))
        .collect();
    let states_before = hub_states(&url);
    drop(hub);
    query_rows(
        &url,
        "DROP TABLE ticket_views; DROP TABLE workspace_views;
         DELETE FROM schema_version WHERE version = 3",
    )
    .unwrap();
    let mut hub = spawn_hub(&url, port);
    let (_, health) = wait_for_health(&mut hub, port);
    assert_eq!(health["schema_version"], 3);
    let after: Vec<_> = hub_views(&url)
        .iter()
        .map(|(e, v)| (e.clone(), essentials(v)))
        .collect();
    assert_eq!(after, before);
    assert_eq!(hub_states(&url), states_before);
}

#[test]
fn once_seeded_the_hub_arbitrates_claims_first_come() {
    let Some((_container, url)) = postgres_for("once_seeded_the_hub_arbitrates_claims_first_come")
    else {
        return;
    };
    let port = free_port();
    let mut hub = spawn_hub(&url, port);
    wait_for_health(&mut hub, port);
    let studio = create_token(&url, "studio", "saltline");
    let ws = Ulid::new();
    let config = [
        state_upsert(ws, 1, "triage", StateCategory::Unstarted, 0),
        state_upsert(ws, 2, "in-progress", StateCategory::Started, 1),
        state_upsert(ws, 3, "done", StateCategory::Completed, 2),
    ];
    push_ok(port, &studio, &config.iter().collect::<Vec<_>>());

    // Seed mode: claims are history; both land, the later one holds.
    let history = Ulid::new();
    let body = push_ok(
        port,
        &studio,
        &[
            &create(history, 10, "triage"),
            &claim(history, 20, "alice"),
            &claim(history, 30, "bob"),
        ],
    );
    assert_eq!(body["ops"].as_array().unwrap().len(), 3);
    assert_eq!(
        hub_view(&url, history).snapshot().assignee,
        Some(ActorId::new("bob"))
    );
    let (status, body) = seeded(port, &studio, 0);
    assert_eq!(status, 200, "{body}");

    // Authoritative: 20 concurrent claims on one ticket, one winner.
    let t = Ulid::new();
    push_ok(port, &studio, &[&create(t, 100, "triage")]);
    let before = count_ops(&url);
    let claims: Vec<Op> = (0..20)
        .map(|i| claim(t, 200 + i, &format!("agent-{i}")))
        .collect();
    let results: Vec<(Op, Value)> = thread::scope(|s| {
        let handles: Vec<_> = claims
            .iter()
            .map(|c| {
                let studio = studio.clone();
                s.spawn(move || {
                    let (status, body) = push(port, &studio, &[c]);
                    assert_eq!(status, 200, "{body}");
                    (c.clone(), body)
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });
    let admitted: Vec<&(Op, Value)> = results
        .iter()
        .filter(|(_, body)| body["ops"][0]["stored"] == true)
        .collect();
    assert_eq!(admitted.len(), 1, "{results:?}");
    let (winner, winner_body) = admitted[0];
    assert!(winner_body["ops"][0]["seq"].is_i64(), "{winner_body}");
    assert_eq!(winner_body["ops"][0]["op_id"], winner.op_id.to_string());
    assert!(winner_body["ops"][0].get("rejected").is_none());
    for (c, body) in &results {
        if c.op_id == winner.op_id {
            continue;
        }
        let ack = &body["ops"][0];
        assert_eq!(ack["op_id"], c.op_id.to_string());
        assert_eq!(ack["seq"], Value::Null, "{body}");
        assert_eq!(ack["stored"], false, "{body}");
        let rejected = &ack["rejected"];
        assert_eq!(rejected["taken_by"], winner.actor.to_string(), "{body}");
        assert_eq!(
            rejected["at"],
            json!({"wall_ms": winner.hlc.wall_ms, "counter": winner.hlc.counter}),
            "{body}"
        );
        assert_eq!(rejected["state"], "in-progress");
        assert_eq!(rejected["code"], "not_unstarted");
        assert_eq!(
            rejected["reason"],
            "ticket is in state 'in-progress', which is not unstarted"
        );
    }
    assert_eq!(count_ops(&url), before + 1, "only the winner was stored");
    assert_eq!(
        hub_view(&url, t).snapshot().assignee,
        Some(winner.actor.clone())
    );
    let winner_seq = winner_body["ops"][0]["seq"].as_i64().unwrap();

    // A re-push of the admitted claim is idempotent: same seq, nothing
    // stored, not re-arbitrated.
    let (status, body) = push(port, &studio, &[winner]);
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["ops"][0]["seq"], winner_seq);
    assert_eq!(body["ops"][0]["stored"], false);
    assert!(body["ops"][0].get("rejected").is_none(), "{body}");
    assert_eq!(count_ops(&url), before + 1);

    // A mixed batch: the refused claim stores nothing, the rest lands,
    // and the batch's seqs stay contiguous around the hole.
    let loser = claim(t, 300, "late");
    let tag = label(t, 301, "x");
    let (status, body) = push(port, &studio, &[&loser, &tag]);
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["ops"][0]["seq"], Value::Null);
    assert_eq!(body["ops"][0]["rejected"]["code"], "not_unstarted");
    assert_eq!(body["ops"][1]["stored"], true);
    assert_eq!(count_ops(&url), before + 2);
    let hub = hub_view(&url, t).snapshot();
    assert_eq!(hub.labels, BTreeSet::from(["x".to_string()]));
    assert_eq!(hub.assignee, Some(winner.actor.clone()));
    // The refused op is unknown to the log: a pull never serves it.
    let logged: Vec<String> = query_rows(&url, "SELECT op_id FROM ops")
        .unwrap()
        .into_iter()
        .map(|r| r[0].clone().unwrap())
        .collect();
    assert!(!logged.contains(&loser.op_id.to_string()));

    // Unclaim (back to triage, unassigned): claimable again. The claim
    // is stamped after the unclaim, as a client's clock would stamp it:
    // the hub admits a claim, and pm-core's LWW decides what its write
    // does — an admitted claim stamped before the unclaim would land in
    // the log and lose the register, on the hub as on every client.
    push_ok(
        port,
        &studio,
        &[
            &transition(t, 400, &winner.actor.to_string(), "triage"),
            &assignee(t, 401, &winner.actor.to_string(), None),
        ],
    );
    let body = push_ok(port, &studio, &[&claim(t, 450, "late")]);
    assert!(body["ops"][0]["seq"].is_i64());
    assert_eq!(
        hub_view(&url, t).snapshot().assignee,
        Some(ActorId::new("late"))
    );
    // Done: not unstarted, still assigned — a claim is refused and the
    // loser learns who holds it and since when.
    let done = transition(t, 500, "late", "done");
    push_ok(port, &studio, &[&done]);
    let (status, body) = push(port, &studio, &[&claim(t, 600, "next")]);
    assert_eq!(status, 200, "{body}");
    let rejected = &body["ops"][0]["rejected"];
    assert_eq!(rejected["taken_by"], "late");
    assert_eq!(rejected["state"], "done");
    assert_eq!(rejected["code"], "not_unstarted");
    assert_eq!(rejected["at"]["wall_ms"], 500);

    // Config-defined states: a claim in a state the workspace's ops call
    // unstarted is admitted; one in a state they do not know is not.
    let refined = Ulid::new();
    let blocked = Ulid::new();
    push_ok(
        port,
        &studio,
        &[
            &state_upsert(ws, 700, "refined", StateCategory::Unstarted, 5),
            &create(refined, 701, "refined"),
            &create(blocked, 702, "blocked"),
        ],
    );
    let (status, body) = push(
        port,
        &studio,
        &[&claim(refined, 800, "r"), &claim(blocked, 801, "b")],
    );
    assert_eq!(status, 200, "{body}");
    assert!(body["ops"][0]["seq"].is_i64(), "{body}");
    assert_eq!(
        body["ops"][1]["rejected"]["code"], "not_unstarted",
        "{body}"
    );
    assert_eq!(body["ops"][1]["rejected"]["taken_by"], Value::Null);
    assert_eq!(body["ops"][1]["rejected"]["state"], "blocked");
    assert_eq!(
        body["ops"][1]["rejected"]["reason"],
        "ticket is in state 'blocked', which is not unstarted"
    );

    // A deleted ticket refuses with `deleted`.
    push_ok(
        port,
        &studio,
        &[&op(blocked, 900, "matt", Payload::Tombstone)],
    );
    let (_, body) = push(port, &studio, &[&claim(blocked, 901, "b")]);
    assert_eq!(body["ops"][0]["rejected"]["code"], "deleted", "{body}");

    // Config for another workspace Ulid is refused whole.
    let (status, body) = push(
        port,
        &studio,
        &[
            &label(t, 950, "never"),
            &state_upsert(Ulid::new(), 951, "x", StateCategory::Unstarted, 9),
        ],
    );
    assert_eq!(status, 400, "{body}");
    assert_eq!(body["error"], "foreign_workspace");
    assert_eq!(body["index"], 1);
    assert!(!hub_view(&url, t).snapshot().labels.contains("never"));

    // Every stored op was folded: the views' entities are exactly the
    // log's ticket entities.
    let entities: HashSet<String> = hub_views(&url).into_iter().map(|(e, _)| e).collect();
    assert_eq!(
        entities,
        [history, t, refined, blocked]
            .iter()
            .map(|u| u.to_string())
            .collect()
    );
}
