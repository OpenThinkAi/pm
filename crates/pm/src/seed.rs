//! The first `pm sync` of a workspace (AGT-1396, README §Sync & hub
//! "First push"): before the hub can be the workspace's authority it has
//! to hold the workspace's whole log — the Studio's ~13k ops and the
//! numbers the vault and pm minted before the hub existed. That upload is
//! the **seed**, and this module is the only place it happens; `pm sync`'s
//! push/pull ([`crate::sync`]) is the same afterwards as for any replica.
//!
//! The hub's side of the contract is `docs/hub-api.md` §Ticket numbers: a
//! workspace starts in *seed mode* (`GET whoami` → `seeded: false`), in
//! which pushed `field.set number` ops are recorded and none are
//! allocated; `POST /seeded {"number_floor"}` ends it and makes the hub
//! the number authority. This module drives that from the client:
//!
//! 1. **Decide.** `sync_state.seeded` (pm-store) says whether this replica
//!    has already been through here; `whoami` says whether the hub has.
//!    The two agree, or:
//!    - hub seeded, replica not, and the replica has **never pushed** yet
//!      holds ops → refused (exit 1). That log would be a second seed of a
//!      workspace the hub already has; a second machine joins from an
//!      empty replica (`pm init --join`) instead.
//!    - hub seeded, replica not, otherwise → the replica is a joined copy
//!      with nothing to seed, or this replica's own seed ended and the
//!      answer was lost: mark it seeded and carry on. Whatever the
//!      seed-end numbered arrives on the pull.
//!    - hub in seed mode and the replica has no log at all → refused: a
//!      joined replica cannot seed anything; the workspace that holds the
//!      log must sync first.
//!    - replica seeded, hub not → refused. The hub was restored from
//!      before the seed; re-seeding on top of that silently is not
//!      something to do at 3 am.
//! 2. **Probe.** One pull page from `since=0` tells whether the hub's
//!    workspace is empty (`head == 0`: a fresh seed) or already holds
//!    ops (a seed that was interrupted). A resumed seed is only ever
//!    *this* log's: every op in that page must be in the local log, else
//!    the hub workspace was seeded from somewhere else and the sync
//!    refuses. Ops the probe finds are marked pushed — the hub has them.
//! 3. **Push** the outbox in batches ([`crate::sync::push_all`]), a
//!    progress line per batch on stderr — **config ops first**
//!    ([`pm_store::Store::outbox_config`]), then everything else in
//!    local `seq` order, which is the order `pm doctor` replays in and
//!    the order a joined replica can apply page by page (migrations
//!    0007/0008 backfilled a legacy log's config ops at its end, after
//!    the ops that depend on them). Pushes are idempotent on `op_id`, so
//!    a resumed seed simply re-runs; ops the hub already stored answer
//!    `stored: false` and cost a round trip, nothing more.
//! 4. **End the seed**: `POST /seeded` with the local allocator floor
//!    (`workspace.number_floor`). The hub adopts the greater of that and
//!    the largest seeded number, numbers any create the seed left
//!    unnumbered (a ticket filed pending a hub number) and returns those
//!    ops; they are applied at once, the local floor is raised to the
//!    hub's, and `sync_state.seeded` is set. `409 already_seeded` — the
//!    seed had ended and the answer was lost — is the same outcome.
//!    Only now is the hub authoritative for this workspace; the round's
//!    pull then runs from cursor 0, which brings the seeded log back and
//!    skips it (this replica's own ops), once.
//!
//! An interruption anywhere leaves the next `pm sync` able to finish: the
//! marks and the cursor are the same commit-per-unit bookkeeping the plain
//! sync relies on, `whoami` re-reads the hub's mode, and the probe
//! re-establishes what the hub holds.

use anyhow::Context;
use pm_core::Op;
use pm_store::Store;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use ulid::Ulid;

