//! AGT-1415: a pulled ticket-description edit ahead of the edits it builds
//! on is deferred, not queued in a view that is persisted right after (and
//! so lost). Pulling a ticket's description edits in any order — one
//! batch or split across two — yields the same description (AC2), and an
//! edit whose predecessor never arrives rolls the batch back.

use pm_core::op::{BodyEdit, TicketCreate};
use pm_core::{
    ActorId, ApplyError, Body, Hlc, Op, Payload, Priority, State, StateCategory, Workspace,
};
use pm_store::{Store, StoreError};
use tempfile::TempDir;
use ulid::Ulid;

fn store() -> (TempDir, Store) {
    let dir = tempfile::tempdir().unwrap();
    let mut store = Store::open(dir.path().join("pm.sqlite")).unwrap();
    store
        .init_workspace(
            &Workspace {
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
            },
            &ActorId::new("matt"),
        )
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

fn body_edit(ticket: Ulid, wall_ms: u64, actor: &str, update: pm_core::BodyUpdate) -> Op {
    op(
        ticket,
        wall_ms,
        actor,
        Payload::BodyEdit(BodyEdit {
            update: update.into_bytes(),
        }),
    )
}

/// A ticket's creation plus a chain of description edits: the laptop
/// writes three in a row, the desktop picks up the first two and edits
/// concurrently with the third, and the laptop then merges the desktop's
/// edit and writes once more — so most edits build on earlier ones.
fn history() -> (Ulid, Op, Vec<Op>) {
    let t = Ulid::new();
    let create = op(
        t,
        10,
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
    );
    let mut laptop = Body::with_peer(3).unwrap();
    let mut desktop = Body::with_peer(5).unwrap();
    let v1 = laptop.diff_from_text("# Problem\n\nv1\n").unwrap();
    let v2 = laptop.diff_from_text("# Problem\n\nv2\n").unwrap();
    desktop.apply(&v1).unwrap();
    desktop.apply(&v2).unwrap();
    let v3 = laptop.diff_from_text("# Problem\n\nv3\n\n## AC\n").unwrap();
    let d1 = desktop
        .diff_from_text("# Problem\n\nv2 from the desktop\n")
        .unwrap();
    laptop.apply(&d1).unwrap();
    let merged = laptop.text();
    let v4 = laptop.diff_from_text(&format!("{merged}\nv4\n")).unwrap();
    let edits = vec![
        body_edit(t, 11, "laptop", v1),
        body_edit(t, 12, "laptop", v2),
        body_edit(t, 13, "laptop", v3),
        body_edit(t, 14, "desktop", d1),
        body_edit(t, 15, "laptop", v4),
    ];
    (t, create, edits)
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

fn description(store: &Store, t: Ulid) -> String {
    store.ticket(t).unwrap().unwrap().description
}

#[test]
fn pulling_description_edits_in_any_order_yields_the_same_description() {
    let (t, create, edits) = history();
    let mut all = vec![create.clone()];
    all.extend(edits.iter().cloned());

    let (_d, mut forward) = store();
    forward.apply_pulled(&all).unwrap();
    let expected = description(&forward, t);
    assert!(expected.contains("v4"), "{expected:?}");
    assert!(expected.contains("desktop"), "{expected:?}");

    let reversed: Vec<Op> = all.iter().rev().cloned().collect();
    // A hub serves ops in its own (causal) order, so a pull split across
    // two batches gets a prefix first — each batch itself arbitrarily
    // reordered.
    let mut orders: Vec<(String, Vec<Vec<Op>>)> = vec![("reversed".into(), vec![reversed])];
    for split in 1..all.len() {
        let (first, second) = all.split_at(split);
        orders.push((
            format!("two batches at {split}, each reversed"),
            vec![
                first.iter().rev().cloned().collect(),
                second.iter().rev().cloned().collect(),
            ],
        ));
        orders.push((
            format!("two batches at {split}, each shuffled"),
            vec![
                shuffled(first, split as u64 * 31),
                shuffled(second, split as u64 * 37),
            ],
        ));
    }
    for seed in 1..=8u64 {
        orders.push((
            format!("shuffled {seed}"),
            vec![shuffled(&all, seed * 7919)],
        ));
    }

    for (name, batches) in orders {
        let (_dir, mut store) = store();
        for batch in &batches {
            store.apply_pulled(batch).unwrap_or_else(|e| {
                panic!("{name}: {e:?}");
            });
        }
        assert_eq!(description(&store, t), expected, "{name}");
        // Landing order is the log order, so a replay reproduces the row.
        let report = store.doctor().unwrap();
        assert!(report.is_healthy(), "{name}: {report:#?}");
        let diff = store.rebuild().unwrap();
        assert!(diff.is_empty(), "{name}: {diff:#?}");
    }
}

#[test]
fn an_edit_whose_predecessor_never_arrives_rolls_the_batch_back() {
    let (t, create, edits) = history();
    let (_dir, mut store) = store();
    let before = store.ops_since(0).unwrap().len();
    // v1 never arrives: v2 and everything after it wait forever.
    let batch: Vec<Op> = std::iter::once(create)
        .chain(edits.into_iter().skip(1))
        .collect();
    let err = store.apply_pulled(&batch).unwrap_err();
    let StoreError::Pull { source, .. } = &err else {
        panic!("{err:?}");
    };
    assert!(
        matches!(
            **source,
            StoreError::Apply(ApplyError::MissingDependency { .. })
        ),
        "{err:?}"
    );
    // The create had nothing to wait on, yet did not land either.
    assert_eq!(store.ops_since(0).unwrap().len(), before);
    assert!(store.ticket(t).unwrap().is_none());
}

#[test]
fn a_local_description_edit_commits_as_before() {
    let (t, create, edits) = history();
    let (_dir, mut store) = store();
    store.commit(&create).unwrap();
    for edit in &edits {
        store.commit(edit).unwrap();
    }
    let (_d, mut pulled) = store_with(&create, &edits);
    assert_eq!(description(&store, t), description(&pulled, t));
    let diff = pulled.rebuild().unwrap();
    assert!(diff.is_empty(), "{diff:#?}");
}

fn store_with(create: &Op, edits: &[Op]) -> (TempDir, Store) {
    let (dir, mut store) = store();
    let mut all = vec![create.clone()];
    all.extend(edits.iter().cloned());
    store.apply_pulled(&all).unwrap();
    (dir, store)
}
