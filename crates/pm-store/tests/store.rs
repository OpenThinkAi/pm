//! `pm-store` through its public API: schema (AC1), R2/R4 as constraints
//! (AC3), number allocation under contention (AC4), the query API (AC5)
//! and the actor/HLC guarantees on `ops` (AC6). The failure-injection test
//! for AC2 lives in `src/commit.rs`, where the hook is reachable.

use std::collections::BTreeSet;
use std::thread;

use pm_core::op::{
    Claim, CommentAdd, FieldSet, HoldSet, LabelAdd, LabelRemove, RelationAdd, StateTransition,
    TicketCreate,
};
use pm_core::{
    ActorId, ClaimRejected, Hlc, Hold, Op, Payload, Priority, Project, ProjectStatus, Relation,
    RelationKind, State, StateCategory, Workspace,
};
use pm_store::{SCHEMA_VERSION, Store, StoreError, TicketFilter};
use rusqlite::Connection;
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
            State {
                name: "done".into(),
                category: StateCategory::Completed,
                position: 2,
            },
        ],
        gate_labels: ["manual".to_string()].into(),
        model_labels: [("model:fable-5".to_string(), "fable".to_string())].into(),
        template_sections: vec!["Problem Statement".into(), "Acceptance Criteria".into()],
        stale_days: 30,
        docs_owned_by: Default::default(),
    }
}

fn project(id: &str) -> Project {
    Project {
        kind: Default::default(),
        id: id.into(),
        title: id.into(),
        status: ProjectStatus::InProgress,
        parent: None,
        repos: [format!("OpenThinkAi/{id}")].into(),
        doc: format!("# {id}\n"),
        documents: [("ideation/IDEA-1".to_string(), "idea".to_string())].into(),
    }
}

/// A fresh store with the saltline workspace and a `pm` project.
fn store() -> (TempDir, Store) {
    let dir = tempfile::tempdir().unwrap();
    let mut store = Store::open(dir.path().join("pm.sqlite")).unwrap();
    store
        .init_workspace(&workspace(), &pm_core::ActorId::new("matt"))
        .unwrap();
    store
        .put_project(&project("pm"), &pm_core::ActorId::new("matt"))
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

fn create(ticket: Ulid, wall_ms: u64, project: Option<&str>) -> Op {
    op(
        ticket,
        wall_ms,
        "matt",
        Payload::TicketCreate(TicketCreate {
            title: format!("ticket {ticket}"),
            state: "triage".into(),
            priority: Priority::Medium,
            project: project.map(String::from),
            repo: Some("OpenThinkAi/pm".into()),
            source: None,
            ext: Default::default(),
        }),
    )
}

fn ids(store: &Store, filter: TicketFilter) -> Vec<Ulid> {
    store
        .tickets(&filter)
        .unwrap()
        .into_iter()
        .map(|t| t.id)
        .collect()
}

fn table_names(conn: &Connection) -> BTreeSet<String> {
    conn.prepare("SELECT name FROM sqlite_master WHERE type = 'table'")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

// ---- AC1: schema ----

#[test]
fn open_creates_a_wal_database_with_every_table_and_records_the_migration() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("pm.sqlite");
    let store = Store::open(&path).unwrap();
    assert_eq!(store.schema_version().unwrap(), SCHEMA_VERSION);

    let conn = Connection::open(&path).unwrap();
    let journal: String = conn
        .query_row("PRAGMA journal_mode", [], |r| r.get(0))
        .unwrap();
    assert_eq!(journal.to_lowercase(), "wal");
    let tables = table_names(&conn);
    for expected in [
        "workspace",
        "state",
        "ticket",
        "ticket_label",
        "relation",
        "comment",
        "marker",
        "project",
        "project_doc",
        "project_doc_view",
        "actor",
        "ops",
        "ticket_view",
        "schema_version",
        "backup_target",
        "sync_state",
        "sync_pushed",
        "pending_number",
    ] {
        assert!(
            tables.contains(expected),
            "missing table {expected}: {tables:?}"
        );
    }
    let applied: Vec<u32> = conn
        .prepare("SELECT version FROM schema_version ORDER BY version")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(applied, (1..=SCHEMA_VERSION).collect::<Vec<_>>());

    // Reopening is a no-op migration, not a re-run.
    drop(store);
    let again = Store::open(&path).unwrap();
    assert_eq!(again.schema_version().unwrap(), SCHEMA_VERSION);
}

#[test]
fn open_refuses_a_database_from_the_future() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("pm.sqlite");
    drop(Store::open(&path).unwrap());
    Connection::open(&path)
        .unwrap()
        .execute("INSERT INTO schema_version (version) VALUES (99)", [])
        .unwrap();
    assert!(matches!(
        Store::open(&path),
        Err(StoreError::SchemaTooNew {
            found: 99,
            supported: SCHEMA_VERSION
        })
    ));
}

