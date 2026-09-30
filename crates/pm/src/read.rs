//! Read verbs: `pm list`, `pm log`, `pm status`, `pm graph`
//! (projects/pm/README.md §CLI verbs; AGT-1339). Also `pm show --section`,
//! which lives here (the markdown-section slicer) but is called from
//! `verbs::show` since `Show` itself predates this module.
//!
//! Every function here only ever calls `Store::tickets` / `Store::ticket` /
//! `Store::relations` / `Store::ops` — the plain queries in
//! `pm_store::query` — so nothing here can block on a writer's lock or
//! touch the network (README §Constraints).

use std::collections::{BTreeMap, BTreeSet};

use pm_core::{ActorId, Op, Payload, Priority, RelationKind, StateCategory, Ticket};
use pm_store::TicketFilter;
use serde_json::{Value, json};
use ulid::Ulid;

use crate::exit::{CliError, Result};
use crate::verbs::{Ctx, SCHEMA, display_id, find, print_json, ticket_json};

// ---------------------------------------------------------------- pm list

pub struct ListArgs {
    pub project: Vec<String>,
    pub state: Vec<String>,
    pub label: Vec<String>,
    pub repo: Vec<String>,
    pub assignee: Vec<String>,
    pub held: bool,
    pub github: Vec<String>,
    pub search: Option<String>,
    pub archived: bool,
}

/// `pm list` (AC1): every value filter AND-combines with the others; each
/// one's own values OR together (`--state triage,in-progress` matches
/// either). `--json` prints a bare array, each element carrying `schema`
/// exactly as `pm show --json` does for one ticket.
pub fn list(ctx: &Ctx<'_>, args: ListArgs) -> Result<()> {
    let (store, ws) = ctx.open()?;
    let filter = TicketFilter {
        state: args.state,
        project: args.project,
        label: args.label,
        repo: args.repo,
        assignee: args.assignee.into_iter().map(ActorId::new).collect(),
        held: args.held,
        github: args.github,
        search: args.search,
        archived: args.archived,
    };
    let tickets = store.tickets(&filter)?;

    if ctx.json {
        let mut out = Vec::with_capacity(tickets.len());
        for t in &tickets {
            out.push(ticket_json(&ws, &store, t)?);
        }
        print_json(&Value::Array(out));
        return Ok(());
    }

    if tickets.is_empty() {
        eprintln!("no tickets found");
        return Ok(());
    }
    let widths = (
        tickets
            .iter()
            .map(|t| display_id(&ws, t).len())
            .max()
            .unwrap_or(2),
        tickets.iter().map(|t| t.state.len()).max().unwrap_or(5),
    );
    for t in &tickets {
        let project = t.project.as_deref().unwrap_or("-");
        println!(
            "{:<id_w$}  {:<state_w$}  {:<8}  {:<12}  {}",
            display_id(&ws, t),
            t.state,
            priority_str(&t.priority),
            project,
            t.title,
            id_w = widths.0,
            state_w = widths.1,
        );
    }
    Ok(())
}

// ----------------------------------------------------------------- pm log

/// `pm log [<id>]` (AC3): the ticket's ops, oldest first (`Store::ops`
/// already returns them in append order). With no `<id>`, the workspace's
/// config ops instead (AGT-1386): every `workspace.set`, `state.upsert`,
/// `actor.upsert`, `project.create`, `project.set` and `project.delete`.
pub fn log(ctx: &Ctx<'_>, reference: Option<&str>) -> Result<()> {
    let (store, ws) = ctx.open()?;
    let ops = match reference {
        Some(reference) => store.ops(find(&store, &ws, reference)?.id)?,
        None => store.config_ops()?,
    };
    // A project's slug is on its `project.create`; that names the
    // `project.set`s and the `project.delete` that follow.
    let slugs: BTreeMap<Ulid, String> = ops
        .iter()
        .filter_map(|op| match &op.payload {
            Payload::ProjectCreate(c) => Some((op.entity, c.id.clone())),
            _ => None,
        })
        .collect();

    if ctx.json {
        let out: Vec<Value> = ops.iter().map(|op| op_json(op, &slugs)).collect();
        print_json(&Value::Array(out));
        return Ok(());
    }
    if ops.is_empty() {
        eprintln!("no ops found");
        return Ok(());
    }
    for op in &ops {
        println!(
            "{:<20}  {:<28}  {:<16}  {}",
            op.hlc,
            op.actor.as_str(),
            op.kind(),
            op_summary_in(op, &slugs)
        );
    }
    Ok(())
}

