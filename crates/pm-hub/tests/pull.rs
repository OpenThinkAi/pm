//! End-to-end `GET /w/<workspace>/ops?since=&limit=` (AGT-1390): paging a
//! log the shape of the Studio's seed back out byte for byte, the cursor
//! contract (`next`/`head`), query validation, auth, and pulls that never
//! wait for a push and never skip a seq. See `common` for where Postgres
//! comes from.

mod common;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use common::*;
use pm_core::domain::{Hold, Priority, Relation, RelationKind};
use pm_core::op::{
    BodyEdit, Claim, CommentAdd, FieldSet, HoldSet, LabelAdd, RelationAdd, StateTransition,
    TicketCreate,
};
use pm_core::{ActorId, Hlc, Op, Payload};
use serde::Deserialize;
use serde_json::Value;
use serde_json::value::RawValue;
use ulid::Ulid;

/// The page as served; `op` is borrowed verbatim from the response text.
#[derive(Deserialize)]
struct Page<'a> {
    #[serde(borrow)]
    ops: Vec<Item<'a>>,
    next: i64,
    head: i64,
}

#[derive(Deserialize)]
struct Item<'a> {
    seq: i64,
    #[serde(borrow)]
    op: &'a RawValue,
}

fn create_token(url: &str, name: &str, workspace: &str) -> String {
    let (ok, stdout, stderr) = admin(url, &["token", "create", name, "--workspace", workspace]);
    assert!(ok, "token create failed: {stderr}");
    stdout.trim().to_string()
}

fn pull_raw(port: u16, token: &str, query: &str) -> Response {
    request(
        port,
        "GET",
        &format!("/w/saltline/ops{query}"),
        &[&format!("Authorization: Bearer {token}")],
    )
}

/// A page as `(ops as (seq, text), next, head)`.
fn pull(port: u16, token: &str, query: &str) -> (Vec<(i64, String)>, i64, i64) {
    let resp = pull_raw(port, token, query);
    assert_eq!(resp.status, 200, "{query}: {resp:?}");
    assert!(
        resp.headers
            .iter()
            .any(|(k, v)| k == "content-type" && v == "application/json"),
        "{:?}",
        resp.headers
    );
    let page: Page = serde_json::from_str(&resp.body)
        .unwrap_or_else(|e| panic!("{query}: body is not a page: {e}"));
    let ops = page
        .ops
        .iter()
        .map(|item| (item.seq, item.op.get().to_string()))
        .collect();
    (ops, page.next, page.head)
}

/// Pages from `since` until caught up: every op in seq order, plus the
/// final `head`.
fn pull_all(port: u16, token: &str, since: i64, limit: i64) -> (Vec<(i64, String)>, i64) {
    let mut all = Vec::new();
    let mut since = since;
    loop {
        let (ops, next, head) = pull(port, token, &format!("?since={since}&limit={limit}"));
        let n = ops.len() as i64;
        assert!(n <= limit, "{n} ops for limit {limit}");
        if ops.is_empty() {
            assert_eq!(next, since);
        } else {
            assert_eq!(next, ops[ops.len() - 1].0);
            assert!(ops[0].0 > since);
            assert!(
                ops.windows(2).all(|w| w[0].0 < w[1].0),
                "page not ascending"
            );
        }
        all.extend(ops);
        since = next;
        if next >= head {
            // A caught-up cursor stays put on the next pull.
            let (rest, again, head2) = pull(port, token, &format!("?since={since}&limit={limit}"));
            if rest.is_empty() {
                assert_eq!(again, since);
                return (all, head2);
            }
            // Something landed between the two pulls; keep paging.
            all.extend(rest);
            since = again;
        }
    }
}

