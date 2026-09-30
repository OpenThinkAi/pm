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
    let parsed: Vec<LabelChange> = changes
        .iter()
        .map(|c| {
            let c = c.trim();
            if let Some(rest) = c.strip_prefix('+') {
                Ok(LabelChange::Add(non_empty("label", rest)?))
            } else if let Some(rest) = c.strip_prefix('-') {
                Ok(LabelChange::Remove(non_empty("label", rest)?))
            } else {
                Err(CliError::usage(format!("'{c}' is not +label or -label")))
            }
        })
        .collect::<Result<_>>()?;

    let actor = ctx.actor()?;
    let (mut store, ws) = ctx.open()?;
    let ticket = find(&store, &ws, reference)?;
    let mut stamper = Stamper::new(&store, actor)?;
    let ops = label_ops(&store, &ws, &ticket, &mut stamper, &parsed)?;
    store.commit_batch(&ops, &[])?;
    print_ticket(ctx, &store, &ws, ticket.id)
}

/// One `+label` / `-label` token, parsed.
pub(crate) enum LabelChange {
    Add(String),
    Remove(String),
}

/// The ops for `changes` on `ticket`, in order: a `label.add` per add, a
/// `label.remove` citing the add-tags this replica currently observes per
/// remove. `pub(crate)`: `pm app`'s label endpoint (AGT-1401) plans its
/// ops here too, so the API and the CLI never disagree on what a remove
/// cites.
pub(crate) fn label_ops(
    store: &pm_store::Store,
    ws: &pm_core::Workspace,
    ticket: &pm_core::Ticket,
    stamper: &mut Stamper,
    changes: &[LabelChange],
) -> Result<Vec<pm_core::Op>> {
    // Only a `-label` token needs the current OR-set state (the add-tags it
    // must cite); skip the fetch entirely for a pure-add invocation.
    let view =
        if changes.iter().any(|c| matches!(c, LabelChange::Remove(_))) {
            Some(store.ticket_view(ticket.id)?.ok_or_else(|| {
                CliError::not_found(format!("no ticket {}", display_id(ws, ticket)))
            })?)
        } else {
            None
        };
    Ok(changes
        .iter()
        .map(|change| match change {
            LabelChange::Add(label) => stamper.op(
                ticket.id,
                Payload::LabelAdd(LabelAdd {
                    label: label.clone(),
                }),
            ),
            LabelChange::Remove(label) => {
                let observed = view
                    .as_ref()
                    .expect("a Remove change means view was fetched")
                    .labels
                    .observed(label);
                stamper.op(
                    ticket.id,
                    Payload::LabelRemove(LabelRemove {
                        label: label.clone(),
                        observed,
                    }),
                )
            }
        })
        .collect())
}

// -------------------------------------------------------------- pm relate

