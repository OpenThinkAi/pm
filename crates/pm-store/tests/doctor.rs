//! `Store::doctor` / `Store::rebuild` (AGT-1337): the ticket tables — and,
//! since AGT-1385, the configuration tables — regenerate from `ops`
//! byte-for-byte, drift is detected without writing, and a rebuild repairs
//! it in one transaction.

use std::collections::BTreeMap;

use pm_core::op::{
    Claim, CommentAdd, FieldSet, HoldSet, LabelAdd, LabelRemove, RelationAdd, RelationRemove,
    StateTransition, TicketCreate,
};
use pm_core::{
    ActorId, Body, Hlc, Hold, Op, Payload, Priority, Project, ProjectStatus, Relation,
    RelationKind, State, StateCategory, Workspace,
};
use pm_store::{CONFIG_TABLES, SCHEMA_VERSION, Store, StoreError, TICKET_TABLES};
use rusqlite::Connection;
use rusqlite::types::Value;
use tempfile::TempDir;
use ulid::Ulid;

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
        gate_labels: ["manual".to_string()].into(),
        model_labels: Default::default(),
        template_sections: Vec::new(),
        stale_days: 30,
        docs_owned_by: Default::default(),
    }
}

fn store() -> (TempDir, Store) {
    let dir = tempfile::tempdir().unwrap();
    let mut store = Store::open(dir.path().join("pm.sqlite")).unwrap();
    store
        .init_workspace(&workspace(), &pm_core::ActorId::new("matt"))
        .unwrap();
    store
        .put_project(
            &Project {
                kind: Default::default(),
                id: "pm".into(),
                title: "pm".into(),
                status: ProjectStatus::InProgress,
                parent: None,
                repos: ["OpenThinkAi/pm".to_string()].into(),
                doc: "# pm\n".into(),
                documents: Default::default(),
            },
            &pm_core::ActorId::new("matt"),
        )
        .unwrap();
    (dir, store)
}

fn raw(dir: &TempDir) -> Connection {
    Connection::open(dir.path().join("pm.sqlite")).unwrap()
}

fn op(ticket: Ulid, wall_ms: u64, actor: &str, payload: Payload) -> Op {
    Op::new(
        Ulid::new(),
        Hlc::new(wall_ms, 0),
        ActorId::new(actor),
        ticket,
        payload,
    )
}

fn create(ticket: Ulid, wall_ms: u64) -> Op {
    op(
        ticket,
        wall_ms,
        "matt",
        Payload::TicketCreate(TicketCreate {
            title: format!("ticket {ticket}"),
            state: "triage".into(),
            priority: Priority::Medium,
            project: Some("pm".into()),
            repo: None,
            source: None,
            ext: [("legacy".to_string(), serde_json::Value::from("x"))].into(),
        }),
    )
}

