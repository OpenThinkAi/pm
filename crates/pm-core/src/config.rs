//! Workspace config and project metadata as merge state (AGT-1384; README
//! §Sync & hub "Config must become ops", decision A4).
//!
//! Until now the workspace (prefix, states, gate labels, template,
//! `stale_days`), its actors and each project's metadata were direct table
//! writes, so push/pull could not carry them. This module gives them the
//! same shape tickets have: a view per entity holding each field as the
//! CRDT its rule calls for (README §Conflict semantics — LWW by HLC then
//! actor for scalars, OR-set add-wins for the label and repo sets), and a
//! pure fold that is idempotent and order-independent.
//!
//! Two views, two entity keys:
//! - [`WorkspaceView`], keyed by the workspace id, folds `workspace.set`,
//!   `state.upsert` and `actor.upsert`. States and actors are name-keyed
//!   children of the workspace (one LWW register each), the way `ext`
//!   keys are of a ticket.
//! - [`ProjectView`], keyed by a per-project Ulid, folds `project.create`,
//!   `project.set`, `project.delete` and `project.doc_add`. The kebab-case
//!   id (`pm`) is a register set by the create, as a ticket's human
//!   number is separate from its Ulid.
//!
//! Document bodies stay where they are: a project's design doc and named
//! documents are [`crate::doc::DocView`]s under their own `doc_id`s. What
//! the project view holds since AGT-1413 is their *identity* — which
//! `doc_id` is the design doc and which is each named document
//! ([`DocClaims`]) — so it travels with the project's own ops.

use std::collections::BTreeMap;
use std::collections::btree_map::Entry;

use serde::{Deserialize, Serialize};
use ulid::Ulid;

use crate::domain::{
    Actor, ActorId, ActorKind, DocsOwner, Project, ProjectStatus, State, Workspace,
};
use crate::hlc::Stamp;
use crate::merge::{Lww, OrSet};
use crate::op::{Op, Payload, ProjectDocAdd, ProjectSet, WorkspaceSet};

/// The workspace's merge state: its config, states and actors.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceView {
    pub id: Ulid,
    /// Greatest stamp of any op applied.
    pub updated: Option<Stamp>,
    pub prefix: Lww<String>,
    pub gate_labels: OrSet<String>,
    /// One register per label; a `None` value is a deleted key.
    pub model_labels: BTreeMap<String, Lww<Option<String>>>,
    pub template_sections: Lww<Vec<String>>,
    pub stale_days: Lww<u32>,
    /// `#[serde(default)]`: views stored before AGT-1406 (client
    /// `workspace_view` rows, the hub's `workspace_views`) have no such key
    /// and must still load — the hub panics on a view it can't decode.
    #[serde(default)]
    pub docs_owned_by: Lww<DocsOwner>,
    /// Keyed by state name; the register holds the whole record, since
    /// `state.upsert` always writes `category` and `position` together.
    pub states: BTreeMap<String, Lww<State>>,
    pub actors: BTreeMap<ActorId, Lww<ActorKind>>,
}

impl WorkspaceView {
    /// An empty view; any op for `id` can be applied first.
    pub fn new(id: Ulid) -> Self {
        WorkspaceView {
            id,
            updated: None,
            prefix: Lww::default(),
            gate_labels: OrSet::default(),
            model_labels: BTreeMap::new(),
            template_sections: Lww::default(),
            stale_days: Lww::default(),
            docs_owned_by: Lww::default(),
            states: BTreeMap::new(),
            actors: BTreeMap::new(),
        }
    }

    /// The plain [`Workspace`]; states are ordered by `position`, then name.
    pub fn snapshot(&self) -> Workspace {
        let mut states: Vec<State> = self.states.values().map(|r| r.value.clone()).collect();
        states.sort_by(|a, b| {
            a.position
                .cmp(&b.position)
                .then_with(|| a.name.cmp(&b.name))
        });
        Workspace {
            id: self.id,
            prefix: self.prefix.value.clone(),
            states,
            gate_labels: self.gate_labels.iter().cloned().collect(),
            model_labels: self
                .model_labels
                .iter()
                .filter_map(|(k, reg)| reg.value.clone().map(|v| (k.clone(), v)))
                .collect(),
            template_sections: self.template_sections.value.clone(),
            stale_days: self.stale_days.value,
            docs_owned_by: self.docs_owned_by.value,
        }
    }

    /// Every actor the workspace has seen, by id.
    pub fn actors(&self) -> Vec<Actor> {
        self.actors
            .iter()
            .map(|(id, reg)| Actor {
                id: id.clone(),
                kind: reg.value,
            })
            .collect()
    }
}

/// A project's metadata as merge state. Its documents are separate
/// [`crate::doc::DocView`]s.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectView {
    pub id: Ulid,
    /// Stamp of the `project.create` op, once seen.
    pub created: Option<Stamp>,
    /// Greatest stamp of any op applied.
    pub updated: Option<Stamp>,
    /// The kebab-case human id (`Project::id`), set by `project.create`.
    pub slug: Lww<String>,
    pub title: Lww<String>,
    pub status: Lww<ProjectStatus>,
    pub parent: Lww<Option<String>>,
    pub repos: OrSet<String>,
    /// Tombstone: the earliest `project.delete` stamp seen. Permanent —
    /// no later op un-deletes the project (README §Conflict semantics,
    /// the ticket tombstone's rule) — and, being a minimum, independent
    /// of the order ops arrive in.
    #[serde(default)]
    pub deleted_at: Option<Stamp>,
    /// The design doc's identity (AGT-1413): a `project.create`'s
    /// `doc_id`, or a `project.doc_add` without a name.
    #[serde(default)]
    pub design_doc: DocClaims,
    /// Each named document's identity, by name (`project.doc_add`).
    #[serde(default)]
    pub documents: BTreeMap<String, DocClaims>,
}

