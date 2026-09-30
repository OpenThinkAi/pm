//! Column encodings: HLCs as two integers, enums by their serde name, ULIDs
//! and JSON as text.

use pm_core::Hlc;
use rusqlite::Row;
use serde::Serialize;
use serde::de::DeserializeOwned;
use ulid::Ulid;

use crate::error::{Result, StoreError};

/// `wall_ms` as stored. HLC wall time is a `u64`; SQLite integers are
/// `i64`, which still covers every millisecond until the year 292 million.
/// A stamp beyond that (only a foreign writer makes one) is a typed
/// [`StoreError::InvalidStamp`], never a panic (oaudit 2026-09-30).
pub(crate) fn wall_ms(hlc: Hlc) -> Result<i64> {
    i64::try_from(hlc.wall_ms).map_err(|_| {
        StoreError::InvalidStamp(pm_core::StampError::WallOutOfRange {
            wall_ms: hlc.wall_ms,
        })
    })
}

pub(crate) fn hlc(row: &Row<'_>, wall_col: &str, counter_col: &str) -> rusqlite::Result<Hlc> {
    let wall: i64 = row.get(wall_col)?;
    let counter: u32 = row.get(counter_col)?;
    Ok(Hlc::new(wall as u64, counter))
}

pub(crate) fn opt_hlc(
    row: &Row<'_>,
    wall_col: &str,
    counter_col: &str,
) -> rusqlite::Result<Option<Hlc>> {
    let wall: Option<i64> = row.get(wall_col)?;
    let counter: Option<u32> = row.get(counter_col)?;
    Ok(wall.zip(counter).map(|(w, c)| Hlc::new(w as u64, c)))
}

pub(crate) fn ulid(what: &'static str, text: &str) -> Result<Ulid> {
    text.parse().map_err(|e| StoreError::corrupt(what)(&e))
}

/// The serde name of a unit enum variant (`Priority::High` → `high`), which
/// is what the CHECK constraints list.
pub(crate) fn enum_name<T: Serialize>(value: &T) -> String {
    match serde_json::to_value(value) {
        Ok(serde_json::Value::String(s)) => s,
        other => unreachable!("enum_name on a non-string-serializing value: {other:?}"),
    }
}

pub(crate) fn enum_from_name<T: DeserializeOwned>(what: &'static str, name: String) -> Result<T> {
    serde_json::from_value(serde_json::Value::String(name))
        .map_err(|e| StoreError::corrupt(what)(&e))
}

pub(crate) fn json<T: Serialize>(value: &T) -> String {
    serde_json::to_string(value).expect("domain types serialize without error")
}

pub(crate) fn from_json<T: DeserializeOwned>(what: &'static str, text: &str) -> Result<T> {
    serde_json::from_str(text).map_err(|e| StoreError::corrupt(what)(&e))
}

pub(crate) fn opt_from_json<T: DeserializeOwned>(
    what: &'static str,
    text: Option<String>,
) -> Result<Option<T>> {
    text.as_deref().map(|t| from_json(what, t)).transpose()
}
