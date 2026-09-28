-- pm-store schema v1 (AGT-1335). Tables follow projects/pm/README.md §Data
-- model; every HLC is stored as its two components (wall_ms, counter) so
-- `ORDER BY wall_ms, counter` is the HLC order.
--
-- Configuration tables (workspace, state, project, project_doc, actor) are
-- written directly. Ticket tables (ticket, ticket_label, relation, comment,
-- marker, ticket_view) are derived: `Store::commit` rewrites them from the
-- op log in the same transaction that appends the op.

CREATE TABLE workspace (
    -- One workspace per database: every row carries singleton = 1 and the
    -- column is unique, so a second insert fails.
    singleton         INTEGER NOT NULL DEFAULT 1 UNIQUE CHECK (singleton = 1),
    id                TEXT    NOT NULL,
    prefix            TEXT    NOT NULL CHECK (prefix <> ''),
    gate_labels       TEXT    NOT NULL, -- JSON array of label names
    model_labels      TEXT    NOT NULL, -- JSON object label -> model
    template_sections TEXT    NOT NULL, -- JSON array of headings
    stale_days        INTEGER NOT NULL CHECK (stale_days >= 0)
);

CREATE TABLE state (
    name     TEXT    PRIMARY KEY CHECK (name <> ''),
    category TEXT    NOT NULL CHECK (category IN ('backlog', 'unstarted', 'started', 'completed', 'canceled')),
    position INTEGER NOT NULL
);

CREATE TABLE actor (
    id   TEXT PRIMARY KEY CHECK (id <> ''),
    kind TEXT NOT NULL CHECK (kind IN ('human', 'agent'))
);

CREATE TABLE project (
    id     TEXT PRIMARY KEY CHECK (id <> ''),
    title  TEXT NOT NULL,
    status TEXT NOT NULL CHECK (status IN ('in-progress', 'complete', 'abandoned')),
    parent TEXT REFERENCES project(id),
    repos  TEXT NOT NULL DEFAULT '[]', -- JSON array
    doc    TEXT NOT NULL DEFAULT ''    -- the design doc (today's README)
);

CREATE TABLE project_doc (
    project TEXT NOT NULL REFERENCES project(id),
    name    TEXT NOT NULL,
    body    TEXT NOT NULL,
    PRIMARY KEY (project, name)
);

CREATE TABLE ticket (
    id                  TEXT    PRIMARY KEY,             -- ULID
    number              INTEGER UNIQUE CHECK (number > 0), -- human number; NULL until allocated (AGT-?)
    title               TEXT    NOT NULL,
    state               TEXT    NOT NULL REFERENCES state(name),
    priority            TEXT    NOT NULL CHECK (priority IN ('low', 'medium', 'high', 'critical')),
    project             TEXT    REFERENCES project(id),  -- R2: a declared project exists
    repo                TEXT,
    assignee            TEXT    REFERENCES actor(id),
    description         TEXT    NOT NULL DEFAULT '',     -- materialized body text
    created_wall_ms     INTEGER NOT NULL,
    created_counter     INTEGER NOT NULL,
    updated_wall_ms     INTEGER NOT NULL,
    updated_counter     INTEGER NOT NULL,
    archived_wall_ms    INTEGER,
    archived_counter    INTEGER,
    deleted             INTEGER NOT NULL DEFAULT 0 CHECK (deleted IN (0, 1)),
    linked_github       TEXT,
    linked_pr           TEXT,
    linear              TEXT,
    source              TEXT,                            -- JSON {type,url,id,fetched_at} or NULL
    ext                 TEXT    NOT NULL DEFAULT '{}',   -- JSON object
    CHECK ((archived_wall_ms IS NULL) = (archived_counter IS NULL))
);

CREATE INDEX ticket_state ON ticket(state);
CREATE INDEX ticket_project ON ticket(project);
CREATE INDEX ticket_assignee ON ticket(assignee);

-- The ticket's merge state (pm_core::TicketView as JSON), loaded and
-- re-saved by every commit so the CRDT rules run once, in pm-core.
CREATE TABLE ticket_view (
    ticket TEXT PRIMARY KEY REFERENCES ticket(id),
    view   TEXT NOT NULL
);

CREATE TABLE ticket_label (
    ticket TEXT NOT NULL REFERENCES ticket(id),
    label  TEXT NOT NULL CHECK (label <> ''),
    PRIMARY KEY (ticket, label)
);

CREATE INDEX ticket_label_label ON ticket_label(label);

-- A relation lives in the OR-set of the ticket whose op added it (`owner`,
-- one of its endpoints); both endpoints must be tickets (R4: blockers
-- exist). Read with DISTINCT over the endpoints.
CREATE TABLE relation (
    owner       TEXT NOT NULL REFERENCES ticket(id),
    kind        TEXT NOT NULL CHECK (kind IN ('blocks', 'parent', 'superseded_by')),
    from_ticket TEXT NOT NULL REFERENCES ticket(id),
    to_ticket   TEXT NOT NULL REFERENCES ticket(id),
    PRIMARY KEY (owner, kind, from_ticket, to_ticket)
);

CREATE INDEX relation_from ON relation(from_ticket);
CREATE INDEX relation_to ON relation(to_ticket);

CREATE TABLE comment (
    id          TEXT    PRIMARY KEY,             -- the comment.add op id
    ticket      TEXT    NOT NULL REFERENCES ticket(id),
    author      TEXT    NOT NULL REFERENCES actor(id),
    hlc_wall_ms INTEGER NOT NULL,
    hlc_counter INTEGER NOT NULL,
    body        TEXT    NOT NULL
);

CREATE INDEX comment_ticket ON comment(ticket, hlc_wall_ms, hlc_counter);

-- Structured markers (README §Data model): hold, waiver (several, ordered
-- by position), not_before, parked. `data` is the marker's JSON.
CREATE TABLE marker (
    ticket   TEXT    NOT NULL REFERENCES ticket(id),
    kind     TEXT    NOT NULL CHECK (kind IN ('hold', 'waiver', 'not_before', 'parked')),
    position INTEGER NOT NULL DEFAULT 0,
    data     TEXT    NOT NULL,
    PRIMARY KEY (ticket, kind, position)
);

-- The op log. `seq` is this replica's append order; op_id, hlc and actor
-- are the envelope (pm_core::Op). The NOT NULL / CHECK constraints are the
-- AC6 guarantee: no op without an actor or an HLC.
CREATE TABLE ops (
    seq         INTEGER PRIMARY KEY AUTOINCREMENT,
    op_id       TEXT    NOT NULL UNIQUE,
    hlc_wall_ms INTEGER NOT NULL,
    hlc_counter INTEGER NOT NULL,
    actor       TEXT    NOT NULL REFERENCES actor(id) CHECK (actor <> ''),
    entity      TEXT    NOT NULL,
    kind        TEXT    NOT NULL,
    payload     TEXT,                            -- JSON; NULL for unit kinds (hold.clear, tombstone)
    version     INTEGER NOT NULL
);

CREATE INDEX ops_entity ON ops(entity, seq);
CREATE INDEX ops_hlc ON ops(hlc_wall_ms, hlc_counter);
