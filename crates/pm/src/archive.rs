//! `pm archive` / `pm unarchive` (AGT-1351, projects/pm/README.md §CLI
//! verbs "`pm archive --auto` — `vault-sweep` §1-2"): fields instead of
//! folder moves. `vault-sweep` archived done tickets by `git mv`-ing their
//! file into `archive/<month>/` once a month had passed, and retired a
//! project (`archive/projects/<p>`) once it had no live tickets and had
//! gone untouched for 30 days; here both become writes against the
//! materialized tables — `archived_at` on a ticket, `status: complete` on
//! a project — so `pm list --archived` and `pm ready`'s blocker check
//! still see the ticket (README: "Archive blindness" is the U1 problem
//! this ticket exists to close).
//!
//! Which tickets/projects qualify is decided by pure functions in
//! `pm_core::archive` (`ticket_archivable`, `project_idle`); this module's
//! job is only to gather their inputs from the store, apply the result
//! (unless `--dry-run`), and print what changed or would change.
//!
//! A ticket's *completion time* is the HLC that last set its `state` LWW
//! register to a `completed`-category value — i.e. `TicketView::state`'s
//! own stamp (`Store::ticket_view`), not `Ticket::updated` (the greatest
//! stamp of *any* field, which a later comment or label would advance past
//! the actual completion). A project's *last doc edit* is the greatest
//! `body.edit` stamp across its design doc and every named document
//! (`Store::project_doc_last_edit`); a project with no doc edit on record
//! at all counts as stale, matching `pm_core::archive::project_idle`.

use std::collections::BTreeSet;

use pm_core::op::FieldSet;
use pm_core::{Hlc, Payload, Project, ProjectStatus, StateCategory, Ticket, Workspace};
use pm_store::{Store, TicketFilter};
use serde_json::{Value, json};
use ulid::Ulid;

use crate::exit::{CliError, Result};
use crate::verbs::{Ctx, SCHEMA, Stamper, display_id, find, print_json, ref_id, ticket_json};

/// `pm archive AGT-N… [--force]` or `pm archive --auto [--dry-run]`;
/// clap's `conflicts_with`/`requires` rule out the other combinations
/// (ids or `--force` with `--auto`, `--dry-run` without `--auto`).
pub fn archive(
    ctx: &Ctx<'_>,
    ids: &[String],
    auto: bool,
    dry_run: bool,
    force: bool,
) -> Result<()> {
    match (ids.is_empty(), auto) {
        (false, false) => archive_ids(ctx, ids, force),
        (true, true) => archive_auto(ctx, dry_run),
        (true, false) => Err(CliError::usage("pm archive requires an id or --auto")),
        (false, true) => unreachable!("clap's conflicts_with rules this out"),
    }
}

// ------------------------------------------------------------ named tickets

/// `pm archive AGT-N` (AC3): sets `archived_at` to this op's own HLC.
/// AGT-1576: several ids (`pm archive AGT-1 AGT-2`, or `AGT-1,AGT-2`)
/// archive in one `commit_batch`, every id resolved first (`crate::bulk`).
///
/// AGT-1572 AC4: only tickets in a `completed` or `canceled` state archive
/// as asked; if any named ticket is in another state the whole call is
/// refused (exit `2`, nothing written, every such ticket listed) unless
/// `--force`, because an archived blocker counts as done for its
/// dependents unless it was canceled — archiving unfinished work is how a
/// dependent used to become ready with no signal. Retire such a ticket by
/// moving it to a canceled state instead (which keeps its dependents
/// blocked until someone decides), then archive it.
fn archive_ids(ctx: &Ctx<'_>, refs: &[String], force: bool) -> Result<()> {
    let actor = ctx.actor()?;
    let (mut store, ws) = ctx.open()?;
    let targets = crate::bulk::resolve(&store, &ws, refs)?;
    if !force {
        refuse_unfinished(&ws, &targets.tickets)?;
    }
    let mut stamper = Stamper::new(&store, actor)?;
    let ops: Vec<pm_core::Op> = targets
        .tickets
        .iter()
        .map(|t| {
            stamper.op_with_hlc(t.id, |hlc| {
                Payload::FieldSet(FieldSet::ArchivedAt(Some(hlc)))
            })
        })
        .collect();
    store.commit_batch(&ops, &[])?;
    let results: Vec<(Ulid, &str)> = targets.tickets.iter().map(|t| (t.id, "archived")).collect();
    crate::bulk::print_results(ctx, &store, &ws, targets.many, &results)
}

/// Exit `2` naming every ticket in `tickets` that is neither completed nor
/// canceled (AGT-1572 AC4), with how to retire it instead.
fn refuse_unfinished(ws: &Workspace, tickets: &[Ticket]) -> Result<()> {
    let unfinished: Vec<&Ticket> = tickets
        .iter()
        .filter(|t| {
            !ws.state(&t.state).is_some_and(|s| {
                matches!(
                    s.category,
                    StateCategory::Completed | StateCategory::Canceled
                )
            })
        })
        .collect();
    if unfinished.is_empty() {
        return Ok(());
    }
    let named: Vec<String> = unfinished
        .iter()
        .map(|t| format!("{} (state '{}')", ref_id(ws, t), t.state))
        .collect();
    let retire = match ws
        .states
        .iter()
        .find(|s| s.category == StateCategory::Canceled)
    {
        Some(state) => {
            let first = ref_id(ws, unfinished[0]);
            format!("cancel it first (`pm move {first} {}`)", state.name)
        }
        None => "add a canceled state (`pm workspace state add <NAME> --category canceled`) \
                 and move it there first"
            .to_string(),
    };
    Err(CliError::usage(format!(
        "{} neither completed nor canceled: archived, it would count as done and silently \
         unblock its dependents. To retire a ticket, {retire}, then archive it; pass --force \
         to archive as it is; nothing was archived",
        named.join(", ") + if named.len() == 1 { " is" } else { " are" }
    )))
}