/// Every op kind at least once across two tickets, committed the way the
/// CLI would: one `Store` per op, so every step round-trips through the
/// persisted view (and its Loro snapshot) exactly as separate processes do.
fn exercise(dir: &TempDir) -> (Ulid, Ulid) {
    let (a, b) = (Ulid::new(), Ulid::new());
    let relation = Relation {
        kind: RelationKind::Blocks,
        from: b,
        to: a,
    };
    let label_add = op(
        a,
        2,
        "matt",
        Payload::LabelAdd(LabelAdd {
            label: "model:fable-5".into(),
        }),
    );
    let rel_add = op(a, 4, "matt", Payload::RelationAdd(RelationAdd { relation }));
    let mut author = Body::with_peer(7).unwrap();
    let body_v1 = author.diff_from_text("# Problem\n\nv1\n").unwrap();
    let body_v2 = author.diff_from_text("# Problem\n\nv1 v2\n").unwrap();
    let ops = vec![
        create(a, 1),
        label_add.clone(),
        op(
            a,
            2,
            "matt",
            Payload::LabelAdd(LabelAdd {
                label: "keep".into(),
            }),
        ),
        create(b, 3),
        rel_add.clone(),
        op(
            a,
            5,
            "matt",
            Payload::FieldSet(FieldSet::Title("renamed".into())),
        ),
        op(
            a,
            5,
            "matt",
            Payload::FieldSet(FieldSet::Ext {
                key: "legacy".into(),
                value: None,
            }),
        ),
        op(
            a,
            6,
            "claude:pm-build",
            Payload::CommentAdd(CommentAdd {
                body: "on it".into(),
            }),
        ),
        op(
            a,
            7,
            "claude:pm-build",
            Payload::Claim(Claim {
                state: "in-progress".into(),
                assignee: ActorId::new("claude:pm-build"),
            }),
        ),
        op(
            b,
            8,
            "matt",
            Payload::HoldSet(HoldSet {
                hold: Hold {
                    reason: "needs Matt".into(),
                    by: ActorId::new("matt"),
                    at: Hlc::new(8, 0),
                },
            }),
        ),
        op(
            a,
            9,
            "matt",
            Payload::BodyEdit(pm_core::op::BodyEdit {
                update: body_v1.into_bytes(),
            }),
        ),
        op(
            a,
            10,
            "matt",
            Payload::BodyEdit(pm_core::op::BodyEdit {
                update: body_v2.into_bytes(),
            }),
        ),
        op(
            a,
            11,
            "matt",
            Payload::LabelRemove(LabelRemove {
                label: "model:fable-5".into(),
                observed: vec![label_add.op_id],
            }),
        ),
        op(
            a,
            12,
            "matt",
            Payload::RelationRemove(RelationRemove {
                relation,
                observed: vec![rel_add.op_id],
            }),
        ),
        op(
            a,
            13,
            "matt",
            Payload::StateTransition(StateTransition {
                state: "done".into(),
            }),
        ),
        op(b, 14, "matt", Payload::HoldClear),
        op(b, 15, "matt", Payload::Tombstone),
    ];
    for o in ops {
        Store::open(dir.path().join("pm.sqlite"))
            .unwrap()
            .commit(&o)
            .unwrap();
    }
    let matt = ActorId::new("matt");
    let mut store = Store::open(dir.path().join("pm.sqlite")).unwrap();
    store.allocate_number(a, &matt).unwrap();
    store.allocate_number(b, &matt).unwrap();
    (a, b)
}