/// Pushes `texts` (already-serialized ops) in batches of 1000; returns
/// `(op_id, seq, text)` in push order.
fn push_texts(port: u16, token: &str, texts: &[(String, String)]) -> Vec<(String, i64, String)> {
    let mut out = Vec::with_capacity(texts.len());
    for chunk in texts.chunks(1000) {
        let body = format!(
            "{{\"ops\": [{}]}}",
            chunk
                .iter()
                .map(|(_, t)| t.as_str())
                .collect::<Vec<_>>()
                .join(",\n")
        );
        let resp = request_body(
            port,
            "POST",
            "/w/saltline/ops",
            &[&format!("Authorization: Bearer {token}")],
            body.as_bytes(),
        );
        assert_eq!(
            resp.status,
            200,
            "{}",
            &resp.body[..resp.body.len().min(300)]
        );
        let acked: Value = serde_json::from_str(&resp.body).unwrap();
        let acked = acked["ops"].as_array().unwrap();
        assert_eq!(acked.len(), chunk.len());
        for (ack, (op_id, text)) in acked.iter().zip(chunk) {
            assert_eq!(ack["op_id"], *op_id);
            assert_eq!(ack["stored"], true);
            out.push((op_id.clone(), ack["seq"].as_i64().unwrap(), text.clone()));
        }
    }
    out
}

fn at(wall_ms: u64, actor: &str, entity: Ulid, payload: Payload) -> Op {
    Op::new(
        Ulid::new(),
        Hlc::new(wall_ms, 0),
        ActorId::new(actor),
        entity,
        payload,
    )
}

/// Deterministic filler that is not compressible to nothing, for large
/// `body.edit` payloads.
fn bytes(n: usize, seed: u64) -> Vec<u8> {
    let mut x = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    (0..n)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            (x >> 24) as u8
        })
        .collect()
}

/// Serializes `op` in one of three spellings: compact, pretty, or a
/// hand-written key order with odd spacing. Byte identity on the way
/// back is only meaningful if the input is not canonical.
fn spell(i: usize, op: &Op) -> String {
    match i % 3 {
        0 => serde_json::to_string(op).unwrap(),
        1 => serde_json::to_string_pretty(op).unwrap(),
        _ => {
            let payload = serde_json::to_value(op).unwrap();
            let payload = payload
                .get("payload")
                .map_or("null".to_string(), Value::to_string);
            format!(
                "{{ \"version\":{},  \"payload\": {payload}, \"kind\" : \"{}\", \"entity\":\"{}\",\n \"actor\":\"{}\", \"hlc\":{{\"counter\":{}, \"wall_ms\":{}}}, \"op_id\":\"{}\" }}",
                op.version,
                op.kind(),
                op.entity,
                op.actor.as_str(),
                op.hlc.counter,
                op.hlc.wall_ms,
                op.op_id
            )
        }
    }
}

