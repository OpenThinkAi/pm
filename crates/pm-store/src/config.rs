//! The configuration tables: workspace + states, projects + docs. These
//! are not op-logged in v1 (no op kind mutates them, README §Op log lists
//! ticket ops only), so they are written directly and are not rebuilt from
//! the log.

use std::collections::{BTreeMap, BTreeSet};

use pm_core::{Project, State, Workspace};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};

use crate::Store;
use crate::codec::{enum_from_name, enum_name, from_json, json, ulid};
use crate::error::{Result, StoreError};

impl Store {
    /// Writes the workspace and its states, replacing whatever was there.
    /// A state that tickets still reference cannot be dropped (foreign
    /// key), so renaming a state in use fails.
    pub fn init_workspace(&mut self, ws: &Workspace) -> Result<()> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute(
            "INSERT INTO workspace (singleton, id, prefix, gate_labels, model_labels, template_sections, stale_days)
             VALUES (1, ?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(singleton) DO UPDATE SET
               id = excluded.id, prefix = excluded.prefix, gate_labels = excluded.gate_labels,
               model_labels = excluded.model_labels, template_sections = excluded.template_sections,
               stale_days = excluded.stale_days",
            params![
                ws.id.to_string(),
                ws.prefix,
                json(&ws.gate_labels),
                json(&ws.model_labels),
                json(&ws.template_sections),
                ws.stale_days,
            ],
        )?;
        let keep: Vec<String> = ws.states.iter().map(|s| json(&s.name)).collect();
        tx.execute(
            "DELETE FROM state WHERE name NOT IN (SELECT value FROM json_each(?1))",
            params![format!("[{}]", keep.join(","))],
        )?;
        for state in &ws.states {
            tx.execute(
                "INSERT INTO state (name, category, position) VALUES (?1, ?2, ?3)
                 ON CONFLICT(name) DO UPDATE SET category = excluded.category, position = excluded.position",
                params![state.name, enum_name(&state.category), state.position],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn workspace(&self) -> Result<Option<Workspace>> {
        let row = self
            .conn
            .query_row(
                "SELECT id, prefix, gate_labels, model_labels, template_sections, stale_days FROM workspace",
                [],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, String>(3)?,
                        r.get::<_, String>(4)?,
                        r.get::<_, u32>(5)?,
                    ))
                },
            )
            .optional()?;
        let Some((id, prefix, gate_labels, model_labels, template_sections, stale_days)) = row
        else {
            return Ok(None);
        };
        Ok(Some(Workspace {
            id: ulid("workspace.id", &id)?,
            prefix,
            states: states(&self.conn)?,
            gate_labels: from_json("workspace.gate_labels", &gate_labels)?,
            model_labels: from_json("workspace.model_labels", &model_labels)?,
            template_sections: from_json("workspace.template_sections", &template_sections)?,
            stale_days,
        }))
    }

    /// Inserts or replaces a project and its documents: a direct write, not
    /// op-logged (AGT-1335). Unlike [`crate::Store::create_project`]
    /// (AGT-1344, `pm project new`), this never assigns a design-doc
    /// `doc_id` — `project.doc` written this way stays a plain cached
    /// column with no `body.edit` history, so `pm doctor`'s replay leaves
    /// it alone (it only ever touches rows with a `doc_id`,
    /// `project.rs::replay_project_docs`) and `pm project edit` refuses it
    /// until the project is recreated through `pm project new`. This path
    /// exists for import and tests that construct a whole [`Project`] at
    /// once; the CLI's own `pm project new` always goes through
    /// `create_project` instead.
    pub fn put_project(&mut self, project: &Project) -> Result<()> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some(parent) = &project.parent
            && !project_exists(&tx, parent)?
        {
            return Err(StoreError::UnknownProject {
                project: parent.clone(),
            });
        }
        tx.execute(
            "INSERT INTO project (id, title, status, parent, repos, doc) VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(id) DO UPDATE SET title = excluded.title, status = excluded.status,
               parent = excluded.parent, repos = excluded.repos, doc = excluded.doc",
            params![
                project.id,
                project.title,
                enum_name(&project.status),
                project.parent,
                json(&project.repos),
                project.doc,
            ],
        )?;
        tx.execute(
            "DELETE FROM project_doc WHERE project = ?1",
            params![project.id],
        )?;
        for (name, body) in &project.documents {
            tx.execute(
                "INSERT INTO project_doc (project, name, body) VALUES (?1, ?2, ?3)",
                params![project.id, name, body],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn project(&self, id: &str) -> Result<Option<Project>> {
        let mut found = load_projects(&self.conn, "WHERE id = ?1", params![id])?;
        Ok(found.pop())
    }

    /// Sets a project's `status` directly, leaving everything else (title,
    /// parent, repos, docs) untouched — project metadata is a direct write,
    /// not op-logged (AGT-1335 module doc). Used by `pm archive --auto` to
    /// retire an idle project to `complete` (AGT-1351 AC1).
    pub fn set_project_status(&mut self, id: &str, status: pm_core::ProjectStatus) -> Result<()> {
        let n = self.conn.execute(
            "UPDATE project SET status = ?1 WHERE id = ?2",
            params![enum_name(&status), id],
        )?;
        if n == 0 {
            return Err(StoreError::UnknownProject {
                project: id.to_string(),
            });
        }
        Ok(())
    }

    /// Every project, by id.
    pub fn projects(&self) -> Result<Vec<Project>> {
        load_projects(&self.conn, "", [])
    }
}

pub(crate) fn states(conn: &Connection) -> Result<Vec<State>> {
    let mut stmt =
        conn.prepare("SELECT name, category, position FROM state ORDER BY position, name")?;
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, u32>(2)?,
        ))
    })?;
    rows.map(|row| {
        let (name, category, position) = row?;
        Ok(State {
            name,
            category: enum_from_name("state.category", category)?,
            position,
        })
    })
    .collect()
}

pub(crate) fn project_exists(conn: &Connection, id: &str) -> rusqlite::Result<bool> {
    conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM project WHERE id = ?1)",
        params![id],
        |r| r.get(0),
    )
}

fn load_projects(
    conn: &Connection,
    where_clause: &str,
    args: impl rusqlite::Params,
) -> Result<Vec<Project>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT id, title, status, parent, repos, doc FROM project {where_clause} ORDER BY id"
    ))?;
    let rows = stmt.query_map(args, |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, Option<String>>(3)?,
            r.get::<_, String>(4)?,
            r.get::<_, String>(5)?,
        ))
    })?;
    let mut docs =
        conn.prepare("SELECT name, body FROM project_doc WHERE project = ?1 ORDER BY name")?;
    let mut projects = Vec::new();
    for row in rows {
        let (id, title, status, parent, repos, doc) = row?;
        let documents: BTreeMap<String, String> = docs
            .query_map(params![id], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?;
        let repos: BTreeSet<String> = from_json("project.repos", &repos)?;
        projects.push(Project {
            id,
            title,
            status: enum_from_name("project.status", status)?,
            parent,
            repos,
            doc,
            documents,
        });
    }
    Ok(projects)
}
