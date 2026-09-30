//! Mutation verbs beyond `pm set` (projects/pm/README.md §CLI verbs): `pm
//! label`, `pm comment`, `pm move`, `pm done`, `pm unclaim`. Every mutation
//! is a `pm_core::Op` committed through `pm_store::Store::commit` (or
//! `commit_batch` when more than one op on a ticket must land atomically);
//! nothing here writes a ticket row directly.
//!
//! No verb reads stdin unless told to (`pm comment --file -`), so a command
//! run with stdin closed or redirected behaves exactly as it does at a
//! terminal (README §Constraints).

use std::fs;
use std::io::Read as _;

use pm_core::op::{
    CommentAdd, FieldSet, LabelAdd, LabelRemove, RelationAdd, RelationRemove, StateTransition,
};
use pm_core::{Payload, Relation, RelationKind, StateCategory};
use serde_json::Value;

use crate::exit::{CliError, Result};
use crate::verbs::{
    Ctx, Stamper, display_id, find, initial_state, non_empty, print_json, ticket_json,
};

// --------------------------------------------------------------- pm label

/// `pm label AGT-N +x -y` (AC1): `+label` emits a `label.add`, `-label` a
/// `label.remove` citing the add-tags this replica currently observes for
/// it (README §Conflict semantics: OR-set, add-wins). One `commit_batch` so
/// a multi-token invocation (`+x -y +z`) can never land partially.
pub fn label(ctx: &Ctx<'_>, reference: &str, changes: &[String]) -> Result<()> {
    enum Change {
        Add(String),
        Remove(String),
    }
    let parsed: Vec<Change> = changes
        .iter()
        .map(|c| {
            let c = c.trim();
            if let Some(rest) = c.strip_prefix('+') {
                Ok(Change::Add(non_empty("label", rest)?))
            } else if let Some(rest) = c.strip_prefix('-') {
                Ok(Change::Remove(non_empty("label", rest)?))
            } else {
                Err(CliError::usage(format!("'{c}' is not +label or -label")))
            }
        })
        .collect::<Result<_>>()?;

    let actor = ctx.actor()?;
    let (mut store, ws) = ctx.open()?;
    let ticket = find(&store, &ws, reference)?;
    // Only a `-label` token needs the current OR-set state (the add-tags it
    // must cite); skip the fetch entirely for a pure-add invocation.
    let view = if parsed.iter().any(|c| matches!(c, Change::Remove(_))) {
        Some(store.ticket_view(ticket.id)?.ok_or_else(|| {
            CliError::not_found(format!("no ticket {}", display_id(&ws, &ticket)))
        })?)
    } else {
        None
    };

    let mut stamper = Stamper::new(&store, actor)?;
    let ops: Vec<_> = parsed
        .into_iter()
        .map(|change| match change {
            Change::Add(label) => stamper.op(ticket.id, Payload::LabelAdd(LabelAdd { label })),
            Change::Remove(label) => {
                let observed = view
                    .as_ref()
                    .expect("a Remove change means view was fetched")
                    .labels
                    .observed(&label);
                stamper.op(
                    ticket.id,
                    Payload::LabelRemove(LabelRemove { label, observed }),
                )
            }
        })
        .collect();
    store.commit_batch(&ops, &[])?;
    print_ticket(ctx, &store, &ws, ticket.id)
}

// -------------------------------------------------------------- pm relate

