//! The three merge rules from README §Conflict semantics, as data
//! structures. Each is a CRDT: applying the same write twice is a no-op,
//! and applying two concurrent writes in either order gives the same
//! result. `view::apply` composes them into a ticket.
//!
//! | Rule | Type | Used for |
//! |---|---|---|
//! | LWW register (HLC, then actor) | [`Lww`] | scalar fields, state, hold, markers |
//! | OR-set, add-wins | [`OrSet`] | labels, relations |
//! | Append-only, ordered by HLC | [`CommentLog`] | comments |

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use ulid::Ulid;

use crate::domain::{ActorId, Comment};
use crate::hlc::{Hlc, Stamp};

/// Last-writer-wins register. A write lands only if its stamp is strictly
/// greater than the current one, so equal-stamp replays are no-ops and
/// ordering of arrival does not matter.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Lww<T> {
    pub value: T,
    /// `None` until the first write (a ticket's registers exist before its
    /// `ticket.create` op arrives, since ops may sync out of order).
    pub stamp: Option<Stamp>,
}

impl<T> Lww<T> {
    /// Returns whether the write won.
    pub fn set(&mut self, value: T, stamp: Stamp) -> bool {
        if self.stamp.as_ref().is_some_and(|current| *current >= stamp) {
            return false;
        }
        self.value = value;
        self.stamp = Some(stamp);
        true
    }
}

/// Observed-remove set. Each add carries a unique tag (the op id); a remove
/// names the tags it observed. An add whose tag the remove never saw
/// survives it — add wins under concurrency.
///
/// Removed tags are remembered so the rule also holds without causal
/// delivery: an add that syncs *after* the remove that observed it is
/// ignored, and the outcome is the same in either arrival order.
///
/// Serializes as `{entries: [[value, [tags…]]…], removed: [tags…]}` so
/// non-string values (relations) work as JSON.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    into = "OrSetRepr<T>",
    from = "OrSetRepr<T>",
    bound = "T: Ord + Clone + Serialize + serde::de::DeserializeOwned"
)]
pub struct OrSet<T: Ord> {
    entries: BTreeMap<T, BTreeSet<Ulid>>,
    removed: BTreeSet<Ulid>,
}

/// Serde shape for [`OrSet`]; not part of the public API.
#[derive(Serialize, Deserialize)]
pub(crate) struct OrSetRepr<T> {
    entries: Vec<(T, Vec<Ulid>)>,
    removed: Vec<Ulid>,
}

impl<T: Ord> Default for OrSet<T> {
    fn default() -> Self {
        OrSet {
            entries: BTreeMap::new(),
            removed: BTreeSet::new(),
        }
    }
}

impl<T: Ord> From<OrSet<T>> for OrSetRepr<T> {
    fn from(set: OrSet<T>) -> Self {
        OrSetRepr {
            entries: set
                .entries
                .into_iter()
                .map(|(value, tags)| (value, tags.into_iter().collect()))
                .collect(),
            removed: set.removed.into_iter().collect(),
        }
    }
}

impl<T: Ord> From<OrSetRepr<T>> for OrSet<T> {
    fn from(repr: OrSetRepr<T>) -> Self {
        OrSet {
            entries: repr
                .entries
                .into_iter()
                .filter(|(_, tags)| !tags.is_empty())
                .map(|(value, tags)| (value, tags.into_iter().collect()))
                .collect(),
            removed: repr.removed.into_iter().collect(),
        }
    }
}

impl<T: Ord> OrSet<T> {
    /// Add `value` under `tag`; a no-op if that tag was already removed.
    pub fn add(&mut self, value: T, tag: Ulid) {
        if self.removed.contains(&tag) {
            return;
        }
        self.entries.entry(value).or_default().insert(tag);
    }

    /// Drop the `observed` tags; the value disappears once no tag is left.
    pub fn remove(&mut self, value: &T, observed: &[Ulid]) {
        self.removed.extend(observed.iter().copied());
        if let Some(tags) = self.entries.get_mut(value) {
            for tag in observed {
                tags.remove(tag);
            }
            if tags.is_empty() {
                self.entries.remove(value);
            }
        }
    }

    pub fn contains(&self, value: &T) -> bool {
        self.entries.contains_key(value)
    }

    /// The add-tags currently backing `value` — what a remover must cite.
    pub fn observed(&self, value: &T) -> Vec<Ulid> {
        self.entries
            .get(value)
            .map(|tags| tags.iter().copied().collect())
            .unwrap_or_default()
    }

    pub fn iter(&self) -> impl Iterator<Item = &T> {
        self.entries.keys()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// Append-only comments, kept sorted by `(hlc, author, id)`. A replayed
/// `comment.add` (same key) is a no-op.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommentLog {
    entries: Vec<Comment>,
}

/// `(hlc, author, id)` — the same order as [`Stamp`], without allocating.
fn comment_key(c: &Comment) -> (Hlc, &ActorId, Ulid) {
    (c.hlc, &c.author, c.id)
}

impl CommentLog {
    /// Returns whether the comment was new.
    pub fn push(&mut self, comment: Comment) -> bool {
        let key = comment_key(&comment);
        match self.entries.binary_search_by(|c| comment_key(c).cmp(&key)) {
            Ok(_) => false,
            Err(pos) => {
                self.entries.insert(pos, comment);
                true
            }
        }
    }

