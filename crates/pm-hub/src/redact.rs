//! Redaction at the hub (AGT-2427): the server-side half of pm-core's
//! `redact` op.
//!
//! A `redact` names ops of its own entity whose text to erase. The hub
//! stores each op's JSON verbatim (`ops.op`), so it must rewrite the
//! targets' rows itself, or every replica that pulls later — and anyone
//! reading the database — would get the text back:
//!
//! - **Stored targets.** Inside the push transaction, after the batch is
//!   stored ([`apply`]), every target of a redact in the workspace whose
//!   entity is touched by the batch — a redact pushed now, or a target
//!   pushed now after its redact — has its `op` rewritten with
//!   `pm_core::Op::redacted` (op id, stamp, actor, entity, kind and
//!   version kept), and the ticket views of those entities are refolded
//!   from the stored, now redacted, log. Either order ends in the same
//!   rows, which pulls then serve.
//! - **Who may redact.** An unrestricted token may name any op. An
//!   actor-bound token may name only ops it could have authored: each
//!   target must already be stored (or come earlier in the same batch) on
//!   the redact's entity, as an actor its binding permits ([`check`]);
//!   otherwise the batch is refused `redact_not_allowed`. Without this, an
//!   agent token could erase a human's comments.
//! - **Older clients.** A build before redaction cannot parse a `redact`
//!   op, and its pull fails on an op it does not understand. So `pull`
//!   serves `redact` ops only to a client that sends
//!   [`ACCEPT_HEADER`]; an older client still receives every redacted
//!   target as rewritten here (a valid op it folds as usual), just not
//!   the redact itself. When it upgrades, its first sync fetches every
//!   redact once (`GET ops?kind=redact`) and applies them locally.

use std::collections::{HashMap, HashSet};

use pm_core::{Op, Payload};
use tokio_postgres::Transaction;
use ulid::Ulid;

use crate::views::Views;

/// The request header a client sends to receive `redact` ops in its
/// pulls (value [`ACCEPT_VALUE`]). Without it they are left out.
pub const ACCEPT_HEADER: &str = "pm-accept-ops";
/// [`ACCEPT_HEADER`]'s value: the op kinds beyond the original set the
/// client understands, comma-separated; today only `redact`.
pub const ACCEPT_VALUE: &str = "redact";

/// The targets of `op` when it is a `redact`.
pub fn targets(op: &Op) -> Option<&[Ulid]> {
    match &op.payload {
        Payload::Redact(r) => Some(&r.targets),
        _ => None,
    }
}

/// Why an actor-bound token may not push a redact: the first target it
/// may not name.
pub struct NotAllowed {
    pub target: Ulid,
    pub why: String,
}

/// For an actor-bound token (`permits` = its binding): each target of
/// `redact` must be an op on the redact's entity, stored already or in
/// `earlier` (this batch's fresh ops before it, by op id: entity and
/// actor), authored by an actor `permits` allows.
pub async fn check(
    tx: &Transaction<'_>,
    workspace: &str,
    redact: &Op,
    earlier: &HashMap<Ulid, (Ulid, String)>,
    permits: impl Fn(&str) -> bool,
) -> Result<Result<(), NotAllowed>, tokio_postgres::Error> {
    let Some(targets) = targets(redact) else {
        return Ok(Ok(()));
    };
    let ids: Vec<String> = targets.iter().map(Ulid::to_string).collect();
    let mut stored: HashMap<Ulid, (Ulid, String)> = HashMap::new();
    for row in tx
        .query(
            "SELECT op_id, entity, actor FROM ops WHERE workspace_id = $1 AND op_id = ANY($2)",
            &[&workspace, &ids],
        )
        .await?
    {
        let (op_id, entity, actor): (String, String, String) = (row.get(0), row.get(1), row.get(2));
        if let (Ok(op_id), Ok(entity)) = (op_id.parse(), entity.parse()) {
            stored.insert(op_id, (entity, actor));
        }
    }
    for target in targets {
        let Some((entity, actor)) = stored.get(target).or_else(|| earlier.get(target)) else {
            return Ok(Err(NotAllowed {
                target: *target,
                why: "the hub does not hold it".to_string(),
            }));
        };
        if *entity != redact.entity {
            return Ok(Err(NotAllowed {
                target: *target,
                why: format!("it is an op of {entity}, not of {}", redact.entity),
            }));
        }
        if !permits(actor) {
            return Ok(Err(NotAllowed {
                target: *target,
                why: format!("it was written by {actor:?}"),
            }));
        }
    }
    Ok(Ok(()))
}

