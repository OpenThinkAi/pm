//! Client sync state (AGT-1393): the outbox and its pushed-through marker,
//! the pull cursor, `apply_pulled` (idempotent, order-independent within a
//! batch, all-or-nothing) and the pending-number marker.

use pm_core::op::{
    BodyEdit, Claim, CommentAdd, FieldSet, LabelAdd, LabelRemove, RelationAdd, TicketCreate,
};
use pm_core::{
    ActorId, Body, Hlc, Op, Payload, Priority, Project, ProjectStatus, Relation, RelationKind,
    State, StateCategory, Workspace,
};
use pm_store::{Pulled, Store, StoreError, SyncStatus};
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
        ],
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
    store
        .put_project(&Project {
            id: "pm".into(),
            title: "pm".into(),
            status: ProjectStatus::InProgress,
            parent: None,
            repos: Default::default(),
            doc: String::new(),
            documents: Default::default(),
        })
        .unwrap();
    (dir, store)
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
            title: "t".into(),
            state: "triage".into(),
            priority: Priority::Medium,
            project: Some("pm".into()),
            repo: None,
            source: None,
            ext: Default::default(),
        }),
    )
}

fn ids(ops: &[(i64, Op)]) -> Vec<Ulid> {
    ops.iter().map(|(_, o)| o.op_id).collect()
}

/// A foreign batch touching two tickets: creation, fields, labels (an
/// add and an observed remove), a relation between them, a comment, a
/// hub-admitted claim, a description edit and a hub-issued number.
fn foreign_batch() -> (Ulid, Ulid, Vec<Op>) {
    let (a, b) = (Ulid::new(), Ulid::new());
    let label = op(
        a,
        12,
        "laptop",
        Payload::LabelAdd(LabelAdd {
            label: "gone".into(),
        }),
    );
    let mut body = Body::with_peer(3).unwrap();
    let v1 = body.diff_from_text("# Problem\n\nv1\n").unwrap();
    let ops = vec![
        create(a, 10),
        create(b, 11),
        label.clone(),
        op(
            a,
            13,
            "laptop",
            Payload::LabelAdd(LabelAdd {
                label: "keep".into(),
            }),
        ),
        op(
            a,
            14,
            "laptop",
            Payload::LabelRemove(LabelRemove {
                label: "gone".into(),
                observed: vec![label.op_id],
            }),
        ),
        op(
            b,
            15,
            "laptop",
            Payload::RelationAdd(RelationAdd {
                relation: Relation {
                    kind: RelationKind::Blocks,
                    from: b,
                    to: a,
                },
            }),
        ),
        op(
            a,
            16,
            "laptop",
            Payload::CommentAdd(CommentAdd {
                body: "from the laptop".into(),
            }),
        ),
        op(
            a,
            17,
            "claude:laptop",
            Payload::Claim(Claim {
                state: "in-progress".into(),
                assignee: ActorId::new("claude:laptop"),
            }),
        ),
        op(
            a,
            18,
            "laptop",
            Payload::BodyEdit(BodyEdit {
                update: v1.into_bytes(),
            }),
        ),
        op(
            a,
            19,
            "laptop",
            Payload::FieldSet(FieldSet::Title("renamed".into())),
        ),
        op(a, 20, "hub", Payload::FieldSet(FieldSet::Number(41))),
        op(b, 21, "hub", Payload::FieldSet(FieldSet::Number(42))),
    ];
    (a, b, ops)
}

/// Everything a reader sees of `ticket`.
fn observed(
    store: &Store,
    ticket: Ulid,
) -> (
    pm_core::Ticket,
    Vec<pm_core::Comment>,
    Vec<pm_core::Relation>,
) {
    (
        store.ticket(ticket).unwrap().unwrap(),
        store.comments(ticket).unwrap(),
        store.relations(ticket).unwrap(),
    )
}

// ---- outbox and marker ----

#[test]
fn a_fresh_store_has_an_empty_outbox_and_a_zero_cursor() {
    let (_dir, store) = store();
    assert_eq!(store.sync_status().unwrap(), SyncStatus::default());
    assert!(store.outbox(10).unwrap().is_empty());
    assert_eq!(store.cursor().unwrap(), 0);
}

