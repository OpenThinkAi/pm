//! AGT-1467: `apply_pulled_page` — a pulled op that cannot apply is
//! parked (waiting on an op not pulled yet) or refused (never admissible)
//! instead of failing every later pull, the cursor moves with the page in
//! one transaction, the retry loop is bounded, and — the property the
//! whole design rests on — every replica ends in the same state, with the
//! same ops quarantined, however the log is cut into pages.

use pm_core::op::{
    ActorUpsert, BodyEdit, CommentAdd, FieldSet, LabelAdd, ProjectCreate, ProjectDocAdd,
    TicketCreate,
};
use pm_core::{
    ActorId, ActorKind, Body, Hlc, Op, Payload, Priority, Project, ProjectStatus, State,
    StateCategory, Workspace,
};
use pm_store::{MAX_PARK_RETRIES, QuarantineStatus, Quarantined, Store, StoreError};
use tempfile::TempDir;
use ulid::Ulid;

/// The hub log's head: a workspace, its states and one project, as a
/// seeding replica would have pushed them.
struct Log {
    ws: Ulid,
    project: Ulid,
    config: Vec<Op>,
}

fn log_head() -> Log {
    let dir = tempfile::tempdir().unwrap();
    let mut source = Store::open(dir.path().join("pm.sqlite")).unwrap();
    let ws = Workspace {
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
    };
    source.init_workspace(&ws, &ActorId::new("matt")).unwrap();
    source
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
    let config: Vec<Op> = source
        .ops_since(0)
        .unwrap()
        .into_iter()
        .map(|(_, op)| op)
        .collect();
    let project = config
        .iter()
        .find(|op| matches!(op.payload, Payload::ProjectCreate(_)))
        .unwrap()
        .entity;
    Log {
        ws: ws.id,
        project,
        config,
    }
}

/// An empty replica of the log's workspace (`pm init --join`).
fn replica(log: &Log) -> (TempDir, Store) {
    let dir = tempfile::tempdir().unwrap();
    let mut store = Store::open(dir.path().join("pm.sqlite")).unwrap();
    store.join_workspace(log.ws, "AGT").unwrap();
    (dir, store)
}

fn op(entity: Ulid, wall_ms: u64, actor: &str, payload: Payload) -> Op {
    Op::new(
        Ulid::new(),
        Hlc::new(wall_ms, 0),
        ActorId::new(actor),
        entity,
        payload,
    )
}

fn create(ticket: Ulid, wall_ms: u64, project: &str) -> Op {
    op(
        ticket,
        wall_ms,
        "laptop",
        Payload::TicketCreate(TicketCreate {
            title: format!("t{wall_ms}"),
            state: "triage".into(),
            priority: Priority::Medium,
            project: Some(project.into()),
            repo: None,
            source: None,
            ext: Default::default(),
        }),
    )
}

fn comment(ticket: Ulid, wall_ms: u64, body: &str) -> Op {
    op(
        ticket,
        wall_ms,
        "laptop",
        Payload::CommentAdd(CommentAdd { body: body.into() }),
    )
}

fn actor_upsert(ws: Ulid, wall_ms: u64, id: &str) -> Op {
    op(
        ws,
        wall_ms,
        "laptop",
        Payload::ActorUpsert(ActorUpsert {
            id: ActorId::new(id),
            kind: ActorKind::Human,
        }),
    )
}

/// `ops` numbered as the hub would serve them, from seq `from`.
fn seqd(from: i64, ops: &[Op]) -> Vec<(i64, Op)> {
    ops.iter()
        .enumerate()
        .map(|(i, op)| (from + i as i64, op.clone()))
        .collect()
}

/// Pulls `log` (hub seq order) cut into pages at `cuts`.
fn pull_in_pages(store: &mut Store, log: &[(i64, Op)], cuts: &[usize]) {
    let mut start = 0;
    for &end in cuts.iter().chain(std::iter::once(&log.len())) {
        let end = end.min(log.len());
        if end <= start {
            continue;
        }
        let page = &log[start..end];
        store
            .apply_pulled_page(page, page.last().unwrap().0)
            .unwrap();
        start = end;
    }
}

fn quarantine_of(store: &Store) -> Vec<(Ulid, QuarantineStatus, u32, String)> {
    store
        .quarantine()
        .unwrap()
        .into_iter()
        .map(|q| (q.op_id, q.status, q.attempts, q.reason))
        .collect()
}

