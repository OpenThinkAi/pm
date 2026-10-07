//! The ready frontier (AGT-1343; projects/pm/README.md §CLI verbs
//! `pm ready`, `pm claim --ready`; research/consumers.md build-loop steps
//! 2–4 and Phase 0.5). One pure definition of "ready", over a snapshot of
//! every ticket and relation, so `pm ready`, `pm claim --ready` and
//! whatever else asks agree — thirteen loops used to recompute it each
//! slightly differently.
//!
//! A ticket is **ready** when all of these hold:
//! - it is live (neither tombstoned nor archived) and its state's category
//!   is `unstarted`;
//! - nobody is assigned to it (a claim would be refused otherwise);
//! - it has no `hold`;
//! - it carries no gate label ([`Rules::gate_labels`]: the workspace's,
//!   e.g. `manual`, plus any the caller excludes);
//! - it is not parked, its project is not parked ([`Rules::parked_projects`],
//!   AGT-1635), and any `not_before` date has arrived;
//! - it is not in a blocker cycle;
//! - every ticket that `blocks` it **resolves** ([`resolves_as_blocker`]):
//!   tombstoned, absent, in a `completed` state, or **archived** out of
//!   any state but a `canceled` one. Archiving is how the old loops went
//!   blind — a blocker swept into the archive stranded its dependents —
//!   so `archived_at` counts as done whatever the state says, *except*
//!   for a canceled ticket.
//!
//! A **canceled** blocker (a `canceled`-category state, archived or not)
//! keeps blocking (AGT-1572; Matt, 2026-10-06): retiring a ticket must not
//! silently unblock its dependents. Whether a dependent still makes sense
//! without it is a judgement call, so pm surfaces it instead of deciding:
//! the dependent is [`Reason::BlockedByCanceled`] (and whatever it blocks
//! [`Reason::TransitivelyBlocked`] with a [`Gate::Canceled`] root) until
//! someone removes the edge (`pm relate <id> --unblock <canceled>`, with a
//! comment saying why) or cancels the dependent too. `pm check` reports
//! the same edges as `blocked-by-canceled` findings.
//!
//! Every other candidate gets one [`Reason`], the first that applies in
//! the order above, so `--explain` can say what a human must do. A
//! ticket whose blocker chain leads to a held or gate-labelled ticket is
//! [`Reason::TransitivelyBlocked`] (consumers.md Phase 0.5: NEEDS-HUMAN
//! and `manual` tickets exclude everything they transitively block) —
//! no amount of building unblocks it.
//!
//! [`Frontier::waves`] is what the loops' TICKET_SUMMARY wants: wave 0
//! is the frontier, each later wave the tickets that become ready once
//! the earlier ones (and whatever is already in flight) land. Waves
//! ignore [`Rules::model`]: the graph is shared across models, so a
//! sonnet ticket can sit in wave 1 behind an opus one.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use serde::Serialize;
use ulid::Ulid;

use crate::check::blocker_cycles;
use crate::domain::Relation;
use crate::domain::{ActorId, Hold, RelationKind, StateCategory, Ticket, Workspace};

/// Labels naming the model a ticket is built on start with this
/// (`model:sonnet-5`, `model:fable-5`).
pub const MODEL_LABEL_PREFIX: &str = "model:";

/// The model a ticket with no `model:` label is built on: the loops'
/// standing default (pm-build rule 8, "a ticket with no `model:` label →
/// opus"). `--model opus-5` therefore returns unlabelled tickets too.
pub const DEFAULT_MODEL: &str = "opus-5";

/// Which tickets to classify. Blockers are always resolved through the
/// whole snapshot, whatever the scope.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Scope {
    All,
    Project(String),
    Ids(BTreeSet<Ulid>),
}

impl Scope {
    fn contains(&self, t: &Ticket) -> bool {
        match self {
            Scope::All => true,
            Scope::Project(p) => t.project.as_deref() == Some(p.as_str()),
            Scope::Ids(ids) => ids.contains(&t.id),
        }
    }
}

/// The knobs of one readiness query. `today` is a `YYYY-MM-DD` the caller
/// supplies (this crate never reads a clock).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Rules {
    /// Date gates (`not_before`, `parked`) later than this hold a ticket back.
    pub today: String,
    /// Labels that keep a ticket (and everything it transitively blocks)
    /// out of the frontier: `Workspace::gate_labels` plus any the caller
    /// adds (`pm ready --exclude-label`).
    pub gate_labels: BTreeSet<String>,
    /// Only tickets built on this model (`sonnet-5`, `model:sonnet-5`, or a
    /// name `Workspace::model_labels` maps a label to). A ticket with no
    /// `model:` label counts as [`DEFAULT_MODEL`].
    pub model: Option<String>,
    /// Projects whose status is `parked` (AGT-1635): their tickets are not
    /// live work, so none of them is ready. They still count as pending
    /// blockers, as a parked ticket does.
    pub parked_projects: BTreeSet<String>,
}