#[test]
fn local_commits_are_the_outbox_oldest_first_and_limit_caps_it() {
    let (_dir, mut store) = store();
    let ops: Vec<Op> = (1..=3).map(|n| create(Ulid::new(), n)).collect();
    for o in &ops {
        store.commit(o).unwrap();
    }
    let all = store.outbox(100).unwrap();
    assert_eq!(ids(&all), ops.iter().map(|o| o.op_id).collect::<Vec<_>>());
    assert!(all.windows(2).all(|w| w[0].0 < w[1].0));
    assert_eq!(ids(&store.outbox(2).unwrap()), ids(&all[..2]));
    assert_eq!(store.outbox_len().unwrap(), 3);
}

#[test]
fn mark_pushed_advances_the_marker_over_contiguous_acks_only() {
    let (_dir, mut store) = store();
    let ops: Vec<Op> = (1..=4).map(|n| create(Ulid::new(), n)).collect();
    for o in &ops {
        store.commit(o).unwrap();
    }
    let seqs: Vec<i64> = store.outbox(10).unwrap().iter().map(|(s, _)| *s).collect();

    // An out-of-order ack leaves the outbox but cannot move the marker
    // past the unacknowledged op below it.
    assert_eq!(store.mark_pushed(&[ops[2].op_id]).unwrap(), 1);
    assert_eq!(
        ids(&store.outbox(10).unwrap()),
        vec![ops[0].op_id, ops[1].op_id, ops[3].op_id]
    );
    assert_eq!(store.sync_status().unwrap().pushed_through, 0);

    // Closing the gap folds the early ack in.
    store.mark_pushed(&[ops[0].op_id, ops[1].op_id]).unwrap();
    assert_eq!(ids(&store.outbox(10).unwrap()), vec![ops[3].op_id]);
    assert_eq!(store.sync_status().unwrap().pushed_through, seqs[2]);

    // Idempotent; unknown ids are ignored.
    assert_eq!(store.mark_pushed(&[ops[0].op_id, Ulid::new()]).unwrap(), 1);
    assert_eq!(store.outbox_len().unwrap(), 1);

    store.mark_pushed(&[ops[3].op_id]).unwrap();
    let status = store.sync_status().unwrap();
    assert_eq!(status.outbox, 0);
    assert_eq!(status.pushed_through, seqs[3]);

    // A new local op after everything was pushed is the new outbox.
    let late = create(Ulid::new(), 9);
    store.commit(&late).unwrap();
    assert_eq!(ids(&store.outbox(10).unwrap()), vec![late.op_id]);
}

#[test]
fn the_cursor_round_trips_and_survives_reopen() {
    let (dir, mut store) = store();
    store.set_cursor(1234).unwrap();
    assert_eq!(store.cursor().unwrap(), 1234);
    drop(store);
    let store = Store::open(dir.path().join("pm.sqlite")).unwrap();
    assert_eq!(store.cursor().unwrap(), 1234);
    assert_eq!(store.sync_status().unwrap().cursor, 1234);
}

// ---- apply_pulled ----

#[test]
fn apply_pulled_commits_foreign_ops_that_never_enter_the_outbox() {
    let (_dir, mut store) = store();
    let (a, b, ops) = foreign_batch();
    let pulled = store.apply_pulled(&ops).unwrap();
    assert_eq!(
        pulled,
        Pulled {
            applied: ops.len(),
            skipped: 0
        }
    );
    let t = store.ticket(a).unwrap().unwrap();
    assert_eq!(t.title, "renamed");
    assert_eq!(t.number, Some(41));
    assert_eq!(t.state, "in-progress");
    assert_eq!(t.assignee, Some(ActorId::new("claude:laptop")));
    assert_eq!(t.labels.iter().collect::<Vec<_>>(), vec!["keep"]);
    assert_eq!(t.description, "# Problem\n\nv1\n");
    assert_eq!(store.comments(a).unwrap().len(), 1);
    assert_eq!(store.relations(a).unwrap().len(), 1);
    assert_eq!(store.ticket(b).unwrap().unwrap().number, Some(42));

    assert_eq!(store.outbox_len().unwrap(), 0);
    let report = store.doctor().unwrap();
    assert!(report.is_healthy(), "{report:#?}");
    assert_eq!(report.sync.outbox, 0);
}

