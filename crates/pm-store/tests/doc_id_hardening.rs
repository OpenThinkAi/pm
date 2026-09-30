//! AGT-1464 (oaudit round 2): document identity and id validation.
//!
//! - AC1/AC4: document names, ticket projects and state names are
//!   path-checked, on every ingest path — a local commit, a restore
//!   (`commit_any`) and a pull — together with the stamp checks.
//! - AC2: a backdated `project.doc_add` cannot take a document another
//!   binding holds, and a second (backdated) `project.create` is refused.
//! - AC3: a document binding whose `doc_id` is a ticket, and a ticket whose
//!   id is a bound document, are refused.
//! - AC5: a pulled `project.delete` for a project this replica still has
//!   tickets (or child projects) in applies; the tickets keep the name in
//!   their views, read no project on their rows, and `pm check` reports
//!   them — the same on every replica and after a rebuild.

use pm_core::op::{FieldSet, ProjectCreate, ProjectDocAdd, TicketCreate};
use pm_core::{
    ActorId, Finding, Hlc, Op, Payload, Priority, Project, ProjectStatus, State, StateCategory,
    Workspace,
};
use pm_store::{Store, StoreError};
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
        stale_days: 0,
        docs_owned_by: Default::default(),
    }
}

fn project(id: &str, parent: Option<&str>) -> Project {
    Project {
        id: id.into(),
        title: id.into(),
        status: ProjectStatus::InProgress,
        parent: parent.map(str::to_string),
        repos: Default::default(),
        doc: format!("# {id}\n"),
        documents: Default::default(),
    }
}

fn empty() -> (TempDir, Store) {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("pm.sqlite")).unwrap();
    (dir, store)
}