/// Why a candidate is not ready. Tickets are ULIDs; the CLI renders them.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "reason", rename_all = "kebab-case")]
pub enum Reason {
    /// Not in an `unstarted` state (started, or backlog).
    State { state: String },
    /// Unstarted but already assigned: a claim would be refused.
    Assigned { assignee: ActorId },
    /// Waiting on a human.
    Held { hold: Hold },
    /// Carries a gate label (`manual`, or one the caller excluded).
    Label { label: String },
    /// Parked until `until` (or forever).
    Parked { until: String },
    /// Its project's status is `parked` (AGT-1635).
    ProjectParked { project: String },
    /// Its `not_before` date has not arrived.
    NotBefore { date: String },
    /// In a blocker cycle with `tickets` (sorted, itself included).
    Cycle { tickets: Vec<Ulid> },
    /// A direct blocker is not done. `gate` says why the blocker itself
    /// is stuck when a human must act on it (held or gate-labelled).
    BlockedBy { blocker: Ulid, gate: Option<Gate> },
    /// A direct blocker is canceled (AGT-1572): it will never be built, so
    /// someone must decide whether this ticket still needs it — remove the
    /// edge, or cancel this ticket too. `state` is the blocker's state.
    BlockedByCanceled { blocker: Ulid, state: String },
    /// The blocker chain `via` → … → `root` ends at a ticket a human must
    /// act on (or a canceled one); building cannot unblock this ticket.
    TransitivelyBlocked { via: Ulid, root: Ulid, gate: Gate },
    /// Built on another model than [`Rules::model`] asks for. `labels`
    /// are its `model:` labels (empty means [`DEFAULT_MODEL`]).
    Model { labels: Vec<String> },
}

/// What stops a blocker from ever being built by a loop.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum Gate {
    Held {
        hold: Hold,
    },
    Label {
        label: String,
    },
    /// The blocker is in a `canceled`-category `state` (AGT-1572).
    Canceled {
        state: String,
    },
}

impl Reason {
    /// The `reason` string `--json` carries.
    pub fn kind(&self) -> &'static str {
        match self {
            Reason::State { .. } => "state",
            Reason::Assigned { .. } => "assigned",
            Reason::Held { .. } => "held",
            Reason::Label { .. } => "label",
            Reason::Parked { .. } => "parked",
            Reason::ProjectParked { .. } => "project-parked",
            Reason::NotBefore { .. } => "not-before",
            Reason::Cycle { .. } => "cycle",
            Reason::BlockedBy { .. } => "blocked-by",
            Reason::BlockedByCanceled { .. } => "blocked-by-canceled",
            Reason::TransitivelyBlocked { .. } => "transitively-blocked",
            Reason::Model { .. } => "model",
        }
    }
}

/// One candidate's verdict.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Verdict {
    Ready,
    Excluded(Reason),
}

/// The result of [`frontier`]. `verdicts` covers every live, not-done
/// ticket in scope, in the order `tickets` were given; `waves` is
/// scope-projected, wave 0 being the (model-agnostic) ready set.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Frontier {
    pub verdicts: Vec<(Ulid, Verdict)>,
    pub waves: Vec<Vec<Ulid>>,
}

impl Frontier {
    /// The ready tickets, in input order.
    pub fn ready(&self) -> Vec<Ulid> {
        self.verdicts
            .iter()
            .filter(|(_, v)| *v == Verdict::Ready)
            .map(|(id, _)| *id)
            .collect()
    }

    /// The excluded tickets with their first reason, in input order.
    pub fn excluded(&self) -> Vec<(Ulid, &Reason)> {
        self.verdicts
            .iter()
            .filter_map(|(id, v)| match v {
                Verdict::Ready => None,
                Verdict::Excluded(r) => Some((*id, r)),
            })
            .collect()
    }
}

/// Classifies every live, not-done ticket in `scope`. `tickets` must be
/// the whole snapshot — tombstoned and archived ones included, since that
/// is how a blocker is known to be done — and `relations` every relation
/// row (duplicates allowed). Output order follows `tickets`, so a caller
/// that passes them numbered-first gets `AGT-1` before `AGT-2`.
pub fn frontier(
    ws: &Workspace,
    tickets: &[Ticket],
    relations: &[Relation],
    scope: &Scope,
    rules: &Rules,
) -> Frontier {
    let graph = Graph::new(ws, tickets, relations);
    let today = rules.today.as_str();

    // Pass 1: everything but blockers and model, for every live pending
    // ticket in the workspace (waves need the whole graph, not the scope).
    let mut own: BTreeMap<Ulid, Option<Reason>> = BTreeMap::new();
    for t in graph.pending.iter().copied() {
        own.insert(t.id, own_reason(t, ws, rules, today));
    }

    // Blocker cycles among not-done tickets.
    let edges: Vec<(Ulid, Ulid)> = graph
        .blocks
        .iter()
        .filter(|(from, to)| own.contains_key(from) && own.contains_key(to))
        .copied()
        .collect();
    let mut cycle_of: BTreeMap<Ulid, Vec<Ulid>> = BTreeMap::new();
    for cycle in blocker_cycles(&edges) {
        for id in &cycle {
            cycle_of.insert(*id, cycle.clone());
        }
    }

    // Pass 2: the verdict before the model filter.
    let mut pre_model: BTreeMap<Ulid, Verdict> = BTreeMap::new();
    for t in graph.pending.iter().copied() {
        let verdict = if let Some(reason) = own[&t.id].clone() {
            Verdict::Excluded(reason)
        } else if let Some(tickets) = cycle_of.get(&t.id) {
            Verdict::Excluded(Reason::Cycle {
                tickets: tickets.clone(),
            })
        } else {
            match graph.blocked_reason(t.id, &own) {
                Some(reason) => Verdict::Excluded(reason),
                None => Verdict::Ready,
            }
        };
        pre_model.insert(t.id, verdict);
    }

    // Waves over the whole workspace: nodes are the tickets only blockers
    // hold back; a started, unheld, ungated ticket is in flight and counts
    // as resolving.
    let nodes: Vec<Ulid> = graph
        .pending
        .iter()
        .filter(|t| {
            matches!(
                pre_model[&t.id],
                Verdict::Ready | Verdict::Excluded(Reason::BlockedBy { gate: None, .. })
            )
        })
        .map(|t| t.id)
        .collect();
    let node_set: BTreeSet<Ulid> = nodes.iter().copied().collect();
    let in_flight: BTreeSet<Ulid> = graph
        .pending
        .iter()
        .filter(|t| {
            graph.category(t) == Some(StateCategory::Started) && graph.gate(t, rules).is_none()
        })
        .map(|t| t.id)
        .collect();
    let blockers: BTreeMap<Ulid, Vec<Ulid>> = nodes
        .iter()
        .map(|id| {
            let pending: Vec<Ulid> = graph
                .unresolved_blockers(*id)
                .into_iter()
                .filter(|b| !in_flight.contains(b))
                .collect();
            (*id, pending)
        })
        .collect();
    let all_waves = waves(&nodes, &blockers).waves;
    let waves: Vec<Vec<Ulid>> = all_waves
        .into_iter()
        .map(|wave| {
            wave.into_iter()
                .filter(|id| node_set.contains(id) && scope.contains(graph.by_id[id]))
                .collect::<Vec<Ulid>>()
        })
        .filter(|wave| !wave.is_empty())
        .collect();

    // Pass 3: scope and the model filter.
    let verdicts = graph
        .pending
        .iter()
        .filter(|t| scope.contains(t))
        .map(|t| {
            let verdict = match &pre_model[&t.id] {
                Verdict::Ready => match &rules.model {
                    Some(model) if !ws_model_matches(ws, t, model) => {
                        Verdict::Excluded(Reason::Model {
                            labels: model_labels(t),
                        })
                    }
                    _ => Verdict::Ready,
                },
                excluded => excluded.clone(),
            };
            (t.id, verdict)
        })
        .collect();
    Frontier { verdicts, waves }
}

