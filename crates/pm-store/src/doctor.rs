//! `pm doctor` (AGT-1337, the P0 exit gate): the op log is the source of
//! truth only if the ticket tables regenerate from it byte-for-byte.
//!
//! [`Store::doctor`] replays the whole log into emptied ticket tables
//! inside a transaction, diffs the result against what was there, and
//! rolls back — so it reports drift without writing. [`Store::rebuild`] is
//! the same replay, committed. Both go through [`crate::commit::replay_in`],
//! the code that materialized the rows in the first place, so a clean
//! table and a rebuilt table are produced identically.
//!
//! Three replays, one diff, in dependency order:
//! 1. Config (AGT-1385, [`crate::config::replay_config`]): the
//!    [`CONFIG_TABLES`] — the `workspace` row (all but `number_floor`),
//!    `state`, `actor`, a project's metadata columns and document identity
//!    (`project.doc_id`, the `project_doc` rows, `project_doc_owner`;
//!    AGT-1413) and the two view tables — from the config ops. `state`,
//!    `workspace_view`, `project_view`, `project_doc` and
//!    `project_doc_owner` are emptied first; the `workspace`, `actor` and
//!    `project` rows are rewritten in place (`ops.actor` references
//!    `actor`, and tickets `project`).
//! 2. Tickets: [`TICKET_TABLES`] emptied and refilled, as before.
//! 3. Project document bodies (AGT-1344, `project.rs`): `project.doc`,
//!    `project_doc.body` and `project_doc_view` from `body.edit` ops the
//!    same way `ticket.description` is — scoped to rows with a `doc_id`.
//!
//! What a rebuild never touches: `workspace.number_floor`,
//! `backup_target`, `sync_*`, `pending_number` — bookkeeping, not derived
//! state.

use std::collections::{BTreeMap, BTreeSet};

use rusqlite::types::Value as Sql;
use rusqlite::{Connection, Transaction, TransactionBehavior, params};
use serde::Serialize;
use serde_json::{Map, Value};
use ulid::Ulid;

use crate::Store;
use crate::codec::ulid;
use crate::commit::replay_in;
use crate::error::{Result, StoreError};
use crate::query::read_ops;
use crate::sync::{SyncStatus, sync_status};

/// The tables derived from `ops`, parents before children — the reverse
/// is the order they are emptied in.
pub const TICKET_TABLES: [&str; 6] = [
    "ticket",
    "ticket_view",
    "ticket_label",
    "relation",
    "comment",
    "marker",
];

/// The tables a project document's `body.edit` ops touch (AGT-1344):
/// `project` and `project_doc` for their cached `doc`/`body` text, and
/// `project_doc_view` (the document analogue of `ticket_view`), which is
/// fully derived. [`crate::project::replay_project_docs`] only ever writes
/// rows with a `doc_id`.
pub const PROJECT_DOC_TABLES: [&str; 3] = ["project", "project_doc", "project_doc_view"];

/// The tables the config ops materialize (AGT-1385): the workspace row
/// and its merge state, states, actors, project metadata and document
/// identity (AGT-1413; the `project` and `project_doc` tables are shared
/// with [`PROJECT_DOC_TABLES`]: their metadata and `doc_id` columns are
/// config-derived, their text columns document-derived), every bound
/// document id, and the project merge state.
pub const CONFIG_TABLES: [&str; 8] = [
    "workspace",
    "workspace_view",
    "state",
    "actor",
    "project",
    "project_doc",
    "project_doc_owner",
    "project_view",
];

/// Every op-derived table, each once, in the order a diff lists them.
fn derived_tables() -> Vec<&'static str> {
    let mut tables: Vec<&'static str> = Vec::new();
    for table in CONFIG_TABLES
        .iter()
        .chain(TICKET_TABLES.iter())
        .chain(PROJECT_DOC_TABLES.iter())
    {
        if !tables.contains(table) {
            tables.push(table);
        }
    }
    tables
}

/// What `pm doctor` found. Healthy when [`Report::is_healthy`].
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Report {
    pub schema_version: u32,
    pub op_count: u64,
    /// Row count of every table, by name.
    pub tables: BTreeMap<String, u64>,
    /// `PRAGMA integrity_check` messages (index, NOT NULL, CHECK and
    /// UNIQUE problems); empty when it said `ok`.
    pub integrity: Vec<String>,
    /// `PRAGMA foreign_key_check` rows.
    pub foreign_keys: Vec<ForeignKeyViolation>,
    /// Why the log could not be replayed, if it could not. Drift is
    /// unknown in that case.
    pub replay_error: Option<String>,
    /// Live derived tables (`before`) against what the log produces
    /// (`after`). Empty when they match.
    pub drift: Diff,
    /// Client sync state (AGT-1393): outbox size, pull cursor, tickets
    /// awaiting a hub number. Informational — never affects health.
    pub sync: SyncStatus,
}

