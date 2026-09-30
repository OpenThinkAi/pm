//! `POST /w/{workspace}/ops`: a client pushes a batch of ops and gets back
//! the hub sequence number of each (AGT-1389, README §Sync & hub).
//!
//! The body is `{"ops": [<op>, ...]}`, each op in the op log's wire format
//! (`{op_id, hlc, actor, entity, kind, payload, version}` — the JSON `pm
//! backup` writes one per line). Every op is parsed as a [`pm_core::Op`] to
//! reject a malformed one and to fill the indexed columns, but what the
//! `ops.op` column stores is the op's JSON text exactly as it arrived
//! (`json`, not `jsonb`), so a pull serves back the bytes that were pushed.
//!
//! **Sequence.** `ops.seq` is the only transport order: a client pulls
//! `since <seq>` and trusts that nothing with a smaller seq will appear
//! later. A bigserial alone does not give that — two concurrent
//! transactions take 5 and 6 and may commit 6 first, so a puller that saw
//! 6 skips 5 forever. Every push therefore runs in one transaction that
//! first locks the workspace's row (`SELECT ... FOR NO KEY UPDATE`, held to
//! commit) and only then takes sequence values: per workspace, seqs are
//! handed out and committed in the same order, with no gaps between a
//! batch's ops. `NO KEY` leaves the row open to `FOR KEY SHARE`, which is
//! what inserting a token for the workspace takes. Inside a batch, seq
//! order is batch order. A push takes its workspace's in-process write
//! lock and then a connection from the writer pool (`writer`, AGT-1463):
//! pushes to one workspace queue behind each other, pushes to different
//! workspaces run side by side, and reads and auth stay on their own
//! connection and never wait for a push.
//!
//! **Idempotency.** `op_id` is an op's identity. An op the workspace
//! already has (from an earlier batch, or earlier in this one) is answered
//! with its existing seq and `stored: false`; its content is not compared
//! and never overwritten. A batch is all-or-nothing: one bad op and nothing
//! in it is stored.
//!
//! **Views and claims (AGT-1392, see `views`).** Every fresh op is folded
//! into the hub's materialized views in batch order, in the same
//! transaction, with pm-core's `apply`. Once the workspace is seeded a
//! `claim` is admitted only if `TicketView::claim_admissible` holds
//! against the view at that point; a refused claim is the one exception
//! to all-or-nothing: the batch still lands, but nothing is stored for
//! that op and its ack carries `rejected: {taken_by, at, state, code,
//! reason}` and `seq: null` (a 200, not a 4xx — the op was understood
//! and decided). An op the views cannot fold at all (a relation that does
//! not touch its ticket, config for a foreign workspace) is a 400 like a
//! malformed one, and the batch is refused whole.
//!
//! **Numbers (AGT-1391, see `numbers`).** Once the workspace is seeded,
//! every fresh `ticket.create` in a batch is numbered here, in the same
//! transaction: the hub's `field.set number` ops go into the log right
//! after the batch (their own seqs, contiguous after the batch's) and
//! come back in the response's `numbers`. A pushed `field.set number` is
//! then refused (`number_not_allowed`). While the workspace is still
//! seeding the roles flip: pushed number ops are recorded and the hub
//! allocates nothing.
//!
//! **Actors (AGT-1450).** A token may author ops only as the actors it is
//! bound to (`auth::ActorBinding`, set by `pm-hub token create --actor` /
//! `token bind`). Every *fresh* op of a batch is checked; an op the
//! workspace already has is acknowledged as before whoever authored it,
//! since nothing is stored. A token minted before bindings (`actors IS
//! NULL`) keeps accepting any actor. The reserved actor `hub` is refused
//! for every client once the workspace is seeded, and while seeding is
//! accepted only from an unrestricted token (a hub-to-hub reseed carries
//! the old hub's number ops). Refusal is `400 actor_not_allowed` /
//! `reserved_actor`, and the batch is refused whole.
//!
//! **Actors named in payloads (AGT-1463).** A fresh op's payload fields
//! that name an actor — a `claim`'s `assignee`, a `hold.set`'s `hold.by`,
//! a `field.set assignee` value and an `actor.upsert`'s `id` — are held to
//! the same two rules as the op's own actor, so a bound token can neither
//! claim nor hold a ticket in someone else's name. One exception: a
//! `field.set assignee` that restates the ticket's current assignee at the
//! hub (at that point in the batch) is accepted from any token — it
//! changes nothing, and it is what a client logs to reconcile after the
//! hub refused its claim (`rejected.taken_by`). Unassigning
//! (`field.set assignee null`) names nobody and is always accepted.
//!
//! **Stamps (oaudit 2026-09-30).** Every op's HLC must be storable and
//! leave its counter room to advance (`pm_core::Hlc::check_range`:
//! `wall_ms <= i64::MAX`, `counter < u32::MAX`; else `400
//! invalid_stamp`) and be at most `pm_core::MAX_FUTURE_SKEW_MS` (one day)
//! ahead of the hub's wall clock (else `400 future_stamp`): a far-future
//! stamp would win every LWW register and drag every clock that sees it
//! forward. Stamps from the past are always accepted — a seed uploads
//! historical ops.
//!
//! **Identifiers (AGT-1450).** A `workspace.set prefix`, `project.create`
//! (id, parent) or `project.set parent` whose value is not a safe path
//! component (`pm_core::ids::is_safe_component`) is `400 invalid_id`:
//! clients use these in file paths.
//!
//! **Limits.** [`MAX_BATCH_OPS`] ops per batch, [`MAX_BODY_BYTES`] of body
//! (the Studio's seed log holds one 23 MB `body.edit`, so the byte limit
//! leaves room for a single large op plus a batch around it). A body over
//! the limit is a 413 whether announced by `Content-Length` or discovered
//! while reading. A `body.edit` whose decoded update is over
//! `pm_core::MAX_BODY_EDIT_BYTES` (32 MiB) is `400 op_too_large`
//! (AGT-1467); replicas refuse one on pull too.

