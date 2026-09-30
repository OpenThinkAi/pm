//! End-to-end `POST /w/<workspace>/ops` (AGT-1389): seqs, idempotency,
//! all-or-nothing batches, structured 400s, size limits, auth, and
//! commit-ordered seqs under concurrent pushes, and one workspace's
//! pushes never stalling another's (AGT-1463). See `common` for where
//! Postgres comes from.

mod common;

use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use common::*;
use pm_core::op::{CommentAdd, LabelAdd};
use pm_core::{ActorId, Hlc, Op, Payload};
use serde_json::{Value, json};
use ulid::Ulid;

fn op(payload: Payload) -> Op {
    Op::new(
        Ulid::new(),
        Hlc::new(1_790_000_000_000, 0),
        ActorId::new("studio"),
        Ulid::new(),
        payload,
    )
}

fn label(label: &str) -> Op {
    op(Payload::LabelAdd(LabelAdd {
        label: label.to_string(),
    }))
}

fn push_raw(port: u16, token: &str, body: &str) -> Response {
    push_raw_to(port, "saltline", token, body)
}

fn push_raw_to(port: u16, workspace: &str, token: &str, body: &str) -> Response {
    request_body(
        port,
        "POST",
        &format!("/w/{workspace}/ops"),
        &[&format!("Authorization: Bearer {token}")],
        body.as_bytes(),
    )
}

/// Pushes `ops` to `saltline` as `{"ops": [...]}`: `(status, body)`.
fn push(port: u16, token: &str, ops: &[Value]) -> (u16, Value) {
    push_to(port, "saltline", token, ops)
}

fn push_to(port: u16, workspace: &str, token: &str, ops: &[Value]) -> (u16, Value) {
    let resp = push_raw_to(port, workspace, token, &json!({ "ops": ops }).to_string());
    let body: Value = serde_json::from_str(&resp.body)
        .unwrap_or_else(|e| panic!("{} body {:?} is not JSON: {e}", resp.status, resp.body));
    (resp.status, body)
}

fn acks(body: &Value) -> Vec<(String, i64, bool)> {
    body["ops"]
        .as_array()
        .unwrap_or_else(|| panic!("no ops in {body}"))
        .iter()
        .map(|a| {
            (
                a["op_id"].as_str().unwrap().to_string(),
                a["seq"].as_i64().unwrap(),
                a["stored"].as_bool().unwrap(),
            )
        })
        .collect()
}

