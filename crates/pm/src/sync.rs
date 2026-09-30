//! `pm sync [--watch <secs>]` (AGT-1395): the client half of README
//! §Sync & hub. Local ops go up, everyone else's come down, and both sides
//! converge by the same `pm-core` merge rules — the hub only orders.
//!
//! One sync is **seed if needed, then push, then pull**, each in units the
//! database commits on its own:
//!
//! - **Seed** ([`crate::seed`], AGT-1396). A workspace whose hub does not
//!   yet hold its log — `GET whoami` says `seeded: false` — uploads the
//!   whole log first and ends the hub's seed mode, so the hub becomes the
//!   number authority only once it has everything. The seed module decides
//!   between seeding, resuming an interrupted seed, joining an already
//!   seeded hub from an empty replica, and refusing; after it has run once
//!   (`sync_state.seeded`) every later sync is push and pull alone.
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
//! - **Rejected claims.** The one per-op outcome inside a `200` is a
//!   refused `claim` (`docs/hub-api.md` §Claims, AGT-1392): its ack is
//!   `{seq: null, stored: false, rejected: {…}}` and nothing was stored.
//!   It is *acknowledged and dropped*: marked pushed like any other ack so
//!   it leaves the outbox, then reconciled — the local log keeps the claim
//!   as admitted (append-only; `pm doctor --rebuild` replays it as such),
//!   so a compensating `state.transition` to the hub's `state` and a
//!   `field.set assignee` to its `taken_by`, stamped now, are logged and
//!   go up with the rest of the outbox; both replicas then converge on the
//!   hub's answer. Each rejection is reported on stderr and listed under
//!   `rejected` in `--json`. This only happens to a claim the local
//!   database admitted while it was the authority (a seed, or a log joined
//!   after another machine's seed): `pm claim` itself goes to the hub
//!   (`crate::claim`, AGT-1397) and never leaves a refused claim behind.
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
//!
//! Two environment hooks exist for the seed tests and nothing else
//! (`crate::workspace::Env`): `PM_SYNC_TEST_BATCH_OPS` shrinks the push
//! batch so a small log takes many batches, and
//! `PM_SYNC_TEST_CRASH_AFTER_BATCHES=n` exits the process after the hub
//! has acknowledged `n` batches and before the `n`th is marked pushed —
//! the worst place a crash can land.

use std::thread::sleep;
use std::time::Duration;

use anyhow::Context;
use pm_core::op::{FieldSet, StateTransition};
use pm_core::{ActorId, Hlc, Op, Payload, Workspace};
use pm_store::Store;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use ulid::Ulid;

use crate::exit::{CliError, Result};
use crate::hub::{HubClient, Transport};
use crate::seed::{self, Seed};
use crate::verbs::{Ctx, SCHEMA, Stamper, print_json};
use crate::workspace::Env;

/// The hub's per-batch op cap (`pm_hub::ops::MAX_BATCH_OPS`, mirrored
/// here: the hub is a binary crate).
pub const MAX_BATCH_OPS: usize = 1000;
/// The hub's per-request body cap (`pm_hub::ops::MAX_BODY_BYTES`).
pub const MAX_BATCH_BYTES: usize = 64 * 1024 * 1024;
/// Ops asked for per pull page (the hub clamps at 1000).
pub(crate) const PAGE_LIMIT: usize = 1000;
/// Per request. A batch can be 64 MiB, and the Studio's uplink is not a
/// data centre's. `pub(crate)`: `pm claim` (`crate::claim`) syncs and
/// pushes through the same client.
pub(crate) const TIMEOUT: Duration = Duration::from_secs(300);

pub struct SyncArgs {
    /// Re-sync every this many seconds, forever.
    pub watch: Option<u64>,
}

/// How pushes are cut, and where a test wants the process to die.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Limits {
    /// Ops per push batch: [`MAX_BATCH_OPS`] unless a test shrinks it.
    pub batch_ops: usize,
    /// `PM_SYNC_TEST_CRASH_AFTER_BATCHES` (module docs).
    pub crash_after_batches: Option<usize>,
}