#[test]
fn workspace_round_trips() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = Store::open(dir.path().join("pm.sqlite")).unwrap();
    let ws = workspace();
    store
        .init_workspace(&ws, &pm_core::ActorId::new("matt"))
        .unwrap();
    assert_eq!(store.workspace().unwrap().unwrap(), ws);

    let mut renamed = ws.clone();
    renamed.prefix = "SL".into();
    renamed.states.push(State {
        name: "qa".into(),
        category: StateCategory::Started,
        position: 3,
    });
    store
        .init_workspace(&renamed, &pm_core::ActorId::new("matt"))
        .unwrap();
    assert_eq!(store.workspace().unwrap().unwrap(), renamed);
}

// ---- AC2: commit appends and materializes together ----

#[test]
fn commit_appends_the_op_and_materializes_the_ticket() {
    let (_dir, mut store) = store();
    // The workspace's own config ops (AGT-1385) are stamped at the wall
    // clock, above this test's small stamps.
    let config_latest = store.latest_hlc().unwrap();
    let id = Ulid::new();
    let created = store.commit(&create(id, 1, Some("pm"))).unwrap().unwrap();
    assert_eq!(created.state, "triage");
    assert_eq!(created.project.as_deref(), Some("pm"));
    assert_eq!(created.number, None);

    store
        .commit(&op(
            id,
            2,
            "matt",
            Payload::LabelAdd(LabelAdd {
                label: "model:fable-5".into(),
            }),
        ))
        .unwrap();
    store
        .commit(&op(
            id,
            3,
            "claude:pm-build",
            Payload::CommentAdd(CommentAdd {
                body: "on it".into(),
            }),
        ))
        .unwrap();
    let held = store
        .commit(&op(
            id,
            4,
            "matt",
            Payload::HoldSet(HoldSet {
                hold: Hold {
                    reason: "needs Matt".into(),
                    by: ActorId::new("matt"),
                    at: Hlc::new(4, 0),
                },
            }),
        ))
        .unwrap()
        .unwrap();
    assert_eq!(held.hold.as_ref().unwrap().reason, "needs Matt");
    assert_eq!(held.labels.iter().collect::<Vec<_>>(), ["model:fable-5"]);
    assert_eq!(held.updated, Hlc::new(4, 0));

    let read = store.ticket(id).unwrap().unwrap();
    assert_eq!(read, held, "the returned ticket is what the tables say");
    let comments = store.comments(id).unwrap();
    assert_eq!(comments.len(), 1);
    assert_eq!(comments[0].author, ActorId::new("claude:pm-build"));
    assert_eq!(comments[0].body, "on it");

    let ops = store.ops(id).unwrap();
    assert_eq!(ops.len(), 4);
    assert_eq!(
        ops.iter().map(Op::kind).collect::<Vec<_>>(),
        ["ticket.create", "label.add", "comment.add", "hold.set"]
    );
    assert_eq!(ops[0].actor, ActorId::new("matt"));
    assert_eq!(ops[2].actor, ActorId::new("claude:pm-build"));
    assert_eq!(
        store.latest_hlc().unwrap(),
        config_latest.max(Hlc::new(4, 0))
    );
}

