//! `pm sync [--watch <secs>]` (AGT-1395): the client half of README
//! §Sync & hub. Local ops go up, everyone else's come down, and both sides
//! converge by the same `pm-core` merge rules — the hub only orders.
//!
//! One sync is **push, then pull**, each in units the database commits on
//! its own:
//!
//! - **Push.** The outbox ([`Store::outbox`]: local ops the hub has not
//!   acknowledged) goes up in batches sized under both of the hub's limits
//!   — [`MAX_BATCH_OPS`] ops and [`MAX_BATCH_BYTES`] of request body
//!   (`docs/hub-api.md`; the byte cap matters because one document edit can
//!   be tens of megabytes). Ops are marked pushed
//!   ([`Store::mark_pushed`]) only for the `op_id`s the hub's `200`
//!   acknowledges, *after* the response is read. A crash between the hub's
//!   commit and the mark leaves those ops in the outbox; the next push
//!   sends them again and the hub answers `stored: false` for each (push
//!   is idempotent on `op_id`), so nothing is lost or doubled.
//! - **Pull.** Pages of `GET /ops?since=<cursor>&limit=` are applied one
//!   page per transaction ([`Store::apply_pulled`]: foreign ops land through
//!   the normal commit path, ops already present by `op_id` — this replica's
//!   own, echoed back — are skipped and counted as pushed), and the cursor
//!   ([`Store::set_cursor`]) moves to the page's `next` only after that
//!   transaction has committed. A crash between the two re-pulls the page,
//!   which then applies as all-skipped. Paging stops at `next >= head`.
//! - **Failure.** A transport error, a `404` (the hub's one answer for a
//!   bad token or unknown workspace), a structured `400`/`413`, or an op the
//!   store refuses all exit `1` with the cause on stderr. Nothing local is
//!   touched beyond what was already committed: every batch the hub had
//!   acknowledged is marked, every page applied has advanced the cursor, and
//!   the rest of the outbox and the cursor are exactly as before.
//! - **`--watch <secs>`** repeats the sync every `secs` seconds until
//!   interrupted; each round's outcome (or error) is printed and a failed
//!   round does not end the loop. Ctrl-C at any point is safe for the same
//!   reason a crash is.
//!
//! Numbers: a `field.set number` the hub authored (AGT-1391) arrives on
//! pull like any other op; `apply_pulled` clears the ticket's pending flag.

use std::thread::sleep;
use std::time::Duration;

use anyhow::Context;
use pm_core::{Op, Workspace};
use pm_store::Store;
use serde::Deserialize;
use serde_json::{Value, json};
use ulid::Ulid;

use crate::exit::{CliError, Result};
use crate::hub::{HubClient, Transport};
use crate::verbs::{Ctx, SCHEMA, print_json};

/// The hub's per-batch op cap (`pm_hub::ops::MAX_BATCH_OPS`, mirrored
/// here: the hub is a binary crate).
pub const MAX_BATCH_OPS: usize = 1000;
/// The hub's per-request body cap (`pm_hub::ops::MAX_BODY_BYTES`).
pub const MAX_BATCH_BYTES: usize = 64 * 1024 * 1024;
/// Ops asked for per pull page (the hub clamps at 1000).
const PAGE_LIMIT: usize = 1000;
/// Per request. A batch can be 64 MiB, and the Studio's uplink is not a
/// data centre's.
const TIMEOUT: Duration = Duration::from_secs(300);

pub struct SyncArgs {
    /// Re-sync every this many seconds, forever.
    pub watch: Option<u64>,
}

/// What one round moved.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Round {
    /// Outbox ops the hub acknowledged this round.
    pushed: usize,
    /// Ops received from the hub this round.
    pulled: usize,
    /// Of those, foreign ops committed into the log.
    applied: usize,
    /// Of those, ops already present (this replica's own, echoed back).
    skipped: usize,
    /// Where the cursor stands now.
    cursor: i64,
    /// The hub's head as of the last page.
    head: i64,
}

