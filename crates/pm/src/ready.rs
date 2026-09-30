//! `pm ready [--project P | --ids …] [--limit n] [--model m]
//! [--exclude-label l] [--explain]` (AGT-1343; README §CLI verbs): the
//! frontier the build loops used to recompute thirteen slightly different
//! ways. The definition is [`pm_core::ready`]'s — the same one
//! `pm claim --ready` takes its candidates from — and this module only
//! scopes it, renders it, and applies `--limit`.
//!
//! `--json` prints `{schema, project, ids, model, today, ready, excluded,
//! waves}`: `ready` as `pm list --json` would print each ticket,
//! `excluded` one `{id, ulid, reason, message, …}` per candidate that is
//! not ready (whatever `--explain` says: it only affects the human
//! output), and `waves` the ready set followed by the waves the graph
//! would unblock (model-agnostic, and never cut by `--limit`).

use std::collections::{BTreeMap, BTreeSet};

use pm_core::markers::date_from_ms;
use pm_core::ready::{DEFAULT_MODEL, Gate, MODEL_LABEL_PREFIX, Reason, Rules, Scope, model_labels};
use pm_core::{Ticket, Workspace};
use pm_store::Store;
use serde_json::{Value, json};
use ulid::Ulid;

use crate::exit::{CliError, Result};
use crate::markers::describe_hold;
use crate::read::priority_str;
use crate::verbs::{
    Ctx, SCHEMA, display_id, find, non_empty, now_ms, print_json, ref_id, require_project,
    ticket_json,
};

pub struct ReadyArgs {
    pub project: Option<String>,
    pub ids: Vec<String>,
    pub limit: Option<usize>,
    pub model: Option<String>,
    pub exclude_labels: Vec<String>,
    pub explain: bool,
}

/// What `pm ready` computes before it prints: the `--json` payload, and
/// the pieces the human rendering needs besides it.
pub(crate) struct Frontier {
    pub(crate) json: Value,
    ready: Vec<Ticket>,
    /// `(ref, message)` per excluded candidate, for `--explain`.
    excluded: Vec<(String, String)>,
}

pub fn ready(ctx: &Ctx<'_>, args: ReadyArgs) -> Result<()> {
    let (store, ws) = ctx.open()?;
    let frontier = compute(&store, &ws, &args)?;
    if ctx.json {
        print_json(&frontier.json);
        return Ok(());
    }

    let ready = &frontier.ready;
    if ready.is_empty() {
        eprintln!("no ready tickets");
    }
    let id_w = ready
        .iter()
        .map(|t| display_id(&ws, t).len())
        .max()
        .unwrap_or(2);
    for t in ready {
        let models: Vec<String> = model_labels(t)
            .iter()
            .map(|l| l[MODEL_LABEL_PREFIX.len()..].to_string())
            .collect();
        println!(
            "{:<id_w$}  {:<8}  {:<10}  {:<12}  {}",
            display_id(&ws, t),
            priority_str(&t.priority),
            if models.is_empty() {
                "-".to_string()
            } else {
                models.join(",")
            },
            crate::text::inline(t.project.as_deref().unwrap_or("-")),
            crate::text::inline(&t.title),
        );
    }
    if args.explain && !frontier.excluded.is_empty() {
        println!("excluded:");
        let ex_w = frontier
            .excluded
            .iter()
            .map(|(name, _)| name.len())
            .max()
            .unwrap_or(2);
        for (name, message) in &frontier.excluded {
            println!("  {name:<ex_w$}  {}", crate::text::inline(message));
        }
    }
    Ok(())
}