#[test]
fn committing_the_same_op_twice_is_refused_and_changes_nothing() {
    let (_dir, mut store) = store();
    let id = Ulid::new();
    let create = create(id, 1, Some("pm"));
    store.commit(&create).unwrap();
    let before = store.ticket(id).unwrap();
    assert!(matches!(
        store.commit(&create),
        Err(StoreError::DuplicateOp { op_id }) if op_id == create.op_id
    ));
    assert_eq!(store.ticket(id).unwrap(), before);
    assert_eq!(store.ops(id).unwrap().len(), 1);
}

#[test]
fn ops_for_a_ticket_that_was_never_created_are_refused() {
    let (_dir, mut store) = store();
    let ghost = Ulid::new();
    let err = store
        .commit(&op(
            ghost,
            1,
            "matt",
            Payload::FieldSet(FieldSet::Title("late".into())),
        ))
        .unwrap_err();
    assert!(matches!(err, StoreError::UnknownTicket { ticket } if ticket == ghost));
    assert!(store.ops(ghost).unwrap().is_empty(), "nothing was logged");
}

#[test]
fn body_edits_materialize_the_description_and_keep_merging_across_reopens() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("pm.sqlite");
    let id = Ulid::new();
    let mut author = pm_core::Body::with_peer(7).unwrap();
    let first = author.diff_from_text("# Problem\n\nv1\n").unwrap();
    let second = author.diff_from_text("# Problem\n\nv1 v2\n").unwrap();

    {
        let mut store = Store::open(&path).unwrap();
        store
            .init_workspace(&workspace(), &pm_core::ActorId::new("matt"))
            .unwrap();
        store.commit(&create(id, 1, None)).unwrap();
        let t = store
            .commit(&op(
                id,
                2,
                "matt",
                Payload::BodyEdit(pm_core::op::BodyEdit {
                    update: first.into_bytes(),
                }),
            ))
            .unwrap()
            .unwrap();
        assert_eq!(t.description, "# Problem\n\nv1\n");
    }
    // A second process picks up the persisted view and folds the next
    // update in: the CRDT history survived the round trip.
    let mut store = Store::open(&path).unwrap();
    let t = store
        .commit(&op(
            id,
            3,
            "matt",
            Payload::BodyEdit(pm_core::op::BodyEdit {
                update: second.into_bytes(),
            }),
        ))
        .unwrap()
        .unwrap();
    assert_eq!(t.description, "# Problem\n\nv1 v2\n");
    assert_eq!(
        store.ticket(id).unwrap().unwrap().description,
        t.description
    );
}

#[test]
fn merge_rules_come_from_pm_core() {
    // Two concurrent title writes with the same HLC: the greater actor id
    // wins whichever lands first (README §Conflict semantics, via
    // pm_core::merge::Lww) — the store did not re-implement the rule.
    let (_dir, mut store) = store();
    let id = Ulid::new();
    store.commit(&create(id, 1, None)).unwrap();
    store
        .commit(&op(
            id,
            5,
            "bob",
            Payload::FieldSet(FieldSet::Title("bob".into())),
        ))
        .unwrap();
    let t = store
        .commit(&op(
            id,
            5,
            "alice",
            Payload::FieldSet(FieldSet::Title("alice".into())),
        ))
        .unwrap()
        .unwrap();
    assert_eq!(t.title, "bob");

    // Add-wins label removal, needing the observed tags from the view.
    let add = op(
        id,
        6,
        "matt",
        Payload::LabelAdd(LabelAdd {
            label: "manual".into(),
        }),
    );
    store.commit(&add).unwrap();
    let observed = store
        .ticket_view(id)
        .unwrap()
        .unwrap()
        .labels
        .observed(&"manual".to_string());
    assert_eq!(observed, vec![add.op_id]);
    let t = store
        .commit(&op(
            id,
            7,
            "matt",
            Payload::LabelRemove(LabelRemove {
                label: "manual".into(),
                observed,
            }),
        ))
        .unwrap()
        .unwrap();
    assert!(t.labels.is_empty());
}

