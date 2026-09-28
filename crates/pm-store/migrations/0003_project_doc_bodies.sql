-- AGT-1344: project document bodies become op-derived. A project's design
-- doc and its extra named documents each get a stable `doc_id` (a Ulid)
-- that `body.edit` ops target, the same text CRDT ticket descriptions use
-- (AGT-1338) — there is exactly one body format in the op log, ever.
-- `project.doc` / `project_doc.body` stay as the materialized-text cache,
-- replayed by `pm doctor` / `--rebuild` the same way `ticket.description`
-- is; project metadata (title/status/parent/repos) is unchanged and still
-- written directly (config.rs). `doc_id` is nullable: a project or
-- document written the old way (direct `put_project`, before an id was
-- assigned) simply has no op-derived history until one is.

-- SQLite's ALTER TABLE ADD COLUMN refuses a UNIQUE constraint directly, so
-- uniqueness is a separate partial index (NULL doc_id, the not-yet-assigned
-- case, is never compared for uniqueness by design).
ALTER TABLE project ADD COLUMN doc_id TEXT;
ALTER TABLE project_doc ADD COLUMN doc_id TEXT;

CREATE UNIQUE INDEX project_doc_id ON project(doc_id) WHERE doc_id IS NOT NULL;
CREATE UNIQUE INDEX project_doc_doc_id ON project_doc(doc_id) WHERE doc_id IS NOT NULL;

-- Cached merge state per document body (pm_core::DocView as JSON), the
-- document analogue of ticket_view; replayed by pm doctor / --rebuild.
CREATE TABLE project_doc_view (
    doc_id TEXT PRIMARY KEY,
    view   TEXT NOT NULL
);