impl Limits {
    pub(crate) fn from_env(env: &Env) -> Limits {
        Limits {
            batch_ops: env
                .sync_test_batch_ops
                .map_or(MAX_BATCH_OPS, |n| n.clamp(1, MAX_BATCH_OPS)),
            crash_after_batches: env.sync_test_crash_after_batches,
        }
    }
}

/// What one round moved.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Round {
    /// The seed this round ran, if it was the workspace's first sync.
    seed: Option<Seed>,
    /// Outbox ops the hub acknowledged this round (rejected claims
    /// included: they are acknowledged too; the seed's push too).
    /// `pub(crate)`: the seed reads how much its two passes moved.
    pub(crate) pushed: usize,
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
    /// Outbox claims the hub refused this round, each reconciled locally.
    rejected: Vec<RejectedClaim>,
}

pub fn sync(ctx: &Ctx<'_>, args: SyncArgs) -> Result<()> {
    let (mut store, ws) = ctx.open()?;
    let hub = HubClient::resolve(ctx.env, &ws, TIMEOUT)?;
    let limits = Limits::from_env(ctx.env);
    match args.watch {
        None => {
            let round = run_once(ctx, &mut store, &hub, &limits)?;
            report(ctx, &store, &ws, &hub, &round, false)
        }
        Some(secs) => loop {
            match run_once(ctx, &mut store, &hub, &limits) {
                Ok(round) => report(ctx, &store, &ws, &hub, &round, true)?,
                Err(e) => eprintln!("pm: sync failed: {:#}", e.error),
            }
            sleep(Duration::from_secs(secs));
        },
    }
}

/// One seed-if-needed, push, pull round. `pub(crate)`: `pm claim` runs
/// one before it asks the hub for a claim, so its candidates and stamps
/// are current.
pub(crate) fn run_once(
    ctx: &Ctx<'_>,
    store: &mut Store,
    hub: &HubClient,
    limits: &Limits,
) -> Result<Round> {
    let mut round = Round::default();
    round.seed = seed::ensure_seeded(ctx, store, hub, limits, &mut round)?;
    push_all(
        ctx,
        store,
        hub,
        limits,
        Outbox::All,
        &mut Progress::quiet(),
        &mut round,
    )?;
    pull_all(store, hub, &mut round)?;
    Ok(round)
}

// ------------------------------------------------------------------- push

/// Where a long push reports to. A plain sync says nothing per batch; a
/// seed ([`Progress::seed`]) prints a line per batch on stderr, since the
/// Studio's seed is thousands of ops and tens of megabytes.
pub(crate) struct Progress {
    total: usize,
    pushed: usize,
    bytes: usize,
    loud: bool,
}

impl Progress {
    pub(crate) fn quiet() -> Progress {
        Progress {
            total: 0,
            pushed: 0,
            bytes: 0,
            loud: false,
        }
    }

    /// Reports every batch of a push of `total` ops.
    pub(crate) fn seed(total: usize) -> Progress {
        Progress {
            total,
            pushed: 0,
            bytes: 0,
            loud: true,
        }
    }

    fn batch(&mut self, ops: usize, bytes: usize) {
        self.pushed += ops;
        self.bytes += bytes;
        if self.loud {
            eprintln!(
                "seed: {}/{} op(s) pushed ({:.1} MiB)",
                self.pushed,
                self.total,
                self.bytes as f64 / (1024.0 * 1024.0)
            );
        }
    }
}

/// Which part of the outbox a push drains.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Outbox {
    /// Everything, in local `seq` order: a plain sync.
    All,
    /// Only the config ops ([`Store::outbox_config`]): the seed's first
    /// pass, so the hub's log has every state, project and document
    /// binding ahead of the ops that need them.
    Config,
}