// ---- AC3: R2 / R4 as constraints with typed errors ----

#[test]
fn r2_a_ticket_cannot_name_a_project_that_does_not_exist() {
    let (_dir, mut store) = store();
    let id = Ulid::new();
    let err = store
        .commit(&create(id, 1, Some("equities-lab")))
        .unwrap_err();
    assert!(
        matches!(&err, StoreError::UnknownProject { project } if project == "equities-lab"),
        "{err}"
    );
    assert!(store.ticket(id).unwrap().is_none());
    assert!(
        store.ops(id).unwrap().is_empty(),
        "the op was not logged either"
    );

    store.commit(&create(id, 1, Some("pm"))).unwrap();
    let err = store
        .commit(&op(
            id,
            2,
            "matt",
            Payload::FieldSet(FieldSet::Project(Some("nope".into()))),
        ))
        .unwrap_err();
    assert!(matches!(err, StoreError::UnknownProject { .. }));
    assert_eq!(
        store.ticket(id).unwrap().unwrap().project.as_deref(),
        Some("pm")
    );
}

#[test]
fn r4_a_relation_must_point_at_an_existing_ticket() {
    let (_dir, mut store) = store();
    let (a, b, ghost) = (Ulid::new(), Ulid::new(), Ulid::new());
    store.commit(&create(a, 1, None)).unwrap();
    store.commit(&create(b, 1, None)).unwrap();

    let err = store
        .commit(&op(
            a,
            2,
            "matt",
            Payload::RelationAdd(RelationAdd {
                relation: Relation {
                    kind: RelationKind::Blocks,
                    from: ghost,
                    to: a,
                },
            }),
        ))
        .unwrap_err();
    assert!(matches!(err, StoreError::UnknownRelationTarget { ticket } if ticket == ghost));
    assert!(store.relations(a).unwrap().is_empty());
    assert_eq!(store.ops(a).unwrap().len(), 1);

    let blocks = Relation {
        kind: RelationKind::Blocks,
        from: b,
        to: a,
    };
    store
        .commit(&op(
            a,
            3,
            "matt",
            Payload::RelationAdd(RelationAdd { relation: blocks }),
        ))
        .unwrap();
    assert_eq!(store.relations(a).unwrap(), vec![blocks]);
    assert_eq!(store.relations(b).unwrap(), vec![blocks]);
}

#[test]
fn r2_and_r4_are_real_foreign_keys_not_just_checks() {
    let (dir, _store) = store();
    let conn = Connection::open(dir.path().join("pm.sqlite")).unwrap();
    conn.pragma_update(None, "foreign_keys", "ON").unwrap();
    let insert = "INSERT INTO ticket (id, title, state, priority, project, created_wall_ms, created_counter, updated_wall_ms, updated_counter)
                  VALUES (?1, 't', 'triage', 'low', ?2, 1, 0, 1, 0)";
    let err = conn
        .execute(insert, ["01ARZ3NDEKTSV4RRFFQ69G5FAV", "nope"])
        .unwrap_err();
    assert!(err.to_string().contains("FOREIGN KEY"), "{err}");
    conn.execute(insert, ["01ARZ3NDEKTSV4RRFFQ69G5FAV", "pm"])
        .unwrap();
    let err = conn
        .execute(
            "INSERT INTO relation (owner, kind, from_ticket, to_ticket) VALUES (?1, 'blocks', ?2, ?1)",
            ["01ARZ3NDEKTSV4RRFFQ69G5FAV", "01ARZ3NDEKTSV4RRFFQ69G5FAW"],
        )
        .unwrap_err();
    assert!(err.to_string().contains("FOREIGN KEY"), "{err}");
}

