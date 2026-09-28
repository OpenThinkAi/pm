//! `pm check`: the invariants the schema cannot enforce (AGT-1342;
//! projects/pm/README.md §Data model "Hygiene rules become constraints",
//! vault-sweep §4). R2, R4, R5 and R6 are true by construction in the
//! store; what remains is checked here, purely, over a snapshot of the
//! tickets and their relations:
//!
//! - **R1** — a live ticket with no project and no R1/standalone waiver.
//! - **stale** — a live ticket in an `unstarted` state not updated for
//!   more than `stale_days` (0 disables), unless it is parked or date-gated.
//! - **held** — a live ticket with a hold: it is waiting on a human.
//! - **assigned-unstarted** (AGT-1379) — a live ticket in an
//!   `unstarted`-or-`backlog` state that still carries an `assignee`:
//!   `pm ready` excludes it and `pm claim` refuses it, so it is stranded
//!   until the assignee is cleared (`pm unclaim` now does this in place,
//!   without a state change, when it finds one already in this shape).
//! - **blocker cycle** — tickets that (transitively) block each other.
//! - **dangling relation** — a relation between a live ticket and one that
//!   is tombstoned or absent.
//!
//! "Live" means neither tombstoned nor archived.

use std::collections::btree_map::Entry;
use std::collections::{BTreeMap, BTreeSet};

use serde::Serialize;
use ulid::Ulid;

use crate::domain::{ActorId, Hold, Relation, RelationKind, StateCategory, Ticket, Workspace};
use crate::markers::{date_from_ms, waives_r1};

const DAY_MS: u64 = 86_400_000;

/// One problem `pm check` reports. Tickets are ULIDs; the CLI renders them.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "rule", rename_all = "kebab-case")]
pub enum Finding {
    /// Hygiene R1: no project and no R1/standalone waiver.
    #[serde(rename = "R1")]
    NoProject { ticket: Ulid },
    /// Unstarted and untouched for `days` (> the workspace's `stale_days`).
    Stale { ticket: Ulid, days: u64 },
    /// Held for a human.
    Held { ticket: Ulid, hold: Hold },
    /// Unstarted or backlog, but still carrying an `assignee` (AGT-1379):
    /// `pm ready` excludes it (assigned) and `pm claim` refuses it
    /// (already assigned), so it is stranded until someone clears the
    /// assignee — `pm move` now does this itself, but a ticket that was
    /// already stranded before that fix needs `pm check` to find it.
    AssignedUnstarted {
        ticket: Ulid,
        assignee: ActorId,
        state: String,
    },
    /// Every ticket in one strongly connected component of the `blocks`
    /// graph, sorted.
    BlockerCycle { tickets: Vec<Ulid> },
    /// `relation` touches `missing`, which is tombstoned or absent.
    DanglingRelation { relation: Relation, missing: Ulid },
}

impl Finding {
    /// Every ticket the finding names.
    pub fn tickets(&self) -> Vec<Ulid> {
        match self {
            Finding::NoProject { ticket }
            | Finding::Stale { ticket, .. }
            | Finding::Held { ticket, .. }
            | Finding::AssignedUnstarted { ticket, .. } => vec![*ticket],
            Finding::BlockerCycle { tickets } => tickets.clone(),
            Finding::DanglingRelation { relation, .. } => vec![relation.from, relation.to],
        }
    }

    /// The rule string `--json` carries under `rule`.
    pub fn rule(&self) -> &'static str {
        match self {
            Finding::NoProject { .. } => "R1",
            Finding::Stale { .. } => "stale",
            Finding::Held { .. } => "held",
            Finding::AssignedUnstarted { .. } => "assigned-unstarted",
            Finding::BlockerCycle { .. } => "blocker-cycle",
            Finding::DanglingRelation { .. } => "dangling-relation",
        }
    }
}