use crate::exit::{CliError, Result};
use crate::hub::HubClient;
use crate::sync::{self, Limits, Outbox, Progress, Round};
use crate::verbs::Ctx;

/// What a seed did, for `pm sync`'s report.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub(crate) struct Seed {
    /// The hub already held part of this log when the seed started.
    pub resumed: bool,
    /// Ops the hub acknowledged during the seed's push.
    pub pushed: usize,
    /// The floor the hub adopted (the local floor after the seed).
    pub number_floor: u64,
    /// Creates the seed left unnumbered that the hub numbered at the end.
    pub numbered: usize,
}

/// Seeds the hub if this is the workspace's first sync; `None` when the
/// hub is already the authority (the usual case). See the module docs
/// for what is refused.
pub(crate) fn ensure_seeded(
    ctx: &Ctx<'_>,
    store: &mut Store,
    hub: &HubClient,
    limits: &Limits,
    round: &mut Round,
) -> Result<Option<Seed>> {
    let hub_seeded = whoami_seeded(hub)?;
    if store.seeded()? {
        if !hub_seeded {
            return Err(CliError::error(format!(
                "hub workspace {} is in seed mode, but this workspace's seed already ended there \
                 — the hub looks restored from before the seed; refusing to seed it again on top \
                 of what it holds (check the hub before syncing)",
                hub.workspace
            )));
        }
        return Ok(None);
    }

    let op_count = store.op_count()?;
    if hub_seeded {
        let outbox = store.outbox_len()?;
        if !store.ever_pushed()? && outbox > 0 {
            return Err(CliError::error(format!(
                "hub workspace {} is already seeded, and this workspace has never synced with it: \
                 its {outbox} local op(s) would be a second seed. A second machine joins a seeded \
                 hub from an empty replica (`pm init --join <workspace-id>`); to seed a hub from \
                 this log, point it at a workspace the hub has not seeded",
                hub.workspace
            )));
        }
        // Joined with nothing to seed, or this replica's seed ended and
        // the answer was lost. Either way the hub is the authority now.
        store.mark_seeded()?;
        return Ok(None);
    }
    if op_count == 0 {
        return Err(CliError::error(format!(
            "hub workspace {} has not been seeded yet, and this workspace has no log to seed it \
             with; run `pm sync` first on the workspace that holds the log",
            hub.workspace
        )));
    }

    // The hub is in seed mode and this replica has a log: seed, or
    // resume the seed.
    let probe = sync::pull_page(hub, 0)?;
    let resumed = probe.head > 0;
    if resumed {
        let ids = probe
            .ops
            .iter()
            .map(|item| {
                serde_json::from_value::<Ulid>(item.op["op_id"].clone())
                    .with_context(|| format!("hub seq {}: op has no op_id", item.seq))
            })
            .collect::<std::result::Result<Vec<Ulid>, _>>()?;
        let unknown = store.unknown_ops(&ids)?;
        if let Some(first) = unknown.first() {
            return Err(CliError::error(format!(
                "hub workspace {} is still seeding but already holds {} op(s) this workspace's \
                 log does not (e.g. {first}): it was seeded from another workspace; refusing to \
                 mix two logs — seed a hub workspace only from the workspace it belongs to",
                hub.workspace,
                unknown.len()
            )));
        }
        // The hub has these; they need not go up again.
        store.mark_pushed(&ids)?;
    }

    let to_push = store.outbox_len()? as usize;
    if resumed {
        eprintln!(
            "seed: resuming the seed of hub workspace {}: {to_push} of {op_count} op(s) still to push",
            crate::text::inline(&hub.workspace)
        );
    } else {
        eprintln!(
            "seed: seeding hub workspace {} with {op_count} op(s)",
            crate::text::inline(&hub.workspace)
        );
    }
    // Config ops first — the order `pm doctor` replays in. Migrations
    // 0007/0008 backfilled the config ops of a pre-AGT-1385 log at its
    // *end*, after the ticket and document ops that depend on them (the
    // Studio's seq 1 is a document edit whose `project.doc_add` sits at
    // seq 14206); a replica applying the hub's log page by page needs
    // every state, project and document binding ahead of those.
    let mut progress = Progress::seed(to_push);
    let before = round.pushed;
    sync::push_all(
        ctx,
        store,
        hub,
        limits,
        Outbox::Config,
        &mut progress,
        round,
    )?;
    sync::push_all(ctx, store, hub, limits, Outbox::All, &mut progress, round)?;
    let pushed = round.pushed - before;

    let floor = store.number_floor()?;
    let ended = end_seed(store, hub, floor)?;
    store.mark_seeded()?;
    eprintln!(
        "seed: ended{}; number floor {}; the hub numbered {} ticket(s)",
        if ended.already {
            " (it had already ended)"
        } else {
            ""
        },
        ended.number_floor,
        ended.numbered
    );
    Ok(Some(Seed {
        resumed,
        pushed,
        number_floor: ended.number_floor,
        numbered: ended.numbered,
    }))
}