#[test]
fn unknown_workflow_states_are_refused() {
    let (_dir, mut store) = store();
    let id = Ulid::new();
    store.commit(&create(id, 1, None)).unwrap();
    let err = store
        .commit(&op(
            id,
            2,
            "matt",
            Payload::StateTransition(StateTransition {
                state: "shipped".into(),
            }),
        ))
        .unwrap_err();
    assert!(matches!(err, StoreError::UnknownState { state } if state == "shipped"));
    assert_eq!(store.ticket(id).unwrap().unwrap().state, "triage");
}

#[test]
fn claims_are_admitted_once() {
    let (_dir, mut store) = store();
    let id = Ulid::new();
    store.commit(&create(id, 1, None)).unwrap();
    let claim = |wall_ms, who: &str| {
        op(
            id,
            wall_ms,
            who,
            Payload::Claim(Claim {
                state: "in-progress".into(),
                assignee: ActorId::new(who),
            }),
        )
    };
    let t = store.commit(&claim(2, "claude:a")).unwrap().unwrap();
    assert_eq!(t.state, "in-progress");
    assert_eq!(t.assignee, Some(ActorId::new("claude:a")));
    let err = store.commit(&claim(3, "claude:b")).unwrap_err();
    assert!(matches!(
        err,
        StoreError::ClaimRejected(ClaimRejected::NotUnstarted { .. })
    ));
    let t = store.ticket(id).unwrap().unwrap();
    assert_eq!(t.assignee, Some(ActorId::new("claude:a")));
    assert_eq!(
        store.ops(id).unwrap().len(),
        2,
        "the rejected claim was not logged"
    );
}

// ---- AC4: numbers ----

#[test]
fn number_is_nullable_and_allocated_separately_from_the_id() {
    let (_dir, mut store) = store();
    let (a, b) = (Ulid::new(), Ulid::new());
    store.commit(&create(a, 1, None)).unwrap();
    store.commit(&create(b, 2, None)).unwrap();
    assert_eq!(store.ticket(a).unwrap().unwrap().number, None);

    let matt = ActorId::new("matt");
    assert_eq!(store.allocate_number(a, &matt).unwrap(), 1);
    assert_eq!(store.allocate_number(b, &matt).unwrap(), 2);
    assert_eq!(store.ticket_by_number(2).unwrap().unwrap().id, b);
    assert!(store.ticket_by_number(3).unwrap().is_none());
    assert!(matches!(
        store.allocate_number(a, &matt),
        Err(StoreError::AlreadyNumbered { number: 1, .. })
    ));
    assert!(matches!(
        store.allocate_number(Ulid::new(), &matt),
        Err(StoreError::UnknownTicket { .. })
    ));

    // The allocation is an op like any other, stamped after everything
    // already in the log.
    let ops = store.ops(a).unwrap();
    assert_eq!(ops.last().unwrap().kind(), "field.set");
    assert!(ops.last().unwrap().hlc > Hlc::new(2, 0));

    // Imported numbers arrive as ops too, and duplicates are refused (R5).
    let c = Ulid::new();
    store.commit(&create(c, 3, None)).unwrap();
    let err = store
        .commit(&op(c, 4, "matt", Payload::FieldSet(FieldSet::Number(2))))
        .unwrap_err();
    assert!(
        matches!(err, StoreError::DuplicateNumber { number: 2 }),
        "{err}"
    );
    store
        .commit(&op(c, 4, "matt", Payload::FieldSet(FieldSet::Number(1300))))
        .unwrap();
    assert!(store.allocate_number(Ulid::new(), &matt).is_err());
    let d = Ulid::new();
    store.commit(&create(d, 5, None)).unwrap();
    assert_eq!(
        store.allocate_number(d, &matt).unwrap(),
        1301,
        "allocation continues after the highest imported number"
    );
}