/// Applies every redact in `workspace` that touches `entities` (the
/// entities of the ops a push just stored) to its stored targets, and
/// refolds the ticket views of the entities whose rows changed into
/// `views` (saved by the caller with the rest of the push). Returns how
/// many rows were rewritten.
pub async fn apply(
    tx: &Transaction<'_>,
    workspace: &str,
    entities: &HashSet<String>,
    views: &mut Views,
) -> Result<usize, tokio_postgres::Error> {
    if entities.is_empty() {
        return Ok(0);
    }
    let entities: Vec<&String> = entities.iter().collect();
    // Every (entity, target) a stored redact on these entities names,
    // joined to the target row when the workspace holds it.
    let rows = tx
        .query(
            "SELECT t.seq, t.op::text
             FROM ops AS r
             CROSS JOIN LATERAL json_array_elements_text(r.op->'payload'->'targets') AS x(id)
             JOIN ops AS t
               ON t.workspace_id = r.workspace_id AND t.op_id = x.id AND t.entity = r.entity
             WHERE r.workspace_id = $1 AND r.kind = 'redact' AND r.entity = ANY($2)
               AND t.kind <> 'redact'",
            &[&workspace, &entities],
        )
        .await?;
    let mut changed = 0;
    let mut refold: HashSet<Ulid> = HashSet::new();
    for row in rows {
        let seq: i64 = row.get(0);
        let text: String = row.get(1);
        let Ok(op) = serde_json::from_str::<Op>(&text) else {
            eprintln!("pm-hub: redact: {workspace} seq {seq} is not a pm op; left as it is");
            continue;
        };
        let Some(blank) = op.redacted() else {
            continue;
        };
        if blank == op {
            continue;
        }
        let json = serde_json::to_string(&blank).expect("an op serializes");
        tx.execute(
            "UPDATE ops SET op = $1::text::json WHERE seq = $2",
            &[&json, &seq],
        )
        .await?;
        changed += 1;
        if !matches!(blank.payload, Payload::BodyEdit(_)) {
            refold.insert(blank.entity);
        }
    }
    for entity in refold {
        refold_ticket(tx, workspace, entity, views).await?;
    }
    Ok(changed)
}

/// Replaces `entity`'s ticket view in `views` with one folded from its
/// stored ops (the ones a view carries), in seq order, as
/// `views::rebuild` does for the whole log.
async fn refold_ticket(
    tx: &Transaction<'_>,
    workspace: &str,
    entity: Ulid,
    views: &mut Views,
) -> Result<(), tokio_postgres::Error> {
    let rows = tx
        .query(
            "SELECT seq, op::text FROM ops
             WHERE workspace_id = $1 AND entity = $2 AND kind <> 'body.edit'
             ORDER BY seq",
            &[&workspace, &entity.to_string()],
        )
        .await?;
    let mut ops = Vec::with_capacity(rows.len());
    for row in rows {
        let seq: i64 = row.get(0);
        let text: String = row.get(1);
        match serde_json::from_str::<Op>(&text) {
            Ok(op) => ops.push(op),
            Err(e) => eprintln!("pm-hub: redact: {workspace} seq {seq} is not a pm op: {e}"),
        }
    }
    views.refold_ticket(entity, &ops);
    Ok(())
}
