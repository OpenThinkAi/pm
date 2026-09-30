//! The ready frontier as the store serves it (`pm claim --ready`,
//! `pm ready`; AGT-1341 AC4, AGT-1343). The definition lives in
//! [`pm_core::ready`] — the one implementation every verb shares — and
//! this module only feeds it the snapshot it needs: **every** ticket,
//! tombstoned and archived ones included, since an archived blocker
//! counts as done.

use std::collections::BTreeSet;

use pm_core::ready::{Frontier, Rules, Scope, frontier};
use pm_core::{Ticket, Workspace};

use crate::Store;
use crate::error::{Result, StoreError};

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
        let ws = self.workspace()?.ok_or(StoreError::NoWorkspace)?;
        let scope = match &query.project {
            Some(p) => Scope::Project(p.clone()),
            None => Scope::All,
        };
        let rules = Rules {
            today: query.today.clone(),
            gate_labels: query.gate_labels.clone(),
            model: None,
        };
        let (tickets, frontier) = self.frontier(&ws, &scope, &rules)?;
        let ready: BTreeSet<_> = frontier.ready().into_iter().collect();
        Ok(tickets
            .into_iter()
            .filter(|t| ready.contains(&t.id))
            .collect())
    }

    /// [`pm_core::ready::frontier`] over this database, with the snapshot
    /// it ran on (so a caller can render the ids it names without going
    /// back to the store).
    pub fn frontier(
        &self,
        ws: &Workspace,
        scope: &Scope,
        rules: &Rules,
    ) -> Result<(Vec<Ticket>, Frontier)> {
        let tickets = self.all_tickets()?;
        let relations = self.all_relations()?;
        let frontier = frontier(ws, &tickets, &relations, scope, rules);
        Ok((tickets, frontier))
    }
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
            .init_workspace(
                &Workspace {
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
                    docs_owned_by: Default::default(),
                },
                &ActorId::new("matt"),
            )
            .unwrap();
        for id in ["p", "q"] {
            store
                .put_project(
                    &Project {
                        id: id.into(),
                        title: id.into(),
                        status: ProjectStatus::InProgress,
                        parent: None,
                        repos: Default::default(),
                        doc: String::new(),
                        documents: Default::default(),
                    },
                    &ActorId::new("matt"),
                )
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
    fn an_archived_blocker_counts_as_done_whatever_its_state() {
        let (_dir, mut store) = fresh();
        let mut seq = Seq(0);
        let blocker = create(&mut store, &mut seq, Some("p"));
        let dependent = create(&mut store, &mut seq, Some("p"));
        block(&mut store, &mut seq, blocker, dependent);
        assert_eq!(ready_ids(&store, &query()), vec![blocker]);

        // Archived straight from `triage` (the sweep does not care about
        // state): the dependent is unblocked and the archived ticket is
        // not itself a candidate.
        store
            .commit(&seq.op(
                blocker,
                Payload::FieldSet(FieldSet::ArchivedAt(Some(Hlc::new(50, 0)))),
            ))
            .unwrap();
        assert_eq!(ready_ids(&store, &query()), vec![dependent]);
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
                    until: "2026-09-27".into(),
                }))),
            ))
            .unwrap();
        let other = create(&mut store, &mut seq, Some("q"));

        // Today: `later` is gated, `parked` (until yesterday) is ready.
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

    // AGT-1351 AC2 ("archived tickets ... count as done for `pm ready`") is
    // covered above by `an_archived_blocker_counts_as_done_whatever_its_state`
    // (AGT-1343) — `Store::ready` delegates entirely to
    // `pm_core::ready::frontier`, which treats `archived_at` as done
    // regardless of state (see that module's `Graph::done`), so there is
    // nothing left for this crate to pin down beyond what AGT-1343 already
    // does. `crates/pm/tests/archive.rs` adds the AC2 proof at the level
    // AGT-1351 owns: `pm archive` (the real command, not a re-derived op)
    // followed by `pm ready`.
}