/// `pm relate AGT-N --blocked-by X,Y --unblock Z` (AGT-1383): edits the
/// blockers of an existing ticket. `--blocked-by X` emits a `relation.add`
/// for `X blocks N`; `--unblock Z` emits a `relation.remove` citing the
/// add-tags this replica observes for `Z blocks N` (OR-set, add-wins).
/// Every id is resolved before anything is written (unknown: exit 3), a
/// ticket blocking itself is a usage error (exit 2), and so is an add
/// that would close a blocker cycle — detected with
/// `pm_core::check::blocker_cycles` over the graph as it would stand after
/// the whole invocation. Adding an existing blocker or removing an absent
/// one is a no-op, so the verb is idempotent. One `commit_batch`, so a
/// multi-id invocation never lands partially.
pub fn relate(
    ctx: &Ctx<'_>,
    reference: &str,
    blocked_by: &[String],
    unblock: &[String],
) -> Result<()> {
    if blocked_by.is_empty() && unblock.is_empty() {
        return Err(CliError::usage(
            "relate needs --blocked-by <ID>[,...] and/or --unblock <ID>[,...]",
        ));
    }
    let actor = ctx.actor()?;
    let (mut store, ws) = ctx.open()?;
    let ticket = find(&store, &ws, reference)?;
    let resolve = |refs: &[String]| -> Result<Vec<pm_core::Ticket>> {
        let mut out: Vec<pm_core::Ticket> = Vec::new();
        for r in refs {
            let other = find(&store, &ws, r)?;
            if !out.iter().any(|t| t.id == other.id) {
                out.push(other);
            }
        }
        Ok(out)
    };
    let to_add = resolve(blocked_by)?;
    let to_remove = resolve(unblock)?;
    for other in &to_add {
        if other.id == ticket.id {
            return Err(CliError::usage(format!(
                "{} cannot be blocked by itself",
                display_id(&ws, &ticket)
            )));
        }
    }
    if let Some(dup) = to_add
        .iter()
        .find(|a| to_remove.iter().any(|r| r.id == a.id))
    {
        return Err(CliError::usage(format!(
            "{} is named by both --blocked-by and --unblock",
            display_id(&ws, dup)
        )));
    }

    let blocks = |from: ulid::Ulid| Relation {
        kind: RelationKind::Blocks,
        from,
        to: ticket.id,
    };
    let view = store
        .ticket_view(ticket.id)?
        .ok_or_else(|| CliError::not_found(format!("no ticket {}", display_id(&ws, &ticket))))?;
    let current: std::collections::BTreeSet<Relation> = store
        .relations(ticket.id)?
        .into_iter()
        .filter(|r| r.kind == RelationKind::Blocks && r.to == ticket.id)
        .collect();
    let adds: Vec<Relation> = to_add
        .iter()
        .map(|t| blocks(t.id))
        .filter(|r| !current.contains(r))
        .collect();
    let removes: Vec<Relation> = to_remove
        .iter()
        .map(|t| blocks(t.id))
        .filter(|r| current.contains(r))
        .collect();

    // The blocker graph as it would stand afterwards: a cycle through a
    // new edge (both endpoints in one component) refuses the whole call.
    if !adds.is_empty() {
        let mut edges: std::collections::BTreeSet<(ulid::Ulid, ulid::Ulid)> = store
            .all_relations()?
            .into_iter()
            .filter(|r| r.kind == RelationKind::Blocks)
            .map(|r| (r.from, r.to))
            .collect();
        for r in &removes {
            edges.remove(&(r.from, r.to));
        }
        edges.extend(adds.iter().map(|r| (r.from, r.to)));
        let edges: Vec<_> = edges.into_iter().collect();
        for cycle in pm_core::check::blocker_cycles(&edges) {
            if let Some(new) = adds
                .iter()
                .find(|r| cycle.contains(&r.from) && cycle.contains(&r.to))
            {
                let names: Vec<String> = cycle
                    .iter()
                    .map(|id| match store.ticket(*id) {
                        Ok(Some(t)) => display_id(&ws, &t),
                        _ => id.to_string(),
                    })
                    .collect();
                let from = match store.ticket(new.from)? {
                    Some(t) => display_id(&ws, &t),
                    None => new.from.to_string(),
                };
                return Err(CliError::usage(format!(
                    "{from} blocking {} would create a blocker cycle ({})",
                    display_id(&ws, &ticket),
                    names.join(", ")
                )));
            }
        }
    }

    let mut stamper = Stamper::new(&store, actor)?;
    let mut ops = Vec::new();
    for relation in adds {
        ops.push(stamper.op(ticket.id, Payload::RelationAdd(RelationAdd { relation })));
    }
    for relation in removes {
        let observed = view.relations.observed(&relation);
        ops.push(stamper.op(
            ticket.id,
            Payload::RelationRemove(RelationRemove { relation, observed }),
        ));
    }
    if !ops.is_empty() {
        store.commit_batch(&ops, &[])?;
    }
    print_ticket(ctx, &store, &ws, ticket.id)
}

// ------------------------------------------------------------- pm comment

