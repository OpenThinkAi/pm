//! Redaction (AGT-2427): erasing the text of earlier ops.
//!
//! A `redact` op ([`pm_core::op::Redact`]) names ops of its own entity.
//! Every ingest path treats it the same way:
//!
//! - **The op log.** Each target already in the log has its `payload`
//!   rewritten with [`Op::redacted`] — op id, stamp, actor, entity, kind
//!   and version stay, so every merge rule and replay still works on it.
//!   A target that arrives *after* its redact (a pull, a restore) is
//!   redacted before it is folded or appended ([`effective`]), so its text
//!   never reaches a table. Either order ends in the same log and tables.
//! - **The materialized rows.** The entity is refolded from its (now
//!   redacted) ops ([`rebuild_entity`]): a ticket's row, view, comments
//!   and markers through [`crate::commit::replay_in`], a project
//!   document's text and view through the same fold `pm doctor` uses — the
//!   code that produced the rows in the first place, so `pm doctor` stays
//!   clean.
//! - **The sync quarantine.** A target parked or refused there has its
//!   kept content redacted too.
//!
//! A redact folds into no view itself and `pm doctor`'s replay skips it;
//! the replay re-applies every redact to the log first
//! ([`reapply_all`]), so a log holding a target's original text shows up
//! as unhealthy rather than silently persisting.
//!
//! Who may redact what is the hub's call (it refuses an actor-bound
//! token's redact of another actor's op, `docs/hub-api.md`); a replica
//! applies every redact the hub admitted, as it applies every other op.

use std::borrow::Cow;

use pm_core::{DocView, Op, Payload, Ticket, apply_doc};
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use ulid::Ulid;

use crate::commit::{
    Origin, append_op, check_ingest, ensure_actor, exists, payload_text, replay_in,
};
use crate::error::{Result, StoreError};
use crate::query::read_ops;

/// `op` as the log must hold it: [redacted](Op::redacted) when a `redact`
/// of the same entity already in the log names it, else unchanged.
pub(crate) fn effective<'a>(conn: &Connection, op: &'a Op) -> Result<Cow<'a, Op>> {
    if matches!(op.payload, Payload::Redact(_)) || !is_target(conn, op.entity, op.op_id)? {
        return Ok(Cow::Borrowed(op));
    }
    Ok(op.redacted().map_or(Cow::Borrowed(op), Cow::Owned))
}

/// Whether a `redact` of `entity` in the log names `op_id`.
fn is_target(conn: &Connection, entity: Ulid, op_id: Ulid) -> Result<bool> {
    Ok(conn.query_row(
        "SELECT EXISTS (
             SELECT 1 FROM ops, json_each(ops.payload, '$.targets') AS t
             WHERE ops.entity = ?1 AND ops.kind = 'redact' AND t.value = ?2)",
        params![entity.to_string(), op_id.to_string()],
        |r| r.get(0),
    )?)
}

/// Commits a `redact` op: the ingest checks every op passes, append, then
/// [`apply_in`]. Returns the ticket as it now reads when the entity is
/// a ticket.
pub(crate) fn commit_in(tx: &Transaction<'_>, op: &Op, origin: Origin) -> Result<Option<Ticket>> {
    check_ingest(op, origin)?;
    ensure_actor(tx, &op.actor)?;
    append_op(tx, op)?;
    apply_in(tx, op)
}

/// Redacts every target of `redact` in the log and the quarantine, and
/// refolds its entity.
fn apply_in(tx: &Transaction<'_>, redact: &Op) -> Result<Option<Ticket>> {
    rewrite_targets(tx, redact)?;
    rewrite_quarantined(tx, redact)?;
    rebuild_entity(tx, redact.entity)
}

/// The targets of `redact` (empty for any other kind).
fn targets(redact: &Op) -> &[Ulid] {
    match &redact.payload {
        Payload::Redact(r) => &r.targets,
        _ => &[],
    }
}

