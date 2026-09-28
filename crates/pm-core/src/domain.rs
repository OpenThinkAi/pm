//! Entity types, straight from projects/pm/README.md §Data model. These are
//! the plain, serde-round-trippable shapes the CLI prints and `pm-store`
//! materializes; the merge metadata that produces them lives in
//! [`crate::view::TicketView`].
//!
//! Every entity has a ULID identity. A ticket's human number (`AGT-1332`)
//! is display-only and allocated by the authority, so it is `Option<u64>`.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use ulid::Ulid;

use crate::hlc::Hlc;

/// Who did it: a human user (`matt`) or an agent session
/// (`claude:think-3-build`). Ordered so it can break HLC ties.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ActorId(pub String);

impl ActorId {
    pub fn new(id: impl Into<String>) -> Self {
        ActorId(id.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The actor a command runs as, from the three places it can come from
    /// (README §Data model "actor"): `PM_ACTOR`, then `--as`, then `$USER`.
    /// The environment wins over the flag so a build loop that exports
    /// `PM_ACTOR=claude:pm-build` attributes every nested invocation to
    /// itself. Blank values count as unset. Pure: the caller reads the
    /// environment and passes the strings in.
    pub fn resolve(
        pm_actor: Option<&str>,
        as_flag: Option<&str>,
        user: Option<&str>,
    ) -> Option<Self> {
        [pm_actor, as_flag, user]
            .into_iter()
            .flatten()
            .map(str::trim)
            .find(|s| !s.is_empty())
            .map(ActorId::new)
    }

    /// Agent sessions are namespaced, `<agent>:<session>`
    /// (`claude:think-3-build`); a bare id (`matt`, from `$USER`) is a
    /// human.
    pub fn kind(&self) -> ActorKind {
        if self.0.contains(':') {
            ActorKind::Agent
        } else {
            ActorKind::Human
        }
    }
}

impl fmt::Display for ActorId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActorKind {
    Human,
    Agent,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Actor {
    pub id: ActorId,
    pub kind: ActorKind,
}

/// Linear's state categories: what a workflow state *means*, independent
/// of what a workspace calls it (`triage` is `unstarted`, `in-progress` is
/// `started`, `done` is `completed`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StateCategory {
    Backlog,
    Unstarted,
    Started,
    Completed,
    Canceled,
}

/// One workflow state. `position` orders states within a workspace.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct State {
    pub name: String,
    pub category: StateCategory,
    pub position: u32,
}

/// Workspace configuration.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Workspace {
    pub id: Ulid,
    /// Human-number prefix: `AGT`.
    pub prefix: String,
    pub states: Vec<State>,
    /// Labels that gate a ticket out of `pm ready` (e.g. `manual`).
    pub gate_labels: BTreeSet<String>,
    /// `model:fable-5` → the model name a build loop should use.
    pub model_labels: BTreeMap<String, String>,
    /// Section headings `pm new` scaffolds into a ticket body.
    pub template_sections: Vec<String>,
    /// Days without an update before `pm check` flags a ticket as stale.
    pub stale_days: u32,
}

impl Workspace {
    pub fn state(&self, name: &str) -> Option<&State> {
        self.states.iter().find(|s| s.name == name)
    }
}

#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum Priority {
    Low,
    #[default]
    Medium,
    High,
    Critical,
}

/// Deliberately `kebab-case`, unlike every other enum here: these are the
/// literal `status:` values in `projects/*/README.md` frontmatter
/// (`in-progress|complete|abandoned`, README §Data model), and import must
/// read them verbatim.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProjectStatus {
    InProgress,
    Complete,
    Abandoned,
}

/// A project: kebab-case id, a design doc (today's README) and any extra
/// named documents (today's sibling `.md` files, `ideation/IDEA-*`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Project {
    pub id: String,
    pub title: String,
    pub status: ProjectStatus,
    pub parent: Option<String>,
    pub repos: BTreeSet<String>,
    pub doc: String,
    pub documents: BTreeMap<String, String>,
}

