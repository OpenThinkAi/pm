//! Hybrid logical clock (Kulkarni et al., "Logical Physical Clocks and
//! Consistent Snapshots in Globally Distributed Databases", 2014).
//!
//! An [`Hlc`] is `(wall_ms, counter)`: the largest physical time observed so
//! far, plus a counter that breaks ties when several events share that
//! millisecond. Ordering is lexicographic, so every op stamped by one
//! [`Clock`] is strictly greater than the previous one *and* than every
//! remote stamp it has received — which is what makes LWW merges agree
//! across replicas regardless of arrival order.
//!
//! This crate never reads the system clock: callers pass `now_ms` in, so the
//! logic is pure and the CLI, the hub and tests all drive the same code.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::domain::ActorId;

/// A hybrid logical timestamp. Derived `Ord` compares `wall_ms` first, then
/// `counter`, which is exactly the HLC order.
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
pub struct Hlc {
    /// Physical component: milliseconds since the Unix epoch, as observed
    /// (never ahead of the greatest `now_ms` or remote `wall_ms` seen).
    pub wall_ms: u64,
    /// Logical component: resets to 0 whenever `wall_ms` advances.
    pub counter: u32,
}

impl Hlc {
    pub const ZERO: Hlc = Hlc {
        wall_ms: 0,
        counter: 0,
    };

    pub fn new(wall_ms: u64, counter: u32) -> Self {
        Hlc { wall_ms, counter }
    }
}

impl fmt::Display for Hlc {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}", self.wall_ms, self.counter)
    }
}

/// Per-replica clock state: the last timestamp this replica issued or
/// observed. Persist `latest()` and restore it with [`Clock::from_latest`]
/// so restarts never re-issue an old stamp.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Clock {
    latest: Hlc,
}

impl Clock {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn from_latest(latest: Hlc) -> Self {
        Clock { latest }
    }

    pub fn latest(&self) -> Hlc {
        self.latest
    }

    /// Stamp a local event ("send" / "local" in the paper). The result is
    /// strictly greater than every stamp this clock has issued or received.
    ///
    /// Never overflows: a spent counter rolls over into the next
    /// millisecond ([`Hlc::successor`]). It could only stop advancing at
    /// `wall_ms == u64::MAX`, which no clock reaches — every clock is
    /// restored from a stored stamp, and storage holds at most
    /// [`MAX_WALL_MS`]. Use [`Clock::try_send`] where the stamp must also
    /// stay storable.
    pub fn send(&mut self, now_ms: u64) -> Hlc {
        let latest = self.latest;
        self.latest = if now_ms > latest.wall_ms {
            Hlc::new(now_ms, 0)
        } else {
            latest.successor()
        };
        self.latest
    }

    /// Fold a remote stamp into this clock and stamp the receive event. The
    /// result is strictly greater than both `remote` and this clock's
    /// previous stamp. A remote counter at `u32::MAX` rolls over into the
    /// next millisecond instead of overflowing (oaudit 2026-09-30).
    pub fn receive(&mut self, remote: Hlc, now_ms: u64) -> Hlc {
        let local = self.latest;
        let wall_ms = now_ms.max(local.wall_ms).max(remote.wall_ms);
        self.latest = if wall_ms == local.wall_ms && wall_ms == remote.wall_ms {
            Hlc::new(wall_ms, local.counter.max(remote.counter)).successor()
        } else if wall_ms == local.wall_ms {
            local.successor()
        } else if wall_ms == remote.wall_ms {
            remote.successor()
        } else {
            Hlc::new(wall_ms, 0)
        };
        self.latest
    }

    /// [`Clock::send`], refused with [`ClockError::Exhausted`] (and the
    /// clock left as it was) when the stamp would not be storable
    /// ([`Hlc::check_range`]).
    pub fn try_send(&mut self, now_ms: u64) -> Result<Hlc, ClockError> {
        let before = *self;
        let next = self.send(now_ms);
        self.keep_in_range(before, next)
    }

    /// [`Clock::receive`], refused like [`Clock::try_send`].
    pub fn try_receive(&mut self, remote: Hlc, now_ms: u64) -> Result<Hlc, ClockError> {
        let before = *self;
        let next = self.receive(remote, now_ms);
        self.keep_in_range(before, next)
    }

    fn keep_in_range(&mut self, before: Clock, next: Hlc) -> Result<Hlc, ClockError> {
        match next.check_range() {
            Ok(()) if next > before.latest => Ok(next),
            _ => {
                *self = before;
                Err(ClockError::Exhausted {
                    latest: before.latest,
                })
            }
        }
    }
}