use std::collections::HashMap;

use axum::Json;
use axum::body::Bytes;
use axum::extract::rejection::{BytesRejection, FailedToBufferBody};
use axum::extract::{FromRequest, Request, State};
use axum::http::StatusCode;
use axum::http::header::CONTENT_LENGTH;
use axum::response::{IntoResponse, Response};
use pm_core::op::FieldSet;
use pm_core::{ActorId, Hlc, MAX_FUTURE_SKEW_MS, OP_VERSION, Op, Payload};
use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;
use tokio_postgres::Transaction;

use crate::Db;
use crate::auth::Authed;
use crate::numbers::{self, Allocator, Create, Numbered};
use crate::views::{FoldError, Rejection, Verdict, Views};

/// Most ops in one batch.
pub const MAX_BATCH_OPS: usize = 1000;
/// Largest request body, in bytes (64 MiB).
pub const MAX_BODY_BYTES: usize = 64 * 1024 * 1024;

#[derive(Deserialize)]
struct Batch<'a> {
    #[serde(borrow)]
    ops: Vec<&'a RawValue>,
}

#[derive(Serialize)]
struct Pushed {
    /// One entry per op in the batch, in batch order.
    ops: Vec<Ack>,
    /// The number of every ticket a `ticket.create` in this batch made,
    /// in the order the numbers were issued: allocated by this push, or
    /// already held (a re-pushed create, a seeded number). Empty while
    /// the workspace is seeding and nothing in the batch is numbered.
    numbers: Vec<Numbered>,
}

#[derive(Serialize)]
struct Ack {
    op_id: String,
    /// The op's seq; `null` for a rejected claim (nothing was stored).
    seq: Option<i64>,
    /// `true` if this push stored the op; `false` if the workspace
    /// already had it (its seq is the existing one) or rejected it.
    stored: bool,
    /// Present only for a `claim` the hub refused (see `views`).
    #[serde(skip_serializing_if = "Option::is_none")]
    rejected: Option<Rejection>,
}

/// What the push decided for one unique op of the batch.
#[derive(Clone)]
enum Outcome {
    Stored(i64),
    Existing(i64),
    Rejected(Rejection),
}

/// The structured error body of every 4xx the write routes return.
#[derive(Serialize)]
pub struct ErrorBody {
    error: &'static str,
    reason: String,
    /// The offending op's position in the batch (per-op errors only).
    #[serde(skip_serializing_if = "Option::is_none")]
    index: Option<usize>,
    /// The offending op's `op_id`, when the JSON had one (per-op errors only).
    #[serde(skip_serializing_if = "Option::is_none")]
    op_id: Option<String>,
}

impl ErrorBody {
    pub fn new(error: &'static str, reason: String) -> Self {
        ErrorBody {
            error,
            reason,
            index: None,
            op_id: None,
        }
    }
}

#[derive(Debug)]
enum PushError {
    TooLarge,
    Batch(String),
    Op {
        index: usize,
        op_id: Option<String>,
        reason: String,
    },
    /// A `field.set number` pushed to a seeded workspace.
    NumberNotAllowed {
        index: usize,
        op_id: String,
    },
    /// A seeded `field.set number` that collides with a number already
    /// issued, or re-numbers a ticket.
    DuplicateNumber {
        index: usize,
        op_id: String,
        reason: String,
    },
    /// A config op for a workspace Ulid other than the one this hub
    /// workspace's config already belongs to.
    ForeignWorkspace {
        index: usize,
        op_id: String,
        reason: String,
    },
    /// An op whose HLC is out of range (`invalid_stamp`) or too far in
    /// the future (`future_stamp`), or that carries a prefix or project id
    /// unsafe in a file path (`invalid_id`).
    OpCheck {
        error: &'static str,
        index: usize,
        op_id: Option<String>,
        reason: String,
    },
    /// A fresh op authored as an actor the token is not bound to.
    ActorNotAllowed {
        index: usize,
        op_id: String,
        reason: String,
    },
    /// A fresh op authored as the hub's own actor.
    ReservedActor {
        index: usize,
        op_id: String,
    },
    /// Something the hub derives itself ran out of range (its clock or
    /// number allocator). Unreachable with the stamp and number checks
    /// above; answered as a 503, like a database failure, and logged.
    Internal(String),
    NoWorkspace,
    Db(tokio_postgres::Error),
}

impl From<numbers::AllocError> for PushError {
    fn from(e: numbers::AllocError) -> Self {
        match e {
            numbers::AllocError::Db(e) => PushError::Db(e),
            numbers::AllocError::Exhausted(why) => PushError::Internal(why),
        }
    }
}

impl From<tokio_postgres::Error> for PushError {
    fn from(e: tokio_postgres::Error) -> Self {
        PushError::Db(e)
    }
}

