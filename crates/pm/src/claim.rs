//! `pm claim <id> [--branch b]` and `pm claim --ready [--project p]`
//! (AGT-1341, AGT-1397; README §Conflict semantics "Claims", §Authority).
//!
//! A claim is the one op that is **not** a CRDT: `claim if state is
//! unstarted and unassigned`. The authority admits the first such op and
//! rejects every later one, so of any number of processes claiming the
//! same ticket exactly one exits 0; the rest exit [`exit::TAKEN`] (75)
//! and, with `--json`, learn `{taken_by, at}`. Nothing here proceeds on an
//! unconfirmed claim.
//!
//! Which authority is [`authority`]'s one decision:
//!
//! - **No hub configured** — this machine's database. The store runs
//!   `claim_admissible` inside the commit transaction (`Store::commit_batch`),
//!   so two local attempts can never both land.
//! - **A hub configured** (`hub` in config.toml, `pm hub login`) — the hub,
//!   once it says the workspace is seeded. The verb syncs first, so its
//!   candidates and its clock are current, then pushes the `claim` op
//!   **alone** (`docs/hub-api.md` §Claims: a companion write in the same
//!   batch would be stored even when the claim is refused) and logs
//!   nothing until the hub has answered: an admitted claim is committed as
//!   the pulled op it now is, plus `--branch`'s `field.set ext` through the
//!   normal outbox; a refused one is reported exactly as a local `75` is,
//!   and nothing is written. A hub that cannot be reached, or that rejects
//!   the token, is exit 1 — "claims require the hub" — with nothing
//!   written: a claim the authority has not confirmed is no claim.
//! - **A hub configured, workspace not yet seeded** — this machine's
//!   database, as above. Until the seed ends (AGT-1396) the hub is not the
//!   authority and stores whatever the seed brings without arbitrating it,
//!   so a claim made now is local-authority history and goes up with the
//!   seed. The hub is still asked (`GET whoami`), so the decision is never
//!   a guess about the seed's progress.

use std::collections::BTreeSet;
use std::time::{Duration, Instant};

use pm_core::markers::date_from_ms;
use pm_core::op::{Claim, FieldSet};
use pm_core::{ActorId, ClaimRejected, Hlc, Payload, StateCategory, Ticket, Workspace};
use pm_store::{ReadyQuery, Store, StoreError};
use serde_json::{Value, json};
use ulid::Ulid;

use crate::exit::{self, CliError, Result};
use crate::hub::{self, HubClient};
use crate::sync;
use crate::verbs::{self, Ctx, SCHEMA, Stamper};
use crate::workspace;

pub struct ClaimArgs {
    pub id: Option<String>,
    pub ready: bool,
    pub project: Option<String>,
    pub branch: Option<String>,
}

/// How long a claim keeps retrying when SQLite reports the database busy
/// on top of the connection's own busy timeout. Twenty build loops
/// fanning out at once must never see a lock as "error"; a busy claim is
/// simply one that has not been decided yet.
const BUSY_RETRY: Duration = Duration::from_secs(30);

/// The `ext` key `--branch` is recorded under.
const BRANCH_KEY: &str = "branch";

/// The hub's `code` for a claim on a deleted ticket (`docs/hub-api.md`
/// §Claims): the local path's [`ClaimRejected::Deleted`], exit 3.
const CODE_DELETED: &str = "deleted";

pub fn claim(ctx: &Ctx<'_>, args: ClaimArgs) -> Result<()> {
    // Clap's `requires` is satisfied by a bool flag's default, so the
    // pairing is checked here.
    if args.project.is_some() && !args.ready {
        return Err(CliError::usage(
            "--project only narrows --ready; to claim one ticket pass its id",
        ));
    }
    let branch = args
        .branch
        .as_deref()
        .map(|b| {
            let b = b.trim();
            if b.is_empty() {
                Err(CliError::usage("--branch must not be empty"))
            } else {
                Ok(b.to_string())
            }
        })
        .transpose()?;
    let actor = ctx.actor()?;
    let dir = workspace::resolve(ctx.workspace, ctx.env)?;
    let (mut store, ws) = with_busy_retry(|| workspace::open(&dir))?;
    let authority = authority(ctx, &ws)?;
    if let Authority::Hub(hub) = &authority {
        // Current before deciding anything: the ready set reflects every
        // claim the hub has admitted, and the clock is past every pulled
        // op — a claim stamped before the unclaim it follows would be
        // admitted and then lose the LWW register (docs/hub-api.md).
        sync::run_once(ctx, &mut store, hub, &sync::Limits::from_env(ctx.env))
            .map_err(hub_required)?;
    }
    let started = started_state(&ws)?;
    let mut claimer = Claimer {
        store: &mut store,
        ws: &ws,
        actor,
        started,
        branch,
        hub: authority.hub(),
        ctx,
    };

    match (args.id.as_deref(), args.ready) {
        (Some(reference), false) => {
            let ticket = verbs::find(claimer.store, &ws, reference)?;
            match claimer.attempt(ticket.id)? {
                Outcome::Claimed(ticket) => print_claimed(ctx, claimer.store, &ws, &ticket),
                Outcome::Taken(taken) => {
                    if ctx.json {
                        verbs::print_json(&taken.json(&ws));
                    }
                    Err(taken.into_error(&ws))
                }
            }
        }
        (None, true) => {
            let project = args
                .project
                .as_deref()
                .map(str::trim)
                .filter(|p| !p.is_empty());
            if let Some(project) = project {
                verbs::require_project(claimer.store, project)?;
            }
            claim_ready(ctx, &mut claimer, project)
        }
        _ => Err(CliError::usage(
            "pass a ticket id (AGT-12) or --ready to claim the lowest-numbered ready ticket",
        )),
    }
}

