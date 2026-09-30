//! The hub-side materialized views and claim arbitration (AGT-1392, README
//! §Sync & hub "Conditional ops at the hub", §Conflict semantics "Claims").
//!
//! A `claim` is the one op that is not a CRDT: `claim if state is
//! unstarted and unassigned`. Once a workspace is seeded the hub is its
//! authority, and to judge a claim it needs to know each ticket's state
//! and assignee. So the hub keeps, per workspace, one [`TicketView`] per
//! ticket and one [`WorkspaceView`] (the states), in `ticket_views` /
//! `workspace_views` (migration 0003), and folds every op it stores into
//! them **inside the push transaction** with pm-core's own [`apply`] and
//! [`apply_workspace`] — the same functions pm-store's commit path runs
//! — so the hub's view of a ticket is, by construction, what a client's
//! `pm doctor --rebuild` of the same ops produces. The hub applies no
//! merge logic of its own; [`Views::fold`] is the whole of it.
//!
//! **Arbitration mirrors pm-store.** pm-store judges a claim in
//! `commit::next_view` with `Fold::Local`: load the ticket's view, run
//! [`TicketView::claim_admissible`] against the workspace's states, and
//! only then fold and append. [`Views::fold`] with `arbitrate = true`
//! does exactly that; a claim it rejects is folded into nothing and the
//! push stores nothing for it (`ops`). Within a batch the views advance
//! op by op, so of two claims on one ticket in one batch the first is
//! admitted and the second rejected; across batches the push path's
//! per-workspace lock serialises every decision, so of any number of
//! concurrent claims exactly one lands.
//!
//! **Seed mode admits every claim.** While a workspace is seeding
//! (`seeded_at IS NULL`, see `numbers`) the log being uploaded is
//! history: each claim in it was admitted by the then-authority (the
//! client's own SQLite database, README §Authority "in phases 1–2 the
//! local database") when it happened, and the hub "becomes the authority
//! only after the seed is fully acknowledged". Re-arbitrating would also
//! be wrong on its face — the seed's `state.upsert` ops may arrive after
//! the claims that depend on them, and a claim admitted in 2026-07 is not
//! void because the ticket has since been done and re-claimed. So a
//! seeding push folds claims the way pm-store's `Fold::Replay` and
//! `Fold::Foreign` do: as plain LWW writes. The same holds for a rebuild
//! from the log ([`rebuild`]): the log holds only admitted claims.
//!
//! **What the views fold.** Every ticket kind except `body.edit`, and the
//! workspace config kinds (`workspace.set`, `state.upsert`,
//! `actor.upsert`). A `body.edit` is skipped: the description is a Loro
//! document that no arbitration reads, and folding it would mean
//! importing and re-snapshotting up to 23 MB of CRDT history per push
//! (the entity namespace is also shared with project documents, whose
//! edits are not ticket ops at all). Project kinds (`project.*`) are
//! skipped too: nothing conditional depends on them. Skipping is not
//! merging — every op is still stored and served verbatim; the view just
//! does not carry those fields. Hub-authored `field.set number` ops and
//! the number ops a seed carries fold like any other.
//!
//! **Order.** The hub folds ops in seq order. pm-core's rules are
//! idempotent and order-independent, so an op that arrives ahead of its
//! ticket's `ticket.create` (a seed pushed in batches, an out-of-order
//! outbox) folds into a fresh [`TicketView::new`], as pm-store's pull
//! path lets it, and the view converges once the create lands.

use std::collections::{HashMap, HashSet};

use pm_core::{
    ActorId, ApplyError, ClaimRejected, ConfigApplyError, Hlc, Op, Payload, State, TicketView,
    WorkspaceView, apply, apply_workspace,
};
use serde::Serialize;
use tokio_postgres::Transaction;
use ulid::Ulid;

use crate::numbers::Numbered;

/// What a push answers for a claim the hub refused: who holds the ticket
/// and since when, mirroring `pm claim --json`'s `{taken_by, at, …}`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Rejection {
    /// The ticket's assignee, if any. `null` when the ticket left
    /// `unstarted` without one (done, canceled, …) or is deleted.
    pub taken_by: Option<ActorId>,
    /// When the ticket entered the condition that refuses the claim: the
    /// stamp of the write that set its assignee (`already_assigned`), its
    /// state (`not_unstarted`) or its tombstone (`deleted`) — for a claimed
    /// ticket, the admitted claim's HLC.
    pub at: Hlc,
    /// The ticket's state at the hub.
    pub state: String,
    /// `not_unstarted`, `already_assigned` or `deleted`.
    pub code: &'static str,
    /// [`ClaimRejected`]'s message, as `pm claim --json` reports it.
    pub reason: String,
}