impl IntoResponse for PushError {
    fn into_response(self) -> Response {
        let (status, body) = match self {
            PushError::TooLarge => (
                StatusCode::PAYLOAD_TOO_LARGE,
                ErrorBody {
                    error: "too_large",
                    reason: format!("request body exceeds {MAX_BODY_BYTES} bytes"),
                    index: None,
                    op_id: None,
                },
            ),
            PushError::Batch(reason) => (
                StatusCode::BAD_REQUEST,
                ErrorBody {
                    error: "invalid_batch",
                    reason,
                    index: None,
                    op_id: None,
                },
            ),
            PushError::Op {
                index,
                op_id,
                reason,
            } => (
                StatusCode::BAD_REQUEST,
                ErrorBody {
                    error: "invalid_op",
                    reason,
                    index: Some(index),
                    op_id,
                },
            ),
            PushError::NumberNotAllowed { index, op_id } => (
                StatusCode::BAD_REQUEST,
                ErrorBody {
                    error: "number_not_allowed",
                    reason: "this workspace is seeded: only the hub issues ticket numbers \
                             (push the ticket.create without one and read the number \
                             from the response or a pull)"
                        .to_string(),
                    index: Some(index),
                    op_id: Some(op_id),
                },
            ),
            PushError::DuplicateNumber {
                index,
                op_id,
                reason,
            } => (
                StatusCode::BAD_REQUEST,
                ErrorBody {
                    error: "duplicate_number",
                    reason,
                    index: Some(index),
                    op_id: Some(op_id),
                },
            ),
            PushError::ForeignWorkspace {
                index,
                op_id,
                reason,
            } => (
                StatusCode::BAD_REQUEST,
                ErrorBody {
                    error: "foreign_workspace",
                    reason,
                    index: Some(index),
                    op_id: Some(op_id),
                },
            ),
            PushError::OpCheck {
                error,
                index,
                op_id,
                reason,
            } => (
                StatusCode::BAD_REQUEST,
                ErrorBody {
                    error,
                    reason,
                    index: Some(index),
                    op_id,
                },
            ),
            PushError::ActorNotAllowed {
                index,
                op_id,
                reason,
            } => (
                StatusCode::BAD_REQUEST,
                ErrorBody {
                    error: "actor_not_allowed",
                    reason,
                    index: Some(index),
                    op_id: Some(op_id),
                },
            ),
            PushError::ReservedActor { index, op_id } => (
                StatusCode::BAD_REQUEST,
                ErrorBody {
                    error: "reserved_actor",
                    reason: format!(
                        "actor {:?} is reserved for the hub's own ops",
                        numbers::HUB_ACTOR
                    ),
                    index: Some(index),
                    op_id: Some(op_id),
                },
            ),
            PushError::Internal(why) => {
                eprintln!("pm-hub: push: {why}");
                return StatusCode::SERVICE_UNAVAILABLE.into_response();
            }
            // The token authenticated, so the workspace row was there a
            // moment ago; answer as auth does when there is nothing there.
            PushError::NoWorkspace => return StatusCode::NOT_FOUND.into_response(),
            PushError::Db(e) => {
                eprintln!("pm-hub: push: {e}");
                return StatusCode::SERVICE_UNAVAILABLE.into_response();
            }
        };
        (status, Json(body)).into_response()
    }
}

/// What the number allocator needs to know about an op.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Role {
    Create,
    /// A `field.set number` carrying this number.
    Number(i64),
    Other,
}

/// One op of a batch, parsed for its indexed columns and for folding
/// into the views; `raw` is what gets stored.
struct Parsed<'a> {
    op_id: String,
    hlc: Hlc,
    hlc_wall_ms: i64,
    hlc_counter: i64,
    actor: String,
    entity: String,
    kind: &'static str,
    role: Role,
    op: Op,
    raw: &'a str,
}

/// One row for [`insert_ops`]: a client op as parsed, or a hub op.
pub struct OpRow<'a> {
    pub op_id: String,
    pub hlc_wall_ms: i64,
    pub hlc_counter: i64,
    pub actor: &'a str,
    pub entity: String,
    pub kind: &'static str,
    pub raw: &'a str,
}

