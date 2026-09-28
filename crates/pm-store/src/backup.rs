//! Backup bookkeeping (AGT-1350, README §Constraints "Durability before the
//! hub exists"): where `pm backup` last got to for a given target, so a
//! re-run appends only ops recorded since; and [`Store::ops_since`], which
//! reads the whole log (unlike [`Store::ops`], scoped to one ticket).
//!
//! A target's row is written directly, like the other configuration tables
//! (`config.rs`) — it is not derived from, or replayed from, the op log.
//! The workspace/project snapshot a restore also needs already has public
//! readers and writers in `config.rs` (`Store::workspace`,
//! `Store::projects`, `Store::init_workspace`, `Store::put_project`); `pm
//! backup` composes those with `ops_since` rather than this module growing
//! a second copy of them.

use pm_core::Op;
use rusqlite::{OptionalExtension, params};

use crate::Store;
use crate::error::Result;
use crate::query::read_ops;

/// What `pm backup status` reports for one target.
#[derive(Clone, Debug, PartialEq)]
pub struct BackupStatus {
    /// The highest op `seq` this target has received.
    pub last_seq: i64,
    /// ISO-8601 UTC timestamp of the last successful backup, if any.
    pub last_success: Option<String>,
    /// Seconds since `last_success`, computed in SQL so no caller needs a
    /// clock/date dependency of its own. `None` exactly when
    /// `last_success` is `None`.
    pub age_seconds: Option<f64>,
}

impl Store {
    /// Every op with `seq` greater than `since`, in `seq` order — the
    /// whole workspace, not one ticket (contrast [`Store::ops`]). `since =
    /// 0` (or a target's high-water mark from [`Store::backup_last_seq`])
    /// reads the whole log.
    pub fn ops_since(&self, since: i64) -> Result<Vec<(i64, Op)>> {
        read_ops(&self.conn, "WHERE seq > ?1", params![since])
    }

    /// The `seq` `target` (an opaque string a caller keys by; `pm backup`
    /// uses the backup directory's absolute path) last received. `0` if
    /// this target has never backed up.
    pub fn backup_last_seq(&self, target: &str) -> Result<i64> {
        Ok(self
            .conn
            .query_row(
                "SELECT last_seq FROM backup_target WHERE target = ?1",
                params![target],
                |r| r.get(0),
            )
            .optional()?
            .unwrap_or(0))
    }

    /// `target`'s backup status, or `None` if it has never backed up here.
    pub fn backup_status(&self, target: &str) -> Result<Option<BackupStatus>> {
        Ok(self
            .conn
            .query_row(
                "SELECT last_seq, last_success,
                        CASE WHEN last_success IS NULL THEN NULL
                             ELSE (julianday('now') - julianday(last_success)) * 86400.0
                        END
                 FROM backup_target WHERE target = ?1",
                params![target],
                |r| {
                    Ok(BackupStatus {
                        last_seq: r.get(0)?,
                        last_success: r.get(1)?,
                        age_seconds: r.get(2)?,
                    })
                },
            )
            .optional()?)
    }

    /// Records a successful `pm backup` to `target`: its `last_seq`
    /// advances to (at least) `through_seq` and `last_success` becomes now
    /// (UTC, computed in SQL). Safe to call even when nothing new was
    /// exported — a no-op backup is still a successful one.
    pub fn backup_record(&mut self, target: &str, through_seq: i64) -> Result<()> {
        self.conn.execute(
            "INSERT INTO backup_target (target, last_seq, last_success)
             VALUES (?1, ?2, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
             ON CONFLICT(target) DO UPDATE SET
               last_seq = MAX(last_seq, excluded.last_seq),
               last_success = excluded.last_success",
            params![target, through_seq],
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use pm_core::op::TicketCreate;
    use pm_core::{ActorId, Hlc, Payload, Priority, State, StateCategory, Workspace};
    use ulid::Ulid;

    use super::*;

    fn fresh() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(dir.path().join("pm.sqlite")).unwrap();
        store
            .init_workspace(&Workspace {
                id: Ulid::new(),
                prefix: "AGT".into(),
                states: vec![State {
                    name: "triage".into(),
                    category: StateCategory::Unstarted,
                    position: 0,
                }],
                gate_labels: Default::default(),
                model_labels: Default::default(),
                template_sections: Vec::new(),
                stale_days: 30,
            })
            .unwrap();
        (dir, store)
    }

    fn create(wall_ms: u64) -> Op {
        Op::new(
            Ulid::new(),
            Hlc::new(wall_ms, 0),
            ActorId::new("matt"),
            Ulid::new(),
            Payload::TicketCreate(TicketCreate {
                title: "t".into(),
                state: "triage".into(),
                priority: Priority::Medium,
                project: None,
                repo: None,
                source: None,
                ext: Default::default(),
            }),
        )
    }

    #[test]
    fn ops_since_zero_is_the_whole_log_and_advances_with_new_commits() {
        let (_dir, mut store) = fresh();
        assert!(store.ops_since(0).unwrap().is_empty());

        store.commit(&create(1)).unwrap();
        let first = store.ops_since(0).unwrap();
        assert_eq!(first.len(), 1);
        let seq = first[0].0;

        assert!(store.ops_since(seq).unwrap().is_empty());
        store.commit(&create(2)).unwrap();
        let second = store.ops_since(seq).unwrap();
        assert_eq!(second.len(), 1);
        assert!(second[0].0 > seq);
    }

    #[test]
    fn a_target_with_no_backups_has_no_status_and_a_zero_high_water_mark() {
        let (_dir, store) = fresh();
        assert_eq!(store.backup_last_seq("/tmp/nope").unwrap(), 0);
        assert_eq!(store.backup_status("/tmp/nope").unwrap(), None);
    }

    #[test]
    fn backup_record_advances_last_seq_and_stamps_a_fresh_success_time() {
        let (_dir, mut store) = fresh();
        store.commit(&create(1)).unwrap();
        let seq = store.ops_since(0).unwrap()[0].0;

        store.backup_record("t1", seq).unwrap();
        let status = store.backup_status("t1").unwrap().unwrap();
        assert_eq!(status.last_seq, seq);
        assert!(status.last_success.is_some());
        // Freshly recorded: well under the 24h staleness window a caller
        // (`pm backup status`) checks against.
        assert!(status.age_seconds.unwrap() < 5.0, "{status:?}");
        assert_eq!(store.backup_last_seq("t1").unwrap(), seq);

        // A second target is independent.
        assert_eq!(store.backup_last_seq("t2").unwrap(), 0);
    }

    #[test]
    fn backup_record_never_moves_last_seq_backwards() {
        let (_dir, mut store) = fresh();
        store.backup_record("t1", 5).unwrap();
        store.backup_record("t1", 2).unwrap();
        assert_eq!(store.backup_last_seq("t1").unwrap(), 5);
    }
}
