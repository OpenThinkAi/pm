//! What `pm import vault` needs from the store beyond the ordinary write
//! path (AGT-1347): the number allocator's floor, and project metadata
//! upserted from a vault snapshot without disturbing the project's
//! op-derived documents.
//!
//! Ticket content itself goes through [`Store::commit_batch`] like every
//! other writer — an import is just a large, backdated batch of ordinary
//! ops. Nothing here materializes a ticket row directly, and since
//! AGT-1385 project metadata is config ops too (`config.rs`).

use std::collections::BTreeSet;

use pm_core::{ActorId, ProjectKind, ProjectStatus};
use rusqlite::{OptionalExtension, TransactionBehavior, params};
use ulid::Ulid;

use crate::Store;
use crate::config::{load_project_view, upsert_meta_in};
use crate::error::{Result, StoreError};

impl Store {
    /// The allocator floor (`workspace.number_floor`): the next allocated
    /// number is always greater than this, whatever the tickets table
    /// holds.
    pub fn number_floor(&self) -> Result<u64> {
        let floor: Option<i64> = self
            .conn
            .query_row("SELECT number_floor FROM workspace", [], |r| r.get(0))
            .optional()?;
        Ok(floor.unwrap_or(0) as u64)
    }

    /// Raises the allocator floor to `floor` (never lowers it) and returns
    /// the floor in effect afterwards. After an import this is
    /// `max(imported number, current)`, so pm never re-issues a number
    /// the vault minted.
    pub fn raise_number_floor(&mut self, floor: u64) -> Result<u64> {
        self.conn.execute(
            "UPDATE workspace SET number_floor = MAX(number_floor, ?1)",
            params![floor as i64],
        )?;
        self.number_floor()
    }

    /// Creates or updates a project's metadata from a vault snapshot with
    /// config ops under `actor` (only what differs is committed) and
    /// returns its design-doc id, binding one (a `project.create`'s
    /// `doc_id`, or a `project.doc_add`, AGT-1413) if it has none. The
    /// document bodies (`doc`, named documents) are untouched: they are
    /// op-derived (AGT-1344) and the importer edits them through
    /// [`Store::commit_doc_edit`]. A `parent` must already exist (R2).
    pub fn upsert_project(
        &mut self,
        id: &str,
        title: &str,
        status: ProjectStatus,
        parent: Option<&str>,
        repos: &BTreeSet<String>,
        actor: &ActorId,
    ) -> Result<Ulid> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let project = upsert_meta_in(
            &tx,
            id,
            title,
            ProjectKind::Project,
            status,
            parent,
            repos,
            None,
            actor,
        )?;
        let doc_id = load_project_view(&tx, project)?
            .and_then(|view| view.design_doc_id())
            .ok_or(StoreError::UnknownProjectEntity { project })?;
        tx.commit()?;
        Ok(doc_id)
    }

    /// A named document's id, creating the (empty) document when the
    /// project has none by that name — [`Store::named_doc_id`] then
    /// [`Store::add_named_doc`], for a re-import that must land on the same
    /// `doc_id` every time. A new document is bound under `actor`.
    pub fn ensure_named_doc(&mut self, project: &str, name: &str, actor: &ActorId) -> Result<Ulid> {
        match self.named_doc_id(project, name)? {
            Some(id) => Ok(id),
            None => self.add_named_doc(project, name, actor),
        }
    }
}

#[cfg(test)]
mod tests {
    use pm_core::op::TicketCreate;
    use pm_core::{ActorId, Hlc, Op, Payload, Priority, State, StateCategory, Workspace};

    use super::*;

