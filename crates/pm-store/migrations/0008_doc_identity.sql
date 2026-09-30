-- AGT-1413: project document identity becomes op-derived. Until now a
-- project's design-doc `doc_id` (`project.doc_id`) and its named documents
-- (`project_doc` rows: name -> doc_id) were direct writes, so a replica that
-- pulled a `project.create` from the hub got a project with no documents
-- and its `body.edit` ops had nowhere to go. Now `project.create` carries
-- the design doc's `doc_id` and a `project.doc_add` op binds a named
-- document (or, without a name, the design doc of a project created before
-- this ticket); `src/config.rs` materializes `project.doc_id` and the
-- `project_doc` rows from the project's view like every other config
-- column, and `pm doctor --rebuild` replays them. The cached text
-- (`project.doc`, `project_doc.body`) stays derived from `body.edit` ops
-- (AGT-1344).
--
-- Written idempotently (IF NOT EXISTS), like 0006/0007.

-- Every doc_id ever bound to a project document, winner or not, with the
-- project (its Ulid) and slot (`name`, NULL = the design doc) that bound it
-- — derived from `project_view`, emptied and refilled by a rebuild. It is
-- how a replica recognizes a `body.edit` as a document's edit even when no
-- row shows that document: a binding that lost to an earlier one, or a
-- document of a deleted project. The rows (`project.doc_id`,
-- `project_doc.doc_id`) hold only the winners.
CREATE TABLE IF NOT EXISTS project_doc_owner (
    doc_id  TEXT PRIMARY KEY,
    project TEXT NOT NULL,        -- the owning project's Ulid (`project.ulid`)
    name    TEXT                  -- NULL = the design doc
);

-- The backfill is Rust (`src/backfill.rs::doc_identity`, run by
-- `Store::open` right after this file, in the same transaction), mirroring
-- 0007's: for each existing project, one `project.doc_add` op binding its
-- design doc's `doc_id` and one per named document, actor `migrate`, every
-- HLC below the log's oldest op. The ops are appended and the project views
-- and `project_doc_owner` written from them; no row is rewritten, so the
-- upgrade changes no data and `pm doctor` reads clean right after it. (A
-- row with no `doc_id` at all — written directly before AGT-1344 — gets a
-- fresh one, plus a `body.edit` carrying its cached text, so it too
-- replays to exactly what it holds.) Like 0007's, these ops count as
-- outbox.
SELECT 1 WHERE 0;