pub fn sync(ctx: &Ctx<'_>, args: SyncArgs) -> Result<()> {
    let (mut store, ws) = ctx.open()?;
    let hub = HubClient::resolve(ctx.env, &ws, TIMEOUT)?;
    match args.watch {
        None => {
            let round = run_once(&mut store, &hub)?;
            report(ctx, &store, &ws, &hub, &round, false)
        }
        Some(secs) => loop {
            match run_once(&mut store, &hub) {
                Ok(round) => report(ctx, &store, &ws, &hub, &round, true)?,
                Err(e) => eprintln!("pm: sync failed: {:#}", e.error),
            }
            sleep(Duration::from_secs(secs));
        },
    }
}

fn run_once(store: &mut Store, hub: &HubClient) -> Result<Round> {
    let mut round = Round {
        pushed: push_all(store, hub)?,
        ..Round::default()
    };
    pull_all(store, hub, &mut round)?;
    Ok(round)
}

// ------------------------------------------------------------------- push

/// Pushes the whole outbox, batch by batch, marking each batch as the hub
/// acknowledges it. Returns how many ops were acknowledged.
fn push_all(store: &mut Store, hub: &HubClient) -> Result<usize> {
    let mut pushed = 0;
    loop {
        let outbox = store.outbox(MAX_BATCH_OPS)?;
        if outbox.is_empty() {
            return Ok(pushed);
        }
        let batch = Batch::take(&outbox, MAX_BATCH_BYTES)?;
        let (status, body) = hub
            .post_json("ops", &batch.body)
            .map_err(|e| transport(hub, e))?;
        match status {
            200 => {}
            404 => return Err(rejected(hub)),
            400 | 413 => return Err(refused(status, &body)),
            503 => return Err(unavailable()),
            other => return Err(unexpected(other, "the push")),
        }
        let acked = batch.acknowledged(&body)?;
        if acked.is_empty() {
            return Err(CliError::error(format!(
                "the hub answered the push with no acknowledgement for any of the {} ops sent",
                batch.op_ids.len()
            )));
        }
        // Only what the hub acknowledged leaves the outbox. Anything it did
        // not name stays and goes up again next round.
        store.mark_pushed(&acked)?;
        pushed += acked.len();
        if acked.len() < batch.op_ids.len() {
            return Err(CliError::error(format!(
                "the hub acknowledged {} of the {} ops sent; run `pm sync` again",
                acked.len(),
                batch.op_ids.len()
            )));
        }
    }
}

/// One push request: the leading run of the outbox that fits under the
/// byte cap, as the request body the hub stores verbatim (each op is the
/// op log's own JSON, `serde_json::to_string`).
#[derive(Debug)]
struct Batch {
    op_ids: Vec<Ulid>,
    body: String,
}

impl Batch {
    fn take(outbox: &[(i64, Op)], max_bytes: usize) -> Result<Batch> {
        const OPEN: &str = "{\"ops\":[";
        const CLOSE: &str = "]}";
        let mut body = String::from(OPEN);
        let mut op_ids = Vec::new();
        for (_, op) in outbox {
            let text = serde_json::to_string(op).expect("an op serializes");
            let separator = usize::from(!op_ids.is_empty());
            if body.len() + separator + text.len() + CLOSE.len() > max_bytes {
                if op_ids.is_empty() {
                    return Err(CliError::error(format!(
                        "op {} ({}) is {} bytes on the wire, over the hub's {} MiB request limit",
                        op.op_id,
                        op.kind(),
                        text.len(),
                        max_bytes / (1024 * 1024)
                    )));
                }
                break;
            }
            if separator == 1 {
                body.push(',');
            }
            body.push_str(&text);
            op_ids.push(op.op_id);
        }
        body.push_str(CLOSE);
        Ok(Batch { op_ids, body })
    }

    /// The `op_id`s of this batch the hub's `200` body acknowledges
    /// (`{"ops": [{"op_id", "seq", "stored"}, …]}`), in batch order. An
    /// acknowledgement for an op that was not sent is ignored.
    fn acknowledged(&self, body: &str) -> Result<Vec<Ulid>> {
        #[derive(Deserialize)]
        struct Acks {
            ops: Vec<Ack>,
        }
        #[derive(Deserialize)]
        struct Ack {
            op_id: Ulid,
        }
        let acks: Acks = serde_json::from_str(body)
            .context("the hub's push response is not the expected JSON")?;
        let acked: std::collections::BTreeSet<Ulid> =
            acks.ops.into_iter().map(|a| a.op_id).collect();
        Ok(self
            .op_ids
            .iter()
            .copied()
            .filter(|id| acked.contains(id))
            .collect())
    }
}

