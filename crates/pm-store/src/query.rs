//! Reads: plain queries over the materialized tables. Nothing here
//! touches the op log except [`Store::ops`], which reads it back verbatim.

use std::collections::BTreeSet;

use pm_core::{
    ActorId, Comment, Hlc, Hold, NotBefore, Op, Parked, Relation, Ticket, TicketView, Waiver,
};
use rusqlite::types::Value;
use rusqlite::{Connection, Row, params, params_from_iter};
use ulid::Ulid;

use crate::Store;
use crate::codec::{enum_from_name, from_json, hlc, opt_from_json, opt_hlc, ulid};
use crate::error::{Result, StoreError};

/// Which tickets [`Store::tickets`] returns. Every field left unset
/// matches all tickets; set fields must all match. Tombstoned tickets are
/// never listed.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TicketFilter {
    pub state: Option<String>,
    pub project: Option<String>,
    pub label: Option<String>,
    pub repo: Option<String>,
    pub assignee: Option<ActorId>,
    /// Only tickets with a hold set.
    pub held: bool,
}

impl Store {
    /// A ticket by ULID, tombstoned or not.
    pub fn ticket(&self, id: Ulid) -> Result<Option<Ticket>> {
        Ok(self
            .load_tickets("WHERE t.id = ?1", vec![Value::from(id.to_string())])?
            .pop())
    }

    /// A ticket by human number, tombstoned or not.
    pub fn ticket_by_number(&self, number: u64) -> Result<Option<Ticket>> {
        Ok(self
            .load_tickets("WHERE t.number = ?1", vec![Value::from(number as i64)])?
            .pop())
    }

    /// Live tickets matching `filter`: numbered ones first in number
    /// order, then unnumbered (`AGT-?`) ones in creation order.
    pub fn tickets(&self, filter: &TicketFilter) -> Result<Vec<Ticket>> {
        // Plain `?` placeholders bind in order, so each clause pushes its
        // value as it is added.
        let mut clauses = vec!["t.deleted = 0"];
        let mut args: Vec<Value> = Vec::new();
        if let Some(state) = &filter.state {
            clauses.push("t.state = ?");
            args.push(Value::from(state.clone()));
        }
        if let Some(project) = &filter.project {
            clauses.push("t.project = ?");
            args.push(Value::from(project.clone()));
        }
        if let Some(repo) = &filter.repo {
            clauses.push("t.repo = ?");
            args.push(Value::from(repo.clone()));
        }
        if let Some(assignee) = &filter.assignee {
            clauses.push("t.assignee = ?");
            args.push(Value::from(assignee.to_string()));
        }
        if let Some(label) = &filter.label {
            clauses.push(
                "EXISTS (SELECT 1 FROM ticket_label l WHERE l.ticket = t.id AND l.label = ?)",
            );
            args.push(Value::from(label.clone()));
        }
        if filter.held {
            clauses
                .push("EXISTS (SELECT 1 FROM marker m WHERE m.ticket = t.id AND m.kind = 'hold')");
        }
        self.load_tickets(&format!("WHERE {}", clauses.join(" AND ")), args)
    }

    /// The ticket's merge state — what a caller needs to build a
    /// `label.remove` / `relation.remove` (the observed add tags).
    pub fn ticket_view(&self, id: Ulid) -> Result<Option<TicketView>> {
        crate::commit::load_view(&self.conn, id)
    }

    /// Comments on a ticket in HLC order.
    pub fn comments(&self, ticket: Ulid) -> Result<Vec<Comment>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, ticket, author, hlc_wall_ms, hlc_counter, body FROM comment
             WHERE ticket = ?1 ORDER BY hlc_wall_ms, hlc_counter, author, id",
        )?;
        let rows = stmt.query_map(params![ticket.to_string()], |r| {
            Ok((
                r.get::<_, String>("id")?,
                r.get::<_, String>("ticket")?,
                r.get::<_, String>("author")?,
                hlc(r, "hlc_wall_ms", "hlc_counter")?,
                r.get::<_, String>("body")?,
            ))
        })?;
        rows.map(|row| {
            let (id, ticket, author, hlc, body) = row?;
            Ok(Comment {
                id: ulid("comment.id", &id)?,
                ticket: ulid("comment.ticket", &ticket)?,
                author: ActorId::new(author),
                hlc,
                body,
            })
        })
        .collect()
    }

    /// Every relation with `ticket` at either end, whichever ticket's op
    /// added it.
    pub fn relations(&self, ticket: Ulid) -> Result<Vec<Relation>> {
        let mut stmt = self.conn.prepare(
            "SELECT DISTINCT kind, from_ticket, to_ticket FROM relation
             WHERE from_ticket = ?1 OR to_ticket = ?1 ORDER BY kind, from_ticket, to_ticket",
        )?;
        let rows = stmt.query_map(params![ticket.to_string()], |r| {
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

    /// The ticket's ops in the order this replica appended them.
    pub fn ops(&self, ticket: Ulid) -> Result<Vec<Op>> {
        read_ops(&self.conn, "WHERE entity = ?1", params![ticket.to_string()])
            .map(|ops| ops.into_iter().map(|(_, op)| op).collect())
    }

    fn load_tickets(&self, where_sql: &str, args: Vec<Value>) -> Result<Vec<Ticket>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT id, number, title, state, priority, project, repo, assignee, description,
                    created_wall_ms, created_counter, updated_wall_ms, updated_counter,
                    archived_wall_ms, archived_counter, deleted, linked_github, linked_pr,
                    linear, source, ext
             FROM ticket t {where_sql}
             ORDER BY t.number IS NULL, t.number, t.created_wall_ms, t.created_counter, t.id"
        ))?;
        let rows = stmt.query_map(params_from_iter(args), TicketRow::read)?;
        rows.map(|row| row?.into_ticket(&self.conn)).collect()
    }
}