    /// Comments in HLC order.
    pub fn iter(&self) -> impl Iterator<Item = &Comment> {
        self.entries.iter()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stamp(wall_ms: u64, actor: &str) -> Stamp {
        Stamp::new(Hlc::new(wall_ms, 0), ActorId::new(actor))
    }

    // ---- LWW ----

    #[test]
    fn lww_later_hlc_wins_in_either_order() {
        let early = ("early", stamp(1, "bob"));
        let late = ("late", stamp(2, "alice"));
        for order in [[early.clone(), late.clone()], [late.clone(), early.clone()]] {
            let mut reg = Lww::default();
            for (v, s) in order {
                reg.set(v, s);
            }
            assert_eq!(reg.value, "late");
        }
    }

    #[test]
    fn lww_equal_hlc_breaks_ties_by_actor_in_either_order() {
        let a = ("from-alice", stamp(7, "alice"));
        let b = ("from-bob", stamp(7, "bob"));
        for order in [[a.clone(), b.clone()], [b.clone(), a.clone()]] {
            let mut reg = Lww::default();
            for (v, s) in order {
                reg.set(v, s);
            }
            assert_eq!(reg.value, "from-bob", "greater actor id wins");
        }
    }

    #[test]
    fn lww_replay_of_the_winning_write_is_a_no_op() {
        let mut reg = Lww::default();
        assert!(reg.set("x", stamp(3, "a")));
        assert!(!reg.set("x", stamp(3, "a")));
        assert!(!reg.set("older", stamp(2, "z")));
        assert_eq!(reg.value, "x");
        assert_eq!(reg.stamp, Some(stamp(3, "a")));
    }

    // ---- OR-set ----

    #[test]
    fn orset_concurrent_add_and_remove_keeps_the_element() {
        // Replica A observed tag t1 and removes; replica B concurrently
        // re-adds with tag t2. Both orders converge on "present".
        let (t1, t2) = (Ulid::new(), Ulid::new());
        let base = {
            let mut s = OrSet::default();
            s.add("bug", t1);
            s
        };
        let mut remove_first = base.clone();
        remove_first.remove(&"bug", &[t1]);
        remove_first.add("bug", t2);

        let mut add_first = base;
        add_first.add("bug", t2);
        add_first.remove(&"bug", &[t1]);

        assert_eq!(remove_first, add_first);
        assert!(remove_first.contains(&"bug"));
        assert_eq!(remove_first.observed(&"bug"), vec![t2]);
    }

    #[test]
    fn orset_remove_of_all_observed_tags_removes_the_element() {
        let (t1, t2) = (Ulid::new(), Ulid::new());
        let mut s = OrSet::default();
        s.add("bug", t1);
        s.add("bug", t2);
        let observed = s.observed(&"bug");
        assert_eq!(observed.len(), 2);
        s.remove(&"bug", &observed);
        assert!(!s.contains(&"bug"));
        assert!(s.is_empty());
    }

    #[test]
    fn orset_remove_arriving_before_its_observed_add_still_removes() {
        let t = Ulid::new();
        let mut early_remove = OrSet::default();
        early_remove.remove(&"bug", &[t]);
        early_remove.add("bug", t);

        let mut in_order = OrSet::default();
        in_order.add("bug", t);
        in_order.remove(&"bug", &[t]);

        assert_eq!(early_remove, in_order);
        assert!(!early_remove.contains(&"bug"));
    }

    #[test]
    fn orset_serde_round_trips_with_struct_values() {
        use crate::domain::{Relation, RelationKind};
        let mut s = OrSet::default();
        let r = Relation {
            kind: RelationKind::Blocks,
            from: Ulid::new(),
            to: Ulid::new(),
        };
        s.add(r, Ulid::new());
        s.remove(&r, &[Ulid::new()]);
        let json = serde_json::to_string(&s).unwrap();
        let back: OrSet<Relation> = serde_json::from_str(&json).unwrap();
        assert_eq!(back, s);
    }

    #[test]
    fn orset_add_and_remove_replays_are_no_ops() {
        let t = Ulid::new();
        let mut s = OrSet::default();
        s.add("x", t);
        let once = s.clone();
        s.add("x", t);
        assert_eq!(s, once);
        s.remove(&"x", &[t]);
        let removed = s.clone();
        s.remove(&"x", &[t]);
        assert_eq!(s, removed);
        s.remove(&"never-there", &[t]);
        assert!(s.is_empty());
    }

    // ---- comments ----

    fn comment(wall_ms: u64, author: &str, body: &str) -> Comment {
        Comment {
            id: Ulid::new(),
            ticket: Ulid::nil(),
            author: ActorId::new(author),
            hlc: Hlc::new(wall_ms, 0),
            body: body.into(),
        }
    }

    #[test]
    fn comments_order_by_hlc_then_actor_regardless_of_arrival() {
        let c1 = comment(1, "zed", "first");
        let c2 = comment(2, "bob", "second-bob");
        let c3 = comment(2, "carol", "second-carol");
        let mut forward = CommentLog::default();
        let mut reverse = CommentLog::default();
        for c in [&c1, &c2, &c3] {
            assert!(forward.push(c.clone()));
        }
        for c in [&c3, &c2, &c1] {
            assert!(reverse.push(c.clone()));
        }
        assert_eq!(forward, reverse);
        let bodies: Vec<&str> = forward.iter().map(|c| c.body.as_str()).collect();
        assert_eq!(bodies, ["first", "second-bob", "second-carol"]);
    }

    #[test]
    fn comment_replay_is_a_no_op() {
        let c = comment(1, "a", "x");
        let mut log = CommentLog::default();
        assert!(log.push(c.clone()));
        assert!(!log.push(c));
        assert_eq!(log.len(), 1);
    }
}
