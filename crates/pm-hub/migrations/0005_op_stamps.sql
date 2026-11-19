-- AGT-2456: the hub refuses a pushed op whose (hlc, actor) stamp another
-- op in the workspace already has (`ops.rs`, "Stamps are unique"), so the
-- merge rules' tie-break on (hlc, actor) never sees two writes it cannot
-- order. This index makes that lookup cheap. It is deliberately not
-- UNIQUE: a log written before the check may already hold such a pair,
-- and the migration must not fail on it. Additive only.
CREATE INDEX IF NOT EXISTS ops_workspace_stamp
    ON ops (workspace_id, hlc_wall_ms, hlc_counter, actor);
