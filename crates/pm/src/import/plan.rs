//! From vault tickets to ops (AGT-1347 AC1, AC4, AC5): a new ticket
//! becomes its full history — create, number, labels, links, comments,
//! body, state, archive month, markers, blockers — each op dated from the
//! file; a ticket already in the store yields only the ops that turn what
//! the store holds into what the file says (a folder move is one
//! `state.transition`, a new comment one `comment.add`, an unchanged file
//! nothing at all).
//!
//! Ops are built as [`Intent`]s (payload + the wall time it should carry)
//! and stamped afterwards by [`stamp`], in wall-time order through one
//! [`Clock`], so every stamp is unique and monotonic while `wall_ms` stays
//! the file's own date wherever the log allows it. Within a ticket the
//! intended times never go backwards (a comment dated before `created`
//! is clamped forward), so each ticket's ops are HLC-ordered exactly as
//! they are listed here.

use std::collections::{BTreeMap, BTreeSet};

use pm_core::op::{
    BodyEdit, CommentAdd, FieldSet, HoldSet, LabelAdd, LabelRemove, RelationAdd, RelationRemove,
    StateTransition, TicketCreate,
};
use pm_core::{
    ActorId, Body, Clock, Hlc, Hold, Op, Parked, Payload, Relation, RelationKind, Ticket, Workspace,
};
use pm_store::Store;
use serde_json::Value;
use ulid::Ulid;

use super::vault::{Snapshot, VaultTicket};
use crate::exit::{CliError, Result};

/// The actor every import op records (AC1); a comment keeps its vault
/// author as the op's actor instead, so `Comment.author` survives (AC2).
pub const IMPORT_ACTOR: &str = "import";

/// `ext` key on a ticket whose vault id was already taken by an earlier
/// file (AC4: AGT-846): the id it duplicated.
pub const DUPLICATE_OF: &str = "duplicate_of_number";

/// When an op is committed relative to the others: relations last, so
/// both endpoints exist (R4) whatever order the vault lists tickets in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    Ticket,
    Relation,
}

/// An op before it is stamped.
#[derive(Clone, Debug)]
pub struct Intent {
    pub entity: Ulid,
    pub at_ms: u64,
    pub actor: ActorId,
    pub payload: Payload,
    pub phase: Phase,
}

/// What one vault file turned into.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize)]
pub struct TicketOutcome {
    pub created: usize,
    pub changed: usize,
    pub unchanged: usize,
    pub skipped: usize,
}

/// The ops for every ticket, plus what the import needs to finish.
#[derive(Debug, Default)]
pub struct Plan {
    pub intents: Vec<Intent>,
    pub outcome: TicketOutcome,
    /// Tickets to give a fresh number after the floor is raised (the
    /// second AGT-846), with the vault id each duplicates.
    pub fresh_numbers: Vec<(Ulid, String)>,
    /// The greatest vault number seen: the allocator floor.
    pub max_number: u64,
    pub anomalies: Vec<String>,
    /// Per ticket, per marker kind: what was migrated (for the report).
    pub migrated: Vec<String>,
    /// `pm ticket`-style lines for the report: `AGT-N <what changed>`.
    pub changes: Vec<String>,
}

/// The store's side of the diff for one existing ticket, loaded lazily:
/// most re-imported tickets are unchanged and need only the `Ticket`.
struct Existing<'a> {
    ticket: &'a Ticket,
}