fn count_ops(url: &str) -> i64 {
    query_rows(url, "SELECT count(*) FROM ops").unwrap()[0][0]
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

#[test]
fn push_assigns_seqs_idempotently_and_rejects_bad_batches_whole() {
    let Some((_container, url)) =
        postgres_for("push_assigns_seqs_idempotently_and_rejects_bad_batches_whole")
    else {
        return;
    };
    let port = free_port();
    let mut hub = spawn_hub(&url, port);
    wait_for_health(&mut hub, port);
    let studio = create_token(&url, "studio", "saltline");
    let other = create_token(&url, "elsewhere", "other");

    // Push three ops: seqs ascend in batch order, columns are indexed,
    // and the stored JSON is byte-for-byte what was sent (odd spacing and
    // key order included).
    let a = label("a");
    let b = op(Payload::CommentAdd(CommentAdd {
        body: "hi — \"quoted\"".to_string(),
    }));
    let c = op(Payload::HoldClear);
    let a_json = serde_json::to_string_pretty(&a).unwrap();
    let b_json = serde_json::to_string(&b).unwrap();
    let c_json = format!(
        r#"{{"version": 1, "kind": "hold.clear", "entity": "{}", "actor": "studio", "hlc": {{"counter": 0, "wall_ms": 1790000000000}}, "op_id": "{}"}}"#,
        c.entity, c.op_id
    );
    let body = format!("{{\"ops\": [{a_json}, {b_json}, {c_json}]}}");
    let resp = push_raw(port, &studio, &body);
    assert_eq!(resp.status, 200, "{resp:?}");
    let first: Value = serde_json::from_str(&resp.body).unwrap();
    let first = acks(&first);
    assert_eq!(first.len(), 3);
    assert_eq!(
        first.iter().map(|a| a.0.as_str()).collect::<Vec<_>>(),
        [
            a.op_id.to_string(),
            b.op_id.to_string(),
            c.op_id.to_string()
        ]
    );
    assert!(first.iter().all(|a| a.2), "{first:?}");
    assert!(
        first[0].1 < first[1].1 && first[1].1 < first[2].1,
        "{first:?}"
    );
    assert_eq!(count_ops(&url), 3);
    let rows = query_rows(
        &url,
        "SELECT workspace_id, op_id, hlc_wall_ms, hlc_counter, actor, entity, kind, op::text
         FROM ops ORDER BY seq",
    )
    .unwrap();
    let want = [
        (&a, &a_json, "label.add"),
        (&b, &b_json, "comment.add"),
        (&c, &c_json, "hold.clear"),
    ];
    for (row, (op, json, kind)) in rows.iter().zip(want) {
        let row: Vec<&str> = row.iter().map(|c| c.as_deref().unwrap()).collect();
        assert_eq!(
            row,
            [
                "saltline",
                op.op_id.to_string().as_str(),
                "1790000000000",
                "0",
                "studio",
                op.entity.to_string().as_str(),
                kind,
                json.as_str(),
            ]
        );
    }

    // Re-pushing the same batch: same seqs, nothing stored, no seq burned.
    let (status, again) = push(
        port,
        &studio,
        &[
            serde_json::to_value(&a).unwrap(),
            serde_json::to_value(&b).unwrap(),
            serde_json::to_value(&c).unwrap(),
        ],
    );
    assert_eq!(status, 200, "{again}");
    let again = acks(&again);
    assert_eq!(
        again
            .iter()
            .map(|a| (a.0.as_str(), a.1))
            .collect::<Vec<_>>(),
        first
            .iter()
            .map(|a| (a.0.as_str(), a.1))
            .collect::<Vec<_>>()
    );
    assert!(again.iter().all(|a| !a.2), "{again:?}");
    assert_eq!(count_ops(&url), 3);

    // Mixed: two known, two new, and a repeat of a new one inside the
    // batch. Known ops keep their seqs; the new ones get fresh ascending
    // seqs after everything so far; the repeat shares its twin's seq and
    // was not stored twice. Seqs are contiguous: replays burn none.
    let d = label("d");
    let e = label("e");
    let (status, mixed) = push(
        port,
        &studio,
        &[
            serde_json::to_value(&b).unwrap(),
            serde_json::to_value(&d).unwrap(),
            serde_json::to_value(&a).unwrap(),
            serde_json::to_value(&e).unwrap(),
            serde_json::to_value(&d).unwrap(),
        ],
    );
    assert_eq!(status, 200, "{mixed}");
    let mixed = acks(&mixed);
    assert_eq!(mixed[0], (b.op_id.to_string(), first[1].1, false));
    assert_eq!(mixed[2], (a.op_id.to_string(), first[0].1, false));
    assert_eq!(mixed[1], (d.op_id.to_string(), first[2].1 + 1, true));
    assert_eq!(mixed[3], (e.op_id.to_string(), first[2].1 + 2, true));
    assert_eq!(mixed[4], (d.op_id.to_string(), first[2].1 + 1, false));
    assert_eq!(count_ops(&url), 5);

    // The same op_id pushed to another workspace is a different row: the
    // unique key is (workspace, op_id).
    let (status, elsewhere) = push_to(port, "other", &other, &[serde_json::to_value(&a).unwrap()]);
    assert_eq!(status, 200, "{elsewhere}");
    assert!(acks(&elsewhere)[0].2);
    assert_eq!(count_ops(&url), 6);

    // Malformed op at index 1 (after a valid new one): 400 naming it, and
    // nothing from the batch stored.
    let f = label("f");
    let mut bad = serde_json::to_value(&f).unwrap();
    bad["kind"] = json!("nope");
    let (status, err) = push(
        port,
        &studio,
        &[serde_json::to_value(label("g")).unwrap(), bad],
    );
    assert_eq!(status, 400, "{err}");
    assert_eq!(err["error"], "invalid_op");
    assert_eq!(err["index"], 1);
    assert_eq!(err["op_id"], f.op_id.to_string());
    assert!(err["reason"].as_str().unwrap().contains("nope"), "{err}");
    assert_eq!(count_ops(&url), 6);
    let mut newer = serde_json::to_value(&f).unwrap();
    newer["version"] = json!(2);
    let (status, err) = push(port, &studio, &[newer]);
    assert_eq!(status, 400, "{err}");
    assert!(
        err["reason"].as_str().unwrap().contains("version 2"),
        "{err}"
    );
    let (status, err) = push(port, &studio, &[json!("not an op")]);
    assert_eq!(status, 400, "{err}");
    assert_eq!(err["error"], "invalid_op");
    assert_eq!(err["index"], 0);
    assert!(err.get("op_id").is_none(), "{err}");
    assert_eq!(count_ops(&url), 6);

    // Malformed batches.
    for body in ["not json", r#"{"ops": "x"}"#, r#"[]"#, r#"{"nope": []}"#] {
        let resp = push_raw(port, &studio, body);
        assert_eq!(resp.status, 400, "{body}: {resp:?}");
        let err: Value = serde_json::from_str(&resp.body).unwrap();
        assert_eq!(err["error"], "invalid_batch", "{body}");
    }
    let (status, empty) = push(port, &studio, &[]);
    assert_eq!(status, 200, "{empty}");
    assert_eq!(empty["ops"], json!([]));
    let too_many: Vec<Value> = (0..1001)
        .map(|_| serde_json::to_value(label("x")).unwrap())
        .collect();
    let (status, err) = push(port, &studio, &too_many);
    assert_eq!(status, 400, "{err}");
    assert_eq!(err["error"], "invalid_batch");
    assert!(err["reason"].as_str().unwrap().contains("1000"), "{err}");
    assert_eq!(count_ops(&url), 6);

    // Over the byte limit: 413 as soon as Content-Length says so.
    let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
    let _ = stream.write_all(
        format!(
            "POST /w/saltline/ops HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\
             Authorization: Bearer {studio}\r\nContent-Length: {}\r\n\r\n{{\"ops\": [",
            64 * 1024 * 1024 + 1
        )
        .as_bytes(),
    );
    let mut raw = String::new();
    let _ = stream.read_to_string(&mut raw);
    let (head, body) = raw
        .split_once("\r\n\r\n")
        .unwrap_or_else(|| panic!("{raw:?}"));
    assert!(head.starts_with("HTTP/1.1 413"), "{head}");
    let err: Value = serde_json::from_str(body).unwrap();
    assert_eq!(err["error"], "too_large");
    assert!(
        err["reason"].as_str().unwrap().contains("67108864"),
        "{err}"
    );

    // Auth: every failure is the unknown-route 404, byte for byte, and
    // stores nothing.
    let unknown = request(port, "GET", "/no/such/route", &[]);
    let batch = json!({ "ops": [serde_json::to_value(label("h")).unwrap()] }).to_string();
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
        let resp = request_body(port, "POST", "/w/saltline/ops", &headers, batch.as_bytes());
        assert_eq!(resp, unknown, "{case}");
    }
    let resp = request_body(
        port,
        "POST",
        "/w/nosuch/ops",
        &[&format!("Authorization: Bearer {studio}")],
        batch.as_bytes(),
    );
    assert_eq!(resp, unknown, "workspace that does not exist");
    assert_eq!(count_ops(&url), 6);
}

