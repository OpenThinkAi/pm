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
use crate::verbs::{Ctx, SCHEMA, Stamper, display_id, find, print_json, ticket_json};

/// `pm archive AGT-N` or `pm archive --auto [--dry-run]`; clap's
/// `conflicts_with`/`requires` rule out the other two combinations
/// (`id` with `--auto`, `--dry-run` without `--auto`).
pub fn archive(ctx: &Ctx<'_>, id: Option<String>, auto: bool, dry_run: bool) -> Result<()> {
    match (id, auto) {
        (Some(id), false) => archive_one(ctx, &id),
        (None, true) => archive_auto(ctx, dry_run),
        (None, false) => Err(CliError::usage("pm archive requires an id or --auto")),
        (Some(_), true) => unreachable!("clap's conflicts_with rules this out"),
    }
}

// ------------------------------------------------------------ single ticket

/// `pm archive AGT-N` (AC3): sets `archived_at` to this op's own HLC,
/// regardless of the ticket's current state — an explicit request, unlike
/// `--auto`, which only ever touches completed tickets.
fn archive_one(ctx: &Ctx<'_>, reference: &str) -> Result<()> {
    let actor = ctx.actor()?;
    let (mut store, ws) = ctx.open()?;
    let ticket = find(&store, &ws, reference)?;
    let mut stamper = Stamper::new(&store, actor)?;
    let op = stamper.op_with_hlc(ticket.id, |hlc| {
        Payload::FieldSet(FieldSet::ArchivedAt(Some(hlc)))
    });
    store.commit(&op)?;
    print_ticket(ctx, &store, &ws, ticket.id)
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
            continue; // already complete or abandoned
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

    if !dry_run {
        for p in &project_targets {
            store.set_project_status(&p.id, ProjectStatus::Complete)?;
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
            "archived_tickets": tickets.iter().map(|(t, _)| Value::String(display_id(ws, t))).collect::<Vec<_>>(),
            "completed_projects": projects.iter().map(|p| Value::String(p.id.clone())).collect::<Vec<_>>(),
        });
        print_json(&out);
        return Ok(());
    }
    let ticket_verb = if dry_run { "would archive" } else { "archived" };
    if tickets.is_empty() {
        println!("{ticket_verb}: none");
    } else {
        let ids: Vec<String> = tickets.iter().map(|(t, _)| display_id(ws, t)).collect();
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
        println!("{project_verb}: {}", ids.join(", "));
    }
    Ok(())
}
