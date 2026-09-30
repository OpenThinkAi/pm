//! The snapshot `pm check` runs over (AGT-1342). The invariants themselves
//! are pure, in [`pm_core::check`]; this module only reads what they need.

use pm_core::{Finding, Relation, Ticket, Workspace};
use rusqlite::params;
use ulid::Ulid;

use crate::Store;
use crate::codec::{enum_from_name, ulid};
use crate::error::Result;

impl Store {
    /// Every ticket, tombstoned ones included (a dangling relation is one
    /// that points at a tombstone), in [`Store::tickets`] order.
    pub fn all_tickets(&self) -> Result<Vec<Ticket>> {
        self.load_tickets("", Vec::new())
    }

    /// Every relation in the database, deduplicated across owners, in
    /// `(kind, from, to)` order.
    pub fn all_relations(&self) -> Result<Vec<Relation>> {
        let mut stmt = self.conn.prepare(
            "SELECT DISTINCT kind, from_ticket, to_ticket FROM relation
             ORDER BY kind, from_ticket, to_ticket",
        )?;
        let rows = stmt.query_map(params![], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
            ))
        })?;
        rows.map(|row| {
            let (kind, from, to) = row?;
            Ok(Relation {
                kind: enum_from_name("relation.kind", kind)?,
                from: ulid("relation.from_ticket", &from)?,
                to: ulid("relation.to_ticket", &to)?,
            })
        })
        .collect()
    }

    /// Every live (not tombstoned, not archived) ticket filed in a project
    /// that has since been deleted, with that project's id (AGT-1464): the
    /// ticket's view still names it, its row reads `NULL` (a pulled
    /// `project.delete` detached it). In ticket id order.
    pub fn deleted_project_refs(&self) -> Result<Vec<(Ulid, String)>> {
        let mut stmt = self.conn.prepare(
            "SELECT t.id, json_extract(v.view, '$.project.value') FROM ticket t
             JOIN ticket_view v ON v.ticket = t.id
             WHERE t.project IS NULL AND t.deleted = 0 AND t.archived_wall_ms IS NULL
               AND json_extract(v.view, '$.project.value') IS NOT NULL
             ORDER BY t.id",
        )?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
        rows.map(|row| {
            let (id, project) = row?;
            Ok((ulid("ticket.id", &id)?, project))
        })
        .collect()
    }

    /// [`pm_core::check::check`] over this database, with its
    /// deleted-project references folded in
    /// ([`pm_core::check::with_deleted_projects`]). `now_ms` is the
    /// caller's clock reading (staleness is measured against it).
    pub fn check(
        &self,
        ws: &Workspace,
        now_ms: u64,
        project: Option<&str>,
    ) -> Result<Vec<Finding>> {
        let mut findings = pm_core::check::check(
            ws,
            &self.all_tickets()?,
            &self.all_relations()?,
            now_ms,
            project,
        );
        pm_core::check::with_deleted_projects(
            &mut findings,
            &self.deleted_project_refs()?,
            project,
        );
        Ok(findings)
    }
}
