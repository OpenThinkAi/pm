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

/// Which tickets [`Store::tickets`] returns (projects/pm/README.md §CLI
/// verbs, `pm list`). Every field left empty/`None`/`false` matches all
/// tickets; set fields must all match (AND across fields). A value filter
/// (`state`, `project`, `label`, `repo`, `assignee`, `github`) that carries
/// several values matches any one of them (OR within the field) — the CLI
/// builds these from `--state a,b`-style comma lists. Tombstoned tickets
/// are never listed; archived tickets (`archived_at` set) are excluded
/// unless `archived` is true.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TicketFilter {
    pub state: Vec<String>,
    pub project: Vec<String>,
    pub label: Vec<String>,
    pub repo: Vec<String>,
    pub assignee: Vec<ActorId>,
    /// Only tickets with a hold set.
    pub held: bool,
    /// `linked_github` matches one of these URLs.
    pub github: Vec<String>,
    /// Case-insensitive substring match against title or description.
    pub search: Option<String>,
    /// Include archived tickets (`archived_at` set). By default they are
    /// excluded.
    pub archived: bool,
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
    /// order, then unnumbered (`AGT-?`) ones in creation order. Archived
    /// tickets (`archived_at` set) are excluded unless `filter.archived` is
    /// true — every caller, not just `pm list`, inherits that default; pass
    /// `TicketFilter { archived: true, .. }` for the full non-tombstoned set.
    pub fn tickets(&self, filter: &TicketFilter) -> Result<Vec<Ticket>> {
        // Plain `?` placeholders bind in order, so each clause pushes its
        // values as it is added.
        let mut clauses = vec!["t.deleted = 0".to_string()];
        let mut args: Vec<Value> = Vec::new();
        in_clause("t.state", &filter.state, &mut clauses, &mut args, |s| {
            Value::from(s.clone())
        });
        in_clause("t.project", &filter.project, &mut clauses, &mut args, |s| {
            Value::from(s.clone())
        });
        in_clause("t.repo", &filter.repo, &mut clauses, &mut args, |s| {
            Value::from(s.clone())
        });
        in_clause(
            "t.assignee",
            &filter.assignee,
            &mut clauses,
            &mut args,
            |a| Value::from(a.to_string()),
        );
        in_clause(
            "t.linked_github",
            &filter.github,
            &mut clauses,
            &mut args,
            |s| Value::from(s.clone()),
        );
        if !filter.label.is_empty() {
            let placeholders = placeholders(filter.label.len());
            clauses.push(format!(
                "EXISTS (SELECT 1 FROM ticket_label l WHERE l.ticket = t.id AND l.label IN ({placeholders}))"
            ));
            args.extend(filter.label.iter().map(|s| Value::from(s.clone())));
        }
        if filter.held {
            clauses.push(
                "EXISTS (SELECT 1 FROM marker m WHERE m.ticket = t.id AND m.kind = 'hold')"
                    .to_string(),
            );
        }
        if !filter.archived {
            clauses.push("t.archived_wall_ms IS NULL".to_string());
        }
        if let Some(search) = &filter.search {
            clauses.push(
                "(t.title LIKE ? ESCAPE '\\' OR t.description LIKE ? ESCAPE '\\')".to_string(),
            );
            let pattern = like_pattern(search);
            args.push(Value::from(pattern.clone()));
            args.push(Value::from(pattern));
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

    /// Tickets matching `tail_sql` — everything after `FROM ticket t`, so
    /// a `WHERE …`, optionally preceded by JOINs against the `t` alias —
    /// in the order [`Store::tickets`] documents.
    pub(crate) fn load_tickets(&self, tail_sql: &str, args: Vec<Value>) -> Result<Vec<Ticket>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT id, number, title, state, priority, project, repo, assignee, description,
                    created_wall_ms, created_counter, updated_wall_ms, updated_counter,
                    archived_wall_ms, archived_counter, deleted, linked_github, linked_pr,
                    linear, source, ext
             FROM ticket t {tail_sql}
             ORDER BY t.number IS NULL, t.number, t.created_wall_ms, t.created_counter, t.id"
        ))?;
        let rows = stmt.query_map(params_from_iter(args), TicketRow::read)?;
        rows.map(|row| row?.into_ticket(&self.conn)).collect()
    }
}

/// Pushes `column IN (?, ?, ...)` onto `clauses` and the encoded `values`
/// onto `args`, unless `values` is empty — an unset filter must match
/// everything, not nothing.
fn in_clause<T>(
    column: &str,
    values: &[T],
    clauses: &mut Vec<String>,
    args: &mut Vec<Value>,
    encode: impl Fn(&T) -> Value,
) {
    if values.is_empty() {
        return;
    }
    clauses.push(format!("{column} IN ({})", placeholders(values.len())));
    args.extend(values.iter().map(encode));
}

fn placeholders(n: usize) -> String {
    vec!["?"; n].join(", ")
}

/// A `LIKE` pattern that matches `text` as a substring, with the pattern's
/// own `%`, `_` and `\` escaped so user input can't smuggle in wildcards
/// (paired with `ESCAPE '\'` at the call site). SQLite's `LIKE` is already
/// case-insensitive for ASCII, so `pm list --search` needs no extra
/// case-folding.
fn like_pattern(text: &str) -> String {
    let escaped = text
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_");
    format!("%{escaped}%")
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