#[test]
fn concurrent_pushes_get_commit_ordered_contiguous_seqs() {
    let Some((_container, url)) =
        postgres_for("concurrent_pushes_get_commit_ordered_contiguous_seqs")
    else {
        return;
    };
    let port = free_port();
    let mut hub = spawn_hub(&url, port);
    wait_for_health(&mut hub, port);
    let token = create_token(&url, "studio", "saltline");

    const PUSHERS: usize = 8;
    const PER_BATCH: usize = 50;
    let batches: Vec<Vec<Value>> = (0..PUSHERS)
        .map(|_| {
            (0..PER_BATCH)
                .map(|_| serde_json::to_value(label("c")).unwrap())
                .collect()
        })
        .collect();
    let results: Vec<Vec<(String, i64, bool)>> = thread::scope(|s| {
        let handles: Vec<_> = batches
            .iter()
            .map(|batch| {
                let token = &token;
                s.spawn(move || {
                    let (status, body) = push(port, token, batch);
                    assert_eq!(status, 200, "{body}");
                    acks(&body)
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });

    // Every batch got a contiguous run of seqs in batch order, and the
    // runs tile the sequence with no interleaving and no gaps: pushes to
    // one workspace commit one at a time, in seq order.
    let mut runs: Vec<(i64, i64)> = Vec::new();
    for acks in &results {
        assert!(acks.iter().all(|a| a.2));
        let first = acks[0].1;
        for (i, ack) in acks.iter().enumerate() {
            assert_eq!(ack.1, first + i as i64, "{acks:?}");
        }
        runs.push((first, acks[PER_BATCH - 1].1));
    }
    runs.sort_unstable();
    for pair in runs.windows(2) {
        assert_eq!(pair[0].1 + 1, pair[1].0, "{runs:?}");
    }
    assert_eq!(runs[0].0, 1);
    assert_eq!(runs[PUSHERS - 1].1, (PUSHERS * PER_BATCH) as i64);
    assert_eq!(count_ops(&url), (PUSHERS * PER_BATCH) as i64);
    let stored: Vec<String> = query_rows(&url, "SELECT op_id FROM ops ORDER BY seq")
        .unwrap()
        .into_iter()
        .map(|r| r[0].clone().unwrap())
        .collect();
    let mut acked: Vec<(i64, String)> = results
        .iter()
        .flatten()
        .map(|a| (a.1, a.0.clone()))
        .collect();
    acked.sort_unstable();
    assert_eq!(stored, acked.into_iter().map(|a| a.1).collect::<Vec<_>>());
}

/// Holds `workspace`'s row lock in its own transaction — what a long push
/// looks like to every other writer — until the returned sender fires.
fn hold_workspace_lock(url: &str, workspace: &str) -> (mpsc::Sender<()>, thread::JoinHandle<()>) {
    let (release_tx, release) = mpsc::channel::<()>();
    let (locked_tx, locked) = mpsc::channel::<()>();
    let (url, workspace) = (url.to_string(), workspace.to_string());
    let done = thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let (mut client, connection) = tokio_postgres::connect(&url, tokio_postgres::NoTls)
                .await
                .unwrap();
            tokio::spawn(connection);
            let tx = client.transaction().await.unwrap();
            tx.execute(
                "SELECT id FROM workspaces WHERE id = $1 FOR NO KEY UPDATE",
                &[&workspace],
            )
            .await
            .unwrap();
            locked_tx.send(()).unwrap();
            let _ = release.recv();
            tx.commit().await.unwrap();
        });
    });
    locked.recv().unwrap();
    (release_tx, done)
}

