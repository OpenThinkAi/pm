//! AGT-1385: the configuration tables are op-derived. `Store::commit` of
//! a config op appends and materializes in one transaction (AC1), migration
//! 0007 backfills config ops for a database written before this ticket so
//! it becomes op-derived with no data change (AC2), and `pm doctor
//! --rebuild` replays config ops too (AC3).

use std::collections::BTreeMap;

use pm_core::op::{ProjectSet, StateUpsert, TicketCreate, WorkspaceSet};
use pm_core::{
    ActorId, Hlc, Op, Payload, Priority, Project, ProjectStatus, State, StateCategory, Workspace,
};
use pm_store::{CONFIG_TABLES, MIGRATE_ACTOR, SCHEMA_VERSION, Store, StoreError};
use rusqlite::Connection;
use rusqlite::types::Value;
use tempfile::TempDir;
use ulid::Ulid;

fn matt() -> ActorId {
    ActorId::new("matt")
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
                name: "done".into(),
                category: StateCategory::Completed,
                position: 2,
            },
        ],
        gate_labels: ["manual".to_string()].into(),
        model_labels: [("model:fable-5".to_string(), "fable".to_string())].into(),
        template_sections: vec!["Problem Statement".into()],
        stale_days: 30,
    }
}

fn project(id: &str, parent: Option<&str>) -> Project {
    Project {
        id: id.into(),
        title: format!("{id} title"),
        status: ProjectStatus::InProgress,
        parent: parent.map(str::to_string),
        repos: [format!("OpenThinkAi/{id}")].into(),
        doc: format!("# {id}\n"),
        documents: [("notes".to_string(), "n\n".to_string())].into(),
    }
}

/// A workspace with two projects (one a child), two tickets (one
/// assigned, so `actor` has more than the ops' own actors) and a number
/// floor — every configuration table populated.
fn populated() -> (TempDir, Store) {
    let dir = tempfile::tempdir().unwrap();
    let mut store = Store::open(dir.path().join("pm.sqlite")).unwrap();
    store.init_workspace(&workspace(), &matt()).unwrap();
    store.put_project(&project("pm", None), &matt()).unwrap();
    store
        .put_project(&project("pm-hub", Some("pm")), &matt())
        .unwrap();
    store.raise_number_floor(1300).unwrap();
    for (n, project) in [(1, "pm"), (2, "pm-hub")] {
        let id = Ulid::new();
        store
            .commit(&Op::new(
                Ulid::new(),
                Hlc::new(1_000 + n, 0),
                ActorId::new("claude:pm-build"),
                id,
                Payload::TicketCreate(TicketCreate {
                    title: format!("t{n}"),
                    state: "triage".into(),
                    priority: Priority::Medium,
                    project: Some(project.into()),
                    repo: None,
                    source: None,
                    ext: Default::default(),
                }),
            ))
            .unwrap();
        store.allocate_number(id, &matt()).unwrap();
    }
    (dir, store)
}

fn raw(dir: &TempDir) -> Connection {
    Connection::open(dir.path().join("pm.sqlite")).unwrap()
}

fn dump(conn: &Connection, table: &str, order: &str) -> Vec<Vec<Value>> {
    let mut stmt = conn
        .prepare(&format!("SELECT * FROM {table} ORDER BY {order}"))
        .unwrap();
    let width = stmt.column_count();
    stmt.query_map([], |r| (0..width).map(|i| r.get::<_, Value>(i)).collect())
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

/// The configuration rows as a pre-AGT-1385 binary left them: every
/// column that existed at schema 6, keyed so a comparison ignores rowid
/// order.
fn legacy_config(conn: &Connection) -> BTreeMap<&'static str, Vec<Vec<Value>>> {
    let mut out = BTreeMap::new();
    for (table, columns, order) in [
        (
            "workspace",
            "id, prefix, gate_labels, model_labels, template_sections, stale_days, number_floor",
            "id",
        ),
        ("state", "name, category, position", "name"),
        ("actor", "id, kind", "id"),
        (
            "project",
            "id, title, status, parent, repos, doc, doc_id",
            "id",
        ),
        (
            "project_doc",
            "project, name, body, doc_id",
            "project, name",
        ),
    ] {
        let mut stmt = conn
            .prepare(&format!("SELECT {columns} FROM {table} ORDER BY {order}"))
            .unwrap();
        let width = stmt.column_count();
        let rows = stmt
            .query_map([], |r| (0..width).map(|i| r.get::<_, Value>(i)).collect())
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        out.insert(table, rows);
    }
    out
}