/// `pm comment AGT-N "…"` / `pm comment AGT-N --file -` (AC2): appends one
/// comment, stamped with this command's actor and HLC.
pub fn comment(
    ctx: &Ctx<'_>,
    reference: &str,
    text: Option<&str>,
    file: Option<&str>,
) -> Result<()> {
    let body = match (text, file) {
        (Some(_), Some(_)) => {
            return Err(CliError::usage(
                "comment text and --file are mutually exclusive",
            ));
        }
        (Some(text), None) => text.to_string(),
        (None, Some("-")) => {
            let mut buf = String::new();
            std::io::stdin()
                .read_to_string(&mut buf)
                .map_err(|e| CliError::error(format!("reading --file - from stdin: {e}")))?;
            buf
        }
        (None, Some(path)) => fs::read_to_string(path)
            .map_err(|e| CliError::error(format!("reading --file {path}: {e}")))?,
        (None, None) => {
            return Err(CliError::usage("comment text or --file is required"));
        }
    };
    let body = non_empty("comment", &body)?;

    let actor = ctx.actor()?;
    let (mut store, ws) = ctx.open()?;
    let ticket = find(&store, &ws, reference)?;
    let mut stamper = Stamper::new(&store, actor)?;
    store.commit(&stamper.op(ticket.id, Payload::CommentAdd(CommentAdd { body })))?;
    print_ticket(ctx, &store, &ws, ticket.id)
}

// ---------------------------------------------------------------- pm move

/// `pm move AGT-N <state>` (AC3): validates the state exists in this
/// workspace before emitting a `state.transition`.
///
/// AGT-1379: moving an assigned ticket into an `unstarted`-or-`backlog`
/// state clears the assignee in the same batch, exactly like `pm
/// unclaim` — otherwise the ticket is stranded (unstarted but assigned:
/// `pm ready` excludes it, `pm claim` refuses it) until someone runs `pm
/// set assignee=`. `--keep-assignee` opts out.
pub fn mv(ctx: &Ctx<'_>, reference: &str, state: &str, keep_assignee: bool) -> Result<()> {
    let state = non_empty("state", state)?;
    let actor = ctx.actor()?;
    let (mut store, ws) = ctx.open()?;
    let ticket = find(&store, &ws, reference)?;
    let target = ws.state(&state).ok_or_else(|| {
        let known: Vec<&str> = ws.states.iter().map(|s| s.name.as_str()).collect();
        CliError::not_found(format!(
            "no such state '{state}': expected one of {}",
            known.join(", ")
        ))
    })?;

    let mut stamper = Stamper::new(&store, actor)?;
    let mut ops = vec![stamper.op(
        ticket.id,
        Payload::StateTransition(StateTransition {
            state: state.clone(),
        }),
    )];
    let clear_assignee =
        !keep_assignee && target.category.is_unstarted_or_backlog() && ticket.assignee.is_some();
    if clear_assignee {
        ops.push(stamper.op(ticket.id, Payload::FieldSet(FieldSet::Assignee(None))));
        eprintln!(
            "pm: cleared assignee ({}) moving {} to '{state}'; pass --keep-assignee to keep it",
            ticket
                .assignee
                .as_ref()
                .expect("clear_assignee implies Some"),
            display_id(&ws, &ticket),
        );
    }
    store.commit_batch(&ops, &[])?;
    print_ticket(ctx, &store, &ws, ticket.id)
}

// ---------------------------------------------------------------- pm done

pub struct DoneArgs {
    pub note: Option<String>,
    pub merged_sha: Option<String>,
    pub pr: Option<String>,
}

