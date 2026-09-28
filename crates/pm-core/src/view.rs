//! A ticket's merge state and the one function that mutates it.
//!
//! [`TicketView`] holds every field as the CRDT its rule calls for
//! (README §Conflict semantics); [`apply`] folds one [`Op`] into it. Because
//! each rule is idempotent and order-independent, replaying the log in any
//! order — or replaying an op twice — yields the same view, and
//! [`TicketView::snapshot`] turns it into the plain [`Ticket`].

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use ulid::Ulid;

use crate::body::{Body, BodyError, BodyUpdate};
use crate::domain::{
    ActorId, Comment, Hold, NotBefore, Parked, Priority, Relation, Source, State, StateCategory,
    Ticket, Waiver,
};
use crate::hlc::{Hlc, Stamp};
use crate::merge::{CommentLog, Lww, OrSet};
use crate::op::{FieldSet, Op, Payload};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TicketView {
    pub id: Ulid,
    /// Stamp of the `ticket.create` op, once seen.
    pub created: Option<Stamp>,
    /// Greatest stamp of any op applied.
    pub updated: Option<Stamp>,
    pub number: Lww<Option<u64>>,
    pub title: Lww<String>,
    pub state: Lww<String>,
    pub priority: Lww<Priority>,
    pub project: Lww<Option<String>>,
    pub repo: Lww<Option<String>>,
    pub assignee: Lww<Option<ActorId>>,
    pub linked_github: Lww<Option<String>>,
    pub linked_pr: Lww<Option<String>>,
    pub linear: Lww<Option<String>>,
    pub source: Lww<Option<Source>>,
    pub hold: Lww<Option<Hold>>,
    pub waivers: Lww<Vec<Waiver>>,
    pub not_before: Lww<Option<NotBefore>>,
    pub parked: Lww<Option<Parked>>,
    pub archived_at: Lww<Option<Hlc>>,
    /// One register per key; a `None` value is a deleted key.
    pub ext: BTreeMap<String, Lww<Option<Value>>>,
    /// Tombstone: the earliest tombstone stamp seen (an earlier one may
    /// arrive out of order); never cleared, since deletes are never undone.
    pub deleted_at: Option<Stamp>,
    pub labels: OrSet<String>,
    pub relations: OrSet<Relation>,
    pub comments: CommentLog,
    /// The description as a text CRDT; every `body.edit` update is folded
    /// in with [`Body::apply`].
    pub body: BodyState,
}

/// A [`Body`] replica that only ever *imports*: the view never edits the
/// text itself, so its peer id is fixed at 0 and [`apply`] stays free of
/// randomness (`Body::new` would draw a random peer).
///
/// `Body` has no `Clone`, `PartialEq` or serde, so this wrapper supplies
/// the semantics the view needs:
/// - **Equality** is by materialized text. Two replicas that have seen the
///   same updates in any order have the same text (that is the CRDT's
///   promise); an update still queued behind a missing dependency is not
///   visible in either the text or the comparison.
/// - **Serde** persists the Loro *snapshot* (`Body::snapshot`), which keeps
///   the full history so a restored view goes on merging later updates.
///   The bytes serialize like `BodyEdit::update` does: a base64 string
///   ([`crate::bytes::base64`], AGT-1378), with the byte-array form a
///   view row was written in before that still accepted.
/// - **Clone** goes through the same snapshot/restore path.
///
/// The alternative — keeping the raw update log on the view and
/// materializing on demand — would have made `snapshot()` fallible and
/// re-run every import per read, so the view holds the live replica.
#[derive(Debug)]
pub struct BodyState {
    body: Body,
}

impl BodyState {
    /// The peer id every view replica uses. The view never produces local
    /// edits, so the id is never stamped on an update; it only exists to
    /// avoid `Body::new`'s random draw.
    pub const PEER: u64 = 0;

    /// An empty body.
    pub fn new() -> Self {
        BodyState {
            body: Body::with_peer(Self::PEER).expect("a fresh doc accepts a peer id"),
        }
    }

