-- AGT-1387: the hub's first schema (projects/pm/README.md §Sync & hub).
-- Workspaces, the op log with its server-assigned sequence, and bearer
-- tokens. Membership/grants beyond one owner are P6; the hub-side
-- materialized ticket rows for conditional ops land with push/pull.

-- One row per synced workspace (today: `saltline`). `number_floor` seeds
-- the hub's number allocator (AGT-1347) so vault-minted and pm-minted
-- numbers never collide.
CREATE TABLE workspaces (
    id           text        PRIMARY KEY,
    number_floor bigint      NOT NULL DEFAULT 0 CHECK (number_floor >= 0),
    created_at   timestamptz NOT NULL DEFAULT now()
);

-- The op log. `seq` is the only transport order (clients pull
-- `since <seq>`); merge order stays the op's own HLC, applied client-side
-- by pm-core. `op` is the op log's JSON exactly as pushed (`json`, not
-- `jsonb`, so it is served back byte-for-byte); the other columns index it.
CREATE TABLE ops (
    seq          bigserial   PRIMARY KEY,
    workspace_id text        NOT NULL REFERENCES workspaces (id),
    op_id        text        NOT NULL,
    hlc_wall_ms  bigint      NOT NULL,
    hlc_counter  bigint      NOT NULL,
    actor        text        NOT NULL,
    entity       text        NOT NULL,
    kind         text        NOT NULL,
    op           json        NOT NULL,
    received_at  timestamptz NOT NULL DEFAULT now(),
    UNIQUE (workspace_id, op_id)
);
CREATE INDEX ops_workspace_seq ON ops (workspace_id, seq);
CREATE INDEX ops_workspace_entity ON ops (workspace_id, entity);

-- Bearer tokens, one per machine or agent. Only a hash is stored; the
-- plaintext is shown once at mint time and never persisted.
CREATE TABLE tokens (
    id           bigserial   PRIMARY KEY,
    workspace_id text        NOT NULL REFERENCES workspaces (id),
    label        text        NOT NULL,
    token_hash   bytea       NOT NULL UNIQUE,
    created_at   timestamptz NOT NULL DEFAULT now(),
    revoked_at   timestamptz
);
