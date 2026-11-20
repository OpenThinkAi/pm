//! End-to-end redaction at the hub (AGT-2427): a pushed `redact`
//! rewrites its stored targets and the ticket view, in either order; an
//! actor-bound token may erase only ops it could have written; and pulls
//! serve `redact` ops only to clients that ask for them, while still
//! moving an older client's cursor past them. See `common` for where
//! Postgres comes from.

mod common;

use common::*;
use pm_core::op::{CommentAdd, HoldSet, Redact, TicketCreate};
use pm_core::{ActorId, Hlc, Hold, Op, Payload, Priority, REDACTED, TicketView};
use serde_json::{Value, json};
use ulid::Ulid;

const SECRET: &str = "hunter2-secret";

fn recent() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
        - 60_000
}

fn on(ticket: Ulid, actor: &str, wall_ms: u64, payload: Payload) -> Op {
    Op::new(
        Ulid::new(),
        Hlc::new(wall_ms, unique_counter(wall_ms, actor)),
        ActorId::new(actor),
        ticket,
        payload,
    )
}

fn comment(ticket: Ulid, actor: &str, wall_ms: u64, body: &str) -> Op {
    on(
        ticket,
        actor,
        wall_ms,
        Payload::CommentAdd(CommentAdd { body: body.into() }),
    )
}

fn redact(ticket: Ulid, actor: &str, wall_ms: u64, targets: &[&Op]) -> Op {
    on(
        ticket,
        actor,
        wall_ms,
        Payload::Redact(Redact {
            targets: targets.iter().map(|o| o.op_id).collect(),
        }),
    )
}

fn push(port: u16, token: &str, ops: &[&Op]) -> (u16, Value) {
    let ops: Vec<Value> = ops
        .iter()
        .map(|o| serde_json::to_value(o).unwrap())
        .collect();
    let resp = request_body(
        port,
        "POST",
        "/w/saltline/ops",
        &[&format!("Authorization: Bearer {token}")],
        json!({ "ops": ops }).to_string().as_bytes(),
    );
    let body: Value = serde_json::from_str(&resp.body)
        .unwrap_or_else(|e| panic!("{} body {:?} is not JSON: {e}", resp.status, resp.body));
    (resp.status, body)
}

fn pull(port: u16, token: &str, query: &str, accept: bool) -> Value {
    let auth = format!("Authorization: Bearer {token}");
    let mut headers = vec![auth.as_str()];
    if accept {
        headers.push("Pm-Accept-Ops: redact");
    }
    let resp = request(port, "GET", &format!("/w/saltline/ops{query}"), &headers);
    assert_eq!(resp.status, 200, "{}", resp.body);
    serde_json::from_str(&resp.body).unwrap()
}

fn kinds(page: &Value) -> Vec<String> {
    page["ops"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["op"]["kind"].as_str().unwrap().to_string())
        .collect()
}

fn mint(url: &str, name: &str, extra: &[&str]) -> String {
    let mut args = vec!["token", "create", name, "--workspace", "saltline"];
    args.extend_from_slice(extra);
    let (ok, stdout, stderr) = admin(url, &args);
    assert!(ok, "token create failed: {stderr}");
    stdout.trim().to_string()
}

fn stored_op(url: &str, op_id: Ulid) -> Op {
    let rows = query_rows(
        url,
        &format!("SELECT op::text FROM ops WHERE op_id = '{op_id}'"),
    )
    .unwrap();
    serde_json::from_str(rows[0][0].as_deref().unwrap()).unwrap()
}

fn hub_view(url: &str, entity: Ulid) -> TicketView {
    let rows = query_rows(
        url,
        &format!("SELECT view FROM ticket_views WHERE entity = '{entity}'"),
    )
    .unwrap();
    serde_json::from_str(rows[0][0].as_deref().unwrap()).unwrap()
}