/// `pm relate AGT-N --blocked-by X --unblock Y --blocks Z --unblocks W`
/// (AGT-1383, AGT-1414): edits the blocker edges around an existing
/// ticket. `--blocked-by X` / `--blocks Z` emit a `relation.add` for
/// `X blocks N` / `N blocks Z`; `--unblock Y` / `--unblocks W` emit a
/// `relation.remove` citing the add-tags this replica observes for that
/// edge (OR-set, add-wins). Every id is resolved before anything is
/// written (unknown: exit 3); a ticket blocking itself, or one edge named
/// by both an add flag and a remove flag, is a usage error (exit 2), and
/// so is an add that would close a blocker cycle — detected with
/// `pm_core::check::blocker_cycles` over the graph as it would stand after
/// the whole invocation. Adding an existing edge or removing an absent one
/// is a no-op, so the verb is idempotent. One `commit_batch`, so a
/// multi-id invocation never lands partially. Added edges are owned by
/// the blocked ticket (the `to` end), as `pm new --blocked-by` does.
pub fn relate(
    ctx: &Ctx<'_>,
    reference: &str,
    blocked_by: &[String],
    unblock: &[String],
    blocks: &[String],
    unblocks: &[String],
) -> Result<()> {
    if blocked_by.is_empty() && unblock.is_empty() && blocks.is_empty() && unblocks.is_empty() {
        return Err(CliError::usage(
            "relate needs at least one of --blocked-by <ID>[,...], --unblock <ID>[,...], --blocks <ID>[,...], --unblocks <ID>[,...]",
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
    let by_add = resolve(blocked_by)?;
    let by_remove = resolve(unblock)?;
    let blocks_add = resolve(blocks)?;
    let blocks_remove = resolve(unblocks)?;
    for other in by_add.iter().chain(&blocks_add) {
        if other.id == ticket.id {
            return Err(CliError::usage(format!(
                "{} cannot block itself",
                display_id(&ws, &ticket)
            )));
        }
    }

    // `X blocks N` for the incoming flags, `N blocks Z` for the outgoing.
    let incoming = |t: &pm_core::Ticket| Relation {
        kind: RelationKind::Blocks,
        from: t.id,
        to: ticket.id,
    };
    let outgoing = |t: &pm_core::Ticket| Relation {
        kind: RelationKind::Blocks,
        from: ticket.id,
        to: t.id,
    };
    let want_add: Vec<Relation> = by_add
        .iter()
        .map(incoming)
        .chain(blocks_add.iter().map(outgoing))
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();
    let want_remove: Vec<Relation> = by_remove
        .iter()
        .map(incoming)
        .chain(blocks_remove.iter().map(outgoing))
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();
    if let Some(dup) = want_add.iter().find(|a| want_remove.contains(a)) {
        let name = |id: ulid::Ulid| -> Result<String> {
            Ok(match store.ticket(id)? {
                Some(t) => display_id(&ws, &t),
                None => id.to_string(),
            })
        };
        return Err(CliError::usage(format!(
            "{} blocks {} is both added and removed in one call",
            name(dup.from)?,
            name(dup.to)?
        )));
    }

    let current: std::collections::BTreeSet<Relation> = store
        .all_relations()?
        .into_iter()
        .filter(|r| r.kind == RelationKind::Blocks)
        .collect();
    let adds: Vec<Relation> = want_add
        .into_iter()
        .filter(|r| !current.contains(r))
        .collect();
    let removes: Vec<Relation> = want_remove
        .into_iter()
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
                let name = |id: ulid::Ulid| -> Result<String> {
                    Ok(match store.ticket(id)? {
                        Some(t) => display_id(&ws, &t),
                        None => id.to_string(),
                    })
                };
                return Err(CliError::usage(format!(
                    "{} blocking {} would create a blocker cycle ({})",
                    name(new.from)?,
                    name(new.to)?,
                    names.join(", ")
                )));
            }
        }
    }

    let mut stamper = Stamper::new(&store, actor)?;
    let mut ops = Vec::new();
    for relation in adds {
        ops.push(stamper.op(relation.to, Payload::RelationAdd(RelationAdd { relation })));
    }
    // An edge may be owned by either end (the OR-set lives on the op's
    // ticket), so cite the observed add-tags from each owner's view.
    for relation in removes {
        for owner in [relation.to, relation.from] {
            let Some(view) = store.ticket_view(owner)? else {
                continue;
            };
            let observed = view.relations.observed(&relation);
            if observed.is_empty() {
                continue;
            }
            ops.push(stamper.op(
                owner,
                Payload::RelationRemove(RelationRemove { relation, observed }),
            ));
        }
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
    let mut stamper = Stamper::new(&store, actor)?;
    let (ops, cleared) = move_ops(&ws, &ticket, &mut stamper, &state, keep_assignee)?;
    if let Some(assignee) = cleared {
        eprintln!(
            "pm: cleared assignee ({}) moving {} to '{}'; pass --keep-assignee to keep it",
            crate::text::inline(&assignee.to_string()),
            display_id(&ws, &ticket),
            crate::text::inline(&state),
        );
    }
    store.commit_batch(&ops, &[])?;
    print_ticket(ctx, &store, &ws, ticket.id)
}

/// The ops that move `ticket` to `state`: the `state.transition`, plus
/// the assignee clear AGT-1379 adds when the target is unstarted/backlog
/// (unless `keep_assignee`). Returns the assignee it cleared, if any, so
/// the caller can say so. An unknown state is exit `3`. `pub(crate)`:
/// `pm app`'s state endpoint (AGT-1401) plans its ops here too.
pub(crate) fn move_ops(
    ws: &pm_core::Workspace,
    ticket: &pm_core::Ticket,
    stamper: &mut Stamper,
    state: &str,
    keep_assignee: bool,
) -> Result<(Vec<pm_core::Op>, Option<pm_core::ActorId>)> {
    let target = ws.state(state).ok_or_else(|| {
        let known: Vec<&str> = ws.states.iter().map(|s| s.name.as_str()).collect();
        CliError::not_found(format!(
            "no such state '{state}': expected one of {}",
            known.join(", ")
        ))
    })?;
    let mut ops = vec![stamper.op(
        ticket.id,
        Payload::StateTransition(StateTransition {
            state: state.to_string(),
        }),
    )];
    let cleared = if !keep_assignee && target.category.is_unstarted_or_backlog() {
        ticket.assignee.clone()
    } else {
        None
    };
    if cleared.is_some() {
        ops.push(stamper.op(ticket.id, Payload::FieldSet(FieldSet::Assignee(None))));
    }
    Ok((ops, cleared))
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
            crate::text::inline(&ticket.state),
            crate::text::inline(
                ticket
                    .assignee
                    .as_ref()
                    .expect("checked Some above")
                    .as_str()
            ),
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