/// Turns a current database into what a schema-6 binary would have left:
/// no config ops in the log, no views, no `project.ulid`, schema version
/// 6. Migration 0007 then runs on the next open.
fn downgrade_to_schema_6(conn: &Connection) {
    conn.execute_batch(
        "DELETE FROM ops WHERE kind IN ('workspace.set', 'state.upsert', 'actor.upsert',
                                         'project.create', 'project.set', 'project.doc_add');
         DELETE FROM workspace_view;
         DELETE FROM project_view;
         DROP TABLE project_doc_owner;
         UPDATE project SET ulid = NULL;
         DELETE FROM schema_version WHERE version >= 7;",
    )
    .unwrap();
}

// ---- AC2: the backfill ----

#[test]
fn migration_0007_backfills_config_ops_with_no_data_change_and_doctor_is_clean() {
    let (dir, store) = populated();
    drop(store);
    let conn = raw(&dir);
    let before = legacy_config(&conn);
    let tickets_before = dump(&conn, "ticket", "id");
    downgrade_to_schema_6(&conn);
    let ticket_ops: i64 = conn
        .query_row("SELECT COUNT(*) FROM ops", [], |r| r.get(0))
        .unwrap();
    let oldest: i64 = conn
        .query_row("SELECT MIN(hlc_wall_ms) FROM ops", [], |r| r.get(0))
        .unwrap();
    drop(conn);

    let mut store = Store::open(dir.path().join("pm.sqlite")).unwrap();
    assert_eq!(store.schema_version().unwrap(), SCHEMA_VERSION);
    let conn = raw(&dir);
    // No data change: the same rows, plus `migrate` in `actor` (the ops
    // it wrote reference it).
    let mut after = legacy_config(&conn);
    let actors = after.get_mut("actor").unwrap();
    let migrate = actors
        .iter()
        .position(|row| row[0] == Value::Text(MIGRATE_ACTOR.into()))
        .expect("the migrate actor row");
    assert_eq!(actors.remove(migrate)[1], Value::Text("human".into()));
    assert_eq!(after, before);
    assert_eq!(dump(&conn, "ticket", "id"), tickets_before);

    // One op per field, gate label, model label, state and actor; one
    // create per project plus one per repo; all by `migrate`, all
    // stamped below the oldest ticket op, all at the end of the log.
    let backfilled: Vec<(i64, Op)> = store
        .ops_since(ticket_ops)
        .unwrap()
        .into_iter()
        .filter(|(_, op)| op.payload.is_config())
        .collect();
    let kinds: Vec<&str> = backfilled.iter().map(|(_, op)| op.kind()).collect();
    let count = |kind: &str| kinds.iter().filter(|k| **k == kind).count();
    assert_eq!(
        count("workspace.set"),
        3 + 1 + 1,
        "prefix, template, stale; manual; model label"
    );
    assert_eq!(count("state.upsert"), 2);
    assert_eq!(count("actor.upsert"), 2, "matt and claude:pm-build");
    assert_eq!(count("project.create"), 2);
    assert_eq!(count("project.set"), 2, "one repo each");
    assert_eq!(
        count("project.doc_add"),
        4,
        "migration 0008: a design doc and `notes` each"
    );
    assert_eq!(kinds.len(), backfilled.len());
    assert_eq!(
        store.ops_since(0).unwrap().len(),
        ticket_ops as usize + backfilled.len(),
        "the backfill is the whole tail of the log"
    );
    for (_, op) in &backfilled {
        assert_eq!(op.actor, ActorId::new(MIGRATE_ACTOR));
        assert!(
            (op.hlc.wall_ms as i64) < oldest,
            "{:?} is not below the oldest op ({oldest})",
            op.hlc
        );
    }
    let projects = store.projects().unwrap();
    assert_eq!(projects.len(), 2);
    assert!(
        store.project_view("pm").unwrap().is_some()
            && store.project_view("pm-hub").unwrap().is_some()
    );
    let parent_first = kinds
        .iter()
        .position(|k| *k == "project.create")
        .map(|i| &backfilled[i].1);
    assert!(matches!(
        parent_first.map(|op| &op.payload),
        Some(Payload::ProjectCreate(c)) if c.id == "pm"
    ));

    // It all counts as outbox: the first push seeds the hub with it.
    assert_eq!(
        store.sync_status().unwrap().outbox as usize,
        ticket_ops as usize + backfilled.len()
    );

    // And the tables are now op-derived: the replay reproduces them.
    let report = store.doctor().unwrap();
    assert!(report.is_healthy(), "{report:#?}");
    assert!(store.rebuild().unwrap().is_empty());
    assert_eq!(store.number_floor().unwrap(), 1300);

    // Re-running the migration (a rolled-back version) changes nothing.
    conn.execute("DELETE FROM schema_version WHERE version >= 7", [])
        .unwrap();
    drop(conn);
    let store = Store::open(dir.path().join("pm.sqlite")).unwrap();
    assert_eq!(
        store.ops_since(0).unwrap().len(),
        ticket_ops as usize + backfilled.len()
    );
}

