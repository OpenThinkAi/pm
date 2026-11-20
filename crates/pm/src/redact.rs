//! `pm redact <ID>` and `pm project redact <ID>` (AGT-2427): erase text
//! for good.
//!
//! Each command picks the ops whose text to erase — a ticket's titles,
//! description edits, comments or hold reasons, or every edit of a project
//! document — and commits one `redact` op naming them
//! ([`pm_core::op::Redact`]). The store rewrites those ops in place
//! (keeping their id, stamp, actor and kind) and refolds the ticket or
//! document, so the text is gone from this replica's log and tables at
//! once; `pm sync` carries the redact to the hub, which rewrites its copy,
//! and from there to every other replica. What a redact cannot reach
//! (backups' git history, exports already written, replicas still on a
//! build before 0.4.0) is spelled out in `docs/data-retention.md`.

use pm_core::op::Redact;
use pm_core::{MAX_REDACT_TARGETS, Op, Payload};
use serde_json::{Value, json};
use ulid::Ulid;

use crate::exit::{CliError, Result};
use crate::verbs::{Ctx, SCHEMA, Stamper, display_id, find, print_json, ticket_json};

/// What `pm redact` erases; at least one is required.
#[derive(Debug, Default)]
pub struct Selection {
    pub title: bool,
    pub description: bool,
    /// 1-based, oldest first, as `pm show` lists comments.
    pub comments: Vec<usize>,
    pub hold: bool,
    pub ops: Vec<Ulid>,
    pub all: bool,
}

impl Selection {
    fn is_empty(&self) -> bool {
        !self.title
            && !self.description
            && self.comments.is_empty()
            && !self.hold
            && self.ops.is_empty()
            && !self.all
    }
}

/// Printed (text mode) after a redact lands, so nobody mistakes "gone
/// here" for "gone everywhere".
const REACH_NOTE: &str = "pm: erased from this replica; `pm sync` carries the redaction to the hub \
     and other replicas (a replica on pm before 0.4.0 keeps its copy until it upgrades). Backups' \
     git history and files already exported keep the text: see docs/data-retention.md";

pub fn ticket(ctx: &Ctx<'_>, reference: &str, sel: &Selection) -> Result<()> {
    if sel.is_empty() {
        return Err(CliError::usage(
            "name what to redact: --title, --description, --comment <N>, --hold, --op <OP-ID>, \
             or --all",
        ));
    }
    let actor = ctx.actor()?;
    let (mut store, ws) = ctx.open()?;
    let ticket = find(&store, &ws, reference)?;
    let ops = store.ops(ticket.id)?;
    let shown = display_id(&ws, &ticket);

    let mut targets: Vec<Ulid> = Vec::new();
    let mut pick = |op: &Op| {
        if !targets.contains(&op.op_id) {
            targets.push(op.op_id);
        }
    };
    for op in &ops {
        let wanted = match &op.payload {
            Payload::TicketCreate(_) | Payload::FieldSet(pm_core::op::FieldSet::Title(_)) => {
                sel.title || sel.all
            }
            Payload::BodyEdit(_) => sel.description || sel.all,
            Payload::CommentAdd(_) => sel.all,
            Payload::HoldSet(_) => sel.hold || sel.all,
            _ => false,
        };
        if wanted {
            pick(op);
        }
    }
    if !sel.comments.is_empty() {
        let comments = store.comments(ticket.id)?;
        for n in &sel.comments {
            let comment = n
                .checked_sub(1)
                .and_then(|i| comments.get(i))
                .ok_or_else(|| {
                    CliError::not_found(format!(
                        "{shown} has no comment {n} (it has {}; they are numbered from 1, oldest \
                         first, as `pm show` lists them)",
                        comments.len()
                    ))
                })?;
            let op = ops
                .iter()
                .find(|o| o.op_id == comment.id)
                .ok_or_else(|| CliError::error(format!("comment {n} has no op in the log")))?;
            pick(op);
        }
    }
    for id in &sel.ops {
        let op = ops.iter().find(|o| o.op_id == *id).ok_or_else(|| {
            CliError::usage(format!(
                "op {id} is not an op of {shown} (see `pm log {shown}`)"
            ))
        })?;
        if op.redacted().is_none() {
            return Err(CliError::usage(format!(
                "op {id} is a {} op, which carries no text to redact (only ticket.create and \
                 field.set title, body.edit, comment.add and hold.set do)",
                op.kind()
            )));
        }
        pick(op);
    }
    let fresh = still_readable(&ops, &targets);
    let mut stamper = Stamper::new(&store, actor)?;
    let redacts = redact_ops(&mut stamper, ticket.id, &fresh);
    if !redacts.is_empty() {
        store.commit_batch(&redacts, &[])?;
        store.scrub_wal()?;
    }
    let ticket = store
        .ticket(ticket.id)?
        .ok_or_else(|| CliError::error(format!("ticket {shown} vanished after mutation")))?;
    if ctx.json {
        let mut out = ticket_json(&ws, &store, &ticket)?;
        out["redacted"] = json!(fresh.len());
        print_json(&out);
    } else {
        report(&shown, fresh.len());
    }
    Ok(())
}