/// Appends `rows` to `workspace`'s log in order, under the caller's
/// workspace lock, and returns each `(op_id, seq)` in the same order.
pub async fn insert_ops(
    tx: &Transaction<'_>,
    workspace: &str,
    rows: &[OpRow<'_>],
) -> Result<Vec<(String, i64)>, tokio_postgres::Error> {
    if rows.is_empty() {
        return Ok(Vec::new());
    }
    let op_ids: Vec<&str> = rows.iter().map(|p| p.op_id.as_str()).collect();
    let wall_ms: Vec<i64> = rows.iter().map(|p| p.hlc_wall_ms).collect();
    let counters: Vec<i64> = rows.iter().map(|p| p.hlc_counter).collect();
    let actors: Vec<&str> = rows.iter().map(|p| p.actor).collect();
    let entities: Vec<&str> = rows.iter().map(|p| p.entity.as_str()).collect();
    let kinds: Vec<&str> = rows.iter().map(|p| p.kind).collect();
    let raws: Vec<&str> = rows.iter().map(|p| p.raw).collect();
    // `ORDER BY ordinality` feeds rows to the insert in batch order, so
    // `nextval` runs in that order too. `::json` from text keeps the text
    // verbatim (a `json` value is its input).
    let inserted = tx
        .query(
            "INSERT INTO ops
                 (workspace_id, op_id, hlc_wall_ms, hlc_counter, actor, entity, kind, op)
             SELECT $1, n.op_id, n.wall_ms, n.counter, n.actor, n.entity, n.kind, n.op::json
             FROM unnest($2::text[], $3::bigint[], $4::bigint[],
                         $5::text[], $6::text[], $7::text[], $8::text[])
                  WITH ORDINALITY
                  AS n (op_id, wall_ms, counter, actor, entity, kind, op, i)
             ORDER BY n.i
             RETURNING op_id, seq",
            &[
                &workspace, &op_ids, &wall_ms, &counters, &actors, &entities, &kinds, &raws,
            ],
        )
        .await?;
    Ok(inserted
        .into_iter()
        .map(|row| (row.get(0), row.get(1)))
        .collect())
}

/// `op_id` alone, for naming an op that failed to parse as a whole.
#[derive(Deserialize)]
struct OpIdOnly {
    op_id: Option<String>,
}

/// Parses one op of a batch and checks its stamp against the hub's wall
/// clock `now_ms` (see the module doc).
fn parse_op(index: usize, raw: &RawValue, now_ms: u64) -> Result<Parsed<'_>, PushError> {
    let op_id_of = || {
        serde_json::from_str::<OpIdOnly>(raw.get())
            .ok()
            .and_then(|o| o.op_id)
    };
    let invalid = |reason: String| PushError::Op {
        index,
        op_id: op_id_of(),
        reason,
    };
    let op: Op =
        serde_json::from_str(raw.get()).map_err(|e| invalid(format!("not a pm op: {e}")))?;
    if op.version > OP_VERSION {
        return Err(invalid(format!(
            "op version {} is newer than this hub understands ({OP_VERSION})",
            op.version
        )));
    }
    if op.actor.as_str().is_empty() {
        return Err(invalid("actor is empty".to_string()));
    }
    let stamp = |error: &'static str, e: pm_core::StampError| PushError::OpCheck {
        error,
        index,
        op_id: Some(op.op_id.to_string()),
        reason: e.to_string(),
    };
    op.hlc
        .check_range()
        .map_err(|e| stamp("invalid_stamp", e))?;
    op.hlc
        .check_not_after(now_ms, MAX_FUTURE_SKEW_MS)
        .map_err(|e| stamp("future_stamp", e))?;
    // Prefixes and project ids end up in client file paths (AGT-1453);
    // refuse a hostile one before it is stored (AGT-1450).
    pm_core::ids::check_op_ids(&op).map_err(|e| PushError::OpCheck {
        error: "invalid_id",
        index,
        op_id: Some(op.op_id.to_string()),
        reason: e.to_string(),
    })?;
    // AGT-1467: one oversized document update would be re-served to, and
    // folded by, every replica.
    op.check_size().map_err(|e| PushError::OpCheck {
        error: "op_too_large",
        index,
        op_id: Some(op.op_id.to_string()),
        reason: e.to_string(),
    })?;
    let hlc_wall_ms = i64::try_from(op.hlc.wall_ms)
        .map_err(|_| invalid(format!("hlc.wall_ms {} is out of range", op.hlc.wall_ms)))?;
    let role = match (&op.payload, numbers::number_value(&op)) {
        (Payload::TicketCreate(_), _) => Role::Create,
        (_, Some(n)) => Role::Number(
            i64::try_from(n)
                .ok()
                .filter(|n| (1..=numbers::MAX_NUMBER).contains(n))
                .ok_or_else(|| {
                    invalid(format!(
                        "number {n} is out of range (1 to {})",
                        numbers::MAX_NUMBER
                    ))
                })?,
        ),
        _ => Role::Other,
    };
    Ok(Parsed {
        op_id: op.op_id.to_string(),
        hlc: op.hlc,
        hlc_wall_ms,
        hlc_counter: i64::from(op.hlc.counter),
        actor: op.actor.as_str().to_string(),
        entity: op.entity.to_string(),
        kind: op.kind(),
        role,
        op,
        raw: raw.get(),
    })
}

/// The body, or 413 when `Content-Length` announces more than the limit
/// (without reading it) or the limit is hit while reading.
async fn body(req: Request) -> Result<Bytes, PushError> {
    let announced = req
        .headers()
        .get(CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<usize>().ok());
    if announced.is_some_and(|n| n > MAX_BODY_BYTES) {
        return Err(PushError::TooLarge);
    }
    Bytes::from_request(req, &()).await.map_err(|e| match e {
        BytesRejection::FailedToBufferBody(FailedToBufferBody::LengthLimitError(_)) => {
            PushError::TooLarge
        }
        e => PushError::Batch(e.body_text()),
    })
}

pub async fn push(State(db): State<Db>, caller: Authed, req: Request) -> Response {
    match push_batch(&db, &caller, req).await {
        Ok(pushed) => Json(pushed).into_response(),
        Err(e) => e.into_response(),
    }
}

