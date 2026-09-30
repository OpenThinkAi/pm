//! Ticket-number allocation (AGT-1391, README §Sync & hub "Conditional
//! ops at the hub"): human numbers are allocated by one authority and
//! never merged, so a number is never issued twice across machines and
//! never collides with one the vault or the Studio minted before the hub
//! existed (`number_floor`, AGT-1347).
//!
//! **Two modes per workspace.** A workspace starts in *seed mode*
//! (`workspaces.seeded_at IS NULL`, the state `token create` makes it in):
//! the first sync (AGT-1396) uploads the client's existing log, whose
//! tickets already carry their own `field.set number` ops, and the hub
//! records those numbers without allocating any. `POST /w/{id}/seeded`
//! ([`finish_seed`]) ends the seed: it sets the floor to the greater of the
//! client's floor and the largest seeded number, numbers any create the
//! seed left unnumbered (a ticket made between `pm hub login` and the
//! first sync is pending a hub number), and marks the workspace
//! *authoritative*. From then on the push path ([`crate::ops`]) allocates
//! `next_number` for every fresh `ticket.create` in the same transaction
//! as the push, appends the hub's own `field.set number` op (actor
//! `hub`) right after the batch, and refuses any pushed `field.set
//! number` with a structured 400 — after seeding, only the hub numbers
//! tickets.
//!
//! **Never twice.** Allocation runs under the same per-workspace row
//! lock that orders seqs (one push, or one seed-end, at a time per
//! workspace), `next_number` lives on that locked row, and the `numbers`
//! table's unique `(workspace, number)` key makes a double issue a
//! constraint violation rather than a silent duplicate. A re-pushed
//! create is known by its `op_id` and its ticket is already in `numbers`,
//! so it allocates nothing.
//!
//! **The hub's op orders after the create.** The hub keeps one
//! [`pm_core::Clock`] per workspace on the locked row. Before allocating
//! it observes the greatest HLC of the batch it is answering; each
//! allocation stamps the hub op with [`Clock::receive`] of the create's
//! HLC, which is strictly greater than the create's stamp and than every
//! stamp the hub issued before. Actor `hub` is reserved for these ops, so
//! their `(hlc, actor)` stamps are unique and the client's LWW register
//! for `number` sees them as the latest write for that ticket.

use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use pm_core::op::FieldSet;
use pm_core::{ActorId, Clock, Hlc, Op, Payload};
use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;
use tokio_postgres::Transaction;
use ulid::Ulid;

use crate::Db;
use crate::auth::Authed;
use crate::ops::{ErrorBody, OpRow, insert_ops};

/// The actor every hub-authored op carries.
pub const HUB_ACTOR: &str = "hub";

/// A ticket's number as the push and seed-end responses report it.
#[derive(Serialize)]
pub struct Numbered {
    /// The ticket (the `ticket.create`'s `entity`).
    pub entity: String,
    pub number: i64,
    /// The seq of the `field.set number` op that issued it.
    pub seq: i64,
    /// That op, exactly as the log stores it — a client can apply it
    /// before pulling.
    pub op: Box<RawValue>,
    /// `true` if this request allocated the number; `false` if the
    /// ticket already had one (a re-pushed create, or a seeded number).
    pub allocated: bool,
}

/// A `ticket.create` awaiting a number.
pub struct Create {
    pub entity: Ulid,
    pub hlc: Hlc,
}

/// The allocator state of one workspace, read from its row under the
/// push lock (`FOR NO KEY UPDATE`) and written back by [`Allocator::save`].
pub struct Allocator {
    pub seeded: bool,
    next_number: i64,
    clock: Clock,
}