/// Where an imported ticket came from (`source {type,url,id,fetched_at}`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Source {
    #[serde(rename = "type")]
    pub kind: String,
    pub url: String,
    pub id: String,
    pub fetched_at: String,
}

/// Structured replacement for `⚠ NEEDS-HUMAN` / `waiting-human` prose.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hold {
    pub reason: String,
    pub by: ActorId,
    pub at: Hlc,
}

/// Structured replacement for `waived: <reason>` lines: which hygiene
/// rule is waived and why.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Waiver {
    pub rule: String,
    pub reason: String,
}

/// Structured replacement for prose date gates: an ISO-8601 date before
/// which the ticket is not ready.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NotBefore {
    pub date: String,
}

/// Parked until an ISO-8601 date.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Parked {
    pub until: String,
}

/// A ticket, as read. `labels` is the materialized OR-set; comments and
/// relations are separate entities (see [`Comment`], [`Relation`]).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ticket {
    pub id: Ulid,
    /// Human number (`AGT-1332` → `1332`); `None` until the authority
    /// allocates one (`AGT-?`).
    pub number: Option<u64>,
    pub title: String,
    pub state: String,
    pub priority: Priority,
    pub project: Option<String>,
    pub repo: Option<String>,
    pub assignee: Option<ActorId>,
    /// Markdown body, materialized from the text CRDT
    /// ([`crate::view::TicketView::body`]).
    pub description: String,
    pub labels: BTreeSet<String>,
    pub created: Hlc,
    pub updated: Hlc,
    pub archived_at: Option<Hlc>,
    pub deleted: bool,
    pub linked_github: Option<String>,
    pub linked_pr: Option<String>,
    pub linear: Option<String>,
    pub source: Option<Source>,
    pub hold: Option<Hold>,
    pub waivers: Vec<Waiver>,
    pub not_before: Option<NotBefore>,
    pub parked: Option<Parked>,
    /// Every unknown frontmatter key preserved on import.
    pub ext: BTreeMap<String, Value>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RelationKind {
    /// `from` blocks `to`.
    Blocks,
    /// `from` is a sub-ticket of `to`.
    Parent,
    /// `from` is superseded by `to`.
    SupersededBy,
}

/// A directed edge between two tickets; read it as `from <kind> to`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Relation {
    pub kind: RelationKind,
    pub from: Ulid,
    pub to: Ulid,
}

impl Relation {
    pub fn touches(&self, ticket: Ulid) -> bool {
        self.from == ticket || self.to == ticket
    }
}

