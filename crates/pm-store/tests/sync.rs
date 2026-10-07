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
        docs_owned_by: Default::default(),
    }
}

/// A workspace with one project whose configuration the hub already
/// has: `init_workspace` / `put_project` commit config ops (AGT-1385),
/// and marking those pushed leaves the outbox empty for the tests below,
/// which count their own ops.
fn store() -> (TempDir, Store) {
    let dir = tempfile::tempdir().unwrap();
    let mut store = Store::open(dir.path().join("pm.sqlite")).unwrap();
    store
        .init_workspace(&workspace(), &ActorId::new("matt"))
        .unwrap();
    store
        .put_project(
            &Project {
                kind: Default::default(),
                id: "pm".into(),
                title: "pm".into(),
                status: ProjectStatus::InProgress,
                parent: None,
                repos: Default::default(),
                doc: String::new(),
                documents: Default::default(),
            },
            &ActorId::new("matt"),
        )
        .unwrap();
    let config = ids(&store.ops_since(0).unwrap());
    store.mark_pushed(&config).unwrap();
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
    assert_eq!(
        store.sync_status().unwrap(),
        SyncStatus {
            pushed_through: config_ops(&store) as i64,
            ..SyncStatus::default()
        }
    );
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
    assert_eq!(
        store.sync_status().unwrap().pushed_through,
        config_ops(&store) as i64
    );

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
            skipped: 0,
            ..Pulled::default()
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
            skipped: ops.len(),
            ..Pulled::default()
        }
    );
    let mut twice = ops.clone();
    twice.extend(ops.iter().cloned());
    assert_eq!(
        store.apply_pulled(&twice).unwrap(),
        Pulled {
            applied: 0,
            skipped: twice.len(),
            ..Pulled::default()
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
            skipped: 1,
            ..Pulled::default()
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
    assert_eq!(store.ops_since(0).unwrap().len(), config_ops(&store));
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

/// oaudit 2026-09-30: a pulled stamp that does not fit the store, has a
/// spent counter, or sits years in the future is a typed error naming
/// the op — never a panic — and nothing in the batch lands.
#[test]
fn a_pulled_op_with_an_inadmissible_stamp_is_a_typed_error() {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    let year_ahead = now + pm_store::PULL_MAX_FUTURE_SKEW_MS + 60_000;
    let cases: Vec<(&str, Hlc)> = vec![
        ("wall_ms above i64::MAX", Hlc::new(u64::MAX, 0)),
        (
            "wall_ms just above i64::MAX",
            Hlc::new(i64::MAX as u64 + 1, 0),
        ),
        ("spent counter", Hlc::new(now, u32::MAX)),
        ("far future", Hlc::new(year_ahead, 0)),
    ];
    for (case, hlc) in cases {
        let (_dir, mut store) = store();
        let before = store.ops_since(0).unwrap().len();
        let latest = store.latest_hlc().unwrap();
        let b = Ulid::new();
        let mut bad = create(b, 0);
        bad.hlc = hlc;
        let batch = vec![create(Ulid::new(), 5), bad.clone()];
        let err = store.apply_pulled(&batch).unwrap_err();
        let StoreError::Pull { op_id, source, .. } = &err else {
            panic!("{case}: {err:?}");
        };
        assert_eq!(*op_id, bad.op_id, "{case}");
        assert!(
            matches!(**source, StoreError::InvalidStamp(_)),
            "{case}: {err:?}"
        );
        assert_eq!(store.ops_since(0).unwrap().len(), before, "{case}");
        assert_eq!(
            store.latest_hlc().unwrap(),
            latest,
            "{case}: clock untouched"
        );
        assert!(store.ticket(b).unwrap().is_none(), "{case}");
    }

    // A stamp a day or two ahead (a replica whose clock runs behind) and
    // one from long ago (a seeded history) still apply.
    let (_dir, mut store) = store();
    let mut ahead = create(Ulid::new(), 0);
    ahead.hlc = Hlc::new(now + 2 * pm_core::MAX_FUTURE_SKEW_MS, 0);
    let old = create(Ulid::new(), 1);
    let pulled = store.apply_pulled(&[ahead, old]).unwrap();
    assert_eq!(pulled.applied, 2);
}

/// AGT-1450: a pulled prefix or project id that would escape a directory
/// is refused before it is stored.
#[test]
fn a_pulled_op_with_a_path_unsafe_id_is_a_typed_error() {
    let (_dir, mut store) = store();
    let ws = store.workspace().unwrap().unwrap().id;
    let bad_prefix = Op::new(
        Ulid::new(),
        Hlc::new(50, 0),
        ActorId::new("laptop"),
        ws,
        Payload::WorkspaceSet(pm_core::op::WorkspaceSet::Prefix("../x".into())),
    );
    let bad_project = Op::new(
        Ulid::new(),
        Hlc::new(51, 0),
        ActorId::new("laptop"),
        Ulid::new(),
        Payload::ProjectCreate(pm_core::op::ProjectCreate {
            kind: Default::default(),
            id: "a/b".into(),
            title: "t".into(),
            status: ProjectStatus::InProgress,
            parent: None,
            doc_id: None,
        }),
    );
    for bad in [bad_prefix, bad_project] {
        let before = store.ops_since(0).unwrap().len();
        let err = store.apply_pulled(std::slice::from_ref(&bad)).unwrap_err();
        assert!(
            matches!(&err, StoreError::Pull { op_id, source, .. }
                if *op_id == bad.op_id && matches!(**source, StoreError::InvalidId(_))),
            "{err:?}"
        );
        assert_eq!(store.ops_since(0).unwrap().len(), before);
    }
    assert_eq!(store.workspace().unwrap().unwrap().prefix, "AGT");
}

/// The store's own codec: a stamp that does not fit an SQLite integer is
/// refused with a typed error on any commit path, not a panic.
#[test]
fn committing_an_unstorable_stamp_is_a_typed_error() {
    let (_dir, mut store) = store();
    let mut bad = create(Ulid::new(), 0);
    bad.hlc = Hlc::new(u64::MAX, 0);
    let err = store.commit(&bad).unwrap_err();
    assert!(matches!(err, StoreError::InvalidStamp(_)), "{err:?}");
    assert!(store.ticket(bad.entity).unwrap().is_none());
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
            skipped: 1,
            ..Pulled::default()
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
    let doc = store
        .add_named_doc("pm", "notes", &ActorId::new("matt"))
        .unwrap();
    let local = store.outbox_len().unwrap();
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
    assert_eq!(
        store.outbox_len().unwrap(),
        local,
        "only the local doc_add is outbox"
    );
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

/// AGT-1398: `commit_batch_pending` lands the ops and the pending flags
/// together — a numberless ticket, flagged, in one transaction — and a
/// failing batch leaves neither behind.
#[test]
fn commit_batch_pending_creates_unnumbered_and_flags_atomically() {
    let (_dir, mut store) = store();
    let before = store.sync_status().unwrap().outbox;
    let (a, b) = (Ulid::new(), Ulid::new());
    let tickets = store
        .commit_batch_pending(&[create(a, 1), create(b, 2)], &[a, b])
        .unwrap();
    assert_eq!(
        tickets.iter().map(|t| t.id).collect::<Vec<_>>(),
        vec![a, b],
        "returned in `pending` order"
    );
    assert!(tickets.iter().all(|t| t.number.is_none()));
    assert_eq!(store.pending_numbers().unwrap(), {
        let mut ids = vec![a, b];
        ids.sort();
        ids
    });
    // No `field.set number` was logged: only the two creates are outbox.
    assert_eq!(store.sync_status().unwrap().outbox, before + 2);

    // A batch whose second op fails (a relation to a ticket that does not
    // exist) commits nothing and flags nothing.
    let c = Ulid::new();
    let dangling = op(
        c,
        4,
        "matt",
        Payload::RelationAdd(RelationAdd {
            relation: Relation {
                kind: RelationKind::Blocks,
                from: Ulid::new(),
                to: c,
            },
        }),
    );
    let err = store
        .commit_batch_pending(&[create(c, 3), dangling], &[c])
        .unwrap_err();
    assert!(
        matches!(err, StoreError::UnknownRelationTarget { .. }),
        "{err}"
    );
    assert!(store.ticket(c).unwrap().is_none());
    assert_eq!(store.pending_numbers().unwrap().len(), 2);
    assert_eq!(store.sync_status().unwrap().outbox, before + 2);

    // Flagging a ticket the batch did not create is refused (and the
    // batch rolled back), so a flag can never point at nothing.
    let d = Ulid::new();
    let err = store
        .commit_batch_pending(&[create(d, 5)], &[Ulid::new()])
        .unwrap_err();
    assert!(matches!(err, StoreError::UnknownTicket { .. }), "{err}");
    assert!(store.ticket(d).unwrap().is_none());

    // The hub's number clears the flag, as for any pending ticket.
    store
        .apply_pulled(&[op(a, 30, "hub", Payload::FieldSet(FieldSet::Number(7)))])
        .unwrap();
    assert_eq!(store.pending_numbers().unwrap(), vec![b]);
    assert_eq!(store.ticket(a).unwrap().unwrap().number, Some(7));
    assert!(store.doctor().unwrap().is_healthy());
}

// ---- config kinds (AGT-1384, folded since AGT-1385) ----

/// A pulled batch may carry config ops in any order: a `project.set`
/// ahead of its `project.create` defers, a `state.upsert` and a
/// `workspace.set` fold into the workspace, and a ticket created in the
/// same batch may name the new project and state. Every applied op counts
/// as pushed, like any other foreign op.
#[test]
fn a_pulled_config_batch_folds_in_any_order_and_is_not_outbox() {
    let (_dir, mut store) = store();
    let ws = store.workspace().unwrap().unwrap();
    // Above the init's own stamps, or LWW keeps the init values.
    let base = store.latest_hlc().unwrap().wall_ms + 1;
    let project = Ulid::new();
    let t = Ulid::new();
    let ops = vec![
        op(
            project,
            base + 3,
            "laptop",
            Payload::ProjectSet(pm_core::op::ProjectSet::RepoAdd(
                "OpenThinkAi/pm-hub".into(),
            )),
        ),
        op(
            t,
            base + 4,
            "laptop",
            Payload::TicketCreate(TicketCreate {
                title: "hub".into(),
                state: "qa".into(),
                priority: Priority::Medium,
                project: Some("pm-hub".into()),
                repo: None,
                source: None,
                ext: Default::default(),
            }),
        ),
        op(
            ws.id,
            base + 1,
            "laptop",
            Payload::StateUpsert(pm_core::op::StateUpsert {
                name: "qa".into(),
                category: StateCategory::Started,
                position: 5,
            }),
        ),
        op(
            ws.id,
            base + 1,
            "laptop",
            Payload::WorkspaceSet(pm_core::op::WorkspaceSet::StaleDays(7)),
        ),
        op(
            project,
            base + 2,
            "laptop",
            Payload::ProjectCreate(pm_core::op::ProjectCreate {
                kind: Default::default(),
                id: "pm-hub".into(),
                title: "pm-hub".into(),
                status: ProjectStatus::InProgress,
                parent: Some("pm".into()),
                doc_id: None,
            }),
        ),
    ];
    let pulled = store.apply_pulled(&ops).unwrap();
    assert_eq!(
        pulled,
        Pulled {
            applied: 5,
            skipped: 0,
            ..Pulled::default()
        }
    );

    let ws = store.workspace().unwrap().unwrap();
    assert_eq!(ws.stale_days, 7);
    assert!(ws.states.iter().any(|s| s.name == "qa" && s.position == 5));
    let p = store.project("pm-hub").unwrap().unwrap();
    assert_eq!(p.parent.as_deref(), Some("pm"));
    assert_eq!(p.repos, ["OpenThinkAi/pm-hub".to_string()].into());
    let ticket = store.ticket(t).unwrap().unwrap();
    assert_eq!(ticket.state, "qa");
    assert_eq!(ticket.project.as_deref(), Some("pm-hub"));

    assert_eq!(
        store.outbox_len().unwrap(),
        0,
        "foreign ops are never outbox"
    );
    assert_eq!(store.ops_since(0).unwrap().len(), 5 + config_ops(&store));
    // Re-pulling is a no-op, and the rebuild reproduces it all.
    assert_eq!(store.apply_pulled(&ops).unwrap().skipped, 5);
    assert!(store.rebuild().unwrap().is_empty());
    assert!(store.doctor().unwrap().is_healthy());
}

/// A `project.set` whose `project.create` is in no batch is what the rest
/// waits on forever: the batch rolls back, config and tickets alike.
#[test]
fn a_config_op_with_an_unmet_dependency_rolls_the_batch_back() {
    let (_dir, mut store) = store();
    let (a, _b, mut ops) = foreign_batch();
    let orphan = op(
        Ulid::new(),
        30,
        "laptop",
        Payload::ProjectSet(pm_core::op::ProjectSet::Title("x".into())),
    );
    ops.push(orphan.clone());
    let err = store.apply_pulled(&ops).unwrap_err();
    assert!(
        matches!(&err, StoreError::Pull { op_id, source, .. }
            if *op_id == orphan.op_id
                && matches!(**source, StoreError::UnknownProjectEntity { .. })),
        "{err:?}"
    );
    assert_eq!(store.ops_since(0).unwrap().len(), config_ops(&store));
    assert!(store.ticket(a).unwrap().is_none());
}

/// How many ops `store()`'s own configuration took.
fn config_ops(store: &Store) -> usize {
    store
        .ops_since(0)
        .unwrap()
        .iter()
        .filter(|(_, o)| o.actor == ActorId::new("matt") && o.payload.is_config())
        .count()
}

// ------------------------------------------------------ seeding (AGT-1396)

/// The seed's first pass: `outbox_config` is the outbox's config ops
/// alone, in `seq` order, however late in the log they sit — a ticket
/// committed before a later `put_project` does not precede its ops.
#[test]
fn outbox_config_is_the_outbox_s_config_ops_in_seq_order() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = Store::open(dir.path().join("pm.sqlite")).unwrap();
    store
        .init_workspace(&workspace(), &ActorId::new("matt"))
        .unwrap();
    let ticket = Ulid::new();
    store
        .commit(&op(
            ticket,
            1,
            "matt",
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
    store
        .put_project(
            &Project {
                kind: Default::default(),
                id: "late".into(),
                title: "late".into(),
                status: ProjectStatus::InProgress,
                parent: None,
                repos: Default::default(),
                doc: "doc".into(),
                documents: Default::default(),
            },
            &ActorId::new("matt"),
        )
        .unwrap();
    let all = store.outbox(100).unwrap();
    let config = store.outbox_config(100).unwrap();
    let config_kinds = [
        "workspace.set",
        "state.upsert",
        "actor.upsert",
        "project.create",
        "project.set",
        "project.delete",
        "project.doc_add",
    ];
    let expected: Vec<Ulid> = all
        .iter()
        .filter(|(_, o)| config_kinds.contains(&o.kind()))
        .map(|(_, o)| o.op_id)
        .collect();
    assert_eq!(ids(&config), expected);
    assert!(
        config.len() < all.len(),
        "the ticket and the doc edit are not config"
    );
    assert!(config.windows(2).all(|w| w[0].0 < w[1].0));
    assert!(
        config.iter().any(|(_, o)| o.kind() == "project.create")
            && all.iter().any(|(_, o)| o.kind() == "body.edit"),
        "the late project is in the config pass"
    );
    assert_eq!(ids(&store.outbox_config(2).unwrap()), expected[..2]);
    // Marking them pushed empties the config pass and leaves the rest.
    store.mark_pushed(&expected).unwrap();
    assert!(store.outbox_config(100).unwrap().is_empty());
    assert_eq!(
        store.outbox_len().unwrap() as usize,
        all.len() - expected.len()
    );
    assert_eq!(store.op_count().unwrap() as usize, all.len());
}

/// The seeded flag, `ever_pushed`, and which op ids the log lacks.
#[test]
fn seeded_flag_ever_pushed_and_unknown_ops() {
    let (_dir, mut store) = store();
    assert!(!store.seeded().unwrap());
    assert!(!store.sync_status().unwrap().seeded);
    // `store()` marked the config ops pushed.
    assert!(store.ever_pushed().unwrap());
    let ticket = Ulid::new();
    let c = create(ticket, 1);
    store.commit(&c).unwrap();
    let stranger = Ulid::new();
    assert_eq!(
        store.unknown_ops(&[c.op_id, stranger, c.op_id]).unwrap(),
        [stranger]
    );
    assert!(store.unknown_ops(&[]).unwrap().is_empty());
    store.mark_seeded().unwrap();
    store.mark_seeded().unwrap();
    assert!(store.seeded().unwrap());
    assert!(store.sync_status().unwrap().seeded);

    let fresh_dir = tempfile::tempdir().unwrap();
    let fresh = Store::open(fresh_dir.path().join("pm.sqlite")).unwrap();
    assert!(!fresh.ever_pushed().unwrap());
    assert_eq!(fresh.op_count().unwrap(), 0);
}

/// `join_workspace`: the row with the given id and nothing else; refused
/// on a database that is already a workspace or already holds ops.
#[test]
fn join_workspace_makes_an_empty_replica_and_refuses_a_used_database() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = Store::open(dir.path().join("pm.sqlite")).unwrap();
    let id = Ulid::new();
    store.join_workspace(id, "AGT").unwrap();
    let ws = store.workspace().unwrap().unwrap();
    assert_eq!((ws.id, ws.prefix.as_str()), (id, "AGT"));
    assert!(ws.states.is_empty() && ws.gate_labels.is_empty());
    assert_eq!(store.op_count().unwrap(), 0);
    assert!(store.workspace_view().unwrap().is_none());
    assert!(matches!(
        store.join_workspace(id, "AGT").unwrap_err(),
        StoreError::AlreadyJoined { workspace } if workspace == id
    ));
    assert!(matches!(
        store.join_workspace(Ulid::new(), "AGT").unwrap_err(),
        StoreError::ForeignWorkspace { workspace, .. } if workspace == id
    ));
    // The joined workspace's config arrives as foreign ops and overwrites
    // the placeholder — the same id, so the row is rewritten in place.
    let mut wanted = workspace();
    wanted.id = id;
    let other_dir = tempfile::tempdir().unwrap();
    let mut other = Store::open(other_dir.path().join("pm.sqlite")).unwrap();
    other
        .init_workspace(&wanted, &ActorId::new("matt"))
        .unwrap();
    let ops: Vec<Op> = other
        .ops_since(0)
        .unwrap()
        .into_iter()
        .map(|(_, o)| o)
        .collect();
    let pulled = store.apply_pulled(&ops).unwrap();
    assert_eq!(pulled.applied, ops.len());
    let ws = store.workspace().unwrap().unwrap();
    assert_eq!(ws, wanted);
    assert!(
        store.outbox(10).unwrap().is_empty(),
        "pulled ops are not outbox"
    );
    assert!(store.ever_pushed().unwrap());
    assert!(store.doctor().unwrap().is_healthy());

    // A database with ops cannot become a joined replica: a `state.upsert`
    // alone lands (the workspace row waits for a prefix), so the log is
    // non-empty while no workspace exists yet.
    let used_dir = tempfile::tempdir().unwrap();
    let mut used = Store::open(used_dir.path().join("pm.sqlite")).unwrap();
    let state_op = ops
        .iter()
        .find(|o| o.kind() == "state.upsert")
        .expect("init_workspace emits a state.upsert")
        .clone();
    assert_eq!(used.apply_pulled(&[state_op]).unwrap().applied, 1);
    assert!(used.workspace().unwrap().is_none());
    assert!(matches!(
        used.join_workspace(id, "AGT").unwrap_err(),
        StoreError::NotEmpty { ops: 1 }
    ));
}

// ------------------------------------------- workflow states (AGT-1518)

/// `upsert_states` is ordinary config: its `state.upsert` ops sit in the
/// outbox's config pass, and a replica that pulls them gets the state —
/// category and position included — and can move a ticket into it.
#[test]
fn an_upserted_state_is_config_that_reaches_another_replica() {
    let dir = tempfile::tempdir().unwrap();
    let mut a = Store::open(dir.path().join("a.sqlite")).unwrap();
    let ws = workspace();
    a.init_workspace(&ws, &ActorId::new("matt")).unwrap();
    let qa = State {
        name: "qa".into(),
        category: StateCategory::Started,
        position: 2,
    };
    assert_eq!(
        a.upsert_states(std::slice::from_ref(&qa), &ActorId::new("matt"))
            .unwrap(),
        1
    );
    // The same record again commits nothing.
    assert_eq!(
        a.upsert_states(std::slice::from_ref(&qa), &ActorId::new("matt"))
            .unwrap(),
        0
    );
    assert!(
        a.outbox_config(100)
            .unwrap()
            .iter()
            .any(|(_, o)| o.kind() == "state.upsert"
                && matches!(&o.payload, Payload::StateUpsert(s) if s.name == "qa"))
    );

    let mut b = Store::open(dir.path().join("b.sqlite")).unwrap();
    b.join_workspace(ws.id, "AGT").unwrap();
    let ops: Vec<Op> = a
        .ops_since(0)
        .unwrap()
        .into_iter()
        .map(|(_, o)| o)
        .collect();
    b.apply_pulled(&ops).unwrap();
    let got = b.workspace().unwrap().unwrap();
    assert_eq!(got.state("qa"), Some(&qa));

    // Re-categorised on b; a pulls it back.
    let review = State {
        category: StateCategory::Unstarted,
        ..qa.clone()
    };
    b.upsert_states(std::slice::from_ref(&review), &ActorId::new("matt"))
        .unwrap();
    let back: Vec<Op> = b
        .ops_since(0)
        .unwrap()
        .into_iter()
        .map(|(_, o)| o)
        .collect();
    a.apply_pulled(&back).unwrap();
    assert_eq!(a.workspace().unwrap().unwrap().state("qa"), Some(&review));
}