/// Who admits this command's claims (module docs).
enum Authority {
    /// This machine's database: no hub is configured, or the workspace's
    /// seed has not ended yet.
    Local,
    /// The hub, which reports the workspace seeded.
    Hub(HubClient),
}

impl Authority {
    fn hub(&self) -> Option<&HubClient> {
        match self {
            Authority::Local => None,
            Authority::Hub(hub) => Some(hub),
        }
    }
}

/// **The** seeded/unseeded predicate — the one place that decides whether
/// a claim is arbitrated here or at the hub (module docs). No hub in
/// config.toml is local authority without any network. A configured hub
/// is asked `GET /w/{ws}/whoami`: `seeded: true` makes it the authority;
/// `seeded: false` (the seed has not ended, AGT-1396) leaves this
/// database the authority for now. A hub that cannot answer — unreachable,
/// no token held, token rejected — is exit 1, whatever the seed's state:
/// this build never guesses which side would have decided.
fn authority(ctx: &Ctx<'_>, ws: &Workspace) -> Result<Authority> {
    if hub::configured_hub(ctx.env)?.is_none() {
        return Ok(Authority::Local);
    }
    let hub = HubClient::resolve(ctx.env, ws, sync::TIMEOUT).map_err(hub_required)?;
    let (status, body) = hub
        .get("whoami")
        .map_err(|e| hub_required(sync::transport(&hub, e)))?;
    match status {
        200 => {}
        404 => return Err(hub_required(sync::unauthorized(&hub))),
        503 => return Err(hub_required(sync::unavailable())),
        other => return Err(hub_required(sync::unexpected(other, "whoami"))),
    }
    let seeded = serde_json::from_str::<Value>(&body)
        .ok()
        .and_then(|v| v.get("seeded")?.as_bool())
        .ok_or_else(|| {
            hub_required(CliError::error(format!(
                "the hub at {} answered whoami without a `seeded` flag",
                hub.url
            )))
        })?;
    Ok(if seeded {
        Authority::Hub(hub)
    } else {
        Authority::Local
    })
}

/// Exit 1, naming the rule (AC2) in front of what went wrong at the hub.
fn hub_required(e: CliError) -> CliError {
    CliError::error(format!(
        "claims require the hub (a claim the authority has not confirmed is no claim; nothing was written): {:#}",
        e.error
    ))
}

/// The state a claimed ticket moves to: the workspace's first `started`
/// state (Saltline: `in-progress`), mirroring how `pm new` picks the first
/// `unstarted` one.
fn started_state(ws: &Workspace) -> Result<String> {
    Ok(ws
        .states
        .iter()
        .filter(|s| s.category == StateCategory::Started)
        .min_by_key(|s| s.position)
        .ok_or_else(|| CliError::error("this workspace has no started state to claim into"))?
        .name
        .clone())
}

/// Everything a single claim attempt needs, so `--ready` can retry
/// across candidates without re-resolving any of it.
struct Claimer<'a> {
    store: &'a mut Store,
    ws: &'a Workspace,
    actor: ActorId,
    started: String,
    branch: Option<String>,
    /// `Some` when the hub arbitrates ([`authority`]).
    hub: Option<&'a HubClient>,
    ctx: &'a Ctx<'a>,
}

enum Outcome {
    Claimed(Ticket),
    Taken(Taken),
}

/// What a loser learns: who holds the ticket and since when (the
/// admitted claim's HLC — or, for a ticket that left `unstarted` some
/// other way, its last update), the ticket's state at the authority and
/// the authority's reason.
struct Taken {
    ticket: Ticket,
    taken_by: Option<ActorId>,
    at: Hlc,
    state: String,
    reason: String,
}