/// Pushes the outbox (`which` part of it), batch by batch, marking each
/// batch as the hub acknowledges it and reconciling any claim it refuses
/// (module docs); the compensating ops a rejection logs join the outbox
/// and go up in a later batch of the same call. `pub(crate)`: `pm claim
/// --branch` pushes its companion write this way once the hub has
/// admitted the claim, and the seed pushes through it twice.
pub(crate) fn push_all(
    ctx: &Ctx<'_>,
    store: &mut Store,
    hub: &HubClient,
    limits: &Limits,
    which: Outbox,
    progress: &mut Progress,
    round: &mut Round,
) -> Result<()> {
    let mut batches = 0;
    loop {
        let outbox = match which {
            Outbox::All => store.outbox(limits.batch_ops)?,
            Outbox::Config => store.outbox_config(limits.batch_ops)?,
        };
        if outbox.is_empty() {
            return Ok(());
        }
        let batch = Batch::take(&outbox, MAX_BATCH_BYTES)?;
        let acks = post_batch(hub, &batch)?;
        batches += 1;
        if limits.crash_after_batches == Some(batches) {
            // Test hook (module docs): the hub has this batch, the outbox
            // still does — exactly what a crash here would leave behind.
            eprintln!("pm: PM_SYNC_TEST_CRASH_AFTER_BATCHES: exiting after batch {batches}");
            std::process::exit(1);
        }
        // Only what the hub acknowledged leaves the outbox — a refused
        // claim included: the hub decided it. Anything it did not name
        // stays and goes up again next round.
        let acked: Vec<Ulid> = acks.iter().map(|a| a.op_id).collect();
        store.mark_pushed(&acked)?;
        round.pushed += acked.len();
        progress.batch(acked.len(), batch.body.len());
        // Reconcile after the mark: a crash between the two leaves the
        // claim out of the outbox rather than re-pushing it, since a re-push
        // is a fresh claim the hub may then admit — which would make the
        // compensation below wrong. The window is one local transaction.
        for ack in &acks {
            let Some(rejected) = &ack.rejected else {
                continue;
            };
            let (_, op) = outbox
                .iter()
                .find(|(_, op)| op.op_id == ack.op_id)
                .expect("acknowledged returns only this batch's ops");
            reconcile(ctx, store, op, rejected)?;
            let claim = RejectedClaim {
                op_id: op.op_id,
                ticket: op.entity,
                rejected: rejected.clone(),
            };
            eprintln!("pm: {claim}");
            round.rejected.push(claim);
        }
        if acked.len() < batch.op_ids.len() {
            return Err(CliError::error(format!(
                "the hub acknowledged {} of the {} ops sent; run `pm sync` again",
                acked.len(),
                batch.op_ids.len()
            )));
        }
    }
}

/// `POST /ops` with `batch`: the hub's acknowledgement of every op in it,
/// in batch order. Every non-`200` is the error `pm sync` reports; so is a
/// `200` that acknowledges none of the batch.
fn post_batch(hub: &HubClient, batch: &Batch) -> Result<Vec<Ack>> {
    let (status, body) = hub
        .post_json("ops", &batch.body)
        .map_err(|e| transport(hub, e))?;
    match status {
        200 => {}
        404 => return Err(unauthorized(hub)),
        400 | 413 => return Err(refused(status, &body)),
        503 => return Err(unavailable()),
        other => return Err(unexpected(other, "the push")),
    }
    let acks = batch.acknowledged(&body)?;
    if acks.is_empty() {
        return Err(CliError::error(format!(
            "the hub answered the push with no acknowledgement for any of the {} ops sent",
            batch.op_ids.len()
        )));
    }
    Ok(acks)
}

/// Pushes one `claim` op on its own — nothing else may ride along, since
/// the rest of a batch is stored even when the claim is refused — and
/// returns the hub's verdict on it. The op is not in the local log; the
/// caller (`crate::claim`) commits it only once admitted.
pub(crate) fn push_claim(hub: &HubClient, claim: &Op) -> Result<Ack> {
    debug_assert!(matches!(claim.payload, Payload::Claim(_)));
    let batch = Batch::take(std::slice::from_ref(&(0, claim.clone())), MAX_BATCH_BYTES)?;
    let mut acks = post_batch(hub, &batch)?;
    Ok(acks.swap_remove(0))
}