/// Runs every check. `tickets` must include tombstoned tickets (they are
/// how a dangling relation is recognized); `relations` is every relation
/// row, duplicates allowed. `now_ms` is the caller's clock reading. With
/// `project`, only findings naming at least one ticket in that project are
/// kept (so R1 findings, which have no project, never appear).
///
/// Order: R1, stale (each in `tickets` order), then held and
/// assigned-unstarted interleaved per ticket (each in `tickets` order),
/// then cycles, then dangling relations (in relation order: kind, from,
/// to).
pub fn check(
    ws: &Workspace,
    tickets: &[Ticket],
    relations: &[Relation],
    now_ms: u64,
    project: Option<&str>,
) -> Vec<Finding> {
    let by_id: BTreeMap<Ulid, &Ticket> = tickets.iter().map(|t| (t.id, t)).collect();
    let today = date_from_ms(now_ms);
    let mut findings = Vec::new();

    let live = tickets
        .iter()
        .filter(|t| !t.deleted && t.archived_at.is_none());
    for t in live.clone() {
        if t.project.is_none() && !waives_r1(&t.waivers) {
            findings.push(Finding::NoProject { ticket: t.id });
        }
    }
    if ws.stale_days > 0 {
        for t in live.clone() {
            let unstarted =
                ws.state(&t.state).map(|s| s.category) == Some(StateCategory::Unstarted);
            let deferred = t.parked.as_ref().is_some_and(|p| p.is_active(&today))
                || t.not_before.as_ref().is_some_and(|n| n.is_active(&today));
            let days = now_ms.saturating_sub(t.updated.wall_ms) / DAY_MS;
            if unstarted && !deferred && days > u64::from(ws.stale_days) {
                findings.push(Finding::Stale { ticket: t.id, days });
            }
        }
    }
    for t in live {
        if let Some(hold) = &t.hold {
            findings.push(Finding::Held {
                ticket: t.id,
                hold: hold.clone(),
            });
        }
        if let Some(assignee) = &t.assignee {
            let unstarted_like = ws
                .state(&t.state)
                .is_some_and(|s| s.category.is_unstarted_or_backlog());
            if unstarted_like {
                findings.push(Finding::AssignedUnstarted {
                    ticket: t.id,
                    assignee: assignee.clone(),
                    state: t.state.clone(),
                });
            }
        }
    }

    let relations: BTreeSet<Relation> = relations.iter().copied().collect();
    let present = |id: &Ulid| by_id.get(id).is_some_and(|t| !t.deleted);
    let blocks: Vec<(Ulid, Ulid)> = relations
        .iter()
        .filter(|r| r.kind == RelationKind::Blocks && present(&r.from) && present(&r.to))
        .map(|r| (r.from, r.to))
        .collect();
    findings.extend(
        blocker_cycles(&blocks)
            .into_iter()
            .map(|tickets| Finding::BlockerCycle { tickets }),
    );

    for r in &relations {
        let live_end = |id: &Ulid| {
            by_id
                .get(id)
                .is_some_and(|t| !t.deleted && t.archived_at.is_none())
        };
        for (end, other) in [(r.from, r.to), (r.to, r.from)] {
            if !present(&end) && live_end(&other) {
                findings.push(Finding::DanglingRelation {
                    relation: *r,
                    missing: end,
                });
            }
        }
    }

    if let Some(project) = project {
        findings.retain(|f| {
            f.tickets().iter().any(|id| {
                by_id
                    .get(id)
                    .is_some_and(|t| t.project.as_deref() == Some(project))
            })
        });
    }
    findings
}