fn refused_ids(store: &Store) -> Vec<Ulid> {
    store
        .quarantine()
        .unwrap()
        .into_iter()
        .filter(|q| q.status == QuarantineStatus::Refused)
        .map(|q| q.op_id)
        .collect()
}

/// AC1: one inadmissible op among good ones is refused, recorded with its
/// reason, and the rest of the page lands; the cursor passes it, and the
/// next page applies as if nothing happened.
#[test]
fn a_refused_op_does_not_wedge_the_pull() {
    let log = log_head();
    let (_d, mut store) = replica(&log);
    let n = log.config.len() as i64;
    store.apply_pulled_page(&seqd(1, &log.config), n).unwrap();

    let (a, b) = (Ulid::new(), Ulid::new());
    let bad = {
        // A ticket in a project whose id is a Windows drive prefix.
        let mut bad = create(Ulid::new(), 11, "pm");
        if let Payload::TicketCreate(c) = &mut bad.payload {
            c.project = Some("C:evil".into());
        }
        bad
    };
    let page = seqd(
        n + 1,
        &[create(a, 10, "pm"), bad.clone(), create(b, 12, "pm")],
    );
    let pulled = store.apply_pulled_page(&page, n + 3).unwrap();
    assert_eq!((pulled.applied, pulled.skipped, pulled.parked), (2, 0, 0));
    assert_eq!(pulled.refused.len(), 1);
    let refused = &pulled.refused[0];
    assert_eq!(refused.op_id, bad.op_id);
    assert_eq!(refused.hub_seq, n + 2);
    assert_eq!(refused.kind, "ticket.create");
    assert_eq!(refused.status, QuarantineStatus::Refused);
    assert!(
        refused.reason.contains("not safe in a file path"),
        "{refused:?}"
    );
    assert_eq!(store.cursor().unwrap(), n + 3);
    assert!(store.ticket(a).unwrap().is_some() && store.ticket(b).unwrap().is_some());
    assert!(store.ticket(bad.entity).unwrap().is_none());
    assert!(
        !store
            .ops_since(0)
            .unwrap()
            .iter()
            .any(|(_, o)| o.op_id == bad.op_id),
        "a refused op never enters the log"
    );

    // The same page again (a crash re-pull): at or below the cursor.
    let again = store.apply_pulled_page(&page, n + 3).unwrap();
    assert_eq!((again.applied, again.skipped), (0, 3));
    assert!(again.refused.is_empty());

    // Later pages flow; the quarantine is visible to `pm doctor`.
    let c = Ulid::new();
    let pulled = store
        .apply_pulled_page(&seqd(n + 4, &[create(c, 13, "pm")]), n + 4)
        .unwrap();
    assert_eq!(pulled.applied, 1);
    let status = store.sync_status().unwrap();
    assert_eq!((status.parked, status.refused), (0, 1));
    let report = store.doctor().unwrap();
    assert!(report.is_healthy(), "{report:#?}");
    assert_eq!(report.quarantine.len(), 1);
    assert_eq!(report.quarantine[0].op_id, bad.op_id);
    // Quarantined ops are never outbox: nothing to push.
    assert_eq!(status.outbox, 0);
}

/// An op ahead of its ticket's creation, a page apart: parked, then
/// applied when the creation arrives — the cursor moved past it meanwhile.
#[test]
fn a_parked_op_lands_when_its_dependency_arrives_on_a_later_page() {
    let log = log_head();
    let (_d, mut store) = replica(&log);
    let n = log.config.len() as i64;
    store.apply_pulled_page(&seqd(1, &log.config), n).unwrap();
    let t = Ulid::new();
    let early = comment(t, 20, "before the create");
    let pulled = store
        .apply_pulled_page(&seqd(n + 1, std::slice::from_ref(&early)), n + 1)
        .unwrap();
    assert_eq!((pulled.applied, pulled.parked), (0, 1));
    assert_eq!(store.cursor().unwrap(), n + 1);
    let q = store.quarantine().unwrap();
    assert_eq!(q.len(), 1);
    assert_eq!(q[0].status, QuarantineStatus::Parked);
    assert!(q[0].reason.contains("does not exist"), "{q:?}");
    assert_eq!(store.sync_status().unwrap().parked, 1);

    let pulled = store
        .apply_pulled_page(&seqd(n + 2, &[create(t, 19, "pm")]), n + 2)
        .unwrap();
    assert_eq!((pulled.applied, pulled.unparked, pulled.parked), (2, 1, 0));
    assert!(store.quarantine().unwrap().is_empty());
    assert_eq!(store.comments(t).unwrap().len(), 1);
    assert!(store.doctor().unwrap().is_healthy());
}