/// Every row of `table`, every column, as SQLite stores it — the
/// byte-for-byte oracle the store's own diff is checked against.
fn dump(conn: &Connection, table: &str) -> Vec<Vec<Value>> {
    let mut stmt = conn
        .prepare(&format!("SELECT * FROM {table} ORDER BY rowid"))
        .unwrap();
    let width = stmt.column_count();
    stmt.query_map([], |r| (0..width).map(|i| r.get::<_, Value>(i)).collect())
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

fn dump_all(conn: &Connection, tables: &[&str]) -> BTreeMap<String, Vec<Vec<Value>>> {
    tables
        .iter()
        .map(|t| (t.to_string(), dump(conn, t)))
        .collect()
}

/// Ops `store()`'s own `init_workspace` + `put_project` committed
/// (AGT-1385: prefix, template sections, stale days, one gate label, three
/// states; the project's create and one repo; AGT-1413: its design doc's
/// text as a `body.edit`).
const CONFIG_OPS: u64 = 10;

// ---- AC1: the report ----

#[test]
fn doctor_reports_counts_and_is_healthy_on_a_fresh_store() {
    let (dir, mut store) = store();
    exercise(&dir);
    let report = store.doctor().unwrap();
    assert!(report.is_healthy(), "{report:#?}");
    assert_eq!(report.schema_version, SCHEMA_VERSION);
    assert_eq!(report.op_count, 19 + CONFIG_OPS);
    assert_eq!(report.tables["ops"], 19 + CONFIG_OPS);
    assert_eq!(report.tables["ticket"], 2);
    assert_eq!(report.tables["ticket_view"], 2);
    assert_eq!(report.tables["ticket_label"], 1);
    assert_eq!(report.tables["relation"], 0);
    assert_eq!(report.tables["comment"], 1);
    assert_eq!(report.tables["marker"], 0);
    assert_eq!(report.tables["project"], 1);
    assert_eq!(report.tables["state"], 3);
    // One row per applied migration (AGT-1350 and AGT-1344 each added one).
    assert_eq!(report.tables["schema_version"], u64::from(SCHEMA_VERSION));
    assert!(report.integrity.is_empty());
    assert!(report.foreign_keys.is_empty());
    assert_eq!(report.replay_error, None);
    assert!(report.drift.is_empty());
}

// ---- AC2: rebuild reproduces identical tables ----

#[test]
fn rebuild_reproduces_every_ticket_table_byte_for_byte() {
    let (dir, mut store) = store();
    let (a, b) = exercise(&dir);
    let conn = raw(&dir);
    let before = dump_all(&conn, &TICKET_TABLES);
    let ticket_a = store.ticket(a).unwrap();
    let ticket_b = store.ticket(b).unwrap();

    let diff = store.rebuild().unwrap();
    assert!(diff.is_empty(), "{diff:#?}");
    assert_eq!(dump_all(&conn, &TICKET_TABLES), before);
    assert_eq!(store.ticket(a).unwrap(), ticket_a);
    assert_eq!(store.ticket(b).unwrap(), ticket_b);
    assert!(store.doctor().unwrap().is_healthy());
}

/// AGT-1385: the configuration tables are replayed from the config ops
/// too, and come out byte-identical (`workspace.number_floor` and the
/// document columns, which no config op writes, included).
#[test]
fn rebuild_reproduces_the_configuration_tables() {
    let (dir, mut store) = store();
    exercise(&dir);
    store.raise_number_floor(500).unwrap();
    let conn = raw(&dir);
    let config = dump_all(&conn, &CONFIG_TABLES);
    let docs = dump(&conn, "project_doc");
    let ops = dump(&conn, "ops");
    assert!(store.rebuild().unwrap().is_empty());
    assert_eq!(dump_all(&conn, &CONFIG_TABLES), config);
    assert_eq!(dump(&conn, "project_doc"), docs);
    assert_eq!(dump(&conn, "ops"), ops, "the log itself is never touched");
    assert_eq!(store.number_floor().unwrap(), 500);
}

/// AGT-1385 AC3: drift in a configuration row is detected by doctor and
/// repaired by rebuild the way ticket drift is — and a project row a
/// foreign writer removed stays removed (a rebuild never resurrects a
/// project, `pm project delete` being a direct write).
#[test]
fn config_drift_is_detected_and_repaired() {
    let (dir, mut store) = store();
    exercise(&dir);
    let conn = raw(&dir);
    let clean = dump_all(&conn, &CONFIG_TABLES);

    conn.execute("UPDATE workspace SET stale_days = 99, prefix = 'ZZ'", [])
        .unwrap();
    conn.execute("UPDATE state SET position = 42 WHERE name = 'done'", [])
        .unwrap();
    conn.execute("UPDATE project SET title = 'tampered' WHERE id = 'pm'", [])
        .unwrap();
    conn.execute(
        "INSERT INTO state (name, category, position) VALUES ('rogue', 'started', 9)",
        [],
    )
    .unwrap();

    let report = store.doctor().unwrap();
    assert!(!report.is_healthy());
    let tables: Vec<&str> = report
        .drift
        .tables
        .iter()
        .map(|t| t.table.as_str())
        .collect();
    assert_eq!(tables, ["workspace", "state", "project"], "{report:#?}");
    assert_ne!(dump_all(&conn, &CONFIG_TABLES), clean);

    let diff = store.rebuild().unwrap();
    assert_eq!(diff.row_count(), 4, "{diff:#?}");
    assert_eq!(dump_all(&conn, &CONFIG_TABLES), clean);
    assert!(store.doctor().unwrap().is_healthy());
}

// ---- AC3: drift is detected without writing and repaired by rebuild ----

#[test]
fn doctor_detects_corrupted_rows_and_rebuild_repairs_them() {
    let (dir, mut store) = store();
    let (a, _) = exercise(&dir);
    let conn = raw(&dir);
    let clean = dump_all(&conn, &TICKET_TABLES);

    // A foreign writer edits a row, drops one and adds one.
    conn.execute(
        "UPDATE ticket SET title = 'x', priority = 'low' WHERE id = ?1",
        [a.to_string()],
    )
    .unwrap();
    conn.execute("DELETE FROM comment", []).unwrap();
    conn.execute(
        "INSERT INTO ticket_label (ticket, label) VALUES (?1, 'stray')",
        [a.to_string()],
    )
    .unwrap();
    let corrupted = dump_all(&conn, &TICKET_TABLES);

    let report = store.doctor().unwrap();
    assert!(!report.is_healthy());
    assert!(report.integrity.is_empty() && report.foreign_keys.is_empty());
    assert_eq!(report.replay_error, None);
    let tables: Vec<&str> = report
        .drift
        .tables
        .iter()
        .map(|t| t.table.as_str())
        .collect();
    assert_eq!(tables, ["ticket", "ticket_label", "comment"]);
    assert_eq!(report.drift.row_count(), 3);

    let ticket = &report.drift.tables[0];
    assert_eq!(ticket.changed.len(), 1);
    assert_eq!(ticket.changed[0].key, [serde_json::json!(a.to_string())]);
    let changed: Vec<(&str, &serde_json::Value, &serde_json::Value)> = ticket.changed[0]
        .columns
        .iter()
        .map(|c| (c.column.as_str(), &c.before, &c.after))
        .collect();
    assert_eq!(
        changed,
        [
            (
                "priority",
                &serde_json::json!("low"),
                &serde_json::json!("medium")
            ),
            (
                "title",
                &serde_json::json!("x"),
                &serde_json::json!("renamed")
            ),
        ]
    );
    let label = &report.drift.tables[1];
    assert_eq!(label.extra.len(), 1);
    assert_eq!(label.extra[0].columns["label"], "stray");
    let comment = &report.drift.tables[2];
    assert_eq!(comment.missing.len(), 1);
    assert_eq!(comment.missing[0].columns["body"], "on it");

    assert_eq!(
        dump_all(&conn, &TICKET_TABLES),
        corrupted,
        "doctor is read-only: the corruption is still there"
    );

    let diff = store.rebuild().unwrap();
    assert_eq!(
        diff, report.drift,
        "rebuild applies exactly what doctor reported"
    );
    assert_eq!(dump_all(&conn, &TICKET_TABLES), clean);
    assert_eq!(store.ticket(a).unwrap().unwrap().title, "renamed");
    assert!(store.doctor().unwrap().is_healthy());
}

#[test]
fn doctor_reports_foreign_key_violations() {
    let (dir, mut store) = store();
    let (a, _) = exercise(&dir);
    let conn = raw(&dir);
    conn.pragma_update(None, "foreign_keys", "OFF").unwrap();
    conn.execute(
        "INSERT INTO comment (id, ticket, author, hlc_wall_ms, hlc_counter, body)
         VALUES (?1, ?2, 'ghost', 1, 0, 'boo')",
        [Ulid::new().to_string(), a.to_string()],
    )
    .unwrap();
    let report = store.doctor().unwrap();
    assert!(!report.is_healthy());
    assert_eq!(report.foreign_keys.len(), 1);
    assert_eq!(report.foreign_keys[0].table, "comment");
    assert_eq!(report.foreign_keys[0].parent, "actor");
    assert_eq!(report.drift.tables[0].table, "comment");
    assert_eq!(report.drift.tables[0].extra.len(), 1);

    store.rebuild().unwrap();
    assert!(store.doctor().unwrap().is_healthy());
}

#[test]
fn a_log_that_no_longer_replays_fails_the_rebuild_and_changes_nothing() {
    let (dir, mut store) = store();
    exercise(&dir);
    let conn = raw(&dir);
    let before = dump_all(&conn, &TICKET_TABLES);
    // An edited op: the transition now names a state that does not exist.
    conn.execute(
        "UPDATE ops SET payload = '{\"state\":\"shipped\"}' WHERE kind = 'state.transition'",
        [],
    )
    .unwrap();
    let seq: i64 = conn
        .query_row(
            "SELECT seq FROM ops WHERE kind = 'state.transition'",
            [],
            |r| r.get(0),
        )
        .unwrap();

    let err = store.rebuild().unwrap_err();
    match &err {
        StoreError::Replay {
            seq: failed,
            kind,
            source,
            ..
        } => {
            assert_eq!(*failed, seq);
            assert_eq!(*kind, "state.transition");
            assert!(
                matches!(**source, StoreError::UnknownState { ref state } if state == "shipped")
            );
        }
        other => panic!("{other}"),
    }
    assert!(err.to_string().contains(&format!("op #{seq}")), "{err}");
    assert_eq!(
        dump_all(&conn, &TICKET_TABLES),
        before,
        "the failed rebuild rolled back"
    );

    let report = store.doctor().unwrap();
    assert!(!report.is_healthy());
    assert!(
        report
            .replay_error
            .as_deref()
            .is_some_and(|e| e.contains("shipped")),
        "{report:#?}"
    );
    assert!(report.drift.is_empty(), "drift is unknown, not reported");
}

#[test]
fn an_empty_store_is_healthy_and_rebuilds_to_nothing() {
    let (_dir, mut store) = store();
    let report = store.doctor().unwrap();
    assert!(report.is_healthy());
    assert_eq!(report.op_count, CONFIG_OPS, "only the configuration");
    assert_eq!(report.tables["ticket"], 0);
    assert!(store.rebuild().unwrap().is_empty());
}