// ---- AC1 / AC3: commit and replay ----

/// A later config write wins over the backfill (its stamps are below
/// every real op), and a rebuild replays both in order.
#[test]
fn a_config_change_after_the_backfill_wins_and_replays() {
    let (dir, store) = populated();
    drop(store);
    let conn = raw(&dir);
    downgrade_to_schema_6(&conn);
    drop(conn);
    let mut store = Store::open(dir.path().join("pm.sqlite")).unwrap();
    let ws = store.workspace().unwrap().unwrap();

    let mut wanted = ws.clone();
    wanted.stale_days = 14;
    wanted.states.push(State {
        name: "qa".into(),
        category: StateCategory::Started,
        position: 1,
    });
    assert_eq!(store.init_workspace(&wanted, &matt()).unwrap(), 2);
    store
        .set_project_status("pm-hub", ProjectStatus::Complete, &matt())
        .unwrap();
    let now = store.workspace().unwrap().unwrap();
    assert_eq!(now.stale_days, 14);
    assert_eq!(now.states.len(), 3);
    assert_eq!(
        store.project("pm-hub").unwrap().unwrap().status,
        ProjectStatus::Complete
    );

    assert!(store.rebuild().unwrap().is_empty());
    assert_eq!(store.workspace().unwrap().unwrap(), now);
    assert!(store.doctor().unwrap().is_healthy());
}

/// Every config kind through `Store::commit` directly, then a rebuild
/// from emptied tables reproduces the rows byte for byte.
#[test]
fn every_config_kind_commits_and_rebuilds_byte_for_byte() {
    let (dir, mut store) = populated();
    let ws = store.workspace().unwrap().unwrap();
    let base = store.latest_hlc().unwrap().wall_ms + 1;
    let op = |n: u64, entity, payload| {
        Op::new(Ulid::new(), Hlc::new(base + n, 0), matt(), entity, payload)
    };
    let project = store.project_view("pm").unwrap().unwrap().id;
    let manual = store
        .workspace_view()
        .unwrap()
        .unwrap()
        .gate_labels
        .observed(&"manual".to_string());
    let ops = vec![
        op(
            1,
            ws.id,
            Payload::WorkspaceSet(WorkspaceSet::Prefix("SL".into())),
        ),
        op(
            2,
            ws.id,
            Payload::WorkspaceSet(WorkspaceSet::GateLabelRemove {
                label: "manual".into(),
                observed: manual,
            }),
        ),
        op(
            3,
            ws.id,
            Payload::WorkspaceSet(WorkspaceSet::ModelLabel {
                label: "model:fable-5".into(),
                model: None,
            }),
        ),
        op(
            4,
            ws.id,
            Payload::StateUpsert(StateUpsert {
                name: "done".into(),
                category: StateCategory::Completed,
                position: 9,
            }),
        ),
        op(
            5,
            ws.id,
            Payload::ActorUpsert(pm_core::op::ActorUpsert {
                id: ActorId::new("bot"),
                kind: pm_core::ActorKind::Agent,
            }),
        ),
        op(
            6,
            project,
            Payload::ProjectSet(ProjectSet::Title("renamed".into())),
        ),
        op(
            7,
            project,
            Payload::ProjectSet(ProjectSet::RepoAdd("OpenThinkAi/extra".into())),
        ),
    ];
    for o in &ops {
        assert!(store.commit(o).unwrap().is_none());
    }
    let ws = store.workspace().unwrap().unwrap();
    assert_eq!(ws.prefix, "SL");
    assert!(ws.gate_labels.is_empty());
    assert!(ws.model_labels.is_empty());
    assert_eq!(ws.state("done").unwrap().position, 9);
    let pm = store.project("pm").unwrap().unwrap();
    assert_eq!(pm.title, "renamed");
    assert_eq!(pm.repos.len(), 2);
    let conn = raw(&dir);
    assert_eq!(
        dump(&conn, "actor", "id")
            .iter()
            .find(|r| r[0] == Value::Text("bot".into()))
            .map(|r| r[1].clone()),
        Some(Value::Text("agent".into()))
    );

    let clean: Vec<Vec<Vec<Value>>> = CONFIG_TABLES.iter().map(|t| dump(&conn, t, "1")).collect();
    assert!(store.rebuild().unwrap().is_empty());
    let rebuilt: Vec<Vec<Vec<Value>>> = CONFIG_TABLES.iter().map(|t| dump(&conn, t, "1")).collect();
    assert_eq!(rebuilt, clean);

    // A duplicate is refused like any op; a ticket op is still a ticket op.
    assert!(matches!(
        store.commit(&ops[0]).unwrap_err(),
        StoreError::DuplicateOp { .. }
    ));
}