/// `pm unarchive AGT-N` (AC3): clears `archived_at`.
pub fn unarchive(ctx: &Ctx<'_>, reference: &str) -> Result<()> {
    let actor = ctx.actor()?;
    let (mut store, ws) = ctx.open()?;
    let ticket = find(&store, &ws, reference)?;
    let mut stamper = Stamper::new(&store, actor)?;
    let op = stamper.op(ticket.id, Payload::FieldSet(FieldSet::ArchivedAt(None)));
    store.commit(&op)?;
    print_ticket(ctx, &store, &ws, ticket.id)
}

fn print_ticket(ctx: &Ctx<'_>, store: &Store, ws: &Workspace, id: Ulid) -> Result<()> {
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

// ------------------------------------------------------------------- --auto

/// `pm archive --auto [--dry-run]` (AC1): archives every completed,
/// not-yet-archived ticket whose completion month is before this one, then
/// retires every `in-progress` project left with zero non-archived tickets
/// and no recent doc edit. Tickets are processed first so a project's
/// "zero non-archived tickets" check already reflects this run's own
/// archiving — in `--dry-run`, which commits nothing, the same tickets are
/// simply subtracted from the live count instead, so a dry run reports
/// exactly what a real run would do.
fn archive_auto(ctx: &Ctx<'_>, dry_run: bool) -> Result<()> {
    let (mut store, ws) = ctx.open()?;
    let now_ms = crate::verbs::now_ms();

    let mut ticket_targets: Vec<(Ticket, Hlc)> = Vec::new();
    for t in store.tickets(&TicketFilter {
        archived: true,
        ..Default::default()
    })? {
        if t.archived_at.is_some() {
            continue; // already archived
        }
        let completed = ws
            .state(&t.state)
            .is_some_and(|s| s.category == StateCategory::Completed);
        if !completed {
            continue;
        }
        // The state field's own LWW stamp is the completion time; fall
        // back to `updated` only if the view is somehow missing (it can't
        // be, for a ticket that loaded at all, but this keeps the read
        // total rather than a hard error over a defensive edge case).
        let completed_at = store
            .ticket_view(t.id)?
            .and_then(|v| v.state.stamp)
            .map(|s| s.hlc)
            .unwrap_or(t.updated);
        if pm_core::ticket_archivable(completed_at.wall_ms, now_ms) {
            ticket_targets.push((t, completed_at));
        }
    }

    if !dry_run && !ticket_targets.is_empty() {
        let actor = ctx.actor()?;
        let mut stamper = Stamper::new(&store, actor)?;
        for (t, _) in &ticket_targets {
            let op = stamper.op_with_hlc(t.id, |hlc| {
                Payload::FieldSet(FieldSet::ArchivedAt(Some(hlc)))
            });
            store.commit(&op)?;
        }
    }
    let archived_this_run: BTreeSet<Ulid> = ticket_targets.iter().map(|(t, _)| t.id).collect();

    let mut project_targets: Vec<Project> = Vec::new();
    for p in store.projects()? {
        if p.status != ProjectStatus::InProgress {
            continue; // already complete or abandoned, or parked (AGT-1635)
        }
        let live = store.tickets(&TicketFilter {
            project: vec![p.id.clone()],
            ..Default::default()
        })?;
        // `--auto` (not dry-run) already committed this run's archives
        // above, so `live` (default `archived: false`) reflects them; in
        // `--dry-run` nothing was committed, so subtract them here instead.
        let non_archived = if dry_run {
            live.iter()
                .filter(|t| !archived_this_run.contains(&t.id))
                .count() as u64
        } else {
            live.len() as u64
        };
        let doc_last_edit = store.project_doc_last_edit(&p.id)?.map(|h| h.wall_ms);
        if pm_core::project_idle(non_archived, doc_last_edit, now_ms, ws.stale_days) {
            project_targets.push(p);
        }
    }

    if !dry_run && !project_targets.is_empty() {
        let actor = ctx.actor()?;
        for p in &project_targets {
            store.set_project_status(&p.id, ProjectStatus::Complete, &actor)?;
        }
    }

    print_auto_result(ctx, &ws, dry_run, &ticket_targets, &project_targets)
}

fn print_auto_result(
    ctx: &Ctx<'_>,
    ws: &Workspace,
    dry_run: bool,
    tickets: &[(Ticket, Hlc)],
    projects: &[Project],
) -> Result<()> {
    if ctx.json {
        let out = json!({
            "schema": SCHEMA,
            "dry_run": dry_run,
            "archived_tickets": tickets.iter().map(|(t, _)| Value::String(ref_id(ws, t))).collect::<Vec<_>>(),
            "completed_projects": projects.iter().map(|p| Value::String(p.id.clone())).collect::<Vec<_>>(),
        });
        print_json(&out);
        return Ok(());
    }
    let ticket_verb = if dry_run { "would archive" } else { "archived" };
    if tickets.is_empty() {
        println!("{ticket_verb}: none");
    } else {
        let ids: Vec<String> = tickets.iter().map(|(t, _)| ref_id(ws, t)).collect();
        println!("{ticket_verb}: {}", ids.join(", "));
    }
    let project_verb = if dry_run {
        "would complete project"
    } else {
        "completed project"
    };
    if projects.is_empty() {
        println!("{project_verb}: none");
    } else {
        let ids: Vec<&str> = projects.iter().map(|p| p.id.as_str()).collect();
        println!("{project_verb}: {}", crate::text::inline(&ids.join(", ")));
    }
    Ok(())
}
