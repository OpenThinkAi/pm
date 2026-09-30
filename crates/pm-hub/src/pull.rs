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
//! empty. `head` is the workspace's largest seq (0 for an empty log). Page
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
use axum::http::{StatusCode, Uri};
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
}

fn parse_query(uri: &Uri) -> Result<Cursor, PullError> {
    let Query(params) = Query::<HashMap<String, String>>::try_from_uri(uri)
        .map_err(|e| PullError::Query(e.body_text()))?;
    let mut cursor = Cursor {
        since: 0,
        limit: DEFAULT_PAGE_OPS,
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
            other => {
                return Err(PullError::Query(format!(
                    "unknown query parameter {other:?}; this route takes since and limit"
                )));
            }
        }
    }
    Ok(cursor)
}

pub async fn pull(State(db): State<Db>, caller: Authed, uri: Uri) -> Response {
    match pull_page(&db, &caller.workspace, &uri).await {
        Ok(body) => ([(CONTENT_TYPE, "application/json")], body).into_response(),
        Err(e) => e.into_response(),
    }
}

/// The response body, assembled from the stored op text.
async fn pull_page(db: &Db, workspace: &str, uri: &Uri) -> Result<String, PullError> {
    let cursor = parse_query(uri)?;
    // One statement, so the page and `head` are read from one snapshot.
    // With nothing past `since` the join yields one row of `(head, NULL,
    // NULL)`, which still carries `head`.
    let rows = db
        .reader
        .query(
            "SELECT h.head, p.seq, p.op
             FROM (SELECT coalesce(max(seq), 0) AS head FROM ops WHERE workspace_id = $1) AS h
             LEFT JOIN (SELECT seq, op::text AS op FROM ops
                        WHERE workspace_id = $1 AND seq > $2
                        ORDER BY seq LIMIT $3) AS p ON true
             ORDER BY p.seq",
            &[&workspace, &cursor.since, &cursor.limit],
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
                limit: DEFAULT_PAGE_OPS
            }
        );
        assert_eq!(
            parse("?since=42&limit=7").unwrap(),
            Cursor {
                since: 42,
                limit: 7
            }
        );
        assert_eq!(parse("?limit=1000000").unwrap().limit, MAX_PAGE_OPS);
        assert_eq!(parse("?limit=1000").unwrap().limit, MAX_PAGE_OPS);
        assert_eq!(parse("?limit=1").unwrap().limit, 1);
        assert_eq!(parse("?since=0").unwrap().since, 0);
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
            ("?kind=claim", "unknown query parameter", "\"kind\""),
        ] {
            let reason = parse(query).unwrap_err();
            assert!(reason.starts_with(expect), "{query}: {reason}");
            assert!(reason.contains(quoted), "{query}: {reason}");
        }
        // A bad value is rejected even next to good ones.
        assert!(parse("?since=1&limit=0").is_err());
    }
}