/// Logs the writes that bring a ticket to the hub's answer on a refused
/// claim: its `state` and its `taken_by`, stamped now so they win the LWW
/// registers over the admitted-locally claim (`docs/hub-api.md` §Claims,
/// "What a client does with `rejected`").
fn reconcile(ctx: &Ctx<'_>, store: &mut Store, claim: &Op, rejected: &Rejected) -> Result<()> {
    let mut stamper = Stamper::new(store, ctx.actor()?)?;
    let ops = [
        stamper.op(
            claim.entity,
            Payload::StateTransition(StateTransition {
                state: rejected.state.clone(),
            }),
        ),
        stamper.op(
            claim.entity,
            Payload::FieldSet(FieldSet::Assignee(rejected.taken_by.clone())),
        ),
    ];
    store.commit_batch(&ops, &[]).map(drop).map_err(|e| {
        let CliError { code, error } = CliError::from(e);
        CliError {
            code,
            error: error.context(format!(
                "reconciling ticket {} after the hub refused claim {}",
                claim.entity, claim.op_id
            )),
        }
    })
}

/// One entry of the hub's push acknowledgement (`docs/hub-api.md` §push).
#[derive(Clone, Debug, Deserialize)]
pub(crate) struct Ack {
    pub op_id: Ulid,
    /// The op's hub seq; `null` for a refused claim (nothing was stored).
    #[serde(default)]
    pub seq: Option<i64>,
    /// `false` when the hub already had the op (its seq is the existing
    /// one) — or refused it, in which case `rejected` says why.
    #[serde(default)]
    pub stored: bool,
    /// Present only on a refused `claim`.
    #[serde(default)]
    pub rejected: Option<Rejected>,
}

impl Ack {
    /// The hub holds this op: stored now, or already before (idempotent
    /// re-push). A refused claim is neither.
    pub(crate) fn admitted(&self) -> bool {
        self.rejected.is_none() && (self.stored || self.seq.is_some())
    }
}

/// The hub's verdict on a refused `claim` (`docs/hub-api.md` §Claims): the
/// same fields `pm claim --json` prints on a local `75`, plus `code`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Rejected {
    /// The ticket's assignee, or `null` when it left `unstarted` without
    /// one (done, canceled) or is deleted.
    pub taken_by: Option<ActorId>,
    /// When the ticket entered the refusing condition — for a claimed
    /// ticket, the admitted claim's HLC.
    pub at: Hlc,
    /// The ticket's state at the hub.
    pub state: String,
    /// `not_unstarted` | `already_assigned` | `deleted`.
    pub code: String,
    /// `pm_core::ClaimRejected`'s message.
    pub reason: String,
}

/// A refused outbox claim as `pm sync` reports it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct RejectedClaim {
    pub op_id: Ulid,
    pub ticket: Ulid,
    #[serde(flatten)]
    pub rejected: Rejected,
}

impl std::fmt::Display for RejectedClaim {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let holder = match &self.rejected.taken_by {
            Some(actor) => format!("taken by {actor}"),
            None => format!("in state '{}'", self.rejected.state),
        };
        write!(
            f,
            "the hub refused claim {} on ticket {}: {holder} since {} ({}); \
             reconciled locally to the hub's answer",
            self.op_id,
            self.ticket,
            crate::verbs::when(&self.rejected.at),
            self.rejected.reason
        )
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

