//! Ticket descriptions and project docs as a text CRDT (spike O1, AGT-1338).
//!
//! [`Body`] wraps a [Loro](https://loro.dev) document holding one text
//! container. The `body.edit` op payload **is** a [`BodyUpdate`]: the raw
//! bytes Loro exports for the edit (`ExportMode::Updates`), opaque to the op
//! log and to pm-hub. Applying the same set of updates in any order on any
//! replica yields the same text, which is what the README's Description row
//! ("ops carry CRDT updates, materialized text is cached") relies on.
//!
//! Why Loro rather than yrs is recorded in the vault at
//! `projects/pm/research/text-crdt-spike.md`; the numbers there come from
//! `examples/text_crdt_bench.rs` in this crate.
//!
//! Loro's peer id plays the role of the CRDT actor. Callers that own an
//! actor id (the op log does) should use [`Body::with_peer`] so two replicas
//! of the same actor never collide; [`Body::new`] draws a random peer id.

use std::fmt;

use loro::{ExportMode, LoroDoc, LoroText, UpdateOptions};

/// The single text container inside every body document.
const TEXT_ID: &str = "body";

/// An encoded Loro update: the `body.edit` op payload.
///
/// Produced by [`Body::diff_from_text`], consumed by [`Body::apply`]. The
/// bytes are Loro's self-describing export blob, so a full snapshot from
/// [`Body::snapshot`] is also a valid `BodyUpdate`.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct BodyUpdate(Vec<u8>);

impl BodyUpdate {
    /// Wraps bytes read back from the op log; no validation happens until
    /// [`Body::apply`].
    pub fn from_bytes(bytes: Vec<u8>) -> Self {
        Self(bytes)
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    pub fn into_bytes(self) -> Vec<u8> {
        self.0
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl AsRef<[u8]> for BodyUpdate {
    fn as_ref(&self) -> &[u8] {
        &self.0
    }
}

impl From<Vec<u8>> for BodyUpdate {
    fn from(bytes: Vec<u8>) -> Self {
        Self(bytes)
    }
}

impl From<BodyUpdate> for Vec<u8> {
    fn from(update: BodyUpdate) -> Self {
        update.0
    }
}

impl fmt::Debug for BodyUpdate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "BodyUpdate({} bytes)", self.0.len())
    }
}

/// Failures crossing the CRDT boundary. The wrapped crate's error types are
/// deliberately not exposed so swapping the library stays a one-module change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BodyError {
    /// The bytes were not a Loro update/snapshot this version can import.
    Import(String),
    /// Loro failed to encode an update or snapshot.
    Export(String),
    /// Computing the text diff failed.
    Diff(String),
    /// The peer id could not be set (only possible on a doc with pending
    /// uncommitted changes, which `Body` never leaves behind).
    Peer(String),
}

impl fmt::Display for BodyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BodyError::Import(e) => write!(f, "body: cannot import update: {e}"),
            BodyError::Export(e) => write!(f, "body: cannot export update: {e}"),
            BodyError::Diff(e) => write!(f, "body: cannot diff text: {e}"),
            BodyError::Peer(e) => write!(f, "body: cannot set peer id: {e}"),
        }
    }
}

impl std::error::Error for BodyError {}

/// A ticket or project-doc body: a text CRDT replica.
pub struct Body {
    doc: LoroDoc,
}

impl Body {
    /// An empty body with a random peer id. Prefer [`Body::with_peer`] when an
    /// actor id is at hand.
    pub fn new() -> Self {
        Self {
            doc: LoroDoc::new(),
        }
    }

    /// An empty body whose edits are attributed to `peer` (the CRDT actor).
    pub fn with_peer(peer: u64) -> Result<Self, BodyError> {
        let doc = LoroDoc::new();
        doc.set_peer_id(peer)
            .map_err(|e| BodyError::Peer(e.to_string()))?;
        Ok(Self { doc })
    }

    /// The peer id this replica stamps on its edits.
    pub fn peer(&self) -> u64 {
        self.doc.peer_id()
    }