/// A log the shape of the Studio's seed (README §Sync & hub: ~12.6k ops):
/// ~1200 tickets with their create, field writes, labels, transitions,
/// claims, comments, holds, relations and small body edits, a handful of
/// config ops, and a few large `body.edit`s (the real log holds one of
/// 23 MB). Returns `(op_id, text)` in log order, with the text in mixed
/// spellings.
fn seed_log() -> Vec<(String, String)> {
    const TICKETS: usize = 1200;
    let mut ops: Vec<Op> = Vec::with_capacity(13_000);
    let mut clock = 1_780_000_000_000u64;
    let mut tick = || {
        clock += 1;
        clock
    };
    let workspace = Ulid::new();
    ops.push(at(
        tick(),
        "studio",
        workspace,
        Payload::WorkspaceSet(pm_core::op::WorkspaceSet::Prefix("AGT".to_string())),
    ));
    let project = Ulid::new();
    ops.push(at(
        tick(),
        "studio",
        project,
        Payload::ProjectCreate(pm_core::op::ProjectCreate {
            id: "pm".to_string(),
            title: "pm — tickets".to_string(),
            status: pm_core::domain::ProjectStatus::InProgress,
            parent: None,
            doc_id: None,
        }),
    ));
    let mut tickets: Vec<Ulid> = Vec::with_capacity(TICKETS);
    for n in 0..TICKETS {
        let t = Ulid::new();
        let actor = ["matt", "claude:pm-build", "studio"][n % 3];
        ops.push(at(
            tick(),
            actor,
            t,
            Payload::TicketCreate(TicketCreate {
                title: format!("Ticket {n}: “quotes”, tabs\t and \\ backslashes"),
                state: "triage".to_string(),
                priority: Priority::Medium,
                project: Some("pm".to_string()),
                repo: Some("OpenThinkAi/pm".to_string()),
                source: None,
                ext: Default::default(),
            }),
        ));
        ops.push(at(
            tick(),
            "studio",
            t,
            Payload::FieldSet(FieldSet::Number(1000 + n as u64)),
        ));
        for label in ["model:fable-5", "wave:3"] {
            ops.push(at(
                tick(),
                actor,
                t,
                Payload::LabelAdd(LabelAdd {
                    label: label.to_string(),
                }),
            ));
        }
        ops.push(at(
            tick(),
            actor,
            t,
            Payload::StateTransition(StateTransition {
                state: "ready".to_string(),
            }),
        ));
        ops.push(at(
            tick(),
            "claude:pm-build",
            t,
            Payload::Claim(Claim {
                state: "in-progress".to_string(),
                assignee: ActorId::new("claude:pm-build"),
            }),
        ));
        ops.push(at(
            tick(),
            "claude:pm-build",
            t,
            Payload::CommentAdd(CommentAdd {
                body: format!("Built {n} — see the review.\n\n```sh\ncargo test\n```\n"),
            }),
        ));
        ops.push(at(
            tick(),
            actor,
            t,
            Payload::BodyEdit(BodyEdit {
                update: bytes(200 + (n * 37) % 1800, n as u64),
            }),
        ));
        if n % 5 == 0 {
            let when = tick();
            ops.push(at(
                when,
                "matt",
                t,
                Payload::HoldSet(HoldSet {
                    hold: Hold {
                        reason: "needs a decision".to_string(),
                        by: ActorId::new("matt"),
                        at: Hlc::new(when, 0),
                    },
                }),
            ));
            ops.push(at(tick(), "matt", t, Payload::HoldClear));
        }
        if let Some(prev) = tickets.last() {
            ops.push(at(
                tick(),
                actor,
                t,
                Payload::RelationAdd(RelationAdd {
                    relation: Relation {
                        kind: RelationKind::Blocks,
                        from: *prev,
                        to: t,
                    },
                }),
            ));
        }
        ops.push(at(
            tick(),
            actor,
            t,
            Payload::FieldSet(FieldSet::Assignee(None)),
        ));
        ops.push(at(
            tick(),
            "claude:pm-build",
            t,
            Payload::StateTransition(StateTransition {
                state: "done".to_string(),
            }),
        ));
        tickets.push(t);
    }
    // Large document edits, spread through the log.
    for (i, size) in [512 * 1024, 2 * 1024 * 1024, 6 * 1024 * 1024]
        .into_iter()
        .enumerate()
    {
        let doc = Ulid::new();
        let op = at(
            tick(),
            "studio",
            doc,
            Payload::BodyEdit(BodyEdit {
                update: bytes(size, 99 + i as u64),
            }),
        );
        let position = ops.len() * (i + 1) / 4;
        ops.insert(position, op);
    }
    assert!(ops.len() > 12_000, "{} ops", ops.len());
    ops.iter()
        .enumerate()
        .map(|(i, op)| (op.op_id.to_string(), spell(i, op)))
        .collect()
}