/// What folding one op did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// The op is in the view (or is one the view does not carry).
    Folded,
    /// A claim the authority refused: the view is unchanged.
    Rejected(Rejection),
}

/// An op that cannot fold. A caller bug or a malformed op: the client's
/// own store would have refused to commit it.
#[derive(Debug, thiserror::Error)]
pub enum FoldError {
    #[error("does not fold into its ticket: {0}")]
    Ticket(#[from] ApplyError),
    #[error("does not fold into the workspace: {0}")]
    Config(#[from] ConfigApplyError),
    #[error("targets workspace {entity}, but this hub workspace's config is workspace {workspace}")]
    ForeignWorkspace { entity: Ulid, workspace: Ulid },
}

/// The views a push works on: the workspace's config view and the ticket
/// views of the entities in the batch, loaded once ([`Views::load`]),
/// folded in memory and written back ([`Views::save`]).
#[derive(Default)]
pub struct Views {
    workspace: Option<WorkspaceView>,
    tickets: HashMap<Ulid, TicketView>,
    dirty: HashSet<Ulid>,
    workspace_dirty: bool,
}

impl Views {
    /// The workspace's states, ordered as pm-store's `state` table is.
    fn states(&self) -> Vec<State> {
        self.workspace
            .as_ref()
            .map(|w| w.snapshot().states)
            .unwrap_or_default()
    }

    /// Folds `op`. With `arbitrate`, a `claim` is first judged by
    /// [`TicketView::claim_admissible`] against the workspace's states
    /// and, when refused, left out ([`Verdict::Rejected`]); without it
    /// (seed mode, rebuild) every claim folds as a plain write. Pure:
    /// no IO.
    pub fn fold(&mut self, op: &Op, arbitrate: bool) -> Result<Verdict, FoldError> {
        match &op.payload {
            Payload::WorkspaceSet(_) | Payload::StateUpsert(_) | Payload::ActorUpsert(_) => {
                let view = self
                    .workspace
                    .get_or_insert_with(|| WorkspaceView::new(op.entity));
                if view.id != op.entity {
                    return Err(FoldError::ForeignWorkspace {
                        entity: op.entity,
                        workspace: view.id,
                    });
                }
                apply_workspace(view, op)?;
                self.workspace_dirty = true;
                Ok(Verdict::Folded)
            }
            Payload::ProjectCreate(_)
            | Payload::ProjectSet(_)
            | Payload::ProjectDelete
            | Payload::ProjectDocAdd(_)
            | Payload::BodyEdit(_) => Ok(Verdict::Folded),
            _ => {
                let states = self.states();
                let view = self
                    .tickets
                    .entry(op.entity)
                    .or_insert_with(|| TicketView::new(op.entity));
                if arbitrate
                    && matches!(op.payload, Payload::Claim(_))
                    && let Err(reason) = view.claim_admissible(&states)
                {
                    return Ok(Verdict::Rejected(rejection(view, reason)));
                }
                apply(view, op)?;
                self.dirty.insert(op.entity);
                Ok(Verdict::Folded)
            }
        }
    }

    /// Folds one of the hub's own `field.set number` ops (`numbers`)
    /// into its ticket's view. The hub built the op a moment ago, so a
    /// fold failure is a bug in this binary, not a request error: it
    /// panics, and the push's transaction rolls back rather than commit
    /// a view that disagrees with the log.
    pub fn fold_hub_op(&mut self, numbered: &Numbered) {
        let op: Op = serde_json::from_str(numbered.op.get()).expect("the hub wrote this op");
        self.fold(&op, false).expect("a number op folds");
    }

    /// The workspace's config view and the views of `entities` (a
    /// missing one is simply not there yet), under the caller's
    /// workspace lock.
    pub async fn load(
        tx: &Transaction<'_>,
        workspace: &str,
        entities: &[String],
    ) -> Result<Views, tokio_postgres::Error> {
        let mut views = Views::default();
        if let Some(row) = tx
            .query_opt(
                "SELECT view FROM workspace_views WHERE workspace_id = $1",
                &[&workspace],
            )
            .await?
        {
            views.workspace = Some(decode(row.get(0)));
        }
        let rows = tx
            .query(
                "SELECT entity, view FROM ticket_views
                 WHERE workspace_id = $1 AND entity = ANY($2)",
                &[&workspace, &entities],
            )
            .await?;
        for row in rows {
            let view: TicketView = decode(row.get(1));
            views.tickets.insert(view.id, view);
        }
        Ok(views)
    }

    /// Writes back every view [`fold`](Self::fold) changed.
    pub async fn save(
        &mut self,
        tx: &Transaction<'_>,
        workspace: &str,
    ) -> Result<(), tokio_postgres::Error> {
        if self.workspace_dirty {
            let view = self.workspace.as_ref().expect("dirty means present");
            tx.execute(
                "INSERT INTO workspace_views (workspace_id, entity, view) VALUES ($1, $2, $3)
                 ON CONFLICT (workspace_id) DO UPDATE SET entity = $2, view = $3",
                &[&workspace, &view.id.to_string(), &encode(view)],
            )
            .await?;
            self.workspace_dirty = false;
        }
        if self.dirty.is_empty() {
            return Ok(());
        }
        let mut entities = Vec::with_capacity(self.dirty.len());
        let mut texts = Vec::with_capacity(self.dirty.len());
        for id in &self.dirty {
            entities.push(id.to_string());
            texts.push(encode(&self.tickets[id]));
        }
        tx.execute(
            "INSERT INTO ticket_views (workspace_id, entity, view)
             SELECT $1, n.entity, n.view FROM unnest($2::text[], $3::text[]) AS n (entity, view)
             ON CONFLICT (workspace_id, entity) DO UPDATE SET view = excluded.view",
            &[&workspace, &entities, &texts],
        )
        .await?;
        self.dirty.clear();
        Ok(())
    }
}

/// The rejection for a claim `view` cannot admit.
fn rejection(view: &TicketView, reason: ClaimRejected) -> Rejection {
    let (code, since) = match &reason {
        ClaimRejected::Deleted => ("deleted", view.deleted_at.as_ref()),
        ClaimRejected::NotUnstarted { .. } => ("not_unstarted", view.state.stamp.as_ref()),
        ClaimRejected::AlreadyAssigned { .. } => ("already_assigned", view.assignee.stamp.as_ref()),
    };
    Rejection {
        taken_by: view.assignee.value.clone(),
        at: since.or(view.updated.as_ref()).map_or(Hlc::ZERO, |s| s.hlc),
        state: view.state.value.clone(),
        code,
        reason: reason.to_string(),
    }
}

fn encode<T: Serialize>(view: &T) -> String {
    serde_json::to_string(view).expect("a view serializes")
}

/// A stored view is one this hub wrote with pm-core's serde; one it
/// cannot read is a deployment bug (a downgraded build), not a request
/// error, so it is loud.
fn decode<T: serde::de::DeserializeOwned>(text: String) -> T {
    serde_json::from_str(&text).expect("a stored view is pm-core JSON")
}

/// [`Views::fold_hub_op`] for a caller holding no views yet (the seed
/// end): loads the tickets, folds, saves.
pub async fn fold_hub_ops(
    tx: &Transaction<'_>,
    workspace: &str,
    numbered: &[Numbered],
) -> Result<(), tokio_postgres::Error> {
    if numbered.is_empty() {
        return Ok(());
    }
    let entities: Vec<String> = numbered.iter().map(|n| n.entity.clone()).collect();
    let mut views = Views::load(tx, workspace, &entities).await?;
    for n in numbered {
        views.fold_hub_op(n);
    }
    views.save(tx, workspace).await
}

/// Rebuilds every workspace's views from its log, in seq order, replacing
/// whatever the tables hold: the migration that adds the tables runs it
/// to backfill existing logs. `body.edit` ops are not read at all (the
/// view does not carry them; the Studio's log holds a 23 MB one). A
/// stored op that does not fold is skipped with a warning — the log is
/// opaque and already accepted, and a rebuild must not stop the hub from
/// starting.
pub async fn rebuild(tx: &Transaction<'_>) -> Result<(), tokio_postgres::Error> {
    tx.batch_execute("DELETE FROM ticket_views; DELETE FROM workspace_views")
        .await?;
    let workspaces = tx
        .query("SELECT id FROM workspaces ORDER BY id", &[])
        .await?;
    for row in workspaces {
        let workspace: String = row.get(0);
        let rows = tx
            .query(
                "SELECT seq, op::text FROM ops
                 WHERE workspace_id = $1 AND kind <> 'body.edit'
                 ORDER BY seq",
                &[&workspace],
            )
            .await?;
        let mut views = Views::default();
        for row in rows {
            let seq: i64 = row.get(0);
            let text: String = row.get(1);
            let op: Op = match serde_json::from_str(&text) {
                Ok(op) => op,
                Err(e) => {
                    eprintln!("pm-hub: views: {workspace} seq {seq} is not a pm op: {e}");
                    continue;
                }
            };
            if let Err(e) = views.fold(&op, false) {
                eprintln!("pm-hub: views: {workspace} seq {seq} ({}): {e}", op.op_id);
            }
        }
        views.save(tx, &workspace).await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use pm_core::op::{Claim, FieldSet, StateTransition, StateUpsert, TicketCreate, WorkspaceSet};
    use pm_core::{Priority, StateCategory};

    fn op(entity: Ulid, wall_ms: u64, actor: &str, payload: Payload) -> Op {
        Op::new(
            Ulid::new(),
            Hlc::new(wall_ms, 0),
            ActorId::new(actor),
            entity,
            payload,
        )
    }

    fn state(ws: Ulid, wall_ms: u64, name: &str, category: StateCategory, position: u32) -> Op {
        op(
            ws,
            wall_ms,
            "matt",
            Payload::StateUpsert(StateUpsert {
                name: name.into(),
                category,
                position,
            }),
        )
    }

    fn create(ticket: Ulid, wall_ms: u64, state: &str) -> Op {
        op(
            ticket,
            wall_ms,
            "matt",
            Payload::TicketCreate(TicketCreate {
                title: "t".into(),
                state: state.into(),
                priority: Priority::Medium,
                project: None,
                repo: None,
                source: None,
                ext: Default::default(),
            }),
        )
    }

    fn claim(ticket: Ulid, wall_ms: u64, actor: &str) -> Op {
        op(
            ticket,
            wall_ms,
            actor,
            Payload::Claim(Claim {
                state: "in-progress".into(),
                assignee: ActorId::new(actor),
            }),
        )
    }

    fn configured() -> (Views, Ulid) {
        let ws = Ulid::new();
        let mut views = Views::default();
        for o in [
            state(ws, 1, "triage", StateCategory::Unstarted, 0),
            state(ws, 2, "in-progress", StateCategory::Started, 1),
            state(ws, 3, "done", StateCategory::Completed, 2),
        ] {
            assert_eq!(views.fold(&o, true).unwrap(), Verdict::Folded);
        }
        (views, ws)
    }

    #[test]
    fn first_claim_wins_and_the_loser_learns_who_and_when() {
        let (mut views, _) = configured();
        let t = Ulid::new();
        views.fold(&create(t, 10, "triage"), true).unwrap();
        let winner = claim(t, 20, "a");
        assert_eq!(views.fold(&winner, true).unwrap(), Verdict::Folded);
        let lost = views.fold(&claim(t, 30, "b"), true).unwrap();
        assert_eq!(
            lost,
            Verdict::Rejected(Rejection {
                taken_by: Some(ActorId::new("a")),
                at: Hlc::new(20, 0),
                state: "in-progress".into(),
                code: "not_unstarted",
                reason: "ticket is in state 'in-progress', which is not unstarted".into(),
            })
        );
        // The refused claim left no trace.
        let view = views.tickets.get(&t).unwrap();
        assert_eq!(view.assignee.value, Some(ActorId::new("a")));
        assert_eq!(view.updated.as_ref().unwrap().hlc, Hlc::new(20, 0));
        assert!(views.dirty.contains(&t));
    }

    #[test]
    fn assigned_but_unstarted_is_already_assigned_and_deleted_is_deleted() {
        let (mut views, _) = configured();
        let t = Ulid::new();
        views.fold(&create(t, 10, "triage"), true).unwrap();
        views
            .fold(
                &op(
                    t,
                    11,
                    "matt",
                    Payload::FieldSet(FieldSet::Assignee(Some(ActorId::new("x")))),
                ),
                true,
            )
            .unwrap();
        match views.fold(&claim(t, 12, "b"), true).unwrap() {
            Verdict::Rejected(r) => {
                assert_eq!(r.code, "already_assigned");
                assert_eq!(r.taken_by, Some(ActorId::new("x")));
                assert_eq!(r.at, Hlc::new(11, 0));
                assert_eq!(r.state, "triage");
            }
            other => panic!("{other:?}"),
        }
        views
            .fold(&op(t, 13, "matt", Payload::Tombstone), true)
            .unwrap();
        match views.fold(&claim(t, 14, "b"), true).unwrap() {
            Verdict::Rejected(r) => {
                assert_eq!(r.code, "deleted");
                assert_eq!(r.at, Hlc::new(13, 0));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn unclaim_or_done_then_claim_again() {
        let (mut views, _) = configured();
        let t = Ulid::new();
        views.fold(&create(t, 10, "triage"), true).unwrap();
        views.fold(&claim(t, 20, "a"), true).unwrap();
        // Unclaim: back to triage, unassigned — claimable again.
        views
            .fold(
                &op(
                    t,
                    30,
                    "a",
                    Payload::StateTransition(StateTransition {
                        state: "triage".into(),
                    }),
                ),
                true,
            )
            .unwrap();
        views
            .fold(
                &op(t, 31, "a", Payload::FieldSet(FieldSet::Assignee(None))),
                true,
            )
            .unwrap();
        assert_eq!(
            views.fold(&claim(t, 40, "b"), true).unwrap(),
            Verdict::Folded
        );
        // Done: not unstarted, still assigned to b.
        views
            .fold(
                &op(
                    t,
                    50,
                    "b",
                    Payload::StateTransition(StateTransition {
                        state: "done".into(),
                    }),
                ),
                true,
            )
            .unwrap();
        match views.fold(&claim(t, 60, "c"), true).unwrap() {
            Verdict::Rejected(r) => {
                assert_eq!(
                    (r.code, r.taken_by),
                    ("not_unstarted", Some(ActorId::new("b")))
                );
                assert_eq!(r.at, Hlc::new(50, 0));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn states_come_from_the_config_ops_and_nothing_else() {
        let (mut views, ws) = configured();
        let refined = Ulid::new();
        let blocked = Ulid::new();
        views.fold(&create(refined, 10, "refined"), true).unwrap();
        views.fold(&create(blocked, 10, "blocked"), true).unwrap();
        // `refined` is not a state yet: not unstarted.
        assert!(matches!(
            views.fold(&claim(refined, 11, "a"), true).unwrap(),
            Verdict::Rejected(Rejection {
                code: "not_unstarted",
                taken_by: None,
                ..
            })
        ));
        views
            .fold(&state(ws, 12, "refined", StateCategory::Unstarted, 5), true)
            .unwrap();
        assert_eq!(
            views.fold(&claim(refined, 13, "a"), true).unwrap(),
            Verdict::Folded
        );
        assert!(matches!(
            views.fold(&claim(blocked, 14, "a"), true).unwrap(),
            Verdict::Rejected(Rejection {
                code: "not_unstarted",
                ..
            })
        ));
        // A workspace with no config at all admits nothing.
        let mut bare = Views::default();
        let t = Ulid::new();
        bare.fold(&create(t, 1, "triage"), true).unwrap();
        assert!(matches!(
            bare.fold(&claim(t, 2, "a"), true).unwrap(),
            Verdict::Rejected(_)
        ));
    }

    #[test]
    fn seed_mode_folds_every_claim_as_history() {
        let (mut views, _) = configured();
        let t = Ulid::new();
        // Claims ahead of the create, twice over: all history, all folded.
        assert_eq!(
            views.fold(&claim(t, 20, "a"), false).unwrap(),
            Verdict::Folded
        );
        assert_eq!(
            views.fold(&claim(t, 30, "b"), false).unwrap(),
            Verdict::Folded
        );
        assert_eq!(
            views.fold(&create(t, 10, "triage"), false).unwrap(),
            Verdict::Folded
        );
        let view = views.tickets.get(&t).unwrap();
        assert_eq!(view.assignee.value, Some(ActorId::new("b")));
        assert_eq!(view.state.value, "in-progress");
        assert_eq!(view.created.as_ref().unwrap().hlc, Hlc::new(10, 0));
    }

    #[test]
    fn config_for_a_second_workspace_ulid_is_foreign() {
        let (mut views, ws) = configured();
        let other = Ulid::new();
        let err = views
            .fold(
                &op(
                    other,
                    9,
                    "matt",
                    Payload::WorkspaceSet(WorkspaceSet::Prefix("X".into())),
                ),
                true,
            )
            .unwrap_err();
        match err {
            FoldError::ForeignWorkspace { entity, workspace } => {
                assert_eq!((entity, workspace), (other, ws));
            }
            other => panic!("{other}"),
        }
        assert_eq!(views.workspace.as_ref().unwrap().id, ws);
    }

    #[test]
    fn body_edits_and_project_ops_are_not_carried() {
        let mut views = Views::default();
        let doc = Ulid::new();
        let edit = op(
            doc,
            1,
            "matt",
            Payload::BodyEdit(pm_core::op::BodyEdit {
                update: vec![1, 2, 3],
            }),
        );
        assert_eq!(views.fold(&edit, true).unwrap(), Verdict::Folded);
        assert_eq!(
            views
                .fold(&op(doc, 2, "matt", Payload::ProjectDelete), true)
                .unwrap(),
            Verdict::Folded
        );
        assert!(views.tickets.is_empty() && views.dirty.is_empty());
        assert!(views.workspace.is_none());
    }
}