    /// The materialized text.
    pub fn text(&self) -> String {
        self.body.text()
    }

    /// Folds one update (or snapshot) in; idempotent and order-independent.
    pub fn apply(&mut self, update: &BodyUpdate) -> Result<(), BodyError> {
        self.body.apply(update)
    }

    /// Everything this replica knows, as one blob: what serde writes.
    pub fn snapshot(&self) -> Result<BodyUpdate, BodyError> {
        self.body.snapshot()
    }

    /// A body rebuilt from a [`BodyState::snapshot`]; empty bytes give an
    /// empty body.
    pub fn from_snapshot(bytes: &[u8]) -> Result<Self, BodyError> {
        let mut state = Self::new();
        if !bytes.is_empty() {
            state.apply(&BodyUpdate::from_bytes(bytes.to_vec()))?;
        }
        Ok(state)
    }
}

impl Default for BodyState {
    fn default() -> Self {
        Self::new()
    }
}

impl Clone for BodyState {
    fn clone(&self) -> Self {
        // Exporting an in-memory doc and importing a snapshot the same Loro
        // build just produced cannot fail short of an allocation failure.
        let snap = self.snapshot().expect("snapshot of an in-memory body");
        Self::from_snapshot(snap.as_bytes()).expect("restore of a fresh snapshot")
    }
}

impl PartialEq for BodyState {
    fn eq(&self, other: &Self) -> bool {
        self.text() == other.text()
    }
}

impl Eq for BodyState {}

impl Serialize for BodyState {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let snap = self.snapshot().map_err(serde::ser::Error::custom)?;
        crate::bytes::base64::serialize(snap.as_bytes(), serializer)
    }
}

impl<'de> Deserialize<'de> for BodyState {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let bytes = crate::bytes::base64::deserialize(deserializer)?;
        Self::from_snapshot(&bytes).map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ApplyError {
    #[error("op {op_id} targets entity {entity}, not ticket {ticket}")]
    EntityMismatch {
        op_id: Ulid,
        entity: Ulid,
        ticket: Ulid,
    },
    #[error("op {op_id}: relation {relation:?} does not touch ticket {ticket}")]
    RelationNotOnTicket {
        op_id: Ulid,
        relation: Relation,
        ticket: Ulid,
    },
    /// The `body.edit` bytes were not a Loro update. The view is unchanged.
    #[error("op {op_id}: {source}")]
    BodyImport {
        op_id: Ulid,
        #[source]
        source: BodyError,
    },
}

/// Why the authority refused a `claim`.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ClaimRejected {
    #[error("ticket is in state '{state}', which is not unstarted")]
    NotUnstarted { state: String },
    #[error("ticket is already assigned to {assignee}")]
    AlreadyAssigned { assignee: ActorId },
    #[error("ticket is deleted")]
    Deleted,
}

impl TicketView {
    /// An empty view. Any op for `id` can be applied first — including a
    /// `field.set` that synced ahead of the `ticket.create`.
    pub fn new(id: Ulid) -> Self {
        TicketView {
            id,
            created: None,
            updated: None,
            number: Lww::default(),
            title: Lww::default(),
            state: Lww::default(),
            priority: Lww::default(),
            project: Lww::default(),
            repo: Lww::default(),
            assignee: Lww::default(),
            linked_github: Lww::default(),
            linked_pr: Lww::default(),
            linear: Lww::default(),
            source: Lww::default(),
            hold: Lww::default(),
            waivers: Lww::default(),
            not_before: Lww::default(),
            parked: Lww::default(),
            archived_at: Lww::default(),
            ext: BTreeMap::new(),
            deleted_at: None,
            labels: OrSet::default(),
            relations: OrSet::default(),
            comments: CommentLog::default(),
            body: BodyState::new(),
        }
    }