    /// The materialized text.
    pub fn text(&self) -> String {
        self.text_handle().to_string()
    }

    /// Applies an update (or snapshot) produced by any replica. Idempotent
    /// and order-independent: an update whose causal dependencies have not
    /// arrived yet is queued and applied once they do.
    pub fn apply(&mut self, update: &BodyUpdate) -> Result<(), BodyError> {
        self.apply_awaiting(update).map(|_| ())
    }

    /// [`Body::apply`], reporting whether part of `update` is still queued
    /// behind causal dependencies this replica has not seen (Loro's
    /// `ImportStatus::pending`). That queue lives only in memory — a
    /// [`Body::snapshot`] leaves it out — so a caller that persists the
    /// body between updates must not keep one that returns `true`: the
    /// queued part would be lost (AGT-1413).
    pub fn apply_awaiting(&mut self, update: &BodyUpdate) -> Result<bool, BodyError> {
        self.doc
            .import(update.as_bytes())
            .map(|status| status.pending.is_some())
            .map_err(|e| BodyError::Import(e.to_string()))
    }

    /// Replaces the body with `new` by the minimal edit Loro can find,
    /// applies it locally, and returns the update to log as `body.edit`.
    /// This is the `$EDITOR` save path: a whole document comes back and only
    /// the changed span travels.
    pub fn diff_from_text(&mut self, new: &str) -> Result<BodyUpdate, BodyError> {
        let before = self.doc.oplog_vv();
        self.text_handle()
            .update(new, UpdateOptions::default())
            .map_err(|e| BodyError::Diff(e.to_string()))?;
        self.doc.commit();
        self.doc
            .export(ExportMode::updates(&before))
            .map(BodyUpdate)
            .map_err(|e| BodyError::Export(e.to_string()))
    }

    /// Everything this replica knows, as one blob: the materialized-cache
    /// form for pm-store. Feed it to [`Body::apply`] on a fresh `Body` (or to
    /// any replica, which will simply learn what it was missing).
    pub fn snapshot(&self) -> Result<BodyUpdate, BodyError> {
        self.doc
            .export(ExportMode::Snapshot)
            .map(BodyUpdate)
            .map_err(|e| BodyError::Export(e.to_string()))
    }

    fn text_handle(&self) -> LoroText {
        self.doc.get_text(TEXT_ID)
    }
}

impl Default for Body {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for Body {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Body")
            .field("peer", &self.peer())
            .field("len", &self.text_handle().len_unicode())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn body(peer: u64) -> Body {
        Body::with_peer(peer).expect("fresh doc accepts a peer id")
    }

    #[test]
    fn body_is_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<Body>();
        assert_send_sync::<BodyUpdate>();
    }

    #[test]
    fn empty_body_has_empty_text() {
        let b = body(1);
        assert_eq!(b.text(), "");
        assert_eq!(b.peer(), 1);
    }

    #[test]
    fn diff_from_text_round_trips_through_apply() {
        let mut a = body(1);
        let u1 = a.diff_from_text("# Title\n\nFirst draft.").unwrap();
        let u2 = a.diff_from_text("# Title\n\nSecond draft.").unwrap();
        assert_eq!(a.text(), "# Title\n\nSecond draft.");

        let mut b = body(2);
        b.apply(&u1).unwrap();
        b.apply(&u2).unwrap();
        assert_eq!(b.text(), a.text());
    }

    #[test]
    fn diff_only_ships_the_changed_span() {
        let mut a = body(1);
        let base = "word ".repeat(400); // ~2 KB
        a.diff_from_text(&base).unwrap();
        let edited = base.replacen("word", "item", 1);
        let u = a.diff_from_text(&edited).unwrap();
        assert!(
            u.len() < 200,
            "a one-word change should not re-send the document, got {} bytes",
            u.len()
        );
        assert_eq!(a.text(), edited);
    }

