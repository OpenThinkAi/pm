//! AGT-1436: a workspace upgraded from schema <= 9 has a `workspace_view`
//! row written before `WorkspaceView` gained `docs_owned_by` (AGT-1406).
//! Migration 0011 re-serializes it, and doctor's view comparison is
//! semantic, so neither path reports false drift.

use pm_core::{ActorId, State, StateCategory, Workspace};
use pm_store::Store;
use rusqlite::{Connection, params};
use serde_json::Value;
use tempfile::TempDir;
use ulid::Ulid;

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
        docs_owned_by: Default::default(),
    }
}

fn open() -> (TempDir, Store) {
    let dir = tempfile::tempdir().unwrap();
    let mut store = Store::open(dir.path().join("pm.sqlite")).unwrap();
    store
        .init_workspace(&workspace(), &ActorId::new("matt"))
        .unwrap();
    (dir, store)
}

fn raw(dir: &TempDir) -> Connection {
    Connection::open(dir.path().join("pm.sqlite")).unwrap()
}

fn view_text(conn: &Connection) -> String {
    conn.query_row("SELECT view FROM workspace_view", [], |r| r.get(0))
        .unwrap()
}

/// Rewrites the stored workspace view as a pre-AGT-1406 binary wrote it.
/// Returns the current text it replaced.
fn make_stale(conn: &Connection) -> String {
    let current = view_text(conn);
    let mut value: Value = serde_json::from_str(&current).unwrap();
    assert!(
        value
            .as_object_mut()
            .unwrap()
            .remove("docs_owned_by")
            .is_some()
    );
    conn.execute(
        "UPDATE workspace_view SET view = ?1",
        params![value.to_string()],
    )
    .unwrap();
    current
}

#[test]
fn upgrading_from_schema_9_leaves_a_doctor_clean_workspace_view() {
    let (dir, store) = open();
    drop(store);
    let conn = raw(&dir);
    let current = make_stale(&conn);
    assert!(!view_text(&conn).contains("docs_owned_by"));
    // Schema 9: forget 0010 and 0011 (both re-runnable over a database
    // that already has their shape).
    conn.execute("DELETE FROM schema_version WHERE version >= 10", [])
        .unwrap();
    drop(conn);

    let mut store = Store::open(dir.path().join("pm.sqlite")).unwrap();
    // The migration rewrote the row to exactly what a replay writes...
    assert_eq!(view_text(&raw(&dir)), current);
    // ...so doctor is clean with no --rebuild.
    let report = store.doctor().unwrap();
    assert!(report.is_healthy(), "{report:?}");
    assert!(store.rebuild().unwrap().is_empty());
}

#[test]
fn doctor_ignores_a_stale_view_spelling_but_not_a_changed_view() {
    // No migration involved: the row is stale at the current schema.
    let (dir, mut store) = open();
    let conn = raw(&dir);
    let current = make_stale(&conn);
    let report = store.doctor().unwrap();
    assert!(report.is_healthy(), "{report:?}");

    // A real difference in content is still drift.
    let mut value: Value = serde_json::from_str(&current).unwrap();
    value["prefix"]["value"] = Value::from("XXX");
    conn.execute(
        "UPDATE workspace_view SET view = ?1",
        params![value.to_string()],
    )
    .unwrap();
    let report = store.doctor().unwrap();
    assert!(!report.is_healthy());
    assert_eq!(report.drift.tables[0].table, "workspace_view");
}