/// Rewrites the payload of each of `redact`'s targets in the log that
/// is an op of its entity and not redacted yet; returns how many changed.
fn rewrite_targets(tx: &Transaction<'_>, redact: &Op) -> Result<usize> {
    let mut changed = 0;
    for target in targets(redact) {
        let found = read_ops(
            tx,
            "WHERE op_id = ?1 AND entity = ?2",
            params![target.to_string(), redact.entity.to_string()],
        )?;
        for (seq, op) in found {
            let Some(blank) = op.redacted() else { continue };
            if blank == op {
                continue;
            }
            tx.execute(
                "UPDATE ops SET payload = ?1 WHERE seq = ?2",
                params![payload_text(&blank), seq],
            )?;
            changed += 1;
        }
    }
    Ok(changed)
}

/// Redacts the kept content of each target parked or refused in
/// `sync_quarantine` (a pruned row keeps none).
fn rewrite_quarantined(tx: &Transaction<'_>, redact: &Op) -> Result<()> {
    for target in targets(redact) {
        let kept: Option<String> = tx
            .query_row(
                "SELECT op FROM sync_quarantine WHERE op_id = ?1 AND entity = ?2 AND op <> ''",
                params![target.to_string(), redact.entity.to_string()],
                |r| r.get(0),
            )
            .optional()?;
        let Some(text) = kept else { continue };
        let op: Op = serde_json::from_str(&text)
            .map_err(|e| StoreError::corrupt("sync_quarantine.op")(&e))?;
        if let Some(blank) = op.redacted() {
            tx.execute(
                "UPDATE sync_quarantine SET op = ?1 WHERE op_id = ?2",
                params![
                    serde_json::to_string(&blank).expect("an op serializes"),
                    target.to_string()
                ],
            )?;
        }
    }
    Ok(())
}

/// Refolds every row derived from `entity`'s ops: a ticket (an entity with
/// a `ticket.create`, the rule `pm doctor` uses) from all its ops in log
/// order, a bound project document from its `body.edit` ops, anything
/// else (a redact that arrived before its entity) not at all. Returns the
/// ticket as it now reads, for a ticket.
fn rebuild_entity(tx: &Transaction<'_>, entity: Ulid) -> Result<Option<Ticket>> {
    let id = entity.to_string();
    let is_ticket = exists(
        tx,
        "SELECT 1 FROM ops WHERE entity = ?1 AND kind = 'ticket.create'",
        &id,
    )?;
    if is_ticket {
        for table in ["ticket_label", "comment", "marker", "ticket_view"] {
            tx.execute(
                &format!("DELETE FROM {table} WHERE ticket = ?1"),
                params![id],
            )?;
        }
        tx.execute("DELETE FROM relation WHERE owner = ?1", params![id])?;
        let mut ticket = None;
        for (seq, op) in read_ops(tx, "WHERE entity = ?1 AND kind <> 'redact'", params![id])? {
            let t = replay_in(tx, &op).map_err(|source| StoreError::Replay {
                seq,
                op_id: op.op_id,
                kind: op.kind(),
                source: Box::new(source),
            })?;
            ticket = Some(t);
        }
        return Ok(ticket);
    }
    if crate::project::is_known_doc(tx, entity)? {
        tx.execute(
            "DELETE FROM project_doc_view WHERE doc_id = ?1",
            params![id],
        )?;
        if !crate::project::doc_has_row(tx, entity)? {
            return Ok(None);
        }
        let mut view = DocView::new(entity);
        for (seq, op) in read_ops(tx, "WHERE entity = ?1 AND kind = 'body.edit'", params![id])? {
            apply_doc(&mut view, &op).map_err(|source| StoreError::Replay {
                seq,
                op_id: op.op_id,
                kind: "body.edit",
                source: Box::new(StoreError::DocApply(source)),
            })?;
        }
        crate::project::materialize_doc(tx, &view)?;
    }
    Ok(None)
}

/// Re-applies every `redact` in the log to its targets' payloads, without
/// refolding anything (`pm doctor`'s replay refolds everything next);
/// returns how many payloads still held text they should not have.
pub(crate) fn reapply_all(tx: &Transaction<'_>) -> Result<usize> {
    let mut changed = 0;
    for (_, redact) in read_ops(tx, "WHERE kind = 'redact'", [])? {
        changed += rewrite_targets(tx, &redact)?;
    }
    Ok(changed)
}