fn op_json(op: &Op, slugs: &BTreeMap<Ulid, String>) -> Value {
    json!({
        "schema": SCHEMA,
        "op_id": op.op_id.to_string(),
        "hlc": { "wall_ms": op.hlc.wall_ms, "counter": op.hlc.counter },
        "actor": op.actor.as_str(),
        "kind": op.kind(),
        "summary": op_summary_in(op, slugs),
    })
}

/// A short, human-readable description of what an op did. Relation
/// endpoints print as raw ULIDs (not display ids): resolving them to
/// `AGT-N` would mean a store lookup per op, and `pm log` is meant to read
/// straight off the log, not to re-derive ticket state. `slugs` maps a
/// project Ulid to its slug, so a config op that targets a project
/// (`project.set`, `project.delete`) names it.
fn op_summary_in(op: &Op, slugs: &BTreeMap<Ulid, String>) -> String {
    let project = || match slugs.get(&op.entity) {
        Some(slug) => format!("'{slug}'"),
        None => format!("{}", op.entity),
    };
    match &op.payload {
        Payload::TicketCreate(c) => format!("created \"{}\"", c.title),
        Payload::FieldSet(f) => field_summary(f),
        Payload::LabelAdd(l) => format!("added label '{}'", l.label),
        Payload::LabelRemove(l) => format!("removed label '{}'", l.label),
        Payload::RelationAdd(r) => format!(
            "added {} relation ({} -> {})",
            relation_kind_str(r.relation.kind),
            r.relation.from,
            r.relation.to
        ),
        Payload::RelationRemove(r) => format!(
            "removed {} relation ({} -> {})",
            relation_kind_str(r.relation.kind),
            r.relation.from,
            r.relation.to
        ),
        Payload::CommentAdd(c) => format!("commented: {}", truncate(&c.body, 60)),
        Payload::StateTransition(s) => format!("moved to '{}'", s.state),
        Payload::Claim(c) => format!("claimed by {} (-> '{}')", c.assignee, c.state),
        Payload::HoldSet(h) => format!("held: {}", h.hold.reason),
        Payload::HoldClear => "cleared hold".to_string(),
        Payload::BodyEdit(_) => "edited description".to_string(),
        Payload::Tombstone => "deleted".to_string(),
        // Config kinds (AGT-1384/1386): the workspace-wide `pm log` (no
        // ticket) lists these.
        Payload::WorkspaceSet(w) => workspace_summary(w),
        Payload::StateUpsert(s) => format!(
            "set state '{}' ({}, position {})",
            s.name,
            word(&s.category),
            s.position
        ),
        Payload::ActorUpsert(a) => format!("registered {} actor '{}'", word(&a.kind), a.id),
        Payload::ProjectCreate(p) => format!(
            "created project '{}' \"{}\" ({}{})",
            p.id,
            p.title,
            word(&p.status),
            p.parent
                .as_ref()
                .map(|parent| format!(", under '{parent}'"))
                .unwrap_or_default()
        ),
        Payload::ProjectSet(p) => project_summary(&project(), p),
        Payload::ProjectDelete => format!("deleted project {}", project()),
    }
}

/// The serde spelling of a unit-variant enum (`in-progress`, `agent`).
fn word<T: serde::Serialize>(v: &T) -> String {
    serde_json::to_value(v)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_default()
}

fn workspace_summary(w: &pm_core::op::WorkspaceSet) -> String {
    use pm_core::op::WorkspaceSet;
    match w {
        WorkspaceSet::Prefix(v) => format!("set workspace prefix to '{v}'"),
        WorkspaceSet::GateLabelAdd(l) => format!("added gate label '{l}'"),
        WorkspaceSet::GateLabelRemove { label, .. } => format!("removed gate label '{label}'"),
        WorkspaceSet::ModelLabel {
            label,
            model: Some(m),
        } => format!("mapped model label '{label}' to '{m}'"),
        WorkspaceSet::ModelLabel { label, model: None } => {
            format!("unmapped model label '{label}'")
        }
        WorkspaceSet::TemplateSections(v) => {
            format!("set template sections to [{}]", v.join(", "))
        }
        WorkspaceSet::StaleDays(d) => format!("set stale days to {d}"),
    }
}

