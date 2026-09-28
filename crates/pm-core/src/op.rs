//! The op envelope: every mutation in pm is one [`Op`], appended to the log
//! and applied to the materialized tables in the same transaction
//! (README §Op log).
//!
//! JSON shape: `{op_id, hlc, actor, entity, kind, payload, version}`. In
//! Rust the `kind` tag and its `payload` are one enum, [`Payload`], so a
//! kind can never be paired with the wrong payload; `Op::kind()` gives the
//! string the store indexes on.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use ulid::Ulid;

use crate::domain::{ActorId, Hold, NotBefore, Parked, Priority, Relation, Source, Waiver};
use crate::hlc::{Hlc, Stamp};

/// Schema version written into every op this build produces. Bump when a
/// payload shape changes incompatibly; readers branch on `op.version`.
pub const OP_VERSION: u16 = 1;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Op {
    pub op_id: Ulid,
    pub hlc: Hlc,
    pub actor: ActorId,
    /// The ticket (or, for `ticket.create`, the new ticket's id) this op
    /// mutates — or, for a `body.edit` targeting a project document
    /// (AGT-1344), that document's `doc_id`, a Ulid pm-store stamps on the
    /// `project` or `project_doc` row it belongs to. Never a project's own
    /// kebab-case `id`.
    pub entity: Ulid,
    /// Serialized as the sibling keys `kind` and `payload`.
    #[serde(flatten)]
    pub payload: Payload,
    pub version: u16,
}

impl Op {
    /// Build an op at [`OP_VERSION`]. The caller mints `op_id` (a ULID
    /// needs the system clock and randomness, which this crate never
    /// touches) and stamps `hlc` from its [`crate::Clock`].
    pub fn new(op_id: Ulid, hlc: Hlc, actor: ActorId, entity: Ulid, payload: Payload) -> Self {
        Op {
            op_id,
            hlc,
            actor,
            entity,
            payload,
            version: OP_VERSION,
        }
    }

    pub fn kind(&self) -> &'static str {
        self.payload.kind()
    }

    /// The `(hlc, actor)` key every merge rule orders by.
    pub fn stamp(&self) -> Stamp {
        Stamp::new(self.hlc, self.actor.clone())
    }
}

/// `kind` + `payload`, one variant per op kind.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "payload")]
pub enum Payload {
    #[serde(rename = "ticket.create")]
    TicketCreate(TicketCreate),
    #[serde(rename = "field.set")]
    FieldSet(FieldSet),
    #[serde(rename = "label.add")]
    LabelAdd(LabelAdd),
    #[serde(rename = "label.remove")]
    LabelRemove(LabelRemove),
    #[serde(rename = "relation.add")]
    RelationAdd(RelationAdd),
    #[serde(rename = "relation.remove")]
    RelationRemove(RelationRemove),
    #[serde(rename = "comment.add")]
    CommentAdd(CommentAdd),
    #[serde(rename = "state.transition")]
    StateTransition(StateTransition),
    #[serde(rename = "claim")]
    Claim(Claim),
    #[serde(rename = "hold.set")]
    HoldSet(HoldSet),
    #[serde(rename = "hold.clear")]
    HoldClear,
    #[serde(rename = "body.edit")]
    BodyEdit(BodyEdit),
    #[serde(rename = "tombstone")]
    Tombstone,
}

impl Payload {
    pub fn kind(&self) -> &'static str {
        match self {
            Payload::TicketCreate(_) => "ticket.create",
            Payload::FieldSet(_) => "field.set",
            Payload::LabelAdd(_) => "label.add",
            Payload::LabelRemove(_) => "label.remove",
            Payload::RelationAdd(_) => "relation.add",
            Payload::RelationRemove(_) => "relation.remove",
            Payload::CommentAdd(_) => "comment.add",
            Payload::StateTransition(_) => "state.transition",
            Payload::Claim(_) => "claim",
            Payload::HoldSet(_) => "hold.set",
            Payload::HoldClear => "hold.clear",
            Payload::BodyEdit(_) => "body.edit",
            Payload::Tombstone => "tombstone",
        }
    }
}

/// Initial scalar fields. Labels, relations, comments and the body follow
/// as their own ops (`pm new` emits `ticket.create` + label ops).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TicketCreate {
    pub title: String,
    /// Name of the initial workflow state (`triage`).
    pub state: String,
    pub priority: Priority,
    pub project: Option<String>,
    pub repo: Option<String>,
    pub source: Option<Source>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub ext: BTreeMap<String, Value>,
}

/// One scalar-field write; every variant is an LWW register on the ticket.
/// Serialized as `{"field": "<name>", "value": …}`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "field", content = "value", rename_all = "snake_case")]
pub enum FieldSet {
    Title(String),
    Priority(Priority),
    Project(Option<String>),
    Repo(Option<String>),
    Assignee(Option<ActorId>),
    LinkedGithub(Option<String>),
    LinkedPr(Option<String>),
    Linear(Option<String>),
    Source(Option<Source>),
    Waivers(Vec<Waiver>),
    NotBefore(Option<NotBefore>),
    Parked(Option<Parked>),
    ArchivedAt(Option<Hlc>),
    /// Human number; only the authority issues this (README §Conflict
    /// semantics: numbers are allocated, never merged).
    Number(u64),
    /// One key of the `ext` map; `None` deletes the key.
    Ext {
        key: String,
        value: Option<Value>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LabelAdd {
    pub label: String,
}

/// Remove a label. `observed` lists the add-tags (op ids) the remover saw,
/// so a concurrent re-add survives (OR-set, add-wins). Read them off the
/// view with [`crate::merge::OrSet::observed`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LabelRemove {
    pub label: String,
    pub observed: Vec<Ulid>,
}

/// Add a relation; `relation` must have the op's `entity` as one endpoint.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RelationAdd {
    pub relation: Relation,
}

