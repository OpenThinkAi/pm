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

use crate::domain::{
    ActorId, ActorKind, Hold, NotBefore, Parked, Priority, ProjectStatus, Relation, Source,
    StateCategory, Waiver,
};
use crate::hlc::{Hlc, Stamp};

/// Schema version written into every op this build produces. Bump when a
/// payload shape changes incompatibly; readers branch on `op.version`.
///
/// Adding a *kind* is not a bump: every op already in a log keeps its
/// shape, and a reader dispatches on `kind`, not `version` (the config
/// kinds of AGT-1384 landed this way).
pub const OP_VERSION: u16 = 1;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Op {
    pub op_id: Ulid,
    pub hlc: Hlc,
    pub actor: ActorId,
    /// The ticket (or, for `ticket.create`, the new ticket's id) this op
    /// mutates — or, for a `body.edit` targeting a project document
    /// (AGT-1344), that document's `doc_id`, a Ulid bound to the project
    /// by its `project.create` or a `project.doc_add` (AGT-1413). Never a
    /// project's own kebab-case `id`.
    ///
    /// Config ops (AGT-1384) target the workspace's id for `workspace.set`,
    /// `state.upsert` and `actor.upsert` — states and actors are name-keyed
    /// children of the workspace, the way `ext` keys are of a ticket — and
    /// a per-project Ulid for `project.create` / `project.set`, with the
    /// kebab-case id carried in the create payload the way a ticket's human
    /// number is separate from its Ulid.
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
    // Config kinds (AGT-1384, README §Sync & hub "Config must become ops",
    // decision A4). Folded by `crate::config`, never by a ticket view.
    #[serde(rename = "workspace.set")]
    WorkspaceSet(WorkspaceSet),
    #[serde(rename = "state.upsert")]
    StateUpsert(StateUpsert),
    #[serde(rename = "actor.upsert")]
    ActorUpsert(ActorUpsert),
    #[serde(rename = "project.create")]
    ProjectCreate(ProjectCreate),
    #[serde(rename = "project.set")]
    ProjectSet(ProjectSet),
    /// Tombstones the project (`pm project delete`, AGT-1386): permanent,
    /// like a ticket's [`Payload::Tombstone`]; a later `project.set` or
    /// `project.create` for the same entity folds into the view but never
    /// brings the project back. Re-using the slug takes a fresh
    /// `project.create` under a new project Ulid.
    #[serde(rename = "project.delete")]
    ProjectDelete,
    /// Names one of the project's documents (AGT-1413): binds a `doc_id`
    /// — the entity its `body.edit` ops target — to the design doc or to a
    /// named document, so a replica that pulls the project also learns
    /// where its document edits go.
    #[serde(rename = "project.doc_add")]
    ProjectDocAdd(ProjectDocAdd),
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
            Payload::WorkspaceSet(_) => "workspace.set",
            Payload::StateUpsert(_) => "state.upsert",
            Payload::ActorUpsert(_) => "actor.upsert",
            Payload::ProjectCreate(_) => "project.create",
            Payload::ProjectSet(_) => "project.set",
            Payload::ProjectDelete => "project.delete",
            Payload::ProjectDocAdd(_) => "project.doc_add",
        }
    }

    /// Whether this kind mutates workspace config (the workspace, its
    /// states, its actors) or a project — anything but a ticket or a
    /// document. `entity` is then the workspace or project id, and only
    /// [`crate::config`] folds it.
    pub fn is_config(&self) -> bool {
        matches!(
            self,
            Payload::WorkspaceSet(_)
                | Payload::StateUpsert(_)
                | Payload::ActorUpsert(_)
                | Payload::ProjectCreate(_)
                | Payload::ProjectSet(_)
                | Payload::ProjectDelete
                | Payload::ProjectDocAdd(_)
        )
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

/// One workspace-config write (`entity` = the workspace id). Scalars are
/// LWW registers; the gate-label set is an OR-set, so its remove cites
/// the add-tags it observed exactly like [`LabelRemove`]; `model_labels`
/// is one register per label, like a ticket's `ext` keys. Serialized as
/// `{"field": "<name>", "value": …}`, the [`FieldSet`] shape.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "field", content = "value", rename_all = "snake_case")]
pub enum WorkspaceSet {
    Prefix(String),
    GateLabelAdd(String),
    GateLabelRemove {
        label: String,
        observed: Vec<Ulid>,
    },
    /// `model_labels[label] = model`; `None` deletes the key.
    ModelLabel {
        label: String,
        model: Option<String>,
    },
    /// The whole ordered list: sections are an ordered template, not a set.
    TemplateSections(Vec<String>),
    StaleDays(u32),
}

/// Insert or replace one workflow state, keyed by `name` (`entity` = the
/// workspace id). The record is one LWW register: the later upsert wins
/// whole, since `category` and `position` are never written separately.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StateUpsert {
    pub name: String,
    pub category: StateCategory,
    pub position: u32,
}

/// Insert or replace one actor, keyed by `id` (`entity` = the workspace
/// id); `kind` is an LWW register.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActorUpsert {
    pub id: ActorId,
    pub kind: ActorKind,
}

/// A project's initial scalars; `entity` is the project's own Ulid and
/// `id` its kebab-case human id (`pm`). Repos follow as `project.set
/// repo_add` ops, the way labels follow a `ticket.create`; named
/// documents as `project.doc_add` ops.
///
/// `doc_id` (AGT-1413) is the design doc's identity, minted with the
/// project so a replica that pulls the create knows where the design
/// doc's `body.edit` ops go. Absent on a create logged before that ticket
/// (the key is omitted, and an old op still parses); such a project gets
/// its design doc from a `project.doc_add` without a `name` instead.
///
/// Two replicas creating the same `id` offline mint two Ulids; the
/// authority (the hub, from P3) arbitrates that the way it does ticket
/// numbers — this crate merges, it does not allocate.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectCreate {
    pub id: String,
    pub title: String,
    pub status: ProjectStatus,
    pub parent: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub doc_id: Option<Ulid>,
}