    /// The conditional check behind `claim` (README: "claim if
    /// state==<unstarted> and unassigned"). The authority runs it inside
    /// its transaction before admitting the op; replicas never re-check.
    pub fn claim_admissible(&self, states: &[State]) -> Result<(), ClaimRejected> {
        if self.deleted_at.is_some() {
            return Err(ClaimRejected::Deleted);
        }
        let unstarted = states
            .iter()
            .any(|s| s.name == self.state.value && s.category == StateCategory::Unstarted);
        if !unstarted {
            return Err(ClaimRejected::NotUnstarted {
                state: self.state.value.clone(),
            });
        }
        if let Some(assignee) = &self.assignee.value {
            return Err(ClaimRejected::AlreadyAssigned {
                assignee: assignee.clone(),
            });
        }
        Ok(())
    }

    /// The plain entity. `created`/`updated` fall back to [`Hlc::ZERO`] on
    /// a view that has seen no ops.
    pub fn snapshot(&self) -> Ticket {
        Ticket {
            id: self.id,
            number: self.number.value,
            title: self.title.value.clone(),
            state: self.state.value.clone(),
            priority: self.priority.value,
            project: self.project.value.clone(),
            repo: self.repo.value.clone(),
            assignee: self.assignee.value.clone(),
            description: self.body.text(),
            labels: self.labels.iter().cloned().collect(),
            created: self.created.as_ref().map_or(Hlc::ZERO, |s| s.hlc),
            updated: self.updated.as_ref().map_or(Hlc::ZERO, |s| s.hlc),
            archived_at: self.archived_at.value,
            deleted: self.deleted_at.is_some(),
            linked_github: self.linked_github.value.clone(),
            linked_pr: self.linked_pr.value.clone(),
            linear: self.linear.value.clone(),
            source: self.source.value.clone(),
            hold: self.hold.value.clone(),
            waivers: self.waivers.value.clone(),
            not_before: self.not_before.value.clone(),
            parked: self.parked.value.clone(),
            ext: self
                .ext
                .iter()
                .filter_map(|(k, reg)| reg.value.clone().map(|v| (k.clone(), v)))
                .collect(),
        }
    }
}