impl Allocator {
    /// Locks `workspace`'s row until the transaction ends and reads its
    /// allocator state; `None` if there is no such workspace.
    pub async fn lock(
        tx: &Transaction<'_>,
        workspace: &str,
    ) -> Result<Option<Allocator>, tokio_postgres::Error> {
        let row = tx
            .query_opt(
                "SELECT seeded_at IS NOT NULL, next_number, clock_wall_ms, clock_counter
                 FROM workspaces WHERE id = $1 FOR NO KEY UPDATE",
                &[&workspace],
            )
            .await?;
        Ok(row.map(|row| Allocator {
            seeded: row.get(0),
            next_number: row.get(1),
            clock: Clock::from_latest(hlc_from_row(row.get(2), row.get(3))),
        }))
    }

    /// Folds a stamp the hub has now seen into its clock, without issuing
    /// one: the next hub op will be stamped after it.
    pub fn observe(&mut self, seen: Hlc) {
        if seen > self.clock.latest() {
            self.clock = Clock::from_latest(seen);
        }
    }

    /// The hub's `field.set number` op for `create`, stamped after the
    /// create and after the hub's previous op. The number is the next
    /// one; the allocator advances.
    fn allocate(&mut self, create: &Create, now_ms: u64) -> Op {
        let number = self.next_number;
        self.next_number += 1;
        let hlc = self.clock.receive(create.hlc, now_ms);
        Op::new(
            Ulid::new(),
            hlc,
            ActorId::new(HUB_ACTOR),
            create.entity,
            Payload::FieldSet(FieldSet::Number(number as u64)),
        )
    }

    /// Numbers every `create` (in order) that is not yet in `numbers`:
    /// appends the hub's ops to the log and records the numbers. Returns
    /// what was numbered, in `creates` order.
    pub async fn allocate_all(
        &mut self,
        tx: &Transaction<'_>,
        workspace: &str,
        creates: &[Create],
    ) -> Result<Vec<Numbered>, tokio_postgres::Error> {
        let entities: Vec<String> = creates.iter().map(|c| c.entity.to_string()).collect();
        let known = numbered(tx, workspace, &entities).await?;
        // One wall-clock reading for the whole batch: within it the
        // clock's counter keeps the hub's ops strictly ascending.
        let now_ms = now_ms();
        let mut ops: Vec<Op> = Vec::new();
        for create in creates {
            if known.contains_key(&create.entity.to_string())
                || ops.iter().any(|op| op.entity == create.entity)
            {
                continue;
            }
            ops.push(self.allocate(create, now_ms));
        }
        if ops.is_empty() {
            return Ok(Vec::new());
        }
        let jsons: Vec<String> = ops
            .iter()
            .map(|op| serde_json::to_string(op).expect("an op serializes"))
            .collect();
        let rows: Vec<OpRow<'_>> = ops
            .iter()
            .zip(&jsons)
            .map(|(op, json)| OpRow {
                op_id: op.op_id.to_string(),
                hlc_wall_ms: op.hlc.wall_ms as i64,
                hlc_counter: i64::from(op.hlc.counter),
                actor: HUB_ACTOR,
                entity: op.entity.to_string(),
                kind: op.kind(),
                raw: json,
            })
            .collect();
        let seqs = insert_ops(tx, workspace, &rows).await?;
        let mut out = Vec::with_capacity(ops.len());
        for ((op, json), (_, seq)) in ops.iter().zip(&jsons).zip(seqs) {
            let Payload::FieldSet(FieldSet::Number(number)) = &op.payload else {
                unreachable!("allocate builds number ops")
            };
            let number = *number as i64;
            record(tx, workspace, &op.entity.to_string(), number, seq).await?;
            out.push(Numbered {
                entity: op.entity.to_string(),
                number,
                seq,
                op: RawValue::from_string(json.clone()).expect("serialized JSON"),
                allocated: true,
            });
        }
        Ok(out)
    }

    /// Writes `next_number` and the clock back to the locked row.
    pub async fn save(
        &self,
        tx: &Transaction<'_>,
        workspace: &str,
    ) -> Result<(), tokio_postgres::Error> {
        let latest = self.clock.latest();
        tx.execute(
            "UPDATE workspaces
             SET next_number = $2, clock_wall_ms = $3, clock_counter = $4
             WHERE id = $1",
            &[
                &workspace,
                &self.next_number,
                &(latest.wall_ms as i64),
                &i64::from(latest.counter),
            ],
        )
        .await?;
        Ok(())
    }
}

