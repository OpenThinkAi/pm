//! `Store::check` (AGT-1342): the snapshot it reads includes tombstoned
//! tickets and every relation, so the pure checker in `pm_core::check`
//! sees dangling relations and blocker cycles end to end.

use pm_core::op::{RelationAdd, TicketCreate};
use pm_core::{
    ActorId, Finding, Hlc, Op, Payload, Priority, Project, ProjectStatus, Relation, RelationKind,
    State, StateCategory, Workspace,
};
use pm_store::Store;
use ulid::Ulid;

const NOW: u64 = 1_790_000_000_000;

fn workspace() -> Workspace {
    let state = |name: &str, category, position| State {
        name: name.into(),
        category,
        position,
    };
    Workspace {
        id: Ulid::new(),
        prefix: "AGT".into(),
        states: vec![
            state("triage", StateCategory::Unstarted, 0),
            state("in-progress", StateCategory::Started, 1),
            state("done", StateCategory::Completed, 2),
        ],
        gate_labels: Default::default(),
        model_labels: Default::default(),
        template_sections: vec![],
        stale_days: 30,
    }
}

fn op(ticket: Ulid, n: u64, payload: Payload) -> Op {
    Op::new(
        Ulid::new(),
        Hlc::new(NOW + n, 0),
        ActorId::new("matt"),
        ticket,
        payload,
    )
}

fn create(ticket: Ulid, n: u64) -> Op {
    op(
        ticket,
        n,
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

fn blocks(owner: Ulid, from: Ulid, to: Ulid, n: u64) -> Op {
    op(
        owner,
        n,
        Payload::RelationAdd(RelationAdd {
            relation: Relation {
                kind: RelationKind::Blocks,
                from,
                to,
            },
        }),
    )
}

#[test]
fn check_sees_cycles_across_owners_and_relations_to_tombstones() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = Store::open(dir.path().join("pm.sqlite")).unwrap();
    let ws = workspace();
    store
        .init_workspace(&ws, &pm_core::ActorId::new("matt"))
        .unwrap();
    store
        .put_project(
            &Project {
                id: "pm".into(),
                title: "pm".into(),
                status: ProjectStatus::InProgress,
                parent: None,
                repos: Default::default(),
                doc: String::new(),
                documents: Default::default(),
            },
            &pm_core::ActorId::new("matt"),
        )
        .unwrap();
    let (a, b, c) = (Ulid::new(), Ulid::new(), Ulid::new());
    for (i, t) in [a, b, c].into_iter().enumerate() {
        store.commit(&create(t, i as u64)).unwrap();
    }
    assert_eq!(store.check(&ws, NOW + 10, None).unwrap(), []);

    // a blocks b (owned by b), b blocks a (owned by a): a cycle whose two
    // edges live in different tickets' OR-sets.
    store.commit(&blocks(b, a, b, 10)).unwrap();
    store.commit(&blocks(a, b, a, 11)).unwrap();
    // c blocks a, then c is tombstoned: a now has a dangling blocker.
    store.commit(&blocks(a, c, a, 12)).unwrap();
    store.commit(&op(c, 13, Payload::Tombstone)).unwrap();

    assert_eq!(store.all_tickets().unwrap().len(), 3, "tombstones included");
    let mut pair = vec![a, b];
    pair.sort();
    assert_eq!(
        store.check(&ws, NOW + 20, None).unwrap(),
        [
            Finding::BlockerCycle { tickets: pair },
            Finding::DanglingRelation {
                relation: Relation {
                    kind: RelationKind::Blocks,
                    from: c,
                    to: a
                },
                missing: c
            },
        ]
    );
    assert_eq!(store.check(&ws, NOW + 20, Some("other")).unwrap(), []);
}
