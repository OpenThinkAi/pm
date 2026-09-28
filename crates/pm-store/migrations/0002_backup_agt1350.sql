-- Backup bookkeeping (AGT-1350, projects/pm/README.md "Durability before
-- the hub exists": "pm backup on a launchd timer (op-log JSONL export to a
-- private git repo). No state may exist only in one SQLite file for more
-- than a day."). One row per backup target (the destination directory's
-- absolute path, as `pm backup` resolves it) tracks how far the op log has
-- been exported, so a re-run appends only ops committed since. Like the
-- other configuration tables (workspace, state, project, project_doc,
-- actor), this is written directly and is not derived from the op log.
--
-- NOTE: this migration file is deliberately named distinctly from a
-- concurrently-developed 0002 migration (AGT-1344); expect a renumber when
-- the two are rebased together.
CREATE TABLE backup_target (
    target       TEXT    PRIMARY KEY CHECK (target <> ''),
    last_seq     INTEGER NOT NULL DEFAULT 0,
    last_success TEXT -- ISO-8601 UTC timestamp; NULL until a backup ever succeeds
);