    #[test]
    fn concurrent_edits_converge() {
        let mut a = body(1);
        let base = a.diff_from_text("alpha\nbeta\ngamma\n").unwrap();
        let mut b = body(2);
        b.apply(&base).unwrap();

        // Both replicas edit offline, in different places.
        let from_a = a.diff_from_text("ALPHA\nbeta\ngamma\n").unwrap();
        let from_b = b.diff_from_text("alpha\nbeta\ngamma\ndelta\n").unwrap();

        a.apply(&from_b).unwrap();
        b.apply(&from_a).unwrap();

        assert_eq!(a.text(), b.text(), "replicas must converge");
        assert_eq!(a.text(), "ALPHA\nbeta\ngamma\ndelta\n");
    }

    #[test]
    fn concurrent_edits_to_the_same_span_converge() {
        let mut a = body(1);
        let base = a.diff_from_text("the quick fox").unwrap();
        let mut b = body(2);
        b.apply(&base).unwrap();

        let from_a = a.diff_from_text("the quick brown fox").unwrap();
        let from_b = b.diff_from_text("the quick red fox").unwrap();
        a.apply(&from_b).unwrap();
        b.apply(&from_a).unwrap();

        assert_eq!(a.text(), b.text());
        let t = a.text();
        assert!(
            t.contains("brown") && t.contains("red"),
            "both edits survive: {t:?}"
        );
    }

    #[test]
    fn out_of_order_delivery_converges() {
        let mut a = body(1);
        let u1 = a.diff_from_text("one").unwrap();
        let u2 = a.diff_from_text("one two").unwrap();
        let u3 = a.diff_from_text("one two three").unwrap();

        let mut b = body(2);
        b.apply(&u3).unwrap(); // queued: depends on u1, u2
        b.apply(&u1).unwrap();
        assert_eq!(b.text(), "one");
        b.apply(&u2).unwrap(); // unblocks u3
        assert_eq!(b.text(), "one two three");
    }

    #[test]
    fn reapplying_an_update_is_idempotent() {
        let mut a = body(1);
        let u = a.diff_from_text("stable").unwrap();
        let mut b = body(2);
        b.apply(&u).unwrap();
        b.apply(&u).unwrap();
        b.apply(&u).unwrap();
        assert_eq!(b.text(), "stable");
    }

    #[test]
    fn snapshot_restores_and_keeps_merging() {
        let mut a = body(1);
        a.diff_from_text("v1").unwrap();
        a.diff_from_text("v1 v2").unwrap();
        let snap = a.snapshot().unwrap();

        let mut restored = body(3);
        restored.apply(&snap).unwrap();
        assert_eq!(restored.text(), "v1 v2");

        // Edits after the snapshot still merge with the original.
        let u = restored.diff_from_text("v1 v2 v3").unwrap();
        a.apply(&u).unwrap();
        assert_eq!(a.text(), "v1 v2 v3");
    }

    #[test]
    fn non_ascii_text_diffs_correctly() {
        let mut a = body(1);
        a.diff_from_text("héllo wörld — 你好").unwrap();
        let u = a.diff_from_text("héllo wörld — 你好 🌍").unwrap();
        let mut b = body(2);
        b.apply(&a.snapshot().unwrap()).unwrap();
        b.apply(&u).unwrap();
        assert_eq!(b.text(), "héllo wörld — 你好 🌍");
    }

    #[test]
    fn garbage_bytes_are_rejected() {
        let mut a = body(1);
        let err = a.apply(&BodyUpdate::from_bytes(b"not a loro blob".to_vec()));
        assert!(matches!(err, Err(BodyError::Import(_))), "{err:?}");
        assert_eq!(a.text(), "");
    }

    #[test]
    fn update_bytes_survive_the_op_log_round_trip() {
        let mut a = body(1);
        let u = a.diff_from_text("payload").unwrap();
        // What pm-store will do: store Vec<u8>, read Vec<u8> back.
        let stored: Vec<u8> = u.clone().into_bytes();
        let reloaded = BodyUpdate::from_bytes(stored);
        assert_eq!(reloaded, u);
        let mut b = body(2);
        b.apply(&reloaded).unwrap();
        assert_eq!(b.text(), "payload");
    }
}