#[test]
fn pull_pages_the_seed_log_back_byte_for_byte() {
    let Some((_container, url)) = postgres_for("pull_pages_the_seed_log_back_byte_for_byte") else {
        return;
    };
    let port = free_port();
    let mut hub = spawn_hub(&url, port);
    wait_for_health(&mut hub, port);
    let studio = create_token(&url, "studio", "saltline");
    let other = create_token(&url, "elsewhere", "other");

    // Empty log: nothing, cursor stays, head 0.
    let resp = pull_raw(port, &studio, "");
    assert_eq!(resp.status, 200);
    assert_eq!(resp.body, r#"{"ops":[],"next":0,"head":0}"#);
    let resp = pull_raw(port, &studio, "?since=7");
    assert_eq!(resp.body, r#"{"ops":[],"next":7,"head":0}"#);

    let log = seed_log();
    let pushed = push_texts(port, &studio, &log);
    let head = pushed[pushed.len() - 1].1;
    assert_eq!(head, pushed.len() as i64);

    // From 0 in pages of 1000: the exact set, in seq order, each op's
    // text byte for byte what was pushed (mixed spellings included).
    let (all, final_head) = pull_all(port, &studio, 0, 1000);
    assert_eq!(final_head, head);
    assert_eq!(all.len(), pushed.len());
    for ((seq, text), (op_id, want_seq, want_text)) in all.iter().zip(&pushed) {
        assert_eq!(seq, want_seq, "{op_id}");
        assert_eq!(text, want_text, "{op_id}");
    }

    // Page shape: a full page's `next` is its last seq and `head` is the
    // log's; the default page is 500; the last page is short.
    let (page, next, h) = pull(port, &studio, "?since=0&limit=1000");
    assert_eq!(page.len(), 1000);
    assert_eq!((page[0].0, next, h), (1, 1000, head));
    let (page, next, h) = pull(port, &studio, "?since=0");
    assert_eq!(page.len(), 500);
    assert_eq!((next, h), (500, head));
    let (page, next, h) = pull(port, &studio, &format!("?since={}&limit=1000", head - 3));
    assert_eq!(page.len(), 3);
    assert_eq!((page[0].0, next, h), (head - 2, head, head));
    let (page, next, _) = pull(port, &studio, "?limit=1&since=41");
    assert_eq!((page.len(), page[0].0, next), (1, 42, 42));

    // A limit past the cap is clamped to it, not rejected.
    let (page, next, _) = pull(port, &studio, "?since=0&limit=100000");
    assert_eq!((page.len(), next), (1000, 1000));

    // At or beyond head: empty, cursor unchanged, head reported.
    for since in [head, head + 1, head + 1_000_000] {
        let resp = pull_raw(port, &studio, &format!("?since={since}"));
        assert_eq!(resp.status, 200);
        assert_eq!(
            resp.body,
            format!(r#"{{"ops":[],"next":{since},"head":{head}}}"#)
        );
    }

    // Bad queries: 400 with the same error shape as a push's.
    for (query, reason) in [
        ("?since=-1", "since must be"),
        ("?since=abc", "since must be"),
        ("?limit=0", "limit must be"),
        ("?limit=-1", "limit must be"),
        ("?limit=x", "limit must be"),
        ("?since=0&kind=claim", "unknown query parameter"),
        ("?since=%ZZ", ""),
    ] {
        let resp = pull_raw(port, &studio, query);
        assert_eq!(resp.status, 400, "{query}: {resp:?}");
        let err: Value = serde_json::from_str(&resp.body).unwrap();
        assert_eq!(err["error"], "invalid_query", "{query}");
        assert!(
            err["reason"].as_str().unwrap().contains(reason),
            "{query}: {err}"
        );
    }

    // Auth: every failure is the unknown-route 404, byte for byte.
    let unknown = request(port, "GET", "/no/such/route", &[]);
    let cases = [
        ("no token", vec![]),
        (
            "other workspace's token",
            vec![format!("Authorization: Bearer {other}")],
        ),
        (
            "garbage token",
            vec!["Authorization: Bearer hunter2".to_string()],
        ),
    ];
    for (case, headers) in cases {
        let headers: Vec<&str> = headers.iter().map(String::as_str).collect();
        let resp = request(port, "GET", "/w/saltline/ops?since=0", &headers);
        assert_eq!(resp, unknown, "{case}");
    }
    let resp = request(
        port,
        "GET",
        "/w/nosuch/ops",
        &[&format!("Authorization: Bearer {studio}")],
    );
    assert_eq!(resp, unknown, "workspace that does not exist");
    // Another workspace's log is its own: the other token sees nothing.
    let resp = request(
        port,
        "GET",
        "/w/other/ops",
        &[&format!("Authorization: Bearer {other}")],
    );
    assert_eq!(resp.status, 200);
    assert_eq!(resp.body, r#"{"ops":[],"next":0,"head":0}"#);
    // A wrong method on the route is still the plain 404.
    let resp = request(
        port,
        "PUT",
        "/w/saltline/ops",
        &[&format!("Authorization: Bearer {studio}")],
    );
    assert_eq!(resp, unknown, "wrong method");
}

fn label_ops(n: usize, actor: &str) -> Vec<(String, String)> {
    (0..n)
        .map(|_| {
            let op = at(
                1_790_000_000_000,
                actor,
                Ulid::new(),
                Payload::LabelAdd(LabelAdd {
                    label: "c".to_string(),
                }),
            );
            (op.op_id.to_string(), serde_json::to_string(&op).unwrap())
        })
        .collect()
}

/// A transaction that holds the workspace's push lock with some ops
/// inserted but not committed, until told to commit: what an in-flight
/// push looks like from a puller's point of view.
struct HeldPush {
    commit: mpsc::Sender<()>,
    done: Option<thread::JoinHandle<()>>,
}

fn hold_push(url: &str, ops: Vec<(String, String)>) -> HeldPush {
    let (commit, release) = mpsc::channel::<()>();
    let (locked_tx, locked) = mpsc::channel::<()>();
    let url = url.to_string();
    let done = thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let (mut client, connection) =
                tokio_postgres::connect(&url, tokio_postgres::NoTls).await.unwrap();
            tokio::spawn(connection);
            let tx = client.transaction().await.unwrap();
            tx.execute(
                "SELECT id FROM workspaces WHERE id = 'saltline' FOR NO KEY UPDATE",
                &[],
            )
            .await
            .unwrap();
            for (op_id, text) in &ops {
                tx.execute(
                    "INSERT INTO ops (workspace_id, op_id, hlc_wall_ms, hlc_counter, actor, entity, kind, op)
                     VALUES ('saltline', $1, 1, 0, 'held', $1, 'label.add', $2::text::json)",
                    &[op_id, text],
                )
                .await
                .unwrap();
            }
            locked_tx.send(()).unwrap();
            let _ = release.recv();
            tx.commit().await.unwrap();
        });
    });
    locked.recv().unwrap();
    HeldPush {
        commit,
        done: Some(done),
    }
}