/// Builds the plan. Reads the store; writes nothing.
pub fn build(store: &Store, ws: &Workspace, snapshot: &Snapshot) -> Result<Plan> {
    let actor = ActorId::new(IMPORT_ACTOR);
    let initial = crate::verbs::initial_state(ws)?;
    let known_projects: BTreeSet<&str> = snapshot.projects.iter().map(|p| p.id.as_str()).collect();

    let existing: Vec<Ticket> = store.all_tickets()?;
    let by_number: BTreeMap<u64, &Ticket> = existing
        .iter()
        .filter_map(|t| t.number.map(|n| (n, t)))
        .collect();
    let duplicates_of: BTreeMap<String, Vec<&Ticket>> =
        existing.iter().fold(BTreeMap::new(), |mut m, t| {
            if let Some(Value::String(of)) = t.ext.get(DUPLICATE_OF) {
                m.entry(of.clone()).or_default().push(t);
            }
            m
        });

    // Group files by vault id; the earliest-created file keeps the number.
    let mut groups: BTreeMap<u64, Vec<&VaultTicket>> = BTreeMap::new();
    for t in &snapshot.tickets {
        groups.entry(t.number).or_default().push(t);
    }
    for files in groups.values_mut() {
        files.sort_by(|a, b| (a.created_ms, &a.path).cmp(&(b.created_ms, &b.path)));
    }

    let mut plan = Plan {
        max_number: snapshot.tickets.iter().map(|t| t.number).max().unwrap_or(0),
        ..Plan::default()
    };

    // Resolve every file to a ticket ULID first, so blockers can point at
    // tickets created later in the same run.
    enum Target<'a> {
        New(Ulid),
        Existing(&'a Ticket),
        Skip,
    }
    let mut targets: Vec<(&VaultTicket, Target<'_>, bool)> =
        Vec::with_capacity(snapshot.tickets.len());
    let mut canonical: BTreeMap<u64, Ulid> = BTreeMap::new();
    for (number, files) in &groups {
        let id = format!("{}-{number}", ws.prefix);
        for (i, vt) in files.iter().enumerate() {
            let is_dup = i > 0;
            let shown = vt.path.display();
            let target = if is_dup {
                plan.anomalies.push(format!(
                    "{shown}: duplicate id {id} (also {}); imported with a fresh number and ext.{DUPLICATE_OF} = {id}",
                    files[0].path.display()
                ));
                let candidates = duplicates_of.get(&id).cloned().unwrap_or_default();
                let found = candidates
                    .iter()
                    .find(|t| t.title == vt.title)
                    .or_else(|| {
                        candidates
                            .iter()
                            .find(|t| t.created.wall_ms == vt.created_ms)
                    })
                    .copied();
                match found {
                    Some(t) if t.deleted => Target::Skip,
                    Some(t) => Target::Existing(t),
                    None => Target::New(Ulid::new()),
                }
            } else {
                match by_number.get(number) {
                    Some(t) if t.deleted => {
                        plan.anomalies
                            .push(format!("{shown}: {id} is tombstoned in pm; skipped"));
                        Target::Skip
                    }
                    Some(t) if t.ext.contains_key(DUPLICATE_OF) => {
                        plan.anomalies.push(format!(
                            "{shown}: number {id} was minted by pm for a duplicate ({}); skipped — renumber it before re-importing",
                            t.ext[DUPLICATE_OF]
                        ));
                        Target::Skip
                    }
                    Some(t) => Target::Existing(t),
                    None => Target::New(Ulid::new()),
                }
            };
            if !is_dup {
                match &target {
                    Target::New(u) => {
                        canonical.insert(*number, *u);
                    }
                    Target::Existing(t) => {
                        canonical.insert(*number, t.id);
                    }
                    Target::Skip => {}
                }
            }
            targets.push((vt, target, is_dup));
        }
    }
    let resolve = |n: u64| -> Option<Ulid> {
        canonical
            .get(&n)
            .copied()
            .or_else(|| by_number.get(&n).filter(|t| !t.deleted).map(|t| t.id))
    };

    for (vt, target, is_dup) in targets {
        let shown = format!("{}-{}", ws.prefix, vt.number);
        if let Some(p) = &vt.project
            && !known_projects.contains(p.as_str())
            && store.project(p)?.is_none()
        {
            return Err(CliError::error(format!(
                "{}: project '{p}' exists neither in the vault nor in pm",
                vt.path.display()
            )));
        }
        let mut blockers = Vec::with_capacity(vt.blocked_by.len());
        for b in &vt.blocked_by {
            match resolve(*b) {
                Some(u) => blockers.push(u),
                None => plan.anomalies.push(format!(
                    "{}: blocked-by {}-{b} names a ticket that exists nowhere; relation skipped",
                    vt.path.display(),
                    ws.prefix
                )),
            }
        }
        for m in &vt.migrated {
            plan.migrated
                .push(format!("{shown} {}: {}", m.kind.name(), m.text));
        }
        match target {
            Target::Skip => plan.outcome.skipped += 1,
            Target::New(id) => {
                let number = (!is_dup).then_some(vt.number);
                let dup_of = is_dup.then(|| format!("{}-{}", ws.prefix, vt.number));
                if is_dup {
                    plan.fresh_numbers
                        .push((id, dup_of.clone().unwrap_or_default()));
                }
                create_intents(
                    &mut plan.intents,
                    vt,
                    id,
                    number,
                    dup_of,
                    &initial,
                    &actor,
                    &blockers,
                );
                plan.outcome.created += 1;
            }
            Target::Existing(ticket) => {
                let before = plan.intents.len();
                let kinds = diff_intents(
                    store,
                    &mut plan.intents,
                    vt,
                    &Existing { ticket },
                    &actor,
                    &blockers,
                )?;
                if plan.intents.len() == before {
                    plan.outcome.unchanged += 1;
                } else {
                    plan.outcome.changed += 1;
                    plan.changes.push(format!("{shown}: {}", kinds.join(", ")));
                }
            }
        }
    }
    Ok(plan)
}

/// Per-ticket intent builder: keeps intended times non-decreasing.
struct Emitter<'a> {
    out: &'a mut Vec<Intent>,
    entity: Ulid,
    actor: &'a ActorId,
    floor: u64,
}