/// A workspace with project `pm` (and its design doc).
fn fresh() -> (TempDir, Store) {
    let (dir, mut store) = empty();
    store.init_workspace(&workspace(), &matt()).unwrap();
    store.put_project(&project("pm", None), &matt()).unwrap();
    (dir, store)
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

/// An op stamped just after everything `store` has seen.
fn next(store: &Store, actor: &str, entity: Ulid, payload: Payload) -> Op {
    let hlc = pm_core::Clock::from_latest(store.latest_hlc().unwrap()).send(now_ms());
    Op::new(Ulid::new(), hlc, ActorId::new(actor), entity, payload)
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

fn ticket_create(project: Option<&str>) -> Payload {
    Payload::TicketCreate(TicketCreate {
        title: "t".into(),
        state: "triage".into(),
        priority: Priority::Medium,
        project: project.map(str::to_string),
        repo: None,
        source: None,
        ext: Default::default(),
    })
}

fn doc_add(name: Option<&str>, doc_id: Ulid) -> Payload {
    Payload::ProjectDocAdd(ProjectDocAdd {
        name: name.map(str::to_string),
        doc_id,
    })
}

fn project_entity(store: &Store, id: &str) -> Ulid {
    store.project_view(id).unwrap().unwrap().id
}

fn all_ops(store: &Store) -> Vec<Op> {
    store
        .ops_since(0)
        .unwrap()
        .into_iter()
        .map(|(_, op)| op)
        .collect()
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

fn pull_source(err: StoreError) -> StoreError {
    match err {
        StoreError::Pull { source, .. } => *source,
        other => panic!("not a pull error: {other}"),
    }
}

fn assert_healthy(store: &mut Store) {
    let report = store.doctor().unwrap();
    assert!(report.is_healthy(), "{report:#?}");
    assert!(
        store.rebuild().unwrap().is_empty(),
        "a rebuild changes nothing"
    );
}

// ------------------------------------------------------------- AC1 + AC4

#[test]
fn unsafe_names_are_refused_on_every_ingest_path() {
    let (_dir, mut store) = fresh();
    let pm = project_entity(&store, "pm");

    // Local: `pm project doc add` and a hand-built commit.
    for bad in ["..", "../x", "a/../b", "/abs", ".hidden", "a\\b"] {
        assert!(
            matches!(
                store.add_named_doc("pm", bad, &matt()).unwrap_err(),
                StoreError::InvalidId(e) if e.what == "document name"
            ),
            "{bad:?}"
        );
    }
    let bad_ticket = next(&store, "matt", Ulid::new(), ticket_create(Some("../x")));
    assert!(matches!(
        store.commit(&bad_ticket).unwrap_err(),
        StoreError::InvalidId(e) if e.what == "ticket project"
    ));
    // Restore (`pm backup --restore` replays through `commit_any`).
    let bad_state = next(
        &store,
        "matt",
        Ulid::new(),
        Payload::TicketCreate(TicketCreate {
            state: "../x".into(),
            ..match ticket_create(None) {
                Payload::TicketCreate(c) => c,
                _ => unreachable!(),
            }
        }),
    );
    assert!(matches!(
        store.commit_any(&bad_state).unwrap_err(),
        StoreError::InvalidId(e) if e.what == "state name"
    ));
    let bad_doc = next(&store, "matt", pm, doc_add(Some(".."), Ulid::new()));
    assert!(matches!(
        store.commit_any(&bad_doc).unwrap_err(),
        StoreError::InvalidId(_)
    ));
    // Stamps too, locally and on restore: a year and a day ahead.
    let far = now_ms() + pm_store::PULL_MAX_FUTURE_SKEW_MS + 86_400_000;
    let future = at(far, "matt", pm, doc_add(Some("notes"), Ulid::new()));
    assert!(matches!(
        store.commit(&future).unwrap_err(),
        StoreError::InvalidStamp(_)
    ));
    let design = store.design_doc_id("pm").unwrap().unwrap();
    let future_edit = at(
        far,
        "matt",
        design,
        Payload::BodyEdit(pm_core::op::BodyEdit { update: vec![] }),
    );
    assert!(matches!(
        store.commit_any(&future_edit).unwrap_err(),
        StoreError::InvalidStamp(_)
    ));
    // A pull.
    let pulled = next(&store, "laptop", pm, doc_add(Some("x/.."), Ulid::new()));
    assert!(matches!(
        pull_source(store.apply_pulled(&[pulled]).unwrap_err()),
        StoreError::InvalidId(e) if e.what == "document name"
    ));
    // Nothing landed; honest names still do.
    store
        .add_named_doc("pm", "ideation/IDEA-1", &matt())
        .unwrap();
    store.add_named_doc("pm", "run notes", &matt()).unwrap();
    assert_eq!(
        store.project("pm").unwrap().unwrap().documents.len(),
        2,
        "only the two honest documents"
    );
    assert_healthy(&mut store);
}

// ------------------------------------------------------------------- AC2

#[test]
fn a_backdated_doc_add_cannot_take_a_bound_document() {
    let (_dir, mut store) = fresh();
    let pm = project_entity(&store, "pm");
    store.add_named_doc("pm", "notes", &matt()).unwrap();
    let design = store.design_doc_id("pm").unwrap().unwrap();
    let notes = store.named_doc_id("pm", "notes").unwrap().unwrap();

    // A peer's binding of the design doc stamped at the dawn of time,
    // plus an edit to the hostile document.
    let evil = Ulid::new();
    let batch = vec![
        at(1, "mallory", pm, doc_add(None, evil)),
        at(
            2,
            "mallory",
            evil,
            Payload::BodyEdit(pm_core::op::BodyEdit {
                update: pm_core::Body::new()
                    .diff_from_text("pwned")
                    .unwrap()
                    .into_bytes(),
            }),
        ),
    ];
    store.apply_pulled(&batch).unwrap();
    assert_eq!(store.design_doc_id("pm").unwrap(), Some(design));
    assert_eq!(store.project("pm").unwrap().unwrap().doc, "# pm\n");

    // ... and of a named document.
    let evil2 = Ulid::new();
    store
        .apply_pulled(&[at(1, "mallory", pm, doc_add(Some("notes"), evil2))])
        .unwrap();
    assert_eq!(store.named_doc_id("pm", "notes").unwrap(), Some(notes));
    assert_healthy(&mut store);

    // A second, backdated `project.create` (which would drag the creation
    // stamp back and make the backdated bindings eligible) is refused,
    // pulled or local.
    let again = at(
        0,
        "mallory",
        pm,
        Payload::ProjectCreate(ProjectCreate {
            id: "pm".into(),
            title: "pm".into(),
            status: ProjectStatus::InProgress,
            parent: None,
            doc_id: Some(Ulid::new()),
        }),
    );
    assert!(matches!(
        pull_source(store.apply_pulled(std::slice::from_ref(&again)).unwrap_err()),
        StoreError::DuplicateProjectCreate { project } if project == pm
    ));
    assert!(matches!(
        store.commit(&again).unwrap_err(),
        StoreError::DuplicateProjectCreate { .. }
    ));
    assert_eq!(store.design_doc_id("pm").unwrap(), Some(design));
}

/// Every replica ends on the same documents whatever order it pulls the
/// honest and the backdated bindings in.
#[test]
fn doc_identity_with_backdated_bindings_converges_across_replicas() {
    let (_dir, mut origin) = fresh();
    let pm = project_entity(&origin, "pm");
    origin.add_named_doc("pm", "notes", &matt()).unwrap();
    let mut ops = all_ops(&origin);
    ops.push(at(1, "mallory", pm, doc_add(Some("notes"), Ulid::new())));
    ops.push(at(1, "mallory", pm, doc_add(None, Ulid::new())));
    let mut reversed = ops.clone();
    reversed.reverse();
    let mut results = Vec::new();
    for batch in [ops, reversed] {
        let (_d, mut replica) = empty();
        replica.apply_pulled(&batch).unwrap();
        results.push((
            replica.design_doc_id("pm").unwrap(),
            replica.named_doc_id("pm", "notes").unwrap(),
            replica.project("pm").unwrap(),
        ));
        assert_healthy(&mut replica);
    }
    assert_eq!(results[0], results[1]);
    assert_eq!(results[0].0, origin.design_doc_id("pm").unwrap());
    assert_eq!(results[0].1, origin.named_doc_id("pm", "notes").unwrap());
}

// ------------------------------------------------------------------- AC3

#[test]
fn tickets_and_documents_cannot_share_an_id() {
    let (_dir, mut store) = fresh();
    let pm = project_entity(&store, "pm");
    let ticket = Ulid::new();
    store
        .commit(&next(&store, "matt", ticket, ticket_create(Some("pm"))))
        .unwrap();

    // A pulled binding of the ticket's id, as a named or design document.
    for name in [Some("notes"), None] {
        let bind = next(&store, "laptop", pm, doc_add(name, ticket));
        assert!(matches!(
            pull_source(store.apply_pulled(&[bind]).unwrap_err()),
            StoreError::EntityInUse { entity, holder: "a ticket" } if entity == ticket
        ));
    }
    // ... or of a project's own id.
    let bind = next(&store, "laptop", pm, doc_add(Some("notes"), pm));
    assert!(matches!(
        pull_source(store.apply_pulled(&[bind]).unwrap_err()),
        StoreError::EntityInUse {
            holder: "a project",
            ..
        }
    ));

    // And the reverse: a ticket whose id is a bound document, or a project.
    let design = store.design_doc_id("pm").unwrap().unwrap();
    for (entity, holder) in [(design, "a project document"), (pm, "a project")] {
        let create = next(&store, "laptop", entity, ticket_create(Some("pm")));
        assert!(matches!(
            pull_source(store.apply_pulled(std::slice::from_ref(&create)).unwrap_err()),
            StoreError::EntityInUse { entity: e, holder: h } if e == entity && h == holder
        ));
        assert!(matches!(
            store.commit(&create).unwrap_err(),
            StoreError::EntityInUse { .. }
        ));
    }
    // The ticket's description still takes the ticket path.
    let t = store.ticket(ticket).unwrap().unwrap();
    assert_eq!(t.project.as_deref(), Some("pm"));
    assert_healthy(&mut store);
}

// ------------------------------------------------------------------- AC5

#[test]
fn a_pulled_delete_of_a_project_with_local_tickets_applies_and_converges() {
    // A and B share project `old` (and B a child project `kid` of it).
    let (_a_dir, mut a) = fresh();
    a.put_project(&project("old", None), &matt()).unwrap();
    let (_b_dir, mut b) = empty();
    b.apply_pulled(&all_ops(&a)).unwrap();

    // B, offline: a ticket filed in `old`, and a child project.
    let b_seq = last_seq(&b);
    let ticket = Ulid::new();
    b.commit(&next(&b, "bob", ticket, ticket_create(Some("old"))))
        .unwrap();
    b.put_project(&project("kid", Some("old")), &ActorId::new("bob"))
        .unwrap();

    // A, which has nothing in `old`, deletes it.
    let a_seq = last_seq(&a);
    a.delete_project("old", &matt()).unwrap();
    let delete = ops_after(&a, a_seq);
    assert_eq!(delete.len(), 1);

    // B pulls the delete: the tombstone applies instead of failing the
    // batch (for good) on R2.
    b.apply_pulled(&delete).unwrap();
    assert!(b.project("old").unwrap().is_none());
    assert_eq!(b.ticket(ticket).unwrap().unwrap().project, None);
    assert_eq!(b.project("kid").unwrap().unwrap().parent, None);
    assert_eq!(
        b.deleted_project_refs().unwrap(),
        [(ticket, "old".to_string())]
    );
    let ws = b.workspace().unwrap().unwrap();
    let findings = b.check(&ws, now_ms(), None).unwrap();
    assert!(
        findings.contains(&Finding::DeletedProject {
            ticket,
            project: "old".into()
        }),
        "{findings:?}"
    );
    assert!(
        !findings.contains(&Finding::NoProject { ticket }),
        "reported as a deleted project, not R1"
    );
    // The ticket is still editable locally.
    b.commit(&next(
        &b,
        "bob",
        ticket,
        Payload::FieldSet(FieldSet::Title("still here".into())),
    ))
    .unwrap();
    // ... but not refiled into the deleted project.
    assert!(matches!(
        b.commit(&next(
            &b,
            "bob",
            ticket,
            Payload::FieldSet(FieldSet::Project(Some("old".into()))),
        ))
        .unwrap_err(),
        StoreError::UnknownProject { .. }
    ));
    assert_healthy(&mut b);

    // A pulls B's offline work: the ticket and child land the same way.
    a.apply_pulled(&ops_after(&b, b_seq)).unwrap();
    assert_eq!(a.ticket(ticket).unwrap(), b.ticket(ticket).unwrap());
    assert_eq!(a.project("kid").unwrap(), b.project("kid").unwrap());
    assert_eq!(
        a.deleted_project_refs().unwrap(),
        b.deleted_project_refs().unwrap()
    );
    assert_healthy(&mut a);

    // A project of that slug again (a new identity): the references
    // point at it once more, on both replicas and after a rebuild.
    let a_seq = last_seq(&a);
    a.put_project(&project("old", None), &matt()).unwrap();
    assert_eq!(
        a.ticket(ticket).unwrap().unwrap().project.as_deref(),
        Some("old")
    );
    assert_eq!(
        a.project("kid").unwrap().unwrap().parent.as_deref(),
        Some("old")
    );
    assert!(a.deleted_project_refs().unwrap().is_empty());
    assert_healthy(&mut a);
    b.apply_pulled(&ops_after(&a, a_seq)).unwrap();
    assert_eq!(a.ticket(ticket).unwrap(), b.ticket(ticket).unwrap());
    assert_eq!(a.project("kid").unwrap(), b.project("kid").unwrap());
    assert_healthy(&mut b);
}

/// A local `pm project delete` still refuses while tickets are in the
/// project (R2): only a pulled tombstone skips it.
#[test]
fn a_local_delete_still_refuses_a_project_with_tickets() {
    let (_dir, mut store) = fresh();
    store
        .commit(&next(
            &store,
            "matt",
            Ulid::new(),
            ticket_create(Some("pm")),
        ))
        .unwrap();
    assert!(matches!(
        store.delete_project("pm", &matt()).unwrap_err(),
        StoreError::ProjectHasTickets { .. }
    ));
}
