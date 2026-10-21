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
//! - **parked** (AGT-1575) — a live, not-done ticket parked `forever`
//!   with no `parked` waiver: `pm ready` never surfaces it, so without
//!   this nothing points at it. Its age is days since it was parked
//!   (refined by [`with_parked_since`]; days since its last update until
//!   then).
//! - **blocker cycle** — tickets that (transitively) block each other.
//! - **dangling relation** — a relation between a live ticket and one that
//!   is tombstoned or absent.
//! - **blocked-by-canceled** (AGT-1572) — a live, not-done ticket that a
//!   canceled ticket (a `canceled`-category state, archived or not)
//!   `blocks`, one finding per such edge. A canceled blocker keeps
//!   blocking (`pm ready` excludes the dependent as `blocked-by-canceled`)
//!   until someone decides: remove the edge if the cancellation really
//!   unblocks it, or cancel the dependent too.
//! - **deleted project** (AGT-1464) — a live ticket filed in a project
//!   that has since been deleted: a `project.delete` synced in while this
//!   replica still had tickets in it. The store keeps the name in the
//!   ticket's view and `NULL` on its row, so it is reported here (by
//!   [`with_deleted_projects`]) instead of as R1.
//! - **decision-uncited** / **decision-unknown** (AGT-1992) — only in a
//!   project with a `decisions` named document: a live ticket that is not
//!   done or canceled and cites no decision on a `Decisions:` line of its
//!   description, or cites an id ([`decision_ids`]) the document does not
//!   define. Folded in by [`with_decisions`], since the documents are not
//!   part of the ticket snapshot. Report-only, like every rule here;
//!   `pm waive <id> decision-uncited|decision-unknown "why"` exempts a
//!   ticket.
//!
//! "Live" means neither tombstoned nor archived.

use std::collections::btree_map::Entry;
use std::collections::{BTreeMap, BTreeSet};

use serde::Serialize;
use ulid::Ulid;

use crate::domain::{ActorId, Hold, Relation, RelationKind, StateCategory, Ticket, Workspace};
use crate::markers::{PARKED_FOREVER, date_from_ms, waives, waives_r1};

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
    /// Parked `forever`, not done, and not waived (AGT-1575): `days` since
    /// it was parked; `state` is its current state.
    Parked {
        ticket: Ulid,
        days: u64,
        state: String,
    },
    /// Every ticket in one strongly connected component of the `blocks`
    /// graph, sorted.
    BlockerCycle { tickets: Vec<Ulid> },
    /// `relation` touches `missing`, which is tombstoned or absent.
    DanglingRelation { relation: Relation, missing: Ulid },
    /// The ticket is filed in `project`, which was deleted (AGT-1464).
    DeletedProject { ticket: Ulid, project: String },
    /// `ticket` is live and not done, and `blocker` — in the
    /// `canceled`-category `state` — blocks it (AGT-1572).
    BlockedByCanceled {
        ticket: Ulid,
        blocker: Ulid,
        state: String,
    },
    /// `ticket`, in `project` (which has a `decisions` document), cites no
    /// decision on a `Decisions:` line (AGT-1992).
    DecisionUncited { ticket: Ulid, project: String },
    /// `ticket` cites `decision`, which `project`'s `decisions` document
    /// does not define (AGT-1992). One finding per unknown id.
    DecisionUnknown {
        ticket: Ulid,
        project: String,
        decision: String,
    },
}

impl Finding {
    /// Every ticket the finding names.
    pub fn tickets(&self) -> Vec<Ulid> {
        match self {
            Finding::NoProject { ticket }
            | Finding::Stale { ticket, .. }
            | Finding::Held { ticket, .. }
            | Finding::AssignedUnstarted { ticket, .. }
            | Finding::Parked { ticket, .. }
            | Finding::DeletedProject { ticket, .. }
            | Finding::DecisionUncited { ticket, .. }
            | Finding::DecisionUnknown { ticket, .. } => vec![*ticket],
            Finding::BlockerCycle { tickets } => tickets.clone(),
            Finding::DanglingRelation { relation, .. } => vec![relation.from, relation.to],
            Finding::BlockedByCanceled {
                ticket, blocker, ..
            } => vec![*ticket, *blocker],
        }
    }