async fn push_batch(db: &Db, caller: &Authed, req: Request) -> Result<Pushed, PushError> {
    let workspace = caller.workspace.as_str();
    let body = body(req).await?;
    let text = std::str::from_utf8(&body)
        .map_err(|_| PushError::Batch("body is not UTF-8".to_string()))?;
    let batch: Batch =
        serde_json::from_str(text).map_err(|e| PushError::Batch(format!("body: {e}")))?;
    if batch.ops.len() > MAX_BATCH_OPS {
        return Err(PushError::Batch(format!(
            "{} ops in one batch; the limit is {MAX_BATCH_OPS}",
            batch.ops.len()
        )));
    }

    // Parse everything before touching the database, and collapse
    // repeats of an op_id within the batch onto its first occurrence.
    let mut unique: Vec<Parsed> = Vec::with_capacity(batch.ops.len());
    let mut first_at: HashMap<String, usize> = HashMap::with_capacity(batch.ops.len());
    let mut position: Vec<usize> = Vec::with_capacity(batch.ops.len());
    // One wall-clock reading bounds every stamp in the batch.
    let now_ms = numbers::now_ms();
    for (index, raw) in batch.ops.iter().enumerate() {
        let parsed = parse_op(index, raw, now_ms)?;
        let at = *first_at
            .entry(parsed.op_id.clone())
            .or_insert_with(|| unique.len());
        if at == unique.len() {
            unique.push(parsed);
        }
        position.push(at);
    }

    let mut outcome: HashMap<String, Outcome> = HashMap::with_capacity(unique.len());
    let numbers;
    {
        let mut writer = db.writer.acquire(workspace).await;
        let tx = writer.transaction().await?;
        // Held until commit: this is what orders seqs per workspace (see
        // the module doc) and serialises number allocation.
        let Some(mut allocator) = Allocator::lock(&tx, workspace).await? else {
            return Err(PushError::NoWorkspace);
        };

        let op_ids: Vec<&str> = unique.iter().map(|p| p.op_id.as_str()).collect();
        let existing = tx
            .query(
                "SELECT op_id, seq FROM ops WHERE workspace_id = $1 AND op_id = ANY($2)",
                &[&workspace, &op_ids],
            )
            .await?;
        for row in existing {
            outcome.insert(row.get(0), Outcome::Existing(row.get(1)));
        }

        let fresh: Vec<&Parsed> = unique
            .iter()
            .filter(|p| !outcome.contains_key(&p.op_id))
            .collect();
        // Seeded client numbers are checked before anything is written;
        // the whole batch is refused on the first problem.
        let seeded_numbers = if allocator.seeded {
            if let Some(p) = fresh.iter().find(|p| matches!(p.role, Role::Number(_))) {
                return Err(PushError::NumberNotAllowed {
                    index: first_at[&p.op_id],
                    op_id: p.op_id.clone(),
                });
            }
            Vec::new()
        } else {
            check_seeded_numbers(&tx, workspace, &fresh, &first_at).await?
        };
        // Only fresh ops are stored, so only they must be the caller's
        // to author (AGT-1450).
        for p in &fresh {
            check_actor(caller, allocator.seeded, p, first_at[&p.op_id])?;
        }

        // Fold the batch into the views in order; once seeded, that is
        // where a claim is decided. A refused claim is answered, not
        // stored (`views`).
        // Also the document ids the batch binds (AGT-1464 admission).
        let entities = crate::views::batch_entities(fresh.iter().map(|p| &p.op));
        let mut views = Views::load(&tx, workspace, &entities).await?;
        let mut admitted: Vec<&Parsed> = Vec::with_capacity(fresh.len());
        for p in &fresh {
            check_named_actors(
                caller,
                allocator.seeded,
                p,
                first_at[&p.op_id],
                views.assignee(p.op.entity),
            )?;
            match views.fold(&p.op, allocator.seeded) {
                Ok(Verdict::Folded) => admitted.push(p),
                Ok(Verdict::Rejected(rejection)) => {
                    outcome.insert(p.op_id.clone(), Outcome::Rejected(rejection));
                }
                Err(e) => return Err(fold_error(first_at[&p.op_id], &p.op_id, e)),
            }
        }

        let rows: Vec<OpRow<'_>> = admitted
            .iter()
            .map(|p| OpRow {
                op_id: p.op_id.clone(),
                hlc_wall_ms: p.hlc_wall_ms,
                hlc_counter: p.hlc_counter,
                actor: &p.actor,
                entity: p.entity.clone(),
                kind: p.kind,
                raw: p.raw,
            })
            .collect();
        for (op_id, seq) in insert_ops(&tx, workspace, &rows).await? {
            outcome.insert(op_id, Outcome::Stored(seq));
        }
        for (entity, number, op_id) in seeded_numbers {
            let Some(Outcome::Stored(seq)) = outcome.get(&op_id) else {
                unreachable!("a number op is never a claim, so it was stored")
            };
            numbers::record(&tx, workspace, &entity, number, *seq).await?;
        }

        // Number the batch's creates. Every fresh op moves the hub's
        // clock, so its number ops are stamped after all of them.
        let creates: Vec<Create> = unique
            .iter()
            .filter(|p| p.role == Role::Create)
            .filter_map(|p| {
                Some(Create {
                    entity: p.entity.parse().ok()?,
                    hlc: p.hlc,
                })
            })
            .collect();
        let allocated = if allocator.seeded && !creates.is_empty() {
            if let Some(latest) = fresh.iter().map(|p| p.hlc).max() {
                allocator.observe(latest);
            }
            let allocated = allocator.allocate_all(&tx, workspace, &creates).await?;
            allocator.save(&tx, workspace).await?;
            // The hub's own ops are part of the view too.
            for n in &allocated {
                views.fold_hub_op(n);
            }
            allocated
        } else {
            Vec::new()
        };
        views.save(&tx, workspace).await?;
        let entities: Vec<String> = creates.iter().map(|c| c.entity.to_string()).collect();
        numbers = numbers::report(&tx, workspace, &entities, allocated).await?;
        tx.commit().await?;
    }

    // A repeat within the batch is acknowledged like a repeat across
    // batches: the first occurrence stored it, later ones did not.
    let mut acked = vec![false; unique.len()];
    let ops = position
        .into_iter()
        .map(|at| {
            let op_id = &unique[at].op_id;
            let first = !std::mem::replace(&mut acked[at], true);
            let (seq, stored, rejected) = match &outcome[op_id] {
                Outcome::Stored(seq) => (Some(*seq), first, None),
                Outcome::Existing(seq) => (Some(*seq), false, None),
                Outcome::Rejected(r) => (None, false, Some(r.clone())),
            };
            Ack {
                op_id: op_id.clone(),
                seq,
                stored,
                rejected,
            }
        })
        .collect();
    Ok(Pushed { ops, numbers })
}

/// Whether `caller` may store `p` as its actor (see the module doc).
fn check_actor(
    caller: &Authed,
    seeded: bool,
    p: &Parsed<'_>,
    index: usize,
) -> Result<(), PushError> {
    check_actor_value(caller, seeded, p, index, None, &p.actor)
}

/// The rules every actor a fresh op carries is held to — its author, or
/// (`field`) an actor its payload names: the reserved `hub` actor only
/// from an unrestricted token while seeding, and otherwise only an actor
/// the token's binding permits.
fn check_actor_value(
    caller: &Authed,
    seeded: bool,
    p: &Parsed<'_>,
    index: usize,
    field: Option<&str>,
    actor: &str,
) -> Result<(), PushError> {
    if actor == numbers::HUB_ACTOR && (seeded || !caller.actors.unrestricted()) {
        return Err(PushError::ReservedActor {
            index,
            op_id: p.op_id.clone(),
        });
    }
    if !caller.actors.permits(actor) {
        let allowed = caller.actors.describe();
        let what = match field {
            None => format!("author ops as {actor:?}"),
            Some(field) => format!("name {actor:?} as a {} op's {field}", p.kind),
        };
        return Err(PushError::ActorNotAllowed {
            index,
            op_id: p.op_id.clone(),
            reason: format!(
                "token {} ({}) may not {what}; it may act as: {allowed}",
                caller.token_id, caller.token_label
            ),
        });
    }
    Ok(())
}

