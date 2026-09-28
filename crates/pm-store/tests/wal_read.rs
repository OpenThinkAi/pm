//! AGT-1339 AC5 (projects/pm/README.md §Constraints: "Reads never block on
//! the network"): no read may block on a lock a writer holds. `Store::open`
//! puts the database in WAL mode, where a reader runs against a snapshot
//! from before an in-flight writer's uncommitted transaction rather than
//! waiting for it — this proves that directly against a second, competing
//! connection that holds the write lock open.

use std::time::{Duration, Instant};

use pm_core::op::TicketCreate;
use pm_core::{ActorId, Hlc, Op, Payload, Priority, State, StateCategory, Workspace};
use pm_store::{Store, TicketFilter};
use rusqlite::Connection;
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

#[test]
fn tickets_read_does_not_block_on_a_writer_holding_the_lock() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("pm.sqlite");
    let mut store = Store::open(&path).unwrap();
    store.init_workspace(&workspace()).unwrap();

    let id = Ulid::new();
    store
        .commit(&Op::new(
            Ulid::new(),
            Hlc::new(1, 0),
            ActorId::new("matt"),
            id,
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

    // A second connection to the same database opens a write transaction
    // and never commits it. `journal_mode` is a database-level setting
    // (already WAL, from `Store::open`), so this connection inherits it
    // without setting the pragma itself.
    let writer = Connection::open(&path).unwrap();
    writer
        .execute_batch("BEGIN IMMEDIATE; UPDATE state SET position = position;")
        .unwrap();

    let start = Instant::now();
    let tickets = store.tickets(&TicketFilter::default()).unwrap();
    let single = store.ticket(id).unwrap();
    let elapsed = start.elapsed();

    // Clean up regardless of what the assertions below find.
    writer.execute_batch("ROLLBACK;").unwrap();

    assert_eq!(tickets.len(), 1, "the read still sees the committed ticket");
    assert!(single.is_some());
    assert!(
        elapsed < Duration::from_secs(2),
        "a read waited on the writer's open, uncommitted transaction: {elapsed:?} \
         (busy_timeout is 10s, so a WAL reader/writer conflict would show up here)"
    );
}