// ------------------------------------------------------------------- pull

/// One page of `GET /ops`, as `docs/hub-api.md` describes it.
#[derive(Deserialize)]
struct Page {
    ops: Vec<Item>,
    next: i64,
    head: i64,
}

#[derive(Deserialize)]
struct Item {
    seq: i64,
    op: Value,
}

/// Pulls from the cursor to the hub's head, a page per transaction, and
/// records the outcome on `round`.
fn pull_all(store: &mut Store, hub: &HubClient, round: &mut Round) -> Result<()> {
    let mut since = store.cursor()?;
    loop {
        let (status, body) = hub
            .get(&format!("ops?since={since}&limit={PAGE_LIMIT}"))
            .map_err(|e| transport(hub, e))?;
        match status {
            200 => {}
            404 => return Err(rejected(hub)),
            400 => return Err(refused(status, &body)),
            503 => return Err(unavailable()),
            other => return Err(unexpected(other, "the pull")),
        }
        let page: Page =
            serde_json::from_str(&body).context("the hub's pull response is not a page")?;
        if page.next < since {
            return Err(CliError::error(format!(
                "the hub answered a pull since {since} with next = {}; refusing to move the cursor backwards",
                page.next
            )));
        }
        let ops = page
            .ops
            .into_iter()
            .map(|item| {
                serde_json::from_value::<Op>(item.op).with_context(|| {
                    format!("hub seq {}: not an op this build understands", item.seq)
                })
            })
            .collect::<std::result::Result<Vec<Op>, _>>()?;
        if !ops.is_empty() {
            let pulled = store.apply_pulled(&ops)?;
            round.pulled += ops.len();
            round.applied += pulled.applied;
            round.skipped += pulled.skipped;
        }
        // The page is committed; now, and only now, the cursor may pass it.
        if page.next > since {
            store.set_cursor(page.next)?;
        }
        round.cursor = page.next;
        round.head = page.head;
        if page.next >= page.head || ops.is_empty() {
            return Ok(());
        }
        since = page.next;
    }
}

// ----------------------------------------------------------------- errors

fn transport(hub: &HubClient, Transport(e): Transport) -> CliError {
    CliError::error(format!(
        "cannot reach the hub at {}: {e} (nothing was changed locally; the outbox and cursor are as they were)",
        hub.url
    ))
}

fn rejected(hub: &HubClient) -> CliError {
    CliError::error(format!(
        "the hub at {} rejected the token or does not know workspace {} (HTTP 404) — run `pm hub status`",
        hub.url, hub.workspace
    ))
}

/// A structured `4xx`: `{"error", "reason", …}` (`docs/hub-api.md`).
fn refused(status: u16, body: &str) -> CliError {
    let detail: Value = serde_json::from_str(body).unwrap_or(Value::Null);
    let field = |k: &str| detail.get(k).and_then(Value::as_str).map(str::to_string);
    let error = field("error").unwrap_or_else(|| format!("HTTP {status}"));
    let reason = field("reason").unwrap_or_else(|| body.trim().to_string());
    let mut msg = format!("the hub refused the request ({error}): {reason}");
    if let Some(index) = detail.get("index").and_then(Value::as_u64) {
        msg.push_str(&format!(" (op #{index} of the batch"));
        if let Some(op_id) = field("op_id") {
            msg.push_str(&format!(", {op_id}"));
        }
        msg.push(')');
    }
    CliError::error(msg)
}

fn unavailable() -> CliError {
    CliError::error("the hub cannot reach its database right now (HTTP 503); try again")
}

fn unexpected(status: u16, what: &str) -> CliError {
    CliError::error(format!("the hub answered {what} with HTTP {status}"))
}

// ----------------------------------------------------------------- output