/// Bounded retries: a parked op is retried each time something that could
/// supply it lands, and refused after `MAX_PARK_RETRIES` of them.
#[test]
fn a_dependency_that_never_comes_is_given_up_after_the_retry_cap() {
    let log = log_head();
    let (_d, mut store) = replica(&log);
    let n = log.config.len() as i64;
    store.apply_pulled_page(&seqd(1, &log.config), n).unwrap();
    // Waits on project "nope", which never comes: every config op wakes it.
    let orphan = create(Ulid::new(), 30, "nope");
    let mut ops = vec![orphan.clone()];
    for i in 0..MAX_PARK_RETRIES {
        ops.push(actor_upsert(log.ws, 31 + u64::from(i), &format!("a{i}")));
    }
    // One short of the cap: still parked.
    let (almost, last) = ops.split_at(ops.len() - 1);
    let pulled = store
        .apply_pulled_page(&seqd(n + 1, almost), n + almost.len() as i64)
        .unwrap();
    assert!(pulled.refused.is_empty());
    let q = store.quarantine().unwrap();
    assert_eq!(
        (q[0].status, q[0].attempts),
        (QuarantineStatus::Parked, MAX_PARK_RETRIES - 1)
    );
    let pulled = store
        .apply_pulled_page(
            &seqd(n + 1 + almost.len() as i64, last),
            n + ops.len() as i64,
        )
        .unwrap();
    assert_eq!(pulled.refused.len(), 1);
    let refused: &Quarantined = &pulled.refused[0];
    assert_eq!(refused.op_id, orphan.op_id);
    assert_eq!(refused.attempts, MAX_PARK_RETRIES);
    assert!(
        refused
            .reason
            .starts_with(&format!("still waiting after {MAX_PARK_RETRIES} retries")),
        "{refused:?}"
    );
    // Never retried again.
    let more = store
        .apply_pulled_page(
            &seqd(n + 1 + ops.len() as i64, &[actor_upsert(log.ws, 99, "z")]),
            n + 1 + ops.len() as i64,
        )
        .unwrap();
    assert!(more.refused.is_empty());
    assert_eq!(store.quarantine().unwrap()[0].attempts, MAX_PARK_RETRIES);
}

/// Bounded work: a chain of description edits delivered newest first —
/// each needs the one after it — costs at most `1 + MAX_PARK_RETRIES`
/// attempts per op instead of a pass per link. The first
/// `MAX_PARK_RETRIES` links land; the older tail, retried once per landed
/// link, is refused — on every replica alike (see the convergence test).
#[test]
fn a_reversed_edit_chain_is_bounded() {
    let log = log_head();
    let (_d, mut store) = replica(&log);
    let n = log.config.len() as i64;
    store.apply_pulled_page(&seqd(1, &log.config), n).unwrap();
    let t = Ulid::new();
    let links = MAX_PARK_RETRIES as usize + 8;
    let chain = edit_chain(t, links, 100);
    let mut ops = vec![create(t, 99, "pm")];
    ops.extend(chain.iter().rev().cloned());
    let started = std::time::Instant::now();
    let pulled = store
        .apply_pulled_page(&seqd(n + 1, &ops), n + ops.len() as i64)
        .unwrap();
    // Newest first: link k (1-based) is retried once per link below it
    // that lands, so links above MAX_PARK_RETRIES + 1 run out of retries.
    assert_eq!(pulled.refused.len(), links - MAX_PARK_RETRIES as usize - 1);
    assert_eq!(pulled.applied, 1 + MAX_PARK_RETRIES as usize + 1);
    assert!(started.elapsed() < std::time::Duration::from_secs(60));
    assert!(store.doctor().unwrap().is_healthy());
    // In order, the same chain lands whole.
    let (_d2, mut other) = replica(&log);
    let mut ops = log.config.clone();
    ops.push(create(t, 99, "pm"));
    ops.extend(chain);
    let pulled = other
        .apply_pulled_page(&seqd(1, &ops), ops.len() as i64)
        .unwrap();
    assert!(pulled.refused.is_empty());
    assert_eq!(pulled.parked, 0);
}

