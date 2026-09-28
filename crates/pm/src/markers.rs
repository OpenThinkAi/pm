//! Structured markers (AGT-1342; projects/pm/README.md §Data model
//! "Structured markers"): `pm hold`, `pm holds`, `pm waive`, and the fixed
//! marker block `pm show` prints. `not_before=` / `parked=` are `pm set`
//! assignments, parsed in `verbs::parse_assignment` with the strict date
//! rules of [`pm_core::markers`].
//!
//! Markers are ticket fields, never description text: every one is an op
//! (`hold.set`, `hold.clear`, `field.set waivers|not_before|parked`).

use std::collections::BTreeSet;

use pm_core::markers::{MarkerError, date_from_ms, normalize_rule, with_waiver};
use pm_core::op::{FieldSet, HoldSet};
use pm_core::{Hlc, Hold, Payload, Ticket, Waiver};
use pm_store::TicketFilter;
use serde_json::json;
use ulid::Ulid;

use crate::exit::{CliError, Result};
use crate::verbs::{Ctx, SCHEMA, Stamper, display_id, find, non_empty, print_json, ticket_json};

impl From<MarkerError> for CliError {
    fn from(e: MarkerError) -> Self {
        CliError::usage(e)
    }
}

/// `pm hold AGT-N "why"` sets `hold{reason, by, at}` (`by` is this
/// command's actor, `at` its op's HLC); `pm hold --clear AGT-N` clears it.
/// Clearing a ticket that is not held is a no-op, not an error.
pub fn hold(ctx: &Ctx<'_>, reference: &str, reason: Option<&str>, clear: bool) -> Result<()> {
    let reason = match (reason, clear) {
        (Some(_), true) => {
            return Err(CliError::usage(
                "a hold reason and --clear are mutually exclusive",
            ));
        }
        (None, false) => {
            return Err(CliError::usage(
                "a hold reason is required (or --clear to release the hold)",
            ));
        }
        (Some(reason), false) => Some(non_empty("hold reason", reason)?),
        (None, true) => None,
    };
    let actor = ctx.actor()?;
    let (mut store, ws) = ctx.open()?;
    let ticket = find(&store, &ws, reference)?;
    let mut stamper = Stamper::new(&store, actor.clone())?;
    match reason {
        Some(reason) => {
            let mut op = stamper.op(
                ticket.id,
                Payload::HoldSet(HoldSet {
                    hold: Hold {
                        reason,
                        by: actor,
                        at: Hlc::ZERO,
                    },
                }),
            );
            let hlc = op.hlc;
            if let Payload::HoldSet(set) = &mut op.payload {
                set.hold.at = hlc;
            }
            store.commit(&op)?;
        }
        None if ticket.hold.is_none() => {
            eprintln!("pm: {} is not held", display_id(&ws, &ticket));
        }
        None => {
            store.commit(&stamper.op(ticket.id, Payload::HoldClear))?;
        }
    }
    print_result(ctx, &store, &ws, ticket.id)
}

pub struct HoldsArgs {
    pub project: Option<String>,
    pub ids: Vec<String>,
}

/// `pm holds [--project P | --ids …]` (AGT-1380 AC1): every live held
/// ticket (tombstoned and archived ones excluded, as `Store::tickets`
/// does), numbered order. `--ids` scopes exactly like `pm ready --ids`: an
/// unknown id is exit `3`, and only the held tickets among that set are
/// listed.
pub fn holds(ctx: &Ctx<'_>, args: HoldsArgs) -> Result<()> {
    let (store, ws) = ctx.open()?;
    let ids: Option<BTreeSet<Ulid>> = if args.ids.is_empty() {
        None
    } else {
        Some(
            args.ids
                .iter()
                .map(|r| find(&store, &ws, r).map(|t| t.id))
                .collect::<Result<BTreeSet<Ulid>>>()?,
        )
    };
    if let Some(p) = &args.project {
        require_project(&store, p)?;
    }
    let tickets = match &ids {
        Some(id_set) => store
            .all_tickets()?
            .into_iter()
            .filter(|t| {
                id_set.contains(&t.id) && !t.deleted && t.archived_at.is_none() && t.hold.is_some()
            })
            .collect::<Vec<_>>(),
        None => store.tickets(&TicketFilter {
            project: args.project.clone().into_iter().collect(),
            held: true,
            ..TicketFilter::default()
        })?,
    };
    if ctx.json {
        let tickets = tickets
            .iter()
            .map(|t| ticket_json(&ws, &store, t))
            .collect::<Result<Vec<_>>>()?;
        print_json(&json!({ "schema": SCHEMA, "tickets": tickets }));
        return Ok(());
    }
    for t in &tickets {
        if let Some(hold) = &t.hold {
            println!(
                "{}  {}  [{}]",
                display_id(&ws, t),
                t.title,
                describe_hold(hold)
            );
        }
    }
    Ok(())
}