impl Report {
    pub fn is_healthy(&self) -> bool {
        self.integrity.is_empty()
            && self.foreign_keys.is_empty()
            && self.replay_error.is_none()
            && self.drift.is_empty()
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ForeignKeyViolation {
    pub table: String,
    pub rowid: Option<i64>,
    pub parent: String,
    /// Index of the failing foreign key in `PRAGMA foreign_key_list`.
    pub fk_index: i64,
}

/// Row-level differences between two states of the ticket tables. Only
/// tables with differences appear.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct Diff {
    pub tables: Vec<TableDiff>,
}

impl Diff {
    pub fn is_empty(&self) -> bool {
        self.tables.is_empty()
    }

    pub fn row_count(&self) -> usize {
        self.tables
            .iter()
            .map(|t| t.missing.len() + t.extra.len() + t.changed.len())
            .sum()
    }
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct TableDiff {
    pub table: String,
    /// Rows the log produces that `before` lacked.
    pub missing: Vec<Row>,
    /// Rows in `before` that the log does not produce.
    pub extra: Vec<Row>,
    /// Rows present in both with different column values.
    pub changed: Vec<RowChange>,
}

/// One row: its primary key and every column, as JSON.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Row {
    pub key: Vec<Value>,
    pub columns: Map<String, Value>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct RowChange {
    pub key: Vec<Value>,
    /// Only the columns that differ.
    pub columns: Vec<ColumnChange>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ColumnChange {
    pub column: String,
    pub before: Value,
    pub after: Value,
}

impl Store {
    /// Checks the database without changing it: schema version, op and
    /// row counts, SQLite's integrity and foreign-key checks, and whether
    /// replaying the log reproduces the derived tables (config, ticket
    /// and project-document). The replay runs in a transaction that is
    /// always rolled back.
    pub fn doctor(&mut self) -> Result<Report> {
        let schema_version = self.schema_version()?;
        let op_count = count(&self.conn, "ops")?;
        let tables = table_counts(&self.conn)?;
        let integrity = integrity_check(&self.conn)?;
        let foreign_keys = foreign_key_check(&self.conn)?;
        let sync = sync_status(&self.conn)?;

        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let replayed = replay_all(&tx);
        tx.rollback()?;
        let (drift, replay_error) = match replayed {
            Ok(diff) => (diff, None),
            Err(e) => (Diff::default(), Some(e.to_string())),
        };
        Ok(Report {
            schema_version,
            op_count,
            tables,
            integrity,
            foreign_keys,
            replay_error,
            drift,
            sync,
        })
    }

    /// Regenerates the derived tables from the log, in one transaction;
    /// returns what changed (`before` = the old rows, `after` = the
    /// rebuilt ones). A replay failure rolls everything back and names
    /// the op.
    pub fn rebuild(&mut self) -> Result<Diff> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let diff = replay_all(&tx)?;
        tx.commit()?;
        Ok(diff)
    }
}

/// Every entity a `ticket.create` op has ever named, straight from the
/// log — never affected by a row's current state (a ticket's tombstone,
/// a project document's deletion), unlike a materialized-table lookup.
fn ticket_create_entities(tx: &Transaction<'_>) -> Result<BTreeSet<Ulid>> {
    let mut stmt = tx.prepare("SELECT DISTINCT entity FROM ops WHERE kind = ?1")?;
    let rows = stmt.query_map(params!["ticket.create"], |r| r.get::<_, String>(0))?;
    rows.map(|row| ulid("ops.entity", &row?)).collect()
}

fn replay_all(tx: &Transaction<'_>) -> Result<Diff> {
    let tables = derived_tables();
    let before = snapshot(tx, &tables)?;
    for table in TICKET_TABLES.iter().rev() {
        tx.execute(&format!("DELETE FROM {table}"), [])?;
    }
    // Config first (AGT-1385): the ticket replay needs the states and
    // projects it produces. `state` can only be emptied once no ticket
    // references it; `workspace`, `actor` and `project` rows are rewritten
    // in place (module docs). A project's documents are rebound from its
    // view (AGT-1413); their text is refilled by the document replay.
    for table in [
        "state",
        "workspace_view",
        "project_view",
        "project_doc",
        "project_doc_owner",
    ] {
        tx.execute(&format!("DELETE FROM {table}"), [])?;
    }
    crate::config::replay_config(tx)?;
    // The op log shares one `entity` namespace between tickets and project
    // documents (AGT-1344): a `body.edit` whose entity is a document, not
    // a ticket, belongs to `crate::project::replay_project_docs` below,
    // not here — replaying it as a ticket op would report `UnknownTicket`.
    //
    // Told apart structurally, not by `known_doc_ids` (live doc_ids on
    // `project`/`project_doc` right now): only a ticket ever gets a
    // `ticket.create` op, so an entity with none is never a ticket. This
    // stays correct after `Store::delete_project` removes a document's
    // doc_id from those rows — its old `body.edit` ops are still in the
    // log (never pruned) but no longer "known"; `known_doc_ids` would
    // wrongly let them fall through to `replay_in` here. A ticket-create
    // check has no such blind spot: it depends only on the log itself.
    let ticket_entities = ticket_create_entities(tx)?;
    for (seq, op) in read_ops(tx, "", [])? {
        if op.payload.is_config() {
            continue; // replayed above
        }
        if op.kind() == "body.edit" && !ticket_entities.contains(&op.entity) {
            continue;
        }
        replay_in(tx, &op).map_err(|source| StoreError::Replay {
            seq,
            op_id: op.op_id,
            kind: op.kind(),
            source: Box::new(source),
        })?;
    }
    // AGT-1344: a project document's body is derived from `body.edit` ops
    // the same way a ticket's description is.
    crate::project::replay_project_docs(tx)?;

    let after = snapshot(tx, &tables)?;
    Ok(diff(before, after))
}

/// Every row of `tables`, keyed by primary key.
type Snapshot = Vec<(&'static str, BTreeMap<String, Row>)>;

fn snapshot(conn: &Connection, tables: &[&'static str]) -> Result<Snapshot> {
    tables
        .iter()
        .map(|table| Ok((*table, rows(conn, table)?)))
        .collect()
}

fn rows(conn: &Connection, table: &str) -> Result<BTreeMap<String, Row>> {
    // `PRAGMA table_info`: (cid, name, type, notnull, dflt_value, pk),
    // where pk is the column's 1-based position in the primary key.
    let mut info = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let mut columns: Vec<(String, i64)> = info
        .query_map([], |r| Ok((r.get::<_, String>(1)?, r.get::<_, i64>(5)?)))?
        .collect::<rusqlite::Result<_>>()?;
    let names: Vec<String> = columns.iter().map(|(name, _)| name.clone()).collect();
    columns.retain(|(_, pk)| *pk > 0);
    columns.sort_by_key(|(_, pk)| *pk);
    let key_columns: Vec<&str> = columns.iter().map(|(name, _)| name.as_str()).collect();

    let mut stmt = conn.prepare(&format!("SELECT {} FROM {table}", names.join(", ")))?;
    let rows = stmt.query_map([], |r| {
        let mut columns = Map::new();
        for (i, name) in names.iter().enumerate() {
            columns.insert(name.clone(), json_cell(r.get::<_, Sql>(i)?));
        }
        let key: Vec<Value> = key_columns.iter().map(|c| columns[*c].clone()).collect();
        Ok(Row { key, columns })
    })?;
    rows.map(|row| {
        let row = row?;
        Ok((Value::Array(row.key.clone()).to_string(), row))
    })
    .collect()
}

/// A cell as JSON. Text and blobs compare byte-for-byte; a blob (none in
/// schema v1) is shown as hex.
fn json_cell(value: Sql) -> Value {
    match value {
        Sql::Null => Value::Null,
        Sql::Integer(i) => Value::from(i),
        Sql::Real(f) => Value::from(f),
        Sql::Text(s) => Value::String(s),
        Sql::Blob(b) => Value::String(b.iter().map(|byte| format!("{byte:02x}")).collect()),
    }
}

fn diff(before: Snapshot, after: Snapshot) -> Diff {
    let mut tables = Vec::new();
    for ((table, mut before), (_, after)) in before.into_iter().zip(after) {
        let mut missing = Vec::new();
        let mut changed = Vec::new();
        for (key, after_row) in after {
            match before.remove(&key) {
                None => missing.push(after_row),
                Some(before_row) if before_row == after_row => {}
                Some(before_row) => changed.push(RowChange {
                    key: after_row.key.clone(),
                    columns: after_row
                        .columns
                        .iter()
                        .filter(|(name, value)| before_row.columns.get(*name) != Some(value))
                        .map(|(name, value)| ColumnChange {
                            column: name.clone(),
                            before: before_row.columns[name].clone(),
                            after: value.clone(),
                        })
                        .collect(),
                }),
            }
        }
        let extra: Vec<Row> = before.into_values().collect();
        if !(missing.is_empty() && extra.is_empty() && changed.is_empty()) {
            tables.push(TableDiff {
                table: table.to_string(),
                missing,
                extra,
                changed,
            });
        }
    }
    Diff { tables }
}

fn table_counts(conn: &Connection) -> Result<BTreeMap<String, u64>> {
    let mut stmt = conn.prepare(
        "SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%'",
    )?;
    let names: Vec<String> = stmt
        .query_map([], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    names
        .into_iter()
        .map(|name| {
            let rows = count(conn, &name)?;
            Ok((name, rows))
        })
        .collect()
}

fn count(conn: &Connection, table: &str) -> Result<u64> {
    let n: i64 = conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))?;
    Ok(n as u64)
}

fn integrity_check(conn: &Connection) -> Result<Vec<String>> {
    let mut stmt = conn.prepare("PRAGMA integrity_check")?;
    let messages: Vec<String> = stmt
        .query_map([], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    Ok(messages.into_iter().filter(|m| m != "ok").collect())
}

fn foreign_key_check(conn: &Connection) -> Result<Vec<ForeignKeyViolation>> {
    let mut stmt = conn.prepare("PRAGMA foreign_key_check")?;
    let violations = stmt
        .query_map([], |r| {
            Ok(ForeignKeyViolation {
                table: r.get(0)?,
                rowid: r.get(1)?,
                parent: r.get(2)?,
                fk_index: r.get(3)?,
            })
        })?
        .collect::<rusqlite::Result<_>>()?;
    Ok(violations)
}