/// `n` description edits of ticket `t`, each building on the last.
fn edit_chain(t: Ulid, n: usize, wall_ms: u64) -> Vec<Op> {
    let mut body = Body::with_peer(7).unwrap();
    let mut text = String::new();
    (0..n)
        .map(|i| {
            text.push_str(&format!("line {i}\n"));
            let update = body.diff_from_text(&text).unwrap();
            op(
                t,
                wall_ms + i as u64,
                "laptop",
                Payload::BodyEdit(BodyEdit {
                    update: update.into_bytes(),
                }),
            )
        })
        .collect()
}

/// The hub log the convergence test pulls: honest ops, ops ahead of what
/// they need (a page or more apart), and every kind of inadmissible op —
/// with dependencies between the two kinds, so where a parked op lands
/// decides what is refused later.
fn hostile_log(log: &Log) -> (Vec<(i64, Op)>, Vec<Ulid>, Op) {
    let (t1, t2, t3, ghost) = (Ulid::new(), Ulid::new(), Ulid::new(), Ulid::new());
    let mut ops = log.config.clone();
    ops.push(create(t1, 1_000, "pm"));
    ops.push(comment(t2, 1_001, "ahead of t2")); // parked until t2
    ops.push(op(t2, 1_002, "hub", Payload::FieldSet(FieldSet::Number(7)))); // parked until t2; then refused: t1 takes 7 first below
    ops.push(op(t1, 1_003, "hub", Payload::FieldSet(FieldSet::Number(7))));
    ops.push(comment(ghost, 1_004, "never created")); // parked for good
    // Inadmissible outright.
    let mut unsafe_state = create(t3, 1_005, "pm");
    if let Payload::TicketCreate(c) = &mut unsafe_state.payload {
        c.state = "NUL".into();
    }
    ops.push(unsafe_state);
    ops.push(op(
        log.project,
        1,
        "matt",
        Payload::ProjectCreate(ProjectCreate {
            kind: Default::default(),
            id: "pm".into(),
            title: "backdated twin".into(),
            status: ProjectStatus::InProgress,
            parent: None,
            doc_id: None,
        }),
    ));
    ops.push(op(
        log.project,
        1_006,
        "laptop",
        Payload::ProjectDocAdd(ProjectDocAdd {
            name: Some("notes".into()),
            doc_id: t1,
        }),
    ));
    // Filler, some of it config (which wakes every parked op).
    for i in 0..6 {
        ops.push(op(
            t1,
            1_010 + i,
            "laptop",
            Payload::LabelAdd(LabelAdd {
                label: format!("l{i}"),
            }),
        ));
        ops.push(actor_upsert(log.ws, 1_020 + i, &format!("a{i}")));
    }
    let t2_create = create(t2, 1_030, "pm");
    ops.push(t2_create.clone());
    ops.push(comment(t2, 1_031, "after t2"));
    // A description chain on t1 delivered newest first, longer than the
    // retry cap, so some of it is refused.
    ops.extend(
        edit_chain(t1, MAX_PARK_RETRIES as usize + 4, 2_000)
            .into_iter()
            .rev(),
    );
    ops.push(comment(t1, 3_000, "tail"));
    (seqd(1, &ops), vec![t1, t2, t3, ghost], t2_create)
}

/// Everything a replica's state is: every ticket, the set of ops in its
/// log, and its quarantine.
fn state_of(store: &Store, tickets: &[Ulid]) -> impl PartialEq + std::fmt::Debug + use<> {
    let mut log: Vec<Ulid> = store
        .ops_since(0)
        .unwrap()
        .into_iter()
        .map(|(_, op)| op.op_id)
        .collect();
    log.sort();
    let tickets: Vec<_> = tickets.iter().map(|t| store.ticket(*t).unwrap()).collect();
    let comments: Vec<_> = tickets
        .iter()
        .flatten()
        .map(|t| store.comments(t.id).unwrap().len())
        .collect();
    (
        log,
        tickets,
        comments,
        quarantine_of(store),
        store.projects().unwrap(),
    )
}