impl Taken {
    /// The `--json` payload a loser gets: `{taken_by, at}` plus context.
    fn json(&self, ws: &Workspace) -> serde_json::Value {
        json!({
            "schema": SCHEMA,
            "id": verbs::display_id(ws, &self.ticket),
            "ulid": self.ticket.id,
            "taken_by": self.taken_by,
            "at": self.at,
            "state": self.state,
            "reason": self.reason,
        })
    }

    /// The exit-75 error (a pure conversion; printing is the caller's).
    /// Names the ticket by reference (`ref_id`: its ULID while its number
    /// is pending, AGT-1398), so the message can be acted on.
    fn into_error(self, ws: &Workspace) -> CliError {
        let id = verbs::ref_id(ws, &self.ticket);
        let holder = match &self.taken_by {
            Some(actor) => format!("taken by {actor}"),
            None => format!("in state '{}'", self.state),
        };
        CliError {
            code: exit::TAKEN,
            error: anyhow::anyhow!("{id} is {holder} (since {}): {}", verbs::when(&self.at), self.reason),
        }
    }
}

impl Claimer<'_> {
    /// One conditional claim of `id`, decided by whichever authority
    /// [`authority`] chose.
    fn attempt(&mut self, id: Ulid) -> Result<Outcome> {
        match self.hub {
            Some(hub) => self.attempt_hub(hub, id),
            None => self.attempt_local(id),
        }
    }

    /// The `claim` op (and the `--branch` ext write, when given) commit in
    /// one transaction; the store runs `claim_admissible` inside it, so
    /// two attempts can never both land.
    fn attempt_local(&mut self, id: Ulid) -> Result<Outcome> {
        let claim = self.claim_payload();
        let branch = self.branch.as_deref().map(branch_payload);
        let result = with_busy_retry(|| {
            // Re-stamped on every retry: a busy attempt wrote nothing.
            let mut stamper = Stamper::new(self.store, self.actor.clone())?;
            let mut ops = vec![stamper.op(id, claim.clone())];
            if let Some(branch) = &branch {
                ops.push(stamper.op(id, branch.clone()));
            }
            self.store.commit_batch(&ops, &[]).map_err(CliError::from)
        });
        let rejected = match result {
            Ok(_) => return Ok(Outcome::Claimed(self.reload(id)?)),
            Err(e) => match e.error.downcast_ref::<StoreError>() {
                Some(StoreError::ClaimRejected(rejected)) => rejected.clone(),
                _ => return Err(e),
            },
        };
        let ticket = self.reload(id)?;
        if rejected == ClaimRejected::Deleted {
            return Err(self.deleted(&ticket));
        }
        let at = self
            .store
            .ops(id)?
            .into_iter()
            .filter(|op| matches!(op.payload, Payload::Claim(_)))
            .map(|op| op.hlc)
            .max()
            .unwrap_or(ticket.updated);
        Ok(Outcome::Taken(Taken {
            taken_by: ticket.assignee.clone(),
            state: ticket.state.clone(),
            ticket,
            at,
            reason: rejected.to_string(),
        }))
    }

    /// The hub decides (module docs): the `claim` op goes up alone and is
    /// logged here only once admitted — as a pulled op, so the store does
    /// not judge it again — followed by the `--branch` write, which is an
    /// ordinary local op pushed through the outbox.
    fn attempt_hub(&mut self, hub: &HubClient, id: Ulid) -> Result<Outcome> {
        // Stamped now, after the sync `claim` ran: past every op pulled.
        let mut stamper = Stamper::new(self.store, self.actor.clone())?;
        let claim = stamper.op(id, self.claim_payload());
        let ack = sync::push_claim(hub, &claim).map_err(hub_required)?;
        if let Some(rejected) = ack.rejected {
            let ticket = self.reload(id)?;
            if rejected.code == CODE_DELETED {
                return Err(self.deleted(&ticket));
            }
            return Ok(Outcome::Taken(Taken {
                ticket,
                taken_by: rejected.taken_by,
                at: rejected.at,
                state: rejected.state,
                reason: rejected.reason,
            }));
        }
        if !ack.admitted() {
            return Err(hub_required(CliError::error(format!(
                "the hub's acknowledgement of claim {} is neither stored nor rejected",
                claim.op_id
            ))));
        }
        with_busy_retry(|| {
            self.store
                .apply_pulled(std::slice::from_ref(&claim))
                .map_err(CliError::from)
        })?;
        if let Some(branch) = &self.branch {
            let op = stamper.op(id, branch_payload(branch));
            with_busy_retry(|| self.store.commit(&op).map_err(CliError::from))?;
            // Best effort: the claim is admitted either way, and the write
            // stays in the outbox for the next sync if this push fails.
            let mut round = Default::default();
            if let Err(e) = sync::push_all(
                self.ctx,
                self.store,
                hub,
                &sync::Limits::from_env(self.ctx.env),
                sync::Outbox::All,
                &mut sync::Progress::quiet(),
                &mut round,
            ) {
                eprintln!(
                    "pm: the claim is admitted; its --branch write stays in the outbox until the next `pm sync`: {:#}",
                    e.error
                );
            }
        }
        Ok(Outcome::Claimed(self.reload(id)?))
    }

    fn claim_payload(&self) -> Payload {
        Payload::Claim(Claim {
            state: self.started.clone(),
            assignee: self.actor.clone(),
        })
    }

    fn deleted(&self, ticket: &Ticket) -> CliError {
        CliError::not_found(format!("{} is deleted", verbs::ref_id(self.ws, ticket)))
    }

    fn reload(&self, id: Ulid) -> Result<Ticket> {
        self.store
            .ticket(id)?
            .ok_or_else(|| CliError::error(format!("ticket {id} vanished during claim")))
    }
}

