//! `pm claim <id> [--branch b]` and `pm claim --ready [--project p]`
//! (AGT-1341; README §Conflict semantics "Claims", §Authority).
//!
//! A claim is the one op that is **not** a CRDT: `claim if state is
//! unstarted and unassigned`. The authority — in phases 1–2 this
//! machine's SQLite database — admits the first such op inside its
//! transaction and rejects every later one, so of any number of processes
//! claiming the same ticket exactly one exits 0; the rest exit
//! [`exit::TAKEN`] (75) and, with `--json`, learn `{taken_by, at}`.
//!
//! Nothing here proceeds on an unconfirmed claim: when the configured
//! authority is a hub this build cannot reach, the verb refuses up front
//! (AC3; the hub client is P3).

use std::collections::BTreeSet;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use pm_core::op::{Claim, FieldSet};
use pm_core::{ActorId, ClaimRejected, Hlc, Payload, StateCategory, Ticket, Workspace};
use pm_store::{ReadyQuery, Store, StoreError};
use serde_json::json;
use ulid::Ulid;

use crate::exit::{self, CliError, Result};
use crate::verbs::{self, Ctx, SCHEMA, Stamper};
use crate::workspace::{self, Config, Env};

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
    require_local_authority(ctx.env)?;
    let (mut store, ws) = with_busy_retry(|| workspace::open(&dir))?;
    let started = started_state(&ws)?;
    let mut claimer = Claimer {
        store: &mut store,
        ws: &ws,
        actor,
        started,
        branch,
    };

    match (args.id.as_deref(), args.ready) {
        (Some(reference), false) => {
            let ticket = verbs::find(claimer.store, &ws, reference)?;
            match claimer.attempt(ticket.id)? {
                Outcome::Claimed(ticket) => print_claimed(ctx, claimer.store, &ws, &ticket),
                Outcome::Taken(taken) => Err(taken.into_error(ctx, &ws)),
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

/// AC3: the authority is this database only while no hub is configured.
/// A configured hub is, for this build, always unreachable — the hub
/// client lands in P3 — and a claim the authority has not confirmed is no
/// claim at all, so refuse before touching anything.
fn require_local_authority(env: &Env) -> Result<()> {
    // No HOME at all means no config.toml, hence no hub: the database
    // named by --workspace / PM_WORKSPACE is the authority.
    let Ok(path) = env.config_path() else {
        return Ok(());
    };
    match Config::load(&path)?.and_then(|c| c.hub) {
        None => Ok(()),
        Some(hub) => Err(CliError::error(format!(
            "the claim authority for this machine is the hub at {hub} (`hub` in {}), which this \
             build cannot reach: the hub protocol is not implemented yet (P3). pm claim never \
             proceeds on an unconfirmed claim; unset `hub` to claim against the local database",
            path.display()
        ))),
    }
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
}

enum Outcome {
    Claimed(Ticket),
    Taken(Taken),
}

/// What a loser learns: who holds the ticket and since when (the
/// admitted claim's HLC — or, for a ticket that left `unstarted` some
/// other way, its last update).
struct Taken {
    ticket: Ticket,
    taken_by: Option<ActorId>,
    at: Hlc,
    reason: ClaimRejected,
}

impl Taken {
    fn into_error(self, ctx: &Ctx<'_>, ws: &Workspace) -> CliError {
        let id = verbs::display_id(ws, &self.ticket);
        if ctx.json {
            verbs::print_json(&json!({
                "schema": SCHEMA,
                "id": id,
                "ulid": self.ticket.id,
                "taken_by": self.taken_by,
                "at": self.at,
                "state": self.ticket.state,
                "reason": self.reason.to_string(),
            }));
        }
        let holder = match &self.taken_by {
            Some(actor) => format!("taken by {actor}"),
            None => format!("in state '{}'", self.ticket.state),
        };
        CliError {
            code: exit::TAKEN,
            error: anyhow::anyhow!("{id} is {holder} (since {}): {}", self.at, self.reason),
        }
    }
}

impl Claimer<'_> {
    /// One conditional claim of `id`. The `claim` op (and the `--branch`
    /// ext write, when given) commit in one transaction; the store runs
    /// `claim_admissible` inside it, so two attempts can never both land.
    fn attempt(&mut self, id: Ulid) -> Result<Outcome> {
        let result = with_busy_retry(|| {
            // Re-stamped on every retry: a busy attempt wrote nothing.
            let mut stamper = Stamper::new(self.store, self.actor.clone())?;
            let mut ops = vec![stamper.op(
                id,
                Payload::Claim(Claim {
                    state: self.started.clone(),
                    assignee: self.actor.clone(),
                }),
            )];
            if let Some(branch) = &self.branch {
                ops.push(stamper.op(
                    id,
                    Payload::FieldSet(FieldSet::Ext {
                        key: BRANCH_KEY.to_string(),
                        value: Some(json!(branch)),
                    }),
                ));
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
            return Err(CliError::not_found(format!(
                "{} is deleted",
                verbs::display_id(self.ws, &ticket)
            )));
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
            ticket,
            at,
            reason: rejected,
        }))
    }

    fn reload(&self, id: Ulid) -> Result<Ticket> {
        self.store
            .ticket(id)?
            .ok_or_else(|| CliError::error(format!("ticket {id} vanished during claim")))
    }
}

/// AC4: the lowest-numbered ready ticket, claimed. A candidate another
/// process takes — or tombstones — between the query and the claim is
/// skipped and the next one tried; exit 3 when nothing is left.
fn claim_ready(ctx: &Ctx<'_>, claimer: &mut Claimer<'_>, project: Option<&str>) -> Result<()> {
    let query = ReadyQuery {
        project: project.map(str::to_string),
        today: today_utc(),
        gate_labels: claimer.ws.gate_labels.clone(),
    };
    // A rejected candidate leaves the ready set by definition; tracking
    // them anyway makes termination a property of this loop, not of the
    // store's consistency.
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

fn print_claimed(ctx: &Ctx<'_>, store: &Store, ws: &Workspace, ticket: &Ticket) -> Result<()> {
    if ctx.json {
        verbs::print_json(&verbs::ticket_json(ws, store, ticket)?);
    } else {
        println!("{}", verbs::display_id(ws, ticket));
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

/// Today as `YYYY-MM-DD` in UTC, for the ready query's date gates.
fn today_utc() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let (y, m, d) = civil_from_days((secs / 86_400) as i64);
    format!("{y:04}-{m:02}-{d:02}")
}

/// Days since 1970-01-01 to a proleptic Gregorian `(year, month, day)`
/// (Howard Hinnant's `civil_from_days`).
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = yoe + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_from_days_matches_known_dates() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(365), (1971, 1, 1));
        assert_eq!(civil_from_days(11_016), (2000, 2, 29));
        assert_eq!(civil_from_days(20_724), (2026, 9, 28));
        assert_eq!(civil_from_days(-1), (1969, 12, 31));
    }

    #[test]
    fn today_is_an_iso_date() {
        let today = today_utc();
        assert_eq!(today.len(), 10, "{today}");
        assert_eq!(today.as_bytes()[4], b'-');
        assert_eq!(today.as_bytes()[7], b'-');
    }
}
