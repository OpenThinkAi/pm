-- AGT-1392: the hub-side materialized views claims are arbitrated
-- against (README §Sync & hub "Conditional ops at the hub").
--
-- One `pm_core::TicketView` per ticket and one `pm_core::WorkspaceView`
-- per workspace, serialized as the JSON pm-core's serde writes, and
-- updated inside the push transaction by folding each stored op with
-- pm-core's own `apply` / `apply_workspace` — the hub applies no merge
-- logic of its own. Rebuilt from the log by `views::rebuild` (the
-- migration that adds these tables backfills them from existing logs).
CREATE TABLE ticket_views (
    workspace_id text NOT NULL REFERENCES workspaces (id),
    entity       text NOT NULL,
    view         text NOT NULL,
    PRIMARY KEY (workspace_id, entity)
);

-- The workspace's config view (states above all): the `entity` is the
-- pm workspace's Ulid, fixed by the first config op; a config op for
-- another Ulid is refused, as pm-store refuses a foreign workspace.
CREATE TABLE workspace_views (
    workspace_id text PRIMARY KEY REFERENCES workspaces (id),
    entity       text NOT NULL,
    view         text NOT NULL
);
