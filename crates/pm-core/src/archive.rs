//! Pure selection logic for `pm archive --auto` (AGT-1351): `vault-sweep`
//! §1-2 ("archive done tickets monthly", "retire idle projects") as fields
//! instead of folder moves.
//!
//! Nothing here touches a clock, the op log or SQLite — every function
//! takes the wall-clock milliseconds and counts it needs as plain
//! arguments, so the CLI (`crates/pm/src/archive.rs`, which gathers those
//! inputs from `pm_store::Store`) and any future `pm-hub` sweep can share
//! one answer for "is this eligible" and unit-test it with fixed dates.
//!
//! - A ticket is eligible once its *completion month* — the calendar month
//!   of the HLC that last set its `state` to a `completed`-category value —
//!   is strictly before the current month. "This month" and last month are
//!   never eligible, matching `vault-sweep`'s `# skip current month`.
//! - A project is eligible once it has zero non-archived tickets **and**
//!   no document (`project.doc` or any `project_doc`) has been edited in
//!   the workspace's `stale_days`. A project whose documents have never
//!   been edited at all (`doc_last_edit_ms: None`) counts as stale — there
//!   is no activity to measure recency against.

/// `YYYY-MM` for a wall-clock millisecond timestamp: the first seven
/// characters of [`crate::markers::date_from_ms`] (itself Howard Hinnant's
/// civil-calendar algorithm, already the one date/time primitive this pure
/// crate has), so the month never disagrees with the day pm derives
/// elsewhere (`pm check`'s staleness, `pm hold`'s `date_from_ms(hold.at)`).
/// Zero-padded, so two keys compare correctly as plain strings.
pub fn month_key(wall_ms: u64) -> String {
    crate::markers::date_from_ms(wall_ms)[..7].to_string()
}

/// Whether a ticket that completed at `completed_wall_ms` is eligible for
/// `pm archive --auto` when the clock reads `now_wall_ms`: its completion
/// month must be strictly before the current one.
pub fn ticket_archivable(completed_wall_ms: u64, now_wall_ms: u64) -> bool {
    month_key(completed_wall_ms) < month_key(now_wall_ms)
}

/// Whether a project is idle enough for `pm archive --auto` to mark it
/// `complete`: zero non-archived tickets, and no document edit within
/// `stale_days` of `now_ms` (a project with no edit on record at all,
/// `doc_last_edit_ms: None`, counts as stale).
pub fn project_idle(
    non_archived_tickets: u64,
    doc_last_edit_ms: Option<u64>,
    now_ms: u64,
    stale_days: u32,
) -> bool {
    if non_archived_tickets != 0 {
        return false;
    }
    match doc_last_edit_ms {
        None => true,
        Some(edit_ms) => {
            let threshold = u64::from(stale_days) * 86_400_000;
            now_ms.saturating_sub(edit_ms) >= threshold
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Fixture timestamps (UTC), computed independently in Python:
    //   2026-08-31T23:59:59Z = 1788220799000
    //   2026-09-01T00:00:00Z = 1788220800000
    //   2026-09-28T00:00:00Z = 1790553600000
    //   2026-09-30T23:59:59Z = 1790812799000
    //   2026-10-01T00:00:00Z = 1790812800000
    //   2000-02-29T00:00:00Z (leap day) = 951782400000
    //   2024-12-31T23:59:59Z = 1735689599000
    //   2025-01-01T00:00:00Z = 1735689600000

    #[test]
    fn month_key_covers_the_epoch_a_leap_day_and_a_year_boundary() {
        assert_eq!(month_key(0), "1970-01");
        assert_eq!(month_key(951_782_400_000), "2000-02", "leap day");
        assert_eq!(
            month_key(1_735_689_599_000),
            "2024-12",
            "last second of the year"
        );
        assert_eq!(
            month_key(1_735_689_600_000),
            "2025-01",
            "first second of the year"
        );
    }

    #[test]
    fn month_key_flips_exactly_at_the_month_boundary() {
        assert_eq!(month_key(1_788_220_799_000), "2026-08");
        assert_eq!(month_key(1_788_220_800_000), "2026-09");
        assert_eq!(month_key(1_790_812_799_000), "2026-09");
        assert_eq!(month_key(1_790_812_800_000), "2026-10");
    }

    #[test]
    fn ticket_archivable_only_once_the_completion_month_has_passed() {
        let now = 1_790_553_600_000; // 2026-09-28
        // Same month as `now`: not yet archivable, "skip current month".
        assert!(!ticket_archivable(1_788_220_800_000, now)); // 2026-09-01
        // Last month: archivable.
        assert!(ticket_archivable(1_788_220_799_000, now)); // 2026-08-31
        // A ticket that "completes" later the same day `now` reads never
        // reads as archivable relative to itself.
        assert!(!ticket_archivable(now, now));
    }

    #[test]
    fn project_idle_requires_both_zero_tickets_and_a_stale_or_absent_doc_edit() {
        let now = 1_790_553_600_000; // 2026-09-28
        let stale_days = 30;
        let just_under_30_days_ago = now - 29 * 86_400_000;
        let exactly_30_days_ago = now - 30 * 86_400_000;
        let over_30_days_ago = now - 31 * 86_400_000;

        assert!(
            !project_idle(1, None, now, stale_days),
            "a live ticket keeps the project active regardless of doc activity"
        );
        assert!(
            project_idle(0, None, now, stale_days),
            "no doc edit on record at all counts as stale"
        );
        assert!(!project_idle(
            0,
            Some(just_under_30_days_ago),
            now,
            stale_days
        ));
        assert!(
            project_idle(0, Some(exactly_30_days_ago), now, stale_days),
            "the boundary itself is stale (>=), matching vault-sweep's 'older than 30 days'"
        );
        assert!(project_idle(0, Some(over_30_days_ago), now, stale_days));
    }
}