fn branch_payload(branch: &str) -> Payload {
    Payload::FieldSet(FieldSet::Ext {
        key: BRANCH_KEY.to_string(),
        value: Some(json!(branch)),
    })
}

/// AC4: the lowest-numbered ready ticket, claimed. A candidate another
/// process takes — or tombstones — between the query and the claim is
/// skipped and the next one tried; exit 3 when nothing is left. Against
/// the hub the local ready set can be a step behind (a claim admitted
/// since the sync), so a `75` from the hub is the same skip.
fn claim_ready(ctx: &Ctx<'_>, claimer: &mut Claimer<'_>, project: Option<&str>) -> Result<()> {
    let query = ReadyQuery {
        project: project.map(str::to_string),
        today: date_from_ms(verbs::now_ms()),
        gate_labels: claimer.ws.gate_labels.clone(),
    };
    // A rejected candidate leaves the local ready set by definition only
    // when this database decided; tracking them makes termination a
    // property of this loop, not of the store's consistency.
    let mut tried: BTreeSet<Ulid> = BTreeSet::new();
    loop {
        let candidate = claimer
            .store
            .ready(&query)?
            .into_iter()
            .find(|t| !tried.contains(&t.id));
        let Some(candidate) = candidate else {
            let scope = project.map_or(String::new(), |p| format!(" in project '{p}'"));
            return Err(CliError::not_found(format!(
                "no ready ticket{scope}: nothing unstarted, unassigned, unblocked and ungated"
            )));
        };
        tried.insert(candidate.id);
        match claimer.attempt(candidate.id) {
            Ok(Outcome::Claimed(ticket)) => {
                return print_claimed(ctx, claimer.store, claimer.ws, &ticket);
            }
            // Taken by someone else, or deleted (`attempt`'s not-found)
            // inside the race window: neither is this call's failure.
            Ok(Outcome::Taken(_)) => {}
            Err(e) if e.code == exit::NOT_FOUND => {}
            Err(e) => return Err(e),
        }
    }
}

/// The claimed ticket: **Ticket** under `--json`, else its id — `AGT-12`,
/// or `AGT-?  <ULID>` while the hub's number is pending (AGT-1398), the
/// line `pm new` prints, so the reference to use next is right there.
fn print_claimed(ctx: &Ctx<'_>, store: &Store, ws: &Workspace, ticket: &Ticket) -> Result<()> {
    if ctx.json {
        verbs::print_json(&verbs::ticket_json(ws, store, ticket)?);
    } else {
        println!("{}", verbs::created_line(ws, ticket));
    }
    Ok(())
}

/// Runs `f` again while it fails with SQLite's busy/locked codes, backing
/// off up to [`BUSY_RETRY`]. Any other outcome is returned as is.
fn with_busy_retry<T>(mut f: impl FnMut() -> Result<T>) -> Result<T> {
    let deadline = Instant::now() + BUSY_RETRY;
    let mut wait = Duration::from_millis(5 + u64::from(std::process::id() % 7));
    loop {
        match f() {
            Err(e) if is_busy(&e) && Instant::now() < deadline => {
                std::thread::sleep(wait);
                wait = (wait * 2).min(Duration::from_millis(250));
            }
            other => return other,
        }
    }
}

fn is_busy(e: &CliError) -> bool {
    e.error
        .downcast_ref::<StoreError>()
        .is_some_and(StoreError::is_busy)
}