/// The reasons that need nothing but the ticket itself, in precedence
/// order; `None` when only blockers (or the model) could hold it back.
fn own_reason(t: &Ticket, ws: &Workspace, rules: &Rules, today: &str) -> Option<Reason> {
    let category = ws.state(&t.state).map(|s| s.category);
    if category != Some(StateCategory::Unstarted) {
        return Some(Reason::State {
            state: t.state.clone(),
        });
    }
    if let Some(assignee) = &t.assignee {
        return Some(Reason::Assigned {
            assignee: assignee.clone(),
        });
    }
    if let Some(gate) = gate_of(t, rules) {
        return Some(match gate {
            Gate::Held { hold } => Reason::Held { hold },
            Gate::Label { label } => Reason::Label { label },
            Gate::Canceled { .. } => unreachable!("gate_of never yields a canceled gate"),
        });
    }
    if let Some(parked) = t.parked.as_ref().filter(|p| p.is_active(today)) {
        return Some(Reason::Parked {
            until: parked.until.clone(),
        });
    }
    if let Some(project) = t
        .project
        .as_ref()
        .filter(|p| rules.parked_projects.contains(*p))
    {
        return Some(Reason::ProjectParked {
            project: project.clone(),
        });
    }
    if let Some(nb) = t.not_before.as_ref().filter(|n| n.is_active(today)) {
        return Some(Reason::NotBefore {
            date: nb.date.clone(),
        });
    }
    None
}

/// Why a human must act on `t` before a loop can build it: a hold, else
/// the first gate label it carries.
fn gate_of(t: &Ticket, rules: &Rules) -> Option<Gate> {
    if let Some(hold) = &t.hold {
        return Some(Gate::Held { hold: hold.clone() });
    }
    t.labels
        .iter()
        .find(|l| rules.gate_labels.contains(*l))
        .map(|label| Gate::Label {
            label: label.clone(),
        })
}

/// The ticket's `model:` labels, sorted.
pub fn model_labels(t: &Ticket) -> Vec<String> {
    t.labels
        .iter()
        .filter(|l| l.starts_with(MODEL_LABEL_PREFIX))
        .cloned()
        .collect()
}

/// Whether `t` is built on `model`: `model` names one of its `model:`
/// labels (with or without the prefix), or a name the workspace maps such
/// a label to. An unlabelled ticket is built on [`DEFAULT_MODEL`].
pub fn ws_model_matches(ws: &Workspace, t: &Ticket, model: &str) -> bool {
    let wanted = model.trim();
    let wanted = wanted.strip_prefix(MODEL_LABEL_PREFIX).unwrap_or(wanted);
    let mut labels = model_labels(t);
    if labels.is_empty() {
        labels.push(format!("{MODEL_LABEL_PREFIX}{DEFAULT_MODEL}"));
    }
    labels.iter().any(|label| {
        label[MODEL_LABEL_PREFIX.len()..] == *wanted
            || ws
                .model_labels
                .get(label)
                .is_some_and(|name| name == wanted)
    })
}

/// Whether `t` is settled — no longer work anyone will do, so never a
/// ready candidate nor a `pm graph` node: tombstoned, archived, or in a
/// `completed` / `canceled` state.
pub fn settled(ws: &Workspace, t: &Ticket) -> bool {
    t.deleted
        || t.archived_at.is_some()
        || ws.state(&t.state).is_some_and(|s| {
            matches!(
                s.category,
                StateCategory::Completed | StateCategory::Canceled
            )
        })
}

/// Whether `t` is canceled: live or archived, but in a `canceled`-category
/// state (AGT-1572). A tombstoned ticket is gone, not canceled.
pub fn canceled(ws: &Workspace, t: &Ticket) -> bool {
    !t.deleted && ws.state(&t.state).map(|s| s.category) == Some(StateCategory::Canceled)
}