impl Emitter<'_> {
    fn at(&mut self, at_ms: u64, payload: Payload, phase: Phase) {
        self.floor = self.floor.max(at_ms);
        self.out.push(Intent {
            entity: self.entity,
            at_ms: self.floor,
            actor: self.actor.clone(),
            payload,
            phase,
        });
    }

    fn with_actor(&mut self, at_ms: u64, actor: ActorId, payload: Payload) {
        self.floor = self.floor.max(at_ms);
        self.out.push(Intent {
            entity: self.entity,
            at_ms: self.floor,
            actor,
            payload,
            phase: Phase::Ticket,
        });
    }
}

fn body_update(text: &str) -> Result<Vec<u8>> {
    Body::new()
        .diff_from_text(text)
        .map(|u| u.into_bytes())
        .map_err(|e| CliError::error(format!("building description: {e}")))
}

fn blocks(from: Ulid, to: Ulid) -> Relation {
    Relation {
        kind: RelationKind::Blocks,
        from,
        to,
    }
}

fn parked_of(vt: &VaultTicket) -> Option<Parked> {
    vt.parked.as_ref().map(|(until, _)| Parked {
        until: until.clone(),
    })
}

#[allow(clippy::too_many_arguments)]
fn create_intents(
    out: &mut Vec<Intent>,
    vt: &VaultTicket,
    id: Ulid,
    number: Option<u64>,
    duplicate_of: Option<String>,
    initial_state: &str,
    actor: &ActorId,
    blockers: &[Ulid],
) {
    let mut ext = vt.ext.clone();
    if let Some(of) = duplicate_of {
        ext.insert(DUPLICATE_OF.to_string(), Value::String(of));
    }
    let mut e = Emitter {
        out,
        entity: id,
        actor,
        floor: 0,
    };
    let c = vt.created_ms;
    e.at(
        c,
        Payload::TicketCreate(TicketCreate {
            title: vt.title.clone(),
            state: initial_state.to_string(),
            priority: vt.priority,
            project: vt.project.clone(),
            repo: vt.repo.clone(),
            source: vt.source.clone(),
            ext,
        }),
        Phase::Ticket,
    );
    if let Some(n) = number {
        e.at(c, Payload::FieldSet(FieldSet::Number(n)), Phase::Ticket);
    }
    for label in &vt.labels {
        e.at(
            c,
            Payload::LabelAdd(LabelAdd {
                label: label.clone(),
            }),
            Phase::Ticket,
        );
    }
    if let Some(g) = &vt.linked_github {
        e.at(
            c,
            Payload::FieldSet(FieldSet::LinkedGithub(Some(g.clone()))),
            Phase::Ticket,
        );
    }
    if let Some(p) = &vt.linked_pr {
        e.at(
            c,
            Payload::FieldSet(FieldSet::LinkedPr(Some(p.clone()))),
            Phase::Ticket,
        );
    }
    for comment in &vt.comments {
        if comment.body.is_empty() {
            continue;
        }
        e.with_actor(
            comment.date_ms,
            ActorId::new(&comment.author),
            Payload::CommentAdd(CommentAdd {
                body: comment.body.clone(),
            }),
        );
    }
    let u = vt.updated_ms;
    if !vt.description.is_empty()
        && let Ok(update) = body_update(&vt.description)
    {
        e.at(u, Payload::BodyEdit(BodyEdit { update }), Phase::Ticket);
    }
    if vt.state != initial_state {
        e.at(
            u,
            Payload::StateTransition(StateTransition {
                state: vt.state.clone(),
            }),
            Phase::Ticket,
        );
    }
    if let Some(month) = vt.archived_month_ms {
        e.at(
            u,
            Payload::FieldSet(FieldSet::ArchivedAt(Some(Hlc::new(month, 0)))),
            Phase::Ticket,
        );
    }
    if !vt.waivers.is_empty() {
        e.at(
            u,
            Payload::FieldSet(FieldSet::Waivers(vt.waivers.clone())),
            Phase::Ticket,
        );
    }
    if let Some(reason) = &vt.hold {
        e.at(u, hold_set(reason, actor), Phase::Ticket);
    }
    if let Some(parked) = parked_of(vt) {
        e.at(
            u,
            Payload::FieldSet(FieldSet::Parked(Some(parked))),
            Phase::Ticket,
        );
    }
    for blocker in blockers {
        e.at(
            c,
            Payload::RelationAdd(RelationAdd {
                relation: blocks(*blocker, id),
            }),
            Phase::Relation,
        );
    }
}