/// `pm project delete` is a `project.delete` op: the row goes, the log
/// keeps every op, a rebuild reproduces the deletion from the tombstone
/// (not from a remembered absence), and a hub peer learns it.
#[test]
fn a_deleted_project_is_a_tombstone_op_that_a_rebuild_honours() {
    let (_dir, mut store) = populated();
    let ulid = store.put_project(&project("gone", None), &matt()).unwrap();
    store.delete_project("gone", &matt()).unwrap();
    assert!(store.project("gone").unwrap().is_none());
    assert!(store.project_view("gone").unwrap().is_none());
    let tomb: Vec<_> = store
        .config_ops()
        .unwrap()
        .into_iter()
        .filter(|op| op.kind() == "project.delete")
        .collect();
    assert_eq!(tomb.len(), 1);
    assert_eq!(tomb[0].entity, ulid);
    assert!(store.rebuild().unwrap().is_empty());
    assert!(store.project("gone").unwrap().is_none());
    assert!(store.doctor().unwrap().is_healthy());

    // Gone is gone: deleting it again names it unknown.
    assert!(matches!(
        store.delete_project("gone", &matt()).unwrap_err(),
        StoreError::UnknownProject { .. }
    ));
    let old_ops: Vec<Op> = store
        .config_ops()
        .unwrap()
        .into_iter()
        .filter(|op| op.entity == ulid)
        .collect();
    assert_eq!(old_ops.last().unwrap().kind(), "project.delete");

    // The slug is free again, under a new identity.
    let again = store.put_project(&project("gone", None), &matt()).unwrap();
    assert_ne!(again, ulid);
    assert!(store.project("gone").unwrap().is_some());
    assert!(store.rebuild().unwrap().is_empty());
    assert!(store.project("gone").unwrap().is_some());

    // A peer applying the old identity's ops (create ... tombstone) ends
    // without the project, and a `project.set` arriving after the delete
    // does not bring it back.
    let late = Op::new(
        Ulid::new(),
        Hlc::new(u64::MAX / 4, 0),
        matt(),
        ulid,
        Payload::ProjectSet(pm_core::op::ProjectSet::Title("late".into())),
    );
    let (_peer_dir, mut peer) = populated();
    let mut batch = old_ops;
    batch.push(late);
    peer.apply_pulled(&batch).unwrap();
    assert!(peer.project("gone").unwrap().is_none());
    assert!(peer.rebuild().unwrap().is_empty());
    assert!(peer.project("gone").unwrap().is_none());
    assert!(peer.doctor().unwrap().is_healthy());
}
