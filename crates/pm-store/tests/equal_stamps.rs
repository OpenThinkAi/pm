//! AGT-2456: two ops sharing one `(hlc, actor)` stamp — two machines
//! running as the same actor — must not leave replicas disagreeing. A log
//! refuses a second op with a stamp it already holds; the hub keeps the
//! op that reached it first; and the replica that authored the other one
//! gives it a fresh stamp (`Store::restamp_outbox`) before anyone else
//! sees it. These tests drive real stores against a model of the hub's
//! admission (`pm-hub`'s `ops.rs`: idempotent on `op_id`, `duplicate_stamp`
//! for a fresh op whose stamp the log or the batch already has).

use std::collections::BTreeMap;

use pm_core::op::{CommentAdd, FieldSet, LabelAdd, StateTransition, TicketCreate};
use pm_core::{ActorId, Hlc, Op, Payload, Priority, State, StateCategory, Workspace};
use pm_store::{QuarantineStatus, Store, StoreError};
use proptest::prelude::*;
use tempfile::TempDir;
use ulid::Ulid;

/// The hub's admission rules, in memory.
#[derive(Default)]
struct Hub {
    log: Vec<Op>,
}

impl Hub {
    /// A push: the batch's op ids, or the first fresh op whose stamp
    /// another op already has (nothing stored, as the hub's 400).
    fn push(&mut self, batch: &[Op]) -> Result<Vec<Ulid>, Ulid> {
        let mut fresh: Vec<&Op> = Vec::new();
        for op in batch {
            if self.log.iter().any(|o| o.op_id == op.op_id)
                || fresh.iter().any(|o| o.op_id == op.op_id)
            {
                continue;
            }
            let taken = |o: &&Op| o.hlc == op.hlc && o.actor == op.actor;
            if self.log.iter().any(|o| taken(&o)) || fresh.iter().any(taken) {
                return Err(op.op_id);
            }
            fresh.push(op);
        }
        self.log.extend(fresh.into_iter().cloned());
        Ok(batch.iter().map(|o| o.op_id).collect())
    }

    /// `pull since <cursor>`: `(seq, op)` with 1-based seqs.
    fn since(&self, cursor: i64) -> Vec<(i64, Op)> {
        self.log
            .iter()
            .enumerate()
            .map(|(i, op)| (i as i64 + 1, op.clone()))
            .filter(|(seq, _)| *seq > cursor)
            .collect()
    }
}