/// Binds `doc_id` to one of the project's documents (`entity` = the
/// project's Ulid): the design doc when `name` is absent, else the named
/// document `name`. Each document slot keeps the *earliest* binding it has
/// seen (by stamp, then `doc_id`) — a document's identity is fixed once
/// made, and two replicas that add the same name offline converge on one
/// of the two ids ([`crate::config::DocClaims`]).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectDocAdd {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub doc_id: Ulid,
}

/// One project-metadata write; scalars are LWW registers and `repos` an
/// OR-set (remove cites observed add-tags, as [`LabelRemove`]).
/// Serialized as `{"field": "<name>", "value": …}`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "field", content = "value", rename_all = "snake_case")]
pub enum ProjectSet {
    Title(String),
    Status(ProjectStatus),
    Parent(Option<String>),
    RepoAdd(String),
    RepoRemove { repo: String, observed: Vec<Ulid> },
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
            op(Payload::WorkspaceSet(WorkspaceSet::Prefix("AGT".into()))),
            op(Payload::WorkspaceSet(WorkspaceSet::GateLabelRemove {
                label: "manual".into(),
                observed: vec![Ulid::new()],
            })),
            op(Payload::WorkspaceSet(WorkspaceSet::ModelLabel {
                label: "model:fable-5".into(),
                model: None,
            })),
            op(Payload::StateUpsert(StateUpsert {
                name: "triage".into(),
                category: StateCategory::Unstarted,
                position: 0,
            })),
            op(Payload::ActorUpsert(ActorUpsert {
                id: ActorId::new("claude:pm-build"),
                kind: ActorKind::Agent,
            })),
            op(Payload::ProjectCreate(ProjectCreate {
                id: "pm".into(),
                title: "pm".into(),
                status: ProjectStatus::InProgress,
                parent: None,
                doc_id: Some(Ulid::new()),
            })),
            op(Payload::ProjectSet(ProjectSet::RepoRemove {
                repo: "OpenThinkAi/pm".into(),
                observed: vec![],
            })),
            op(Payload::ProjectDocAdd(ProjectDocAdd {
                name: Some("notes".into()),
                doc_id: Ulid::new(),
            })),
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
            "workspace.set",
            "workspace.set",
            "workspace.set",
            "state.upsert",
            "actor.upsert",
            "project.create",
            "project.set",
            "project.doc_add",
        ];
        let ops = all_kinds();
        assert_eq!(ops.len(), expected.len());
        for (op, kind) in ops.into_iter().zip(expected) {
            assert_eq!(op.kind(), kind);
            let json = serde_json::to_value(&op).unwrap();
            assert_eq!(json["kind"], kind, "{json}");
            assert_eq!(json["version"], OP_VERSION);
            let back: Op = serde_json::from_value(json).unwrap();
            assert_eq!(back, op);
        }
    }

    /// AGT-1384: the config kinds share `field.set`'s `{field, value}`
    /// payload shape, and only they answer `is_config`.
    #[test]
    fn config_kinds_use_the_field_value_shape() {
        let ops = all_kinds();
        let (tickets, config): (Vec<_>, Vec<_>) = ops.iter().partition(|o| !o.payload.is_config());
        assert_eq!(tickets.len(), 14);
        assert_eq!(config.len(), 8);

        let json =
            serde_json::to_value(op(Payload::WorkspaceSet(WorkspaceSet::StaleDays(30)))).unwrap();
        assert_eq!(
            json["payload"],
            serde_json::json!({"field": "stale_days", "value": 30})
        );
        let json = serde_json::to_value(op(Payload::ProjectSet(ProjectSet::RepoRemove {
            repo: "OpenThinkAi/pm".into(),
            observed: vec![],
        })))
        .unwrap();
        assert_eq!(
            json["payload"],
            serde_json::json!({"field": "repo_remove", "value": {"repo": "OpenThinkAi/pm", "observed": []}})
        );
        let json = serde_json::to_value(op(Payload::ProjectSet(ProjectSet::Status(
            ProjectStatus::Complete,
        ))))
        .unwrap();
        assert_eq!(
            json["payload"],
            serde_json::json!({"field": "status", "value": "complete"}),
            "project status keeps its kebab-case frontmatter spelling"
        );
    }

    /// AGT-1413: a create logged before `doc_id` existed still parses,
    /// and one without it omits the key; a design-doc `project.doc_add`
    /// omits `name`.
    #[test]
    fn doc_identity_fields_are_optional_on_the_wire() {
        let legacy = serde_json::json!({
            "id": "pm", "title": "pm", "status": "in-progress", "parent": null
        });
        let create: ProjectCreate = serde_json::from_value(legacy.clone()).unwrap();
        assert_eq!(create.doc_id, None);
        assert_eq!(serde_json::to_value(&create).unwrap(), legacy);

        let doc_id = Ulid::new();
        let design = ProjectDocAdd { name: None, doc_id };
        assert_eq!(
            serde_json::to_value(&design).unwrap(),
            serde_json::json!({"doc_id": doc_id.to_string()})
        );
        let back: ProjectDocAdd =
            serde_json::from_value(serde_json::json!({"doc_id": doc_id.to_string()})).unwrap();
        assert_eq!(back, design);
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