/// `GET whoami` → the hub's `seeded`.
fn whoami_seeded(hub: &HubClient) -> Result<bool> {
    let (status, body) = hub.get("whoami").map_err(|e| sync::transport(hub, e))?;
    match status {
        200 => {}
        404 => return Err(sync::unauthorized(hub)),
        503 => return Err(sync::unavailable()),
        other => return Err(sync::unexpected(other, "whoami")),
    }
    let v: Value = serde_json::from_str(&body).context("the hub's whoami is not JSON")?;
    v["seeded"].as_bool().ok_or_else(|| {
        CliError::error("the hub's whoami does not say whether the workspace is seeded")
    })
}

/// The seed-end's outcome.
struct Ended {
    number_floor: u64,
    numbered: usize,
    /// The hub answered `409 already_seeded`: a previous seed-end had
    /// landed and its answer was lost. Its numbers arrive on the pull.
    already: bool,
}

/// `POST /seeded {"number_floor": floor}`; applies the ops the hub
/// numbered stragglers with and raises the local floor to the hub's.
fn end_seed(store: &mut Store, hub: &HubClient, floor: u64) -> Result<Ended> {
    #[derive(Deserialize)]
    struct Seeded {
        number_floor: u64,
        numbers: Vec<Numbered>,
    }
    #[derive(Deserialize)]
    struct Numbered {
        op: Value,
    }
    let body = json!({ "number_floor": floor }).to_string();
    let (status, body) = hub
        .post_json("seeded", &body)
        .map_err(|e| sync::transport(hub, e))?;
    match status {
        200 => {}
        409 => {
            let detail: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
            if detail["error"] == "already_seeded" {
                // The hub's adopted floor is unknown from a 409: report
                // the local one. Not a bug — the round's pull brings the
                // hub's number ops, and the local allocator reads the
                // greater of its floor and the numbers in use.
                return Ok(Ended {
                    number_floor: floor,
                    numbered: 0,
                    already: true,
                });
            }
            return Err(sync::refused(status, &body));
        }
        404 => return Err(sync::unauthorized(hub)),
        400 => return Err(sync::refused(status, &body)),
        503 => return Err(sync::unavailable()),
        other => return Err(sync::unexpected(other, "the seed end")),
    }
    let seeded: Seeded = serde_json::from_str(&body)
        .context("the hub's seed-end response is not the expected JSON")?;
    let ops = seeded
        .numbers
        .into_iter()
        .map(|n| {
            serde_json::from_value::<Op>(n.op)
                .context("a hub number op this build does not understand")
        })
        .collect::<std::result::Result<Vec<Op>, _>>()?;
    if !ops.is_empty() {
        store.apply_pulled(&ops)?;
    }
    store.raise_number_floor(seeded.number_floor)?;
    Ok(Ended {
        number_floor: seeded.number_floor,
        numbered: ops.len(),
        already: false,
    })
}
