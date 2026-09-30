-- AGT-1385: the configuration tables become op-derived (README §Sync & hub
-- "Config must become ops", decision A4). `workspace` (all but
-- `number_floor`), `state`, `actor` and a project's metadata columns
-- (`id`, `title`, `status`, `parent`, `repos`) are now materialized from the
-- config op kinds AGT-1384 added to pm-core (`workspace.set`,
-- `state.upsert`, `actor.upsert`, `project.create`, `project.set`) the way
-- the ticket tables are from ticket ops: `Store::commit` appends the op and
-- rewrites the rows in one transaction, and `pm doctor --rebuild` replays
-- them (`src/config.rs`). What stays a direct write: `workspace.number_floor`
-- (allocator bookkeeping, like `backup_target` and `sync_state`),
-- `project.doc_id` / `project_doc` (document identity — a document's
-- *body* is already op-derived, AGT-1344), and the *existence* of a project
-- row: `pm project delete` removes the row directly and a rebuild never
-- resurrects one (there is no project tombstone kind).

-- Written idempotently (IF NOT EXISTS), like 0006, so re-running the file
-- against a database that already has it is a no-op; the one statement
-- SQLite cannot make idempotent (`ALTER TABLE project ADD COLUMN ulid`)
-- lives in `src/backfill.rs` behind a column check, with the index on it.

-- The merge state per config entity (pm_core::WorkspaceView /
-- pm_core::ProjectView as JSON), the analogue of `ticket_view`: loaded,
-- folded and re-saved by every config commit so the CRDT rules run once,
-- in pm-core. Fully derived; emptied and refilled by a rebuild.
CREATE TABLE IF NOT EXISTS workspace_view (
    workspace TEXT PRIMARY KEY,   -- the workspace's Ulid (`workspace.id`)
    view      TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS project_view (
    project TEXT PRIMARY KEY,     -- the project's Ulid (`project.ulid`)
    view    TEXT NOT NULL
);

-- `project.ulid` (added by `src/backfill.rs`): a project's op-log
-- identity. `project.create` / `project.set` target this Ulid, and the
-- kebab-case `id` is carried in the create payload the way a ticket's
-- human number is separate from its Ulid. Nullable only because ALTER
-- TABLE cannot add a NOT NULL column without a default; every writer sets
-- it and the backfill fills every existing row.
--
-- The backfill is Rust, not SQL (`src/backfill.rs`, run by `Store::open`
-- right after this file, in the same transaction): one config op per
-- existing workspace field, state, actor and project (plus one
-- `project.set repo_add` per repo), actor `migrate`, every HLC below the
-- log's oldest op so any later real config write wins LWW. The ops are
-- appended and the views written from them; no row is rewritten, so the
-- upgrade changes no data and `pm doctor` reads clean right after it.
--
-- These ops land at the end of the log, above `sync_state.pushed_through`,
-- so they count as outbox — deliberately: no existing database has pushed
-- anything yet, and the first push seeds the hub with the whole log (README
-- "First push uploads the Studio's existing log"), config included.
SELECT 1 WHERE 0;