/// An append-only comment. Ordered by `(hlc, author)` — see
/// [`crate::merge::CommentLog`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Comment {
    pub id: Ulid,
    pub ticket: Ulid,
    pub author: ActorId,
    pub hlc: Hlc,
    pub body: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip<T>(value: &T)
    where
        T: Serialize + for<'de> Deserialize<'de> + PartialEq + fmt::Debug,
    {
        let json = serde_json::to_string(value).unwrap();
        let back: T = serde_json::from_str(&json).unwrap();
        assert_eq!(&back, value, "{json}");
    }

    #[test]
    fn every_entity_round_trips_through_serde() {
        let actor = ActorId::new("claude:pm-build");
        round_trip(&Actor {
            id: actor.clone(),
            kind: ActorKind::Agent,
        });
        round_trip(&Workspace {
            id: Ulid::new(),
            prefix: "AGT".into(),
            states: vec![
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
                State {
                    name: "done".into(),
                    category: StateCategory::Completed,
                    position: 2,
                },
            ],
            gate_labels: ["manual".to_string()].into(),
            model_labels: [("model:fable-5".to_string(), "fable".to_string())].into(),
            template_sections: vec!["Problem Statement".into(), "Acceptance Criteria".into()],
            stale_days: 30,
        });
        round_trip(&Project {
            id: "pm".into(),
            title: "pm".into(),
            status: ProjectStatus::InProgress,
            parent: None,
            repos: ["OpenThinkAi/pm".to_string()].into(),
            doc: "# pm\n".into(),
            documents: [("ideation/IDEA-1".to_string(), "…".to_string())].into(),
        });
        let (a, b) = (Ulid::new(), Ulid::new());
        round_trip(&Relation {
            kind: RelationKind::Blocks,
            from: a,
            to: b,
        });
        round_trip(&Comment {
            id: Ulid::new(),
            ticket: a,
            author: actor.clone(),
            hlc: Hlc::new(7, 0),
            body: "hi".into(),
        });
        round_trip(&Ticket {
            id: a,
            number: Some(1334),
            title: "pm-core".into(),
            state: "in-progress".into(),
            priority: Priority::High,
            project: Some("pm".into()),
            repo: Some("OpenThinkAi/pm".into()),
            assignee: Some(actor.clone()),
            description: String::new(),
            labels: ["model:fable-5".to_string()].into(),
            created: Hlc::new(1, 0),
            updated: Hlc::new(9, 2),
            archived_at: None,
            deleted: false,
            linked_github: None,
            linked_pr: Some("https://github.com/OpenThinkAi/pm/pull/1".into()),
            linear: None,
            source: Some(Source {
                kind: "manual".into(),
                url: String::new(),
                id: String::new(),
                fetched_at: String::new(),
            }),
            hold: Some(Hold {
                reason: "needs Matt".into(),
                by: actor,
                at: Hlc::new(8, 0),
            }),
            waivers: vec![Waiver {
                rule: "R1".into(),
                reason: "standalone".into(),
            }],
            not_before: Some(NotBefore {
                date: "2026-10-01".into(),
            }),
            parked: Some(Parked {
                until: "2026-11-01".into(),
            }),
            ext: [("custom".to_string(), Value::from(3))].into(),
        });
    }

    #[test]
    fn enums_serialize_as_snake_case_names() {
        assert_eq!(
            serde_json::to_string(&StateCategory::Unstarted).unwrap(),
            r#""unstarted""#
        );
        assert_eq!(
            serde_json::to_string(&Priority::Critical).unwrap(),
            r#""critical""#
        );
        assert_eq!(
            serde_json::to_string(&RelationKind::SupersededBy).unwrap(),
            r#""superseded_by""#
        );
        assert_eq!(
            serde_json::to_string(&ProjectStatus::InProgress).unwrap(),
            r#""in-progress""#
        );
        assert!(Priority::Low < Priority::Critical);
    }

    #[test]
    fn actor_resolves_env_then_flag_then_user() {
        let claude = Some("claude:pm-build");
        assert_eq!(
            ActorId::resolve(claude, Some("bob"), Some("matt")),
            Some(ActorId::new("claude:pm-build"))
        );
        assert_eq!(
            ActorId::resolve(None, Some("bob"), Some("matt")),
            Some(ActorId::new("bob"))
        );
        assert_eq!(
            ActorId::resolve(Some("  "), Some(""), Some(" matt ")),
            Some(ActorId::new("matt")),
            "blank values are unset and the winner is trimmed"
        );
        assert_eq!(ActorId::resolve(None, None, None), None);
    }

    #[test]
    fn actor_kind_follows_the_session_namespace() {
        assert_eq!(
            ActorId::new("claude:think-3-build").kind(),
            ActorKind::Agent
        );
        assert_eq!(ActorId::new("matt").kind(), ActorKind::Human);
    }

    #[test]
    fn source_uses_the_frontmatter_key_type() {
        let src = Source {
            kind: "github".into(),
            url: "u".into(),
            id: "1".into(),
            fetched_at: "t".into(),
        };
        let json = serde_json::to_value(&src).unwrap();
        assert_eq!(json["type"], "github");
    }
}
