//! AGT-2427: `redact` ops. A redact rewrites the payloads of the ops it
//! names (keeping op id, stamp, actor and kind) and refolds what they
//! materialized; it converges whatever order it and its targets arrive
//! in, leaves the original text nowhere in the database, and keeps `pm
//! doctor` clean.

use pm_core::op::{BodyEdit, CommentAdd, FieldSet, HoldSet, Redact, TicketCreate};
use pm_core::{
    ActorId, Body, Hlc, Hold, Op, Payload, Priority, Project, ProjectStatus, REDACTED, State,
    StateCategory, Workspace,
};
use pm_store::{Store, StoreError};
use rusqlite::Connection;
use tempfile::TempDir;
use ulid::Ulid;

const SECRET: &str = "hunter2-secret";

/// The hub log's head: a workspace, its states and a project whose
/// design doc holds the secret.
struct Log {
    ws: Ulid,
    config: Vec<Op>,
    design_doc: Ulid,
}

fn log_head() -> Log {
    let dir = tempfile::tempdir().unwrap();
    let mut source = Store::open(dir.path().join("pm.sqlite")).unwrap();
    let ws = Workspace {
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
                doc: format!("design notes: {SECRET}\n"),
                documents: Default::default(),
            },
            &ActorId::new("matt"),
        )
        .unwrap();
    let design_doc = source.design_doc_id("pm").unwrap().unwrap();
    let config = source
        .ops_since(0)
        .unwrap()
        .into_iter()
        .map(|(_, op)| op)
        .collect();
    Log {
        ws: ws.id,
        config,
        design_doc,
    }
}

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

/// A ticket whose title (twice), comment, hold reason and description
/// (two edits, the second building on the first) all carry the secret,
/// plus a comment that does not.
struct Ticket {
    id: Ulid,
    ops: Vec<Op>,
    keep_comment: Op,
}

fn secret_ticket() -> Ticket {
    secret_ticket_at(0)
}

/// [`secret_ticket`] with every stamp moved `offset` ms later.
fn secret_ticket_at(offset: u64) -> Ticket {
    let id = Ulid::new();
    let mut body = Body::new();
    let first = body.diff_from_text(&format!("intro {SECRET}\n")).unwrap();
    let second = body
        .diff_from_text(&format!("intro {SECRET}\nmore {SECRET}\n"))
        .unwrap();
    let keep_comment = op(
        id,
        1_006 + offset,
        "laptop",
        Payload::CommentAdd(CommentAdd {
            body: "harmless".into(),
        }),
    );
    let ops = vec![
        op(
            id,
            1_000 + offset,
            "laptop",
            Payload::TicketCreate(TicketCreate {
                title: format!("title {SECRET}"),
                state: "triage".into(),
                priority: Priority::High,
                project: Some("pm".into()),
                repo: None,
                source: None,
                ext: Default::default(),
            }),
        ),
        op(
            id,
            1_001 + offset,
            "laptop",
            Payload::FieldSet(FieldSet::Title(format!("retitled {SECRET}"))),
        ),
        op(
            id,
            1_002 + offset,
            "studio",
            Payload::CommentAdd(CommentAdd {
                body: format!("see {SECRET}"),
            }),
        ),
        op(
            id,
            1_003 + offset,
            "studio",
            Payload::HoldSet(HoldSet {
                hold: Hold {
                    reason: format!("waiting on {SECRET}"),
                    by: ActorId::new("studio"),
                    at: Hlc::new(1_003 + offset, 0),
                },
            }),
        ),
        op(
            id,
            1_004 + offset,
            "laptop",
            Payload::BodyEdit(BodyEdit {
                update: first.into_bytes(),
            }),
        ),
        op(
            id,
            1_005 + offset,
            "laptop",
            Payload::BodyEdit(BodyEdit {
                update: second.into_bytes(),
            }),
        ),
    ];
    Ticket {
        id,
        ops,
        keep_comment,
    }
}

fn redact(entity: Ulid, wall_ms: u64, targets: &[&Op]) -> Op {
    op(
        entity,
        wall_ms,
        "matt",
        Payload::Redact(Redact {
            targets: targets.iter().map(|o| o.op_id).collect(),
        }),
    )
}

fn seqd(ops: &[Op]) -> Vec<(i64, Op)> {
    ops.iter()
        .enumerate()
        .map(|(i, op)| (i as i64 + 1, op.clone()))
        .collect()
}