/// The frontier for `args` — scope, rules, `--limit` — validated the
/// way the verb validates its flags (exit `2`/`3`). `pub(crate)`: `pm
/// app`'s ready endpoint (AGT-1401) serves `Frontier::json` unchanged.
pub(crate) fn compute(store: &Store, ws: &Workspace, args: &ReadyArgs) -> Result<Frontier> {
    if args.limit == Some(0) {
        return Err(CliError::usage("--limit must be at least 1"));
    }
    let model = args
        .model
        .as_deref()
        .map(|m| non_empty("--model", m))
        .transpose()?;

    let scope = match (&args.project, args.ids.is_empty()) {
        (Some(p), _) => {
            let p = non_empty("--project", p)?;
            require_project(store, &p)?;
            Scope::Project(p)
        }
        (None, false) => Scope::Ids(
            args.ids
                .iter()
                .map(|r| find(store, ws, r).map(|t| t.id))
                .collect::<Result<BTreeSet<Ulid>>>()?,
        ),
        (None, true) => Scope::All,
    };
    let mut gate_labels = ws.gate_labels.clone();
    for label in &args.exclude_labels {
        gate_labels.insert(non_empty("--exclude-label", label)?);
    }
    let today = date_from_ms(now_ms());
    let rules = Rules {
        today: today.clone(),
        gate_labels,
        model: model.clone(),
    };

    let (tickets, frontier) = store.frontier(ws, &scope, &rules)?;
    let by_id: BTreeMap<Ulid, &Ticket> = tickets.iter().map(|t| (t.id, t)).collect();
    // Waves, `ids` and the excluded list are references (`ref_id`): a
    // ticket still awaiting its hub number is named by ULID, which
    // `--ids` accepts back. The `ready` rows keep their display id.
    let name = |id: &Ulid| -> String {
        by_id
            .get(id)
            .map(|t| ref_id(ws, t))
            .unwrap_or_else(|| id.to_string())
    };

    let ready: Vec<&Ticket> = frontier
        .ready()
        .iter()
        .filter_map(|id| by_id.get(id).copied())
        .take(args.limit.unwrap_or(usize::MAX))
        .collect();

    // Every candidate with a reason, plus — when the caller named ids —
    // the ones that are not candidates at all (done, archived, deleted),
    // so `--ids AGT-3 --explain` never answers with silence.
    let mut excluded: Vec<(Ulid, String, Value)> = frontier
        .excluded()
        .into_iter()
        .map(|(id, reason)| {
            let (message, extra) = describe(reason, ws, &rules, &name);
            let mut json = json!({ "reason": reason.kind(), "message": message });
            merge(&mut json, extra);
            (id, message, json)
        })
        .collect();
    if let Scope::Ids(ids) = &scope {
        let judged: BTreeSet<Ulid> = frontier.verdicts.iter().map(|(id, _)| *id).collect();
        for id in ids.iter().filter(|id| !judged.contains(id)) {
            let message = match by_id.get(id) {
                Some(t) if t.deleted => "deleted".to_string(),
                Some(t) if t.archived_at.is_some() => "archived".to_string(),
                Some(t) => format!("done: in state '{}'", t.state),
                None => "no such ticket".to_string(),
            };
            excluded.push((
                *id,
                message.clone(),
                json!({ "reason": "done", "message": message }),
            ));
        }
    }
    let waves: Vec<Vec<String>> = frontier
        .waves
        .iter()
        .map(|wave| wave.iter().map(&name).collect())
        .collect();

    let mut ready_json = Vec::with_capacity(ready.len());
    for t in &ready {
        ready_json.push(ticket_json(ws, store, t)?);
    }
    let explained: Vec<(String, String)> = excluded
        .iter()
        .map(|(id, message, _)| (name(id), message.clone()))
        .collect();
    let excluded_json: Vec<Value> = excluded
        .into_iter()
        .map(|(id, _, mut json)| {
            merge(&mut json, json!({ "id": name(&id), "ulid": id }));
            json
        })
        .collect();
    let json = json!({
        "schema": SCHEMA,
        "project": args.project.clone(),
        "ids": match &scope {
            Scope::Ids(ids) => Some(
                tickets
                    .iter()
                    .filter(|t| ids.contains(&t.id))
                    .map(|t| ref_id(ws, t))
                    .collect::<Vec<_>>(),
            ),
            _ => None,
        },
        "model": model,
        "today": today,
        "limit": args.limit,
        "ready": ready_json,
        "excluded": excluded_json,
        "waves": waves,
    });
    Ok(Frontier {
        json,
        ready: ready.into_iter().cloned().collect(),
        excluded: explained,
    })
}

/// The human sentence for a reason, and the `--json` fields it carries
/// besides `reason` and `message` (ticket ids rendered as `AGT-N`).
fn describe(
    reason: &Reason,
    ws: &Workspace,
    rules: &Rules,
    name: &impl Fn(&Ulid) -> String,
) -> (String, Value) {
    let gate_text = |gate: &Gate| match gate {
        Gate::Held { hold } => format!("held: {}", describe_hold(hold)),
        Gate::Label { label } => format!("label {label}"),
    };
    let gate_json = |gate: &Gate| match gate {
        Gate::Held { hold } => json!({ "kind": "held", "hold": hold }),
        Gate::Label { label } => json!({ "kind": "label", "label": label }),
    };
    match reason {
        Reason::State { state } => (format!("in state '{state}'"), json!({ "state": state })),
        Reason::Assigned { assignee } => (
            format!("assigned to {assignee}"),
            json!({ "assignee": assignee }),
        ),
        Reason::Held { hold } => (
            format!("held: {}", describe_hold(hold)),
            json!({ "hold": hold }),
        ),
        Reason::Label { label } => (format!("label {label}"), json!({ "label": label })),
        Reason::Parked { until } => (format!("parked until {until}"), json!({ "until": until })),
        Reason::NotBefore { date } => (format!("not_before {date}"), json!({ "date": date })),
        Reason::Cycle { tickets } => {
            let ids: Vec<String> = tickets.iter().map(name).collect();
            (
                format!("blocker cycle: {}", ids.join(", ")),
                json!({ "tickets": ids }),
            )
        }
        Reason::BlockedBy { blocker, gate } => {
            let b = name(blocker);
            let message = match gate {
                Some(gate) => format!("blocked by {b} ({})", gate_text(gate)),
                None => format!("blocked by {b}"),
            };
            (
                message,
                json!({ "blocker": b, "gate": gate.as_ref().map(gate_json) }),
            )
        }
        Reason::TransitivelyBlocked { via, root, gate } => (
            format!(
                "transitively blocked by {} ({}) via {}",
                name(root),
                gate_text(gate),
                name(via)
            ),
            json!({ "via": name(via), "root": name(root), "gate": gate_json(gate) }),
        ),
        Reason::Model { labels } => {
            let wanted = rules.model.as_deref().unwrap_or_default();
            let has = if labels.is_empty() {
                format!("no model label ({DEFAULT_MODEL} by default)")
            } else {
                let names: Vec<String> = labels
                    .iter()
                    .map(|l| {
                        let short = &l[MODEL_LABEL_PREFIX.len()..];
                        match ws.model_labels.get(l) {
                            Some(mapped) if mapped != short => format!("{short} ({mapped})"),
                            _ => short.to_string(),
                        }
                    })
                    .collect();
                format!("model {}", names.join(","))
            };
            (
                format!("{has}, not {wanted}"),
                json!({ "labels": labels, "wanted": wanted }),
            )
        }
    }
}

fn merge(into: &mut Value, extra: Value) {
    if let (Value::Object(into), Value::Object(extra)) = (into, extra) {
        into.extend(extra);
    }
}