/// Every `doc_id` ever bound to one document slot (the design doc, or one
/// document name), with the earliest stamp that bound it. The slot's
/// document is the earliest *eligible* binding — [`DocClaims::winner`],
/// by stamp then `doc_id` — so a document's identity never moves once
/// made, and two replicas that bind the same slot offline converge on one
/// id whatever order the ops arrive in. The losers stay listed: their
/// `body.edit` ops are still documents' edits (a replica must route them
/// as such) even though no row shows them.
///
/// **Eligibility (AGT-1464).** Earliest-wins is the one rule where a
/// *backdated* op wins (under LWW it loses): a peer could send a
/// `project.doc_add` stamped `hlc 0` and take any project's design doc or
/// named document for good. So a binding stamped before the project's
/// `project.create` ([`ProjectView::created`]) is eligible only when the
/// project's creator made it (the create's actor). An honest binding is
/// never earlier than the create unless the creator made it: binding a
/// document needs the project, and every replica's clock has moved past
/// the create's stamp by the time it has the project; the one exception,
/// migration 0008's backfill, stamps its `project.doc_add`s a millisecond
/// *below* the log's oldest op, under the same `migrate` actor that
/// migration 0007 gave every backfilled `project.create`. When no binding
/// of a slot is eligible, the earliest overall is used (a database whose
/// projects were created by someone else before 0008 ran keeps its
/// documents); before the create is seen every binding is eligible.
///
/// Why this converges: eligibility reads only the slot's claims and
/// `created` — each a pure, order-independent fold of the op set (a
/// minimum per id, a minimum over creates) — so every replica holding the
/// same ops picks the same winner, whatever order they arrived in. What it
/// cannot stop is a binding stamped *after* the create but before a later
/// honest one; stamps alone cannot tell that apart from an honest offline
/// binding, only arrival order at the hub can. A second `project.create`
/// for a project (which could drag `created` back) is refused at every
/// ingest path instead (`pm-store`, and the hub's push).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct DocClaims(BTreeMap<Ulid, Stamp>);

impl DocClaims {
    /// Records that `stamp` bound `doc_id` to this slot; keeps the
    /// earliest stamp per id, so re-applying an op changes nothing.
    pub fn claim(&mut self, doc_id: Ulid, stamp: Stamp) {
        match self.0.entry(doc_id) {
            Entry::Vacant(slot) => {
                slot.insert(stamp);
            }
            Entry::Occupied(mut slot) => {
                if stamp < *slot.get() {
                    slot.insert(stamp);
                }
            }
        }
    }

    /// The slot's document for a project created at `created` (`None`:
    /// no `project.create` seen yet): the earliest eligible binding (see
    /// the type's docs), else the earliest; `None` before any binding.
    pub fn winner(&self, created: Option<&Stamp>) -> Option<Ulid> {
        let eligible = |stamp: &Stamp| {
            created.is_none_or(|created| stamp >= created || stamp.actor == created.actor)
        };
        let earliest = |eligible_only: bool| {
            self.0
                .iter()
                .filter(|(_, stamp)| !eligible_only || eligible(stamp))
                .min_by(|(a_id, a), (b_id, b)| a.cmp(b).then_with(|| a_id.cmp(b_id)))
                .map(|(id, _)| *id)
        };
        earliest(true).or_else(|| earliest(false))
    }

    /// Whether `doc_id` was ever bound to this slot.
    pub fn contains(&self, doc_id: Ulid) -> bool {
        self.0.contains_key(&doc_id)
    }

    /// Every id ever bound to this slot, winner or not.
    pub fn ids(&self) -> impl Iterator<Item = Ulid> + '_ {
        self.0.keys().copied()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl ProjectView {
    /// An empty view; any op for `id` can be applied first, including a
    /// `project.set` that synced ahead of the `project.create`.
    pub fn new(id: Ulid) -> Self {
        ProjectView {
            id,
            created: None,
            updated: None,
            slug: Lww::default(),
            title: Lww::default(),
            status: Lww::default(),
            parent: Lww::default(),
            repos: OrSet::default(),
            deleted_at: None,
            design_doc: DocClaims::default(),
            documents: BTreeMap::new(),
        }
    }

    /// The design doc's `doc_id`, once one is bound.
    pub fn design_doc_id(&self) -> Option<Ulid> {
        self.design_doc.winner(self.created.as_ref())
    }

    /// The named document `name`'s `doc_id`, once one is bound.
    pub fn doc_id(&self, name: &str) -> Option<Ulid> {
        self.documents
            .get(name)
            .and_then(|claims| claims.winner(self.created.as_ref()))
    }

    /// Every `doc_id` ever bound to one of this project's documents,
    /// with its slot (`None` = the design doc) — winners and losers.
    pub fn doc_ids(&self) -> impl Iterator<Item = (Option<&str>, Ulid)> + '_ {
        self.design_doc.ids().map(|id| (None, id)).chain(
            self.documents
                .iter()
                .flat_map(|(name, claims)| claims.ids().map(move |id| (Some(name.as_str()), id))),
        )
    }

    /// The plain [`Project`]. The design doc and named documents are not
    /// part of this view (they fold from `body.edit` ops under their own
    /// ids), so the caller supplies their materialized text.
    pub fn snapshot(&self, doc: String, documents: BTreeMap<String, String>) -> Project {
        Project {
            id: self.slug.value.clone(),
            title: self.title.value.clone(),
            status: self.status.value,
            parent: self.parent.value.clone(),
            repos: self.repos.iter().cloned().collect(),
            doc,
            documents,
        }
    }
}

/// Why an op could not fold into a config view. Both cases are caller
/// bugs — pm-store routes an op by its kind and entity before folding it.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ConfigApplyError {
    #[error("op {op_id} targets entity {entity}, not {view} {id}")]
    EntityMismatch {
        op_id: Ulid,
        entity: Ulid,
        view: &'static str,
        id: Ulid,
    },
    #[error("op {op_id}: a {view} does not accept the op kind '{kind}'")]
    WrongKind {
        op_id: Ulid,
        view: &'static str,
        kind: &'static str,
    },
    /// Admission, not folding (AGT-1464; the hub's push, pm-store's commit
    /// paths): a second `project.create` for a project. It could move
    /// [`ProjectView::created`], which document identity is anchored to.
    #[error("op {op_id}: project {project} already has a project.create")]
    DuplicateCreate { op_id: Ulid, project: Ulid },
    /// Admission (AGT-1464): an id already taken in the op log's shared
    /// entity namespace — a `ticket.create` for a bound document's id.
    #[error("op {op_id}: id {entity} is already {holder}")]
    EntityInUse {
        op_id: Ulid,
        entity: Ulid,
        holder: &'static str,
    },
}