/// Every text value in every table of the database at `path`.
fn all_text(path: &std::path::Path) -> String {
    let conn = Connection::open(path).unwrap();
    let tables: Vec<String> = conn
        .prepare("SELECT name FROM sqlite_master WHERE type = 'table'")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    let mut out = String::new();
    for table in tables {
        let mut stmt = conn.prepare(&format!("SELECT * FROM {table}")).unwrap();
        let n = stmt.column_count();
        let mut rows = stmt.query([]).unwrap();
        while let Some(row) = rows.next().unwrap() {
            for i in 0..n {
                if let Ok(rusqlite::types::ValueRef::Text(t)) = row.get_ref(i) {
                    out.push_str(&String::from_utf8_lossy(t));
                    out.push('\n');
                }
            }
        }
    }
    out
}

/// What two replicas must agree on.
fn state_of(store: &Store, ticket: Ulid) -> impl PartialEq + std::fmt::Debug + use<> {
    let mut ops: Vec<(Ulid, String)> = store
        .ops_since(0)
        .unwrap()
        .into_iter()
        .map(|(_, op)| (op.op_id, serde_json::to_string(&op).unwrap()))
        .collect();
    ops.sort();
    (
        store.ticket(ticket).unwrap(),
        store.comments(ticket).unwrap(),
        store.project("pm").unwrap(),
        ops,
    )
}

fn assert_clean(dir: &TempDir, store: &mut Store) {
    let report = store.doctor().unwrap();
    assert!(report.is_healthy(), "{report:#?}");
    assert_eq!(report.unredacted, 0);
    let text = all_text(&dir.path().join("pm.sqlite"));
    assert!(!text.contains(SECRET), "the secret survived in:\n{text}");
}

/// The property: replicas that receive the redact after its targets,
/// before them, and before the ticket even exists, end in the same state;
/// the secret is gone from every op payload and every table on each, and
/// `pm doctor` is clean on each.
#[test]
fn a_redact_converges_whether_it_arrives_before_or_after_its_targets() {
    let log = log_head();
    let t = secret_ticket();
    let doc_edits: Vec<&Op> = log
        .config
        .iter()
        .filter(|o| o.entity == log.design_doc && o.kind() == "body.edit")
        .collect();
    assert!(!doc_edits.is_empty());
    let ticket_redact = redact(t.id, 2_000, &t.ops.iter().collect::<Vec<_>>());
    let doc_redact = redact(log.design_doc, 2_001, &doc_edits);

    let mut ticket_ops = t.ops.clone();
    ticket_ops.push(t.keep_comment.clone());
    let after: Vec<Op> = log
        .config
        .iter()
        .chain(&ticket_ops)
        .chain([&ticket_redact, &doc_redact])
        .cloned()
        .collect();
    let before: Vec<Op> = [&doc_redact, &ticket_redact]
        .into_iter()
        .chain(&log.config)
        .chain(&ticket_ops)
        .cloned()
        .collect();
    // The redact between the targets: half rewritten, half arriving late.
    let mut between: Vec<Op> = log.config.clone();
    between.extend(ticket_ops[..3].iter().cloned());
    between.push(ticket_redact.clone());
    between.extend(ticket_ops[3..].iter().cloned());
    between.push(doc_redact.clone());

    let mut states = Vec::new();
    for (name, order) in [("after", after), ("before", before), ("between", between)] {
        let (dir, mut store) = replica(&log);
        let pulled = store
            .apply_pulled_page(&seqd(&order), order.len() as i64)
            .unwrap();
        assert!(pulled.refused.is_empty(), "{name}: {pulled:?}");
        assert_eq!(pulled.parked, 0, "{name}: {pulled:?}");

        let ticket = store.ticket(t.id).unwrap().unwrap();
        assert_eq!(ticket.title, REDACTED, "{name}");
        assert_eq!(ticket.description, "", "{name}");
        assert_eq!(ticket.priority, Priority::High, "{name}: other fields kept");
        assert_eq!(ticket.hold.as_ref().unwrap().reason, REDACTED, "{name}");
        assert_eq!(
            ticket.hold.as_ref().unwrap().by,
            ActorId::new("studio"),
            "{name}"
        );
        let comments = store.comments(t.id).unwrap();
        assert_eq!(
            comments.iter().map(|c| c.body.as_str()).collect::<Vec<_>>(),
            [REDACTED, "harmless"],
            "{name}"
        );
        assert_eq!(store.project("pm").unwrap().unwrap().doc, "", "{name}");
        // The targets keep their envelope.
        let log_ops = store.ops(t.id).unwrap();
        for target in &t.ops {
            let held = log_ops.iter().find(|o| o.op_id == target.op_id).unwrap();
            assert_eq!(
                (held.hlc, &held.actor, held.kind()),
                (target.hlc, &target.actor, target.kind()),
                "{name}"
            );
        }
        assert_clean(&dir, &mut store);
        states.push((name, state_of(&store, t.id), dir, store));
    }
    for (name, state, _, _) in &states[1..] {
        assert_eq!(state, &states[0].1, "{name} vs {}", states[0].0);
    }

    // Editing on after a redact starts a fresh history that every replica
    // folds alike.
    for (_, _, dir, store) in &mut states {
        let mut body = Body::new();
        let edit = op(
            t.id,
            3_000,
            "laptop",
            Payload::BodyEdit(BodyEdit {
                update: body.diff_from_text("clean text\n").unwrap().into_bytes(),
            }),
        );
        let n = store.cursor().unwrap();
        store.apply_pulled_page(&[(n + 1, edit)], n + 1).unwrap();
        assert_eq!(
            store.ticket(t.id).unwrap().unwrap().description,
            "clean text\n"
        );
        assert_clean(dir, store);
    }
}

