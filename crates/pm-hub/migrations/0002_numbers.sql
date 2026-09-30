-- AGT-1391: per-workspace ticket-number allocation (README §Sync & hub
-- "Conditional ops at the hub").
--
-- A workspace starts in *seed mode* (`seeded_at IS NULL`): the client that
-- brings an existing log uploads it with its own `field.set number` ops,
-- then `POST /w/{id}/seeded` sets the floor and flips the workspace to
-- *authoritative* (`seeded_at` set). From then on only the hub numbers
-- tickets: a pushed `ticket.create` gets the next number in the same
-- transaction, and a pushed `field.set number` is refused.
ALTER TABLE workspaces
    -- The next number the allocator hands out; `number_floor + 1` once
    -- seeded, then one more per allocation.
    ADD COLUMN next_number   bigint NOT NULL DEFAULT 1 CHECK (next_number >= 1),
    -- The hub's HLC for this workspace (`pm_core::Clock`), so every op it
    -- authors is stamped after the op it answers and after its own last.
    ADD COLUMN clock_wall_ms bigint NOT NULL DEFAULT 0,
    ADD COLUMN clock_counter bigint NOT NULL DEFAULT 0,
    ADD COLUMN seeded_at     timestamptz;

-- Every numbered ticket: the number and the seq of the `field.set number`
-- op that issued it (a seeded client op, or the hub's own). The unique
-- key is what makes "never issued twice" a database invariant, not just
-- allocator discipline.
CREATE TABLE numbers (
    workspace_id text   NOT NULL REFERENCES workspaces (id),
    entity       text   NOT NULL,
    number       bigint NOT NULL,
    seq          bigint NOT NULL REFERENCES ops (seq),
    PRIMARY KEY (workspace_id, entity),
    UNIQUE (workspace_id, number)
);