#[test]
fn one_hundred_concurrent_allocations_never_share_a_number() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("pm.sqlite");
    Store::open(&path)
        .unwrap()
        .init_workspace(&workspace(), &pm_core::ActorId::new("matt"))
        .unwrap();

    let handles: Vec<_> = (0..100)
        .map(|i| {
            let path = path.clone();
            thread::spawn(move || {
                let mut store = Store::open(&path).unwrap();
                let id = Ulid::new();
                store.commit(&create(id, 1, None)).unwrap();
                let actor = ActorId::new(format!("claude:worker-{i}"));
                (id, store.allocate_number(id, &actor).unwrap())
            })
        })
        .collect();
    let allocated: Vec<(Ulid, u64)> = handles.into_iter().map(|h| h.join().unwrap()).collect();

    let numbers: BTreeSet<u64> = allocated.iter().map(|(_, n)| *n).collect();
    assert_eq!(numbers.len(), 100, "duplicate numbers: {allocated:?}");
    assert_eq!(
        numbers.iter().copied().collect::<Vec<_>>(),
        (1..=100).collect::<Vec<_>>()
    );

    let store = Store::open(&path).unwrap();
    for (id, n) in allocated {
        assert_eq!(store.ticket_by_number(n).unwrap().unwrap().id, id);
    }
    assert_eq!(store.tickets(&TicketFilter::default()).unwrap().len(), 100);
}

// ---- AC5: query API ----

#[test]
fn list_filters_by_state_project_label_repo_assignee_and_held() {
    let (_dir, mut store) = store();
    store
        .put_project(&project("think-3"), &pm_core::ActorId::new("matt"))
        .unwrap();
    let (a, b, c) = (Ulid::new(), Ulid::new(), Ulid::new());
    store.commit(&create(a, 1, Some("pm"))).unwrap();
    store.commit(&create(b, 2, Some("think-3"))).unwrap();
    store.commit(&create(c, 3, None)).unwrap();
    store
        .commit(&op(
            b,
            4,
            "matt",
            Payload::FieldSet(FieldSet::Repo(Some("OpenThinkAi/think-cli".into()))),
        ))
        .unwrap();
    store
        .commit(&op(
            a,
            5,
            "matt",
            Payload::LabelAdd(LabelAdd {
                label: "model:fable-5".into(),
            }),
        ))
        .unwrap();
    store
        .commit(&op(
            a,
            6,
            "claude:pm-build",
            Payload::Claim(Claim {
                state: "in-progress".into(),
                assignee: ActorId::new("claude:pm-build"),
            }),
        ))
        .unwrap();
    store
        .commit(&op(
            c,
            7,
            "matt",
            Payload::HoldSet(HoldSet {
                hold: Hold {
                    reason: "needs Matt".into(),
                    by: ActorId::new("matt"),
                    at: Hlc::new(7, 0),
                },
            }),
        ))
        .unwrap();
    let matt = ActorId::new("matt");
    store.allocate_number(c, &matt).unwrap();
    store.allocate_number(a, &matt).unwrap();

    assert_eq!(
        ids(&store, TicketFilter::default()),
        [c, a, b],
        "numbered tickets first (c=1, a=2), then AGT-? by creation"
    );
    assert_eq!(
        ids(
            &store,
            TicketFilter {
                state: vec!["in-progress".into()],
                ..Default::default()
            }
        ),
        [a]
    );
    assert_eq!(
        ids(
            &store,
            TicketFilter {
                project: vec!["think-3".into()],
                ..Default::default()
            }
        ),
        [b]
    );
    assert_eq!(
        ids(
            &store,
            TicketFilter {
                label: vec!["model:fable-5".into()],
                ..Default::default()
            }
        ),
        [a]
    );
    assert_eq!(
        ids(
            &store,
            TicketFilter {
                repo: vec!["OpenThinkAi/pm".into()],
                ..Default::default()
            }
        ),
        [c, a]
    );
    assert_eq!(
        ids(
            &store,
            TicketFilter {
                assignee: vec![ActorId::new("claude:pm-build")],
                ..Default::default()
            }
        ),
        [a]
    );
    assert_eq!(
        ids(
            &store,
            TicketFilter {
                held: true,
                ..Default::default()
            }
        ),
        [c]
    );
    assert_eq!(
        ids(
            &store,
            TicketFilter {
                state: vec!["triage".into()],
                held: true,
                ..Default::default()
            }
        ),
        [c]
    );
    assert!(
        ids(
            &store,
            TicketFilter {
                state: vec!["done".into()],
                held: true,
                ..Default::default()
            }
        )
        .is_empty(),
        "filters combine with AND"
    );
    assert_eq!(
        ids(
            &store,
            TicketFilter {
                state: vec!["triage".into(), "in-progress".into()],
                ..Default::default()
            }
        ),
        [c, a, b],
        "a multi-value filter matches any one of its values (b is still triage, never claimed)"
    );

    store.commit(&op(b, 8, "matt", Payload::Tombstone)).unwrap();
    assert_eq!(
        ids(&store, TicketFilter::default()),
        [c, a],
        "tombstoned tickets are not listed"
    );
    assert!(
        store.ticket(b).unwrap().unwrap().deleted,
        "but still readable by id"
    );
}