/// A local redact: the commit returns the ticket as it now reads, `pm
/// doctor` notices a target whose text was put back behind pm's back,
/// and `--rebuild` erases it again.
#[test]
fn a_local_redact_rewrites_in_place_and_doctor_guards_it() {
    let log = log_head();
    let t = secret_ticket();
    let (dir, mut store) = replica(&log);
    let mut ops = log.config.clone();
    ops.extend(t.ops.iter().cloned());
    store
        .apply_pulled_page(&seqd(&ops), ops.len() as i64)
        .unwrap();
    assert!(store.ticket(t.id).unwrap().unwrap().title.contains(SECRET));

    let comment = &t.ops[2];
    let r = redact(t.id, 2_000, &[comment]);
    let ticket = store
        .commit(&r)
        .unwrap()
        .expect("a ticket redact returns it");
    assert!(ticket.title.contains(SECRET), "only the comment was named");
    assert_eq!(store.comments(t.id).unwrap()[0].body, REDACTED);
    assert_eq!(store.outbox_len().unwrap(), 1, "the redact is pushed");
    assert!(store.doctor().unwrap().is_healthy());

    // Put the comment's text back in the log by hand.
    let conn = Connection::open(dir.path().join("pm.sqlite")).unwrap();
    conn.execute(
        "UPDATE ops SET payload = ?1 WHERE op_id = ?2",
        rusqlite::params![
            serde_json::json!({ "body": format!("see {SECRET}") }).to_string(),
            comment.op_id.to_string()
        ],
    )
    .unwrap();
    drop(conn);
    let report = store.doctor().unwrap();
    assert_eq!(report.unredacted, 1);
    assert!(!report.is_healthy());
    store.rebuild().unwrap();
    assert!(store.doctor().unwrap().is_healthy());
    let held = store
        .ops(t.id)
        .unwrap()
        .into_iter()
        .find(|o| o.op_id == comment.op_id)
        .unwrap();
    assert_eq!(held.payload, comment.redacted().unwrap().payload);

    // Redacting everything else, and the design doc, leaves nothing
    // behind.
    let rest: Vec<&Op> = t.ops.iter().collect();
    store.commit(&redact(t.id, 2_001, &rest)).unwrap();
    let doc_edits: Vec<&Op> = log
        .config
        .iter()
        .filter(|o| o.entity == log.design_doc && o.kind() == "body.edit")
        .collect();
    assert_eq!(
        store
            .commit(&redact(log.design_doc, 2_002, &doc_edits))
            .unwrap(),
        None
    );
    assert_eq!(store.project("pm").unwrap().unwrap().doc, "");
    assert_clean(&dir, &mut store);
}