fn project_summary(name: &str, p: &pm_core::op::ProjectSet) -> String {
    use pm_core::op::ProjectSet;
    match p {
        ProjectSet::Title(v) => format!("set project {name} title to \"{v}\""),
        ProjectSet::Status(v) => format!("set project {name} status to {}", word(v)),
        ProjectSet::Parent(Some(v)) => format!("set project {name} parent to '{v}'"),
        ProjectSet::Parent(None) => format!("cleared project {name} parent"),
        ProjectSet::RepoAdd(r) => format!("added repo '{r}' to project {name}"),
        ProjectSet::RepoRemove { repo, .. } => format!("removed repo '{repo}' from project {name}"),
    }
}

fn field_summary(f: &pm_core::op::FieldSet) -> String {
    use pm_core::op::FieldSet;
    let opt = |v: &Option<String>| v.clone().unwrap_or_else(|| "-".into());
    match f {
        FieldSet::Title(v) => format!("set title to \"{v}\""),
        FieldSet::Priority(v) => format!("set priority to {}", priority_str(v)),
        FieldSet::Project(v) => format!("set project to {}", opt(v)),
        FieldSet::Repo(v) => format!("set repo to {}", opt(v)),
        FieldSet::Assignee(v) => format!(
            "set assignee to {}",
            v.as_ref()
                .map(ActorId::to_string)
                .unwrap_or_else(|| "-".into())
        ),
        FieldSet::LinkedGithub(v) => format!("set linked-github to {}", opt(v)),
        FieldSet::LinkedPr(v) => format!("set linked-pr to {}", opt(v)),
        FieldSet::Linear(v) => format!("set linear to {}", opt(v)),
        FieldSet::Source(v) => format!(
            "set source ({})",
            v.as_ref().map(|s| s.kind.as_str()).unwrap_or("-")
        ),
        FieldSet::Waivers(v) => format!("set {} waiver(s)", v.len()),
        FieldSet::NotBefore(v) => format!(
            "set not-before to {}",
            v.as_ref().map(|n| n.date.as_str()).unwrap_or("-")
        ),
        FieldSet::Parked(v) => format!(
            "set parked-until to {}",
            v.as_ref().map(|p| p.until.as_str()).unwrap_or("-")
        ),
        FieldSet::ArchivedAt(v) => (if v.is_some() {
            "archived"
        } else {
            "unarchived"
        })
        .into(),
        FieldSet::Number(n) => format!("numbered {n}"),
        FieldSet::Ext { key, value } => {
            if value.is_some() {
                format!("set ext.{key}")
            } else {
                format!("cleared ext.{key}")
            }
        }
    }
}

fn relation_kind_str(kind: RelationKind) -> &'static str {
    match kind {
        RelationKind::Blocks => "blocks",
        RelationKind::Parent => "parent",
        RelationKind::SupersededBy => "superseded-by",
    }
}

/// Round-trips through `serde_json` rather than a `Display`/`as_str` impl
/// (`pm_core::Priority` has neither) — the same approach `verbs::print_human`
/// already uses for the same field. Coupled to `Priority`'s serde shape:
/// if that ever changes, this display string changes with it.
pub(crate) fn priority_str(p: &Priority) -> String {
    serde_json::to_value(p)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_default()
}

fn truncate(s: &str, max: usize) -> String {
    let s = s.trim();
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let cut: String = s.chars().take(max).collect();
        format!("{cut}\u{2026}")
    }
}

// -------------------------------------------------------------- pm status

/// `pm status [--project]` (AC4): counts per workflow state (every state
/// listed, even at 0), plus how many are held or parked.
pub fn status(ctx: &Ctx<'_>, project: Option<String>) -> Result<()> {
    let (store, ws) = ctx.open()?;
    let filter = TicketFilter {
        project: project.clone().into_iter().collect(),
        ..Default::default()
    };
    let tickets = store.tickets(&filter)?;

    let mut counts: BTreeMap<&str, u64> = ws.states.iter().map(|s| (s.name.as_str(), 0)).collect();
    let mut held = 0u64;
    let mut parked = 0u64;
    for t in &tickets {
        *counts.entry(t.state.as_str()).or_insert(0) += 1;
        if t.hold.is_some() {
            held += 1;
        }
        if t.parked.is_some() {
            parked += 1;
        }
    }

    if ctx.json {
        // AGT-1339 review: a `BTreeMap` here serialized `states` with keys
        // in alphabetical order, not workflow order (`ws.states` is
        // `ORDER BY position, name` — pm-store::config::states). Since
        // this ticket freezes `--json` shapes, an ordered array preserves
        // workflow order and survives a schema change that makes `Value`
        // an object instead of a bare `u64`.
        let states: Vec<Value> = ws
            .states
            .iter()
            .map(|s| {
                json!({
                    "name": s.name,
                    "category": s.category,
                    "count": counts.get(s.name.as_str()).copied().unwrap_or(0),
                })
            })
            .collect();
        print_json(&json!({
            "schema": SCHEMA,
            "project": project,
            "states": states,
            "held": held,
            "parked": parked,
        }));
        return Ok(());
    }
    let width = ws
        .states
        .iter()
        .map(|s| s.name.len())
        .max()
        .unwrap_or(5)
        .max(6);
    for state in &ws.states {
        println!(
            "{:<width$}  {}",
            state.name,
            counts.get(state.name.as_str()).copied().unwrap_or(0)
        );
    }
    println!("{:<width$}  {held}", "held");
    println!("{:<width$}  {parked}", "parked");
    Ok(())
}

