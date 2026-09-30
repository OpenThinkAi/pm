//! AGT-1378: byte payloads live in the database as base64, and migration
//! 0005 (`src/reencode.rs`) rewrites rows a pre-AGT-1378 binary wrote as
//! JSON arrays of integers — in place, byte-identical to a fresh
//! materialization, so `pm doctor`'s byte-for-byte replay check is clean
//! straight after the upgrade.

use std::collections::BTreeSet;

use pm_core::op::{BodyEdit, TicketCreate};
use pm_core::{ActorId, Body, Hlc, Op, Payload, Priority, State, StateCategory, Workspace};
use pm_store::{SCHEMA_VERSION, Store};
use rusqlite::{Connection, params};
use serde_json::Value;
use tempfile::TempDir;
use ulid::Ulid;

/// Migration 0005, the one under test.
const COMPACT_BYTES_VERSION: u32 = 5;

fn workspace() -> Workspace {
    Workspace {
        id: Ulid::new(),
        prefix: "AGT".into(),
        states: vec![State {
            name: "triage".into(),
            category: StateCategory::Unstarted,
            position: 0,
        }],
        gate_labels: Default::default(),
        model_labels: Default::default(),
        template_sections: Vec::new(),
        stale_days: 30,
    }
}

fn op(entity: Ulid, wall_ms: u64, payload: Payload) -> Op {
    Op::new(
        Ulid::new(),
        Hlc::new(wall_ms, 0),
        ActorId::new("matt"),
        entity,
        payload,
    )
}

fn body_edit(entity: Ulid, wall_ms: u64, body: &mut Body, text: &str) -> Op {
    let update = body.diff_from_text(text).unwrap().into_bytes();
    op(entity, wall_ms, Payload::BodyEdit(BodyEdit { update }))
}

/// A store with one ticket (two description edits) and one project whose
/// design doc has one edit — every table that carries byte payloads has a
/// row. Returns the ticket and doc ids.
fn populated() -> (TempDir, Ulid, Ulid) {
    let dir = tempfile::tempdir().unwrap();
    let mut store = Store::open(dir.path().join("pm.sqlite")).unwrap();
    store.init_workspace(&workspace()).unwrap();

    let ticket = Ulid::new();
    store
        .commit(&op(
            ticket,
            1,
            Payload::TicketCreate(TicketCreate {
                title: "t".into(),
                state: "triage".into(),
                priority: Priority::Medium,
                project: None,
                repo: None,
                source: None,
                ext: Default::default(),
            }),
        ))
        .unwrap();
    let mut author = Body::with_peer(7).unwrap();
    store
        .commit(&body_edit(ticket, 2, &mut author, "first draft"))
        .unwrap();
    store
        .commit(&body_edit(ticket, 3, &mut author, "first draft, revised"))
        .unwrap();

    let doc = store
        .create_project("proj", "Proj", &BTreeSet::new(), None)
        .unwrap();
    let mut doc_author = Body::with_peer(8).unwrap();
    store
        .commit_doc_edit(doc, &body_edit(doc, 4, &mut doc_author, "# Design"))
        .unwrap();
    (dir, ticket, doc)
}

fn raw(dir: &TempDir) -> Connection {
    Connection::open(dir.path().join("pm.sqlite")).unwrap()
}

fn column(conn: &Connection, sql: &str) -> Vec<(String, String)> {
    conn.prepare(sql)
        .unwrap()
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap()
}

const SELECT_OPS: &str =
    "SELECT CAST(seq AS TEXT), payload FROM ops WHERE kind = 'body.edit' ORDER BY seq";
const SELECT_TICKET_VIEWS: &str = "SELECT ticket, view FROM ticket_view ORDER BY ticket";
const SELECT_DOC_VIEWS: &str = "SELECT doc_id, view FROM project_doc_view ORDER BY doc_id";

/// Turns a base64 string at `key` back into the array of integers a
/// pre-AGT-1378 binary stored.
fn to_legacy(text: &str, key: &str) -> String {
    let mut value: Value = serde_json::from_str(text).unwrap();
    let encoded = value[key].as_str().expect("current form is a string");
    let bytes = pm_core::bytes::decode(encoded).unwrap();
    value[key] = Value::Array(bytes.into_iter().map(Value::from).collect());
    value.to_string()
}

