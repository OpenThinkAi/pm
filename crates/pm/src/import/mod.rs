//! `pm import vault <path>` (AGT-1347; projects/pm/README.md P1): a
//! lossless, incremental import of a saltline-style markdown vault —
//! tickets and their archive, projects with their design docs and named
//! documents — into ops.
//!
//! - [`vault`] reads the files: lenient frontmatter, comment entries,
//!   marker migration, the anomalies pm knows about (AGT-806 from git).
//! - [`plan`] turns each ticket into ops: everything for a new ticket,
//!   only the differences for one already in the store (keyed on the
//!   vault id, never the path), stamped with the file's own dates.
//! - This module commits: projects first (R2), then ticket ops in chunks,
//!   then relations (R4), then raises the number floor and gives
//!   duplicate-id tickets a fresh number; `--dry-run` stops before the
//!   first write. Re-running on an unchanged vault commits nothing.
//! - [`report`] prints what happened, including every migrated marker
//!   and every non-template value (AC3, AC6).
//! - [`parity`] (`--report <file>`, AGT-1348) renders every imported
//!   ticket back the way `pm export md --legacy-markers` does and diffs
//!   it against its source file: the round-trip evidence.
//!
//! The vault is only ever read (and `git show`n); pm never writes to it.
//! Project design docs stay owned by the vault until the handover (README
//! A3): what lands here is a snapshot, refreshed by re-importing. Once
//! `pm workspace docs-owned-by pm` flips the workspace setting (AGT-1406)
//! the import still brings project metadata and tickets but leaves every
//! `projects/*/README.md` and sibling document alone, listing them as
//! skipped in the report.

mod parity;
mod plan;
mod prose;
mod report;
mod vault;

use std::path::Path;
use std::time::Instant;

use anyhow::Context;
use pm_core::op::BodyEdit;
use pm_core::{ActorId, Body, Clock, Payload, ProjectStatus};
use pm_store::Store;
use ulid::Ulid;

use self::plan::{IMPORT_ACTOR, Intent, Phase};
use self::report::Report;
use crate::exit::{CliError, Result};
use crate::verbs::Ctx;

/// Ops per transaction. A vault import is thousands of ops; committing
/// them a few hundred at a time keeps each transaction short (other pm
/// invocations wait on the write lock) while a failure still rolls back
/// whole tickets' worth, and a re-run picks up where it left off since
/// already-imported tickets diff to nothing.
const CHUNK: usize = 500;

