//! `GET /w/{workspace}/ops?since=<seq>&limit=<n>`: a client pulls the ops
//! it has not seen yet, in seq order, a page at a time (AGT-1390, README
//! §Sync & hub).
//!
//! The response is `{"ops": [{"seq": <seq>, "op": <op>}, ...], "next":
//! <seq>, "head": <seq>}`. Each `op` is the op's JSON text exactly as the
//! hub stored it (`ops.op` is `json`, so `::text` gives the pushed bytes
//! back); this module never parses or re-serializes an op, it splices the
//! stored text into the array. Whether an op came from a client push or
//! was authored by the hub itself (number allocation, AGT-1391) makes no
//! difference here: every op in the log is served the same way.
//!
//! **Cursor.** `since` is the last seq the client has (0 to start); the
//! page holds the workspace's ops with `seq > since`, ascending, at most
//! `limit` of them. `next` is the seq to pass as `since` on the next
//! request: the last seq in the page, or `since` itself when the page is
//! empty. A page also stops before [`MAX_PAGE_BYTES`] of op text (always
//! serving at least one op), so `next < head` can mean the byte cap cut it
//! short (AGT-1467). `head` is the workspace's largest seq (0 for an empty log). Page
//! and `head` come from one statement, so they are one snapshot: `next <
//! head` means more ops were already waiting when this page was read;
//! `next >= head` means the client had everything as of that snapshot.
//! Because a push takes its seqs under the workspace lock and commits
//! before the next push can take any (`ops`), every seq at or below `head`
//! is committed when `head` is read, and an op with a seq at or below a
//! `next` the client has seen can never appear later; a client that pages
//! `since = next` misses nothing. Seqs are not contiguous (one sequence
//! serves every workspace, and a rolled-back push burns its values), so
//! nothing counts them.
//!
//! Pulls run on the reader connection and never wait for a push, which
//! holds the writer connection and the workspace row lock; a pull only
//! ever sees committed ops.

use std::collections::HashMap;
use std::fmt::Write as _;

use axum::Json;
use axum::extract::{Query, State};
use axum::http::header::CONTENT_TYPE;
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use serde::Serialize;

use crate::Db;
use crate::auth::Authed;

/// Ops per page when the request names no `limit`.
pub const DEFAULT_PAGE_OPS: i64 = 500;
/// Most ops in one page; a larger `limit` is clamped to this. It matches
/// `ops::MAX_BATCH_OPS`, so a client can pull no more per request than it
/// can push.
pub const MAX_PAGE_OPS: i64 = 1000;
/// Most bytes of op text in one page (16 MiB, AGT-1467), whatever
/// `limit` says: a page stops before the op that would take it over, so
/// the hub never buffers (and a client never receives) a thousand
/// document edits at once. A page always holds at least one op when any
/// is past `since`, so a single op larger than this (up to a push's
/// 64 MiB) is served on its own and the cursor still advances. `next` is
/// the last seq served, as for a count-capped page: a client that pages
/// until `next >= head` — every client, old or new — needs nothing else.
pub const MAX_PAGE_BYTES: i64 = 16 * 1024 * 1024;

/// The structured error body of a 400 from this route.
#[derive(Serialize)]
struct ErrorBody {
    error: &'static str,
    reason: String,
}

#[derive(Debug)]
enum PullError {
    Query(String),
    Db(tokio_postgres::Error),
}

impl From<tokio_postgres::Error> for PullError {
    fn from(e: tokio_postgres::Error) -> Self {
        PullError::Db(e)
    }
}

impl IntoResponse for PullError {
    fn into_response(self) -> Response {
        match self {
            PullError::Query(reason) => (
                StatusCode::BAD_REQUEST,
                Json(ErrorBody {
                    error: "invalid_query",
                    reason,
                }),
            )
                .into_response(),
            PullError::Db(e) => {
                eprintln!("pm-hub: pull: {e}");
                StatusCode::SERVICE_UNAVAILABLE.into_response()
            }
        }
    }
}

/// The validated query string: `since` (default 0) and `limit` (default
/// [`DEFAULT_PAGE_OPS`], clamped to [`MAX_PAGE_OPS`]).
#[derive(Debug, PartialEq, Eq)]
struct Cursor {
    since: i64,
    limit: i64,
    /// `kind=redact` (AGT-2427): only `redact` ops — the one-off fetch
    /// an upgraded client makes of every redact it may have missed.
    only_redacts: bool,
}