/// `hold.at` is patched to the op's own HLC by [`stamp`].
fn hold_set(reason: &str, actor: &ActorId) -> Payload {
    Payload::HoldSet(HoldSet {
        hold: Hold {
            reason: reason.to_string(),
            by: actor.clone(),
            at: Hlc::ZERO,
        },
    })
}

/// The ops that make `existing` read like `vt`; returns the kinds of
/// change, for the report.
fn diff_intents(
    store: &Store,
    out: &mut Vec<Intent>,
    vt: &VaultTicket,
    existing: &Existing<'_>,
    actor: &ActorId,
    blockers: &[Ulid],
) -> Result<Vec<&'static str>> {
    let t = existing.ticket;
    let mut kinds: Vec<&'static str> = Vec::new();
    let mut e = Emitter {
        out,
        entity: t.id,
        actor,
        floor: 0,
    };
    let u = vt.updated_ms;
    let mut field = |changed: bool, name: &'static str, f: FieldSet, e: &mut Emitter<'_>| {
        if changed {
            kinds.push(name);
            e.at(u, Payload::FieldSet(f), Phase::Ticket);
        }
    };
    field(
        t.title != vt.title,
        "title",
        FieldSet::Title(vt.title.clone()),
        &mut e,
    );
    field(
        t.priority != vt.priority,
        "priority",
        FieldSet::Priority(vt.priority),
        &mut e,
    );
    field(
        t.project != vt.project,
        "project",
        FieldSet::Project(vt.project.clone()),
        &mut e,
    );
    field(
        t.repo != vt.repo,
        "repo",
        FieldSet::Repo(vt.repo.clone()),
        &mut e,
    );
    field(
        t.linked_github != vt.linked_github,
        "linked-github",
        FieldSet::LinkedGithub(vt.linked_github.clone()),
        &mut e,
    );
    field(
        t.linked_pr != vt.linked_pr,
        "linked-pr",
        FieldSet::LinkedPr(vt.linked_pr.clone()),
        &mut e,
    );
    field(
        t.source != vt.source,
        "source",
        FieldSet::Source(vt.source.clone()),
        &mut e,
    );
    // `ext` keys are set from the file, never removed: pm's own keys
    // (`branch`, `merged_sha`, `duplicate_of_number`) have no vault
    // counterpart and must survive a re-import.
    for (key, value) in &vt.ext {
        field(
            t.ext.get(key) != Some(value),
            "ext",
            FieldSet::Ext {
                key: key.clone(),
                value: Some(value.clone()),
            },
            &mut e,
        );
    }
    field(
        t.waivers != vt.waivers,
        "waivers",
        FieldSet::Waivers(vt.waivers.clone()),
        &mut e,
    );
    let parked = parked_of(vt);
    field(
        t.parked != parked,
        "parked",
        FieldSet::Parked(parked),
        &mut e,
    );
    let archived = vt.archived_month_ms.map(|m| Hlc::new(m, 0));
    field(
        t.archived_at != archived,
        "archived_at",
        FieldSet::ArchivedAt(archived),
        &mut e,
    );

    // Labels mirror the file; a remove cites the add-tags this replica
    // observes (OR-set, add-wins), read from the view only when needed.
    let view = if t.labels.iter().any(|l| !vt.labels.contains(l)) || t.description != vt.description
    {
        store.ticket_view(t.id)?
    } else {
        None
    };
    for label in vt.labels.difference(&t.labels) {
        kinds.push("label.add");
        e.at(
            u,
            Payload::LabelAdd(LabelAdd {
                label: label.clone(),
            }),
            Phase::Ticket,
        );
    }
    for label in t.labels.difference(&vt.labels) {
        let observed = view
            .as_ref()
            .map(|v| v.labels.observed(label))
            .unwrap_or_default();
        kinds.push("label.remove");
        e.at(
            u,
            Payload::LabelRemove(LabelRemove {
                label: label.clone(),
                observed,
            }),
            Phase::Ticket,
        );
    }

    // Comments: a file entry with no counterpart (same author and text)
    // in the store is new. Matching is one-to-one, so repeated identical
    // entries still all import.
    let mut have: Vec<(String, String)> = store
        .comments(t.id)?
        .into_iter()
        .map(|c| (c.author.to_string(), c.body))
        .collect();
    for c in &vt.comments {
        if c.body.is_empty() {
            continue;
        }
        if let Some(pos) = have
            .iter()
            .position(|(a, b)| *a == c.author && *b == c.body)
        {
            have.swap_remove(pos);
            continue;
        }
        kinds.push("comment");
        e.with_actor(
            c.date_ms,
            ActorId::new(&c.author),
            Payload::CommentAdd(CommentAdd {
                body: c.body.clone(),
            }),
        );
    }

    if t.description != vt.description {
        kinds.push("body");
        let err = |e: pm_core::BodyError| CliError::error(format!("editing description: {e}"));
        let mut body = Body::with_peer(crate::edit::session_peer(Ulid::new())).map_err(err)?;
        if let Some(v) = &view {
            body.apply(&v.body.snapshot().map_err(err)?).map_err(err)?;
        }
        let update = body.diff_from_text(&vt.description).map_err(err)?;
        e.at(
            u,
            Payload::BodyEdit(BodyEdit {
                update: update.into_bytes(),
            }),
            Phase::Ticket,
        );
    }

    if t.state != vt.state {
        kinds.push("state");
        e.at(
            u,
            Payload::StateTransition(StateTransition {
                state: vt.state.clone(),
            }),
            Phase::Ticket,
        );
    }

    // A hold set by an earlier import follows the file; one a human set
    // with `pm hold` is theirs to clear.
    match (&vt.hold, &t.hold) {
        (Some(reason), None) => {
            kinds.push("hold");
            e.at(u, hold_set(reason, actor), Phase::Ticket);
        }
        (Some(reason), Some(h)) if h.by.as_str() == IMPORT_ACTOR && h.reason != *reason => {
            kinds.push("hold");
            e.at(u, hold_set(reason, actor), Phase::Ticket);
        }
        (None, Some(h)) if h.by.as_str() == IMPORT_ACTOR => {
            kinds.push("hold.clear");
            e.at(u, Payload::HoldClear, Phase::Ticket);
        }
        _ => {}
    }

    // Blockers mirror `blocked-by`.
    let current: BTreeSet<Ulid> = store
        .relations(t.id)?
        .into_iter()
        .filter(|r| r.kind == RelationKind::Blocks && r.to == t.id)
        .map(|r| r.from)
        .collect();
    let wanted: BTreeSet<Ulid> = blockers.iter().copied().collect();
    if current != wanted {
        let rel_view = match view {
            Some(v) => Some(v),
            None if current.difference(&wanted).next().is_some() => store.ticket_view(t.id)?,
            None => None,
        };
        for from in wanted.difference(&current) {
            kinds.push("blocked-by");
            e.at(
                vt.created_ms,
                Payload::RelationAdd(RelationAdd {
                    relation: blocks(*from, t.id),
                }),
                Phase::Relation,
            );
        }
        for from in current.difference(&wanted) {
            let relation = blocks(*from, t.id);
            let observed = rel_view
                .as_ref()
                .map(|v| v.relations.observed(&relation))
                .unwrap_or_default();
            kinds.push("blocked-by");
            e.at(
                u,
                Payload::RelationRemove(RelationRemove { relation, observed }),
                Phase::Relation,
            );
        }
    }
    kinds.dedup();
    Ok(kinds)
}