fn lock_waiters(url: &str) -> i64 {
    query_rows(url, "SELECT count(*) FROM pg_locks WHERE NOT granted").unwrap()[0][0]
        .as_deref()
        .unwrap()
        .parse()
        .unwrap()
}

#[test]
fn a_stalled_workspace_never_blocks_another() {
    let Some((_container, url)) = postgres_for("a_stalled_workspace_never_blocks_another") else {
        return;
    };
    let port = free_port();
    let mut hub = spawn_hub(&url, port);
    wait_for_health(&mut hub, port);
    let busy = create_token(&url, "studio", "saltline");
    let other = create_token(&url, "elsewhere", "other");

    // saltline's writer is stuck behind its row lock, with more pushes
    // queued behind it than the hub has write connections.
    const QUEUED: usize = 10;
    let (release, holder) = hold_workspace_lock(&url, "saltline");
    let queued: Vec<_> = (0..QUEUED)
        .map(|_| {
            let token = busy.clone();
            let batch: Vec<Value> = (0..5)
                .map(|_| serde_json::to_value(label("q")).unwrap())
                .collect();
            thread::spawn(move || {
                let (status, body) = push(port, &token, &batch);
                assert_eq!(status, 200, "{body}");
                acks(&body)
            })
        })
        .collect();
    let deadline = Instant::now() + Duration::from_secs(20);
    while lock_waiters(&url) == 0 {
        assert!(
            Instant::now() < deadline,
            "no saltline push queued on the lock"
        );
        thread::sleep(Duration::from_millis(50));
    }
    // Only one of them holds a connection: the rest wait in the hub, not
    // in Postgres.
    thread::sleep(Duration::from_millis(300));
    assert_eq!(lock_waiters(&url), 1, "queued pushes hold no connection");
    assert!(queued.iter().all(|h| !h.is_finished()));

    // Another workspace pushes (and ends its seed) straight through.
    let started = Instant::now();
    for _ in 0..3 {
        let (status, body) = push_to(
            port,
            "other",
            &other,
            &[serde_json::to_value(label("x")).unwrap()],
        );
        assert_eq!(status, 200, "{body}");
    }
    let resp = request_body(
        port,
        "POST",
        "/w/other/seeded",
        &[&format!("Authorization: Bearer {other}")],
        br#"{"number_floor": 0}"#,
    );
    assert_eq!(resp.status, 200, "{resp:?}");
    let took = started.elapsed();
    assert!(took < Duration::from_secs(5), "other waited {took:?}");
    assert!(
        queued.iter().all(|h| !h.is_finished()),
        "saltline still stalled"
    );

    // Released, saltline's pushes land one after another with contiguous,
    // non-interleaved runs of seqs.
    release.send(()).unwrap();
    holder.join().unwrap();
    let mut runs: Vec<(i64, i64)> = queued
        .into_iter()
        .map(|h| {
            let acks = h.join().unwrap();
            for (i, ack) in acks.iter().enumerate() {
                assert_eq!(ack.1, acks[0].1 + i as i64, "{acks:?}");
            }
            (acks[0].1, acks[acks.len() - 1].1)
        })
        .collect();
    runs.sort_unstable();
    for pair in runs.windows(2) {
        assert!(pair[0].1 < pair[1].0, "{runs:?}");
    }
    let saltline: Vec<i64> = query_rows(
        &url,
        "SELECT seq FROM ops WHERE workspace_id = 'saltline' ORDER BY seq",
    )
    .unwrap()
    .into_iter()
    .map(|r| r[0].as_deref().unwrap().parse().unwrap())
    .collect();
    assert_eq!(saltline.len(), QUEUED * 5);
}