/// One round's report. In `--watch` mode (`compact`) each round is one
/// line — one JSON object per line under `--json` — so the stream can be
/// tailed.
fn report(
    ctx: &Ctx<'_>,
    store: &Store,
    ws: &Workspace,
    hub: &HubClient,
    round: &Round,
    compact: bool,
) -> Result<()> {
    let state = store.sync_status()?;
    if ctx.json {
        let value = json!({
            "schema": SCHEMA,
            "hub": hub.url,
            "workspace": ws.id.to_string(),
            "pushed": round.pushed,
            "pulled": round.pulled,
            "applied": round.applied,
            "skipped": round.skipped,
            "cursor": round.cursor,
            "head": round.head,
            "outbox": state.outbox,
            "pending_numbers": state.pending_numbers,
        });
        if compact {
            println!("{value}");
        } else {
            print_json(&value);
        }
    } else {
        let mut line = format!(
            "pushed {} op(s); pulled {} op(s): {} applied, {} skipped; cursor {} (hub head {})",
            round.pushed, round.pulled, round.applied, round.skipped, round.cursor, round.head
        );
        if state.outbox > 0 {
            line.push_str(&format!("; {} op(s) still in the outbox", state.outbox));
        }
        if state.pending_numbers > 0 {
            line.push_str(&format!(
                "; {} ticket(s) still awaiting a hub number",
                state.pending_numbers
            ));
        }
        println!("{line}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use pm_core::op::CommentAdd;
    use pm_core::{ActorId, Hlc, Payload};

    fn comment(body: &str) -> (i64, Op) {
        (
            0,
            Op::new(
                Ulid::new(),
                Hlc::new(1_790_000_000_000, 0),
                ActorId::new("t"),
                Ulid::new(),
                Payload::CommentAdd(CommentAdd {
                    body: body.to_string(),
                }),
            ),
        )
    }

    #[test]
    fn batch_is_valid_json_the_hub_can_parse() {
        let outbox = [comment("a"), comment("b")];
        let batch = Batch::take(&outbox, MAX_BATCH_BYTES).unwrap();
        let v: Value = serde_json::from_str(&batch.body).unwrap();
        assert_eq!(v["ops"].as_array().unwrap().len(), 2);
        assert_eq!(batch.op_ids, [outbox[0].1.op_id, outbox[1].1.op_id]);
        let one = Batch::take(&outbox[..1], MAX_BATCH_BYTES).unwrap();
        let v: Value = serde_json::from_str(&one.body).unwrap();
        assert_eq!(v["ops"].as_array().unwrap().len(), 1);
        let none = Batch::take(&[], MAX_BATCH_BYTES).unwrap();
        assert_eq!(none.body, "{\"ops\":[]}");
    }

    #[test]
    fn batch_stops_under_the_byte_cap_and_never_splits_below_one_op() {
        let outbox = [comment("aaaa"), comment("bbbb"), comment("cccc")];
        let one_op = serde_json::to_string(&outbox[0].1).unwrap().len();
        // Room for two ops and the framing, not three.
        let cap = "{\"ops\":[".len() + 2 * one_op + 1 + "]}".len();
        let batch = Batch::take(&outbox, cap).unwrap();
        assert_eq!(batch.op_ids.len(), 2);
        assert!(batch.body.len() <= cap);
        // A single op that cannot fit is an error, not an empty batch.
        let err = Batch::take(&outbox, one_op).unwrap_err();
        assert!(
            err.error.to_string().contains("over the hub's"),
            "{}",
            err.error
        );
    }

    #[test]
    fn acknowledgements_are_filtered_to_the_batch_in_batch_order() {
        let outbox = [comment("a"), comment("b"), comment("c")];
        let batch = Batch::take(&outbox, MAX_BATCH_BYTES).unwrap();
        let stranger = Ulid::new();
        let body = json!({"ops": [
            {"op_id": outbox[2].1.op_id, "seq": 9, "stored": true},
            {"op_id": stranger, "seq": 10, "stored": true},
            {"op_id": outbox[0].1.op_id, "seq": 7, "stored": false},
        ]})
        .to_string();
        assert_eq!(
            batch.acknowledged(&body).unwrap(),
            [outbox[0].1.op_id, outbox[2].1.op_id]
        );
        assert!(batch.acknowledged("not json").is_err());
        assert!(batch.acknowledged("{\"ops\": []}").unwrap().is_empty());
    }

    #[test]
    fn refusals_quote_the_hub() {
        let e = refused(
            400,
            r#"{"error":"invalid_op","reason":"actor is empty","index":3,"op_id":"01X"}"#,
        );
        let msg = e.error.to_string();
        assert!(msg.contains("invalid_op") && msg.contains("actor is empty"));
        assert!(msg.contains("op #3 of the batch, 01X"), "{msg}");
        let e = refused(413, "");
        assert!(e.error.to_string().contains("HTTP 413"));
    }
}