// --------------------------------------------------------------- pm graph

pub struct GraphArgs {
    pub project: Option<String>,
    pub ids: Vec<String>,
}

/// `pm graph [--project P | --ids …]` (AC4; AGT-1380 AC1): tickets not yet
/// in a `completed`-category state, grouped into readiness waves by their
/// still-pending `blocks` relations, plus a `done` flag (`true` once
/// nothing in scope is pending — what a build loop's self-retire check
/// wants). `--ids` scopes exactly like `pm ready --ids`: an unknown id is
/// exit `3`, and waves/`done` are computed over just that id set (a
/// resolved id already archived, tombstoned or completed simply is not a
/// pending node — matching what `--project` already did by never fetching
/// those rows). Blockers are always resolved through the whole workspace,
/// whatever the scope: a blocker outside it still counts, it is just never
/// itself a node. A blocker that sits outside the scope and never
/// resolves, or a dependency cycle, lands the rest of the graph in one
/// final, unordered wave rather than looping forever.
pub fn graph(ctx: &Ctx<'_>, args: GraphArgs) -> Result<()> {
    let (store, ws) = ctx.open()?;
    let completed = |state: &str| {
        ws.state(state)
            .map(|s| s.category == StateCategory::Completed)
            .unwrap_or(false)
    };
    let is_done = |t: &Ticket| t.deleted || t.archived_at.is_some() || completed(&t.state);

    let ids: Option<BTreeSet<Ulid>> = if args.ids.is_empty() {
        None
    } else {
        Some(
            args.ids
                .iter()
                .map(|r| find(&store, &ws, r).map(|t| t.id))
                .collect::<Result<BTreeSet<Ulid>>>()?,
        )
    };

    let tickets: Vec<Ticket> = match &ids {
        // The whole snapshot (tombstoned/archived included, same as
        // `pm ready`'s `Scope::Ids`), filtered to the requested set — so a
        // requested id that is already done still shows in `ids` below,
        // just not as a pending node.
        Some(id_set) => store
            .all_tickets()?
            .into_iter()
            .filter(|t| id_set.contains(&t.id))
            .collect(),
        None => store.tickets(&TicketFilter {
            project: args.project.clone().into_iter().collect(),
            ..Default::default()
        })?,
    };

    let pending: Vec<&Ticket> = tickets.iter().filter(|t| !is_done(t)).collect();
    let done = pending.is_empty();

    // Each pending ticket's still-pending blockers (a completed blocker
    // never holds anything back, so it is dropped here).
    let mut blockers: BTreeMap<Ulid, Vec<Ulid>> = BTreeMap::new();
    for t in &pending {
        let mut unresolved = Vec::new();
        for r in store.relations(t.id)? {
            if r.kind != RelationKind::Blocks || r.to != t.id {
                continue;
            }
            // Archived counts as done whatever the state says (AGT-1343:
            // the loops went blind once a blocker was swept into the
            // archive), as does a tombstoned or absent blocker.
            let blocker_done = match store.ticket(r.from)? {
                Some(b) => is_done(&b),
                None => true,
            };
            if !blocker_done {
                unresolved.push(r.from);
            }
        }
        blockers.insert(t.id, unresolved);
    }

    // The wave routine `pm ready` uses too (`pm_core::ready::waves`);
    // `pending` is already in `Store::tickets` order, which each wave
    // keeps. No progress possible — a dependency cycle, or a blocker
    // outside `pending` that never resolves — lands the rest in a final,
    // unordered wave.
    let pending_ids: Vec<Ulid> = pending.iter().map(|t| t.id).collect();
    let pm_core::ready::Waves { mut waves, stuck } = pm_core::ready::waves(&pending_ids, &blockers);
    if !stuck.is_empty() {
        waves.push(stuck);
    }
    let names: BTreeMap<Ulid, String> =
        tickets.iter().map(|t| (t.id, display_id(&ws, t))).collect();
    let wave_ids: Vec<Vec<String>> = waves
        .iter()
        .map(|wave| wave.iter().map(|id| names[id].clone()).collect())
        .collect();

    if ctx.json {
        print_json(&json!({
            "schema": SCHEMA,
            "project": args.project,
            "ids": ids.as_ref().map(|id_set| {
                tickets
                    .iter()
                    .filter(|t| id_set.contains(&t.id))
                    .map(|t| display_id(&ws, t))
                    .collect::<Vec<_>>()
            }),
            "waves": wave_ids,
            "done": done,
        }));
        return Ok(());
    }
    for (i, wave) in wave_ids.iter().enumerate() {
        println!("wave {i}: {}", wave.join(" "));
    }
    println!("done: {done}");
    Ok(())
}