/// An HLC from its two stored columns. Both are written from the
/// unsigned types, so they always fit; should a row ever not (a corrupt
/// or hand-edited one), each component falls back to 0, the
/// conservative minimum — the clock then only moves forward from what
/// it next observes, rather than jumping to an inflated stamp.
fn hlc_from_row(wall_ms: i64, counter: i64) -> Hlc {
    Hlc::new(
        u64::try_from(wall_ms).unwrap_or(0),
        u32::try_from(counter).unwrap_or(0),
    )
}

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// The numbers already issued to any of `entities`: entity → (number, seq).
pub async fn numbered(
    tx: &Transaction<'_>,
    workspace: &str,
    entities: &[String],
) -> Result<HashMap<String, (i64, i64)>, tokio_postgres::Error> {
    let rows = tx
        .query(
            "SELECT entity, number, seq FROM numbers
             WHERE workspace_id = $1 AND entity = ANY($2)",
            &[&workspace, &entities],
        )
        .await?;
    Ok(rows
        .into_iter()
        .map(|r| (r.get(0), (r.get(1), r.get(2))))
        .collect())
}

/// Which of `numbers` are already taken, and by which ticket.
pub async fn taken(
    tx: &Transaction<'_>,
    workspace: &str,
    numbers: &[i64],
) -> Result<HashMap<i64, String>, tokio_postgres::Error> {
    let rows = tx
        .query(
            "SELECT number, entity FROM numbers
             WHERE workspace_id = $1 AND number = ANY($2)",
            &[&workspace, &numbers],
        )
        .await?;
    Ok(rows.into_iter().map(|r| (r.get(0), r.get(1))).collect())
}

/// Records that `entity` holds `number`, issued by the op at `seq`.
pub async fn record(
    tx: &Transaction<'_>,
    workspace: &str,
    entity: &str,
    number: i64,
    seq: i64,
) -> Result<(), tokio_postgres::Error> {
    tx.execute(
        "INSERT INTO numbers (workspace_id, entity, number, seq) VALUES ($1, $2, $3, $4)",
        &[&workspace, &entity, &number, &seq],
    )
    .await?;
    Ok(())
}

/// The [`Numbered`] entries for every one of `entities` that has a
/// number, with the issuing op read back from the log. `allocated` is
/// `true` for the ones in `fresh`.
pub async fn report(
    tx: &Transaction<'_>,
    workspace: &str,
    entities: &[String],
    fresh: Vec<Numbered>,
) -> Result<Vec<Numbered>, tokio_postgres::Error> {
    let rows = tx
        .query(
            "SELECT n.entity, n.number, n.seq, o.op::text
             FROM numbers n JOIN ops o ON o.seq = n.seq
             WHERE n.workspace_id = $1 AND n.entity = ANY($2)
             ORDER BY n.seq",
            &[&workspace, &entities],
        )
        .await?;
    let mut fresh: HashMap<String, Numbered> =
        fresh.into_iter().map(|n| (n.entity.clone(), n)).collect();
    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        let entity: String = row.get(0);
        out.push(match fresh.remove(&entity) {
            Some(n) => n,
            None => Numbered {
                entity,
                number: row.get(1),
                seq: row.get(2),
                op: RawValue::from_string(row.get(3)).expect("stored op is JSON"),
                allocated: false,
            },
        });
    }
    Ok(out)
}

// ---------------------------------------------------- POST /seeded

#[derive(Deserialize)]
struct SeedEnd {
    /// The client's allocator floor (`workspace.number_floor` in
    /// pm-store): the greatest number the seed may have used.
    number_floor: u64,
}

#[derive(Serialize)]
struct Seeded {
    /// The floor the hub adopted: the greater of the request's and the
    /// largest number in the seeded log.
    number_floor: i64,
    /// Creates the seed left unnumbered, numbered now (from the floor).
    numbers: Vec<Numbered>,
}