/// The largest storable `wall_ms`: both stores keep it in a signed 64-bit
/// integer (SQLite `INTEGER`, Postgres `bigint`). Still the year 292
/// million.
pub const MAX_WALL_MS: u64 = i64::MAX as u64;

/// The largest counter a stamp may carry across a trust boundary. One
/// below `u32::MAX`, so a received stamp always leaves the counter room
/// to advance within its millisecond.
pub const MAX_COUNTER: u32 = u32::MAX - 1;

/// How far ahead of the receiver's wall clock a remote stamp may be
/// before the hub refuses it (oaudit 2026-09-30): one day. A stamp
/// further ahead would win every LWW register until the world caught up
/// with it, and drag every clock that saw it into the future.
pub const MAX_FUTURE_SKEW_MS: u64 = 24 * 60 * 60 * 1000;

/// Why a stamp from another replica is not admissible
/// ([`Hlc::check_range`], [`Hlc::check_not_after`]).
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum StampError {
    #[error("hlc.wall_ms {wall_ms} is out of range (at most {MAX_WALL_MS})")]
    WallOutOfRange { wall_ms: u64 },
    #[error("hlc.counter {counter} is out of range (at most {MAX_COUNTER})")]
    CounterOutOfRange { counter: u32 },
    #[error(
        "hlc.wall_ms {wall_ms} is more than {max_skew_ms} ms ahead of the receiver's clock ({now_ms})"
    )]
    FarFuture {
        wall_ms: u64,
        now_ms: u64,
        max_skew_ms: u64,
    },
    /// A stamp carried as data in the payload (`archived_at`, `hold.at`;
    /// AGT-1482) is not storable ([`Hlc::check_range`]).
    #[error(
        "payload {field} {wall_ms}.{counter} is out of range (wall_ms at most {MAX_WALL_MS}, counter at most {MAX_COUNTER})"
    )]
    PayloadOutOfRange {
        field: &'static str,
        wall_ms: u64,
        counter: u32,
    },
    /// A stamp carried as data in the payload is more than `max_skew_ms`
    /// ahead of the receiver's clock (AGT-1482): the payload twin of
    /// [`StampError::FarFuture`], and like it a property of the clock,
    /// not of the op.
    #[error(
        "payload {field} wall_ms {wall_ms} is more than {max_skew_ms} ms ahead of the receiver's clock ({now_ms})"
    )]
    PayloadFarFuture {
        field: &'static str,
        wall_ms: u64,
        now_ms: u64,
        max_skew_ms: u64,
    },
}

/// A clock that cannot issue another storable stamp.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ClockError {
    #[error("the hybrid logical clock is exhausted at {latest}")]
    Exhausted { latest: Hlc },
}

impl Hlc {
    /// The least stamp strictly greater than this one: the next counter
    /// value, or — when the counter is spent — the next millisecond at
    /// counter 0. Checked arithmetic throughout: it saturates at
    /// `(u64::MAX, u32::MAX)` instead of wrapping, which no stored stamp
    /// can reach (see [`MAX_WALL_MS`]).
    pub fn successor(self) -> Hlc {
        match self.counter.checked_add(1) {
            Some(counter) => Hlc::new(self.wall_ms, counter),
            None => match self.wall_ms.checked_add(1) {
                Some(wall_ms) => Hlc::new(wall_ms, 0),
                None => self,
            },
        }
    }

    /// Whether this stamp is storable and leaves its counter room to
    /// advance: `wall_ms <= MAX_WALL_MS` and `counter <= MAX_COUNTER`.
    /// Every trust boundary (the hub's push, a replica's pull) checks
    /// this before a remote stamp reaches a clock or a store.
    pub fn check_range(self) -> Result<(), StampError> {
        if self.wall_ms > MAX_WALL_MS {
            return Err(StampError::WallOutOfRange {
                wall_ms: self.wall_ms,
            });
        }
        if self.counter > MAX_COUNTER {
            return Err(StampError::CounterOutOfRange {
                counter: self.counter,
            });
        }
        Ok(())
    }

    /// Whether this stamp is at most `max_skew_ms` ahead of `now_ms` (the
    /// receiver's wall clock). Stamps from the past are always fine —
    /// seeding uploads a log of historical ops.
    pub fn check_not_after(self, now_ms: u64, max_skew_ms: u64) -> Result<(), StampError> {
        if self.wall_ms > now_ms.saturating_add(max_skew_ms) {
            return Err(StampError::FarFuture {
                wall_ms: self.wall_ms,
                now_ms,
                max_skew_ms,
            });
        }
        Ok(())
    }
}