    /// The acknowledgements in the hub's `200` body (`{"ops": [{"op_id",
    /// "seq", "stored", "rejected"?}, …]}`) for the ops of this batch, in
    /// batch order. An acknowledgement for an op that was not sent is
    /// ignored; an op acknowledged twice takes the first entry.
    fn acknowledged(&self, body: &str) -> Result<Vec<Ack>> {
        #[derive(Deserialize)]
        struct Acks {
            ops: Vec<Ack>,
        }
        let acks: Acks = serde_json::from_str(body)
            .context("the hub's push response is not the expected JSON")?;
        let by_id: std::collections::BTreeMap<Ulid, Ack> =
            acks.ops.into_iter().rev().map(|a| (a.op_id, a)).collect();
        Ok(self
            .op_ids
            .iter()
            .filter_map(|id| by_id.get(id).cloned())
            .collect())
    }
}

// ------------------------------------------------------------------- pull

/// One page of `GET /ops`, as `docs/hub-api.md` describes it.
#[derive(Deserialize)]
pub(crate) struct Page {
    pub(crate) ops: Vec<Item>,
    pub(crate) next: i64,
    pub(crate) head: i64,
}

#[derive(Deserialize)]
pub(crate) struct Item {
    pub(crate) seq: i64,
    pub(crate) op: Value,
}