    /// The rule string `--json` carries under `rule`.
    pub fn rule(&self) -> &'static str {
        match self {
            Finding::NoProject { .. } => "R1",
            Finding::Stale { .. } => "stale",
            Finding::Held { .. } => "held",
            Finding::AssignedUnstarted { .. } => "assigned-unstarted",
            Finding::Parked { .. } => "parked",
            Finding::BlockerCycle { .. } => "blocker-cycle",
            Finding::DanglingRelation { .. } => "dangling-relation",
            Finding::DeletedProject { .. } => "deleted-project",
            Finding::BlockedByCanceled { .. } => "blocked-by-canceled",
            Finding::DecisionUncited { .. } => DECISION_UNCITED,
            Finding::DecisionUnknown { .. } => DECISION_UNKNOWN,
        }
    }
}

/// Runs every check. `tickets` must include tombstoned tickets (they are
/// how a dangling relation is recognized); `relations` is every relation
/// row, duplicates allowed. `now_ms` is the caller's clock reading. With
/// `project`, only findings naming at least one ticket in that project are
/// kept (so R1 findings, which have no project, never appear).
///
/// Order: R1, stale (each in `tickets` order), then held,
/// assigned-unstarted and parked interleaved per ticket (each in `tickets`
/// order),
/// then cycles, then dangling relations, then blocked-by-canceled edges
/// (both in relation order: kind, from, to). The store's folds append
/// deleted-project findings ([`with_deleted_projects`]) and then decision
/// findings ([`with_decisions`]) after these.
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
        let forever = t.parked.as_ref().is_some_and(|p| p.until == PARKED_FOREVER);
        let done = ws.state(&t.state).is_some_and(|s| {
            matches!(
                s.category,
                StateCategory::Completed | StateCategory::Canceled
            )
        });
        if forever && !done && !waives(&t.waivers, "parked") {
            findings.push(Finding::Parked {
                ticket: t.id,
                days: now_ms.saturating_sub(t.updated.wall_ms) / DAY_MS,
                state: t.state.clone(),
            });
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

    for r in relations.iter().filter(|r| r.kind == RelationKind::Blocks) {
        let (Some(blocker), Some(dependent)) = (by_id.get(&r.from), by_id.get(&r.to)) else {
            continue;
        };
        if crate::ready::canceled(ws, blocker) && !crate::ready::settled(ws, dependent) {
            findings.push(Finding::BlockedByCanceled {
                ticket: dependent.id,
                blocker: blocker.id,
                state: blocker.state.clone(),
            });
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

/// Folds the store's deleted-project references (`(ticket, project)`, for
/// live tickets whose project was deleted — see the module docs) into
/// `findings` from [`check`]: each such ticket's R1 finding (its row has
/// no project) becomes a `deleted-project` finding, appended after the
/// rest. With a `project` filter nothing is added — a deleted project
/// cannot be the filter, and the ticket is no longer in any other.
pub fn with_deleted_projects(
    findings: &mut Vec<Finding>,
    deleted: &[(Ulid, String)],
    project: Option<&str>,
) {
    if project.is_some() || deleted.is_empty() {
        return;
    }
    let tickets: BTreeSet<Ulid> = deleted.iter().map(|(t, _)| *t).collect();
    findings.retain(|f| !matches!(f, Finding::NoProject { ticket } if tickets.contains(ticket)));
    findings.extend(
        deleted
            .iter()
            .map(|(ticket, project)| Finding::DeletedProject {
                ticket: *ticket,
                project: project.clone(),
            }),
    );
}

/// Replaces each `parked` finding's age in `findings` (from [`check`]:
/// days since the ticket's last update) with days since it was parked,
/// for every ticket `parked_ms` (ticket → wall-clock ms its `parked`
/// register was last written) knows (AGT-1575). Its last update can be
/// any later edit — a comment, a retitle — which would make a ticket
/// parked for months read as fresh.
pub fn with_parked_since(findings: &mut [Finding], parked_ms: &BTreeMap<Ulid, u64>, now_ms: u64) {
    for f in findings {
        if let Finding::Parked { ticket, days, .. } = f
            && let Some(ms) = parked_ms.get(ticket)
        {
            *days = now_ms.saturating_sub(*ms) / DAY_MS;
        }
    }
}

/// The rule names of the two decision findings (AGT-1992), which are also
/// what `pm waive` takes to exempt a ticket from each.
pub const DECISION_UNCITED: &str = "decision-uncited";
pub const DECISION_UNKNOWN: &str = "decision-unknown";

/// The decision ids a project's `decisions` document defines (AGT-1992):
/// the text of every ATX heading, at any level and outside fenced code
/// ([`crate::markdown::headings`]), that is exactly `D-` followed by one
/// or more ASCII digits. Ids compare as written: `D-3` and `D-03` differ.
pub fn decision_ids(doc: &str) -> BTreeSet<String> {
    let lines: Vec<&str> = doc.lines().collect();
    crate::markdown::headings(&lines)
        .into_iter()
        .map(|h| h.text)
        .filter(|text| is_decision_id(text))
        .collect()
}

/// The decision ids a ticket description cites (AGT-1992): every
/// `D-<digits>` token on each line that starts (after leading whitespace)
/// with `Decisions:`, in order, each once. Tokens are split on anything
/// but ASCII letters, digits and `-`, so `D-03, D-07.` cites two.
pub fn cited_decisions(description: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for line in description.lines() {
        let Some(rest) = line.trim_start().strip_prefix("Decisions:") else {
            continue;
        };
        for token in rest.split(|c: char| !(c.is_ascii_alphanumeric() || c == '-')) {
            if is_decision_id(token) && !out.iter().any(|d| d == token) {
                out.push(token.to_string());
            }
        }
    }
    out
}

fn is_decision_id(text: &str) -> bool {
    text.strip_prefix("D-")
        .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
}

/// Appends the decision findings (AGT-1992) to `findings` from [`check`].
/// `decisions` maps each project that has a `decisions` named document to
/// the ids it defines ([`decision_ids`]); a project absent from it gets
/// neither finding. For every live ticket (not tombstoned, not archived)
/// in such a project whose state is not `completed` or `canceled`: no
/// citation ([`cited_decisions`]) is `decision-uncited`, and each cited id
/// the document does not define is `decision-unknown`, unless the ticket
/// waives that rule. In `tickets` order, then citation order. With
/// `project`, only that project's tickets are considered.
pub fn with_decisions(
    findings: &mut Vec<Finding>,
    ws: &Workspace,
    tickets: &[Ticket],
    decisions: &BTreeMap<String, BTreeSet<String>>,
    project: Option<&str>,
) {
    for t in tickets {
        let Some(p) = t.project.as_deref() else {
            continue;
        };
        let Some(defined) = decisions.get(p) else {
            continue;
        };
        if project.is_some_and(|f| f != p) || crate::ready::settled(ws, t) {
            continue;
        }
        let cited = cited_decisions(&t.description);
        if cited.is_empty() {
            if !waives(&t.waivers, DECISION_UNCITED) {
                findings.push(Finding::DecisionUncited {
                    ticket: t.id,
                    project: p.to_string(),
                });
            }
            continue;
        }
        if waives(&t.waivers, DECISION_UNKNOWN) {
            continue;
        }
        for decision in cited.into_iter().filter(|d| !defined.contains(d)) {
            findings.push(Finding::DecisionUnknown {
                ticket: t.id,
                project: p.to_string(),
                decision,
            });
        }
    }
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
                state("canceled", StateCategory::Canceled, 3),
            ],
            gate_labels: Default::default(),
            model_labels: Default::default(),
            template_sections: vec![],
            stale_days,
            docs_owned_by: Default::default(),
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
        // Waived, so only staleness is under test (parked is AGT-1575's).
        parked.waivers = vec![Waiver {
            rule: "parked".into(),
            reason: "x".into(),
        }];
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

    /// AGT-1572: every `blocks` edge from a canceled ticket (archived or
    /// not) to a live, not-done one is a finding; a dependent that is
    /// done, canceled, archived or tombstoned is not, nor is a tombstoned
    /// blocker. The project filter keeps a finding naming either end.
    #[test]
    fn a_canceled_blocker_of_a_pending_ticket_is_reported_per_edge() {
        let mut canceled = ticket(1, Some("pm"));
        canceled.state = "canceled".into();
        let mut archived = ticket(2, Some("other"));
        archived.state = "canceled".into();
        archived.archived_at = Some(Hlc::new(NOW, 1));
        let dependent = ticket(3, Some("pm"));
        let mut started = ticket(4, Some("other"));
        started.state = "in-progress".into();
        let mut done = ticket(5, Some("pm"));
        done.state = "done".into();
        let mut also_canceled = ticket(6, Some("pm"));
        also_canceled.state = "canceled".into();
        let mut gone = ticket(7, Some("pm"));
        gone.state = "canceled".into();
        gone.deleted = true;
        let mut archived_dependent = ticket(8, Some("pm"));
        archived_dependent.archived_at = Some(Hlc::new(NOW, 1));
        let rels = [
            blocks(&canceled, &dependent),
            blocks(&archived, &started),
            blocks(&canceled, &done),
            blocks(&canceled, &also_canceled),
            blocks(&gone, &dependent),
            blocks(&canceled, &archived_dependent),
        ];
        let tickets = [
            canceled.clone(),
            archived.clone(),
            dependent.clone(),
            started.clone(),
            done,
            also_canceled,
            gone,
            archived_dependent,
        ];
        // The tombstoned blocker is a dangling relation, not this rule.
        let found: Vec<Finding> = check(&ws(0), &tickets, &rels, NOW, None)
            .into_iter()
            .filter(|f| f.rule() != "dangling-relation")
            .collect();
        let edge = |t: &Ticket, b: &Ticket| Finding::BlockedByCanceled {
            ticket: t.id,
            blocker: b.id,
            state: "canceled".into(),
        };
        assert_eq!(
            found,
            [edge(&dependent, &canceled), edge(&started, &archived)]
        );
        assert_eq!(found[0].rule(), "blocked-by-canceled");
        assert_eq!(found[0].tickets(), [dependent.id, canceled.id]);
        assert_eq!(
            check(&ws(0), &tickets, &rels, NOW, Some("other")),
            [edge(&started, &archived)]
        );
    }

    /// AGT-1575: parked `forever` and not done is a finding, aged from
    /// the last update until [`with_parked_since`] knows better; a dated
    /// park, a done ticket, an archived one or a `parked` waiver is not.
    #[test]
    fn parked_forever_is_reported_unless_done_or_waived() {
        let forever = || {
            Some(crate::Parked {
                until: PARKED_FOREVER.into(),
            })
        };
        let mut parked = ticket(1, Some("pm"));
        parked.parked = forever();
        parked.updated = Hlc::new(NOW - 40 * DAY_MS, 0);
        let mut started = ticket(2, Some("pm"));
        started.parked = forever();
        started.state = "in-progress".into();
        let mut done = ticket(3, Some("pm"));
        done.parked = forever();
        done.state = "done".into();
        let mut dated = ticket(4, Some("pm"));
        dated.parked = Some(crate::Parked {
            until: "2027-01-01".into(),
        });
        let mut waived = ticket(5, Some("pm"));
        waived.parked = forever();
        waived.waivers = vec![Waiver {
            rule: "Parked".into(),
            reason: "kept for reference".into(),
        }];
        let mut archived = ticket(6, Some("pm"));
        archived.parked = forever();
        archived.archived_at = Some(Hlc::new(NOW, 1));
        let tickets = [
            parked.clone(),
            started.clone(),
            done,
            dated,
            waived,
            archived,
        ];
        let mut found = check(&ws(0), &tickets, &[], NOW, None);
        assert_eq!(
            found,
            [
                Finding::Parked {
                    ticket: parked.id,
                    days: 40,
                    state: "triage".into()
                },
                Finding::Parked {
                    ticket: started.id,
                    days: 0,
                    state: "in-progress".into()
                },
            ]
        );
        assert_eq!(found[0].rule(), "parked");
        assert_eq!(serde_json::to_value(&found[0]).unwrap()["rule"], "parked");
        assert_eq!(found[0].tickets(), [parked.id]);

        let since = BTreeMap::from([(started.id, NOW - 90 * DAY_MS)]);
        with_parked_since(&mut found, &since, NOW);
        assert!(matches!(found[0], Finding::Parked { days: 40, .. }));
        assert!(matches!(found[1], Finding::Parked { days: 90, .. }));
        assert_eq!(check(&ws(0), &tickets, &[], NOW, Some("other")), []);
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

    /// AGT-1464: a ticket whose project was deleted is reported as such,
    /// not as R1; nothing is added under a project filter.
    #[test]
    fn deleted_project_references_replace_r1() {
        let (gone, bare) = (ticket(1, None), ticket(2, None));
        let mut found = check(&ws(30), &[gone.clone(), bare.clone()], &[], NOW, None);
        assert_eq!(found.len(), 2);
        let deleted = [(gone.id, "old".to_string())];
        with_deleted_projects(&mut found, &deleted, None);
        assert_eq!(
            found,
            [
                Finding::NoProject { ticket: bare.id },
                Finding::DeletedProject {
                    ticket: gone.id,
                    project: "old".into()
                },
            ]
        );
        assert_eq!(found[1].rule(), "deleted-project");
        assert_eq!(
            serde_json::to_value(&found[1]).unwrap()["rule"],
            "deleted-project"
        );
        assert_eq!(found[1].tickets(), [gone.id]);
        let mut filtered = Vec::new();
        with_deleted_projects(&mut filtered, &deleted, Some("pm"));
        assert!(filtered.is_empty());
    }

    /// AGT-1992: decision ids are exactly-`D-<digits>` ATX headings at any
    /// level, outside fenced code.
    #[test]
    fn decision_ids_are_d_digit_headings() {
        let doc = "# Decisions\n\n## D-01\ntext\n### D-2 ###\n#### D-03 extra\n\
                   ## d-04\n##D-05\n    ## D-06\n```\n## D-07\n```\nD-08\n## D-\n\
                   ###### D-10\n## D-1x\n";
        assert_eq!(
            decision_ids(doc),
            BTreeSet::from(["D-01".into(), "D-10".into(), "D-2".into()])
        );
        assert!(decision_ids("").is_empty());
    }

    /// AGT-1992: every `D-<digits>` token on a `Decisions:` line is a
    /// citation; other lines and other tokens are not.
    #[test]
    fn cited_decisions_come_from_decisions_lines() {
        let desc = "Cites D-99 in prose.\n\nDecisions: D-03, D-04,D-10.\n  Decisions: D-03 (again) and D-7\n\
                    decisions: D-11\nNot Decisions: D-12\nDecisions: none, XD-13, D-14a, D-\n";
        assert_eq!(cited_decisions(desc), ["D-03", "D-04", "D-10", "D-7"]);
        assert!(cited_decisions("Decisions:").is_empty());
        assert!(cited_decisions("").is_empty());
    }

    /// AGT-1992: in a project with a `decisions` doc, a live, not-done
    /// ticket citing nothing is `decision-uncited` and each unknown cited
    /// id is `decision-unknown`; done, canceled, archived and tombstoned
    /// tickets, projects without the doc, and waived rules are not.
    #[test]
    fn decision_findings_only_in_projects_with_a_decisions_doc() {
        let cite = |n, project, line: &str| {
            let mut t = ticket(n, Some(project));
            t.description = format!("Intro.\n\n{line}\n");
            t
        };
        let waive = |t: &mut Ticket, rule: &str| {
            t.waivers.push(Waiver {
                rule: rule.into(),
                reason: "why".into(),
            })
        };
        let uncited = cite(1, "pm", "No citation here.");
        let good = cite(2, "pm", "Decisions: D-01, D-02");
        let unknown = cite(3, "pm", "Decisions: D-01, D-09, D-08");
        let other_project = cite(4, "other", "nothing");
        let mut done = cite(5, "pm", "");
        done.state = "done".into();
        let mut canceled = cite(6, "pm", "Decisions: D-77");
        canceled.state = "canceled".into();
        let mut archived = cite(7, "pm", "");
        archived.archived_at = Some(Hlc::new(NOW, 1));
        let mut gone = cite(8, "pm", "");
        gone.deleted = true;
        let mut waived_uncited = cite(9, "pm", "");
        waive(&mut waived_uncited, "Decision-Uncited");
        let mut waived_unknown = cite(10, "pm", "Decisions: D-66");
        waive(&mut waived_unknown, DECISION_UNKNOWN);
        // Waiving decision-unknown does not excuse citing nothing.
        let mut wrong_waiver = cite(11, "pm", "");
        waive(&mut wrong_waiver, DECISION_UNKNOWN);
        let mut started = cite(12, "pm", "Decisions: D-5");
        started.state = "in-progress".into();
        let no_project = ticket(13, None);
        let tickets = [
            uncited.clone(),
            good,
            unknown.clone(),
            other_project,
            done,
            canceled,
            archived,
            gone,
            waived_uncited,
            waived_unknown,
            wrong_waiver.clone(),
            started.clone(),
            no_project,
        ];
        let decisions = BTreeMap::from([(
            "pm".to_string(),
            BTreeSet::from(["D-01".to_string(), "D-02".to_string()]),
        )]);
        let uncited_f = |t: &Ticket| Finding::DecisionUncited {
            ticket: t.id,
            project: "pm".into(),
        };
        let unknown_f = |t: &Ticket, d: &str| Finding::DecisionUnknown {
            ticket: t.id,
            project: "pm".into(),
            decision: d.into(),
        };

        let mut found = Vec::new();
        with_decisions(&mut found, &ws(0), &tickets, &decisions, None);
        assert_eq!(
            found,
            [
                uncited_f(&uncited),
                unknown_f(&unknown, "D-09"),
                unknown_f(&unknown, "D-08"),
                uncited_f(&wrong_waiver),
                unknown_f(&started, "D-5"),
            ]
        );
        assert_eq!(found[0].rule(), DECISION_UNCITED);
        assert_eq!(found[1].rule(), DECISION_UNKNOWN);
        let json = serde_json::to_value(&found[1]).unwrap();
        assert_eq!(json["rule"], "decision-unknown");
        assert_eq!(json["decision"], "D-09");
        assert_eq!(
            serde_json::to_value(&found[0]).unwrap()["rule"],
            "decision-uncited"
        );
        assert_eq!(found[1].tickets(), [unknown.id]);

        // The project filter, and a project without the doc.
        let mut filtered = Vec::new();
        with_decisions(&mut filtered, &ws(0), &tickets, &decisions, Some("other"));
        assert_eq!(filtered, []);
        with_decisions(&mut filtered, &ws(0), &tickets, &decisions, Some("pm"));
        assert_eq!(filtered, found);
        let mut none = Vec::new();
        with_decisions(&mut none, &ws(0), &tickets, &BTreeMap::new(), None);
        assert_eq!(none, []);

        // An empty `decisions` doc defines nothing: every citation is unknown.
        let empty = BTreeMap::from([("pm".to_string(), BTreeSet::new())]);
        let mut all = Vec::new();
        with_decisions(&mut all, &ws(0), &tickets[1..2], &empty, None);
        assert_eq!(
            all,
            [
                unknown_f(&tickets[1], "D-01"),
                unknown_f(&tickets[1], "D-02")
            ]
        );
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