enum SeedError {
    Body(String),
    AlreadySeeded,
    NoWorkspace,
    Db(tokio_postgres::Error),
}

impl From<tokio_postgres::Error> for SeedError {
    fn from(e: tokio_postgres::Error) -> Self {
        SeedError::Db(e)
    }
}

impl IntoResponse for SeedError {
    fn into_response(self) -> Response {
        match self {
            SeedError::Body(reason) => (
                StatusCode::BAD_REQUEST,
                Json(ErrorBody::new("invalid_body", reason)),
            )
                .into_response(),
            SeedError::AlreadySeeded => (
                StatusCode::CONFLICT,
                Json(ErrorBody::new(
                    "already_seeded",
                    "this workspace's seed already ended; the hub is its number authority"
                        .to_string(),
                )),
            )
                .into_response(),
            SeedError::NoWorkspace => StatusCode::NOT_FOUND.into_response(),
            SeedError::Db(e) => {
                eprintln!("pm-hub: seeded: {e}");
                StatusCode::SERVICE_UNAVAILABLE.into_response()
            }
        }
    }
}

/// Largest `POST /seeded` body, in bytes: the body is one integer field.
pub const MAX_SEED_BODY_BYTES: usize = 1024;

/// `POST /w/{workspace}/seeded` with `{"number_floor": <n>}`: ends the
/// seed (see the module doc). `409 already_seeded` the second time.
pub async fn finish_seed(State(db): State<Db>, caller: Authed, body: String) -> Response {
    match finish(&db, &caller.workspace, &body).await {
        Ok(seeded) => Json(seeded).into_response(),
        Err(e) => e.into_response(),
    }
}

async fn finish(db: &Db, workspace: &str, body: &str) -> Result<Seeded, SeedError> {
    let SeedEnd { number_floor } =
        serde_json::from_str(body).map_err(|e| SeedError::Body(format!("body: {e}")))?;
    let number_floor = i64::try_from(number_floor)
        .map_err(|_| SeedError::Body(format!("number_floor {number_floor} is out of range")))?;

    let mut writer = db.writer.lock().await;
    let tx = writer.transaction().await?;
    let Some(mut allocator) = Allocator::lock(&tx, workspace).await? else {
        return Err(SeedError::NoWorkspace);
    };
    if allocator.seeded {
        return Err(SeedError::AlreadySeeded);
    }
    let seeded_max: i64 = tx
        .query_one(
            "SELECT COALESCE(MAX(number), 0) FROM numbers WHERE workspace_id = $1",
            &[&workspace],
        )
        .await?
        .get(0);
    let floor = number_floor.max(seeded_max);
    allocator.next_number = floor + 1;

    // The hub's clock has seen the whole seed.
    let latest = tx
        .query_one(
            "SELECT COALESCE(MAX(hlc_wall_ms), 0), COALESCE(MAX(hlc_counter), 0)
             FROM ops WHERE workspace_id = $1
               AND hlc_wall_ms = (SELECT MAX(hlc_wall_ms) FROM ops WHERE workspace_id = $1)",
            &[&workspace],
        )
        .await?;
    allocator.observe(hlc_from_row(latest.get(0), latest.get(1)));

    // Creates the seed left unnumbered, in log order.
    let rows = tx
        .query(
            "SELECT DISTINCT ON (o.entity) o.entity, o.hlc_wall_ms, o.hlc_counter, o.seq
             FROM ops o
             WHERE o.workspace_id = $1 AND o.kind = 'ticket.create'
               AND NOT EXISTS (SELECT 1 FROM numbers n
                               WHERE n.workspace_id = $1 AND n.entity = o.entity)
             ORDER BY o.entity, o.seq",
            &[&workspace],
        )
        .await?;
    let mut creates: Vec<(i64, Create)> = rows
        .into_iter()
        .filter_map(|r| {
            let entity: String = r.get(0);
            Some((
                r.get::<_, i64>(3),
                Create {
                    entity: entity.parse().ok()?,
                    hlc: hlc_from_row(r.get(1), r.get(2)),
                },
            ))
        })
        .collect();
    creates.sort_by_key(|(seq, _)| *seq);
    let creates: Vec<Create> = creates.into_iter().map(|(_, c)| c).collect();
    let numbers = allocator.allocate_all(&tx, workspace, &creates).await?;
    crate::views::fold_hub_ops(&tx, workspace, &numbers).await?;

    allocator.save(&tx, workspace).await?;
    tx.execute(
        "UPDATE workspaces SET number_floor = $2, seeded_at = now() WHERE id = $1",
        &[&workspace, &floor],
    )
    .await?;
    tx.commit().await?;
    Ok(Seeded {
        number_floor: floor,
        numbers,
    })
}