    fn fresh() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(dir.path().join("pm.sqlite")).unwrap();
        store
            .init_workspace(
                &Workspace {
                    id: Ulid::new(),
                    prefix: "AGT".into(),
                    states: vec![State {
                        name: "triage".into(),
                        category: StateCategory::Unstarted,
                        position: 0,
                    }],
                    gate_labels: Default::default(),
                    model_labels: Default::default(),
                    template_sections: Vec::new(),
                    stale_days: 30,
                    docs_owned_by: Default::default(),
                },
                &matt(),
            )
            .unwrap();
        (dir, store)
    }

    fn matt() -> ActorId {
        ActorId::new("matt")
    }

    fn create(ticket: Ulid) -> Op {
        Op::new(
            Ulid::new(),
            Hlc::new(1, 0),
            ActorId::new("matt"),
            ticket,
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

    /// AC5: after the floor is raised past the tickets table's maximum,
    /// the next allocation starts above the floor; the floor never lowers.
    #[test]
    fn allocation_respects_the_floor() {
        let (_dir, mut store) = fresh();
        assert_eq!(store.number_floor().unwrap(), 0);
        let actor = ActorId::new("matt");
        let a = Ulid::new();
        store.commit(&create(a)).unwrap();
        assert_eq!(store.allocate_number(a, &actor).unwrap(), 1);

        assert_eq!(store.raise_number_floor(1376).unwrap(), 1376);
        assert_eq!(store.raise_number_floor(10).unwrap(), 1376);
        let b = Ulid::new();
        store.commit(&create(b)).unwrap();
        assert_eq!(store.allocate_number(b, &actor).unwrap(), 1377);
        // The table's maximum still wins when it is above the floor.
        let c = Ulid::new();
        store.commit(&create(c)).unwrap();
        assert_eq!(store.allocate_number(c, &actor).unwrap(), 1378);
        // init_workspace on an existing row leaves the floor alone.
        let ws = store.workspace().unwrap().unwrap();
        store.init_workspace(&ws, &matt()).unwrap();
        assert_eq!(store.number_floor().unwrap(), 1376);
    }

    #[test]
    fn upsert_project_keeps_its_doc_id_and_documents() {
        let (_dir, mut store) = fresh();
        let repos: BTreeSet<String> = ["OpenThinkAi/pm".to_string()].into();
        let first = store
            .upsert_project("pm", "pm", ProjectStatus::InProgress, None, &repos, &matt())
            .unwrap();
        assert_eq!(store.design_doc_id("pm").unwrap(), Some(first));
        let doc = store.ensure_named_doc("pm", "notes", &matt()).unwrap();
        assert_eq!(store.ensure_named_doc("pm", "notes", &matt()).unwrap(), doc);

        let again = store
            .upsert_project(
                "pm",
                "pm (renamed)",
                ProjectStatus::Complete,
                None,
                &repos,
                &matt(),
            )
            .unwrap();
        assert_eq!(again, first, "a re-import lands on the same doc_id");
        let p = store.project("pm").unwrap().unwrap();
        assert_eq!(p.title, "pm (renamed)");
        assert_eq!(p.status, ProjectStatus::Complete);
        assert!(p.documents.contains_key("notes"));

        // put_project binds a design doc too (AGT-1413), and upsert keeps it.
        let old = store
            .put_project(
                &pm_core::Project {
                    kind: Default::default(),
                    id: "old".into(),
                    title: "old".into(),
                    status: ProjectStatus::InProgress,
                    parent: None,
                    repos: Default::default(),
                    doc: String::new(),
                    documents: Default::default(),
                },
                &matt(),
            )
            .unwrap();
        let _ = old;
        let before = store.design_doc_id("old").unwrap();
        assert!(before.is_some());
        let id = store
            .upsert_project(
                "old",
                "old",
                ProjectStatus::Abandoned,
                Some("pm"),
                &repos,
                &matt(),
            )
            .unwrap();
        assert_eq!(store.design_doc_id("old").unwrap(), Some(id));
        assert_eq!(before, Some(id));
        assert_eq!(
            store.project("old").unwrap().unwrap().parent.as_deref(),
            Some("pm")
        );

        let err = store
            .upsert_project(
                "x",
                "x",
                ProjectStatus::InProgress,
                Some("nope"),
                &repos,
                &matt(),
            )
            .unwrap_err();
        assert!(matches!(err, crate::StoreError::UnknownProject { .. }));
    }
}