/// `pm done AGT-N [--note --merged-sha --pr]` (AC3): transitions to the
/// workspace's completed state (the lowest-position state whose category is
/// `completed`) and records the note as a comment and the links as fields —
/// `--pr` on `linked-pr` (a real `FieldSet` variant), `--merged-sha` under
/// `ext.merged_sha` (no dedicated field exists for it). One `commit_batch`
/// so a partial done state can never land.
pub fn done(ctx: &Ctx<'_>, reference: &str, args: DoneArgs) -> Result<()> {
    let actor = ctx.actor()?;
    let (mut store, ws) = ctx.open()?;
    let ticket = find(&store, &ws, reference)?;
    let completed = ws
        .states
        .iter()
        .filter(|s| s.category == StateCategory::Completed)
        .min_by_key(|s| s.position)
        .ok_or_else(|| CliError::error("this workspace has no completed state to move into"))?
        .name
        .clone();

    let mut stamper = Stamper::new(&store, actor)?;
    let mut ops = vec![stamper.op(
        ticket.id,
        Payload::StateTransition(StateTransition { state: completed }),
    )];
    if let Some(pr) = &args.pr {
        let pr = non_empty("--pr", pr)?;
        ops.push(stamper.op(ticket.id, Payload::FieldSet(FieldSet::LinkedPr(Some(pr)))));
    }
    if let Some(sha) = &args.merged_sha {
        let sha = non_empty("--merged-sha", sha)?;
        ops.push(stamper.op(
            ticket.id,
            Payload::FieldSet(FieldSet::Ext {
                key: "merged_sha".to_string(),
                value: Some(Value::String(sha)),
            }),
        ));
    }
    if let Some(note) = &args.note {
        let note = non_empty("--note", note)?;
        ops.push(stamper.op(ticket.id, Payload::CommentAdd(CommentAdd { body: note })));
    }

    store.commit_batch(&ops, &[])?;
    print_ticket(ctx, &store, &ws, ticket.id)
}

// ------------------------------------------------------------- pm unclaim

/// `pm unclaim AGT-N` (AC3): a started ticket returns to the workspace's
/// initial unstarted state with its assignee cleared, the two landing in
/// one `commit_batch` so the ticket is never observed half-unclaimed
/// (state changed, assignee still set, or vice versa).
///
/// AGT-1379: a ticket that is already `unstarted`-or-`backlog` but still
/// carries an assignee (the `pm check` `assigned-unstarted` finding —
/// stranded before this fix, or left that way by `pm move
/// --keep-assignee`) has no state to un-start, so this is the one case
/// that clears the assignee alone, with a stderr note. Anything else
/// (unstarted-and-unassigned, completed, canceled) refuses: there is
/// nothing to unclaim.
pub fn unclaim(ctx: &Ctx<'_>, reference: &str) -> Result<()> {
    let actor = ctx.actor()?;
    let (mut store, ws) = ctx.open()?;
    let ticket = find(&store, &ws, reference)?;
    let category = ws.state(&ticket.state).map(|s| s.category);

    let mut stamper = Stamper::new(&store, actor)?;
    let ops = if category == Some(StateCategory::Started) {
        let unstarted = initial_state(&ws)?;
        vec![
            stamper.op(
                ticket.id,
                Payload::StateTransition(StateTransition { state: unstarted }),
            ),
            stamper.op(ticket.id, Payload::FieldSet(FieldSet::Assignee(None))),
        ]
    } else if category.is_some_and(StateCategory::is_unstarted_or_backlog)
        && ticket.assignee.is_some()
    {
        eprintln!(
            "pm: {} is already unstarted (state '{}'); cleared the stray assignee ({})",
            display_id(&ws, &ticket),
            ticket.state,
            ticket.assignee.as_ref().expect("checked Some above"),
        );
        vec![stamper.op(ticket.id, Payload::FieldSet(FieldSet::Assignee(None)))]
    } else {
        return Err(CliError::error(format!(
            "{} is not started (state '{}'); nothing to unclaim",
            display_id(&ws, &ticket),
            ticket.state
        )));
    };
    store.commit_batch(&ops, &[])?;
    print_ticket(ctx, &store, &ws, ticket.id)
}

// ----------------------------------------------------------------- shared

/// The `set`/`label`/`comment`/`move`/`done`/`unclaim` result shape: the
/// ticket as it now reads, same as `pm show`.
fn print_ticket(
    ctx: &Ctx<'_>,
    store: &pm_store::Store,
    ws: &pm_core::Workspace,
    id: ulid::Ulid,
) -> Result<()> {
    let ticket = store
        .ticket(id)?
        .ok_or_else(|| CliError::error(format!("ticket {id} vanished after mutation")))?;
    if ctx.json {
        print_json(&ticket_json(ws, store, &ticket)?);
    } else {
        println!("{}", display_id(ws, &ticket));
    }
    Ok(())
}
