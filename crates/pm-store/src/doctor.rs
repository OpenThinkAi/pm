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
//! Configuration tables (workspace, state, project, project_doc, actor)
//! are not op-logged (`config.rs`), so a rebuild leaves them alone: only
//! [`TICKET_TABLES`] are emptied and replayed.

use std::collections::BTreeMap;

use rusqlite::types::Value as Sql;
use rusqlite::{Connection, Transaction, TransactionBehavior};
use serde::Serialize;
use serde_json::{Map, Value};

use crate::Store;
use crate::commit::replay_in;
use crate::error::{Result, StoreError};
use crate::query::read_ops;

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
    /// Live ticket tables (`before`) against what the log produces
    /// (`after`). Empty when they match.
    pub drift: Diff,
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
    /// replaying the log reproduces the ticket tables. The replay runs in
    /// a transaction that is always rolled back.
    pub fn doctor(&mut self) -> Result<Report> {
        let schema_version = self.schema_version()?;
        let op_count = count(&self.conn, "ops")?;
        let tables = table_counts(&self.conn)?;
        let integrity = integrity_check(&self.conn)?;
        let foreign_keys = foreign_key_check(&self.conn)?;

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
        })
    }

    /// Empties the ticket tables and replays the log into them, in one
    /// transaction; returns what changed (`before` = the old rows,
    /// `after` = the rebuilt ones). A replay failure rolls everything
    /// back and names the op.
    pub fn rebuild(&mut self) -> Result<Diff> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let diff = replay_all(&tx)?;
        tx.commit()?;
        Ok(diff)
    }
}

fn replay_all(tx: &Transaction<'_>) -> Result<Diff> {
    let before = snapshot(tx)?;
    for table in TICKET_TABLES.iter().rev() {
        tx.execute(&format!("DELETE FROM {table}"), [])?;
    }
    for (seq, op) in read_ops(tx, "", [])? {
        replay_in(tx, &op).map_err(|source| StoreError::Replay {
            seq,
            op_id: op.op_id,
            kind: op.kind(),
            source: Box::new(source),
        })?;
    }
    let after = snapshot(tx)?;
    Ok(diff(before, after))
}

/// Every row of every ticket table, keyed by primary key.
type Snapshot = Vec<(&'static str, BTreeMap<String, Row>)>;

fn snapshot(conn: &Connection) -> Result<Snapshot> {
    TICKET_TABLES
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