/// Fold `op` into `view`. Pure and idempotent: no clock, no IO, and
/// applying the same op again leaves `view` unchanged.
pub fn apply(view: &mut TicketView, op: &Op) -> Result<(), ApplyError> {
    if op.entity != view.id {
        return Err(ApplyError::EntityMismatch {
            op_id: op.op_id,
            entity: op.entity,
            ticket: view.id,
        });
    }
    let stamp = op.stamp();
    match &op.payload {
        Payload::TicketCreate(c) => {
            if view.created.as_ref().is_none_or(|s| stamp < *s) {
                view.created = Some(stamp.clone());
            }
            view.title.set(c.title.clone(), stamp.clone());
            view.state.set(c.state.clone(), stamp.clone());
            view.priority.set(c.priority, stamp.clone());
            view.project.set(c.project.clone(), stamp.clone());
            view.repo.set(c.repo.clone(), stamp.clone());
            view.source.set(c.source.clone(), stamp.clone());
            for (key, value) in &c.ext {
                view.ext
                    .entry(key.clone())
                    .or_default()
                    .set(Some(value.clone()), stamp.clone());
            }
        }
        Payload::FieldSet(field) => match field {
            FieldSet::Title(v) => {
                view.title.set(v.clone(), stamp.clone());
            }
            FieldSet::Priority(v) => {
                view.priority.set(*v, stamp.clone());
            }
            FieldSet::Project(v) => {
                view.project.set(v.clone(), stamp.clone());
            }
            FieldSet::Repo(v) => {
                view.repo.set(v.clone(), stamp.clone());
            }
            FieldSet::Assignee(v) => {
                view.assignee.set(v.clone(), stamp.clone());
            }
            FieldSet::LinkedGithub(v) => {
                view.linked_github.set(v.clone(), stamp.clone());
            }
            FieldSet::LinkedPr(v) => {
                view.linked_pr.set(v.clone(), stamp.clone());
            }
            FieldSet::Linear(v) => {
                view.linear.set(v.clone(), stamp.clone());
            }
            FieldSet::Source(v) => {
                view.source.set(v.clone(), stamp.clone());
            }
            FieldSet::Waivers(v) => {
                view.waivers.set(v.clone(), stamp.clone());
            }
            FieldSet::NotBefore(v) => {
                view.not_before.set(v.clone(), stamp.clone());
            }
            FieldSet::Parked(v) => {
                view.parked.set(v.clone(), stamp.clone());
            }
            FieldSet::ArchivedAt(v) => {
                view.archived_at.set(*v, stamp.clone());
            }
            FieldSet::Number(v) => {
                view.number.set(Some(*v), stamp.clone());
            }
            FieldSet::Ext { key, value } => {
                view.ext
                    .entry(key.clone())
                    .or_default()
                    .set(value.clone(), stamp.clone());
            }
        },
        Payload::LabelAdd(l) => view.labels.add(l.label.clone(), op.op_id),
        Payload::LabelRemove(l) => view.labels.remove(&l.label, &l.observed),
        Payload::RelationAdd(r) => {
            relation_on_ticket(view, op, r.relation)?;
            view.relations.add(r.relation, op.op_id);
        }
        Payload::RelationRemove(r) => {
            relation_on_ticket(view, op, r.relation)?;
            view.relations.remove(&r.relation, &r.observed);
        }
        Payload::CommentAdd(c) => {
            view.comments.push(Comment {
                id: op.op_id,
                ticket: view.id,
                author: op.actor.clone(),
                hlc: op.hlc,
                body: c.body.clone(),
            });
        }
        Payload::StateTransition(t) => {
            view.state.set(t.state.clone(), stamp.clone());
        }
        Payload::Claim(c) => {
            view.state.set(c.state.clone(), stamp.clone());
            view.assignee.set(Some(c.assignee.clone()), stamp.clone());
        }
        Payload::HoldSet(h) => {
            view.hold.set(Some(h.hold.clone()), stamp.clone());
        }
        Payload::HoldClear => {
            view.hold.set(None, stamp.clone());
        }
        Payload::BodyEdit(b) => {
            view.body
                .apply(&BodyUpdate::from_bytes(b.update.clone()))
                .map_err(|source| ApplyError::BodyImport {
                    op_id: op.op_id,
                    source,
                })?;
        }
        Payload::Tombstone => {
            if view.deleted_at.as_ref().is_none_or(|s| stamp < *s) {
                view.deleted_at = Some(stamp.clone());
            }
        }
    }
    if view.updated.as_ref().is_none_or(|s| stamp > *s) {
        view.updated = Some(stamp);
    }
    Ok(())
}

