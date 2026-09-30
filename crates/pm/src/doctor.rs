//! `pm doctor [--rebuild]` (AGT-1337): renders `pm_store::Store::doctor`
//! and `Store::rebuild`. Exit 0 when the database is healthy — every
//! constraint holds and replaying the op log reproduces the ticket tables
//! exactly — else 1. `--rebuild` regenerates the ticket tables from the
//! log first, prints what changed, then reports on the result.

use pm_store::{Diff, Report};
use serde_json::{Value, json};

use crate::exit::{CliError, Result};
use crate::verbs::{Ctx, SCHEMA};
use crate::workspace;

pub fn doctor(ctx: &Ctx<'_>, rebuild: bool) -> Result<()> {
    let (mut store, _) = workspace::open(&workspace::resolve(ctx.workspace, ctx.env)?)?;
    let rebuilt = if rebuild {
        Some(store.rebuild()?)
    } else {
        None
    };
    let report = store.doctor()?;

    if ctx.json {
        let mut out = json!({
            "schema": SCHEMA,
            "healthy": report.is_healthy(),
            "rebuilt": rebuilt,
        });
        let Value::Object(fields) = serde_json::to_value(&report).expect("a report serializes")
        else {
            unreachable!("a report serializes to an object");
        };
        out.as_object_mut()
            .expect("an object literal")
            .extend(fields);
        println!(
            "{}",
            serde_json::to_string_pretty(&out).expect("a JSON value serializes")
        );
    } else {
        if let Some(diff) = &rebuilt {
            print_rebuilt(diff, report.op_count);
        }
        print_report(&report);
    }

    if report.is_healthy() {
        Ok(())
    } else if rebuild {
        Err(CliError::error(
            "database is still unhealthy after the rebuild; see the report above",
        ))
    } else {
        Err(CliError::error(
            "database is unhealthy; `pm doctor --rebuild` regenerates the ticket tables from the op log",
        ))
    }
}

fn print_rebuilt(diff: &Diff, op_count: u64) {
    if diff.is_empty() {
        println!("rebuilt ticket tables from {op_count} ops: no changes, they already matched");
    } else {
        println!(
            "rebuilt ticket tables from {op_count} ops: {} row(s) changed",
            diff.row_count()
        );
        print_diff(diff);
    }
    println!();
}

fn print_report(report: &Report) {
    println!("schema version  {}", report.schema_version);
    println!("ops             {}", report.op_count);
    let counts: Vec<String> = report
        .tables
        .iter()
        .map(|(table, rows)| format!("{table} {rows}"))
        .collect();
    println!("rows            {}", counts.join(", "));

    if report.integrity.is_empty() {
        println!("integrity       ok");
    } else {
        println!("integrity       {} problem(s)", report.integrity.len());
        for message in &report.integrity {
            println!("  {message}");
        }
    }

    if report.foreign_keys.is_empty() {
        println!("foreign keys    ok");
    } else {
        println!("foreign keys    {} violation(s)", report.foreign_keys.len());
        for v in &report.foreign_keys {
            let rowid = v.rowid.map_or_else(|| "?".into(), |r| r.to_string());
            println!(
                "  {} rowid {rowid} -> {} (foreign key #{})",
                v.table, v.parent, v.fk_index
            );
        }
    }

    let sync = &report.sync;
    let cursor = if sync.cursor == 0 {
        "0 (never pulled)".to_string()
    } else {
        sync.cursor.to_string()
    };
    println!(
        "sync            outbox {} op(s), pushed through seq {}, cursor {cursor}, {} ticket(s) awaiting a hub number",
        sync.outbox, sync.pushed_through, sync.pending_numbers
    );

    match &report.replay_error {
        Some(error) => println!("replay          FAILED: {error}"),
        None if report.drift.is_empty() => {
            println!("replay          ok: ticket tables match the op log");
        }
        None => {
            println!(
                "replay          DRIFT: {} row(s) differ from the op log",
                report.drift.row_count()
            );
            print_diff(&report.drift);
        }
    }
}

/// `before` is what the tables held, `after` what the log produces.
fn print_diff(diff: &Diff) {
    for table in &diff.tables {
        println!("  {}", table.table);
        for row in &table.extra {
            println!("    - {}  (not produced by the log)", key(&row.key));
        }
        for row in &table.missing {
            println!("    + {}  (produced by the log, was absent)", key(&row.key));
        }
        for change in &table.changed {
            let columns: Vec<String> = change
                .columns
                .iter()
                .map(|c| format!("{}: {} -> {}", c.column, cell(&c.before), cell(&c.after)))
                .collect();
            println!("    ~ {}  {}", key(&change.key), columns.join(", "));
        }
    }
}

fn key(key: &[Value]) -> String {
    let parts: Vec<String> = key.iter().map(cell).collect();
    format!("[{}]", parts.join(", "))
}

/// A cell for one line of output: JSON, cut short past 60 characters (a
/// `ticket_view.view` blob runs to kilobytes; `--json` has it whole).
fn cell(value: &Value) -> String {
    let text = match value {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    };
    match text.char_indices().nth(60) {
        Some((cut, _)) => format!("{}…", &text[..cut]),
        None => text,
    }
}
