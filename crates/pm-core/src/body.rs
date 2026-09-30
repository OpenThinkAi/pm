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

use loro::{ExportMode, LoroDoc, LoroText, VersionVector};

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

    /// Replaces the body with `new`, applies the edit locally, and returns
    /// the update to log as `body.edit`. This is the `$EDITOR` save path: a
    /// whole document comes back and only the changed spans travel.
    ///
    /// The edit is computed **line-wise** (see [`line_diff`]): lines are
    /// aligned first, and only lines that changed are diffed by character.
    /// A purely character-level diff (Loro's `LoroText::update`, used until
    /// AGT-1429) is free to anchor an unchanged character to a different
    /// line — appending `\nC.` after `B.` can come out as "insert `.\nC`
    /// before the old `.`" — and a concurrent edit to line `B` then merges
    /// onto the new line instead. Aligning lines first keeps every edit on
    /// the line it was made to. Only how an edit is computed changes: the
    /// update is still ordinary Loro text ops, so stored bodies and updates
    /// from older builds stay fully compatible.
    pub fn diff_from_text(&mut self, new: &str) -> Result<BodyUpdate, BodyError> {
        let before = self.doc.oplog_vv();
        let text = self.text_handle();
        let old = text.to_string();
        line_diff::apply(&text, &old, new).map_err(|e| BodyError::Diff(e.to_string()))?;
        debug_assert_eq!(
            text.to_string(),
            new,
            "line diff must reproduce the saved text"
        );
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

    /// This replica's version: its oplog version vector in Loro's own
    /// encoding (`VersionVector.encode()` in `loro-crdt`), which names
    /// every op it holds. Hand it to [`Body::updates_since`] on another
    /// replica to learn what this one is missing.
    pub fn version(&self) -> Vec<u8> {
        self.doc.oplog_vv().encode()
    }

    /// Every op this replica holds that `version` (an encoded version
    /// vector, [`Body::version`]'s bytes) does not cover, as one update.
    /// A version that is *ahead* of this replica for some peer is fine —
    /// those ops are simply not included — so an editor may send its own
    /// version, unsent local edits and all. Empty `version` means "from
    /// the beginning".
    pub fn updates_since(&self, version: &[u8]) -> Result<BodyUpdate, BodyError> {
        let from = if version.is_empty() {
            VersionVector::default()
        } else {
            VersionVector::decode(version).map_err(|e| BodyError::Import(e.to_string()))?
        };
        self.doc
            .export(ExportMode::updates(&from))
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

/// The line-aligned diff behind [`Body::diff_from_text`] (AGT-1429).
///
/// Loro ships a line diff too (`LoroText::update_by_line`), but it is not
/// used: it treats a line *with* its trailing `\n` as the unit, so the last
/// line of a body without a trailing newline (`B.`) differs from the same
/// line once something is appended after it (`B.\n`) and gets deleted and
/// re-inserted whole — two replicas that both append then each keep a copy
/// of `B.`. It also replaces changed lines wholesale, so two edits to
/// different words of one line would both survive as duplicate lines.
///
/// Here a text is its `\n`-separated lines (`"A.\nB."` is `["A.", "B."]`,
/// and a trailing newline is a final empty line), which makes the last line
/// compare equal however the text ends. The algorithm:
///
/// 1. Align old and new lines (common prefix/suffix, then Myers on the
///    rest). Unchanged lines are never touched.
/// 2. Each run of changed lines (a hunk) pairs old lines with new lines:
///    one-to-one when the counts match; otherwise the shorter side pairs
///    with the head or the tail of the longer, whichever shares more
///    leading/trailing characters, and the leftover lines are inserted or
///    deleted whole. So "edit line B and add D after it" is an in-line edit
///    of `B` plus a new line, not a character diff that could anchor `B`'s
///    final `.` to `D`.
/// 3. Paired lines are diffed by character (Myers again), so concurrent
///    edits to different words of one line still both land in that line.
///
/// Whole-line inserts and deletes carry their separator on the side that
/// keeps them attached to their neighbours: `X\n` before the following line,
/// or `\nX` after the last line.
///
/// Both Myers passes run under a step budget (not a clock, so the result is
/// deterministic): when a region is too large and too different to align
/// within it (on the order of a thousand scattered changed lines), it is
/// replaced wholesale, which is correct, just coarser.
/// Unchanged prefixes and suffixes are trimmed before the budget applies,
/// so a small edit to a very large body stays a small update.
mod line_diff {
    use std::collections::HashMap;
    use std::fmt;

    use loro::LoroText;

    /// Upper bound on the edit distance Myers searches for (a changed line
    /// costs 2: one delete, one insert). The trace it keeps for
    /// backtracking is ~`MAX_D²` `u32`s (16 MB at 2048).
    const MAX_D: usize = 2048;

    /// Upper bound on the steps (diagonals probed plus elements compared)
    /// one Myers run may take before giving up; ~0.2 s in a release build.
    const WORK: u64 = 1 << 28;

    /// A Loro text op failed while replaying the diff.
    #[derive(Debug)]
    pub(super) struct ApplyError(String);

    impl fmt::Display for ApplyError {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str(&self.0)
        }
    }

    /// One text edit in **old** text coordinates (unicode scalar values).
    /// Edits are produced in non-decreasing `pos` order and replayed in that
    /// order with a running offset.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub(super) enum Edit {
        Delete { pos: usize, len: usize },
        Insert { pos: usize, text: String },
    }

    /// Edits `text` (whose content is `old`) into `new`.
    pub(super) fn apply(text: &LoroText, old: &str, new: &str) -> Result<(), ApplyError> {
        let mut offset: isize = 0;
        for edit in edits(old, new) {
            match edit {
                Edit::Delete { pos, len } => {
                    let at = (pos as isize + offset) as usize;
                    text.delete(at, len)
                        .map_err(|e| ApplyError(e.to_string()))?;
                    offset -= len as isize;
                }
                Edit::Insert { pos, text: s } => {
                    let at = (pos as isize + offset) as usize;
                    text.insert(at, &s).map_err(|e| ApplyError(e.to_string()))?;
                    offset += s.chars().count() as isize;
                }
            }
        }
        Ok(())
    }

    /// A text split into `\n`-separated lines, with each line's starting
    /// offset in unicode scalar values.
    struct Lines<'a> {
        lines: Vec<&'a str>,
        starts: Vec<usize>,
        total: usize,
    }

    impl<'a> Lines<'a> {
        fn new(s: &'a str) -> Self {
            let lines: Vec<&str> = s.split('\n').collect();
            let mut starts = Vec::with_capacity(lines.len());
            let mut at = 0;
            for line in &lines {
                starts.push(at);
                at += line.chars().count() + 1;
            }
            Self {
                lines,
                starts,
                total: at - 1,
            }
        }

        fn len(&self) -> usize {
            self.lines.len()
        }
    }

    /// The edits turning `old` into `new`, line-aligned (see the module docs).
    pub(super) fn edits(old: &str, new: &str) -> Vec<Edit> {
        let mut out = Vec::new();
        if old == new {
            return out;
        }
        let old = Lines::new(old);
        let new = Lines::new(new);

        // Intern lines so the line-level Myers compares integers.
        let mut ids: HashMap<&str, u32> = HashMap::new();
        let mut interned = [Vec::new(), Vec::new()];
        for (side, lines) in interned.iter_mut().zip([&old.lines, &new.lines]) {
            side.reserve(lines.len());
            for &line in lines.iter() {
                let next = ids.len() as u32;
                side.push(*ids.entry(line).or_insert(next));
            }
        }
        let [old_ids, new_ids] = interned;

        let (pairs, aligned) = matching(&old_ids, &new_ids);
        // Past the alignment budget the changed middle is replaced
        // wholesale rather than pairing thousands of unrelated lines
        // character by character.
        let emit = if aligned { hunk } else { replace_lines };
        let (mut i, mut j) = (0, 0);
        for (mi, mj) in pairs {
            if mi > i || mj > j {
                emit(&old, &new, (i, mi - i), (j, mj - j), &mut out);
            }
            i = mi + 1;
            j = mj + 1;
        }
        if i < old.len() || j < new.len() {
            emit(&old, &new, (i, old.len() - i), (j, new.len() - j), &mut out);
        }
        out
    }

    /// Emits the edits replacing old lines `[i, i + k)` with new lines
    /// `[j, j + l)`.
    fn hunk(
        old: &Lines,
        new: &Lines,
        (i, k): (usize, usize),
        (j, l): (usize, usize),
        out: &mut Vec<Edit>,
    ) {
        let pair = |oi: usize, nj: usize, out: &mut Vec<Edit>| {
            chars(old.lines[oi], new.lines[nj], old.starts[oi], out);
        };
        if k == 0 {
            insert_lines(old, i, &new.lines[j..j + l], out);
        } else if l == 0 {
            delete_lines(old, i, k, out);
        } else if k == l {
            for t in 0..k {
                pair(i + t, j + t, out);
            }
        } else {
            let p = k.min(l);
            let head: usize = (0..p)
                .map(|t| affinity(old.lines[i + t], new.lines[j + t]))
                .sum();
            let tail: usize = (0..p)
                .map(|t| affinity(old.lines[i + k - p + t], new.lines[j + l - p + t]))
                .sum();
            if head >= tail {
                for t in 0..p {
                    pair(i + t, j + t, out);
                }
                if l > k {
                    insert_lines(old, i + k, &new.lines[j + k..j + l], out);
                } else {
                    delete_lines(old, i + l, k - l, out);
                }
            } else {
                if l > k {
                    insert_lines(old, i, &new.lines[j..j + l - k], out);
                } else {
                    delete_lines(old, i, k - l, out);
                }
                for t in 0..p {
                    pair(i + k - p + t, j + l - p + t, out);
                }
            }
        }
    }

    /// Replaces old lines `[i, i + k)` with new lines `[j, j + l)` as one
    /// delete and one insert, keeping the separators around them.
    fn replace_lines(
        old: &Lines,
        new: &Lines,
        (i, k): (usize, usize),
        (j, l): (usize, usize),
        out: &mut Vec<Edit>,
    ) {
        if k == 0 || l == 0 {
            return hunk(old, new, (i, k), (j, l), out);
        }
        let pos = old.starts[i];
        let end = old.starts[i + k - 1] + old.lines[i + k - 1].chars().count();
        out.push(Edit::Delete {
            pos,
            len: end - pos,
        });
        out.push(Edit::Insert {
            pos: end,
            text: new.lines[j..j + l].join("\n"),
        });
    }

    /// Inserts whole `lines` before old line `at`, or after the last line
    /// when `at` is past the end.
    fn insert_lines(old: &Lines, at: usize, lines: &[&str], out: &mut Vec<Edit>) {
        let joined = lines.join("\n");
        if at < old.len() {
            out.push(Edit::Insert {
                pos: old.starts[at],
                text: joined + "\n",
            });
        } else {
            out.push(Edit::Insert {
                pos: old.total,
                text: format!("\n{joined}"),
            });
        }
    }

    /// Deletes old lines `[at, at + count)` with one separator: the one
    /// after them, or the one before when they run to the end of the text.
    fn delete_lines(old: &Lines, at: usize, count: usize, out: &mut Vec<Edit>) {
        if at + count < old.len() {
            let pos = old.starts[at];
            out.push(Edit::Delete {
                pos,
                len: old.starts[at + count] - pos,
            });
        } else {
            // Some line survives (a text is never zero lines), so a run that
            // reaches the end cannot also start at line 0.
            debug_assert!(at > 0, "deleting every line of a text");
            let pos = old.starts[at].saturating_sub(1);
            out.push(Edit::Delete {
                pos,
                len: old.total - pos,
            });
        }
    }

    /// How alike two lines look at their ends: common leading plus common
    /// trailing characters (not overlapping). Used only to choose which
    /// lines of an uneven hunk to pair.
    fn affinity(a: &str, b: &str) -> usize {
        let prefix = a.chars().zip(b.chars()).take_while(|(x, y)| x == y).count();
        let room = a.chars().count().min(b.chars().count()) - prefix;
        let suffix = a
            .chars()
            .rev()
            .zip(b.chars().rev())
            .take(room)
            .take_while(|(x, y)| x == y)
            .count();
        prefix + suffix
    }

    /// Character-level edits turning line `a` (starting at old offset
    /// `base`) into line `b`.
    fn chars(a: &str, b: &str, base: usize, out: &mut Vec<Edit>) {
        if a == b {
            return;
        }
        let a: Vec<char> = a.chars().collect();
        let b: Vec<char> = b.chars().collect();
        let (mut i, mut j) = (0, 0);
        let gap = |i: usize, mi: usize, j: usize, mj: usize, out: &mut Vec<Edit>| {
            if mi > i {
                out.push(Edit::Delete {
                    pos: base + i,
                    len: mi - i,
                });
            }
            if mj > j {
                out.push(Edit::Insert {
                    pos: base + mi,
                    text: b[j..mj].iter().collect(),
                });
            }
        };
        // Past the budget only the common prefix/suffix match and the
        // rest of the line is replaced, which is what the gap walk emits.
        for (mi, mj) in matching(&a, &b).0 {
            gap(i, mi, j, mj, out);
            i = mi + 1;
            j = mj + 1;
        }
        gap(i, a.len(), j, b.len(), out);
    }

    /// Pairs `(i, j)` with `a[i] == b[j]` forming a longest common
    /// subsequence, in increasing order, and `true`; or, past the work
    /// budget, just the common prefix and suffix and `false`.
    pub(super) fn matching<T: Eq>(a: &[T], b: &[T]) -> (Vec<(usize, usize)>, bool) {
        let prefix = a.iter().zip(b).take_while(|(x, y)| x == y).count();
        let suffix = a[prefix..]
            .iter()
            .rev()
            .zip(b[prefix..].iter().rev())
            .take_while(|(x, y)| x == y)
            .count();
        let (a_mid, b_mid) = (&a[prefix..a.len() - suffix], &b[prefix..b.len() - suffix]);

        let mut pairs: Vec<(usize, usize)> = (0..prefix).map(|t| (t, t)).collect();
        let mut aligned = true;
        if !a_mid.is_empty() && !b_mid.is_empty() {
            match myers(a_mid, b_mid) {
                Some(mid) => {
                    pairs.extend(mid.into_iter().map(|(x, y)| (x + prefix, y + prefix)));
                }
                None => aligned = false,
            }
        }
        pairs.extend((0..suffix).map(|t| (a.len() - suffix + t, b.len() - suffix + t)));
        (pairs, aligned)
    }

    /// Myers' greedy O((n + m) · d) LCS, giving up (`None`) past edit
    /// distance [`MAX_D`] or [`WORK`] steps.
    fn myers<T: Eq>(a: &[T], b: &[T]) -> Option<Vec<(usize, usize)>> {
        let (n, m) = (a.len() as isize, b.len() as isize);
        let max_d = MAX_D.min(a.len() + b.len()) as isize;
        let off = max_d + 1;
        let idx = |k: isize| (off + k) as usize;
        let mut v = vec![0isize; (2 * max_d + 3) as usize];
        // trace[d - 1] holds diagonals -(d - 1)..=(d - 1) as they stood
        // before round d, which is what backtracking round d reads.
        let mut trace: Vec<Vec<u32>> = Vec::new();
        let mut work: u64 = 0;
        for d in 0..=max_d {
            if d > 0 {
                trace.push(
                    v[idx(-(d - 1))..=idx(d - 1)]
                        .iter()
                        .map(|&x| x as u32)
                        .collect(),
                );
            }
            let mut k = -d;
            while k <= d {
                let mut x = if k == -d || (k != d && v[idx(k - 1)] < v[idx(k + 1)]) {
                    v[idx(k + 1)]
                } else {
                    v[idx(k - 1)] + 1
                };
                let mut y = x - k;
                let from = x;
                while x < n && y < m && a[x as usize] == b[y as usize] {
                    x += 1;
                    y += 1;
                }
                work += 1 + (x - from) as u64;
                v[idx(k)] = x;
                if x >= n && y >= m {
                    return Some(backtrack(&trace, d, n, m));
                }
                k += 2;
            }
            if work > WORK {
                return None;
            }
        }
        None
    }

    fn backtrack(trace: &[Vec<u32>], d_end: isize, n: isize, m: isize) -> Vec<(usize, usize)> {
        let (mut x, mut y) = (n, m);
        let mut pairs = Vec::new();
        for d in (1..=d_end).rev() {
            let v = &trace[(d - 1) as usize];
            let at = |k: isize| v[(k + d - 1) as usize] as isize;
            let k = x - y;
            let prev_k = if k == -d || (k != d && at(k - 1) < at(k + 1)) {
                k + 1
            } else {
                k - 1
            };
            let prev_x = at(prev_k);
            let prev_y = prev_x - prev_k;
            while x > prev_x && y > prev_y {
                x -= 1;
                y -= 1;
                pairs.push((x as usize, y as usize));
            }
            x = prev_x;
            y = prev_y;
        }
        while x > 0 && y > 0 {
            x -= 1;
            y -= 1;
            pairs.push((x as usize, y as usize));
        }
        pairs.reverse();
        pairs
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
    fn updates_since_ships_only_what_the_other_side_lacks() {
        let mut server = body(1);
        server.diff_from_text("one\ntwo").unwrap();
        let mut editor = body(2);
        editor.apply(&server.snapshot().unwrap()).unwrap();

        // The editor edits locally (not yet sent) while the server moves on.
        editor.diff_from_text("ONE\ntwo").unwrap();
        server.diff_from_text("one\ntwo\nthree").unwrap();

        // The editor's version is ahead of the server for peer 2 and behind
        // for peer 1: the server ships exactly its own new op.
        let catch_up = server.updates_since(&editor.version()).unwrap();
        editor.apply(&catch_up).unwrap();
        assert_eq!(editor.text(), "ONE\ntwo\nthree");

        // Nothing new once the editor is caught up: an update that applies
        // cleanly and changes nothing.
        let nothing = server.updates_since(&server.version()).unwrap();
        let before = editor.text();
        assert!(!editor.apply_awaiting(&nothing).unwrap());
        assert_eq!(editor.text(), before);

        // Empty means everything; garbage is an import error.
        let mut fresh = body(3);
        fresh.apply(&server.updates_since(&[]).unwrap()).unwrap();
        assert_eq!(fresh.text(), server.text());
        assert!(matches!(
            server.updates_since(&[0xff, 0xff, 0xff]),
            Err(BodyError::Import(_))
        ));
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

    /// Two replicas start from `base`, save `left` and `right` offline,
    /// then exchange updates both ways. Returns the converged text.
    fn merge3(base: &str, left: &str, right: &str) -> String {
        let mut a = body(1);
        let b0 = a.diff_from_text(base).unwrap();
        let mut b = body(2);
        b.apply(&b0).unwrap();

        let from_a = a.diff_from_text(left).unwrap();
        let from_b = b.diff_from_text(right).unwrap();
        a.apply(&from_b).unwrap();
        b.apply(&from_a).unwrap();
        assert_eq!(a.text(), b.text(), "replicas must converge");
        a.text()
    }

    #[test]
    fn gate_repro_each_edit_lands_on_its_own_line() {
        // P3 gate finding 1 (AGT-1429, scratch AGT-114): the char-level diff
        // merged this as "A, y.\nB.\nC, x.\nD.".
        let merged = merge3("A.\nB.", "A, y.\nB.\nC.", "A.\nB, x.\nD.");
        assert!(
            merged == "A, y.\nB, x.\nC.\nD." || merged == "A, y.\nB, x.\nD.\nC.",
            "{merged:?}"
        );
    }

    #[test]
    fn gate_repro_holds_with_crlf_line_endings() {
        let merged = merge3("A.\r\nB.", "A, y.\r\nB.\r\nC.", "A.\r\nB, x.\r\nD.");
        assert!(
            merged == "A, y.\r\nB, x.\r\nC.\r\nD." || merged == "A, y.\r\nB, x.\r\nD.\r\nC.",
            "{merged:?}"
        );
    }

    #[test]
    fn appends_without_a_trailing_newline_merge_as_whole_lines() {
        let merged = merge3("A.\nB.", "A.\nB.\nC.", "A.\nB.\nD.");
        assert!(
            merged == "A.\nB.\nC.\nD." || merged == "A.\nB.\nD.\nC.",
            "{merged:?}"
        );
    }

    #[test]
    fn appends_with_a_trailing_newline_merge_as_whole_lines() {
        let merged = merge3("A.\nB.\n", "A.\nB.\nC.\n", "A.\nB.\nD.\n");
        assert!(
            merged == "A.\nB.\nC.\nD.\n" || merged == "A.\nB.\nD.\nC.\n",
            "{merged:?}"
        );
    }

    #[test]
    fn different_words_of_one_line_both_land_in_that_line() {
        let merged = merge3(
            "title\nthe cat sat\nend",
            "title\nthe big cat sat\nend",
            "title\nthe cat sat down\nend",
        );
        assert_eq!(merged, "title\nthe big cat sat down\nend");
    }

    #[test]
    fn same_spot_of_one_line_keeps_both_edits_on_that_line() {
        let merged = merge3("a\nfox\nz", "a\nred fox\nz", "a\nbrown fox\nz");
        let lines: Vec<&str> = merged.lines().collect();
        assert_eq!(lines.len(), 3, "{merged:?}");
        assert_eq!((lines[0], lines[2]), ("a", "z"));
        assert!(
            lines[1].contains("red") && lines[1].contains("brown") && lines[1].ends_with("fox"),
            "{merged:?}"
        );
    }

    #[test]
    fn insert_at_top_and_bottom_merge() {
        let merged = merge3("m1\nm2\n", "top\nm1\nm2\n", "m1\nm2\nbottom\n");
        assert_eq!(merged, "top\nm1\nm2\nbottom\n");
        let merged = merge3("m1\nm2", "top\nm1\nm2", "m1\nm2\nbottom");
        assert_eq!(merged, "top\nm1\nm2\nbottom");
    }

    #[test]
    fn edit_line_next_to_a_deleted_line() {
        // One side deletes line b, the other edits line c: both apply, and
        // the neighbours are untouched.
        let merged = merge3("a\nb\nc\nd", "a\nc\nd", "a\nb\nc, edited\nd");
        assert_eq!(merged, "a\nc, edited\nd");
        // Deleting the last line while the other side edits the one above.
        let merged = merge3("a\nb\nc", "a\nb", "a\nb, edited\nc");
        assert_eq!(merged, "a\nb, edited");
    }

    #[test]
    fn delete_line_vs_edit_same_line_keeps_the_edit() {
        // The deleted line's text goes; the concurrent insert into it has
        // nowhere else to be and survives (no edit is lost) — the CRDT
        // analogue of a conflict, not silent loss.
        let merged = merge3("a\nb\nc", "a\nc", "a\nb2\nc");
        assert!(merged.contains('2'), "{merged:?}");
        assert!(
            merged.starts_with("a\n") && merged.ends_with('c'),
            "{merged:?}"
        );
        assert!(!merged.contains('b'), "{merged:?}");
    }

    #[test]
    fn edits_to_empty_and_back() {
        let mut a = body(1);
        a.diff_from_text("").unwrap();
        assert_eq!(a.text(), "");
        a.diff_from_text("x\ny").unwrap();
        assert_eq!(a.text(), "x\ny");
        a.diff_from_text("\n").unwrap();
        assert_eq!(a.text(), "\n");
        a.diff_from_text("").unwrap();
        assert_eq!(a.text(), "");
    }

    #[test]
    fn unchanged_lines_are_not_touched() {
        // Replacing one line's text wholesale must not disturb its
        // neighbours even when they share characters with it.
        let edits = line_diff::edits("A.\nB.", "A.\nB.\nC.");
        assert_eq!(
            edits,
            vec![line_diff::Edit::Insert {
                pos: 5,
                text: "\nC.".into()
            }]
        );
        let edits = line_diff::edits("A.\nB.", "A.\nB, x.\nD.");
        assert_eq!(
            edits,
            vec![
                line_diff::Edit::Insert {
                    pos: 4,
                    text: ", x".into()
                },
                line_diff::Edit::Insert {
                    pos: 5,
                    text: "\nD.".into()
                },
            ]
        );
    }

    #[test]
    fn large_body_small_edit_stays_small() {
        let base: String = (0..50_000)
            .map(|i| format!("line {i} of a long body\n"))
            .collect();
        let mut a = body(1);
        a.diff_from_text(&base).unwrap();
        let edited =
            base.replacen("line 25000 of", "line 25000 (edited) of", 1) + "one more line\n";
        let u = a.diff_from_text(&edited).unwrap();
        assert!(u.len() < 300, "got {} bytes", u.len());
        assert_eq!(a.text(), edited);
    }

    #[test]
    fn myers_over_budget_falls_back_to_a_correct_replacement() {
        // Two unrelated large texts: alignment gives up, the result is
        // still exact.
        let old: String = (0..40_000).map(|i| format!("{i}\n")).collect();
        let new: String = (0..40_000).map(|i| format!("{}\n", i * 7 + 3)).collect();
        let mut a = body(1);
        a.diff_from_text(&old).unwrap();
        a.diff_from_text(&new).unwrap();
        assert_eq!(a.text(), new);
    }

    mod props {
        use super::*;
        use proptest::prelude::*;

        fn text() -> impl Strategy<Value = String> {
            proptest::collection::vec(
                prop_oneof![
                    Just('a'),
                    Just('b'),
                    Just('.'),
                    Just(' '),
                    Just('\n'),
                    Just('\n'),
                    Just('\r'),
                    Just('é'),
                    Just('🌍'),
                ],
                0..40,
            )
            .prop_map(|cs| cs.into_iter().collect())
        }

        proptest! {
            #[test]
            fn diff_reproduces_the_saved_text(old in text(), new in text()) {
                let mut a = body(1);
                let base = a.diff_from_text(&old).unwrap();
                prop_assert_eq!(a.text(), old.clone());
                let u = a.diff_from_text(&new).unwrap();
                prop_assert_eq!(a.text(), new.clone());

                // A replica replaying the two updates lands on the same text.
                let mut b = body(2);
                b.apply(&base).unwrap();
                b.apply(&u).unwrap();
                prop_assert_eq!(b.text(), new);
            }

            #[test]
            fn disjoint_line_edits_merge_exactly(
                n in 1usize..12,
                picks in proptest::collection::vec(0u8..3, 12),
                words in proptest::collection::vec("[a-z ,.]{0,8}", 12),
                trailing_newline in any::<bool>(),
            ) {
                // Each base line is unique; each side rewrites its own
                // lines (0 = untouched, 1 = left, 2 = right) to text that
                // keeps a piece of the original, so pairing is exercised.
                let base: Vec<String> = (0..n).map(|i| format!("line {i}.")).collect();
                let edit = |i: usize, who: &str| format!("line {i}{} {who}.", words[i]);
                let side = |who: u8, tag: &str| -> Vec<String> {
                    (0..n)
                        .map(|i| if picks[i] == who { edit(i, tag) } else { base[i].clone() })
                        .collect()
                };
                let join = |lines: &[String]| {
                    let mut t = lines.join("\n");
                    if trailing_newline {
                        t.push('\n');
                    }
                    t
                };
                let expected: Vec<String> = (0..n)
                    .map(|i| match picks[i] {
                        1 => edit(i, "L"),
                        2 => edit(i, "R"),
                        _ => base[i].clone(),
                    })
                    .collect();
                let merged = merge3(&join(&base), &join(&side(1, "L")), &join(&side(2, "R")));
                prop_assert_eq!(merged, join(&expected));
            }

            #[test]
            fn concurrent_saves_converge(base in text(), left in text(), right in text()) {
                let merged = merge3(&base, &left, &right);
                // Everything either side inserted relative to an empty base
                // survives.
                if base.is_empty() {
                    prop_assert_eq!(merged.chars().count(), left.chars().count() + right.chars().count());
                }
            }
        }
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
