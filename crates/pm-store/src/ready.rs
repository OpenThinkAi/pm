//! The ready frontier: which tickets an agent may pick up next
//! (README §CLI verbs `pm ready`, `pm claim --ready`; AGT-1341 AC4). One
//! definition, in the store, so every verb that asks "what is ready"
//! agrees.
//!
//! A ticket is **ready** when all of these hold:
//! - it is live (not tombstoned) and its state's category is `unstarted`;
//! - nobody is assigned to it;
//! - it carries no gate label (the workspace's `gate_labels`, e.g. `manual`);
//! - it has no `hold`, and any `parked {until}` / `not_before {date}` is
//!   today or earlier;
//! - every ticket that `blocks` it is resolved: tombstoned, or in a state
//!   whose category is `completed` or `canceled`.

use std::collections::BTreeSet;

use pm_core::Ticket;
use rusqlite::types::Value;

use crate::Store;
use crate::error::Result;

/// The inputs to [`Store::ready`]. `today` is an ISO-8601 date
/// (`YYYY-MM-DD`) the caller supplies, so the store never reads a clock.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ReadyQuery {
    /// Only tickets in this project.
    pub project: Option<String>,
    /// `YYYY-MM-DD`; date gates (`not_before`, `parked`) later than this
    /// hold the ticket back.
    pub today: String,
    /// Labels that keep a ticket out of the frontier (`Workspace::gate_labels`).
    pub gate_labels: BTreeSet<String>,
}

impl Store {
    /// Ready tickets in [`Store::tickets`] order: numbered ones by number
    /// (so `.first()` is "the lowest-numbered ready ticket"), then
    /// unnumbered ones in creation order.
    pub fn ready(&self, query: &ReadyQuery) -> Result<Vec<Ticket>> {
        let mut clauses = vec![
            "t.deleted = 0",
            "t.assignee IS NULL",
            "s.category = 'unstarted'",
            // Unresolved blocker: a live `from` ticket whose state is not
            // completed/canceled. Relation rows are stored once per
            // owning ticket, so the same edge may appear twice; EXISTS
            // does not care.
            "NOT EXISTS (
                SELECT 1 FROM relation r
                JOIN ticket b ON b.id = r.from_ticket
                JOIN state bs ON bs.name = b.state
                WHERE r.kind = 'blocks' AND r.to_ticket = t.id
                  AND b.deleted = 0
                  AND bs.category NOT IN ('completed', 'canceled'))",
            "NOT EXISTS (SELECT 1 FROM marker m WHERE m.ticket = t.id AND m.kind = 'hold')",
        ];
        let mut args: Vec<Value> = Vec::new();
        if let Some(project) = &query.project {
            clauses.push("t.project = ?");
            args.push(Value::from(project.clone()));
        }
        if !query.gate_labels.is_empty() {
            clauses.push(
                "NOT EXISTS (
                    SELECT 1 FROM ticket_label l
                    JOIN json_each(?) g ON g.value = l.label
                    WHERE l.ticket = t.id)",
            );
            let gate: Vec<&str> = query.gate_labels.iter().map(String::as_str).collect();
            args.push(Value::from(
                serde_json::to_string(&gate).expect("a list of strings serializes"),
            ));
        }
        let sql = format!(
            "JOIN state s ON s.name = t.state WHERE {}",
            clauses.join(" AND ")
        );
        let candidates = self.load_tickets(&sql, args)?;
        // Date gates are decoded markers on the ticket; comparing them
        // here keeps the ISO-date rule in one readable place.
        Ok(candidates
            .into_iter()
            .filter(|t| !date_gated(t, &query.today))
            .collect())
    }
}

/// Whether a `not_before` or `parked` marker holds the ticket past
/// `today`. Dates compare as `YYYY-MM-DD` strings; a longer value (a
/// datetime) is cut to its date part first.
fn date_gated(t: &Ticket, today: &str) -> bool {
    let day = |s: &str| s.trim().chars().take(10).collect::<String>();
    let not_before = t.not_before.as_ref().map(|n| day(&n.date));
    let parked = t.parked.as_ref().map(|p| day(&p.until));
    [not_before, parked]
        .into_iter()
        .flatten()
        .any(|gate| !gate.is_empty() && gate.as_str() > today)
}