fn relation_on_ticket(view: &TicketView, op: &Op, relation: Relation) -> Result<(), ApplyError> {
    if relation.touches(view.id) {
        Ok(())
    } else {
        Err(ApplyError::RelationNotOnTicket {
            op_id: op.op_id,
            relation,
            ticket: view.id,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::RelationKind;
    use crate::op::{
        BodyEdit, Claim, CommentAdd, HoldSet, LabelAdd, LabelRemove, RelationAdd, RelationRemove,
        StateTransition, TicketCreate,
    };

    fn states() -> Vec<State> {
        vec![
            State {
                name: "triage".into(),
                category: StateCategory::Unstarted,
                position: 0,
            },
            State {
                name: "in-progress".into(),
                category: StateCategory::Started,
                position: 1,
            },
        ]
    }

    fn op(ticket: Ulid, wall_ms: u64, actor: &str, payload: Payload) -> Op {
        Op::new(
            Ulid::new(),
            Hlc::new(wall_ms, 0),
            ActorId::new(actor),
            ticket,
            payload,
        )
    }

    /// A real Loro update: `text` typed into a fresh replica owned by `peer`.
    fn body_update(peer: u64, text: &str) -> Vec<u8> {
        Body::with_peer(peer)
            .unwrap()
            .diff_from_text(text)
            .unwrap()
            .into_bytes()
    }

    fn body_edit(ticket: Ulid, wall_ms: u64, actor: &str, update: Vec<u8>) -> Op {
        op(
            ticket,
            wall_ms,
            actor,
            Payload::BodyEdit(BodyEdit { update }),
        )
    }

    fn create(ticket: Ulid, wall_ms: u64) -> Op {
        op(
            ticket,
            wall_ms,
            "matt",
            Payload::TicketCreate(TicketCreate {
                title: "original".into(),
                state: "triage".into(),
                priority: Priority::Medium,
                project: Some("pm".into()),
                repo: None,
                source: None,
                ext: [("legacy".to_string(), Value::from("x"))].into(),
            }),
        )
    }

    /// Every op kind once, in a plausible sequence.
    fn full_history(ticket: Ulid, other: Ulid) -> Vec<Op> {
        let relation = Relation {
            kind: RelationKind::Blocks,
            from: other,
            to: ticket,
        };
        let label_add = op(
            ticket,
            2,
            "matt",
            Payload::LabelAdd(LabelAdd { label: "x".into() }),
        );
        let rel_add = op(
            ticket,
            3,
            "matt",
            Payload::RelationAdd(RelationAdd { relation }),
        );
        vec![
            create(ticket, 1),
            label_add.clone(),
            op(
                ticket,
                2,
                "matt",
                Payload::LabelAdd(LabelAdd {
                    label: "keep".into(),
                }),
            ),
            rel_add.clone(),
            op(
                ticket,
                4,
                "matt",
                Payload::FieldSet(FieldSet::Title("renamed".into())),
            ),
            op(
                ticket,
                4,
                "matt",
                Payload::FieldSet(FieldSet::Ext {
                    key: "legacy".into(),
                    value: None,
                }),
            ),
            op(
                ticket,
                5,
                "matt",
                Payload::CommentAdd(CommentAdd { body: "c1".into() }),
            ),
            op(
                ticket,
                6,
                "claude:pm-build",
                Payload::Claim(Claim {
                    state: "in-progress".into(),
                    assignee: ActorId::new("claude:pm-build"),
                }),
            ),
            op(
                ticket,
                7,
                "matt",
                Payload::HoldSet(HoldSet {
                    hold: Hold {
                        reason: "needs Matt".into(),
                        by: ActorId::new("matt"),
                        at: Hlc::new(7, 0),
                    },
                }),
            ),
            op(ticket, 8, "matt", Payload::HoldClear),
            body_edit(ticket, 9, "matt", body_update(1, "# original\n\nBody.\n")),
            op(
                ticket,
                10,
                "matt",
                Payload::LabelRemove(LabelRemove {
                    label: "x".into(),
                    observed: vec![label_add.op_id],
                }),
            ),
            op(
                ticket,
                11,
                "matt",
                Payload::RelationRemove(RelationRemove {
                    relation,
                    observed: vec![rel_add.op_id],
                }),
            ),
            op(
                ticket,
                12,
                "matt",
                Payload::StateTransition(StateTransition {
                    state: "done".into(),
                }),
            ),
            op(
                ticket,
                12,
                "matt",
                Payload::FieldSet(FieldSet::Number(1334)),
            ),
            op(ticket, 13, "matt", Payload::Tombstone),
        ]
    }

    #[test]
    fn applying_every_op_twice_is_idempotent() {
        let (ticket, other) = (Ulid::new(), Ulid::new());
        let mut once = TicketView::new(ticket);
        let mut twice = TicketView::new(ticket);
        for op in full_history(ticket, other) {
            apply(&mut once, &op).unwrap();
            apply(&mut twice, &op).unwrap();
            apply(&mut twice, &op).unwrap();
            assert_eq!(once, twice, "after replaying {}", op.kind());
        }
        let t = once.snapshot();
        assert_eq!(t.title, "renamed");
        assert_eq!(t.state, "done");
        assert_eq!(t.number, Some(1334));
        assert_eq!(t.labels.iter().collect::<Vec<_>>(), ["keep"]);
        assert!(t.deleted);
        assert!(t.hold.is_none());
        assert!(t.ext.is_empty(), "ext key deleted by field.set");
        assert_eq!(t.created, Hlc::new(1, 0));
        assert_eq!(t.updated, Hlc::new(13, 0));
        assert_eq!(once.comments.len(), 1);
        assert!(once.relations.is_empty());
        assert_eq!(t.description, "# original\n\nBody.\n");
        assert_eq!(t.description, once.body.text());
    }

    #[test]
    fn body_edit_applied_twice_is_idempotent() {
        let ticket = Ulid::new();
        let edit = body_edit(ticket, 2, "matt", body_update(1, "once"));
        let mut view = TicketView::new(ticket);
        apply(&mut view, &edit).unwrap();
        let after_first = view.clone();
        apply(&mut view, &edit).unwrap();
        assert_eq!(view, after_first);
        assert_eq!(view.snapshot().description, "once");
    }

    #[test]
    fn concurrent_body_edits_converge_in_either_order() {
        let ticket = Ulid::new();
        // Two replicas start from a shared base and edit different spans
        // offline; the view sees the three resulting ops.
        let mut a = Body::with_peer(1).unwrap();
        let base = a.diff_from_text("alpha\nbeta\n").unwrap();
        let mut b = Body::with_peer(2).unwrap();
        b.apply(&base).unwrap();
        let from_a = a.diff_from_text("ALPHA\nbeta\n").unwrap();
        let from_b = b.diff_from_text("alpha\nbeta\ngamma\n").unwrap();

        let base = body_edit(ticket, 1, "matt", base.into_bytes());
        let from_a = body_edit(ticket, 2, "matt", from_a.into_bytes());
        let from_b = body_edit(ticket, 2, "claude:pm-build", from_b.into_bytes());

        let mut texts = Vec::new();
        for order in [
            [&base, &from_a, &from_b],
            [&base, &from_b, &from_a],
            [&from_b, &from_a, &base],
        ] {
            let mut view = TicketView::new(ticket);
            for o in order {
                apply(&mut view, o).unwrap();
            }
            assert_eq!(view.snapshot().description, view.body.text());
            texts.push(view.body.text());
        }
        assert!(texts.iter().all(|t| t == &texts[0]), "{texts:?}");
        assert_eq!(texts[0], "ALPHA\nbeta\ngamma\n");
    }

    #[test]
    fn body_edit_with_garbage_bytes_is_rejected_and_leaves_the_view_untouched() {
        let ticket = Ulid::new();
        let mut view = TicketView::new(ticket);
        apply(&mut view, &create(ticket, 1)).unwrap();
        let before = view.clone();
        let bad = body_edit(ticket, 2, "matt", vec![9, 9]);
        assert!(matches!(
            apply(&mut view, &bad),
            Err(ApplyError::BodyImport { op_id, .. }) if op_id == bad.op_id
        ));
        assert_eq!(view, before);
    }

    #[test]
    fn body_survives_serde_and_keeps_merging() {
        let ticket = Ulid::new();
        let mut author = Body::with_peer(1).unwrap();
        let first = author.diff_from_text("v1").unwrap();
        let second = author.diff_from_text("v1 v2").unwrap();

        let mut view = TicketView::new(ticket);
        apply(&mut view, &create(ticket, 1)).unwrap();
        apply(&mut view, &body_edit(ticket, 2, "matt", first.into_bytes())).unwrap();

        let json = serde_json::to_string(&view).unwrap();
        let mut back: TicketView = serde_json::from_str(&json).unwrap();
        assert_eq!(back, view);
        assert_eq!(back.body.text(), "v1");

        // The snapshot carried the history, so the next update still lands.
        apply(
            &mut back,
            &body_edit(ticket, 3, "matt", second.into_bytes()),
        )
        .unwrap();
        assert_eq!(back.snapshot().description, "v1 v2");
    }

    /// AGT-1378: the view row persists its snapshot as base64, and a row
    /// written before that (a JSON array of bytes) still loads.
    #[test]
    fn body_serializes_as_base64_and_reads_the_legacy_array() {
        let ticket = Ulid::new();
        let update = Body::with_peer(1)
            .unwrap()
            .diff_from_text("persisted")
            .unwrap();
        let mut view = TicketView::new(ticket);
        apply(&mut view, &create(ticket, 1)).unwrap();
        apply(
            &mut view,
            &body_edit(ticket, 2, "matt", update.into_bytes()),
        )
        .unwrap();

        let mut json = serde_json::to_value(&view).unwrap();
        let encoded = json["body"]
            .as_str()
            .expect("body is a base64 string")
            .to_string();
        let snapshot = crate::bytes::decode(&encoded).unwrap();
        assert_eq!(snapshot, view.body.snapshot().unwrap().into_bytes());

        json["body"] = Value::Array(snapshot.into_iter().map(Value::from).collect());
        let legacy: TicketView = serde_json::from_value(json).unwrap();
        assert_eq!(legacy, view);
        assert_eq!(legacy.body.text(), "persisted");
    }

    #[test]
    fn replaying_the_log_in_any_order_converges() {
        let (ticket, other) = (Ulid::new(), Ulid::new());
        let history = full_history(ticket, other);
        let mut forward = TicketView::new(ticket);
        for op in &history {
            apply(&mut forward, op).unwrap();
        }
        let mut reverse = TicketView::new(ticket);
        for op in history.iter().rev() {
            apply(&mut reverse, op).unwrap();
        }
        assert_eq!(forward, reverse);
    }

    #[test]
    fn concurrent_field_sets_resolve_by_hlc_then_actor() {
        let ticket = Ulid::new();
        let base = create(ticket, 1);
        let alice = op(
            ticket,
            5,
            "alice",
            Payload::FieldSet(FieldSet::Title("alice".into())),
        );
        let bob = op(
            ticket,
            5,
            "bob",
            Payload::FieldSet(FieldSet::Title("bob".into())),
        );
        let later = op(
            ticket,
            6,
            "aaron",
            Payload::FieldSet(FieldSet::Priority(Priority::High)),
        );
        for order in [[&alice, &bob, &later], [&later, &bob, &alice]] {
            let mut view = TicketView::new(ticket);
            apply(&mut view, &base).unwrap();
            for o in order {
                apply(&mut view, o).unwrap();
            }
            assert_eq!(view.title.value, "bob");
            assert_eq!(view.priority.value, Priority::High);
        }
    }

    #[test]
    fn concurrent_label_remove_and_re_add_keeps_the_label() {
        let ticket = Ulid::new();
        let first_add = op(
            ticket,
            2,
            "matt",
            Payload::LabelAdd(LabelAdd { label: "x".into() }),
        );
        let remove = op(
            ticket,
            3,
            "matt",
            Payload::LabelRemove(LabelRemove {
                label: "x".into(),
                observed: vec![first_add.op_id],
            }),
        );
        let re_add = op(
            ticket,
            3,
            "claude:pm-build",
            Payload::LabelAdd(LabelAdd { label: "x".into() }),
        );
        for order in [
            [&first_add, &remove, &re_add],
            [&first_add, &re_add, &remove],
        ] {
            let mut view = TicketView::new(ticket);
            for o in order {
                apply(&mut view, o).unwrap();
            }
            assert!(view.labels.contains(&"x".to_string()));
            assert_eq!(view.labels.observed(&"x".to_string()), vec![re_add.op_id]);
        }
    }

    #[test]
    fn field_set_that_syncs_before_create_still_wins() {
        let ticket = Ulid::new();
        let rename = op(
            ticket,
            9,
            "matt",
            Payload::FieldSet(FieldSet::Title("late".into())),
        );
        let mut view = TicketView::new(ticket);
        apply(&mut view, &rename).unwrap();
        apply(&mut view, &create(ticket, 1)).unwrap();
        assert_eq!(view.title.value, "late");
        assert_eq!(view.created, Some(create(ticket, 1).stamp()));
        assert_eq!(view.snapshot().created, Hlc::new(1, 0));
    }

    #[test]
    fn tombstone_is_permanent() {
        let ticket = Ulid::new();
        let mut view = TicketView::new(ticket);
        apply(&mut view, &create(ticket, 1)).unwrap();
        apply(&mut view, &op(ticket, 5, "matt", Payload::Tombstone)).unwrap();
        let first = view.deleted_at.clone();
        apply(&mut view, &op(ticket, 3, "zed", Payload::Tombstone)).unwrap();
        assert_eq!(
            view.deleted_at.as_ref().unwrap().hlc,
            Hlc::new(3, 0),
            "earliest tombstone stamp is kept"
        );
        assert_ne!(view.deleted_at, first);
        assert!(view.snapshot().deleted);
    }

    #[test]
    fn claim_admissible_only_when_unstarted_and_unassigned() {
        let ticket = Ulid::new();
        let mut view = TicketView::new(ticket);
        apply(&mut view, &create(ticket, 1)).unwrap();
        assert_eq!(view.claim_admissible(&states()), Ok(()));

        let claim = op(
            ticket,
            2,
            "claude:a",
            Payload::Claim(Claim {
                state: "in-progress".into(),
                assignee: ActorId::new("claude:a"),
            }),
        );
        apply(&mut view, &claim).unwrap();
        assert_eq!(
            view.claim_admissible(&states()),
            Err(ClaimRejected::NotUnstarted {
                state: "in-progress".into()
            })
        );

        let mut assigned = TicketView::new(ticket);
        apply(&mut assigned, &create(ticket, 1)).unwrap();
        apply(
            &mut assigned,
            &op(
                ticket,
                2,
                "matt",
                Payload::FieldSet(FieldSet::Assignee(Some(ActorId::new("matt")))),
            ),
        )
        .unwrap();
        assert_eq!(
            assigned.claim_admissible(&states()),
            Err(ClaimRejected::AlreadyAssigned {
                assignee: ActorId::new("matt")
            })
        );

        apply(&mut assigned, &op(ticket, 3, "matt", Payload::Tombstone)).unwrap();
        assert_eq!(
            assigned.claim_admissible(&states()),
            Err(ClaimRejected::Deleted)
        );
    }

    #[test]
    fn ops_for_another_entity_or_foreign_relations_are_rejected() {
        let (ticket, a, b) = (Ulid::new(), Ulid::new(), Ulid::new());
        let mut view = TicketView::new(ticket);
        let foreign = create(a, 1);
        assert!(matches!(
            apply(&mut view, &foreign),
            Err(ApplyError::EntityMismatch { .. })
        ));
        let unrelated = op(
            ticket,
            2,
            "matt",
            Payload::RelationAdd(RelationAdd {
                relation: Relation {
                    kind: RelationKind::Parent,
                    from: a,
                    to: b,
                },
            }),
        );
        assert!(matches!(
            apply(&mut view, &unrelated),
            Err(ApplyError::RelationNotOnTicket { .. })
        ));
        assert_eq!(
            view,
            TicketView::new(ticket),
            "rejected ops leave the view untouched"
        );
    }

    #[test]
    fn view_round_trips_through_serde() {
        let (ticket, other) = (Ulid::new(), Ulid::new());
        let mut view = TicketView::new(ticket);
        for op in full_history(ticket, other).iter().take(8) {
            apply(&mut view, op).unwrap();
        }
        let json = serde_json::to_string(&view).unwrap();
        let back: TicketView = serde_json::from_str(&json).unwrap();
        assert_eq!(back, view);
    }
}
