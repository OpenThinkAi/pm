//! AGT-1413: project document identity travels in ops. A project's design
//! doc is bound by its `project.create` (`doc_id`) and each named document
//! by a `project.doc_add`, so a replica that pulls a project also learns
//! where its `body.edit` ops go (AC1); migration 0008 binds every existing
//! document with no data change (AC2); and pulling a project's ops in any
//! order yields the same documents (AC3).

use std::collections::BTreeMap;

use pm_core::op::BodyEdit;
use pm_core::{
    ActorId, Body, Op, Payload, Project, ProjectStatus, State, StateCategory, Workspace,
};
use pm_store::{MIGRATE_ACTOR, SCHEMA_VERSION, Store};
use rusqlite::Connection;
use tempfile::TempDir;
use ulid::Ulid;

fn matt() -> ActorId {
    ActorId::new("matt")
}

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

fn open(dir: &TempDir) -> Store {
    Store::open(dir.path().join("pm.sqlite")).unwrap()
}

fn fresh() -> (TempDir, Store) {
    let dir = tempfile::tempdir().unwrap();
    let mut store = open(&dir);
    store.init_workspace(&workspace(), &matt()).unwrap();
    (dir, store)
}

/// A `body.edit` taking document `doc_id` to `text`, continuing its
/// history the way `pm project edit` does.
fn edit(store: &mut Store, doc_id: Ulid, text: &str, actor: &str) -> Op {
    let mut body = Body::new();
    if let Some(view) = store.doc_view(doc_id).unwrap() {
        body.apply(&view.body.snapshot().unwrap()).unwrap();
    }
    let update = body.diff_from_text(text).unwrap();
    let hlc = pm_core::Clock::from_latest(store.latest_hlc().unwrap()).send(now_ms());
    let op = Op::new(
        Ulid::new(),
        hlc,
        ActorId::new(actor),
        doc_id,
        Payload::BodyEdit(BodyEdit {
            update: update.into_bytes(),
        }),
    );
    store.commit_doc_edit(doc_id, &op).unwrap();
    op
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

fn ops_after(store: &Store, seq: i64) -> Vec<Op> {
    store
        .ops_since(seq)
        .unwrap()
        .into_iter()
        .map(|(_, op)| op)
        .collect()
}

fn last_seq(store: &Store) -> i64 {
    store
        .ops_since(0)
        .unwrap()
        .last()
        .map_or(0, |(seq, _)| *seq)
}

/// A project, its design doc's id and each named document's id.
type WithDocIds = (Project, Option<Ulid>, BTreeMap<String, Option<Ulid>>);

/// Every project with the id of each of its documents.
fn documents(store: &Store) -> Vec<WithDocIds> {
    store
        .projects()
        .unwrap()
        .into_iter()
        .map(|p| {
            let design = store.design_doc_id(&p.id).unwrap();
            let named = p
                .documents
                .keys()
                .map(|name| (name.clone(), store.named_doc_id(&p.id, name).unwrap()))
                .collect();
            (p, design, named)
        })
        .collect()
}

fn assert_healthy(store: &mut Store) {
    let report = store.doctor().unwrap();
    assert!(report.is_healthy(), "{report:#?}");
    let diff = store.rebuild().unwrap();
    assert!(diff.is_empty(), "{diff:#?}");
}

/// A deterministic shuffle (xorshift), so the test needs no rng crate.
fn shuffled<T: Clone>(items: &[T], seed: u64) -> Vec<T> {
    let mut out = items.to_vec();
    let mut x = seed | 1;
    for i in (1..out.len()).rev() {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        out.swap(i, (x % (i as u64 + 1)) as usize);
    }
    out
}

// ---- AC1: the writers bind documents with ops ----

#[test]
fn create_and_doc_add_carry_the_document_ids() {
    let (_dir, mut store) = fresh();
    let design = store
        .create_project("pm", "pm", &Default::default(), None, &matt())
        .unwrap();
    let notes = store.add_named_doc("pm", "notes", &matt()).unwrap();

    let ops = store.config_ops().unwrap();
    let create = ops
        .iter()
        .find_map(|op| match &op.payload {
            Payload::ProjectCreate(c) => Some(c),
            _ => None,
        })
        .unwrap();
    assert_eq!(create.doc_id, Some(design));
    let add = ops
        .iter()
        .find_map(|op| match &op.payload {
            Payload::ProjectDocAdd(d) => Some((op.actor.clone(), d)),
            _ => None,
        })
        .unwrap();
    assert_eq!(add.0, matt());
    assert_eq!(add.1.name.as_deref(), Some("notes"));
    assert_eq!(add.1.doc_id, notes);

    let view = store.project_view("pm").unwrap().unwrap();
    assert_eq!(view.design_doc_id(), Some(design));
    assert_eq!(view.doc_id("notes"), Some(notes));
    assert!(store.is_known_doc_id(design).unwrap());
    assert!(store.is_known_doc_id(notes).unwrap());
    assert_healthy(&mut store);
}

// ---- AC2: migration 0008 ----

/// Turns a current database into what a schema-7 binary would have left:
/// no `project.doc_add` ops, no `doc_id` on the creates, no document
/// identity in the project views, no `project_doc_owner`, schema 7 — and,
/// like a database written before AGT-1344, one design doc and one named
/// document with no `doc_id` at all, their text only in the row.
fn downgrade_to_schema_7(conn: &Connection) {
    conn.execute_batch(
        "DELETE FROM ops WHERE kind = 'project.doc_add';
         UPDATE ops SET payload = json_remove(payload, '$.doc_id') WHERE kind = 'project.create';
         UPDATE project_view SET view = json_remove(view, '$.design_doc', '$.documents');
         DROP TABLE project_doc_owner;
         DELETE FROM schema_version WHERE version >= 8;

         DELETE FROM project_doc_view WHERE doc_id = (SELECT doc_id FROM project WHERE id = 'pm-hub');
         DELETE FROM ops WHERE entity = (SELECT doc_id FROM project WHERE id = 'pm-hub');
         UPDATE project SET doc_id = NULL WHERE id = 'pm-hub';
         INSERT INTO project_doc (project, name, body, doc_id) VALUES ('pm', 'legacy', 'old text\n', NULL);",
    )
    .unwrap();
}

#[test]
fn migration_0008_binds_every_document_with_no_data_change() {
    let (dir, mut store) = fresh();
    let design = store
        .create_project("pm", "pm", &Default::default(), None, &matt())
        .unwrap();
    edit(&mut store, design, "# pm\n", "matt");
    let notes = store.add_named_doc("pm", "notes", &matt()).unwrap();
    edit(&mut store, notes, "notes\n", "matt");
    // A metadata write after the doc_add, so the view's `updated` stamp
    // is one a schema-7 log also has.
    store
        .set_project_status("pm", ProjectStatus::Complete, &matt())
        .unwrap();
    store
        .put_project(
            &Project {
                id: "pm-hub".into(),
                title: "pm-hub".into(),
                status: ProjectStatus::InProgress,
                parent: Some("pm".into()),
                repos: Default::default(),
                doc: "# pm-hub\n".into(),
                documents: Default::default(),
            },
            &matt(),
        )
        .unwrap();
    drop(store);
    let conn = Connection::open(dir.path().join("pm.sqlite")).unwrap();
    downgrade_to_schema_7(&conn);
    let kept = conn
        .query_row("SELECT MAX(seq) FROM ops", [], |r| r.get::<_, i64>(0))
        .unwrap();
    let oldest: i64 = conn
        .query_row("SELECT MIN(hlc_wall_ms) FROM ops", [], |r| r.get(0))
        .unwrap();
    drop(conn);

    let mut store = open(&dir);
    assert_eq!(store.schema_version().unwrap(), SCHEMA_VERSION);
    let before_hub = store.project("pm-hub").unwrap().unwrap();
    assert_eq!(before_hub.doc, "# pm-hub\n", "no text lost");
    let pm = store.project("pm").unwrap().unwrap();
    assert_eq!(pm.doc, "# pm\n");
    assert_eq!(pm.documents["notes"], "notes\n");
    assert_eq!(pm.documents["legacy"], "old text\n");
    // Existing ids are kept; the id-less documents got one.
    assert_eq!(store.design_doc_id("pm").unwrap(), Some(design));
    assert_eq!(store.named_doc_id("pm", "notes").unwrap(), Some(notes));
    let hub = store.design_doc_id("pm-hub").unwrap().expect("bound");
    let legacy = store.named_doc_id("pm", "legacy").unwrap().expect("bound");

    let backfilled = ops_after(&store, kept);
    let kinds: Vec<&str> = backfilled.iter().map(Op::kind).collect();
    assert_eq!(
        kinds.iter().filter(|k| **k == "project.doc_add").count(),
        4,
        "pm's design doc, notes and legacy; pm-hub's design doc"
    );
    assert_eq!(
        kinds.iter().filter(|k| **k == "body.edit").count(),
        2,
        "the id-less documents' text"
    );
    assert_eq!(kinds.len(), 6, "{kinds:?}");
    for op in &backfilled {
        assert_eq!(op.actor, ActorId::new(MIGRATE_ACTOR));
        assert!((op.hlc.wall_ms as i64) < oldest, "{:?}", op.hlc);
    }
    let view = store.project_view("pm").unwrap().unwrap();
    assert_eq!(view.design_doc_id(), Some(design));
    assert_eq!(view.doc_id("legacy"), Some(legacy));
    assert_eq!(
        store
            .project_view("pm-hub")
            .unwrap()
            .unwrap()
            .design_doc_id(),
        Some(hub)
    );
    assert_eq!(store.doc_view(hub).unwrap().unwrap().text(), "# pm-hub\n");
    assert_healthy(&mut store);

    // Re-running the migration changes nothing.
    let total = last_seq(&store);
    drop(store);
    let conn = Connection::open(dir.path().join("pm.sqlite")).unwrap();
    conn.execute("DELETE FROM schema_version WHERE version >= 8", [])
        .unwrap();
    drop(conn);
    let store = open(&dir);
    assert_eq!(last_seq(&store), total);
}

// ---- AC3: pulled in any order ----

/// A replica's whole log: a workspace, a project with a design doc and
/// two named documents, each edited (one twice), and a child project.
fn source() -> (TempDir, Store) {
    let (dir, mut store) = fresh();
    let design = store
        .create_project(
            "pm",
            "pm",
            &["OpenThinkAi/pm".to_string()].into(),
            None,
            &matt(),
        )
        .unwrap();
    edit(&mut store, design, "# pm\n\nThe design.\n", "matt");
    let notes = store.add_named_doc("pm", "notes", &matt()).unwrap();
    edit(&mut store, notes, "first\n", "matt");
    edit(&mut store, notes, "first\nsecond\n", "claude:pm-build");
    let spike = store
        .add_named_doc("pm", "research/spike", &matt())
        .unwrap();
    edit(&mut store, spike, "loro\n", "matt");
    let child = store
        .create_project("pm-hub", "pm-hub", &Default::default(), Some("pm"), &matt())
        .unwrap();
    edit(&mut store, child, "# hub\n", "matt");
    (dir, store)
}

#[test]
fn pulling_a_projects_ops_in_any_order_yields_the_same_documents() {
    let (_src_dir, source) = source();
    let expected = documents(&source);
    let all = ops_after(&source, 0);
    let (config, edits): (Vec<Op>, Vec<Op>) =
        all.iter().cloned().partition(|op| op.payload.is_config());

    let mut orders: Vec<Vec<Vec<Op>>> = vec![
        vec![all.clone()],
        vec![all.iter().rev().cloned().collect()],
        // Every body.edit ahead of the ops binding its document.
        vec![edits.iter().chain(&config).cloned().collect()],
        // The project's create and doc ops, then its body.edit ops.
        vec![config.clone(), edits.clone()],
        vec![shuffled(&config, 7), shuffled(&edits, 11)],
    ];
    for seed in 1..=8 {
        orders.push(vec![shuffled(&all, seed * 7919)]);
    }
    for batches in orders {
        let dir = tempfile::tempdir().unwrap();
        let mut replica = open(&dir);
        for batch in &batches {
            replica.apply_pulled(batch).unwrap();
        }
        assert_eq!(documents(&replica), expected);
        for (_, design, named) in &expected {
            for doc_id in design.iter().chain(named.values().flatten()) {
                assert_eq!(
                    replica.doc_view(*doc_id).unwrap().map(|v| v.text()),
                    source.doc_view(*doc_id).unwrap().map(|v| v.text())
                );
            }
        }
        assert_healthy(&mut replica);
    }
}

/// Two replicas that add the same document name offline converge on one
/// id — the earliest binding — once each pulls the other's ops, and the
/// losing binding's edits still pull (they are a document's edits, just
/// not one any row shows).
#[test]
fn a_document_added_on_two_replicas_converges() {
    let (_a_dir, mut a) = source();
    let b_dir = tempfile::tempdir().unwrap();
    let mut b = open(&b_dir);
    b.apply_pulled(&ops_after(&a, 0)).unwrap();
    let (a_seq, b_seq) = (last_seq(&a), last_seq(&b));

    let from_a = a
        .add_named_doc("pm", "plan", &ActorId::new("alice"))
        .unwrap();
    edit(&mut a, from_a, "alice's plan\n", "alice");
    let from_b = b.add_named_doc("pm", "plan", &ActorId::new("bob")).unwrap();
    edit(&mut b, from_b, "bob's plan\n", "bob");

    let a_new = ops_after(&a, a_seq);
    let b_new = ops_after(&b, b_seq);
    a.apply_pulled(&b_new).unwrap();
    b.apply_pulled(&a_new).unwrap();

    let winner = a.named_doc_id("pm", "plan").unwrap().unwrap();
    assert_eq!(b.named_doc_id("pm", "plan").unwrap(), Some(winner));
    assert!([from_a, from_b].contains(&winner));
    assert_eq!(documents(&a), documents(&b));
    let text = a.project("pm").unwrap().unwrap().documents["plan"].clone();
    let expected = if winner == from_a {
        "alice's plan\n"
    } else {
        "bob's plan\n"
    };
    assert_eq!(text, expected);
    for store in [&a, &b] {
        assert!(store.is_known_doc_id(from_a).unwrap());
        assert!(store.is_known_doc_id(from_b).unwrap());
    }
    assert_healthy(&mut a);
    assert_healthy(&mut b);
}

/// An edit to a document of a project another replica deleted still
/// pulls there: the document is known (its binding is in the deleted
/// project's view), it just has no row to show it.
#[test]
fn an_edit_to_a_deleted_projects_document_still_pulls() {
    let (_a_dir, mut a) = source();
    let b_dir = tempfile::tempdir().unwrap();
    let mut b = open(&b_dir);
    b.apply_pulled(&ops_after(&a, 0)).unwrap();
    let (a_seq, b_seq) = (last_seq(&a), last_seq(&b));

    let hub = b.design_doc_id("pm-hub").unwrap().unwrap();
    b.delete_project("pm-hub", &matt()).unwrap();
    edit(&mut a, hub, "# hub, edited\n", "matt");

    let a_new = ops_after(&a, a_seq);
    let b_new = ops_after(&b, b_seq);
    b.apply_pulled(&a_new).unwrap();
    a.apply_pulled(&b_new).unwrap();
    for store in [&mut a, &mut b] {
        assert!(store.project("pm-hub").unwrap().is_none());
        assert!(store.is_known_doc_id(hub).unwrap());
        assert!(store.doc_view(hub).unwrap().is_none());
        assert_healthy(store);
    }
    assert_eq!(documents(&a), documents(&b));
}