/// Fold `op` into `view`. Pure and idempotent: applying the same op again
/// leaves `view` unchanged, and replaying a workspace's ops in any order
/// converges.
pub fn apply_workspace(view: &mut WorkspaceView, op: &Op) -> Result<(), ConfigApplyError> {
    if op.entity != view.id {
        return Err(ConfigApplyError::EntityMismatch {
            op_id: op.op_id,
            entity: op.entity,
            view: "workspace",
            id: view.id,
        });
    }
    let stamp = op.stamp();
    match &op.payload {
        Payload::WorkspaceSet(field) => match field {
            WorkspaceSet::Prefix(v) => {
                view.prefix.set(v.clone(), stamp.clone());
            }
            WorkspaceSet::GateLabelAdd(label) => view.gate_labels.add(label.clone(), op.op_id),
            WorkspaceSet::GateLabelRemove { label, observed } => {
                view.gate_labels.remove(label, observed);
            }
            WorkspaceSet::ModelLabel { label, model } => {
                view.model_labels
                    .entry(label.clone())
                    .or_default()
                    .set(model.clone(), stamp.clone());
            }
            WorkspaceSet::TemplateSections(v) => {
                view.template_sections.set(v.clone(), stamp.clone());
            }
            WorkspaceSet::StaleDays(v) => {
                view.stale_days.set(*v, stamp.clone());
            }
            WorkspaceSet::DocsOwnedBy(v) => {
                view.docs_owned_by.set(*v, stamp.clone());
            }
        },
        Payload::StateUpsert(s) => upsert(
            &mut view.states,
            s.name.clone(),
            State {
                name: s.name.clone(),
                category: s.category,
                position: s.position,
            },
            stamp.clone(),
        ),
        Payload::ActorUpsert(a) => upsert(&mut view.actors, a.id.clone(), a.kind, stamp.clone()),
        other => {
            return Err(ConfigApplyError::WrongKind {
                op_id: op.op_id,
                view: "workspace",
                kind: other.kind(),
            });
        }
    }
    bump(&mut view.updated, stamp);
    Ok(())
}

/// Fold `op` into `view`; the project analogue of [`apply_workspace`],
/// with the same guarantees.
pub fn apply_project(view: &mut ProjectView, op: &Op) -> Result<(), ConfigApplyError> {
    if op.entity != view.id {
        return Err(ConfigApplyError::EntityMismatch {
            op_id: op.op_id,
            entity: op.entity,
            view: "project",
            id: view.id,
        });
    }
    let stamp = op.stamp();
    match &op.payload {
        Payload::ProjectCreate(c) => {
            if view.created.as_ref().is_none_or(|s| stamp < *s) {
                view.created = Some(stamp.clone());
            }
            view.slug.set(c.id.clone(), stamp.clone());
            view.title.set(c.title.clone(), stamp.clone());
            view.status.set(c.status, stamp.clone());
            view.parent.set(c.parent.clone(), stamp.clone());
            if let Some(doc_id) = c.doc_id {
                view.design_doc.claim(doc_id, stamp.clone());
            }
        }
        Payload::ProjectDocAdd(ProjectDocAdd { name, doc_id }) => match name {
            None => view.design_doc.claim(*doc_id, stamp.clone()),
            Some(name) => view
                .documents
                .entry(name.clone())
                .or_default()
                .claim(*doc_id, stamp.clone()),
        },
        Payload::ProjectSet(field) => match field {
            ProjectSet::Title(v) => {
                view.title.set(v.clone(), stamp.clone());
            }
            ProjectSet::Status(v) => {
                view.status.set(*v, stamp.clone());
            }
            ProjectSet::Parent(v) => {
                view.parent.set(v.clone(), stamp.clone());
            }
            ProjectSet::RepoAdd(repo) => view.repos.add(repo.clone(), op.op_id),
            ProjectSet::RepoRemove { repo, observed } => view.repos.remove(repo, observed),
        },
        Payload::ProjectDelete => {
            if view.deleted_at.as_ref().is_none_or(|s| stamp < *s) {
                view.deleted_at = Some(stamp.clone());
            }
        }
        other => {
            return Err(ConfigApplyError::WrongKind {
                op_id: op.op_id,
                view: "project",
                kind: other.kind(),
            });
        }
    }
    bump(&mut view.updated, stamp);
    Ok(())
}

/// LWW-write `value` under `key`, creating the register on first sight.
/// (`Lww::default` needs `T: Default`, which a state record or an actor
/// kind has no meaningful value for.)
fn upsert<K: Ord, T>(map: &mut BTreeMap<K, Lww<T>>, key: K, value: T, stamp: Stamp) {
    match map.entry(key) {
        Entry::Vacant(slot) => {
            slot.insert(Lww {
                value,
                stamp: Some(stamp),
            });
        }
        Entry::Occupied(mut slot) => {
            slot.get_mut().set(value, stamp);
        }
    }
}