/// The property: replicas that pull the same log in different page cuts —
/// one op per page, one page, and odd sizes — and one that authored an op
/// itself (so it arrives as an echo) end in the same state and quarantine
/// the same ops, with the same retry counts.
#[test]
fn every_page_cut_converges_on_the_same_state_and_quarantine() {
    let log = log_head();
    let (hub_log, tickets, t2_create) = hostile_log(&log);
    let len = hub_log.len();

    let mut cuts: Vec<(String, Vec<usize>)> = vec![
        ("one page".into(), vec![]),
        ("one op per page".into(), (1..len).collect()),
    ];
    for size in [2, 3, 5, 7, 11] {
        cuts.push((
            format!("pages of {size}"),
            (1..len).filter(|i| i.is_multiple_of(size)).collect(),
        ));
    }
    // Pseudo-random cuts, fixed seed.
    let mut x: u64 = 0x2545_F491_4F6C_DD1D;
    for round in 0..4 {
        let mut c = Vec::new();
        for i in 1..len {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            if x.is_multiple_of(4) {
                c.push(i);
            }
        }
        cuts.push((format!("random {round}"), c));
    }

    let (_d, mut reference) = replica(&log);
    pull_in_pages(&mut reference, &hub_log, &[]);
    let expected = state_of(&reference, &tickets);
    // Sanity: the log really exercises each outcome.
    let q = reference.quarantine().unwrap();
    assert!(
        q.iter().any(|q| q.status == QuarantineStatus::Parked),
        "{q:#?}"
    );
    let refused = refused_ids(&reference);
    assert!(refused.len() >= 5, "{q:#?}");
    assert!(reference.doctor().unwrap().is_healthy());
    // t2 got its early comment once it was created, but not number 7.
    let t2 = reference.ticket(tickets[1]).unwrap().unwrap();
    assert_eq!(t2.number, None);
    assert_eq!(reference.comments(tickets[1]).unwrap().len(), 2);
    assert_eq!(
        reference.ticket(tickets[0]).unwrap().unwrap().number,
        Some(7)
    );

    for (name, cut) in &cuts {
        let (_d, mut store) = replica(&log);
        pull_in_pages(&mut store, &hub_log, cut);
        assert_eq!(state_of(&store, &tickets), expected, "{name}");
        assert!(store.doctor().unwrap().is_healthy(), "{name}");
    }

    // The author of t2's creation: it pulls up to the op before it,
    // commits the same op itself, then pulls the rest (its own op coming
    // back as an echo, which still wakes what waits on it).
    let at = hub_log
        .iter()
        .position(|(_, op)| op.op_id == t2_create.op_id)
        .unwrap();
    let (_d, mut author) = replica(&log);
    pull_in_pages(&mut author, &hub_log[..at], &[]);
    author.commit(&t2_create).unwrap();
    pull_in_pages(&mut author, &hub_log[at..], &[1, 4]);
    assert_eq!(state_of(&author, &tickets), expected, "author");

    // An author that holds t2 *earlier* than its hub position, while ops
    // naming t2 are already parked: only a log that references an entity
    // before its creation reached the hub (a hostile one — nobody else can
    // know a fresh id) does this. Config ops in between then retry the
    // parked ops against the author's own t2, so they are decided sooner
    // (retry counts differ); here the outcome is still the same.
    let (_d, mut early) = replica(&log);
    pull_in_pages(&mut early, &hub_log[..at - 3], &[]);
    early.commit(&t2_create).unwrap();
    pull_in_pages(&mut early, &hub_log[at - 3..], &[1, 4]);
    let without_attempts = |store: &Store| {
        let mut q = quarantine_of(store);
        for entry in &mut q {
            entry.2 = 0;
        }
        (store.ops_since(0).unwrap().len(), q)
    };
    assert_eq!(without_attempts(&early), without_attempts(&reference));
}

/// The two failures that are not a property of the op fail the page and
/// leave the cursor alone, as before: a stamp far ahead of this machine's
/// clock (quarantining it would make the decision depend on the clock).
#[test]
fn a_far_future_stamp_fails_the_page_instead_of_being_quarantined() {
    let log = log_head();
    let (_d, mut store) = replica(&log);
    let n = log.config.len() as i64;
    store.apply_pulled_page(&seqd(1, &log.config), n).unwrap();
    let mut ahead = create(Ulid::new(), 0, "pm");
    ahead.hlc = Hlc::new(u64::MAX / 4, 0);
    let err = store
        .apply_pulled_page(&seqd(n + 1, &[create(Ulid::new(), 5, "pm"), ahead]), n + 2)
        .unwrap_err();
    assert!(
        matches!(&err, StoreError::Pull { source, .. }
            if matches!(**source, StoreError::InvalidStamp(pm_core::StampError::FarFuture { .. }))),
        "{err:?}"
    );
    assert_eq!(store.cursor().unwrap(), n);
    assert!(store.quarantine().unwrap().is_empty());
}

