//! Project documents as op-derived text (AGT-1344): `create_project`
//! assigns the design doc's stable id, `add_named_doc` + `commit_doc_edit`
//! cover a named document the same way, `delete_project` is refused while
//! a ticket or a child project still references it (AC4), and the doctor
//! replay this ticket extends (`replay_project_docs`) reproduces the
//! cached text and catches a row corrupted behind pm's back.

use std::collections::BTreeSet;

use pm_core::op::{BodyEdit, TicketCreate};
use pm_core::{ActorId, Body, Hlc, Op, Payload, Priority, State, StateCategory, Workspace};
use pm_store::{Store, StoreError};
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
    }
}

fn store() -> (TempDir, Store) {
    let dir = tempfile::tempdir().unwrap();
    let mut store = Store::open(dir.path().join("pm.sqlite")).unwrap();
    store.init_workspace(&workspace()).unwrap();
    (dir, store)
}

/// A `body.edit` op for `entity`, diffing from `before` to `after` with a
/// fresh actor-owned replica the way `pm project edit`/`doc add` do:
/// import the current snapshot (if any), then diff to the new text.
fn body_edit_op(entity: Ulid, wall_ms: u64, before: Option<&[u8]>, after: &str) -> Op {
    let mut body = Body::new();
    if let Some(snapshot) = before {
        body.apply(&pm_core::BodyUpdate::from_bytes(snapshot.to_vec()))
            .unwrap();
    }
    let update = body.diff_from_text(after).unwrap();
    Op::new(
        Ulid::new(),
        Hlc::new(wall_ms, 0),
        ActorId::new("matt"),
        entity,
        Payload::BodyEdit(BodyEdit {
            update: update.into_bytes(),
        }),
    )
}

fn create_ticket(id: Ulid, wall_ms: u64, project: &str) -> Op {
    Op::new(
        Ulid::new(),
        Hlc::new(wall_ms, 0),
        ActorId::new("matt"),
        id,
        Payload::TicketCreate(TicketCreate {
            title: "t".into(),
            state: "triage".into(),
            priority: Priority::Medium,
            project: Some(project.into()),
            repo: None,
            source: None,
            ext: Default::default(),
        }),
    )
}

// ---------------------------------------------------------------- create

#[test]
fn create_project_assigns_a_stable_doc_id_and_starts_empty() {
    let (_dir, mut store) = store();
    let doc_id = store
        .create_project("pm", "pm", &BTreeSet::new(), None)
        .unwrap();
    assert_eq!(store.design_doc_id("pm").unwrap(), Some(doc_id));
    let project = store.project("pm").unwrap().unwrap();
    assert_eq!(project.doc, "");
    assert_eq!(project.title, "pm");
}

#[test]
fn create_project_rejects_a_duplicate_id() {
    let (_dir, mut store) = store();
    store
        .create_project("pm", "pm", &BTreeSet::new(), None)
        .unwrap();
    let err = store
        .create_project("pm", "again", &BTreeSet::new(), None)
        .unwrap_err();
    assert!(matches!(err, StoreError::DuplicateProject { id } if id == "pm"));
}

#[test]
fn create_project_checks_the_parent_exists() {
    let (_dir, mut store) = store();
    let err = store
        .create_project("child", "child", &BTreeSet::new(), Some("nope"))
        .unwrap_err();
    assert!(matches!(err, StoreError::UnknownProject { project } if project == "nope"));

    store
        .create_project("parent", "parent", &BTreeSet::new(), None)
        .unwrap();
    store
        .create_project("child", "child", &BTreeSet::new(), Some("parent"))
        .unwrap();
    assert_eq!(
        store.project("child").unwrap().unwrap().parent,
        Some("parent".into())
    );
}

// ------------------------------------------------------------ doc edits

#[test]
fn commit_doc_edit_materializes_the_design_doc_and_keeps_merging_across_reopens() {
    let (dir, mut store) = store();
    let doc_id = store
        .create_project("pm", "pm", &BTreeSet::new(), None)
        .unwrap();

    let first = body_edit_op(doc_id, 1, None, "# pm\n\nv1");
    let text = store.commit_doc_edit(doc_id, &first).unwrap();
    assert_eq!(text, "# pm\n\nv1");
    assert_eq!(store.project("pm").unwrap().unwrap().doc, "# pm\n\nv1");

    let snapshot = store
        .doc_view(doc_id)
        .unwrap()
        .unwrap()
        .body
        .snapshot()
        .unwrap()
        .into_bytes();
    let second = body_edit_op(doc_id, 2, Some(&snapshot), "# pm\n\nv1 v2");
    store.commit_doc_edit(doc_id, &second).unwrap();
    assert_eq!(store.project("pm").unwrap().unwrap().doc, "# pm\n\nv1 v2");

    // Reopening replays nothing new but the cached text must survive.
    drop(store);
    let store = Store::open(dir.path().join("pm.sqlite")).unwrap();
    assert_eq!(store.project("pm").unwrap().unwrap().doc, "# pm\n\nv1 v2");
    assert_eq!(
        store.doc_view(doc_id).unwrap().unwrap().text(),
        "# pm\n\nv1 v2"
    );
}

#[test]
fn a_duplicate_doc_edit_op_is_refused_and_changes_nothing() {
    let (_dir, mut store) = store();
    let doc_id = store
        .create_project("pm", "pm", &BTreeSet::new(), None)
        .unwrap();
    let edit = body_edit_op(doc_id, 1, None, "once");
    store.commit_doc_edit(doc_id, &edit).unwrap();
    let err = store.commit_doc_edit(doc_id, &edit).unwrap_err();
    assert!(matches!(err, StoreError::DuplicateOp { .. }));
    assert_eq!(store.project("pm").unwrap().unwrap().doc, "once");
}

