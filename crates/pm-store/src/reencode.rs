//! Migration 0005 (AGT-1378): rewrite every stored byte payload from
//! serde's `[108,111,…]` array form into base64, in place. Runs once,
//! inside the migration's transaction, from [`crate::Store::open`]; the
//! rationale (and why touching the append-only `ops` table is a
//! representation change, not an edit) is in
//! `migrations/0005_compact_bytes.sql`.
//!
//! Every row goes through the same typed round trip `Store::commit` and
//! `pm doctor --rebuild` use — parse with the reader that accepts both
//! spellings, write with the serializer that now emits base64 — so the
//! rewritten text is exactly what a fresh materialization produces.

use pm_core::op::BodyEdit;
use pm_core::{DocView, ProjectView, TicketView, WorkspaceView};
use rusqlite::{Transaction, params};
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::codec::{from_json, json};
use crate::error::Result;

/// Rows rewritten, per table — reported nowhere today, returned so a
/// test can assert the migration did (or did not) touch a row.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Rewritten {
    pub ops: usize,
    pub ticket_views: usize,
    pub project_doc_views: usize,
}

pub(crate) fn run(tx: &Transaction<'_>) -> Result<Rewritten> {
    Ok(Rewritten {
        ops: rewrite::<BodyEdit>(
            tx,
            "SELECT seq, payload FROM ops WHERE kind = 'body.edit' AND payload IS NOT NULL",
            "UPDATE ops SET payload = ?2 WHERE seq = ?1",
            "ops.payload",
        )?,
        ticket_views: rewrite::<TicketView>(
            tx,
            "SELECT ticket, view FROM ticket_view",
            "UPDATE ticket_view SET view = ?2 WHERE ticket = ?1",
            "ticket_view.view",
        )?,
        project_doc_views: rewrite::<DocView>(
            tx,
            "SELECT doc_id, view FROM project_doc_view",
            "UPDATE project_doc_view SET view = ?2 WHERE doc_id = ?1",
            "project_doc_view.view",
        )?,
    })
}

/// Migration 0011 (AGT-1436): re-serialize every stored merge-state view
/// through the current types, so a row written before a view gained a
/// defaulted field (`WorkspaceView.docs_owned_by`, AGT-1406) is rewritten
/// to the text a replay produces today. Re-runnable: a row already in the
/// current form round-trips to itself and is skipped. Returns the rows
/// rewritten.
pub(crate) fn views(tx: &Transaction<'_>) -> Result<usize> {
    Ok(rewrite::<WorkspaceView>(
        tx,
        "SELECT workspace, view FROM workspace_view",
        "UPDATE workspace_view SET view = ?2 WHERE workspace = ?1",
        "workspace_view.view",
    )? + rewrite::<ProjectView>(
        tx,
        "SELECT project, view FROM project_view",
        "UPDATE project_view SET view = ?2 WHERE project = ?1",
        "project_view.view",
    )? + rewrite::<TicketView>(
        tx,
        "SELECT ticket, view FROM ticket_view",
        "UPDATE ticket_view SET view = ?2 WHERE ticket = ?1",
        "ticket_view.view",
    )? + rewrite::<DocView>(
        tx,
        "SELECT doc_id, view FROM project_doc_view",
        "UPDATE project_doc_view SET view = ?2 WHERE doc_id = ?1",
        "project_doc_view.view",
    )?)
}

/// Re-serializes every `(key, text)` row `select_sql` yields through `T`
/// and writes it back with `update_sql` when the text changed. A row
/// already in the current form round-trips to itself and is skipped.
fn rewrite<T: Serialize + DeserializeOwned>(
    tx: &Transaction<'_>,
    select_sql: &str,
    update_sql: &str,
    what: &'static str,
) -> Result<usize> {
    let rows: Vec<(rusqlite::types::Value, String)> = tx
        .prepare(select_sql)?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    let mut rewritten = 0;
    for (key, text) in rows {
        let value: T = from_json(what, &text)?;
        let current = json(&value);
        if current != text {
            tx.execute(update_sql, params![key, current])?;
            rewritten += 1;
        }
    }
    Ok(rewritten)
}