/// Whether `t`, as a blocker, no longer holds its dependents back:
/// tombstoned, in a `completed` state, or archived out of any state but a
/// canceled one (an archived blocker counts as done whatever its state —
/// AGT-1343 — unless it was canceled). A [`canceled`] blocker keeps
/// blocking until the edge is removed (AGT-1572). An absent blocker
/// resolves too; callers handle that before they have a `Ticket`.
pub fn resolves_as_blocker(ws: &Workspace, t: &Ticket) -> bool {
    if t.deleted {
        return true;
    }
    if canceled(ws, t) {
        return false;
    }
    t.archived_at.is_some()
        || ws.state(&t.state).map(|s| s.category) == Some(StateCategory::Completed)
}

/// Dependency waves of `nodes`: wave 0 is every node with no pending
/// blocker, wave n every node whose blockers all sit in earlier waves.
/// A blocker that is not itself a node never resolves, so its dependents
/// end up in `stuck` (in `nodes` order) — a cycle, or a blocker outside
/// the node set. Each wave keeps `nodes` order. Shared by `pm graph`.
pub fn waves(nodes: &[Ulid], blockers: &BTreeMap<Ulid, Vec<Ulid>>) -> Waves {
    let mut waves: Vec<Vec<Ulid>> = Vec::new();
    let mut resolved: BTreeSet<Ulid> = BTreeSet::new();
    let mut remaining: Vec<Ulid> = nodes.to_vec();
    loop {
        let (ready, unready): (Vec<Ulid>, Vec<Ulid>) = remaining.iter().partition(|id| {
            blockers
                .get(id)
                .is_none_or(|b| b.iter().all(|d| resolved.contains(d)))
        });
        if ready.is_empty() {
            return Waves {
                waves,
                stuck: unready,
            };
        }
        resolved.extend(ready.iter().copied());
        waves.push(ready);
        remaining = unready;
    }
}

/// [`waves`]'s result.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Waves {
    pub waves: Vec<Vec<Ulid>>,
    /// Nodes no wave ever reaches.
    pub stuck: Vec<Ulid>,
}

/// The snapshot indexed for the questions above.
struct Graph<'a> {
    ws: &'a Workspace,
    by_id: BTreeMap<Ulid, &'a Ticket>,
    /// Tickets not [`settled`], in input order.
    pending: Vec<&'a Ticket>,
    /// Position of each ticket in the input, to order blockers.
    order: BTreeMap<Ulid, usize>,
    /// Every `blocks` edge `(from, to)`, deduplicated.
    blocks: BTreeSet<(Ulid, Ulid)>,
    /// `to` → its blockers that do not resolve (pending or canceled), in
    /// input order.
    blocked_by: BTreeMap<Ulid, Vec<Ulid>>,
}

impl<'a> Graph<'a> {
    fn new(ws: &'a Workspace, tickets: &'a [Ticket], relations: &[Relation]) -> Self {
        let by_id: BTreeMap<Ulid, &Ticket> = tickets.iter().map(|t| (t.id, t)).collect();
        let order: BTreeMap<Ulid, usize> =
            tickets.iter().enumerate().map(|(i, t)| (t.id, i)).collect();
        let mut graph = Graph {
            ws,
            by_id,
            pending: Vec::new(),
            order,
            blocks: BTreeSet::new(),
            blocked_by: BTreeMap::new(),
        };
        graph.pending = tickets.iter().filter(|t| !settled(ws, t)).collect();
        graph.blocks = relations
            .iter()
            .filter(|r| r.kind == RelationKind::Blocks)
            .map(|r| (r.from, r.to))
            .collect();
        let mut blocked_by: BTreeMap<Ulid, Vec<Ulid>> = BTreeMap::new();
        for (from, to) in &graph.blocks {
            if !graph.resolves(*from) {
                blocked_by.entry(*to).or_default().push(*from);
            }
        }
        for list in blocked_by.values_mut() {
            list.sort_by_key(|id| graph.order[id]);
        }
        graph.blocked_by = blocked_by;
        graph
    }

    fn category(&self, t: &Ticket) -> Option<StateCategory> {
        self.ws.state(&t.state).map(|s| s.category)
    }

    /// Resolved as a blocker: absent, or [`resolves_as_blocker`].
    fn resolves(&self, id: Ulid) -> bool {
        self.by_id
            .get(&id)
            .is_none_or(|t| resolves_as_blocker(self.ws, t))
    }

    /// The [`Gate::Canceled`] a canceled blocker `id` puts on its
    /// dependents.
    fn canceled_gate(&self, id: Ulid) -> Option<Gate> {
        let t = self.by_id.get(&id)?;
        canceled(self.ws, t).then(|| Gate::Canceled {
            state: t.state.clone(),
        })
    }

    fn gate(&self, t: &Ticket, rules: &Rules) -> Option<Gate> {
        gate_of(t, rules)
    }

    fn unresolved_blockers(&self, id: Ulid) -> Vec<Ulid> {
        self.blocked_by.get(&id).cloned().unwrap_or_default()
    }