/// A redact only erases ops of its own entity, ignores kinds without
/// text, and a malformed one is refused on ingest.
#[test]
fn a_redact_is_scoped_to_its_entity_and_checked() {
    let log = log_head();
    let a = secret_ticket();
    let b = secret_ticket_at(100);
    let (_dir, mut store) = replica(&log);
    let mut ops = log.config.clone();
    ops.extend(a.ops.iter().cloned());
    ops.extend(b.ops.iter().cloned());
    store
        .apply_pulled_page(&seqd(&ops), ops.len() as i64)
        .unwrap();

    // Naming b's comment from a's entity erases nothing.
    store.commit(&redact(a.id, 2_000, &[&b.ops[2]])).unwrap();
    assert!(store.comments(b.id).unwrap()[0].body.contains(SECRET));
    assert!(store.doctor().unwrap().is_healthy());

    let empty = redact(a.id, 2_001, &[]);
    assert!(matches!(
        store.commit(&empty),
        Err(StoreError::InvalidRedact(_))
    ));
}

/// A restore replays a backup in file order through `commit_any`, the
/// document redact included; and the redact backfill applies only
/// redacts, skipping those already held.
#[test]
fn restore_and_backfill_apply_redacts() {
    let log = log_head();
    let t = secret_ticket();
    let doc_edits: Vec<&Op> = log
        .config
        .iter()
        .filter(|o| o.entity == log.design_doc && o.kind() == "body.edit")
        .collect();
    let ticket_redact = redact(t.id, 2_000, &t.ops.iter().collect::<Vec<_>>());
    let doc_redact = redact(log.design_doc, 2_001, &doc_edits);

    // Restore: an empty replica, every op in order.
    let (dir, mut restored) = replica(&log);
    for op in log
        .config
        .iter()
        .chain(&t.ops)
        .chain([&ticket_redact, &doc_redact])
    {
        restored.commit_any(op).unwrap();
    }
    assert_eq!(restored.project("pm").unwrap().unwrap().doc, "");
    assert_clean(&dir, &mut restored);

    // Backfill: a replica that pulled past both redacts without them.
    let (dir, mut behind) = replica(&log);
    let mut ops = log.config.clone();
    ops.extend(t.ops.iter().cloned());
    behind
        .apply_pulled_page(&seqd(&ops), ops.len() as i64)
        .unwrap();
    assert!(behind.ticket(t.id).unwrap().unwrap().title.contains(SECRET));
    let n = ops.len() as i64;
    assert!(matches!(
        behind.apply_redact_backfill(&[(1, log.config[0].clone())]),
        Err(StoreError::NotARedact { .. })
    ));
    let pulled = behind
        .apply_redact_backfill(&[(n + 1, ticket_redact.clone()), (n + 2, doc_redact.clone())])
        .unwrap();
    assert_eq!(pulled.applied, 2);
    let again = behind
        .apply_redact_backfill(&[(n + 1, ticket_redact.clone())])
        .unwrap();
    assert_eq!((again.applied, again.skipped), (0, 1));
    assert_eq!(behind.cursor().unwrap(), n, "the cursor does not move");
    assert_eq!(behind.outbox_len().unwrap(), 0, "pulled, so never pushed");
    assert_clean(&dir, &mut behind);
}

/// A target parked in the sync quarantine (its ticket not pulled yet) has
/// its kept content redacted when the redact lands, and lands redacted.
#[test]
fn a_parked_target_is_redacted_in_the_quarantine() {
    let log = log_head();
    let t = secret_ticket();
    let comment = t.ops[2].clone();
    let r = redact(t.id, 2_000, &[&comment]);
    let (dir, mut store) = replica(&log);
    let mut ops = log.config.clone();
    ops.push(comment.clone());
    ops.push(r);
    let n = ops.len() as i64;
    let pulled = store.apply_pulled_page(&seqd(&ops), n).unwrap();
    assert_eq!(pulled.parked, 1);
    let text = all_text(&dir.path().join("pm.sqlite"));
    assert!(!text.contains(&format!("see {SECRET}")), "{text}");

    let create = t.ops[0].clone();
    store.apply_pulled_page(&[(n + 1, create)], n + 1).unwrap();
    assert_eq!(store.comments(t.id).unwrap()[0].body, REDACTED);
    assert!(store.quarantine().unwrap().is_empty());
    assert!(store.doctor().unwrap().is_healthy());
}