/// AC2: an oversized `body.edit` is refused on pull (and so never folded
/// or re-served); one at the bound would pass the size check.
#[test]
fn an_oversized_body_edit_is_refused_on_pull() {
    let log = log_head();
    let (_d, mut store) = replica(&log);
    let n = log.config.len() as i64;
    let t = Ulid::new();
    let mut ops = log.config.clone();
    ops.push(create(t, 10, "pm"));
    ops.push(op(
        t,
        11,
        "laptop",
        Payload::BodyEdit(BodyEdit {
            update: vec![0; pm_core::MAX_BODY_EDIT_BYTES + 1],
        }),
    ));
    let pulled = store.apply_pulled_page(&seqd(1, &ops), n + 2).unwrap();
    assert_eq!(pulled.refused.len(), 1);
    assert!(
        pulled.refused[0].reason.contains("over the"),
        "{:?}",
        pulled.refused[0]
    );
    assert!(store.ticket(t).unwrap().is_some());
}

// ------------------------------------------------------------ AGT-1482

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

fn claim(ticket: Ulid, wall_ms: u64, state: &str) -> Op {
    op(
        ticket,
        wall_ms,
        "laptop",
        Payload::Claim(pm_core::op::Claim {
            state: state.into(),
            assignee: ActorId::new("laptop"),
        }),
    )
}

fn hold(ticket: Ulid, wall_ms: u64, at: Hlc) -> Op {
    op(
        ticket,
        wall_ms,
        "laptop",
        Payload::HoldSet(pm_core::op::HoldSet {
            hold: pm_core::Hold {
                reason: "r".into(),
                by: ActorId::new("laptop"),
                at,
            },
        }),
    )
}

fn archived(ticket: Ulid, wall_ms: u64, at: Hlc) -> Op {
    op(
        ticket,
        wall_ms,
        "laptop",
        Payload::FieldSet(FieldSet::ArchivedAt(Some(at))),
    )
}

/// The ops AGT-1482 makes inadmissible, each a pure function of the op:
/// a claim whose state is not path-safe, payload stamps out of range or
/// more than a day ahead of their op, and an oversized non-`body.edit`
/// payload. All are refused on pull — never fail the page — and the rest
/// lands; the same ops are refused locally.
#[test]
fn claim_states_payload_stamps_and_payload_sizes_are_refused_on_pull() {
    let log = log_head();
    let (_d, mut store) = replica(&log);
    let n = log.config.len() as i64;
    store.apply_pulled_page(&seqd(1, &log.config), n).unwrap();
    let t = Ulid::new();
    let day = pm_core::MAX_FUTURE_SKEW_MS;
    let bad = vec![
        claim(t, 11, "../../x"),
        claim(t, 12, "NUL"),
        hold(t, 13, Hlc::new(13, u32::MAX)),
        hold(t, 14, Hlc::new(u64::MAX, 0)),
        archived(t, 15, Hlc::new(i64::MAX as u64 + 1, 0)),
        archived(t, 16, Hlc::new(u64::MAX, 1)),
        comment(t, 17, &"x".repeat(pm_core::MAX_OP_PAYLOAD_BYTES)),
    ];
    let mut page = vec![create(t, 10, "pm")];
    page.extend(bad.iter().cloned());
    // Honest neighbours: a hold stamped with its op, and an archive month
    // that postdates its op (as a vault import writes).
    page.push(hold(t, 20, Hlc::new(20, 0)));
    page.push(archived(t, 21, Hlc::new(21 + 30 * day, 0)));
    page.push(comment(t, 22, "fine"));
    let pulled = store
        .apply_pulled_page(&seqd(n + 1, &page), n + page.len() as i64)
        .unwrap();
    assert_eq!(pulled.applied, 4, "{pulled:?}");
    assert_eq!(
        refused_ids(&store),
        bad.iter().map(|o| o.op_id).collect::<Vec<_>>()
    );
    let reasons: Vec<String> = pulled.refused.iter().map(|q| q.reason.clone()).collect();
    assert!(reasons[0].contains("state name"), "{reasons:?}");
    assert!(reasons[1].contains("state name"), "{reasons:?}");
    assert!(reasons[2].contains("payload hold.at"), "{reasons:?}");
    assert!(reasons[3].contains("payload hold.at"), "{reasons:?}");
    assert!(reasons[4].contains("payload archived_at"), "{reasons:?}");
    assert!(reasons[5].contains("out of range"), "{reasons:?}");
    assert!(reasons[6].contains("comment.add payload"), "{reasons:?}");
    let ticket = store.ticket(t).unwrap().unwrap();
    assert_eq!(ticket.state, "triage");
    assert_eq!(ticket.archived_at, Some(Hlc::new(21 + 30 * day, 0)));
    assert_eq!(ticket.hold.unwrap().at, Hlc::new(20, 0));

    // Local commits refuse them the same way.
    for o in &bad {
        let mut local = o.clone();
        local.op_id = Ulid::new();
        let err = store.commit(&local).unwrap_err();
        assert!(
            matches!(
                err,
                StoreError::InvalidId(_) | StoreError::InvalidStamp(_) | StoreError::OpTooLarge(_)
            ),
            "{}: {err:?}",
            o.kind()
        );
    }
}