/// The actors an op's payload names, by field (see the module doc).
fn named_actors(op: &Op) -> Vec<(&'static str, &ActorId)> {
    match &op.payload {
        Payload::Claim(c) => vec![("assignee", &c.assignee)],
        Payload::HoldSet(h) => vec![("hold.by", &h.hold.by)],
        Payload::FieldSet(FieldSet::Assignee(Some(a))) => vec![("assignee", a)],
        Payload::ActorUpsert(a) => vec![("id", &a.id)],
        _ => Vec::new(),
    }
}

/// Checks the actors `p`'s payload names against `caller`'s binding.
/// `current` is the ticket's assignee at the hub just before `p` folds:
/// a `field.set assignee` restating it is accepted from any token.
fn check_named_actors(
    caller: &Authed,
    seeded: bool,
    p: &Parsed<'_>,
    index: usize,
    current: Option<&ActorId>,
) -> Result<(), PushError> {
    let restates = |a: &ActorId| {
        matches!(p.op.payload, Payload::FieldSet(FieldSet::Assignee(_))) && current == Some(a)
    };
    for (field, actor) in named_actors(&p.op) {
        if restates(actor) {
            continue;
        }
        check_actor_value(caller, seeded, p, index, Some(field), actor.as_str())?;
    }
    Ok(())
}

/// The 400 for an op the views cannot fold.
fn fold_error(index: usize, op_id: &str, e: FoldError) -> PushError {
    let reason = e.to_string();
    match e {
        FoldError::ForeignWorkspace { .. } => PushError::ForeignWorkspace {
            index,
            op_id: op_id.to_string(),
            reason,
        },
        FoldError::Ticket(_) | FoldError::Config(_) => PushError::Op {
            index,
            op_id: Some(op_id.to_string()),
            reason,
        },
    }
}

