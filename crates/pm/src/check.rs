//! `pm check [--project P]` (AGT-1342; README §CLI verbs, replacing
//! vault-sweep §4 and the hygiene 30-second check). Runs every invariant in
//! [`pm_core::check`] and prints one line per finding; exit 1 if there is
//! any finding, 0 when clean. `--json` prints
//! `{schema, ok, count, findings: [{rule, tickets, message, …}]}` either way.

use std::collections::BTreeMap;

use pm_core::{Finding, Workspace};
use serde_json::{Value, json};
use ulid::Ulid;

use crate::exit::{CliError, Result};
use crate::markers::{describe_hold, require_project};
use crate::verbs::{Ctx, SCHEMA, now_ms, print_json, ref_id};

pub fn check(ctx: &Ctx<'_>, project: Option<&str>) -> Result<()> {
    let (store, ws) = ctx.open()?;
    if let Some(p) = project {
        require_project(&store, p)?;
    }
    // One snapshot serves both the checker and the id → AGT-N names. A
    // finding names tickets to act on (`pm set <id> …`), so a ticket still
    // awaiting its hub number is named by ULID (`ref_id`), not `AGT-?`.
    let tickets = store.all_tickets()?;
    let now = now_ms();
    let mut findings = pm_core::check::check(&ws, &tickets, &store.all_relations()?, now, project);
    pm_core::check::with_deleted_projects(&mut findings, &store.deleted_project_refs()?, project);
    pm_core::check::with_parked_since(&mut findings, &store.parked_since()?, now);
    let names: BTreeMap<Ulid, String> = tickets.iter().map(|t| (t.id, ref_id(&ws, t))).collect();
    let name = |id: &Ulid| names.get(id).cloned().unwrap_or_else(|| id.to_string());

    if ctx.json {
        let rendered: Vec<Value> = findings
            .iter()
            .map(|f| finding_json(f, &ws, &name))
            .collect();
        print_json(&json!({
            "schema": SCHEMA,
            "ok": findings.is_empty(),
            "count": findings.len(),
            "findings": rendered,
        }));
    } else {
        for f in &findings {
            let ids: Vec<String> = f.tickets().iter().map(&name).collect();
            println!(
                "{:<19} {:<14} {}",
                f.rule(),
                ids.join(","),
                crate::text::inline(&message(f, &ws, &name))
            );
        }
        if findings.is_empty() {
            println!("ok: no findings");
        }
    }
    if findings.is_empty() {
        Ok(())
    } else {
        Err(CliError::error(format!("{} finding(s)", findings.len())))
    }
}

fn finding_json(f: &Finding, ws: &Workspace, name: &impl Fn(&Ulid) -> String) -> Value {
    let tickets: Vec<String> = f.tickets().iter().map(name).collect();
    let mut out = json!({
        "rule": f.rule(),
        "tickets": tickets,
        "message": message(f, ws, name),
    });
    let extra = match f {
        Finding::Stale { days, .. } => json!({ "days": days, "stale_days": ws.stale_days }),
        Finding::Held { hold, .. } => json!({ "hold": hold }),
        Finding::AssignedUnstarted {
            assignee, state, ..
        } => {
            json!({ "assignee": assignee, "state": state })
        }
        Finding::DanglingRelation { relation, missing } => json!({
            "relation": {
                "kind": relation.kind,
                "from": name(&relation.from),
                "to": name(&relation.to),
            },
            "missing": name(missing),
        }),
        Finding::Parked { days, state, .. } => json!({ "days": days, "state": state }),
        Finding::DeletedProject { project, .. } => json!({ "project": project }),
        Finding::BlockedByCanceled { blocker, state, .. } => {
            json!({ "blocker": name(blocker), "state": state })
        }
        Finding::NoProject { .. } | Finding::BlockerCycle { .. } => json!({}),
    };
    if let (Value::Object(out), Value::Object(extra)) = (&mut out, extra) {
        out.extend(extra);
    }
    out
}

fn message(f: &Finding, ws: &Workspace, name: &impl Fn(&Ulid) -> String) -> String {
    match f {
        Finding::NoProject { .. } => {
            "no project and no R1 waiver (`pm set <id> project=…` or `pm waive <id> R1 \"why\"`)"
                .to_string()
        }
        Finding::Stale { days, .. } => format!(
            "unstarted and not updated for {days} days (stale_days = {})",
            ws.stale_days
        ),
        Finding::Held { hold, .. } => format!("held: {}", describe_hold(hold)),
        Finding::AssignedUnstarted {
            assignee, state, ..
        } => format!(
            "assigned to {assignee} but in unstarted state '{state}'; \
             `pm claim` refuses it and `pm ready` excludes it — run \
             `pm unclaim <id>` to clear the stray assignee"
        ),
        Finding::Parked { days, state, .. } => format!(
            "parked forever for {days} days in state '{state}'; `pm ready` never \
             surfaces it — unpark it (`pm set <id> parked=`) or say why it stays \
             parked (`pm waive <id> parked \"why\"`)"
        ),
        Finding::BlockerCycle { tickets } => {
            let ids: Vec<String> = tickets.iter().map(name).collect();
            format!("blocker cycle: {} block each other", ids.join(", "))
        }
        Finding::DeletedProject { project, .. } => format!(
            "filed in project '{project}', which was deleted; \
             move it (`pm set <id> project=…`)"
        ),
        Finding::BlockedByCanceled {
            ticket,
            blocker,
            state,
        } => {
            let (t, b) = (name(ticket), name(blocker));
            format!(
                "blocked by canceled {b} (state '{state}'), which still blocks it; if the \
                 cancellation unblocks it, `pm relate {t} --unblock {b}` and comment why, \
                 else cancel {t} too"
            )
        }
        Finding::DanglingRelation { relation, missing } => {
            let kind = serde_json::to_value(relation.kind)
                .ok()
                .and_then(|v| v.as_str().map(str::to_string))
                .unwrap_or_default();
            format!(
                "{} {kind} {}, but {} is tombstoned or missing",
                name(&relation.from),
                name(&relation.to),
                name(missing)
            )
        }
    }
}