/// Which ops a page serves (AGT-2427, `redact`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kinds {
    /// Every op: the client sent `redact::ACCEPT_HEADER`.
    All,
    /// Every op but `redact`: a client that never said it understands
    /// them (an older build would fail its pull on one).
    NoRedacts,
    /// Only `redact` ops (`kind=redact`).
    OnlyRedacts,
}

impl Kinds {
    fn sql(self) -> &'static str {
        match self {
            Kinds::All => "all",
            Kinds::NoRedacts => "no-redacts",
            Kinds::OnlyRedacts => "only-redacts",
        }
    }
}

fn parse_query(uri: &Uri) -> Result<Cursor, PullError> {
    let Query(params) = Query::<HashMap<String, String>>::try_from_uri(uri)
        .map_err(|e| PullError::Query(e.body_text()))?;
    let mut cursor = Cursor {
        since: 0,
        limit: DEFAULT_PAGE_OPS,
        only_redacts: false,
    };
    for (name, value) in &params {
        match name.as_str() {
            "since" => {
                cursor.since = value
                    .parse::<i64>()
                    .ok()
                    .filter(|since| *since >= 0)
                    .ok_or_else(|| {
                        PullError::Query(format!(
                            "since must be a seq (an integer >= 0), got {value:?}"
                        ))
                    })?;
            }
            "limit" => {
                let limit = value
                    .parse::<i64>()
                    .ok()
                    .filter(|limit| *limit >= 1)
                    .ok_or_else(|| {
                        PullError::Query(format!(
                            "limit must be an integer >= 1 (at most {MAX_PAGE_OPS} is served), got {value:?}"
                        ))
                    })?;
                cursor.limit = limit.min(MAX_PAGE_OPS);
            }
            "kind" if value == "redact" => cursor.only_redacts = true,
            "kind" => {
                return Err(PullError::Query(format!(
                    "kind may only be \"redact\", got {value:?}"
                )));
            }
            other => {
                return Err(PullError::Query(format!(
                    "unknown query parameter {other:?}; this route takes since, limit and kind"
                )));
            }
        }
    }
    Ok(cursor)
}

pub async fn pull(State(db): State<Db>, caller: Authed, headers: HeaderMap, uri: Uri) -> Response {
    let accepts_redacts = headers
        .get_all(crate::redact::ACCEPT_HEADER)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .any(|kind| kind.trim() == crate::redact::ACCEPT_VALUE);
    match pull_page(&db, &caller.workspace, &uri, accepts_redacts).await {
        Ok(body) => ([(CONTENT_TYPE, "application/json")], body).into_response(),
        Err(e) => e.into_response(),
    }
}