/// `pm import vault <path> [--dry-run] [--recover PATH=REV]… [--report FILE]`.
pub fn vault(
    ctx: &Ctx<'_>,
    path: &Path,
    dry_run: bool,
    recover: &[String],
    report_path: Option<&Path>,
) -> Result<()> {
    let started = Instant::now();
    let recover: Vec<(String, String)> = recover
        .iter()
        .map(|r| {
            r.split_once('=')
                .filter(|(p, rev)| !p.is_empty() && !rev.is_empty())
                .map(|(p, rev)| (p.to_string(), rev.to_string()))
                .ok_or_else(|| {
                    CliError::usage(format!(
                        "--recover '{r}' is not PATH=REV (e.g. tickets/triage/AGT-123-….md=<sha>)"
                    ))
                })
        })
        .collect::<Result<_>>()?;
    let (mut store, ws) = ctx.open()?;
    let snapshot = vault::read(path, &ws, &recover)?;
    let mut report = Report::new(path.display().to_string(), &ws.prefix, dry_run);
    report.files = snapshot.tickets.len();
    report.archived = snapshot
        .tickets
        .iter()
        .filter(|t| t.archived_month_ms.is_some())
        .count();
    report.non_template = snapshot.findings.non_template.clone();
    report.ext_keys = snapshot.findings.ext_keys.clone();
    report.anomalies = snapshot.findings.anomalies.clone();
    report.projects = snapshot.projects.len();

    // Projects a ticket names that have no README anywhere: created empty
    // so the ticket's `project` (R2) resolves, and reported.
    let known: std::collections::BTreeSet<&str> =
        snapshot.projects.iter().map(|p| p.id.as_str()).collect();
    let mut stubs: Vec<String> = snapshot
        .tickets
        .iter()
        .filter_map(|t| t.project.as_deref())
        .filter(|p| !known.contains(p))
        .map(str::to_string)
        .collect();
    stubs.sort();
    stubs.dedup();
    for stub in &stubs {
        if store.project(stub)?.is_none() {
            report.project_stubs.push(stub.clone());
        }
    }

    let actor = ActorId::new(IMPORT_ACTOR);
    if !dry_run {
        for stub in &report.project_stubs {
            store.upsert_project(
                stub,
                stub,
                ProjectStatus::Abandoned,
                None,
                &Default::default(),
                &actor,
            )?;
        }
        // Two passes so a parent is present whatever the folder order.
        // The first leaves an existing project's parent as it is (a
        // re-import must not emit a `project.set parent` pair per
        // project); the second sets it.
        for p in &snapshot.projects {
            let parent = store.project(&p.id)?.and_then(|c| c.parent);
            store.upsert_project(
                &p.id,
                &p.title,
                p.status,
                parent.as_deref(),
                &p.repos,
                &actor,
            )?;
        }
        for p in snapshot.projects.iter().filter(|p| p.parent.is_some()) {
            let parent = p.parent.as_deref();
            if parent.is_some_and(|parent| !known.contains(parent)) {
                report.anomalies.push(format!(
                    "{}: parent-project '{}' has no README; parent left unset",
                    p.path.display(),
                    parent.unwrap_or_default()
                ));
                continue;
            }
            store.upsert_project(&p.id, &p.title, p.status, parent, &p.repos, &actor)?;
        }
    }

    let plan = plan::build(&store, &ws, &snapshot)?;
    report.tickets = plan.outcome.clone();
    report.anomalies.extend(plan.anomalies.iter().cloned());
    report.migrated = plan.migrated.clone();
    report.changes = plan.changes.clone();
    report.max_number = plan.max_number;
    for m in &plan.migrated {
        let kind = m.split(' ').nth(1).unwrap_or("").trim_end_matches(':');
        *report.markers.entry(kind.to_string()).or_default() += 1;
    }

    // Document bodies: one body.edit per document whose text differs.
    let mut intents = plan.intents;
    let docs_owned_by_pm = ws.docs_owned_by == pm_core::DocsOwner::Pm;
    for p in &snapshot.projects {
        if docs_owned_by_pm {
            report
                .docs
                .skipped
                .push(format!("projects/{}/README.md", p.id));
            report.docs.skipped.extend(
                p.documents
                    .keys()
                    .map(|n| format!("projects/{}/{n}.md", p.id)),
            );
            continue;
        }
        let current = store.project(&p.id)?;
        let mut docs: Vec<(Option<&str>, &str, u64)> = vec![(None, p.doc.as_str(), p.doc_mtime_ms)];
        docs.extend(
            p.documents
                .iter()
                .map(|(n, (t, m))| (Some(n.as_str()), t.as_str(), *m)),
        );
        for (name, text, at_ms) in docs {
            let have = match (&current, name) {
                (Some(c), None) => Some(c.doc.as_str()),
                (Some(c), Some(n)) => c.documents.get(n).map(String::as_str),
                (None, _) => None,
            };
            match have {
                Some(h) if h == text => {
                    report.docs.unchanged += 1;
                    continue;
                }
                Some(h) if !h.is_empty() => report.docs.updated += 1,
                _ => report.docs.created += 1,
            }
            if dry_run {
                continue;
            }
            let doc_id = match name {
                None => store.design_doc_id(&p.id)?.ok_or_else(|| {
                    CliError::error(format!("project '{}' has no design doc id", p.id))
                })?,
                Some(n) => store.ensure_named_doc(&p.id, n, &actor)?,
            };
            let err =
                |e: pm_core::BodyError| CliError::error(format!("building document {}: {e}", p.id));
            let mut body = Body::with_peer(crate::edit::session_peer(Ulid::new())).map_err(err)?;
            if let Some(view) = store.doc_view(doc_id)? {
                body.apply(&view.body.snapshot().map_err(err)?)
                    .map_err(err)?;
            }
            let update = body.diff_from_text(text).map_err(err)?;
            intents.push(Intent {
                entity: doc_id,
                at_ms,
                actor: actor.clone(),
                payload: Payload::BodyEdit(BodyEdit {
                    update: update.into_bytes(),
                }),
                phase: Phase::Ticket,
                dated: false,
            });
        }
    }

    for i in &intents {
        *report
            .ops_by_kind
            .entry(i.payload.kind().to_string())
            .or_default() += 1;
        if matches!(i.payload, Payload::CommentAdd(_)) {
            report.comments += 1;
        }
    }
    report.ops = intents.len();

    if dry_run {
        report.number_floor = store.number_floor()?.max(plan.max_number);
        write_parity(&store, &ws, &snapshot, &mut report, report_path)?;
        report.elapsed_ms = started.elapsed().as_millis();
        report.print(ctx.json);
        return Ok(());
    }

    let mut clock = Clock::from_latest(store.latest_hlc()?);
    let floor_counters = store.max_counters(&plan::dated_days(&intents))?;
    let stamped = plan::stamp(intents, &mut clock, &floor_counters);
    let doc_ids: std::collections::BTreeSet<Ulid> = stamped
        .iter()
        .filter(|(op, _)| store.is_known_doc_id(op.entity).unwrap_or(false))
        .map(|(op, _)| op.entity)
        .collect();
    // Every `ticket.create` goes first: a comment keeps its entry's date
    // even when that is before the ticket's `created` (`plan` module
    // docs), so in stamp order it can precede the create the store needs
    // to have seen. The view folds ops in any order (LWW by stamp;
    // `created` is the create's own stamp), so committing the create
    // ahead of an older-stamped comment reads exactly as the file does.
    let mut creates = Vec::new();
    let mut ticket_ops = Vec::with_capacity(stamped.len());
    let mut relation_ops = Vec::new();
    for (op, phase) in stamped {
        if doc_ids.contains(&op.entity) {
            store.commit_doc_edit(op.entity, &op)?;
        } else if phase == Phase::Relation {
            relation_ops.push(op);
        } else if matches!(op.payload, Payload::TicketCreate(_)) {
            creates.push(op);
        } else {
            ticket_ops.push(op);
        }
    }
    commit_chunks(&mut store, &creates)?;
    commit_chunks(&mut store, &ticket_ops)?;
    commit_chunks(&mut store, &relation_ops)?;

    report.number_floor = store.raise_number_floor(plan.max_number)?;
    for (id, of) in &plan.fresh_numbers {
        let tickets = store.commit_batch(&[], &[(*id, actor.clone())])?;
        if let Some(t) = tickets.first()
            && let Some(n) = t.number
        {
            report
                .renumbered
                .push(format!("{}-{n} (was {of})", ws.prefix));
        }
    }

    write_parity(&store, &ws, &snapshot, &mut report, report_path)?;
    report.elapsed_ms = started.elapsed().as_millis();
    report.print(ctx.json);
    Ok(())
}

/// `--report`: the parity comparison against what is in the store now —
/// after this run's commits, or, with `--dry-run`, whatever an earlier
/// run left there (a file with no ticket in pm is reported as missing).
fn write_parity(
    store: &Store,
    ws: &pm_core::Workspace,
    snapshot: &vault::Snapshot,
    report: &mut Report,
    report_path: Option<&Path>,
) -> Result<()> {
    let Some(path) = report_path else {
        return Ok(());
    };
    let parity = parity::run(
        store,
        ws,
        snapshot,
        &report.anomalies,
        &report.changes,
        &report.non_template,
        path,
    )?;
    std::fs::write(path, &parity.markdown)
        .with_context(|| format!("writing {}", path.display()))?;
    report.parity = Some(parity.summary);
    Ok(())
}

fn commit_chunks(store: &mut Store, ops: &[pm_core::Op]) -> Result<()> {
    for chunk in ops.chunks(CHUNK) {
        store.commit_batch(chunk, &[])?;
    }
    Ok(())
}