    /// The blocker reason for `id`, if any: a chain to a human-gated
    /// ticket wins over a plain pending blocker. `own` holds each pending
    /// ticket's own reason, so a gate is read off it rather than recomputed.
    fn blocked_reason(&self, id: Ulid, own: &BTreeMap<Ulid, Option<Reason>>) -> Option<Reason> {
        let direct = self.unresolved_blockers(id);
        let first = *direct.first()?;
        // A pending ticket's gate is read off its own reason; a blocker
        // that is not pending but still unresolved is a canceled one.
        let gate_on = |b: Ulid| -> Option<Gate> {
            match own.get(&b) {
                Some(Some(Reason::Held { hold })) => Some(Gate::Held { hold: hold.clone() }),
                Some(Some(Reason::Label { label })) => Some(Gate::Label {
                    label: label.clone(),
                }),
                Some(_) => None,
                None => self.canceled_gate(b),
            }
        };
        // Breadth-first from each direct blocker, so the nearest gated
        // ancestor (and the direct blocker it sits behind) is reported.
        let mut seen: BTreeSet<Ulid> = [id].into();
        let mut queue: VecDeque<(Ulid, Ulid)> = direct.iter().map(|d| (*d, *d)).collect();
        while let Some((node, via)) = queue.pop_front() {
            if !seen.insert(node) {
                continue;
            }
            if let Some(gate) = gate_on(node) {
                return Some(if node == via {
                    match gate {
                        Gate::Canceled { state } => Reason::BlockedByCanceled {
                            blocker: via,
                            state,
                        },
                        gate => Reason::BlockedBy {
                            blocker: via,
                            gate: Some(gate),
                        },
                    }
                } else {
                    Reason::TransitivelyBlocked {
                        via,
                        root: node,
                        gate,
                    }
                });
            }
            for next in self.unresolved_blockers(node) {
                queue.push_back((next, via));
            }
        }
        Some(Reason::BlockedBy {
            blocker: first,
            gate: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{NotBefore, Parked, Priority, State};
    use crate::hlc::Hlc;

    const TODAY: &str = "2026-09-28";

    fn ws() -> Workspace {
        let state = |name: &str, category, position| State {
            name: name.into(),
            category,
            position,
        };
        Workspace {
            id: Ulid::new(),
            prefix: "AGT".into(),
            states: vec![
                state("backlog", StateCategory::Backlog, 0),
                state("triage", StateCategory::Unstarted, 1),
                state("in-progress", StateCategory::Started, 2),
                state("done", StateCategory::Completed, 3),
                state("canceled", StateCategory::Canceled, 4),
            ],
            gate_labels: ["manual".to_string()].into(),
            model_labels: [("model:fable-5".to_string(), "fable".to_string())].into(),
            template_sections: vec![],
            stale_days: 30,
            docs_owned_by: Default::default(),
        }
    }

    fn rules() -> Rules {
        Rules {
            today: TODAY.into(),
            gate_labels: ["manual".to_string()].into(),
            model: None,
            parked_projects: Default::default(),
        }
    }

    fn ticket(n: u64) -> Ticket {
        Ticket {
            id: Ulid::from_parts(n, 0),
            number: Some(n),
            title: format!("t{n}"),
            state: "triage".into(),
            priority: Priority::Medium,
            project: Some("pm".into()),
            repo: None,
            assignee: None,
            description: String::new(),
            labels: Default::default(),
            created: Hlc::new(n, 0),
            updated: Hlc::new(n, 0),
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

    fn id(n: u64) -> Ulid {
        Ulid::from_parts(n, 0)
    }

    fn blocks(from: u64, to: u64) -> Relation {
        Relation {
            kind: RelationKind::Blocks,
            from: id(from),
            to: id(to),
        }
    }

    fn hold() -> Hold {
        Hold {
            reason: "needs Matt".into(),
            by: ActorId::new("matt"),
            at: Hlc::new(1, 0),
        }
    }

    fn verdict(f: &Frontier, n: u64) -> &Verdict {
        &f.verdicts.iter().find(|(i, _)| *i == id(n)).unwrap().1
    }

    fn ids(v: &[u64]) -> Vec<Ulid> {
        v.iter().map(|n| id(*n)).collect()
    }

    #[test]
    fn done_blockers_include_archived_tombstoned_and_absent() {
        let mut archived = ticket(1);
        archived.archived_at = Some(Hlc::new(5, 0));
        // Archived while still `triage`: done all the same.
        let mut gone = ticket(3);
        gone.deleted = true;
        let mut done = ticket(4);
        done.state = "done".into();
        let dependent = ticket(5);
        let rels = [
            blocks(1, 5),
            blocks(3, 5),
            blocks(4, 5),
            Relation {
                kind: RelationKind::Blocks,
                from: id(99),
                to: id(5),
            },
        ];
        let f = frontier(
            &ws(),
            &[archived, gone, done, dependent],
            &rels,
            &Scope::All,
            &rules(),
        );
        assert_eq!(f.ready(), ids(&[5]));
        assert_eq!(f.waves, vec![ids(&[5])]);
        // Done tickets are not candidates, so they get no verdict.
        assert_eq!(f.verdicts.len(), 1);
    }

    #[test]
    fn a_canceled_blocker_keeps_blocking_archived_or_not() {
        // AGT-1572: 1 canceled, 2 canceled then archived; 3 blocked by 1,
        // 4 blocked by 2, 5 blocked by 3 (transitively by 1), 6 blocked by
        // a pending ticket 7 and canceled 1 (the canceled edge wins: a
        // decision is owed whatever 7 does).
        let mut canceled = ticket(1);
        canceled.state = "canceled".into();
        let mut archived = ticket(2);
        archived.state = "canceled".into();
        archived.archived_at = Some(Hlc::new(5, 0));
        let rels = [
            blocks(1, 3),
            blocks(2, 4),
            blocks(3, 5),
            blocks(7, 6),
            blocks(1, 6),
        ];
        let f = frontier(
            &ws(),
            &[
                canceled,
                archived,
                ticket(3),
                ticket(4),
                ticket(5),
                ticket(6),
                ticket(7),
            ],
            &rels,
            &Scope::All,
            &rules(),
        );
        let on_canceled = |blocker| {
            Verdict::Excluded(Reason::BlockedByCanceled {
                blocker: id(blocker),
                state: "canceled".into(),
            })
        };
        assert_eq!(*verdict(&f, 3), on_canceled(1));
        assert_eq!(*verdict(&f, 4), on_canceled(2));
        assert_eq!(
            *verdict(&f, 5),
            Verdict::Excluded(Reason::TransitivelyBlocked {
                via: id(3),
                root: id(1),
                gate: Gate::Canceled {
                    state: "canceled".into()
                },
            })
        );
        assert_eq!(*verdict(&f, 6), on_canceled(1));
        assert_eq!(f.ready(), ids(&[7]));
        // Nothing behind a canceled blocker is ever in a wave.
        assert_eq!(f.waves, vec![ids(&[7])]);
        // Canceled tickets are not candidates themselves.
        assert_eq!(f.verdicts.len(), 5);
        assert_eq!(
            Reason::BlockedByCanceled {
                blocker: id(1),
                state: "canceled".into()
            }
            .kind(),
            "blocked-by-canceled"
        );
    }

    #[test]
    fn removing_the_edge_or_canceling_the_dependent_settles_it() {
        let mut canceled = ticket(1);
        canceled.state = "canceled".into();
        let mut also_canceled = ticket(2);
        also_canceled.state = "canceled".into();
        // 3 lost its edge to 1 (pm relate --unblock): ready. 2 is canceled
        // too: not a candidate, so nothing to report.
        let f = frontier(
            &ws(),
            &[canceled, also_canceled, ticket(3)],
            &[blocks(1, 2)],
            &Scope::All,
            &rules(),
        );
        assert_eq!(f.ready(), ids(&[3]));
        assert_eq!(f.verdicts.len(), 1);
    }

    #[test]
    fn blocker_resolution_helpers() {
        let w = ws();
        let mut t = ticket(1);
        assert!(!settled(&w, &t) && !resolves_as_blocker(&w, &t));
        t.archived_at = Some(Hlc::new(5, 0));
        assert!(settled(&w, &t) && resolves_as_blocker(&w, &t));
        t.state = "canceled".into();
        assert!(settled(&w, &t) && canceled(&w, &t) && !resolves_as_blocker(&w, &t));
        t.archived_at = None;
        assert!(settled(&w, &t) && !resolves_as_blocker(&w, &t));
        t.deleted = true;
        assert!(!canceled(&w, &t) && resolves_as_blocker(&w, &t));
        let mut done = ticket(2);
        done.state = "done".into();
        assert!(settled(&w, &done) && resolves_as_blocker(&w, &done));
    }

    #[test]
    fn each_own_reason_in_precedence_order() {
        let mut started = ticket(1);
        started.state = "in-progress".into();
        started.assignee = Some(ActorId::new("bob"));
        let mut backlog = ticket(2);
        backlog.state = "backlog".into();
        let mut assigned = ticket(3);
        assigned.assignee = Some(ActorId::new("bob"));
        assigned.hold = Some(hold());
        let mut held = ticket(4);
        held.hold = Some(hold());
        held.labels.insert("manual".into());
        let mut manual = ticket(5);
        manual.labels.insert("manual".into());
        manual.parked = Some(Parked {
            until: "forever".into(),
        });
        let mut parked = ticket(6);
        parked.parked = Some(Parked {
            until: TODAY.into(),
        });
        parked.not_before = Some(NotBefore {
            date: "2026-10-01".into(),
        });
        let mut later = ticket(7);
        later.not_before = Some(NotBefore {
            date: "2026-10-01".into(),
        });
        let mut park_over = ticket(8);
        park_over.parked = Some(Parked {
            until: "2026-09-27".into(),
        });
        let mut arrived = ticket(9);
        arrived.not_before = Some(NotBefore { date: TODAY.into() });
        let tickets = [
            started, backlog, assigned, held, manual, parked, later, park_over, arrived,
        ];
        let f = frontier(&ws(), &tickets, &[], &Scope::All, &rules());
        assert_eq!(
            verdict(&f, 1),
            &Verdict::Excluded(Reason::State {
                state: "in-progress".into()
            })
        );
        assert_eq!(
            verdict(&f, 2),
            &Verdict::Excluded(Reason::State {
                state: "backlog".into()
            })
        );
        assert_eq!(
            verdict(&f, 3),
            &Verdict::Excluded(Reason::Assigned {
                assignee: ActorId::new("bob")
            })
        );
        assert_eq!(
            verdict(&f, 4),
            &Verdict::Excluded(Reason::Held { hold: hold() })
        );
        assert_eq!(
            verdict(&f, 5),
            &Verdict::Excluded(Reason::Label {
                label: "manual".into()
            })
        );
        assert_eq!(
            verdict(&f, 6),
            &Verdict::Excluded(Reason::Parked {
                until: TODAY.into()
            })
        );
        assert_eq!(
            verdict(&f, 7),
            &Verdict::Excluded(Reason::NotBefore {
                date: "2026-10-01".into()
            })
        );
        assert_eq!(f.ready(), ids(&[8, 9]));
        assert_eq!(f.waves, vec![ids(&[8, 9])]);
    }

    #[test]
    fn exclude_labels_gate_like_manual_and_are_reported_first_alphabetically() {
        let mut t = ticket(1);
        t.labels.insert("qwen".into());
        t.labels.insert("live-session".into());
        let r = Rules {
            gate_labels: [
                "manual".to_string(),
                "qwen".to_string(),
                "live-session".to_string(),
            ]
            .into(),
            ..rules()
        };
        let f = frontier(&ws(), &[t], &[], &Scope::All, &r);
        assert_eq!(
            verdict(&f, 1),
            &Verdict::Excluded(Reason::Label {
                label: "live-session".into()
            })
        );
    }

    #[test]
    fn blocked_by_a_pending_ticket_names_the_first_blocker_in_input_order() {
        let (a, b, c) = (ticket(1), ticket(2), ticket(3));
        let rels = [blocks(2, 3), blocks(1, 3)];
        let f = frontier(&ws(), &[a, b, c], &rels, &Scope::All, &rules());
        assert_eq!(f.ready(), ids(&[1, 2]));
        assert_eq!(
            verdict(&f, 3),
            &Verdict::Excluded(Reason::BlockedBy {
                blocker: id(1),
                gate: None
            })
        );
        assert_eq!(f.waves, vec![ids(&[1, 2]), ids(&[3])]);
    }

    #[test]
    fn a_held_or_manual_ticket_excludes_everything_it_transitively_blocks() {
        // 1 (held) → 2 → 3;  4 (manual) → 5;  6 → 7 with 6 in flight
        let mut held = ticket(1);
        held.hold = Some(hold());
        let mut manual = ticket(4);
        manual.labels.insert("manual".into());
        let mut in_flight = ticket(6);
        in_flight.state = "in-progress".into();
        in_flight.assignee = Some(ActorId::new("claude:x"));
        let tickets = [
            held,
            ticket(2),
            ticket(3),
            manual,
            ticket(5),
            in_flight,
            ticket(7),
        ];
        let rels = [blocks(1, 2), blocks(2, 3), blocks(4, 5), blocks(6, 7)];
        let f = frontier(&ws(), &tickets, &rels, &Scope::All, &rules());
        assert_eq!(f.ready(), vec![]);
        assert_eq!(
            verdict(&f, 2),
            &Verdict::Excluded(Reason::BlockedBy {
                blocker: id(1),
                gate: Some(Gate::Held { hold: hold() })
            })
        );
        assert_eq!(
            verdict(&f, 3),
            &Verdict::Excluded(Reason::TransitivelyBlocked {
                via: id(2),
                root: id(1),
                gate: Gate::Held { hold: hold() }
            })
        );
        assert_eq!(
            verdict(&f, 5),
            &Verdict::Excluded(Reason::BlockedBy {
                blocker: id(4),
                gate: Some(Gate::Label {
                    label: "manual".into()
                })
            })
        );
        // 7 waits on in-flight 6: the graph will unblock it, so it is
        // wave 0 of what comes next, while 2, 3 and 5 are in no wave.
        assert_eq!(
            verdict(&f, 7),
            &Verdict::Excluded(Reason::BlockedBy {
                blocker: id(6),
                gate: None
            })
        );
        assert_eq!(f.waves, vec![ids(&[7])]);

        // Releasing the hold: 2 becomes ready, 3 follows in wave 1.
        let mut released = tickets.clone();
        released[0].hold = None;
        let f = frontier(&ws(), &released, &rels, &Scope::All, &rules());
        assert_eq!(f.ready(), ids(&[1]));
        assert_eq!(f.waves, vec![ids(&[1, 7]), ids(&[2]), ids(&[3])]);
    }

    #[test]
    fn a_held_in_flight_blocker_gates_its_dependents() {
        let mut stuck = ticket(1);
        stuck.state = "in-progress".into();
        stuck.hold = Some(hold());
        let f = frontier(
            &ws(),
            &[stuck, ticket(2)],
            &[blocks(1, 2)],
            &Scope::All,
            &rules(),
        );
        // The started ticket's own reason is its state; its hold still
        // gates what it blocks.
        assert_eq!(
            verdict(&f, 1),
            &Verdict::Excluded(Reason::State {
                state: "in-progress".into()
            })
        );
        assert_eq!(
            verdict(&f, 2),
            &Verdict::Excluded(Reason::BlockedBy {
                blocker: id(1),
                gate: None
            })
        );
        assert_eq!(f.waves, Vec::<Vec<Ulid>>::new(), "no wave reaches 2");
    }

    #[test]
    fn cycle_members_and_their_dependents_are_out() {
        let tickets = [ticket(1), ticket(2), ticket(3)];
        let rels = [blocks(1, 2), blocks(2, 1), blocks(2, 3)];
        let f = frontier(&ws(), &tickets, &rels, &Scope::All, &rules());
        assert_eq!(
            verdict(&f, 1),
            &Verdict::Excluded(Reason::Cycle {
                tickets: ids(&[1, 2])
            })
        );
        assert_eq!(
            verdict(&f, 3),
            &Verdict::Excluded(Reason::BlockedBy {
                blocker: id(2),
                gate: None
            })
        );
        assert_eq!(f.waves, Vec::<Vec<Ulid>>::new());
    }

    #[test]
    fn model_filter_matches_label_suffix_prefix_or_workspace_name_and_defaults_to_opus() {
        let plain = ticket(1);
        let mut sonnet = ticket(2);
        sonnet.labels.insert("model:sonnet-5".into());
        let mut fable = ticket(3);
        fable.labels.insert("model:fable-5".into());
        let tickets = [plain, sonnet, fable];
        let with = |model: &str| {
            frontier(
                &ws(),
                &tickets,
                &[],
                &Scope::All,
                &Rules {
                    model: Some(model.into()),
                    ..rules()
                },
            )
        };
        assert_eq!(with("sonnet-5").ready(), ids(&[2]));
        assert_eq!(with("model:sonnet-5").ready(), ids(&[2]));
        assert_eq!(
            with("fable").ready(),
            ids(&[3]),
            "workspace model_labels name"
        );
        assert_eq!(with("fable-5").ready(), ids(&[3]));
        assert_eq!(
            with(DEFAULT_MODEL).ready(),
            ids(&[1]),
            "no model label means the default model"
        );
        assert_eq!(with("opus-4").ready(), vec![]);
        let f = with("sonnet-5");
        assert_eq!(
            verdict(&f, 1),
            &Verdict::Excluded(Reason::Model { labels: vec![] })
        );
        assert_eq!(
            verdict(&f, 3),
            &Verdict::Excluded(Reason::Model {
                labels: vec!["model:fable-5".into()]
            })
        );
        // Waves are model-agnostic.
        assert_eq!(f.waves, vec![ids(&[1, 2, 3])]);
    }

    #[test]
    fn scope_narrows_verdicts_and_waves_but_blockers_resolve_workspace_wide() {
        let mut other = ticket(1);
        other.project = Some("other".into());
        let mut other_done = ticket(2);
        other_done.project = Some("other".into());
        other_done.state = "done".into();
        let tickets = [other, other_done, ticket(3), ticket(4), ticket(5)];
        let rels = [blocks(1, 3), blocks(2, 4), blocks(4, 5)];
        let f = frontier(
            &ws(),
            &tickets,
            &rels,
            &Scope::Project("pm".into()),
            &rules(),
        );
        assert_eq!(f.ready(), ids(&[4]));
        assert_eq!(
            verdict(&f, 3),
            &Verdict::Excluded(Reason::BlockedBy {
                blocker: id(1),
                gate: None
            })
        );
        assert!(f.verdicts.iter().all(|(i, _)| *i != id(1)));
        // 1 is wave 0 workspace-wide but out of scope, so wave 0 here is
        // [4]; 3 follows 1 in wave 1, alongside 5.
        assert_eq!(f.waves, vec![ids(&[4]), ids(&[3, 5])]);

        let f = frontier(
            &ws(),
            &tickets,
            &rels,
            &Scope::Ids([id(5)].into()),
            &rules(),
        );
        assert_eq!(f.verdicts.len(), 1);
        assert_eq!(f.waves, vec![ids(&[5])]);
    }

    /// AGT-1635: a parked project's tickets are never ready — after the
    /// ticket's own park, before `not_before` — and a ticket elsewhere
    /// blocked by one waits on it, as on a parked ticket.
    #[test]
    fn a_parked_projects_tickets_are_not_ready() {
        let mut in_parked = ticket(1);
        in_parked.project = Some("api-router".into());
        let mut also_parked = ticket(2);
        also_parked.project = Some("api-router".into());
        also_parked.parked = Some(Parked {
            until: "forever".into(),
        });
        let mut later = ticket(3);
        later.project = Some("api-router".into());
        later.not_before = Some(NotBefore {
            date: "2026-10-01".into(),
        });
        let elsewhere = ticket(4);
        let blocked = ticket(5);
        let rel = blocks(1, 5);
        let mut rules = rules();
        rules.parked_projects.insert("api-router".into());
        let tickets = [in_parked, also_parked, later, elsewhere, blocked];
        let f = frontier(
            &ws(),
            &tickets,
            std::slice::from_ref(&rel),
            &Scope::All,
            &rules,
        );
        let parked = Verdict::Excluded(Reason::ProjectParked {
            project: "api-router".into(),
        });
        assert_eq!(verdict(&f, 1), &parked);
        assert_eq!(
            verdict(&f, 2),
            &Verdict::Excluded(Reason::Parked {
                until: "forever".into()
            })
        );
        assert_eq!(verdict(&f, 3), &parked);
        assert_eq!(verdict(&f, 4), &Verdict::Ready);
        assert_eq!(
            verdict(&f, 5),
            &Verdict::Excluded(Reason::BlockedBy {
                blocker: id(1),
                gate: None
            })
        );
        // Scoped to the parked project itself, still nothing is ready.
        let scoped = frontier(
            &ws(),
            &tickets,
            &[rel],
            &Scope::Project("api-router".into()),
            &rules,
        );
        assert!(scoped.ready().is_empty());
        assert_eq!(f.ready(), ids(&[4]));
    }

    #[test]
    fn waves_reports_stuck_nodes() {
        let nodes = ids(&[1, 2, 3]);
        let blockers: BTreeMap<Ulid, Vec<Ulid>> = [(id(2), ids(&[1])), (id(3), ids(&[9]))].into();
        assert_eq!(
            waves(&nodes, &blockers),
            Waves {
                waves: vec![ids(&[1]), ids(&[2])],
                stuck: ids(&[3]),
            }
        );
    }

    #[test]
    fn reasons_serialize_with_their_kind() {
        for r in [
            Reason::State { state: "x".into() },
            Reason::Assigned {
                assignee: ActorId::new("b"),
            },
            Reason::Held { hold: hold() },
            Reason::Label { label: "l".into() },
            Reason::Parked { until: "u".into() },
            Reason::ProjectParked {
                project: "p".into(),
            },
            Reason::NotBefore { date: "d".into() },
            Reason::Cycle { tickets: vec![] },
            Reason::BlockedBy {
                blocker: id(1),
                gate: None,
            },
            Reason::TransitivelyBlocked {
                via: id(1),
                root: id(2),
                gate: Gate::Label { label: "l".into() },
            },
            Reason::Model { labels: vec![] },
        ] {
            assert_eq!(serde_json::to_value(&r).unwrap()["reason"], r.kind());
        }
    }
}