/// `pm sync` against the model hub: push the outbox, re-stamping on a
/// `duplicate_stamp`, then pull everything new. Returns how many ops
/// were re-stamped.
fn sync(store: &mut Store, hub: &mut Hub) -> usize {
    let mut restamped = 0;
    loop {
        let outbox: Vec<Op> = store
            .outbox(1000)
            .unwrap()
            .into_iter()
            .map(|(_, op)| op)
            .collect();
        if outbox.is_empty() {
            break;
        }
        match hub.push(&outbox) {
            Ok(acked) => {
                store.mark_pushed(&acked).unwrap();
            }
            Err(op_id) => {
                restamped += store.restamp_outbox(op_id).unwrap();
            }
        }
    }
    let page = hub.since(store.cursor().unwrap());
    if let Some(&(next, _)) = page.last() {
        store.apply_pulled_page(&page, next).unwrap();
    }
    restamped
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

/// Two replicas of one workspace, both synced with `hub`, and one ticket
/// they both hold.
fn two_replicas(hub: &mut Hub) -> ((TempDir, Store), (TempDir, Store), Ulid) {
    let ws = workspace();
    let a_dir = tempfile::tempdir().unwrap();
    let mut a = Store::open(a_dir.path().join("pm.sqlite")).unwrap();
    a.init_workspace(&ws, &ActorId::new("matt")).unwrap();
    let ticket = Ulid::new();
    let create = Op::new(
        Ulid::new(),
        a.latest_hlc().unwrap().successor(),
        ActorId::new("matt"),
        ticket,
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
    a.commit(&create).unwrap();
    sync(&mut a, hub);

    let b_dir = tempfile::tempdir().unwrap();
    let mut b = Store::open(b_dir.path().join("pm.sqlite")).unwrap();
    b.join_workspace(ws.id, &ws.prefix).unwrap();
    sync(&mut b, hub);
    assert!(b.ticket(ticket).unwrap().is_some());
    ((a_dir, a), (b_dir, b), ticket)
}

/// What a replica's state is, for comparing two: the ticket as it reads,
/// its comments, and every op's stamp by id.
fn state(store: &Store, ticket: Ulid) -> (String, String, BTreeMap<Ulid, (Hlc, ActorId)>) {
    let read = serde_json::to_string(&store.ticket(ticket).unwrap()).unwrap();
    let comments = serde_json::to_string(&store.comments(ticket).unwrap()).unwrap();
    let stamps = store
        .ops_since(0)
        .unwrap()
        .into_iter()
        .map(|(_, op)| (op.op_id, (op.hlc, op.actor)))
        .collect();
    (read, comments, stamps)
}

fn payload() -> impl Strategy<Value = Payload> {
    prop_oneof![
        "[a-z]{1,8}".prop_map(|t| Payload::FieldSet(FieldSet::Title(t))),
        prop_oneof![
            Just(Priority::Low),
            Just(Priority::High),
            Just(Priority::Critical)
        ]
        .prop_map(|p| Payload::FieldSet(FieldSet::Priority(p))),
        "[a-z]{1,6}".prop_map(|label| Payload::LabelAdd(LabelAdd { label })),
        "[a-z ]{1,12}".prop_map(|body| Payload::CommentAdd(CommentAdd { body })),
        Just(Payload::StateTransition(StateTransition {
            state: "done".into()
        })),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(48))]

    /// The acceptance property: replica A authors `x`, replica B authors
    /// `y`, both stamped `(h, matt)`. Each receives the pair in the
    /// opposite order — its own op first, the other's from the hub — and
    /// whichever reaches the hub first, both end in the same state: the
    /// first keeps the stamp, the second is re-stamped by its author.
    /// Each may also write more after its colliding op (`a_more`,
    /// `b_more`), which re-stamping must keep in order.
    #[test]
    fn equal_stamp_writes_converge_whichever_reaches_the_hub_first(
        px in payload(),
        py in payload(),
        a_more in proptest::option::of(payload()),
        b_more in proptest::option::of(payload()),
        a_first in any::<bool>(),
    ) {
        let mut hub = Hub::default();
        let ((_ad, mut a), (_bd, mut b), ticket) = two_replicas(&mut hub);
        let matt = ActorId::new("matt");
        let h = a.latest_hlc().unwrap().max(b.latest_hlc().unwrap()).successor();
        let x = Op::new(Ulid::new(), h, matt.clone(), ticket, px);
        let y = Op::new(Ulid::new(), h, matt.clone(), ticket, py);
        a.commit(&x).unwrap();
        b.commit(&y).unwrap();
        for (store, more) in [(&mut a, a_more), (&mut b, b_more)] {
            if let Some(p) = more {
                let next = store.latest_hlc().unwrap().successor();
                store.commit(&Op::new(Ulid::new(), next, matt.clone(), ticket, p)).unwrap();
            }
        }

        let (first, second) = if a_first { (&mut a, &mut b) } else { (&mut b, &mut a) };
        prop_assert_eq!(sync(first, &mut hub), 0);
        prop_assert!(sync(second, &mut hub) >= 1, "the later author re-stamps");
        sync(first, &mut hub);
        sync(second, &mut hub);

        let (sa, sb) = (state(&a, ticket), state(&b, ticket));
        prop_assert_eq!(&sa, &sb);
        // The hub's op kept its stamp; the other moved past it.
        let (kept, moved) = if a_first { (&x, &y) } else { (&y, &x) };
        prop_assert_eq!(&sa.2[&kept.op_id].0, &h);
        prop_assert!(sa.2[&moved.op_id].0 > h);
        // Both replicas' logs and the hub's hold the same ops.
        prop_assert_eq!(sa.2.len(), hub.log.len());
        prop_assert!(a.quarantine().unwrap().is_empty());
        prop_assert!(b.quarantine().unwrap().is_empty());
        // The rebuilt tables are what the log replays to.
        for store in [&mut a, &mut b] {
            prop_assert!(store.doctor().unwrap().is_healthy());
        }
    }
}

/// A log holds one op per stamp, whichever path brings it: a local commit
/// of a second op on a stamp already held is refused and changes nothing.
#[test]
fn a_local_commit_on_a_held_stamp_is_refused() {
    let mut hub = Hub::default();
    let ((_ad, mut a), _b, ticket) = two_replicas(&mut hub);
    let h = a.latest_hlc().unwrap().successor();
    let write = |title: &str| {
        Op::new(
            Ulid::new(),
            h,
            ActorId::new("matt"),
            ticket,
            Payload::FieldSet(FieldSet::Title(title.into())),
        )
    };
    let first = write("first");
    a.commit(&first).unwrap();
    let before = a.op_count().unwrap();
    let err = a.commit(&write("second")).unwrap_err();
    assert!(
        matches!(err, StoreError::DuplicateStamp { existing, .. } if existing == first.op_id),
        "{err:?}"
    );
    assert_eq!(a.op_count().unwrap(), before);
    assert_eq!(a.ticket(ticket).unwrap().unwrap().title, "first");
    // The same stamp by another actor is a different stamp.
    let other = Op::new(
        Ulid::new(),
        h,
        ActorId::new("alice"),
        ticket,
        Payload::FieldSet(FieldSet::Title("alice".into())),
    );
    a.commit(&other).unwrap();
}

/// A hub log written before the hub checked stamps may serve both ops
/// of a pair. Every replica pulling it keeps the first in hub order and
/// refuses the second into quarantine, whatever its page boundaries.
#[test]
fn a_pulled_pair_keeps_the_first_in_hub_order_on_every_replica() {
    let mut hub = Hub::default();
    let ((_ad, mut a), (_bd, mut b), ticket) = two_replicas(&mut hub);
    let h = a
        .latest_hlc()
        .unwrap()
        .max(b.latest_hlc().unwrap())
        .successor();
    let write = |title: &str| {
        Op::new(
            Ulid::new(),
            h,
            ActorId::new("laptop"),
            ticket,
            Payload::FieldSet(FieldSet::Title(title.into())),
        )
    };
    let (x, y) = (write("x"), write("y"));
    // Stored as an old hub would have: straight into the log.
    hub.log.push(x.clone());
    hub.log.push(y.clone());

    // A pulls both in one page; B one page per op.
    let page = hub.since(a.cursor().unwrap());
    let next = page.last().unwrap().0;
    a.apply_pulled_page(&page, next).unwrap();
    for (seq, op) in hub.since(b.cursor().unwrap()) {
        b.apply_pulled_page(&[(seq, op)], seq).unwrap();
    }
    for store in [&a, &b] {
        assert_eq!(store.ticket(ticket).unwrap().unwrap().title, "x");
        let q = store.quarantine().unwrap();
        assert_eq!(q.len(), 1, "{q:?}");
        assert_eq!(q[0].op_id, y.op_id);
        assert_eq!(q[0].status, QuarantineStatus::Refused);
    }
    assert_eq!(state(&a, ticket), state(&b, ticket));
}

/// A pulled op whose stamp a local op still in the outbox holds — the hub
/// admitted the pulled one, so the local one never reached it — lands,
/// and the local op is re-stamped past it instead of refusing it.
#[test]
fn a_pull_re_stamps_an_unpushed_local_op_that_holds_the_stamp() {
    let mut hub = Hub::default();
    let ((_ad, mut a), (_bd, mut b), ticket) = two_replicas(&mut hub);
    let h = a
        .latest_hlc()
        .unwrap()
        .max(b.latest_hlc().unwrap())
        .successor();
    let write = |title: &str| {
        Op::new(
            Ulid::new(),
            h,
            ActorId::new("matt"),
            ticket,
            Payload::FieldSet(FieldSet::Title(title.into())),
        )
    };
    let (x, y) = (write("from a"), write("from b"));
    a.commit(&x).unwrap();
    b.commit(&y).unwrap();
    sync(&mut a, &mut hub);
    // B pulls before it pushes.
    let page = hub.since(b.cursor().unwrap());
    let next = page.last().unwrap().0;
    b.apply_pulled_page(&page, next).unwrap();
    assert!(b.quarantine().unwrap().is_empty());
    let y_now = b
        .ops_since(0)
        .unwrap()
        .into_iter()
        .find(|(_, op)| op.op_id == y.op_id)
        .unwrap()
        .1;
    assert!(y_now.hlc > h, "y moved past x");
    assert_eq!(b.ticket(ticket).unwrap().unwrap().title, "from b");
    assert_eq!(sync(&mut b, &mut hub), 0, "nothing left to re-stamp");
    sync(&mut a, &mut hub);
    assert_eq!(state(&a, ticket), state(&b, ticket));
}

/// Only an op the hub has not acknowledged may change its stamp.
#[test]
fn an_acknowledged_op_is_never_re_stamped() {
    let mut hub = Hub::default();
    let ((_ad, mut a), _b, ticket) = two_replicas(&mut hub);
    let pushed = a.ops(ticket).unwrap()[0].op_id;
    let err = a.restamp_outbox(pushed).unwrap_err();
    assert!(matches!(err, StoreError::NotInOutbox { op_id } if op_id == pushed));
    let err = a.restamp_outbox(Ulid::new()).unwrap_err();
    assert!(matches!(err, StoreError::NotInOutbox { .. }));
}