fn bump(updated: &mut Option<Stamp>, stamp: Stamp) {
    if updated.as_ref().is_none_or(|s| stamp > *s) {
        *updated = Some(stamp);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Priority;
    use crate::domain::StateCategory;
    use crate::hlc::Hlc;
    use crate::op::{ActorUpsert, ProjectCreate, StateUpsert, TicketCreate};
    use proptest::prelude::*;
    use proptest::sample::Index;

    fn op(entity: Ulid, wall_ms: u64, actor: &str, payload: Payload) -> Op {
        Op::new(
            Ulid::new(),
            Hlc::new(wall_ms, 0),
            ActorId::new(actor),
            entity,
            payload,
        )
    }

    fn ws(entity: Ulid, wall_ms: u64, actor: &str, field: WorkspaceSet) -> Op {
        op(entity, wall_ms, actor, Payload::WorkspaceSet(field))
    }

    fn state(entity: Ulid, wall_ms: u64, name: &str, category: StateCategory, position: u32) -> Op {
        op(
            entity,
            wall_ms,
            "matt",
            Payload::StateUpsert(StateUpsert {
                name: name.into(),
                category,
                position,
            }),
        )
    }

    fn actor(entity: Ulid, wall_ms: u64, id: &str, kind: ActorKind) -> Op {
        op(
            entity,
            wall_ms,
            "matt",
            Payload::ActorUpsert(ActorUpsert {
                id: ActorId::new(id),
                kind,
            }),
        )
    }

    fn create(project: Ulid, wall_ms: u64) -> Op {
        op(
            project,
            wall_ms,
            "matt",
            Payload::ProjectCreate(ProjectCreate {
                id: "pm".into(),
                title: "pm".into(),
                status: ProjectStatus::InProgress,
                parent: None,
                doc_id: None,
            }),
        )
    }

    fn ps(project: Ulid, wall_ms: u64, actor: &str, field: ProjectSet) -> Op {
        op(project, wall_ms, actor, Payload::ProjectSet(field))
    }

    fn ticket_create(entity: Ulid) -> Op {
        op(
            entity,
            1,
            "matt",
            Payload::TicketCreate(TicketCreate {
                title: "t".into(),
                state: "triage".into(),
                priority: Priority::Medium,
                project: None,
                repo: None,
                source: None,
                ext: Default::default(),
            }),
        )
    }

    /// Every workspace-side kind once, in a plausible sequence: init,
    /// a rename, a gate label added then removed, a model label set then
    /// deleted, states and actors upserted twice.
    fn workspace_history(id: Ulid) -> Vec<Op> {
        let manual = ws(id, 2, "matt", WorkspaceSet::GateLabelAdd("manual".into()));
        vec![
            ws(id, 1, "matt", WorkspaceSet::Prefix("AGT".into())),
            ws(id, 1, "matt", WorkspaceSet::StaleDays(30)),
            ws(
                id,
                1,
                "matt",
                WorkspaceSet::TemplateSections(vec!["Problem Statement".into()]),
            ),
            state(id, 1, "triage", StateCategory::Unstarted, 0),
            state(id, 1, "done", StateCategory::Completed, 5),
            actor(id, 1, "matt", ActorKind::Human),
            manual.clone(),
            ws(id, 2, "matt", WorkspaceSet::GateLabelAdd("blocked".into())),
            ws(
                id,
                3,
                "matt",
                WorkspaceSet::ModelLabel {
                    label: "model:fable-5".into(),
                    model: Some("fable".into()),
                },
            ),
            ws(
                id,
                3,
                "matt",
                WorkspaceSet::ModelLabel {
                    label: "model:opus-5".into(),
                    model: Some("opus".into()),
                },
            ),
            state(id, 4, "in-progress", StateCategory::Started, 1),
            state(id, 5, "done", StateCategory::Completed, 2),
            actor(id, 5, "claude:pm-build", ActorKind::Agent),
            actor(id, 6, "matt", ActorKind::Human),
            ws(
                id,
                7,
                "matt",
                WorkspaceSet::GateLabelRemove {
                    label: "manual".into(),
                    observed: vec![manual.op_id],
                },
            ),
            ws(
                id,
                8,
                "matt",
                WorkspaceSet::ModelLabel {
                    label: "model:opus-5".into(),
                    model: None,
                },
            ),
            ws(
                id,
                9,
                "matt",
                WorkspaceSet::TemplateSections(vec![
                    "Problem Statement".into(),
                    "Acceptance Criteria".into(),
                ]),
            ),
            ws(id, 10, "matt", WorkspaceSet::StaleDays(14)),
        ]
    }

    fn project_history(id: Ulid) -> Vec<Op> {
        let pm_repo = ps(id, 2, "matt", ProjectSet::RepoAdd("OpenThinkAi/pm".into()));
        vec![
            create(id, 1),
            pm_repo.clone(),
            ps(
                id,
                2,
                "matt",
                ProjectSet::RepoAdd("OpenThinkAi/pm-hub".into()),
            ),
            ps(
                id,
                3,
                "matt",
                ProjectSet::Title("pm — the vault as a database".into()),
            ),
            ps(id, 4, "matt", ProjectSet::Parent(Some("openthink".into()))),
            ps(
                id,
                5,
                "matt",
                ProjectSet::RepoRemove {
                    repo: "OpenThinkAi/pm".into(),
                    observed: vec![pm_repo.op_id],
                },
            ),
            ps(id, 6, "matt", ProjectSet::Status(ProjectStatus::Complete)),
        ]
    }

    #[test]
    fn workspace_history_folds_and_is_idempotent() {
        let id = Ulid::new();
        let mut once = WorkspaceView::new(id);
        let mut twice = WorkspaceView::new(id);
        for op in workspace_history(id) {
            apply_workspace(&mut once, &op).unwrap();
            apply_workspace(&mut twice, &op).unwrap();
            apply_workspace(&mut twice, &op).unwrap();
            assert_eq!(once, twice, "after replaying {}", op.kind());
        }
        let w = once.snapshot();
        assert_eq!(w.id, id);
        assert_eq!(w.prefix, "AGT");
        assert_eq!(w.stale_days, 14);
        assert_eq!(
            w.template_sections,
            ["Problem Statement", "Acceptance Criteria"]
        );
        assert_eq!(w.gate_labels.iter().collect::<Vec<_>>(), ["blocked"]);
        assert_eq!(
            w.model_labels,
            [("model:fable-5".to_string(), "fable".to_string())].into(),
            "deleted model label is gone"
        );
        let states: Vec<(&str, StateCategory, u32)> = w
            .states
            .iter()
            .map(|s| (s.name.as_str(), s.category, s.position))
            .collect();
        assert_eq!(
            states,
            [
                ("triage", StateCategory::Unstarted, 0),
                ("in-progress", StateCategory::Started, 1),
                ("done", StateCategory::Completed, 2),
            ],
            "ordered by position; the later upsert of 'done' won"
        );
        assert_eq!(
            once.actors(),
            [
                Actor {
                    id: ActorId::new("claude:pm-build"),
                    kind: ActorKind::Agent
                },
                Actor {
                    id: ActorId::new("matt"),
                    kind: ActorKind::Human
                },
            ]
        );
        assert_eq!(once.updated.as_ref().unwrap().hlc, Hlc::new(10, 0));
    }

    #[test]
    fn project_history_folds_and_is_idempotent() {
        let id = Ulid::new();
        let mut once = ProjectView::new(id);
        let mut twice = ProjectView::new(id);
        for op in project_history(id) {
            apply_project(&mut once, &op).unwrap();
            apply_project(&mut twice, &op).unwrap();
            apply_project(&mut twice, &op).unwrap();
            assert_eq!(once, twice, "after replaying {}", op.kind());
        }
        let p = once.snapshot(
            "# pm\n".into(),
            [("notes".to_string(), "n".to_string())].into(),
        );
        assert_eq!(p.id, "pm");
        assert_eq!(p.title, "pm — the vault as a database");
        assert_eq!(p.status, ProjectStatus::Complete);
        assert_eq!(p.parent.as_deref(), Some("openthink"));
        assert_eq!(p.repos.iter().collect::<Vec<_>>(), ["OpenThinkAi/pm-hub"]);
        assert_eq!(p.doc, "# pm\n");
        assert_eq!(p.documents.len(), 1);
        assert_eq!(once.created, Some(create(id, 1).stamp()));
        assert_eq!(once.updated.as_ref().unwrap().hlc, Hlc::new(6, 0));
    }

    #[test]
    fn replaying_either_log_in_reverse_converges() {
        let id = Ulid::new();
        let history = workspace_history(id);
        let mut forward = WorkspaceView::new(id);
        let mut reverse = WorkspaceView::new(id);
        for op in &history {
            apply_workspace(&mut forward, op).unwrap();
        }
        for op in history.iter().rev() {
            apply_workspace(&mut reverse, op).unwrap();
        }
        assert_eq!(forward, reverse);

        let history = project_history(id);
        let mut forward = ProjectView::new(id);
        let mut reverse = ProjectView::new(id);
        for op in &history {
            apply_project(&mut forward, op).unwrap();
        }
        for op in history.iter().rev() {
            apply_project(&mut reverse, op).unwrap();
        }
        assert_eq!(forward, reverse);
    }

    #[test]
    fn a_view_stored_before_docs_owned_by_still_loads() {
        let mut view = WorkspaceView::new(Ulid::new());
        let mut json = serde_json::to_value(&view).unwrap();
        json.as_object_mut().unwrap().remove("docs_owned_by");
        view = serde_json::from_value(json).unwrap();
        assert_eq!(view.snapshot().docs_owned_by, DocsOwner::Vault);
    }

    #[test]
    fn docs_owned_by_is_a_default_vault_lww_scalar() {
        let id = Ulid::new();
        let mut view = WorkspaceView::new(id);
        assert_eq!(view.snapshot().docs_owned_by, DocsOwner::Vault);
        let pm = ws(id, 5, "matt", WorkspaceSet::DocsOwnedBy(DocsOwner::Pm));
        let vault = ws(id, 3, "matt", WorkspaceSet::DocsOwnedBy(DocsOwner::Vault));
        apply_workspace(&mut view, &pm).unwrap();
        apply_workspace(&mut view, &vault).unwrap();
        apply_workspace(&mut view, &pm).unwrap();
        assert_eq!(view.snapshot().docs_owned_by, DocsOwner::Pm);
        let later = ws(id, 9, "matt", WorkspaceSet::DocsOwnedBy(DocsOwner::Vault));
        apply_workspace(&mut view, &later).unwrap();
        assert_eq!(view.snapshot().docs_owned_by, DocsOwner::Vault);
    }

    #[test]
    fn concurrent_scalar_writes_resolve_by_hlc_then_actor() {
        let id = Ulid::new();
        let alice = ws(id, 5, "alice", WorkspaceSet::Prefix("ALI".into()));
        let bob = ws(id, 5, "bob", WorkspaceSet::Prefix("BOB".into()));
        let later = state(id, 6, "triage", StateCategory::Backlog, 9);
        let earlier = state(id, 4, "triage", StateCategory::Unstarted, 0);
        for order in [
            [&alice, &bob, &later, &earlier],
            [&earlier, &later, &bob, &alice],
        ] {
            let mut view = WorkspaceView::new(id);
            for o in order {
                apply_workspace(&mut view, o).unwrap();
            }
            assert_eq!(view.prefix.value, "BOB", "greater actor id breaks the tie");
            assert_eq!(view.snapshot().states[0].category, StateCategory::Backlog);
        }
    }

    #[test]
    fn concurrent_gate_label_remove_and_re_add_keeps_the_label() {
        let id = Ulid::new();
        let first_add = ws(id, 2, "matt", WorkspaceSet::GateLabelAdd("manual".into()));
        let remove = ws(
            id,
            3,
            "matt",
            WorkspaceSet::GateLabelRemove {
                label: "manual".into(),
                observed: vec![first_add.op_id],
            },
        );
        let re_add = ws(
            id,
            3,
            "claude:pm-build",
            WorkspaceSet::GateLabelAdd("manual".into()),
        );
        for order in [
            [&first_add, &remove, &re_add],
            [&re_add, &remove, &first_add],
        ] {
            let mut view = WorkspaceView::new(id);
            for o in order {
                apply_workspace(&mut view, o).unwrap();
            }
            assert!(view.gate_labels.contains(&"manual".to_string()));
            assert_eq!(
                view.gate_labels.observed(&"manual".to_string()),
                vec![re_add.op_id]
            );
        }
    }

    #[test]
    fn project_set_that_syncs_before_create_still_wins() {
        let id = Ulid::new();
        let rename = ps(id, 9, "matt", ProjectSet::Title("late".into()));
        let mut view = ProjectView::new(id);
        apply_project(&mut view, &rename).unwrap();
        apply_project(&mut view, &create(id, 1)).unwrap();
        assert_eq!(view.title.value, "late");
        assert_eq!(view.slug.value, "pm");
        assert_eq!(view.created, Some(create(id, 1).stamp()));
    }

    /// AGT-1413: a document slot keeps its earliest binding, whichever
    /// order the bindings arrive in, and remembers the losers.
    #[test]
    fn doc_identity_is_first_binding_wins_and_order_independent() {
        let id = Ulid::new();
        let (design, early, late) = (Ulid::new(), Ulid::new(), Ulid::new());
        let mut with_doc = create(id, 1);
        if let Payload::ProjectCreate(c) = &mut with_doc.payload {
            c.doc_id = Some(design);
        }
        let add = |wall, actor: &str, name: Option<&str>, doc_id| {
            op(
                id,
                wall,
                actor,
                Payload::ProjectDocAdd(ProjectDocAdd {
                    name: name.map(str::to_string),
                    doc_id,
                }),
            )
        };
        let ops = [
            with_doc,
            add(5, "zed", Some("notes"), late),
            add(5, "amy", Some("notes"), early),
            // A second design-doc binding, later than the create's.
            add(9, "matt", None, Ulid::new()),
        ];
        for order in [[0, 1, 2, 3], [3, 2, 1, 0], [2, 0, 3, 1]] {
            let mut view = ProjectView::new(id);
            for i in order {
                apply_project(&mut view, &ops[i]).unwrap();
                apply_project(&mut view, &ops[i]).unwrap();
            }
            assert_eq!(view.design_doc_id(), Some(design));
            assert_eq!(view.doc_id("notes"), Some(early), "amy < zed at one hlc");
            assert_eq!(view.doc_id("other"), None);
            assert_eq!(view.doc_ids().count(), 4, "losers stay listed");
        }
        // A view stored before AGT-1413 (no identity keys) still loads.
        let mut json = serde_json::to_value(ProjectView::new(id)).unwrap();
        let obj = json.as_object_mut().unwrap();
        obj.remove("design_doc");
        obj.remove("documents");
        let back: ProjectView = serde_json::from_value(json).unwrap();
        assert_eq!(back.design_doc_id(), None);
    }

    /// AGT-1464: a binding stamped before the project's create, by anyone
    /// but its creator, cannot take a slot another binding holds — in any
    /// arrival order — while the creator's own backdated binding (migration
    /// 0008's shape) still counts.
    #[test]
    fn a_backdated_binding_cannot_hijack_a_document() {
        let id = Ulid::new();
        let add = |wall, actor: &str, name: Option<&str>, doc_id| {
            op(
                id,
                wall,
                actor,
                Payload::ProjectDocAdd(ProjectDocAdd {
                    name: name.map(str::to_string),
                    doc_id,
                }),
            )
        };
        let (design, notes, evil) = (Ulid::new(), Ulid::new(), Ulid::new());
        let with_doc = op(
            id,
            100,
            "matt",
            Payload::ProjectCreate(ProjectCreate {
                id: "pm".into(),
                title: "pm".into(),
                status: ProjectStatus::InProgress,
                parent: None,
                doc_id: Some(design),
            }),
        );
        let ops = [
            with_doc,
            add(200, "claude:pm-build", Some("notes"), notes),
            // Hostile: stamped at the dawn of time, for both slots.
            add(0, "mallory", None, evil),
            add(0, "mallory", Some("notes"), evil),
            // Just under the create, or tied with it by a smaller actor.
            add(99, "aaa", None, evil),
            add(100, "aaa", None, evil),
        ];
        for order in [[0, 1, 2, 3, 4, 5], [5, 4, 3, 2, 1, 0], [2, 3, 0, 5, 1, 4]] {
            let mut view = ProjectView::new(id);
            for i in order {
                apply_project(&mut view, &ops[i]).unwrap();
            }
            assert_eq!(view.design_doc_id(), Some(design), "{order:?}");
            assert_eq!(view.doc_id("notes"), Some(notes), "{order:?}");
            assert!(view.doc_ids().any(|(_, d)| d == evil), "losers stay listed");
        }

        // Migration 0008's shape: the creator (`migrate`) binds a
        // millisecond below its own create, and keeps the slot against a
        // later-arriving backdated binding by anyone else.
        let migrated = Ulid::new();
        let mut create = create(id, 1_000);
        create.actor = ActorId::new("migrate");
        let mut view = ProjectView::new(id);
        for o in [
            add(999, "migrate", None, migrated),
            add(999, "migrate", Some("notes"), notes),
            create,
            add(5, "mallory", None, evil),
            add(5, "mallory", Some("notes"), evil),
        ] {
            apply_project(&mut view, &o).unwrap();
        }
        assert_eq!(view.design_doc_id(), Some(migrated));
        assert_eq!(view.doc_id("notes"), Some(notes));
    }

    /// With no eligible binding the slot still has a document (the earliest
    /// overall), and before the create is seen every binding counts.
    #[test]
    fn doc_eligibility_falls_back_and_waits_for_the_create() {
        let id = Ulid::new();
        let (early, late) = (Ulid::new(), Ulid::new());
        let add = |wall, actor: &str, doc_id| {
            op(
                id,
                wall,
                actor,
                Payload::ProjectDocAdd(ProjectDocAdd {
                    name: Some("notes".into()),
                    doc_id,
                }),
            )
        };
        let mut view = ProjectView::new(id);
        apply_project(&mut view, &add(3, "zed", early)).unwrap();
        apply_project(&mut view, &add(4, "amy", late)).unwrap();
        assert_eq!(view.doc_id("notes"), Some(early), "no create yet");
        apply_project(&mut view, &create(id, 10)).unwrap();
        assert_eq!(
            view.doc_id("notes"),
            Some(early),
            "both predate the create and neither is the creator's"
        );
        let later = Ulid::new();
        apply_project(&mut view, &add(20, "amy", later)).unwrap();
        assert_eq!(
            view.doc_id("notes"),
            Some(later),
            "the one eligible binding"
        );
    }

    #[test]
    fn project_delete_is_permanent_and_order_independent() {
        let id = Ulid::new();
        let del = |wall, actor: &str| op(id, wall, actor, Payload::ProjectDelete);
        let (early, late) = (del(5, "matt"), del(9, "zed"));
        let rename = ps(id, 7, "matt", ProjectSet::Title("after".into()));
        let mut view = ProjectView::new(id);
        for o in [&create(id, 1), &late, &rename, &early] {
            apply_project(&mut view, o).unwrap();
        }
        // The earliest tombstone stamp wins; a later write folds in but
        // does not undo the delete.
        assert_eq!(view.deleted_at, Some(early.stamp()));
        assert_eq!(view.title.value, "after");
        // A delete that syncs before the create still sticks.
        let mut ahead = ProjectView::new(id);
        apply_project(&mut ahead, &early).unwrap();
        apply_project(&mut ahead, &create(id, 1)).unwrap();
        assert!(ahead.deleted_at.is_some());
        // A view stored before AGT-1386 (no `deleted_at` key) still loads.
        let mut json = serde_json::to_value(ProjectView::new(id)).unwrap();
        json.as_object_mut().unwrap().remove("deleted_at");
        let back: ProjectView = serde_json::from_value(json).unwrap();
        assert_eq!(back.deleted_at, None);
    }

    #[test]
    fn ops_for_another_entity_or_the_wrong_kind_are_rejected_untouched() {
        let (id, other) = (Ulid::new(), Ulid::new());
        let mut workspace = WorkspaceView::new(id);
        let foreign = ws(other, 1, "matt", WorkspaceSet::StaleDays(1));
        assert_eq!(
            apply_workspace(&mut workspace, &foreign),
            Err(ConfigApplyError::EntityMismatch {
                op_id: foreign.op_id,
                entity: other,
                view: "workspace",
                id,
            })
        );
        let project_op = create(id, 1);
        assert_eq!(
            apply_workspace(&mut workspace, &project_op),
            Err(ConfigApplyError::WrongKind {
                op_id: project_op.op_id,
                view: "workspace",
                kind: "project.create",
            })
        );
        let ticket_op = ticket_create(id);
        assert!(matches!(
            apply_workspace(&mut workspace, &ticket_op),
            Err(ConfigApplyError::WrongKind {
                kind: "ticket.create",
                ..
            })
        ));
        assert_eq!(workspace, WorkspaceView::new(id));

        let mut project = ProjectView::new(id);
        let workspace_op = ws(id, 1, "matt", WorkspaceSet::Prefix("AGT".into()));
        assert_eq!(
            apply_project(&mut project, &workspace_op),
            Err(ConfigApplyError::WrongKind {
                op_id: workspace_op.op_id,
                view: "project",
                kind: "workspace.set",
            })
        );
        assert!(matches!(
            apply_project(&mut project, &create(other, 1)),
            Err(ConfigApplyError::EntityMismatch { .. })
        ));
        assert_eq!(project, ProjectView::new(id));
    }

    #[test]
    fn views_round_trip_through_serde() {
        let id = Ulid::new();
        let mut workspace = WorkspaceView::new(id);
        for op in workspace_history(id) {
            apply_workspace(&mut workspace, &op).unwrap();
        }
        let json = serde_json::to_string(&workspace).unwrap();
        let back: WorkspaceView = serde_json::from_str(&json).unwrap();
        assert_eq!(back, workspace);

        let mut project = ProjectView::new(id);
        for op in project_history(id) {
            apply_project(&mut project, &op).unwrap();
        }
        let json = serde_json::to_string(&project).unwrap();
        let back: ProjectView = serde_json::from_str(&json).unwrap();
        assert_eq!(back, project);
    }

    // ---- proptest: idempotent and order-independent under random logs ----

    /// One random op, before it is stamped. Set removes cite a random
    /// subset of the *earlier adds of the same value* (the OR-set's
    /// precondition, which every real remover meets by reading `observed`
    /// off the view).
    #[derive(Clone, Debug)]
    enum Spec {
        Prefix(u8),
        GateAdd(u8),
        GateRemove(u8, Vec<Index>),
        ModelLabel(u8, Option<u8>),
        Template(Vec<u8>),
        Stale(u32),
        State(u8, u8, u32),
        Actor(u8, bool),
        Create(u8, Option<u8>),
        DocAdd(Option<u8>, u8),
        Title(u8),
        Status(u8),
        Parent(Option<u8>),
        RepoAdd(u8),
        RepoRemove(u8, Vec<Index>),
        Delete,
    }

    fn cites() -> impl Strategy<Value = Vec<Index>> {
        prop::collection::vec(any::<Index>(), 0..3)
    }

    fn workspace_spec() -> impl Strategy<Value = Spec> {
        prop_oneof![
            any::<u8>().prop_map(Spec::Prefix),
            (0u8..3).prop_map(Spec::GateAdd),
            ((0u8..3), cites()).prop_map(|(l, c)| Spec::GateRemove(l, c)),
            ((0u8..3), prop::option::of(any::<u8>())).prop_map(|(l, m)| Spec::ModelLabel(l, m)),
            prop::collection::vec(any::<u8>(), 0..3).prop_map(Spec::Template),
            any::<u32>().prop_map(Spec::Stale),
            ((0u8..3), (0u8..5), any::<u32>()).prop_map(|(n, c, p)| Spec::State(n, c, p)),
            ((0u8..3), any::<bool>()).prop_map(|(i, a)| Spec::Actor(i, a)),
        ]
    }

    fn project_spec() -> impl Strategy<Value = Spec> {
        prop_oneof![
            (any::<u8>(), prop::option::of(0u8..3)).prop_map(|(t, d)| Spec::Create(t, d)),
            (prop::option::of(0u8..2), 0u8..3).prop_map(|(n, d)| Spec::DocAdd(n, d)),
            any::<u8>().prop_map(Spec::Title),
            (0u8..3).prop_map(Spec::Status),
            prop::option::of(any::<u8>()).prop_map(Spec::Parent),
            (0u8..3).prop_map(Spec::RepoAdd),
            ((0u8..3), cites()).prop_map(|(r, c)| Spec::RepoRemove(r, c)),
            Just(Spec::Delete),
        ]
    }

    /// One spec with its stamp inputs: `(spec, wall_ms, counter, actor)`.
    type Stamped = (Spec, u64, u32, bool);

    /// A random log: each spec stamped from a small clock range so HLC
    /// ties (broken by actor) are common, with ops that would repeat a
    /// `(hlc, actor)` stamp dropped — one actor's clock never issues the
    /// same stamp twice. The second vector drives a permutation of it.
    fn log() -> impl Strategy<Value = (Vec<Stamped>, Vec<Index>)> {
        (
            prop::collection::vec(
                (
                    prop_oneof![workspace_spec(), project_spec()],
                    0u64..3,
                    0u32..3,
                    any::<bool>(),
                ),
                1..24,
            ),
            prop::collection::vec(any::<Index>(), 24),
        )
    }

    fn category(c: u8) -> StateCategory {
        [
            StateCategory::Backlog,
            StateCategory::Unstarted,
            StateCategory::Started,
            StateCategory::Completed,
            StateCategory::Canceled,
        ][c as usize]
    }

    fn status(s: u8) -> ProjectStatus {
        [
            ProjectStatus::InProgress,
            ProjectStatus::Complete,
            ProjectStatus::Abandoned,
        ][s as usize]
    }

    /// Pick the cited add-tags: the earlier ops that added `value` to the
    /// set `is_add` describes, sampled by `cites`.
    fn observed(
        earlier: &[Op],
        value: &str,
        is_add: impl Fn(&Payload) -> Option<&str>,
        cites: &[Index],
    ) -> Vec<Ulid> {
        let adds: Vec<Ulid> = earlier
            .iter()
            .filter(|o| is_add(&o.payload) == Some(value))
            .map(|o| o.op_id)
            .collect();
        if adds.is_empty() {
            return Vec::new();
        }
        let mut tags: Vec<Ulid> = cites.iter().map(|i| adds[i.index(adds.len())]).collect();
        tags.sort();
        tags.dedup();
        tags
    }

    /// Stamp the specs into ops for `entity`; the workspace and project
    /// halves are folded by different views, so they are split here.
    fn build(entity: Ulid, specs: &[Stamped]) -> (Vec<Op>, Vec<Op>) {
        let actors = [ActorId::new("a"), ActorId::new("b")];
        let mut seen = std::collections::BTreeSet::new();
        let (mut workspace, mut project) = (Vec::new(), Vec::new());
        for (spec, wall, counter, who) in specs {
            let stamp = Stamp::new(Hlc::new(*wall, *counter), actors[usize::from(*who)].clone());
            if !seen.insert(stamp.clone()) {
                continue;
            }
            let label = |l: &u8| format!("l{l}");
            let payload = match spec {
                Spec::Prefix(p) => Payload::WorkspaceSet(WorkspaceSet::Prefix(p.to_string())),
                Spec::GateAdd(l) => Payload::WorkspaceSet(WorkspaceSet::GateLabelAdd(label(l))),
                Spec::GateRemove(l, cites) => {
                    let label = label(l);
                    let observed = observed(
                        &workspace,
                        &label,
                        |p| match p {
                            Payload::WorkspaceSet(WorkspaceSet::GateLabelAdd(l)) => Some(l),
                            _ => None,
                        },
                        cites,
                    );
                    Payload::WorkspaceSet(WorkspaceSet::GateLabelRemove { label, observed })
                }
                Spec::ModelLabel(l, m) => Payload::WorkspaceSet(WorkspaceSet::ModelLabel {
                    label: label(l),
                    model: m.map(|m| m.to_string()),
                }),
                Spec::Template(t) => Payload::WorkspaceSet(WorkspaceSet::TemplateSections(
                    t.iter().map(u8::to_string).collect(),
                )),
                Spec::Stale(d) => Payload::WorkspaceSet(WorkspaceSet::StaleDays(*d)),
                Spec::State(n, c, p) => Payload::StateUpsert(StateUpsert {
                    name: format!("s{n}"),
                    category: category(*c),
                    position: *p,
                }),
                Spec::Actor(i, agent) => Payload::ActorUpsert(ActorUpsert {
                    id: ActorId::new(format!("actor{i}")),
                    kind: if *agent {
                        ActorKind::Agent
                    } else {
                        ActorKind::Human
                    },
                }),
                // A small pool of doc ids, so two slots and two bindings
                // of one slot collide often.
                Spec::Create(t, d) => Payload::ProjectCreate(ProjectCreate {
                    id: "p".into(),
                    title: t.to_string(),
                    status: ProjectStatus::InProgress,
                    parent: None,
                    doc_id: d.map(|d| Ulid::from_parts(0, u128::from(d))),
                }),
                Spec::DocAdd(n, d) => Payload::ProjectDocAdd(crate::op::ProjectDocAdd {
                    name: n.map(|n| format!("doc{n}")),
                    doc_id: Ulid::from_parts(0, u128::from(*d)),
                }),
                Spec::Title(t) => Payload::ProjectSet(ProjectSet::Title(t.to_string())),
                Spec::Status(s) => Payload::ProjectSet(ProjectSet::Status(status(*s))),
                Spec::Parent(p) => {
                    Payload::ProjectSet(ProjectSet::Parent(p.map(|p| p.to_string())))
                }
                Spec::RepoAdd(r) => Payload::ProjectSet(ProjectSet::RepoAdd(label(r))),
                Spec::RepoRemove(r, cites) => {
                    let repo = label(r);
                    let observed = observed(
                        &project,
                        &repo,
                        |p| match p {
                            Payload::ProjectSet(ProjectSet::RepoAdd(r)) => Some(r),
                            _ => None,
                        },
                        cites,
                    );
                    Payload::ProjectSet(ProjectSet::RepoRemove { repo, observed })
                }
                Spec::Delete => Payload::ProjectDelete,
            };
            let op = Op::new(Ulid::new(), stamp.hlc, stamp.actor, entity, payload);
            if matches!(
                op.payload,
                Payload::ProjectCreate(_)
                    | Payload::ProjectSet(_)
                    | Payload::ProjectDelete
                    | Payload::ProjectDocAdd(_)
            ) {
                project.push(op);
            } else {
                workspace.push(op);
            }
        }
        (workspace, project)
    }

    /// Fisher–Yates driven by the generated indices, so a shrunk case is
    /// still a permutation.
    fn permute<T: Clone>(ops: &[T], perm: &[Index]) -> Vec<T> {
        let mut out = ops.to_vec();
        for i in (1..out.len()).rev() {
            let j = perm[i].index(i + 1);
            out.swap(i, j);
        }
        out
    }

    /// The property both views must satisfy: folding the log forward,
    /// reversed, or in a random permutation — applying every op twice
    /// along the way — gives one view.
    fn converges<V: Clone + PartialEq + std::fmt::Debug>(
        new: impl Fn() -> V,
        fold: impl Fn(&mut V, &Op) -> Result<(), ConfigApplyError>,
        ops: &[Op],
        perm: &[Index],
    ) -> Result<(), TestCaseError> {
        let mut forward = new();
        for op in ops {
            fold(&mut forward, op).unwrap();
        }
        let mut twice = new();
        for op in ops.iter().rev() {
            fold(&mut twice, op).unwrap();
            fold(&mut twice, op).unwrap();
        }
        let mut shuffled = new();
        for op in permute(ops, perm) {
            fold(&mut shuffled, &op).unwrap();
        }
        prop_assert_eq!(&forward, &twice, "reverse order + double apply");
        prop_assert_eq!(&forward, &shuffled, "random permutation");
        Ok(())
    }

    proptest! {
        #[test]
        fn config_folds_are_idempotent_and_order_independent((specs, perm) in log()) {
            let id = Ulid::new();
            let (workspace, project) = build(id, &specs);
            converges(|| WorkspaceView::new(id), apply_workspace, &workspace, &perm)?;
            converges(|| ProjectView::new(id), apply_project, &project, &perm)?;
        }
    }
}