/// The response body, assembled from the stored op text.
async fn pull_page(
    db: &Db,
    workspace: &str,
    uri: &Uri,
    accepts_redacts: bool,
) -> Result<String, PullError> {
    let cursor = parse_query(uri)?;
    let kinds = if cursor.only_redacts {
        Kinds::OnlyRedacts
    } else if accepts_redacts {
        Kinds::All
    } else {
        Kinds::NoRedacts
    };
    // One statement, so the page and `head` are read from one snapshot.
    // With nothing past `since` the join yields one row of `(head, NULL,
    // NULL)`, which still carries `head`. The byte cap ([`MAX_PAGE_BYTES`])
    // is applied in the database, over the running total of the first
    // `limit` ops' sizes, so only the ops served ever reach the hub: the
    // page is the longest prefix within the cap, and never empty
    // (`n = 1`). `octet_length` of a `json` value reads its stored size.
    let rows = db
        .reader
        .query(
            "SELECT h.head, p.seq, p.op
             FROM (SELECT coalesce(max(seq), 0) AS head FROM ops WHERE workspace_id = $1) AS h
             LEFT JOIN (
                 SELECT seq, op FROM (
                     SELECT seq, op::text AS op,
                            row_number() OVER (ORDER BY seq) AS n,
                            sum(octet_length(op::text)) OVER (ORDER BY seq) AS through
                     FROM (SELECT seq, op FROM ops
                           WHERE workspace_id = $1 AND seq > $2
                             AND CASE $5::text
                                     WHEN 'no-redacts' THEN kind <> 'redact'
                                     WHEN 'only-redacts' THEN kind = 'redact'
                                     ELSE true
                                 END
                           ORDER BY seq LIMIT $3) AS l
                 ) AS w
                 WHERE n = 1 OR through <= $4
             ) AS p ON true
             ORDER BY p.seq",
            &[
                &workspace,
                &cursor.since,
                &cursor.limit,
                &MAX_PAGE_BYTES,
                &kinds.sql(),
            ],
        )
        .await?;
    let head: i64 = rows.first().map_or(0, |row| row.get(0));
    let mut next = cursor.since;
    let mut body = String::from(r#"{"ops":["#);
    for (i, row) in rows.iter().enumerate() {
        let Some(seq) = row.get::<_, Option<i64>>(1) else {
            break;
        };
        let op: &str = row.get(2);
        if i > 0 {
            body.push(',');
        }
        write!(body, r#"{{"seq":{seq},"op":{op}}}"#).expect("writing to a String");
        next = seq;
    }
    // AGT-2427: a page left empty only because its ops were all withheld
    // `redact`s moves the cursor past them (still within `head`, and an
    // empty page may move `next` that far), or a client could never get
    // beyond a run of them.
    if next == cursor.since && kinds == Kinds::NoRedacts && head > cursor.since {
        let skipped: Option<i64> = db
            .reader
            .query_one(
                "SELECT max(seq) FROM (SELECT seq FROM ops
                                       WHERE workspace_id = $1 AND seq > $2
                                       ORDER BY seq LIMIT $3) AS s",
                &[&workspace, &cursor.since, &cursor.limit],
            )
            .await?
            .get(0);
        next = skipped.unwrap_or(next).min(head);
    }
    write!(body, r#"],"next":{next},"head":{head}}}"#).expect("writing to a String");
    Ok(body)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(query: &str) -> Result<Cursor, String> {
        let uri: Uri = format!("/w/saltline/ops{query}").parse().unwrap();
        parse_query(&uri).map_err(|e| match e {
            PullError::Query(reason) => reason,
            PullError::Db(_) => unreachable!(),
        })
    }

    #[test]
    fn defaults_parses_and_clamps() {
        assert_eq!(
            parse("").unwrap(),
            Cursor {
                since: 0,
                limit: DEFAULT_PAGE_OPS,
                only_redacts: false,
            }
        );
        assert_eq!(
            parse("?since=42&limit=7").unwrap(),
            Cursor {
                since: 42,
                limit: 7,
                only_redacts: false,
            }
        );
        assert_eq!(parse("?limit=1000000").unwrap().limit, MAX_PAGE_OPS);
        assert_eq!(parse("?limit=1000").unwrap().limit, MAX_PAGE_OPS);
        assert_eq!(parse("?limit=1").unwrap().limit, 1);
        assert_eq!(parse("?since=0").unwrap().since, 0);
        assert!(parse("?kind=redact").unwrap().only_redacts);
        assert!(parse("?kind=ticket.create").is_err());
    }

    #[test]
    fn rejects_bad_values_and_unknown_parameters() {
        // Each reason names the rule and quotes the offending value.
        for (query, expect, quoted) in [
            ("?since=-1", "since must be a seq", "\"-1\""),
            ("?since=x", "since must be a seq", "\"x\""),
            ("?since=", "since must be a seq", "\"\""),
            (
                "?since=99999999999999999999",
                "since must be a seq",
                "\"9999",
            ),
            ("?limit=0", "limit must be an integer >= 1", "\"0\""),
            ("?limit=-5", "limit must be an integer >= 1", "\"-5\""),
            ("?limit=ten", "limit must be an integer >= 1", "\"ten\""),
            ("?kind=claim", "kind may only be", "\"claim\""),
            ("?order=desc", "unknown query parameter", "\"order\""),
        ] {
            let reason = parse(query).unwrap_err();
            assert!(reason.starts_with(expect), "{query}: {reason}");
            assert!(reason.contains(quoted), "{query}: {reason}");
        }
        // A bad value is rejected even next to good ones.
        assert!(parse("?since=1&limit=0").is_err());
    }
}