/// Ops matching `where_sql` in `seq` order, each with its `seq`.
pub(crate) fn read_ops(
    conn: &Connection,
    where_sql: &str,
    args: impl rusqlite::Params,
) -> Result<Vec<(i64, Op)>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT seq, op_id, hlc_wall_ms, hlc_counter, actor, entity, kind, payload, version
         FROM ops {where_sql} ORDER BY seq"
    ))?;
    let rows = stmt.query_map(args, |r| {
        let envelope = serde_json::json!({
            "op_id": r.get::<_, String>("op_id")?,
            "hlc": hlc(r, "hlc_wall_ms", "hlc_counter")?,
            "actor": r.get::<_, String>("actor")?,
            "entity": r.get::<_, String>("entity")?,
            "kind": r.get::<_, String>("kind")?,
            "version": r.get::<_, u16>("version")?,
        });
        let payload: Option<String> = r.get("payload")?;
        Ok((r.get::<_, i64>("seq")?, envelope, payload))
    })?;
    rows.map(|row| {
        let (seq, mut envelope, payload) = row?;
        if let Some(payload) = payload {
            envelope["payload"] = from_json("ops.payload", &payload)?;
        }
        let op =
            serde_json::from_value(envelope).map_err(|e| StoreError::corrupt("ops row")(&e))?;
        Ok((seq, op))
    })
    .collect()
}

/// One `ticket` row as SQLite hands it over, before decoding.
struct TicketRow {
    id: String,
    number: Option<i64>,
    title: String,
    state: String,
    priority: String,
    project: Option<String>,
    repo: Option<String>,
    assignee: Option<String>,
    description: String,
    created: Hlc,
    updated: Hlc,
    archived_at: Option<Hlc>,
    deleted: bool,
    linked_github: Option<String>,
    linked_pr: Option<String>,
    linear: Option<String>,
    source: Option<String>,
    ext: String,
}

impl TicketRow {
    fn read(r: &Row<'_>) -> rusqlite::Result<Self> {
        Ok(TicketRow {
            id: r.get("id")?,
            number: r.get("number")?,
            title: r.get("title")?,
            state: r.get("state")?,
            priority: r.get("priority")?,
            project: r.get("project")?,
            repo: r.get("repo")?,
            assignee: r.get("assignee")?,
            description: r.get("description")?,
            created: hlc(r, "created_wall_ms", "created_counter")?,
            updated: hlc(r, "updated_wall_ms", "updated_counter")?,
            archived_at: opt_hlc(r, "archived_wall_ms", "archived_counter")?,
            deleted: r.get("deleted")?,
            linked_github: r.get("linked_github")?,
            linked_pr: r.get("linked_pr")?,
            linear: r.get("linear")?,
            source: r.get("source")?,
            ext: r.get("ext")?,
        })
    }

    fn into_ticket(self, conn: &Connection) -> Result<Ticket> {
        let labels: BTreeSet<String> = conn
            .prepare("SELECT label FROM ticket_label WHERE ticket = ?1")?
            .query_map(params![self.id], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        let markers: Vec<(String, String)> = conn
            .prepare("SELECT kind, data FROM marker WHERE ticket = ?1 ORDER BY kind, position")?
            .query_map(params![self.id], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?;
        let mut hold: Option<Hold> = None;
        let mut waivers: Vec<Waiver> = Vec::new();
        let mut not_before: Option<NotBefore> = None;
        let mut parked: Option<Parked> = None;
        for (kind, data) in markers {
            match kind.as_str() {
                "hold" => hold = Some(from_json("marker hold", &data)?),
                "waiver" => waivers.push(from_json("marker waiver", &data)?),
                "not_before" => not_before = Some(from_json("marker not_before", &data)?),
                "parked" => parked = Some(from_json("marker parked", &data)?),
                other => return Err(StoreError::corrupt("marker.kind")(&other)),
            }
        }
        Ok(Ticket {
            id: ulid("ticket.id", &self.id)?,
            number: self.number.map(|n| n as u64),
            title: self.title,
            state: self.state,
            priority: enum_from_name("ticket.priority", self.priority)?,
            project: self.project,
            repo: self.repo,
            assignee: self.assignee.map(ActorId::new),
            description: self.description,
            labels,
            created: self.created,
            updated: self.updated,
            archived_at: self.archived_at,
            deleted: self.deleted,
            linked_github: self.linked_github,
            linked_pr: self.linked_pr,
            linear: self.linear,
            source: opt_from_json("ticket.source", self.source)?,
            hold,
            waivers,
            not_before,
            parked,
            ext: from_json("ticket.ext", &self.ext)?,
        })
    }
}