#[cfg(test)]
mod tests {
    use pm_core::op::{FieldSet, HoldSet, LabelAdd, RelationAdd, StateTransition, TicketCreate};
    use pm_core::{
        ActorId, Hlc, Hold, NotBefore, Op, Parked, Payload, Priority, Project, ProjectStatus,
        Relation, RelationKind, State, StateCategory, Workspace,
    };
    use ulid::Ulid;

    use super::*;

    fn fresh() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(dir.path().join("pm.sqlite")).unwrap();
        let state = |name: &str, category, position| State {
            name: name.into(),
            category,
            position,
        };
        store
            .init_workspace(&Workspace {
                id: Ulid::new(),
                prefix: "AGT".into(),
                states: vec![
                    state("triage", StateCategory::Unstarted, 0),
                    state("in-progress", StateCategory::Started, 1),
                    state("done", StateCategory::Completed, 2),
                    state("canceled", StateCategory::Canceled, 3),
                ],
                gate_labels: ["manual".to_string()].into(),
                model_labels: Default::default(),
                template_sections: Vec::new(),
                stale_days: 30,
            })
            .unwrap();
        for id in ["p", "q"] {
            store
                .put_project(&Project {
                    id: id.into(),
                    title: id.into(),
                    status: ProjectStatus::InProgress,
                    parent: None,
                    repos: Default::default(),
                    doc: String::new(),
                    documents: Default::default(),
                })
                .unwrap();
        }
        (dir, store)
    }

    struct Seq(u64);

    impl Seq {
        fn op(&mut self, ticket: Ulid, payload: Payload) -> Op {
            self.0 += 1;
            Op::new(
                Ulid::new(),
                Hlc::new(self.0, 0),
                ActorId::new("matt"),
                ticket,
                payload,
            )
        }
    }

    fn create(store: &mut Store, seq: &mut Seq, project: Option<&str>) -> Ulid {
        let id = Ulid::new();
        store
            .commit(&seq.op(
                id,
                Payload::TicketCreate(TicketCreate {
                    title: "t".into(),
                    state: "triage".into(),
                    priority: Priority::Medium,
                    project: project.map(str::to_string),
                    repo: None,
                    source: None,
                    ext: Default::default(),
                }),
            ))
            .unwrap();
        store.allocate_number(id, &ActorId::new("matt")).unwrap();
        id
    }

    fn block(store: &mut Store, seq: &mut Seq, blocker: Ulid, blocked: Ulid) {
        store
            .commit(&seq.op(
                blocked,
                Payload::RelationAdd(RelationAdd {
                    relation: Relation {
                        kind: RelationKind::Blocks,
                        from: blocker,
                        to: blocked,
                    },
                }),
            ))
            .unwrap();
    }

    fn transition(store: &mut Store, seq: &mut Seq, ticket: Ulid, state: &str) {
        store
            .commit(&seq.op(
                ticket,
                Payload::StateTransition(StateTransition {
                    state: state.into(),
                }),
            ))
            .unwrap();
    }

    fn ready_ids(store: &Store, query: &ReadyQuery) -> Vec<Ulid> {
        store
            .ready(query)
            .unwrap()
            .into_iter()
            .map(|t| t.id)
            .collect()
    }

    fn query() -> ReadyQuery {
        ReadyQuery {
            project: None,
            today: "2026-09-28".into(),
            gate_labels: ["manual".to_string()].into(),
        }
    }

    #[test]
    fn blocked_tickets_become_ready_when_every_blocker_resolves() {
        let (_dir, mut store) = fresh();
        let mut seq = Seq(0);
        let a = create(&mut store, &mut seq, Some("p"));
        let b = create(&mut store, &mut seq, Some("p"));
        let c = create(&mut store, &mut seq, Some("p"));
        block(&mut store, &mut seq, a, c);
        block(&mut store, &mut seq, b, c);
        assert_eq!(ready_ids(&store, &query()), vec![a, b]);

        transition(&mut store, &mut seq, a, "done");
        assert_eq!(ready_ids(&store, &query()), vec![b], "one blocker left");

        // A canceled blocker no longer blocks; a tombstoned one neither.
        transition(&mut store, &mut seq, b, "canceled");
        assert_eq!(ready_ids(&store, &query()), vec![c]);

        let d = create(&mut store, &mut seq, Some("p"));
        block(&mut store, &mut seq, d, c);
        assert_eq!(ready_ids(&store, &query()), vec![d]);
        store.commit(&seq.op(d, Payload::Tombstone)).unwrap();
        assert_eq!(ready_ids(&store, &query()), vec![c]);
    }

    #[test]
    fn started_assigned_held_gated_and_deleted_tickets_are_not_ready() {
        let (_dir, mut store) = fresh();
        let mut seq = Seq(0);
        let started = create(&mut store, &mut seq, None);
        transition(&mut store, &mut seq, started, "in-progress");
        let assigned = create(&mut store, &mut seq, None);
        store
            .commit(&seq.op(
                assigned,
                Payload::FieldSet(FieldSet::Assignee(Some(ActorId::new("bob")))),
            ))
            .unwrap();
        let held = create(&mut store, &mut seq, None);
        store
            .commit(&seq.op(
                held,
                Payload::HoldSet(HoldSet {
                    hold: Hold {
                        reason: "waiting".into(),
                        by: ActorId::new("matt"),
                        at: Hlc::new(1, 0),
                    },
                }),
            ))
            .unwrap();
        let manual = create(&mut store, &mut seq, None);
        store
            .commit(&seq.op(
                manual,
                Payload::LabelAdd(LabelAdd {
                    label: "manual".into(),
                }),
            ))
            .unwrap();
        let deleted = create(&mut store, &mut seq, None);
        store.commit(&seq.op(deleted, Payload::Tombstone)).unwrap();
        let plain = create(&mut store, &mut seq, None);

        assert_eq!(ready_ids(&store, &query()), vec![plain]);

        // Without gate labels the `manual` ticket is ready again.
        let open = ReadyQuery {
            gate_labels: Default::default(),
            ..query()
        };
        assert_eq!(ready_ids(&store, &open), vec![manual, plain]);
    }

    #[test]
    fn date_gates_hold_until_the_day_arrives_and_project_filters() {
        let (_dir, mut store) = fresh();
        let mut seq = Seq(0);
        let later = create(&mut store, &mut seq, Some("p"));
        store
            .commit(&seq.op(
                later,
                Payload::FieldSet(FieldSet::NotBefore(Some(NotBefore {
                    date: "2026-10-01".into(),
                }))),
            ))
            .unwrap();
        let parked = create(&mut store, &mut seq, Some("p"));
        store
            .commit(&seq.op(
                parked,
                Payload::FieldSet(FieldSet::Parked(Some(Parked {
                    until: "2026-09-28T09:00:00Z".into(),
                }))),
            ))
            .unwrap();
        let other = create(&mut store, &mut seq, Some("q"));

        // Today: `later` is gated, `parked` (until today) is ready.
        assert_eq!(ready_ids(&store, &query()), vec![parked, other]);
        // October: both are ready.
        let october = ReadyQuery {
            today: "2026-10-01".into(),
            ..query()
        };
        assert_eq!(ready_ids(&store, &october), vec![later, parked, other]);
        // Project filter.
        let p = ReadyQuery {
            project: Some("p".into()),
            ..october
        };
        assert_eq!(ready_ids(&store, &p), vec![later, parked]);
        let none = ReadyQuery {
            project: Some("nope".into()),
            ..query()
        };
        assert!(ready_ids(&store, &none).is_empty());
    }
}