/// `GET /ops?since=<since>&limit=` — one page, or the sync's error for
/// whatever the hub answered instead. `pub(crate)`: the seed's probe.
pub(crate) fn pull_page(hub: &HubClient, since: i64) -> Result<Page> {
    let (status, body) = hub
        .get(&format!("ops?since={since}&limit={PAGE_LIMIT}"))
        .map_err(|e| transport(hub, e))?;
    match status {
        200 => {}
        404 => return Err(unauthorized(hub)),
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
    Ok(page)
}

/// Pulls from the cursor to the hub's head, a page per transaction, and
/// records the outcome on `round`.
fn pull_all(store: &mut Store, hub: &HubClient, round: &mut Round) -> Result<()> {
    let mut since = store.cursor()?;
    loop {
        let page = pull_page(hub, since)?;
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

/// `pub(crate)`: `crate::claim`'s probe of the hub reports the same way.
pub(crate) fn transport(hub: &HubClient, Transport(e): Transport) -> CliError {
    CliError::error(format!(
        "cannot reach the hub at {}: {e} (nothing was changed locally; the outbox and cursor are as they were)",
        hub.url
    ))
}

/// The hub's bare `404`: a rejected token or an unknown workspace.
pub(crate) fn unauthorized(hub: &HubClient) -> CliError {
    CliError::error(format!(
        "the hub at {} rejected the token or does not know workspace {} (HTTP 404) — run `pm hub status`",
        hub.url, hub.workspace
    ))
}

/// A structured `4xx`: `{"error", "reason", …}` (`docs/hub-api.md`).
/// `pub(crate)`: the seed's end reports the same way.
pub(crate) fn refused(status: u16, body: &str) -> CliError {
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

pub(crate) fn unavailable() -> CliError {
    CliError::error("the hub cannot reach its database right now (HTTP 503); try again")
}

pub(crate) fn unexpected(status: u16, what: &str) -> CliError {
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
            "seed": round.seed,
            "seeded": state.seeded,
            "pushed": round.pushed,
            "pulled": round.pulled,
            "applied": round.applied,
            "skipped": round.skipped,
            "cursor": round.cursor,
            "head": round.head,
            "outbox": state.outbox,
            "pending_numbers": state.pending_numbers,
            "rejected": round.rejected,
        });
        if compact {
            println!("{value}");
        } else {
            print_json(&value);
        }
    } else {
        let mut line = String::new();
        if let Some(seed) = &round.seed {
            line.push_str(&format!(
                "seeded hub workspace {} ({}{} op(s), number floor {}, {} ticket(s) numbered by the hub); ",
                hub.workspace,
                if seed.resumed { "resumed; " } else { "" },
                seed.pushed,
                seed.number_floor,
                seed.numbered
            ));
        }
        line.push_str(&format!(
            "pushed {} op(s); pulled {} op(s): {} applied, {} skipped; cursor {} (hub head {})",
            round.pushed, round.pulled, round.applied, round.skipped, round.cursor, round.head
        ));
        if state.outbox > 0 {
            line.push_str(&format!("; {} op(s) still in the outbox", state.outbox));
        }
        if state.pending_numbers > 0 {
            line.push_str(&format!(
                "; {} ticket(s) still awaiting a hub number",
                state.pending_numbers
            ));
        }
        if !round.rejected.is_empty() {
            line.push_str(&format!(
                "; {} claim(s) refused by the hub and reconciled",
                round.rejected.len()
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
        let acks = batch.acknowledged(&body).unwrap();
        let ids: Vec<Ulid> = acks.iter().map(|a| a.op_id).collect();
        assert_eq!(ids, [outbox[0].1.op_id, outbox[2].1.op_id]);
        assert_eq!((acks[0].seq, acks[0].stored), (Some(7), false));
        assert!(acks[0].admitted(), "already held is admitted");
        assert_eq!((acks[1].seq, acks[1].stored), (Some(9), true));
        assert!(batch.acknowledged("not json").is_err());
        assert!(batch.acknowledged("{\"ops\": []}").unwrap().is_empty());
    }

    /// A refused claim's ack has `seq: null` (`docs/hub-api.md` §Claims);
    /// it must not break the parse of the whole response (AGT-1392 shape,
    /// which AGT-1395's `seq: i64` could not read).
    #[test]
    fn a_rejected_claim_ack_parses_alongside_the_rest() {
        let outbox = [comment("a"), comment("b")];
        let batch = Batch::take(&outbox, MAX_BATCH_BYTES).unwrap();
        let body = json!({"ops": [
            {"op_id": outbox[0].1.op_id, "seq": null, "stored": false,
             "rejected": {"taken_by": "claude:pm-build",
                          "at": {"wall_ms": 1_790_000_000_200_u64, "counter": 3},
                          "state": "in-progress", "code": "not_unstarted",
                          "reason": "ticket is in state 'in-progress', which is not unstarted"}},
            {"op_id": outbox[1].1.op_id, "seq": 12, "stored": true},
        ], "numbers": []})
        .to_string();
        let acks = batch.acknowledged(&body).unwrap();
        assert_eq!(acks.len(), 2);
        assert!(!acks[0].admitted());
        assert_eq!(acks[0].seq, None);
        let rejected = acks[0].rejected.as_ref().unwrap();
        assert_eq!(
            *rejected,
            Rejected {
                taken_by: Some(ActorId::new("claude:pm-build")),
                at: Hlc::new(1_790_000_000_200, 3),
                state: "in-progress".into(),
                code: "not_unstarted".into(),
                reason: "ticket is in state 'in-progress', which is not unstarted".into(),
            }
        );
        assert!(acks[1].admitted() && acks[1].rejected.is_none());
        // The report flattens the verdict next to the op and ticket ids.
        let report = serde_json::to_value(RejectedClaim {
            op_id: outbox[0].1.op_id,
            ticket: outbox[0].1.entity,
            rejected: rejected.clone(),
        })
        .unwrap();
        assert_eq!(report["op_id"], outbox[0].1.op_id.to_string());
        assert_eq!(report["ticket"], outbox[0].1.entity.to_string());
        assert_eq!(report["taken_by"], "claude:pm-build");
        assert_eq!(report["code"], "not_unstarted");
        assert_eq!(report["at"]["counter"], 3);
    }

    #[test]
    fn limits_clamp_the_test_batch_size_to_the_hub_cap() {
        let mut env = Env::default();
        assert_eq!(Limits::from_env(&env).batch_ops, MAX_BATCH_OPS);
        env.sync_test_batch_ops = Some(0);
        assert_eq!(Limits::from_env(&env).batch_ops, 1);
        env.sync_test_batch_ops = Some(7);
        assert_eq!(Limits::from_env(&env).batch_ops, 7);
        env.sync_test_batch_ops = Some(MAX_BATCH_OPS * 2);
        assert_eq!(Limits::from_env(&env).batch_ops, MAX_BATCH_OPS);
        assert_eq!(Limits::from_env(&env).crash_after_batches, None);
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