/// `pm list --github`, `--search` and `--archived` (AGT-1339 AC1).
#[test]
fn list_filters_by_github_search_and_archived() {
    let (_dir, mut store) = store();
    let (a, b, c) = (Ulid::new(), Ulid::new(), Ulid::new());
    store.commit(&create(a, 1, Some("pm"))).unwrap();
    store.commit(&create(b, 2, Some("pm"))).unwrap();
    store.commit(&create(c, 3, Some("pm"))).unwrap();
    store
        .commit(&op(
            a,
            4,
            "matt",
            Payload::FieldSet(FieldSet::LinkedGithub(Some(
                "https://github.com/OpenThinkAi/pm/issues/1".into(),
            ))),
        ))
        .unwrap();
    store
        .commit(&op(
            b,
            5,
            "matt",
            Payload::FieldSet(FieldSet::LinkedGithub(Some(
                "https://github.com/OpenThinkAi/pm/issues/2".into(),
            ))),
        ))
        .unwrap();
    store
        .commit(&op(
            a,
            6,
            "matt",
            Payload::BodyEdit(pm_core::op::BodyEdit {
                update: {
                    let mut body = pm_core::Body::new();
                    body.diff_from_text("needle in the body")
                        .unwrap()
                        .into_bytes()
                },
            }),
        ))
        .unwrap();

    assert_eq!(
        ids(
            &store,
            TicketFilter {
                github: vec!["https://github.com/OpenThinkAi/pm/issues/1".into()],
                ..Default::default()
            }
        ),
        [a]
    );
    assert_eq!(
        ids(
            &store,
            TicketFilter {
                github: vec![
                    "https://github.com/OpenThinkAi/pm/issues/1".into(),
                    "https://github.com/OpenThinkAi/pm/issues/2".into(),
                ],
                ..Default::default()
            }
        ),
        [a, b],
        "a multi-value github filter matches either url"
    );
    assert_eq!(
        ids(
            &store,
            TicketFilter {
                search: Some("needle".into()),
                ..Default::default()
            }
        ),
        [a],
        "search matches the description"
    );
    assert_eq!(
        ids(
            &store,
            TicketFilter {
                search: Some("TICKET".into()),
                ..Default::default()
            }
        ),
        [a, b, c],
        "search is case-insensitive and matches the title"
    );
    assert!(
        ids(
            &store,
            TicketFilter {
                search: Some("nothing matches this".into()),
                ..Default::default()
            }
        )
        .is_empty()
    );

    store
        .commit(&op(
            c,
            7,
            "matt",
            Payload::FieldSet(FieldSet::ArchivedAt(Some(Hlc::new(7, 0)))),
        ))
        .unwrap();
    assert_eq!(
        ids(&store, TicketFilter::default()),
        [a, b],
        "archived tickets are excluded by default"
    );
    assert_eq!(
        ids(
            &store,
            TicketFilter {
                archived: true,
                ..Default::default()
            }
        ),
        [a, b, c],
        "--archived includes them"
    );
}