impl HeldPush {
    fn commit(mut self) {
        self.commit.send(()).unwrap();
        self.done.take().unwrap().join().unwrap();
    }
}

fn waiting_on_a_lock(url: &str) -> bool {
    query_rows(url, "SELECT count(*) FROM pg_locks WHERE NOT granted").unwrap()[0][0]
        .as_deref()
        .unwrap()
        != "0"
}

#[test]
fn pulls_never_wait_for_a_push_and_never_skip_a_seq() {
    let Some((_container, url)) = postgres_for("pulls_never_wait_for_a_push_and_never_skip_a_seq")
    else {
        return;
    };
    let port = free_port();
    let mut hub = spawn_hub(&url, port);
    wait_for_health(&mut hub, port);
    let token = create_token(&url, "studio", "saltline");

    let before = push_texts(port, &token, &label_ops(100, "studio"));
    let head0 = before[99].1;

    // An in-flight push holds the workspace lock with 20 ops inserted
    // but uncommitted; a hub push behind it blocks on that lock.
    let held_ops = label_ops(20, "held");
    let held = hold_push(&url, held_ops.clone());
    let blocked = thread::spawn({
        let token = token.clone();
        let ops = label_ops(30, "studio");
        move || push_texts(port, &token, &ops)
    });
    let deadline = Instant::now() + Duration::from_secs(20);
    while !waiting_on_a_lock(&url) {
        assert!(
            Instant::now() < deadline,
            "the hub push never queued on the lock"
        );
        assert!(
            !blocked.is_finished(),
            "the hub push did not wait for the lock"
        );
        thread::sleep(Duration::from_millis(50));
    }

    // Pulls (and auth, which shares the reader) answer while both are
    // pending, and see only what is committed: head is still head0.
    let started = Instant::now();
    let (all, head) = pull_all(port, &token, 0, 1000);
    let took = started.elapsed();
    assert!(
        took < Duration::from_secs(5),
        "pull took {took:?} behind a push"
    );
    assert_eq!(head, head0);
    assert_eq!(all.len(), 100);
    let resp = request(
        port,
        "GET",
        "/w/saltline/whoami",
        &[&format!("Authorization: Bearer {token}")],
    );
    assert_eq!(resp.status, 200);
    assert!(
        !blocked.is_finished(),
        "the hub push did not wait for the lock"
    );

    // Release: the held ops commit first (lower seqs), then the hub's
    // push lands after them. Paging from head0 sees both, in order, with
    // nothing skipped.
    held.commit();
    let pushed = blocked.join().unwrap();
    let (after, head) = pull_all(port, &token, head0, 7);
    assert_eq!(after.len(), 50);
    assert_eq!(head, pushed[29].1);
    let held_ids: Vec<&str> = held_ops.iter().map(|(id, _)| id.as_str()).collect();
    let pushed_ids: Vec<&str> = pushed.iter().map(|(id, _, _)| id.as_str()).collect();
    let seen_ids: Vec<String> = after
        .iter()
        .map(|(_, text)| {
            serde_json::from_str::<Value>(text).unwrap()["op_id"]
                .as_str()
                .unwrap()
                .to_string()
        })
        .collect();
    let seen_ids: Vec<&str> = seen_ids.iter().map(String::as_str).collect();
    assert_eq!(&seen_ids[..20], held_ids);
    assert_eq!(&seen_ids[20..], pushed_ids);
    assert!(after[19].0 < pushed[0].1);
    for ((seq, text), (_, want_seq, want_text)) in after[20..].iter().zip(&pushed) {
        assert_eq!((seq, text), (want_seq, want_text));
    }

    // Pushers and pullers at once: each puller walks its cursor while
    // the log grows and ends up with exactly every op, in seq order.
    // A seq that appeared out of commit order would be skipped by a
    // cursor that had already moved past it, so this is the gap check.
    const PUSHERS: usize = 4;
    const BATCHES: usize = 12;
    const PER_BATCH: usize = 25;
    const PULLERS: usize = 3;
    let pushers_done = AtomicBool::new(false);
    let (mut want, pulled) = thread::scope(|s| {
        let pushers: Vec<_> = (0..PUSHERS)
            .map(|p| {
                let token = &token;
                s.spawn(move || {
                    let mut acked = Vec::new();
                    for _ in 0..BATCHES {
                        acked.extend(push_texts(
                            port,
                            token,
                            &label_ops(PER_BATCH, &format!("pusher-{p}")),
                        ));
                    }
                    acked
                })
            })
            .collect();
        let pullers: Vec<_> = (0..PULLERS)
            .map(|_| {
                let token = &token;
                let pushers_done = &pushers_done;
                s.spawn(move || {
                    let mut seen: Vec<(i64, String)> = Vec::new();
                    let mut since = head;
                    loop {
                        let done = pushers_done.load(Ordering::SeqCst);
                        let (ops, next, head) =
                            pull(port, token, &format!("?since={since}&limit=40"));
                        if !ops.is_empty() {
                            assert!(ops[0].0 > since);
                            assert!(ops.windows(2).all(|w| w[0].0 < w[1].0));
                            assert_eq!(next, ops[ops.len() - 1].0);
                        } else {
                            assert_eq!(next, since);
                        }
                        seen.extend(ops);
                        since = next;
                        if done && next >= head {
                            return seen;
                        }
                    }
                })
            })
            .collect();
        let acked: Vec<(i64, String)> = pushers
            .into_iter()
            .flat_map(|h| h.join().unwrap())
            .map(|(_, seq, text)| (seq, text))
            .collect();
        pushers_done.store(true, Ordering::SeqCst);
        let pulled: Vec<Vec<(i64, String)>> =
            pullers.into_iter().map(|h| h.join().unwrap()).collect();
        (acked, pulled)
    });
    want.sort_unstable();
    assert_eq!(want.len(), PUSHERS * BATCHES * PER_BATCH);
    for seen in &pulled {
        assert_eq!(seen.len(), want.len(), "a puller missed ops");
        assert_eq!(*seen, want);
    }
}