/// Remove a relation; `observed` as in [`LabelRemove`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RelationRemove {
    pub relation: Relation,
    pub observed: Vec<Ulid>,
}

/// Append a comment. The comment's id, author and hlc are the op's own.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommentAdd {
    pub body: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StateTransition {
    pub state: String,
}

/// `unstarted → started` with an assignee. **Not a CRDT**: the authority
/// admits it only if the ticket is still unstarted and unassigned (see
/// [`crate::view::TicketView::claim_admissible`]); replicas apply admitted
/// claims as plain LWW writes of `state` and `assignee`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Claim {
    pub state: String,
    pub assignee: ActorId,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HoldSet {
    pub hold: Hold,
}

/// One text-CRDT update: the bytes of a [`crate::BodyUpdate`] (a Loro
/// update or snapshot, AGT-1338). Opaque to the op log and to pm-hub; the
/// view folds it in with [`crate::Body::apply`].
///
/// On the wire `update` is a base64 string (AGT-1378,
/// [`crate::bytes::base64`]); an op written before that as an array of
/// integers still parses. Op identity is `op_id`, never a hash of this
/// JSON, so the two spellings are the same op.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BodyEdit {
    #[serde(with = "crate::bytes::base64")]
    pub update: Vec<u8>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::RelationKind;

    fn op(payload: Payload) -> Op {
        Op::new(
            Ulid::new(),
            Hlc::new(5, 1),
            ActorId::new("matt"),
            Ulid::new(),
            payload,
        )
    }

    fn all_kinds() -> Vec<Op> {
        let other = Ulid::new();
        let ticket = Ulid::new();
        let relation = Relation {
            kind: RelationKind::Blocks,
            from: ticket,
            to: other,
        };
        vec![
            op(Payload::TicketCreate(TicketCreate {
                title: "t".into(),
                state: "triage".into(),
                priority: Priority::Medium,
                project: Some("pm".into()),
                repo: None,
                source: None,
                ext: [("x".to_string(), Value::Bool(true))].into(),
            })),
            op(Payload::FieldSet(FieldSet::Title("new".into()))),
            op(Payload::FieldSet(FieldSet::Ext {
                key: "k".into(),
                value: None,
            })),
            op(Payload::LabelAdd(LabelAdd { label: "l".into() })),
            op(Payload::LabelRemove(LabelRemove {
                label: "l".into(),
                observed: vec![Ulid::new()],
            })),
            op(Payload::RelationAdd(RelationAdd { relation })),
            op(Payload::RelationRemove(RelationRemove {
                relation,
                observed: vec![],
            })),
            op(Payload::CommentAdd(CommentAdd { body: "c".into() })),
            op(Payload::StateTransition(StateTransition {
                state: "done".into(),
            })),
            op(Payload::Claim(Claim {
                state: "in-progress".into(),
                assignee: ActorId::new("matt"),
            })),
            op(Payload::HoldSet(HoldSet {
                hold: Hold {
                    reason: "r".into(),
                    by: ActorId::new("matt"),
                    at: Hlc::new(5, 1),
                },
            })),
            op(Payload::HoldClear),
            op(Payload::BodyEdit(BodyEdit {
                update: vec![1, 2, 3],
            })),
            op(Payload::Tombstone),
        ]
    }

    #[test]
    fn every_kind_round_trips_and_carries_its_kind_string() {
        let expected = [
            "ticket.create",
            "field.set",
            "field.set",
            "label.add",
            "label.remove",
            "relation.add",
            "relation.remove",
            "comment.add",
            "state.transition",
            "claim",
            "hold.set",
            "hold.clear",
            "body.edit",
            "tombstone",
        ];
        for (op, kind) in all_kinds().into_iter().zip(expected) {
            assert_eq!(op.kind(), kind);
            let json = serde_json::to_value(&op).unwrap();
            assert_eq!(json["kind"], kind, "{json}");
            assert_eq!(json["version"], OP_VERSION);
            let back: Op = serde_json::from_value(json).unwrap();
            assert_eq!(back, op);
        }
    }

    #[test]
    fn envelope_has_the_documented_keys() {
        let json = serde_json::to_value(op(Payload::FieldSet(FieldSet::Priority(Priority::High))))
            .unwrap();
        let keys: Vec<&str> = json
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(
            keys,
            [
                "actor", "entity", "hlc", "kind", "op_id", "payload", "version"
            ]
        );
        assert_eq!(
            json["payload"],
            serde_json::json!({"field": "priority", "value": "high"})
        );
    }

    /// AGT-1378: the update travels as base64, and the array form every op
    /// log and backup carried before that still reads as the same op.
    #[test]
    fn body_edit_update_is_base64_on_the_wire_and_reads_the_legacy_array() {
        let op = op(Payload::BodyEdit(BodyEdit {
            update: b"loro".to_vec(),
        }));
        let json = serde_json::to_value(&op).unwrap();
        assert_eq!(json["payload"], serde_json::json!({"update": "bG9ybw=="}));

        let mut legacy = json.clone();
        legacy["payload"] = serde_json::json!({"update": [108, 111, 114, 111]});
        let back: Op = serde_json::from_value(legacy).unwrap();
        assert_eq!(back, op);
    }

    #[test]
    fn unit_kinds_omit_the_payload_key() {
        let json = serde_json::to_value(op(Payload::Tombstone)).unwrap();
        assert!(json.get("payload").is_none(), "{json}");
        let back: Op = serde_json::from_value(json).unwrap();
        assert_eq!(back.payload, Payload::Tombstone);
    }
}