pub fn project_doc(ctx: &Ctx<'_>, project: &str, doc: Option<&str>) -> Result<()> {
    let actor = ctx.actor()?;
    let (mut store, _ws) = ctx.open()?;
    store
        .project(project)?
        .ok_or_else(|| CliError::not_found(format!("no project '{project}'")))?;
    let (doc_id, name) = match doc {
        None => (store.design_doc_id(project)?, "design".to_string()),
        Some(name) => (store.named_doc_id(project, name)?, name.to_string()),
    };
    let doc_id = doc_id.ok_or_else(|| {
        CliError::not_found(format!("project '{project}' has no document '{name}'"))
    })?;
    let ops = store.ops(doc_id)?;
    let edits: Vec<Ulid> = ops
        .iter()
        .filter(|o| matches!(o.payload, Payload::BodyEdit(_)))
        .map(|o| o.op_id)
        .collect();
    let fresh = still_readable(&ops, &edits);
    let mut stamper = Stamper::new(&store, actor)?;
    let redacts = redact_ops(&mut stamper, doc_id, &fresh);
    if !redacts.is_empty() {
        store.commit_batch(&redacts, &[])?;
        store.scrub_wal()?;
    }
    if ctx.json {
        let out: Value = json!({
            "schema": SCHEMA,
            "project": project,
            "doc": name,
            "redacted": fresh.len(),
        });
        print_json(&out);
    } else {
        report(&format!("{project} {name}"), fresh.len());
    }
    Ok(())
}

/// Of `targets`, those whose text is still there (an op already redacted
/// gives nothing more to erase), in log order.
fn still_readable(ops: &[Op], targets: &[Ulid]) -> Vec<Ulid> {
    ops.iter()
        .filter(|o| targets.contains(&o.op_id))
        .filter(|o| o.redacted().is_some_and(|r| r != **o))
        .map(|o| o.op_id)
        .collect()
}

/// One `redact` op per [`MAX_REDACT_TARGETS`] targets.
fn redact_ops(stamper: &mut Stamper, entity: Ulid, targets: &[Ulid]) -> Vec<Op> {
    targets
        .chunks(MAX_REDACT_TARGETS)
        .map(|chunk| {
            stamper.op(
                entity,
                Payload::Redact(Redact {
                    targets: chunk.to_vec(),
                }),
            )
        })
        .collect()
}

fn report(what: &str, n: usize) {
    let what = crate::text::inline(what);
    match n {
        0 => println!("{what}: nothing left to redact"),
        1 => println!("{what}: redacted 1 op"),
        n => println!("{what}: redacted {n} ops"),
    }
    if n > 0 {
        eprintln!("{REACH_NOTE}");
    }
}