#[test]
fn add_named_doc_then_commit_doc_edit_materializes_project_doc_body() {
    let (_dir, mut store) = store();
    store
        .create_project("pm", "pm", &BTreeSet::new(), None)
        .unwrap();
    let doc_id = store.add_named_doc("pm", "research/spike").unwrap();
    assert_eq!(
        store.named_doc_id("pm", "research/spike").unwrap(),
        Some(doc_id)
    );

    let edit = body_edit_op(doc_id, 1, None, "spike notes");
    store.commit_doc_edit(doc_id, &edit).unwrap();

    let project = store.project("pm").unwrap().unwrap();
    assert_eq!(
        project.documents.get("research/spike").map(String::as_str),
        Some("spike notes")
    );
    // The design doc itself is untouched.
    assert_eq!(project.doc, "");
}

#[test]
fn add_named_doc_rejects_a_duplicate_name_and_a_missing_project() {
    let (_dir, mut store) = store();
    store
        .create_project("pm", "pm", &BTreeSet::new(), None)
        .unwrap();
    store.add_named_doc("pm", "notes").unwrap();
    let err = store.add_named_doc("pm", "notes").unwrap_err();
    assert!(matches!(
        err,
        StoreError::DuplicateDocument { project, name }
            if project == "pm" && name == "notes"
    ));

    let err = store.add_named_doc("nope", "notes").unwrap_err();
    assert!(matches!(err, StoreError::UnknownProject { project } if project == "nope"));
}

// ------------------------------------------------------------- delete

#[test]
fn delete_project_is_refused_while_it_has_tickets() {
    let (_dir, mut store) = store();
    store
        .create_project("pm", "pm", &BTreeSet::new(), None)
        .unwrap();
    store.commit(&create_ticket(Ulid::new(), 1, "pm")).unwrap();

    let err = store.delete_project("pm").unwrap_err();
    assert!(matches!(err, StoreError::ProjectHasTickets { project } if project == "pm"));
    assert!(store.project("pm").unwrap().is_some());
}

#[test]
fn delete_project_is_refused_while_it_has_children() {
    let (_dir, mut store) = store();
    store
        .create_project("parent", "parent", &BTreeSet::new(), None)
        .unwrap();
    store
        .create_project("child", "child", &BTreeSet::new(), Some("parent"))
        .unwrap();

    let err = store.delete_project("parent").unwrap_err();
    assert!(matches!(err, StoreError::ProjectHasChildren { project } if project == "parent"));
}

#[test]
fn delete_project_removes_it_and_its_document_view_rows() {
    let (_dir, mut store) = store();
    let doc_id = store
        .create_project("pm", "pm", &BTreeSet::new(), None)
        .unwrap();
    store
        .commit_doc_edit(doc_id, &body_edit_op(doc_id, 1, None, "text"))
        .unwrap();

    store.delete_project("pm").unwrap();
    assert!(store.project("pm").unwrap().is_none());
    assert!(store.doc_view(doc_id).unwrap().is_none());
}

// ------------------------------------------------------------- doctor

#[test]
fn doctor_rebuild_reproduces_project_doc_bodies_and_repairs_corruption() {
    let (dir, mut store) = store();
    let doc_id = store
        .create_project("pm", "pm", &BTreeSet::new(), None)
        .unwrap();
    store
        .commit_doc_edit(doc_id, &body_edit_op(doc_id, 1, None, "design"))
        .unwrap();

    let report = store.doctor().unwrap();
    assert!(report.is_healthy(), "{report:#?}");

    let rebuilt = store.rebuild().unwrap();
    assert!(
        rebuilt.is_empty(),
        "a clean rebuild changes nothing: {rebuilt:#?}"
    );

    // Corrupt the cached column directly, behind pm's back.
    let conn = rusqlite::Connection::open(dir.path().join("pm.sqlite")).unwrap();
    conn.execute("UPDATE project SET doc = 'tampered'", [])
        .unwrap();
    drop(conn);

    let mut store = Store::open(dir.path().join("pm.sqlite")).unwrap();
    let report = store.doctor().unwrap();
    assert!(!report.is_healthy());
    assert_eq!(report.drift.tables.len(), 1);
    assert_eq!(report.drift.tables[0].table, "project");
    assert_eq!(report.drift.tables[0].changed[0].columns[0].column, "doc");

    let diff = store.rebuild().unwrap();
    assert_eq!(diff.tables[0].table, "project");
    assert_eq!(store.project("pm").unwrap().unwrap().doc, "design");
    assert!(store.doctor().unwrap().is_healthy());
}

#[test]
fn doctor_rebuild_leaves_a_directly_written_document_alone() {
    let (dir, mut store) = store();
    // put_project (not create_project): no doc_id, so it's not op-derived
    // and doctor/rebuild must not touch it (config.rs, project.rs docs).
    store
        .put_project(&pm_core::Project {
            id: "legacy".into(),
            title: "legacy".into(),
            status: pm_core::ProjectStatus::InProgress,
            parent: None,
            repos: Default::default(),
            doc: "# written directly\n".into(),
            documents: Default::default(),
        })
        .unwrap();
    assert_eq!(store.design_doc_id("legacy").unwrap(), None);

    let report = store.doctor().unwrap();
    assert!(report.is_healthy(), "{report:#?}");
    let diff = store.rebuild().unwrap();
    assert!(diff.is_empty(), "{diff:#?}");
    assert_eq!(
        store.project("legacy").unwrap().unwrap().doc,
        "# written directly\n"
    );
    let _ = dir; // keep the tempdir alive for the whole test
}