fn everything(url: &str) -> String {
    let ops = query_rows(url, "SELECT op::text FROM ops").unwrap();
    let views = query_rows(url, "SELECT view FROM ticket_views").unwrap();
    ops.into_iter()
        .chain(views)
        .flatten()
        .flatten()
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn the_hub_redacts_stored_ops_and_gates_who_may() {
    let Some((_container, url)) = postgres_for("the_hub_redacts_stored_ops_and_gates_who_may")
    else {
        return;
    };
    let port = free_port();
    let mut hub = spawn_hub(&url, port);
    wait_for_health(&mut hub, port);
    let studio = mint(&url, "studio", &["--any"]);
    let agent = mint(&url, "agent", &["--actor", "claude:*"]);
    let t = recent();
    let ticket = Ulid::new();
    let create = on(
        ticket,
        "matt",
        t,
        Payload::TicketCreate(TicketCreate {
            title: format!("title {SECRET}"),
            state: "triage".into(),
            priority: Priority::High,
            project: None,
            repo: None,
            source: None,
            ext: Default::default(),
        }),
    );
    let matt_comment = comment(ticket, "matt", t + 1, &format!("matt {SECRET}"));
    let agent_comment = comment(ticket, "claude:x", t + 2, &format!("agent {SECRET}"));
    let hold = on(
        ticket,
        "matt",
        t + 3,
        Payload::HoldSet(HoldSet {
            hold: Hold {
                reason: format!("hold {SECRET}"),
                by: ActorId::new("matt"),
                at: Hlc::new(t + 3, 0),
            },
        }),
    );
    let (status, body) = push(port, &studio, &[&create, &matt_comment, &hold]);
    assert_eq!(status, 200, "{body}");
    let (status, body) = push(port, &agent, &[&agent_comment]);
    assert_eq!(status, 200, "{body}");

    // A malformed redact is refused for everyone.
    let (status, body) = push(port, &studio, &[&redact(ticket, "matt", t + 4, &[])]);
    assert_eq!(
        (status, body["error"].as_str()),
        (400, Some("invalid_redact"))
    );

    // A bound token may not erase another actor's comment, or an op the
    // hub does not hold; nothing is stored.
    let before = query_rows(&url, "SELECT count(*) FROM ops").unwrap();
    let ghost = comment(ticket, "claude:x", t + 5, "never pushed");
    for target in [&matt_comment, &ghost] {
        let (status, body) = push(
            port,
            &agent,
            &[&redact(ticket, "claude:x", t + 6, &[target])],
        );
        assert_eq!(status, 400, "{body}");
        assert_eq!(body["error"], "redact_not_allowed", "{body}");
    }
    assert_eq!(
        query_rows(&url, "SELECT count(*) FROM ops").unwrap(),
        before
    );

    // Its own comment it may.
    let own = redact(ticket, "claude:x", t + 7, &[&agent_comment]);
    let (status, body) = push(port, &agent, &[&own]);
    assert_eq!(status, 200, "{body}");
    let held = stored_op(&url, agent_comment.op_id);
    assert_eq!(held, agent_comment.redacted().unwrap());
    let view = hub_view(&url, ticket);
    assert_eq!(view.comments.iter().count(), 2);
    assert!(view.comments.iter().any(|c| c.body == REDACTED));

    // An unrestricted token erases the rest: title, comment, hold reason.
    let rest = redact(ticket, "matt", t + 8, &[&create, &matt_comment, &hold]);
    let (status, body) = push(port, &studio, &[&rest]);
    assert_eq!(status, 200, "{body}");
    let view = hub_view(&url, ticket);
    assert_eq!(view.title.value, REDACTED);
    assert_eq!(view.priority.value, Priority::High);
    assert_eq!(view.hold.value.as_ref().unwrap().reason, REDACTED);
    assert!(!everything(&url).contains(SECRET), "{}", everything(&url));

    // A redact stored before its target: the target is stored redacted.
    let late = comment(ticket, "matt", t + 9, &format!("late {SECRET}"));
    let early = redact(ticket, "matt", t + 10, &[&late]);
    let (status, body) = push(port, &studio, &[&early]);
    assert_eq!(status, 200, "{body}");
    let (status, body) = push(port, &studio, &[&late]);
    assert_eq!(status, 200, "{body}");
    assert_eq!(stored_op(&url, late.op_id), late.redacted().unwrap());
    assert!(!everything(&url).contains(SECRET));
    // A redact and its target in one batch.
    let same = comment(ticket, "claude:y", t + 11, &format!("same {SECRET}"));
    let both = redact(ticket, "claude:y", t + 12, &[&same]);
    let (status, body) = push(port, &agent, &[&same, &both]);
    assert_eq!(status, 200, "{body}");
    assert!(!everything(&url).contains(SECRET));

    // Pulls: redacts only for a client that asks; `kind=redact` for the
    // one-off backfill; a page of nothing but withheld redacts still
    // moves an older client's cursor to the head.
    let all = pull(port, &studio, "?since=0", true);
    let old = pull(port, &studio, "?since=0", false);
    assert_eq!(kinds(&all).iter().filter(|k| *k == "redact").count(), 4);
    assert!(!kinds(&old).iter().any(|k| k == "redact"));
    assert_eq!(kinds(&old).len() + 4, kinds(&all).len());
    let only = pull(port, &studio, "?since=0&kind=redact", false);
    assert_eq!(kinds(&only), ["redact"; 4]);
    let tail = [
        redact(ticket, "matt", t + 13, &[&late]),
        redact(ticket, "matt", t + 14, &[&late]),
    ];
    let (status, body) = push(port, &studio, &[&tail[0], &tail[1]]);
    assert_eq!(status, 200, "{body}");
    let head = pull(port, &studio, "?since=0", true)["head"]
        .as_i64()
        .unwrap();
    let last_old = old["next"].as_i64().unwrap();
    let skipped = pull(port, &studio, &format!("?since={last_old}"), false);
    assert_eq!(kinds(&skipped), Vec::<String>::new());
    assert_eq!(skipped["next"].as_i64(), Some(head));
}