/// The total order every merge rule uses: HLC first, actor id as the
/// tie-break. Two ops from different actors can share an `Hlc`; two ops
/// from the same actor never can (its clock is strictly monotonic), so
/// `Stamp` is unique per op and `Ord` is total.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Stamp {
    pub hlc: Hlc,
    pub actor: ActorId,
}

impl Stamp {
    pub fn new(hlc: Hlc, actor: ActorId) -> Self {
        Stamp { hlc, actor }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn send_advances_with_wall_clock_and_resets_counter() {
        let mut c = Clock::new();
        assert_eq!(c.send(10), Hlc::new(10, 0));
        assert_eq!(c.send(10), Hlc::new(10, 1));
        assert_eq!(c.send(9), Hlc::new(10, 2)); // wall clock went backwards
        assert_eq!(c.send(11), Hlc::new(11, 0));
    }

    #[test]
    fn receive_adopts_the_greatest_component() {
        let mut c = Clock::from_latest(Hlc::new(10, 3));
        // remote ahead in wall time
        assert_eq!(c.receive(Hlc::new(20, 5), 15), Hlc::new(20, 6));
        // physical clock ahead of both
        assert_eq!(c.receive(Hlc::new(20, 9), 30), Hlc::new(30, 0));
        // same wall time on both sides: max counter + 1
        assert_eq!(c.receive(Hlc::new(30, 7), 25), Hlc::new(30, 8));
        // local ahead of remote and physical
        assert_eq!(c.receive(Hlc::new(1, 1), 1), Hlc::new(30, 9));
    }

    #[test]
    fn hlc_serde_round_trips() {
        let h = Hlc::new(1_700_000_000_000, 42);
        let json = serde_json::to_string(&h).unwrap();
        assert_eq!(json, r#"{"wall_ms":1700000000000,"counter":42}"#);
        assert_eq!(serde_json::from_str::<Hlc>(&json).unwrap(), h);
    }

    #[test]
    fn stamp_orders_by_hlc_then_actor() {
        let a = Stamp::new(Hlc::new(5, 0), ActorId::new("alice"));
        let b = Stamp::new(Hlc::new(5, 0), ActorId::new("bob"));
        let later = Stamp::new(Hlc::new(5, 1), ActorId::new("aaron"));
        assert!(a < b);
        assert!(b < later);
        assert!(a < later);
    }

    #[test]
    fn a_spent_counter_rolls_over_instead_of_overflowing() {
        // oaudit 2026-09-30: a remote counter at u32::MAX used to overflow.
        let mut c = Clock::from_latest(Hlc::new(10, 3));
        assert_eq!(c.receive(Hlc::new(10, u32::MAX), 5), Hlc::new(11, 0));
        let mut c = Clock::from_latest(Hlc::new(10, u32::MAX));
        assert_eq!(c.send(10), Hlc::new(11, 0));
        let mut c = Clock::from_latest(Hlc::new(1, 1));
        assert_eq!(c.receive(Hlc::new(20, u32::MAX), 5), Hlc::new(21, 0));
        let mut c = Clock::from_latest(Hlc::new(20, u32::MAX));
        assert_eq!(c.receive(Hlc::new(1, 1), 5), Hlc::new(21, 0));
        // Saturates rather than wraps at the very top.
        let top = Hlc::new(u64::MAX, u32::MAX);
        assert_eq!(top.successor(), top);
        let mut c = Clock::from_latest(top);
        assert_eq!(c.send(0), top);
    }

    #[test]
    fn try_send_and_try_receive_refuse_unstorable_stamps() {
        let mut c = Clock::from_latest(Hlc::new(MAX_WALL_MS, u32::MAX));
        assert_eq!(
            c.try_send(0),
            Err(ClockError::Exhausted {
                latest: Hlc::new(MAX_WALL_MS, u32::MAX)
            })
        );
        assert_eq!(
            c.latest(),
            Hlc::new(MAX_WALL_MS, u32::MAX),
            "left as it was"
        );
        let mut c = Clock::from_latest(Hlc::new(5, 0));
        assert!(c.try_receive(Hlc::new(MAX_WALL_MS + 1, 0), 5).is_err());
        assert_eq!(c.latest(), Hlc::new(5, 0));
        assert!(c.try_send(u64::MAX).is_err());
        assert_eq!(c.try_send(6), Ok(Hlc::new(6, 0)));
        assert_eq!(
            c.try_receive(Hlc::new(MAX_WALL_MS, 7), 6),
            Ok(Hlc::new(MAX_WALL_MS, 8))
        );
    }

    #[test]
    fn remote_stamps_are_range_and_skew_checked() {
        assert_eq!(Hlc::new(MAX_WALL_MS, MAX_COUNTER).check_range(), Ok(()));
        assert_eq!(
            Hlc::new(MAX_WALL_MS + 1, 0).check_range(),
            Err(StampError::WallOutOfRange {
                wall_ms: MAX_WALL_MS + 1
            })
        );
        assert_eq!(
            Hlc::new(0, u32::MAX).check_range(),
            Err(StampError::CounterOutOfRange { counter: u32::MAX })
        );
        let now = 1_790_000_000_000;
        assert_eq!(
            Hlc::new(0, 0).check_not_after(now, MAX_FUTURE_SKEW_MS),
            Ok(())
        );
        assert_eq!(
            Hlc::new(now + MAX_FUTURE_SKEW_MS, 9).check_not_after(now, MAX_FUTURE_SKEW_MS),
            Ok(())
        );
        assert!(matches!(
            Hlc::new(now + MAX_FUTURE_SKEW_MS + 1, 0).check_not_after(now, MAX_FUTURE_SKEW_MS),
            Err(StampError::FarFuture { .. })
        ));
        // No overflow when the receiver's clock is itself absurd.
        assert_eq!(
            Hlc::new(u64::MAX, 0).check_not_after(u64::MAX, MAX_FUTURE_SKEW_MS),
            Ok(())
        );
    }

    /// One replica's inputs: a physical-clock reading (possibly going
    /// backwards) and, optionally, a remote stamp to fold in.
    #[derive(Clone, Debug)]
    enum Event {
        Send(u64),
        Receive(u64, Hlc),
    }

    fn event() -> impl Strategy<Value = Event> {
        prop_oneof![
            (0u64..1000).prop_map(Event::Send),
            ((0u64..1000), (0u64..1000), (0u32..8))
                .prop_map(|(now, w, c)| Event::Receive(now, Hlc::new(w, c))),
            // Counters at the top of their range must roll over, not wrap.
            ((0u64..1000), (0u64..1000), (u32::MAX - 2..=u32::MAX))
                .prop_map(|(now, w, c)| Event::Receive(now, Hlc::new(w, c))),
        ]
    }

    proptest! {
        /// Every stamp a clock issues is strictly greater than the previous
        /// one and than any remote stamp it received, even when the wall
        /// clock runs backwards.
        #[test]
        fn clock_is_strictly_monotonic(events in prop::collection::vec(event(), 1..64)) {
            let mut clock = Clock::new();
            let mut prev = Hlc::ZERO;
            for ev in events {
                let next = match ev {
                    Event::Send(now) => {
                        let h = clock.send(now);
                        prop_assert!(h.wall_ms >= now);
                        h
                    }
                    Event::Receive(now, remote) => {
                        let h = clock.receive(remote, now);
                        prop_assert!(h > remote);
                        prop_assert!(h.wall_ms >= now);
                        h
                    }
                };
                prop_assert!(next > prev, "{next} must exceed {prev}");
                prop_assert_eq!(clock.latest(), next);
                prev = next;
            }
        }

        /// Two replicas exchanging stamps in arbitrary order never produce
        /// equal (hlc, actor) pairs, and the pairs sort into one total order
        /// on which every replica agrees.
        #[test]
        fn stamps_form_a_total_order_with_actor_tie_break(
            steps in prop::collection::vec((0u64..50, any::<bool>(), any::<bool>()), 1..64)
        ) {
            let actors = [ActorId::new("a"), ActorId::new("b")];
            let mut clocks = [Clock::new(), Clock::new()];
            let mut stamps: Vec<Stamp> = Vec::new();
            for (now, who, exchange) in steps {
                let i = usize::from(who);
                let hlc = clocks[i].send(now);
                stamps.push(Stamp::new(hlc, actors[i].clone()));
                if exchange {
                    let hlc = clocks[1 - i].receive(hlc, now);
                    stamps.push(Stamp::new(hlc, actors[1 - i].clone()));
                }
            }
            let mut sorted = stamps.clone();
            sorted.sort();
            sorted.dedup();
            prop_assert_eq!(sorted.len(), stamps.len(), "stamps must be unique");
            for pair in sorted.windows(2) {
                prop_assert!(pair[0] < pair[1]);
                if pair[0].hlc == pair[1].hlc {
                    prop_assert!(pair[0].actor < pair[1].actor);
                }
            }
            // Ord is antisymmetric and consistent with equality.
            for x in &stamps {
                for y in &stamps {
                    prop_assert_eq!(x.cmp(y), y.cmp(x).reverse());
                    prop_assert_eq!(x.cmp(y).is_eq(), x == y);
                }
            }
        }
    }
}