/// Stamps every intent through `clock`, in intended-time order (stable,
/// so a ticket's ops keep their order), and returns the ops in that
/// order. `clock` is seeded from the log's newest HLC: on a first import
/// every stamp carries the file's own date; on a re-import an intent
/// older than the log's newest op is stamped just after it instead, so
/// LWW fields take the file's value (README §Conflict semantics).
pub fn stamp(intents: Vec<Intent>, clock: &mut Clock) -> Vec<(Op, Phase)> {
    let mut indexed: Vec<(usize, Intent)> = intents.into_iter().enumerate().collect();
    indexed.sort_by_key(|(i, intent)| (intent.at_ms, *i));
    indexed
        .into_iter()
        .map(|(_, intent)| {
            let hlc = clock.send(intent.at_ms);
            let mut payload = intent.payload;
            if let Payload::HoldSet(set) = &mut payload {
                set.hold.at = hlc;
            }
            (
                Op::new(Ulid::new(), hlc, intent.actor, intent.entity, payload),
                intent.phase,
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn intent(entity: Ulid, at_ms: u64) -> Intent {
        Intent {
            entity,
            at_ms,
            actor: ActorId::new(IMPORT_ACTOR),
            payload: Payload::HoldClear,
            phase: Phase::Ticket,
        }
    }

    #[test]
    fn stamps_are_unique_monotonic_and_keep_file_dates() {
        let (a, b) = (Ulid::new(), Ulid::new());
        let intents = vec![
            intent(a, 500),
            intent(a, 500),
            intent(b, 100),
            intent(a, 700),
            intent(b, 100),
        ];
        let ops = stamp(intents, &mut Clock::new());
        let stamps: Vec<Hlc> = ops.iter().map(|(op, _)| op.hlc).collect();
        assert_eq!(
            stamps,
            [
                Hlc::new(100, 0),
                Hlc::new(100, 1),
                Hlc::new(500, 0),
                Hlc::new(500, 1),
                Hlc::new(700, 0)
            ]
        );
        assert_eq!(ops[0].0.entity, b);
        assert_eq!(ops[2].0.entity, a);

        // Seeded past the intents: stamped after the log's newest op.
        let ops = stamp(
            vec![intent(a, 500)],
            &mut Clock::from_latest(Hlc::new(900, 3)),
        );
        assert_eq!(ops[0].0.hlc, Hlc::new(900, 4));
    }

    #[test]
    fn emitter_never_lets_a_tickets_times_go_backwards() {
        let mut out = Vec::new();
        let actor = ActorId::new(IMPORT_ACTOR);
        let mut e = Emitter {
            out: &mut out,
            entity: Ulid::new(),
            actor: &actor,
            floor: 0,
        };
        e.at(200, Payload::HoldClear, Phase::Ticket);
        e.at(100, Payload::HoldClear, Phase::Ticket);
        e.at(300, Payload::HoldClear, Phase::Relation);
        let times: Vec<u64> = out.iter().map(|i| i.at_ms).collect();
        assert_eq!(times, [200, 200, 300]);
    }
}