/// `pm waive AGT-N <rule> "reason"`: records (or re-reasons) a waiver of a
/// hygiene rule. The waiver list is one LWW register, so this reads the
/// current list, extends it, and sets the whole list.
pub fn waive(ctx: &Ctx<'_>, reference: &str, rule: &str, reason: &str) -> Result<()> {
    let waiver = Waiver {
        rule: normalize_rule(rule)?,
        reason: non_empty("waiver reason", reason)?,
    };
    let actor = ctx.actor()?;
    let (mut store, ws) = ctx.open()?;
    let ticket = find(&store, &ws, reference)?;
    let waivers = with_waiver(&ticket.waivers, waiver);
    let mut stamper = Stamper::new(&store, actor)?;
    store.commit(&stamper.op(ticket.id, Payload::FieldSet(FieldSet::Waivers(waivers))))?;
    print_result(ctx, &store, &ws, ticket.id)
}

/// `pm show`'s marker block: always after the ticket's fields and before
/// its description, one line per marker, omitted when there are none.
pub fn print_block(t: &Ticket) {
    let lines = block_lines(t);
    if lines.is_empty() {
        return;
    }
    println!("markers:");
    for line in lines {
        println!("  {line}");
    }
}

fn block_lines(t: &Ticket) -> Vec<String> {
    let mut lines = Vec::new();
    if let Some(hold) = &t.hold {
        lines.push(format!("hold:        {}", describe_hold(hold)));
    }
    if let Some(nb) = &t.not_before {
        lines.push(format!("not-before:  {}", nb.date));
    }
    if let Some(parked) = &t.parked {
        lines.push(format!("parked:      {}", parked.until));
    }
    for w in &t.waivers {
        lines.push(format!("waiver:      {}: {}", w.rule, w.reason));
    }
    lines
}

pub(crate) fn describe_hold(hold: &Hold) -> String {
    format!(
        "{} (by {}, {})",
        hold.reason,
        hold.by,
        date_from_ms(hold.at.wall_ms)
    )
}

pub(crate) fn require_project(store: &pm_store::Store, project: &str) -> Result<()> {
    if store.project(project)?.is_none() {
        return Err(CliError::not_found(format!("no project '{project}'")));
    }
    Ok(())
}

fn print_result(
    ctx: &Ctx<'_>,
    store: &pm_store::Store,
    ws: &pm_core::Workspace,
    id: ulid::Ulid,
) -> Result<()> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use pm_core::{ActorId, NotBefore, Parked};

    #[test]
    fn marker_block_is_fixed_order_and_empty_without_markers() {
        let mut t: Ticket = serde_json::from_value(json!({
            "id": ulid::Ulid::nil(), "number": 1, "title": "t", "state": "triage",
            "priority": "medium", "project": null, "repo": null, "assignee": null,
            "description": "hold: not a marker\n", "labels": [],
            "created": {"wall_ms": 0, "counter": 0}, "updated": {"wall_ms": 0, "counter": 0},
            "archived_at": null, "deleted": false, "linked_github": null, "linked_pr": null,
            "linear": null, "source": null, "hold": null, "waivers": [],
            "not_before": null, "parked": null, "ext": {}
        }))
        .unwrap();
        assert!(block_lines(&t).is_empty());
        t.waivers = vec![Waiver {
            rule: "R1".into(),
            reason: "standalone".into(),
        }];
        t.parked = Some(Parked {
            until: "forever".into(),
        });
        t.not_before = Some(NotBefore {
            date: "2026-10-01".into(),
        });
        t.hold = Some(Hold {
            reason: "needs Matt".into(),
            by: ActorId::new("matt"),
            at: Hlc::new(1_790_000_000_000, 0),
        });
        assert_eq!(
            block_lines(&t),
            [
                "hold:        needs Matt (by matt, 2026-09-21)",
                "not-before:  2026-10-01",
                "parked:      forever",
                "waiver:      R1: standalone",
            ]
        );
    }
}