#[test]
fn apply_pulled_is_idempotent() {
    let (dir, mut store) = store();
    let (a, b, ops) = foreign_batch();
    store.apply_pulled(&ops).unwrap();
    let before = (observed(&store, a), observed(&store, b));
    let op_count = store.ops_since(0).unwrap().len();
    let conn = rusqlite::Connection::open(dir.path().join("pm.sqlite")).unwrap();
    let view: String = conn
        .query_row(
            "SELECT view FROM ticket_view WHERE ticket = ?1",
            [a.to_string()],
            |r| r.get(0),
        )
        .unwrap();

    // The same batch again, then a superset with a duplicate inside it.
    assert_eq!(
        store.apply_pulled(&ops).unwrap(),
        Pulled {
            applied: 0,
            skipped: ops.len()
        }
    );
    let mut twice = ops.clone();
    twice.extend(ops.iter().cloned());
    assert_eq!(
        store.apply_pulled(&twice).unwrap(),
        Pulled {
            applied: 0,
            skipped: twice.len()
        }
    );

    assert_eq!((observed(&store, a), observed(&store, b)), before);
    assert_eq!(store.ops_since(0).unwrap().len(), op_count);
    let view_after: String = conn
        .query_row(
            "SELECT view FROM ticket_view WHERE ticket = ?1",
            [a.to_string()],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(view_after, view, "a skipped op must not touch the view");
    assert!(store.doctor().unwrap().is_healthy());
}

#[test]
fn a_duplicate_inside_one_batch_applies_once() {
    let (_dir, mut store) = store();
    let (_a, _b, ops) = foreign_batch();
    let mut doubled = ops.clone();
    doubled.insert(1, ops[0].clone());
    assert_eq!(
        store.apply_pulled(&doubled).unwrap(),
        Pulled {
            applied: ops.len(),
            skipped: 1
        }
    );
}

#[test]
fn apply_pulled_is_order_independent() {
    let (a, b, ops) = foreign_batch();

    let (_d1, mut forward) = store();
    forward.apply_pulled(&ops).unwrap();
    let expected = (observed(&forward, a), observed(&forward, b));

    let mut reversed = ops.clone();
    reversed.reverse();
    // A fixed interleaving: odd indices first, then even — creates land
    // mid-batch, after ops that depend on them.
    let interleaved: Vec<Op> = ops
        .iter()
        .skip(1)
        .step_by(2)
        .chain(ops.iter().step_by(2))
        .cloned()
        .collect();
    // Split across two pulls, the dependent half first within each.
    let (first, second) = ops.split_at(ops.len() / 2);

    for (name, batches) in [
        ("reversed", vec![reversed]),
        ("interleaved", vec![interleaved]),
        (
            "two batches",
            vec![
                first.iter().rev().cloned().collect::<Vec<_>>(),
                second.iter().rev().cloned().collect(),
            ],
        ),
    ] {
        let (_dir, mut store) = store();
        let mut applied = 0;
        for batch in &batches {
            applied += store.apply_pulled(batch).unwrap().applied;
        }
        assert_eq!(applied, ops.len(), "{name}");
        assert_eq!(
            (observed(&store, a), observed(&store, b)),
            expected,
            "{name}"
        );
        // The log was appended in landing order, so a replay in seq order
        // reproduces the tables byte for byte.
        let report = store.doctor().unwrap();
        assert!(report.is_healthy(), "{name}: {report:#?}");
        assert_eq!(report.sync.outbox, 0, "{name}");
    }
}

#[test]
fn an_unmet_dependency_rolls_the_whole_batch_back() {
    let (_dir, mut store) = store();
    let (a, b, ops) = foreign_batch();
    // Everything but a's creation: every op on `a` waits forever.
    let orphaned: Vec<Op> = ops.into_iter().skip(1).collect();
    let err = store.apply_pulled(&orphaned).unwrap_err();
    let StoreError::Pull { source, .. } = &err else {
        panic!("{err:?}");
    };
    assert!(
        matches!(**source, StoreError::UnknownTicket { ticket } if ticket == a)
            || matches!(**source, StoreError::UnknownRelationTarget { ticket } if ticket == a),
        "{err:?}"
    );
    assert!(store.ops_since(0).unwrap().is_empty());
    // b's creation had nothing to wait on, yet did not land either.
    assert!(store.ticket(b).unwrap().is_none());
}

#[test]
fn a_non_dependency_failure_fails_fast() {
    let (_dir, mut store) = store();
    let a = Ulid::new();
    let local = create(a, 1);
    store.commit(&local).unwrap();
    store.allocate_number(a, &ActorId::new("matt")).unwrap();
    let b = Ulid::new();
    // b's number collides with a's (R5): no later op can fix that.
    let batch = vec![
        create(b, 2),
        op(b, 3, "hub", Payload::FieldSet(FieldSet::Number(1))),
    ];
    let err = store.apply_pulled(&batch).unwrap_err();
    assert!(
        matches!(&err, StoreError::Pull { source, .. }
            if matches!(**source, StoreError::DuplicateNumber { number: 1 })),
        "{err:?}"
    );
    assert!(store.ticket(b).unwrap().is_none());
}

#[test]
fn a_local_op_echoed_back_by_the_hub_leaves_the_outbox() {
    let (_dir, mut store) = store();
    let mine = create(Ulid::new(), 1);
    store.commit(&mine).unwrap();
    assert_eq!(store.outbox_len().unwrap(), 1);
    assert_eq!(
        store.apply_pulled(std::slice::from_ref(&mine)).unwrap(),
        Pulled {
            applied: 0,
            skipped: 1
        }
    );
    assert_eq!(store.outbox_len().unwrap(), 0);
}

#[test]
fn pulled_ops_interleaved_with_local_ones_keep_only_the_local_ones_outbox() {
    let (_dir, mut store) = store();
    let early = create(Ulid::new(), 1);
    store.commit(&early).unwrap();
    let (_a, _b, ops) = foreign_batch();
    store.apply_pulled(&ops).unwrap();
    let late = create(Ulid::new(), 50);
    store.commit(&late).unwrap();
    assert_eq!(
        ids(&store.outbox(100).unwrap()),
        vec![early.op_id, late.op_id]
    );
    store.mark_pushed(&[early.op_id]).unwrap();
    // The marker folds over the pulled run in one step.
    assert_eq!(ids(&store.outbox(100).unwrap()), vec![late.op_id]);
    let late_seq = store.outbox(1).unwrap()[0].0;
    assert_eq!(store.sync_status().unwrap().pushed_through, late_seq - 1);
}

#[test]
fn a_pulled_body_edit_on_a_project_document_takes_the_document_path() {
    let (_dir, mut store) = store();
    let doc = store.add_named_doc("pm", "notes").unwrap();
    let mut body = Body::with_peer(9).unwrap();
    let update = body.diff_from_text("remote notes\n").unwrap();
    let edit = op(
        doc,
        5,
        "laptop",
        Payload::BodyEdit(BodyEdit {
            update: update.into_bytes(),
        }),
    );
    store.apply_pulled(&[edit]).unwrap();
    assert_eq!(
        store.doc_view(doc).unwrap().unwrap().text(),
        "remote notes\n"
    );
    assert_eq!(store.outbox_len().unwrap(), 0);
    assert!(store.doctor().unwrap().is_healthy());
}

// ---- pending numbers ----

#[test]
fn a_pending_number_clears_when_the_hub_number_is_pulled() {
    let (_dir, mut store) = store();
    let a = Ulid::new();
    store.commit(&create(a, 1)).unwrap();
    store.mark_pending_number(a).unwrap();
    store.mark_pending_number(a).unwrap();
    assert_eq!(store.pending_numbers().unwrap(), vec![a]);
    assert_eq!(store.sync_status().unwrap().pending_numbers, 1);

    // A pull that does not number it leaves it pending.
    let (_x, _y, unrelated) = foreign_batch();
    store.apply_pulled(&unrelated).unwrap();
    assert_eq!(store.pending_numbers().unwrap(), vec![a]);

    store
        .apply_pulled(&[op(a, 30, "hub", Payload::FieldSet(FieldSet::Number(7)))])
        .unwrap();
    assert!(store.pending_numbers().unwrap().is_empty());
    assert_eq!(store.ticket(a).unwrap().unwrap().number, Some(7));
}

#[test]
fn the_pending_marker_survives_a_rebuild() {
    let (_dir, mut store) = store();
    let a = Ulid::new();
    store.commit(&create(a, 1)).unwrap();
    store.mark_pending_number(a).unwrap();
    store.rebuild().unwrap();
    assert_eq!(store.pending_numbers().unwrap(), vec![a]);
}