// ---------------------------------------------------------- pm show --section

/// `pm show --section <name>` (AGT-1339 AC2): one `## <name>` markdown
/// section of the ticket's description, matched case-insensitively.
pub fn print_section(ctx: &Ctx<'_>, ticket: &Ticket, section: &str) -> Result<()> {
    match extract_section(&ticket.description, section) {
        Some(body) => {
            if ctx.json {
                print_json(&json!({ "schema": SCHEMA, "section": section, "body": body }));
            } else {
                print!("{body}");
                if !body.ends_with('\n') {
                    println!();
                }
            }
            Ok(())
        }
        None => {
            let known = section_headings(&ticket.description);
            let hint = if known.is_empty() {
                String::new()
            } else {
                format!("; sections on this ticket: {}", known.join(", "))
            };
            Err(CliError::not_found(format!("no '{section}' section{hint}")))
        }
    }
}

/// Every `## Heading` in `body`, in document order.
fn section_headings(body: &str) -> Vec<String> {
    body.lines()
        .filter(|l| l.trim_start().starts_with("## "))
        .map(|l| l.trim_start()[3..].trim().to_string())
        .collect()
}

/// The body text between a `## <name>` heading (case-insensitive) and the
/// next `##` heading or the end of the description; `None` if no heading
/// matches.
fn extract_section(body: &str, name: &str) -> Option<String> {
    let target = name.trim();
    let lines: Vec<&str> = body.lines().collect();
    let is_h2 = |l: &&str| l.trim_start().starts_with("## ");
    let heading = |l: &str| l.trim_start()[3..].trim().to_string();

    let start = lines
        .iter()
        .position(|l| is_h2(l) && heading(l).eq_ignore_ascii_case(target))?;
    let end = lines[start + 1..]
        .iter()
        .position(is_h2)
        .map(|i| start + 1 + i)
        .unwrap_or(lines.len());
    Some(
        lines[start + 1..end]
            .join("\n")
            .trim_matches('\n')
            .to_string(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_section_finds_a_heading_case_insensitively_and_stops_at_the_next_one() {
        let body = "## Problem Statement\n\nSomething's broken.\n\n## Acceptance Criteria\n\n1. X\n2. Y\n\n## Comments\n\nnote";
        assert_eq!(
            extract_section(body, "acceptance criteria").as_deref(),
            Some("1. X\n2. Y")
        );
        assert_eq!(
            extract_section(body, "Problem Statement").as_deref(),
            Some("Something's broken.")
        );
        assert_eq!(extract_section(body, "Comments").as_deref(), Some("note"));
        assert_eq!(extract_section(body, "Nope"), None);
    }

    #[test]
    fn extract_section_does_not_stop_at_a_deeper_heading() {
        let body = "## Acceptance Criteria\n\n1. X\n\n### Notes\n\nmore\n\n## Comments\n\nc";
        assert_eq!(
            extract_section(body, "Acceptance Criteria").as_deref(),
            Some("1. X\n\n### Notes\n\nmore")
        );
    }

    #[test]
    fn section_headings_lists_every_h2_in_order() {
        let body = "## A\n\nx\n\n## B\n\ny";
        assert_eq!(
            section_headings(body),
            vec!["A".to_string(), "B".to_string()]
        );
    }
}
