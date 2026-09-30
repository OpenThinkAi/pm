//! Live updates (`docs/app-api.md` §`GET /events`) and the idle exit.
//!
//! The watcher owns one `Store` and reads `ops_since(head)` every
//! [`POLL`] — or at once when a handler nudges it after a commit — so an
//! op committed by *any* process (this API, a `pm set` in a terminal, a
//! `pm sync` pull) reaches every open stream well inside a second. Each
//! stream is one SSE response: a `hello` with the head `seq` it starts
//! from, then one `op` event per op, and a comment every 15 s to keep the
//! connection alive. A stream that falls more than [`BUFFER`] ops behind
//! gets a `lagged` event and should refetch what it shows.
//!
//! Every open stream counts as a viewer ([`ViewerGuard`]); [`idle`]
//! resolves once none has been open for the grace period, and `pm app`
//! shuts down on it.

use std::convert::Infallible;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use axum::extract::State;
use axum::response::sse::{Event, KeepAlive, Sse};
use futures_util::stream::{self, Stream, StreamExt};
use pm_store::Store;
use serde::Serialize;
use tokio::sync::broadcast::error::RecvError;

use super::AppState;
use crate::exit::Result;
use crate::verbs::{SCHEMA, display_id};
use crate::workspace;

/// How often the watcher looks for new ops when nothing nudged it.
pub(crate) const POLL: Duration = Duration::from_millis(250);
/// Ops a stream may fall behind before it is told to resync.
pub(crate) const BUFFER: usize = 1024;

/// One committed op, as the stream announces it (never the payload —
/// `docs/cli-contract.md` §`pm log` keeps payloads out of JSON views).
#[derive(Clone, Debug, Serialize)]
pub(crate) struct OpEvent {
    pub schema: u32,
    /// The log's own sequence number: monotonic per replica.
    pub seq: i64,
    pub op_id: String,
    pub hlc: serde_json::Value,
    pub actor: String,
    pub kind: &'static str,
    /// The op's entity: a ticket ULID, or a project/document/workspace
    /// ULID for config and document ops.
    pub entity: String,
    /// The entity's display id when it is a ticket (`AGT-12`, `AGT-?`).
    pub id: Option<String>,
}

/// Ops past `since`, oldest first.
fn read_new(store: &Store, since: i64) -> Result<Vec<OpEvent>> {
    let ws = store.workspace()?;
    let mut out = Vec::new();
    for (seq, op) in store.ops_since(since)? {
        let id = match (&ws, store.ticket(op.entity)?) {
            (Some(ws), Some(t)) => Some(display_id(ws, &t)),
            _ => None,
        };
        out.push(OpEvent {
            schema: SCHEMA,
            seq,
            op_id: op.op_id.to_string(),
            hlc: serde_json::json!({ "wall_ms": op.hlc.wall_ms, "counter": op.hlc.counter }),
            actor: op.actor.as_str().to_string(),
            kind: op.kind(),
            entity: op.entity.to_string(),
            id,
        });
    }
    Ok(out)
}

/// The watcher task: polls the log from the head `pm app` started at and
/// broadcasts every new op. Runs until the runtime stops.
pub(crate) async fn watch(state: Arc<AppState>) {
    let mut store: Option<Store> = None;
    loop {
        tokio::select! {
            _ = tokio::time::sleep(POLL) => {}
            _ = state.nudge.notified() => {}
        }
        let since = state.head.load(Ordering::SeqCst);
        let dir = state.dir.clone();
        let conn = store.take();
        let read = tokio::task::spawn_blocking(move || {
            let store = match conn {
                Some(store) => store,
                None => workspace::open(&dir).map(|(store, _)| store)?,
            };
            let events = read_new(&store, since);
            Ok::<_, crate::exit::CliError>((store, events))
        })
        .await;
        match read {
            Ok(Ok((conn, Ok(events)))) => {
                store = Some(conn);
                for event in events {
                    state.head.store(event.seq, Ordering::SeqCst);
                    // No receiver is not an error: nobody is watching yet.
                    let _ = state.events.send(event);
                }
            }
            Ok(Ok((conn, Err(e)))) => {
                store = Some(conn);
                eprintln!("pm app: reading the op log: {:#}", e.error);
            }
            Ok(Err(e)) => eprintln!("pm app: opening the workspace: {:#}", e.error),
            Err(e) => eprintln!("pm app: op watcher: {e}"),
        }
    }
}

/// Counts one open event stream for as long as it lives.
struct ViewerGuard(Arc<AppState>);

impl ViewerGuard {
    fn new(state: Arc<AppState>) -> Self {
        state.viewers.fetch_add(1, Ordering::SeqCst);
        state.connected_once.store(true, Ordering::SeqCst);
        state.viewers_changed.notify_one();
        ViewerGuard(state)
    }
}

impl Drop for ViewerGuard {
    fn drop(&mut self) {
        self.0.viewers.fetch_sub(1, Ordering::SeqCst);
        self.0.viewers_changed.notify_one();
    }
}

/// Resolves once no event stream has been open for `grace` since the
/// last one closed — or for `startup` from start, before any has opened.
/// Never resolves when `grace` is zero (`--idle 0`).
pub(crate) async fn idle(state: Arc<AppState>, startup: Duration, grace: Duration) {
    if grace.is_zero() {
        std::future::pending::<()>().await;
    }
    loop {
        if state.viewers.load(Ordering::SeqCst) > 0 {
            state.viewers_changed.notified().await;
            continue;
        }
        let wait = if state.connected_once.load(Ordering::SeqCst) {
            grace
        } else {
            startup
        };
        tokio::select! {
            _ = tokio::time::sleep(wait) => {
                if state.viewers.load(Ordering::SeqCst) == 0 {
                    return;
                }
            }
            _ = state.viewers_changed.notified() => {}
        }
    }
}

fn event(name: &str, data: &impl Serialize) -> Event {
    Event::default()
        .event(name)
        .json_data(data)
        .unwrap_or_else(|e| Event::default().event("error").data(e.to_string()))
}

/// `GET /events`: the SSE stream.
pub(crate) async fn events(
    State(state): State<Arc<AppState>>,
) -> Sse<impl Stream<Item = std::result::Result<Event, Infallible>> + Send> {
    let receiver = state.events.subscribe();
    let guard = ViewerGuard::new(state.clone());
    let hello = event(
        "hello",
        &serde_json::json!({ "schema": SCHEMA, "seq": state.head.load(Ordering::SeqCst) }),
    );
    let ops = stream::unfold((receiver, guard), |(mut receiver, guard)| async move {
        let next = match receiver.recv().await {
            Ok(op) => event("op", &op).id(op.seq.to_string()),
            Err(RecvError::Lagged(missed)) => event(
                "lagged",
                &serde_json::json!({ "schema": SCHEMA, "missed": missed }),
            ),
            Err(RecvError::Closed) => return None,
        };
        Some((Ok(next), (receiver, guard)))
    });
    Sse::new(stream::once(async { Ok(hello) }).chain(ops)).keep_alive(
        KeepAlive::new()
            .interval(Duration::from_secs(15))
            .text("keep-alive"),
    )
}