/// Rewrites every byte payload into the legacy array form and forgets
/// that migration 0005 ran — the database a pre-AGT-1378 binary leaves
/// behind, as far as this migration is concerned.
fn downgrade(conn: &Connection) {
    for (seq, payload) in column(conn, SELECT_OPS) {
        conn.execute(
            "UPDATE ops SET payload = ?2 WHERE seq = ?1",
            params![seq.parse::<i64>().unwrap(), to_legacy(&payload, "update")],
        )
        .unwrap();
    }
    for (ticket, view) in column(conn, SELECT_TICKET_VIEWS) {
        conn.execute(
            "UPDATE ticket_view SET view = ?2 WHERE ticket = ?1",
            params![ticket, to_legacy(&view, "body")],
        )
        .unwrap();
    }
    for (doc_id, view) in column(conn, SELECT_DOC_VIEWS) {
        conn.execute(
            "UPDATE project_doc_view SET view = ?2 WHERE doc_id = ?1",
            params![doc_id, to_legacy(&view, "body")],
        )
        .unwrap();
    }
    // Back to schema 4: forget 0005 and everything after it. Later
    // migrations (0006, AGT-1393) are written idempotently, so re-running
    // them over their own tables on reopen is a no-op.
    conn.execute(
        "DELETE FROM schema_version WHERE version >= ?1",
        params![COMPACT_BYTES_VERSION],
    )
    .unwrap();
}

#[test]
fn byte_payloads_are_stored_as_base64_strings() {
    let (dir, _ticket, _doc) = populated();
    let conn = raw(&dir);
    let ops = column(&conn, SELECT_OPS);
    assert_eq!(ops.len(), 3);
    for (_, payload) in &ops {
        let value: Value = serde_json::from_str(payload).unwrap();
        assert!(value["update"].is_string(), "{payload}");
    }
    for (_, view) in column(&conn, SELECT_TICKET_VIEWS)
        .into_iter()
        .chain(column(&conn, SELECT_DOC_VIEWS))
    {
        let value: Value = serde_json::from_str(&view).unwrap();
        assert!(value["body"].is_string(), "{view}");
    }
}

#[test]
fn migration_0005_rewrites_legacy_rows_back_to_the_exact_current_form() {
    let (dir, ticket, doc) = populated();
    let conn = raw(&dir);
    let ops_before = column(&conn, SELECT_OPS);
    let ticket_views_before = column(&conn, SELECT_TICKET_VIEWS);
    let doc_views_before = column(&conn, SELECT_DOC_VIEWS);

    downgrade(&conn);
    let legacy_ops = column(&conn, SELECT_OPS);
    assert_ne!(legacy_ops, ops_before);
    assert!(
        legacy_ops[0].1.contains("\"update\":["),
        "{}",
        legacy_ops[0].1
    );
    let legacy_bytes: usize = legacy_ops.iter().map(|(_, p)| p.len()).sum();
    let current_bytes: usize = ops_before.iter().map(|(_, p)| p.len()).sum();
    assert!(
        legacy_bytes > current_bytes,
        "the array form is larger ({legacy_bytes} vs {current_bytes})"
    );
    drop(conn);

    // Reopening runs the migration.
    let mut store = Store::open(dir.path().join("pm.sqlite")).unwrap();
    assert_eq!(store.schema_version().unwrap(), SCHEMA_VERSION);
    let conn = raw(&dir);
    assert_eq!(column(&conn, SELECT_OPS), ops_before);
    assert_eq!(column(&conn, SELECT_TICKET_VIEWS), ticket_views_before);
    assert_eq!(column(&conn, SELECT_DOC_VIEWS), doc_views_before);

    // Nothing about the ops changed but their spelling.
    let ops = store.ops(ticket).unwrap();
    assert_eq!(ops.len(), 3);
    assert_eq!(
        store.ticket(ticket).unwrap().unwrap().description,
        "first draft, revised"
    );
    assert_eq!(store.doc_view(doc).unwrap().unwrap().text(), "# Design");

    // And the byte-for-byte replay check is clean right after the upgrade.
    let report = store.doctor().unwrap();
    assert!(report.is_healthy(), "{report:?}");
    assert!(store.rebuild().unwrap().is_empty());
}

#[test]
fn a_legacy_row_still_reads_without_the_migration() {
    // A row in the array form is readable on its own merits (the serde
    // readers accept both spellings), not only because 0005 rewrote it —
    // that is what keeps an old backup's JSONL restorable.
    let (dir, ticket, _doc) = populated();
    let conn = raw(&dir);
    for (seq, payload) in column(&conn, SELECT_OPS) {
        conn.execute(
            "UPDATE ops SET payload = ?2 WHERE seq = ?1",
            params![seq.parse::<i64>().unwrap(), to_legacy(&payload, "update")],
        )
        .unwrap();
    }
    for (id, view) in column(&conn, SELECT_TICKET_VIEWS) {
        conn.execute(
            "UPDATE ticket_view SET view = ?2 WHERE ticket = ?1",
            params![id, to_legacy(&view, "body")],
        )
        .unwrap();
    }
    drop(conn);

    // schema_version still says 5, so nothing is rewritten on open.
    let store = Store::open(dir.path().join("pm.sqlite")).unwrap();
    let ops = store.ops(ticket).unwrap();
    let Payload::BodyEdit(edit) = &ops[1].payload else {
        panic!("{:?}", ops[1]);
    };
    assert!(!edit.update.is_empty());
    assert_eq!(
        store.ticket(ticket).unwrap().unwrap().description,
        "first draft, revised"
    );
}