/// Seed mode: the fresh `field.set number` ops of a batch, as `(entity,
/// number, op_id)`, checked against the numbers already issued and
/// against each other. A ticket may be numbered once and a number may
/// name one ticket; the client (pm-store) enforces the same locally.
async fn check_seeded_numbers(
    tx: &Transaction<'_>,
    workspace: &str,
    fresh: &[&Parsed<'_>],
    first_at: &HashMap<String, usize>,
) -> Result<Vec<(String, i64, String)>, PushError> {
    let numbered: Vec<&Parsed> = fresh
        .iter()
        .copied()
        .filter(|p| matches!(p.role, Role::Number(_)))
        .collect();
    if numbered.is_empty() {
        return Ok(Vec::new());
    }
    let entities: Vec<String> = numbered.iter().map(|p| p.entity.clone()).collect();
    let values: Vec<i64> = numbered
        .iter()
        .map(|p| match p.role {
            Role::Number(n) => n,
            _ => unreachable!(),
        })
        .collect();
    let mut held = numbers::numbered(tx, workspace, &entities).await?;
    let mut taken = numbers::taken(tx, workspace, &values).await?;
    let mut out = Vec::with_capacity(numbered.len());
    for (p, number) in numbered.iter().zip(values) {
        let duplicate = |reason: String| PushError::DuplicateNumber {
            index: first_at[&p.op_id],
            op_id: p.op_id.clone(),
            reason,
        };
        if let Some((have, _)) = held.get(&p.entity) {
            return Err(duplicate(format!(
                "ticket {} already has number {have}",
                p.entity
            )));
        }
        if let Some(by) = taken.get(&number) {
            return Err(duplicate(format!(
                "number {number} is already ticket {by}'s"
            )));
        }
        held.insert(p.entity.clone(), (number, 0));
        taken.insert(number, p.entity.clone());
        out.push((p.entity.clone(), number, p.op_id.clone()));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: u64 = 1_790_000_000_000;

    fn err_of(index: usize, raw: &str) -> (Option<String>, String) {
        let raw: &RawValue = serde_json::from_str(raw).unwrap();
        match parse_op(index, raw, NOW) {
            Err(PushError::Op { op_id, reason, .. }) => (op_id, reason),
            Err(_) => panic!("not an op error"),
            Ok(_) => panic!("parsed {raw}"),
        }
    }

    #[test]
    fn parses_indexed_columns_and_keeps_the_raw_text() {
        let raw = r#"{ "op_id": "01ARZ3NDEKTSV4RRFFQ69G5FAV", "hlc": {"wall_ms": 5, "counter": 2},
            "actor":"matt", "entity":"01ARZ3NDEKTSV4RRFFQ69G5FAW", "kind":"hold.clear", "version":1 }"#;
        let value: &RawValue = serde_json::from_str(raw).unwrap();
        let parsed = parse_op(0, value, NOW).unwrap();
        assert_eq!(parsed.op_id, "01ARZ3NDEKTSV4RRFFQ69G5FAV");
        assert_eq!((parsed.hlc_wall_ms, parsed.hlc_counter), (5, 2));
        assert_eq!(parsed.actor, "matt");
        assert_eq!(parsed.entity, "01ARZ3NDEKTSV4RRFFQ69G5FAW");
        assert_eq!(parsed.kind, "hold.clear");
        assert_eq!(parsed.raw, raw);
    }

    #[test]
    fn names_the_bad_op_when_it_can() {
        let (op_id, reason) = err_of(
            3,
            r#"{"op_id":"01ARZ3NDEKTSV4RRFFQ69G5FAV","hlc":{"wall_ms":1,"counter":0},
                "actor":"a","entity":"01ARZ3NDEKTSV4RRFFQ69G5FAW","kind":"nope","version":1}"#,
        );
        assert_eq!(op_id.as_deref(), Some("01ARZ3NDEKTSV4RRFFQ69G5FAV"));
        assert!(reason.contains("nope"), "{reason}");
        let (op_id, _) = err_of(0, "[1, 2]");
        assert_eq!(op_id, None);
        let (_, reason) = err_of(
            0,
            r#"{"op_id":"01ARZ3NDEKTSV4RRFFQ69G5FAV","hlc":{"wall_ms":1,"counter":0},
                "actor":"","entity":"01ARZ3NDEKTSV4RRFFQ69G5FAW","kind":"tombstone","version":1}"#,
        );
        assert_eq!(reason, "actor is empty");
        let (_, reason) = err_of(
            0,
            r#"{"op_id":"01ARZ3NDEKTSV4RRFFQ69G5FAV","hlc":{"wall_ms":1,"counter":0},
                "actor":"a","entity":"01ARZ3NDEKTSV4RRFFQ69G5FAW","kind":"tombstone","version":2}"#,
        );
        assert!(reason.starts_with("op version 2 is newer"), "{reason}");
    }

    fn stamp_err(raw: &str) -> (&'static str, Option<String>, String) {
        stamp_err_at(2, raw)
    }

    fn stamp_err_at(at: usize, raw: &str) -> (&'static str, Option<String>, String) {
        let raw: &RawValue = serde_json::from_str(raw).unwrap();
        match parse_op(at, raw, NOW) {
            Err(PushError::OpCheck {
                error,
                index,
                op_id,
                reason,
            }) => {
                assert_eq!(index, at);
                (error, op_id, reason)
            }
            Err(e) => panic!("not a stamp error: {e:?}"),
            Ok(_) => panic!("parsed {raw}"),
        }
    }

    fn with_hlc(wall_ms: u64, counter: u32) -> String {
        format!(
            r#"{{"op_id":"01ARZ3NDEKTSV4RRFFQ69G5FAV","hlc":{{"wall_ms":{wall_ms},"counter":{counter}}},
                "actor":"a","entity":"01ARZ3NDEKTSV4RRFFQ69G5FAW","kind":"tombstone","version":1}}"#
        )
    }

    #[test]
    fn refuses_out_of_range_and_far_future_stamps() {
        let (error, op_id, reason) = stamp_err(&with_hlc(u64::MAX, 0));
        assert_eq!(error, "invalid_stamp");
        assert_eq!(op_id.as_deref(), Some("01ARZ3NDEKTSV4RRFFQ69G5FAV"));
        assert!(reason.contains("wall_ms"), "{reason}");
        let (error, _, _) = stamp_err(&with_hlc(i64::MAX as u64 + 1, 0));
        assert_eq!(error, "invalid_stamp");
        let (error, _, reason) = stamp_err(&with_hlc(5, u32::MAX));
        assert_eq!(error, "invalid_stamp");
        assert!(reason.contains("counter"), "{reason}");
        let (error, _, _) = stamp_err(&with_hlc(NOW + MAX_FUTURE_SKEW_MS + 1, 0));
        assert_eq!(error, "future_stamp");

        // A day ahead is tolerated; history of any age is accepted.
        for (wall_ms, counter) in [(NOW + MAX_FUTURE_SKEW_MS, 3), (0, 0), (1, u32::MAX - 1)] {
            let raw = with_hlc(wall_ms, counter);
            let raw: &RawValue = serde_json::from_str(&raw).unwrap();
            assert!(parse_op(0, raw, NOW).is_ok(), "{wall_ms}.{counter}");
        }
    }

    #[test]
    fn refuses_path_unsafe_prefixes_and_project_ids() {
        let raw = r#"{"op_id":"01ARZ3NDEKTSV4RRFFQ69G5FAV","hlc":{"wall_ms":1,"counter":0},
            "actor":"a","entity":"01ARZ3NDEKTSV4RRFFQ69G5FAW","kind":"workspace.set",
            "payload":{"field":"prefix","value":"../../etc"},"version":1}"#;
        let (error, op_id, reason) = stamp_err_at(0, raw);
        assert_eq!(error, "invalid_id");
        assert_eq!(op_id.as_deref(), Some("01ARZ3NDEKTSV4RRFFQ69G5FAV"));
        assert!(reason.contains("workspace prefix"), "{reason}");
    }

    /// AGT-1467: a `body.edit` over the size bound is refused before it
    /// is stored; one at the bound, and ':'/device names, are judged by
    /// pm-core's rules.
    #[test]
    fn refuses_oversized_body_edits_and_windows_unsafe_names() {
        use base64::Engine as _;
        let edit = |len: usize| {
            let update = base64::engine::general_purpose::STANDARD.encode(vec![0u8; len]);
            format!(
                r#"{{"op_id":"01ARZ3NDEKTSV4RRFFQ69G5FAV","hlc":{{"wall_ms":1,"counter":0}},
                    "actor":"a","entity":"01ARZ3NDEKTSV4RRFFQ69G5FAW","kind":"body.edit",
                    "payload":{{"update":"{update}"}},"version":1}}"#
            )
        };
        let (error, op_id, reason) = stamp_err_at(1, &edit(pm_core::MAX_BODY_EDIT_BYTES + 1));
        assert_eq!(error, "op_too_large");
        assert_eq!(op_id.as_deref(), Some("01ARZ3NDEKTSV4RRFFQ69G5FAV"));
        assert!(reason.contains("body.edit update"), "{reason}");
        let ok = edit(pm_core::MAX_BODY_EDIT_BYTES);
        let raw: &RawValue = serde_json::from_str(&ok).unwrap();
        assert!(parse_op(0, raw, NOW).is_ok());

        let state = |name: &str| {
            format!(
                r#"{{"op_id":"01ARZ3NDEKTSV4RRFFQ69G5FAV","hlc":{{"wall_ms":1,"counter":0}},
                    "actor":"a","entity":"01ARZ3NDEKTSV4RRFFQ69G5FAW","kind":"state.upsert",
                    "payload":{{"name":"{name}","category":"started","position":0}},"version":1}}"#
            )
        };
        for bad in ["C:evil", "NUL", "com1.txt"] {
            let (error, _, _) = stamp_err_at(0, &state(bad));
            assert_eq!(error, "invalid_id", "{bad}");
        }
    }

    #[test]
    fn refuses_numbers_outside_the_allocator_range() {
        let number = |n: u64| {
            format!(
                r#"{{"op_id":"01ARZ3NDEKTSV4RRFFQ69G5FAV","hlc":{{"wall_ms":1,"counter":0}},
                    "actor":"a","entity":"01ARZ3NDEKTSV4RRFFQ69G5FAW","kind":"field.set",
                    "payload":{{"field":"number","value":{n}}},"version":1}}"#
            )
        };
        for bad in [0, numbers::MAX_NUMBER as u64 + 1, i64::MAX as u64, u64::MAX] {
            let (_, reason) = err_of(0, &number(bad));
            assert!(reason.contains("out of range"), "{bad}: {reason}");
        }
        let raw = number(numbers::MAX_NUMBER as u64);
        let raw: &RawValue = serde_json::from_str(&raw).unwrap();
        let parsed = parse_op(0, raw, NOW).unwrap();
        assert_eq!(parsed.role, Role::Number(numbers::MAX_NUMBER));
    }

    use crate::auth::ActorBinding;

    fn authed(actors: ActorBinding) -> Authed {
        Authed {
            workspace: "saltline".into(),
            token_id: 7,
            token_label: "laptop".into(),
            actors,
        }
    }

    fn parsed_as(actor: &str) -> String {
        format!(
            r#"{{"op_id":"01ARZ3NDEKTSV4RRFFQ69G5FAV","hlc":{{"wall_ms":1,"counter":0}},
                "actor":"{actor}","entity":"01ARZ3NDEKTSV4RRFFQ69G5FAW","kind":"tombstone","version":1}}"#
        )
    }

    #[test]
    fn named_actors_cover_every_actor_valued_payload_field() {
        let op = |kind: &str, payload: &str| -> Op {
            serde_json::from_str(&format!(
                r#"{{"op_id":"01ARZ3NDEKTSV4RRFFQ69G5FAV","hlc":{{"wall_ms":1,"counter":0}},
                    "actor":"a","entity":"01ARZ3NDEKTSV4RRFFQ69G5FAW","kind":"{kind}",
                    "payload":{payload},"version":1}}"#
            ))
            .unwrap()
        };
        let named = |o: &Op| -> Vec<(&str, String)> {
            named_actors(o)
                .into_iter()
                .map(|(f, a)| (f, a.to_string()))
                .collect()
        };
        assert_eq!(
            named(&op("claim", r#"{"state":"s","assignee":"m"}"#)),
            [("assignee", "m".to_string())]
        );
        assert_eq!(
            named(&op(
                "hold.set",
                r#"{"hold":{"reason":"r","by":"m","at":{"wall_ms":1,"counter":0}}}"#
            )),
            [("hold.by", "m".to_string())]
        );
        assert_eq!(
            named(&op("field.set", r#"{"field":"assignee","value":"m"}"#)),
            [("assignee", "m".to_string())]
        );
        assert!(named(&op("field.set", r#"{"field":"assignee","value":null}"#)).is_empty());
        assert_eq!(
            named(&op("actor.upsert", r#"{"id":"m","kind":"human"}"#)),
            [("id", "m".to_string())]
        );
        assert!(named(&op("label.add", r#"{"label":"l"}"#)).is_empty());
    }

    #[test]
    fn actors_are_checked_against_the_token_binding() {
        let bound = authed(ActorBinding::Patterns(vec![
            "matt".into(),
            "claude:*".into(),
        ]));
        let legacy = authed(ActorBinding::Legacy);
        let any = authed(ActorBinding::Patterns(vec!["*".into()]));
        let check = |who: &Authed, actor: &str, seeded: bool| {
            let raw = parsed_as(actor);
            let raw: &RawValue = serde_json::from_str(&raw).unwrap();
            let p = parse_op(0, raw, NOW).unwrap();
            check_actor(who, seeded, &p, 4)
        };
        for seeded in [false, true] {
            assert!(check(&bound, "matt", seeded).is_ok());
            assert!(check(&bound, "claude:pm-build", seeded).is_ok());
            match check(&bound, "alice", seeded) {
                Err(PushError::ActorNotAllowed { index, reason, .. }) => {
                    assert_eq!(index, 4);
                    assert!(reason.contains("token 7 (laptop)"), "{reason}");
                    assert!(reason.contains("matt,claude:*"), "{reason}");
                }
                other => panic!("{other:?}"),
            }
            // Legacy and `*` tokens: any actor, as before bindings.
            assert!(check(&legacy, "alice", seeded).is_ok());
            assert!(check(&any, "alice", seeded).is_ok());
            // The hub's actor: never from a bound token.
            assert!(matches!(
                check(&bound, "hub", seeded),
                Err(PushError::ReservedActor { index: 4, .. })
            ));
        }
        // Once seeded, not from anyone; while seeding, an unrestricted
        // token may reseed the hub's history.
        assert!(matches!(
            check(&legacy, "hub", true),
            Err(PushError::ReservedActor { .. })
        ));
        assert!(check(&legacy, "hub", false).is_ok());
        assert!(check(&any, "hub", false).is_ok());
    }
}