/// A payload stamp far ahead of this machine's clock is judged like the
/// op's own stamp: it fails the page (the verdict depends on the clock, so
/// quarantining it would split replicas), and is refused locally past the
/// week.
#[test]
fn a_far_future_payload_stamp_fails_the_page_like_a_far_future_op() {
    let log = log_head();
    let (_d, mut store) = replica(&log);
    let n = log.config.len() as i64;
    let t = Ulid::new();
    let mut ops = log.config.clone();
    ops.push(create(t, 10, "pm"));
    store.apply_pulled_page(&seqd(1, &ops), n + 1).unwrap();
    let far = now_ms() + pm_store::PULL_MAX_FUTURE_SKEW_MS + 60_000;
    for bad in [
        hold(t, 11, Hlc::new(far, 0)),
        archived(t, 12, Hlc::new(far, 0)),
    ] {
        let err = store
            .apply_pulled_page(&seqd(n + 2, std::slice::from_ref(&bad)), n + 2)
            .unwrap_err();
        assert!(
            matches!(&err, StoreError::Pull { source, .. }
                if matches!(**source, StoreError::InvalidStamp(pm_core::StampError::PayloadFarFuture { .. }))),
            "{err:?}"
        );
        assert_eq!(store.cursor().unwrap(), n + 1);
        assert!(store.quarantine().unwrap().is_empty());
    }
    let week_on = now_ms() + pm_store::LOCAL_MAX_FUTURE_SKEW_MS + 60_000;
    let err = store
        .commit(&archived(t, 13, Hlc::new(week_on, 0)))
        .unwrap_err();
    assert!(
        matches!(
            err,
            StoreError::InvalidStamp(pm_core::StampError::PayloadFarFuture { .. })
        ),
        "{err:?}"
    );
    // Within the pull's year, the same payload applies on pull.
    let pulled = store
        .apply_pulled_page(
            &seqd(n + 2, &[archived(t, 14, Hlc::new(week_on, 0))]),
            n + 2,
        )
        .unwrap();
    assert_eq!(pulled.applied, 1);
}

/// Local commits and restores allow a stamp at most a week ahead of this
/// machine's clock; a pull keeps its year, for a replica whose clock runs
/// behind.
#[test]
fn local_and_restore_stamps_have_a_tighter_future_window_than_pulls() {
    let log = log_head();
    let (_d, mut store) = replica(&log);
    let n = log.config.len() as i64;
    store.apply_pulled_page(&seqd(1, &log.config), n).unwrap();
    assert_eq!(
        pm_store::LOCAL_MAX_FUTURE_SKEW_MS,
        7 * pm_core::MAX_FUTURE_SKEW_MS
    );
    let now = now_ms();
    let ahead = |ms: u64| create(Ulid::new(), now + ms, "pm");

    // Two days ahead (a slow clock): fine locally and on restore.
    store
        .commit(&ahead(2 * pm_core::MAX_FUTURE_SKEW_MS))
        .unwrap();
    store
        .commit_any(&ahead(2 * pm_core::MAX_FUTURE_SKEW_MS))
        .unwrap();
    // Past the week: refused on both.
    let far = pm_store::LOCAL_MAX_FUTURE_SKEW_MS + 60_000;
    for err in [
        store.commit(&ahead(far)).unwrap_err(),
        store.commit_any(&ahead(far)).unwrap_err(),
    ] {
        assert!(
            matches!(
                err,
                StoreError::InvalidStamp(pm_core::StampError::FarFuture { max_skew_ms, .. })
                    if max_skew_ms == pm_store::LOCAL_MAX_FUTURE_SKEW_MS
            ),
            "{err:?}"
        );
    }
    // A config op and a document edit take the local window too.
    let mut upsert = actor_upsert(log.ws, now + far, "someone");
    upsert.op_id = Ulid::new();
    assert!(matches!(
        store.commit(&upsert).unwrap_err(),
        StoreError::InvalidStamp(_)
    ));
    // The same op, pulled, applies: the pull's window is a year.
    let pulled_op = ahead(far);
    let pulled = store
        .apply_pulled_page(&seqd(n + 1, &[pulled_op.clone(), upsert]), n + 2)
        .unwrap();
    assert_eq!(pulled.applied, 2, "{pulled:?}");
    assert!(store.ticket(pulled_op.entity).unwrap().is_some());
}