#[test]
fn projects_list_and_show_with_their_documents() {
    let (_dir, mut store) = store();
    let mut child = project("pm-hub");
    child.parent = Some("pm".into());
    child.status = ProjectStatus::Complete;
    store
        .put_project(&child, &pm_core::ActorId::new("matt"))
        .unwrap();

    let listed = store.projects().unwrap();
    assert_eq!(
        listed.iter().map(|p| p.id.as_str()).collect::<Vec<_>>(),
        ["pm", "pm-hub"]
    );
    assert_eq!(store.project("pm-hub").unwrap().unwrap(), child);
    assert!(store.project("nope").unwrap().is_none());

    let mut orphan = project("orphan");
    orphan.parent = Some("nope".into());
    assert!(matches!(
        store.put_project(&orphan, &pm_core::ActorId::new("matt")),
        Err(StoreError::UnknownProject { project }) if project == "nope"
    ));

    // Putting a project again rewrites the documents it lists; one it
    // does not list stays (AGT-1413: there is no document-remove kind).
    let mut pm = project("pm");
    pm.documents = [("research/spike".to_string(), "loro".to_string())].into();
    store
        .put_project(&pm, &pm_core::ActorId::new("matt"))
        .unwrap();
    let mut expected = pm.clone();
    expected.documents.extend(project("pm").documents);
    assert_eq!(store.project("pm").unwrap().unwrap(), expected);
    assert!(store.doctor().unwrap().is_healthy());
}

// ---- AC6: every op has an actor and an HLC ----

#[test]
fn the_schema_rejects_ops_without_an_actor_or_an_hlc() {
    let (dir, mut store) = store();
    let id = Ulid::new();
    store.commit(&create(id, 1, None)).unwrap();

    let conn = Connection::open(dir.path().join("pm.sqlite")).unwrap();
    conn.pragma_update(None, "foreign_keys", "ON").unwrap();
    let attempt = |actor: Option<&str>, wall: Option<i64>, counter: Option<i64>| {
        conn.execute(
            "INSERT INTO ops (op_id, hlc_wall_ms, hlc_counter, actor, entity, kind, version)
             VALUES (?1, ?2, ?3, ?4, ?5, 'tombstone', 1)",
            rusqlite::params![
                Ulid::new().to_string(),
                wall,
                counter,
                actor,
                id.to_string()
            ],
        )
    };
    for (actor, wall, counter) in [
        (None, Some(1), Some(0)),
        (Some(""), Some(1), Some(0)),
        (Some("ghost"), Some(1), Some(0)),
        (Some("matt"), None, Some(0)),
        (Some("matt"), Some(1), None),
    ] {
        let err = attempt(actor, wall, counter).unwrap_err();
        assert!(
            matches!(
                err,
                rusqlite::Error::SqliteFailure(e, _)
                    if e.code == rusqlite::ErrorCode::ConstraintViolation
            ),
            "actor={actor:?} wall={wall:?} counter={counter:?}: {err}"
        );
    }
    attempt(Some("matt"), Some(1), Some(0)).unwrap();
}

#[test]
fn actors_are_registered_with_their_kind_on_first_op() {
    let (dir, mut store) = store();
    let id = Ulid::new();
    store.commit(&create(id, 1, None)).unwrap();
    store
        .commit(&op(
            id,
            2,
            "claude:pm-build",
            Payload::FieldSet(FieldSet::Assignee(Some(ActorId::new("bob")))),
        ))
        .unwrap();
    let conn = Connection::open(dir.path().join("pm.sqlite")).unwrap();
    let actors: Vec<(String, String)> = conn
        .prepare("SELECT id, kind FROM actor ORDER BY id")
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(
        actors,
        [
            ("bob".to_string(), "human".to_string()),
            ("claude:pm-build".to_string(), "agent".to_string()),
            ("matt".to_string(), "human".to_string()),
        ]
    );
}