/// Whether `workspace`'s seed has ended (the hub is its number authority).
pub async fn is_seeded(
    client: &tokio_postgres::Client,
    workspace: &str,
) -> Result<Option<bool>, tokio_postgres::Error> {
    Ok(client
        .query_opt(
            "SELECT seeded_at IS NOT NULL FROM workspaces WHERE id = $1",
            &[&workspace],
        )
        .await?
        .map(|r| r.get(0)))
}

/// The number a `field.set number` op carries — the one op kind the hub
/// reserves for itself after seeding — or `None` for any other op.
pub fn number_value(op: &Op) -> Option<u64> {
    match &op.payload {
        Payload::FieldSet(FieldSet::Number(n)) => Some(*n),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hub_ops_are_stamped_after_the_create_and_after_each_other() {
        let mut a = Allocator {
            seeded: true,
            next_number: 1377,
            clock: Clock::from_latest(Hlc::new(50, 0)),
        };
        let create = Create {
            entity: Ulid::new(),
            hlc: Hlc::new(1_800_000_000_000, 3),
        };
        // The create is ahead of both the hub's clock and its wall clock:
        // the hub op still lands strictly after it.
        let first = a.allocate(&create, 60);
        assert!(first.hlc > create.hlc, "{} vs {}", first.hlc, create.hlc);
        assert_eq!(first.hlc, Hlc::new(1_800_000_000_000, 4));
        assert_eq!(first.actor.as_str(), HUB_ACTOR);
        assert_eq!(first.entity, create.entity);
        assert_eq!(first.version, pm_core::OP_VERSION);
        assert_eq!(number_value(&first), Some(1377));
        let second = a.allocate(
            &Create {
                entity: Ulid::new(),
                hlc: Hlc::new(10, 0),
            },
            60,
        );
        assert!(second.hlc > first.hlc);
        assert_eq!(number_value(&second), Some(1378));
        assert_eq!(a.next_number, 1379);
        assert_eq!(a.clock.latest(), second.hlc);

        // Observing a later stamp moves the clock; an earlier one does not.
        a.observe(Hlc::new(1_900_000_000_000, 0));
        assert_eq!(a.clock.latest(), Hlc::new(1_900_000_000_000, 0));
        a.observe(Hlc::new(1, 1));
        assert_eq!(a.clock.latest(), Hlc::new(1_900_000_000_000, 0));
    }

    #[test]
    fn the_hub_op_is_a_plain_field_set_number_on_the_wire() {
        let mut a = Allocator {
            seeded: true,
            next_number: 7,
            clock: Clock::new(),
        };
        let entity = Ulid::new();
        let op = a.allocate(
            &Create {
                entity,
                hlc: Hlc::new(5, 0),
            },
            5,
        );
        let json = serde_json::to_value(&op).unwrap();
        assert_eq!(json["kind"], "field.set");
        assert_eq!(
            json["payload"],
            serde_json::json!({"field": "number", "value": 7})
        );
        assert_eq!(json["actor"], "hub");
        assert_eq!(json["entity"], entity.to_string());
        let back: Op = serde_json::from_value(json).unwrap();
        assert_eq!(back, op);
    }
}