/// A refused op's content is dropped 30 days after its refusal (on the
/// next pull) or at once by `prune_quarantine`; its row — op id, seq,
/// kind, reason — stays, so the op stays refused. Parked ops keep theirs.
#[test]
fn refused_content_is_pruned_after_the_retention_window() {
    let log = log_head();
    let (dir, mut store) = replica(&log);
    let n = log.config.len() as i64;
    store.apply_pulled_page(&seqd(1, &log.config), n).unwrap();
    let t = Ulid::new();
    let (old, recent) = (claim(t, 11, "../x"), claim(t, 12, "a/b"));
    let early = comment(Ulid::new(), 13, "waits for its ticket");
    let page = seqd(
        n + 1,
        &[create(t, 10, "pm"), old.clone(), recent.clone(), early],
    );
    store.apply_pulled_page(&page, n + 4).unwrap();
    let content = |op_id: Ulid| -> String {
        let conn = rusqlite::Connection::open(dir.path().join("pm.sqlite")).unwrap();
        conn.query_row(
            "SELECT op FROM sync_quarantine WHERE op_id = ?1",
            [op_id.to_string()],
            |r| r.get(0),
        )
        .unwrap()
    };
    assert!(content(old.op_id).contains("../x"));
    assert!(store.quarantine().unwrap().iter().all(|q| !q.pruned));

    // Age one refusal past the window; the next pull prunes it alone.
    {
        let conn = rusqlite::Connection::open(dir.path().join("pm.sqlite")).unwrap();
        let aged = now_ms() - pm_store::QUARANTINE_RETENTION_MS - 1;
        conn.execute(
            "UPDATE sync_quarantine SET recorded_ms = ?2 WHERE op_id = ?1",
            rusqlite::params![old.op_id.to_string(), aged as i64],
        )
        .unwrap();
    }
    store
        .apply_pulled_page(&seqd(n + 5, &[create(Ulid::new(), 20, "pm")]), n + 5)
        .unwrap();
    assert_eq!(content(old.op_id), "");
    assert!(content(recent.op_id).contains("a/b"));
    let q = store.quarantine().unwrap();
    let pruned: Vec<(Ulid, bool)> = q.iter().map(|q| (q.op_id, q.pruned)).collect();
    assert!(pruned.contains(&(old.op_id, true)), "{pruned:?}");
    assert!(pruned.contains(&(recent.op_id, false)), "{pruned:?}");
    let kept = q.iter().find(|q| q.op_id == old.op_id).unwrap();
    assert_eq!(kept.hub_seq, n + 2);
    assert_eq!(kept.kind, "claim");
    assert!(kept.reason.contains("state name"), "{kept:?}");

    // `pm doctor --prune-quarantine`: every refused op now, parked ones
    // untouched (they are still retried).
    assert_eq!(store.prune_quarantine().unwrap(), 1);
    assert_eq!(store.prune_quarantine().unwrap(), 0);
    assert_eq!(content(recent.op_id), "");
    let q = store.quarantine().unwrap();
    for entry in &q {
        assert_eq!(
            entry.pruned,
            entry.status == QuarantineStatus::Refused,
            "{entry:?}"
        );
    }
    assert!(q.iter().any(|e| e.status == QuarantineStatus::Parked));

    // A pruned op served again is still recognised, and still refused.
    let again = store
        .apply_pulled_page(&seqd(n + 6, std::slice::from_ref(&old)), n + 6)
        .unwrap();
    assert_eq!((again.applied, again.skipped), (0, 1));
    assert_eq!(store.ticket(t).unwrap().unwrap().state, "triage");
    assert!(store.doctor().unwrap().is_healthy());
}