/// The cycles of a directed graph given as edges: each strongly connected
/// component with more than one node, or one node with a self-edge, as a
/// sorted list; components in order of their smallest member. Iterative
/// Tarjan, so a long blocker chain cannot overflow the stack.
pub fn blocker_cycles(edges: &[(Ulid, Ulid)]) -> Vec<Vec<Ulid>> {
    let mut adj: BTreeMap<Ulid, BTreeSet<Ulid>> = BTreeMap::new();
    for &(a, b) in edges {
        adj.entry(a).or_default().insert(b);
        adj.entry(b).or_default();
    }
    let nodes: Vec<Ulid> = adj.keys().copied().collect();
    let succ: BTreeMap<Ulid, Vec<Ulid>> = adj
        .iter()
        .map(|(k, v)| (*k, v.iter().copied().collect()))
        .collect();

    let mut index: BTreeMap<Ulid, usize> = BTreeMap::new();
    let mut low: BTreeMap<Ulid, usize> = BTreeMap::new();
    let mut on_stack: BTreeSet<Ulid> = BTreeSet::new();
    let mut stack: Vec<Ulid> = Vec::new();
    let mut next = 0usize;
    let mut out: Vec<Vec<Ulid>> = Vec::new();

    for &root in &nodes {
        if index.contains_key(&root) {
            continue;
        }
        // (node, position of the next successor to visit)
        let mut work: Vec<(Ulid, usize)> = vec![(root, 0)];
        index.insert(root, next);
        low.insert(root, next);
        next += 1;
        stack.push(root);
        on_stack.insert(root);
        while let Some(&mut (v, ref mut i)) = work.last_mut() {
            let children = &succ[&v];
            if *i < children.len() {
                let w = children[*i];
                *i += 1;
                match index.entry(w) {
                    Entry::Vacant(slot) => {
                        slot.insert(next);
                        low.insert(w, next);
                        next += 1;
                        stack.push(w);
                        on_stack.insert(w);
                        work.push((w, 0));
                    }
                    Entry::Occupied(seen) if on_stack.contains(&w) => {
                        let lw = (*seen.get()).min(low[&v]);
                        low.insert(v, lw);
                    }
                    Entry::Occupied(_) => {}
                }
                continue;
            }
            work.pop();
            if let Some(&(parent, _)) = work.last() {
                let lp = low[&parent].min(low[&v]);
                low.insert(parent, lp);
            }
            if low[&v] == index[&v] {
                let mut component = Vec::new();
                loop {
                    let w = stack.pop().expect("v is on the stack");
                    on_stack.remove(&w);
                    component.push(w);
                    if w == v {
                        break;
                    }
                }
                let self_loop = succ[&v].contains(&v);
                if component.len() > 1 || self_loop {
                    component.sort();
                    out.push(component);
                }
            }
        }
    }
    out.sort();
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{ActorId, Priority, State, Waiver};
    use crate::hlc::Hlc;

    const NOW: u64 = 1_790_000_000_000; // 2026-09-21

    fn ws(stale_days: u32) -> Workspace {
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
            stale_days,
        }
    }

    fn ticket(n: u64, project: Option<&str>) -> Ticket {
        Ticket {
            id: Ulid::from_parts(n, 0),
            number: Some(n),
            title: format!("t{n}"),
            state: "triage".into(),
            priority: Priority::Medium,
            project: project.map(str::to_string),
            repo: None,
            assignee: None,
            description: String::new(),
            labels: Default::default(),
            created: Hlc::new(NOW, 0),
            updated: Hlc::new(NOW, 0),
            archived_at: None,
            deleted: false,
            linked_github: None,
            linked_pr: None,
            linear: None,
            source: None,
            hold: None,
            waivers: vec![],
            not_before: None,
            parked: None,
            ext: Default::default(),
        }
    }

    fn blocks(a: &Ticket, b: &Ticket) -> Relation {
        Relation {
            kind: RelationKind::Blocks,
            from: a.id,
            to: b.id,
        }
    }

    #[test]
    fn a_clean_workspace_has_no_findings() {
        let (a, b) = (ticket(1, Some("pm")), ticket(2, Some("pm")));
        let rels = [blocks(&a, &b)];
        assert_eq!(check(&ws(30), &[a, b], &rels, NOW, None), []);
    }

    #[test]
    fn r1_needs_a_project_or_a_waiver() {
        let bare = ticket(1, None);
        let mut waived = ticket(2, None);
        waived.waivers = vec![Waiver {
            rule: "R1".into(),
            reason: "standalone".into(),
        }];
        let mut other_waiver = ticket(3, None);
        other_waiver.waivers = vec![Waiver {
            rule: "R3".into(),
            reason: "x".into(),
        }];
        let mut archived = ticket(4, None);
        archived.archived_at = Some(Hlc::new(NOW, 1));
        let mut deleted = ticket(5, None);
        deleted.deleted = true;
        let tickets = [
            bare.clone(),
            waived,
            other_waiver.clone(),
            archived,
            deleted,
        ];
        assert_eq!(
            check(&ws(30), &tickets, &[], NOW, None),
            [
                Finding::NoProject { ticket: bare.id },
                Finding::NoProject {
                    ticket: other_waiver.id
                }
            ]
        );
        assert_eq!(check(&ws(30), &tickets, &[], NOW, Some("pm")), []);
    }

    #[test]
    fn stale_is_unstarted_and_untouched_past_stale_days() {
        let old = |n| {
            let mut t = ticket(n, Some("pm"));
            t.updated = Hlc::new(NOW - 31 * DAY_MS, 0);
            t
        };
        let stale = old(1);
        let mut started = old(2);
        started.state = "in-progress".into();
        let fresh = ticket(3, Some("pm"));
        let mut parked = old(4);
        parked.parked = Some(crate::Parked {
            until: "forever".into(),
        });
        let mut park_expired = old(5);
        park_expired.parked = Some(crate::Parked {
            until: "2026-01-01".into(),
        });
        let mut gated = old(6);
        gated.not_before = Some(crate::NotBefore {
            date: "2026-12-01".into(),
        });
        let tickets = [
            stale.clone(),
            started,
            fresh,
            parked,
            park_expired.clone(),
            gated,
        ];
        assert_eq!(
            check(&ws(30), &tickets, &[], NOW, None),
            [
                Finding::Stale {
                    ticket: stale.id,
                    days: 31
                },
                Finding::Stale {
                    ticket: park_expired.id,
                    days: 31
                }
            ]
        );
        assert_eq!(check(&ws(31), &tickets, &[], NOW, None), []);
        assert_eq!(check(&ws(0), &tickets, &[], NOW, None), [], "0 disables");
    }

    #[test]
    fn held_tickets_are_reported() {
        let mut t = ticket(1, Some("pm"));
        let hold = Hold {
            reason: "needs Matt".into(),
            by: ActorId::new("matt"),
            at: Hlc::new(NOW, 0),
        };
        t.hold = Some(hold.clone());
        assert_eq!(
            check(&ws(30), &[t.clone()], &[], NOW, None),
            [Finding::Held { ticket: t.id, hold }]
        );
    }

    /// AGT-1379: an assigned ticket is only a finding while it is
    /// unstarted-or-backlog — the ready/claim stranding this rule exists
    /// to catch never happens once a ticket has actually started.
    #[test]
    fn assigned_unstarted_covers_backlog_and_unstarted_but_not_started() {
        let state = |name: &str, category, position| State {
            name: name.into(),
            category,
            position,
        };
        let mut with_backlog = ws(30);
        with_backlog.states = vec![
            state("backlog", StateCategory::Backlog, 0),
            state("triage", StateCategory::Unstarted, 1),
            state("in-progress", StateCategory::Started, 2),
            state("done", StateCategory::Completed, 3),
        ];

        let mut backlog = ticket(1, Some("pm"));
        backlog.state = "backlog".into();
        backlog.assignee = Some(ActorId::new("matt"));
        let mut unstarted = ticket(2, Some("pm"));
        unstarted.assignee = Some(ActorId::new("matt"));
        let mut started = ticket(3, Some("pm"));
        started.state = "in-progress".into();
        started.assignee = Some(ActorId::new("matt"));
        let unassigned = ticket(4, Some("pm"));

        assert_eq!(
            check(
                &with_backlog,
                &[backlog.clone(), unstarted.clone(), started, unassigned],
                &[],
                NOW,
                None
            ),
            [
                Finding::AssignedUnstarted {
                    ticket: backlog.id,
                    assignee: ActorId::new("matt"),
                    state: "backlog".into(),
                },
                Finding::AssignedUnstarted {
                    ticket: unstarted.id,
                    assignee: ActorId::new("matt"),
                    state: "triage".into(),
                },
            ]
        );
    }

    #[test]
    fn blocker_cycles_are_found_once_each() {
        let t: Vec<Ticket> = (1..=6).map(|n| ticket(n, Some("pm"))).collect();
        let rels = [
            // 1 → 2 → 3 → 1, with 3 → 4 hanging off it
            blocks(&t[0], &t[1]),
            blocks(&t[1], &t[2]),
            blocks(&t[2], &t[0]),
            blocks(&t[2], &t[3]),
            // 5 ⇄ 6, recorded by both owners
            blocks(&t[4], &t[5]),
            blocks(&t[5], &t[4]),
            blocks(&t[5], &t[4]),
        ];
        assert_eq!(
            check(&ws(30), &t, &rels, NOW, None),
            [
                Finding::BlockerCycle {
                    tickets: vec![t[0].id, t[1].id, t[2].id]
                },
                Finding::BlockerCycle {
                    tickets: vec![t[4].id, t[5].id]
                },
            ]
        );
    }

    #[test]
    fn self_block_is_a_cycle_and_other_kinds_are_not() {
        let (a, b) = (ticket(1, Some("pm")), ticket(2, Some("pm")));
        let parent = |x: &Ticket, y: &Ticket| Relation {
            kind: RelationKind::Parent,
            from: x.id,
            to: y.id,
        };
        let rels = [blocks(&a, &a), parent(&a, &b), parent(&b, &a)];
        assert_eq!(
            check(&ws(30), &[a.clone(), b], &rels, NOW, None),
            [Finding::BlockerCycle {
                tickets: vec![a.id]
            }]
        );
    }

    #[test]
    fn long_chains_do_not_overflow() {
        let ids: Vec<Ulid> = (1..=100_000).map(|n| Ulid::from_parts(n, 0)).collect();
        let mut edges: Vec<(Ulid, Ulid)> = ids.windows(2).map(|w| (w[0], w[1])).collect();
        assert!(blocker_cycles(&edges).is_empty());
        edges.push((ids[ids.len() - 1], ids[0]));
        assert_eq!(blocker_cycles(&edges), [ids]);
    }

    #[test]
    fn dangling_relations_point_at_tombstoned_or_absent_tickets() {
        let live = ticket(1, Some("pm"));
        let mut gone = ticket(2, Some("pm"));
        gone.deleted = true;
        let absent = ticket(3, Some("pm"));
        let mut also_gone = ticket(4, Some("pm"));
        also_gone.deleted = true;
        let rels = [
            blocks(&gone, &live),
            blocks(&live, &absent),
            // both ends tombstoned: nothing live dangles
            blocks(&gone, &also_gone),
        ];
        let tickets = [live.clone(), gone.clone(), also_gone];
        assert_eq!(
            check(&ws(30), &tickets, &rels, NOW, None),
            [
                Finding::DanglingRelation {
                    relation: blocks(&live, &absent),
                    missing: absent.id
                },
                Finding::DanglingRelation {
                    relation: blocks(&gone, &live),
                    missing: gone.id
                },
            ]
        );
    }

    #[test]
    fn project_filter_keeps_findings_touching_the_project() {
        let mut a = ticket(1, Some("pm"));
        a.hold = Some(Hold {
            reason: "r".into(),
            by: ActorId::new("matt"),
            at: Hlc::new(NOW, 0),
        });
        let mut b = ticket(2, Some("other"));
        b.hold = a.hold.clone();
        let c = ticket(3, Some("other"));
        let rels = [blocks(&a, &c), blocks(&c, &a)];
        let found = check(&ws(30), &[a.clone(), b, c], &rels, NOW, Some("pm"));
        assert_eq!(found.len(), 2, "{found:?}");
        assert!(found.iter().all(|f| f.tickets().contains(&a.id)));
    }

    #[test]
    fn findings_serialize_with_their_rule() {
        let f = Finding::NoProject {
            ticket: Ulid::nil(),
        };
        assert_eq!(serde_json::to_value(&f).unwrap()["rule"], "R1");
        assert_eq!(f.rule(), "R1");
        let f = Finding::BlockerCycle { tickets: vec![] };
        assert_eq!(serde_json::to_value(&f).unwrap()["rule"], f.rule());
    }
}
