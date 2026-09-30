-- AGT-1467: pulled ops this replica could not apply, kept out of the log
-- instead of failing every later pull (`Store::apply_pulled_page`, see
-- src/sync.rs for the rules and why every replica decides the same way).
--
--   status 'parked':  the op waits on something not here yet (its ticket,
--                     project, state, document binding, or the edits a
--                     body.edit builds on). It is retried when an op that
--                     could supply it lands, and given up on ('refused')
--                     after `sync::MAX_PARK_RETRIES` retries.
--   status 'refused': the op can never apply here (an unsafe id, a stamp
--                     out of range, a duplicate project.create, a number
--                     already taken, an oversized body.edit, ...). Kept for
--                     `pm doctor` and never retried.
--
--   hub_seq:     the op's seq in the hub log — the order parked ops are
--                retried in, the same on every replica.
--   wake:        parked only: '*' (wait for any config op) or the entity
--                id whose next landed op may supply what is missing.
--   attempts:    parked only: retries so far.
--   op:          the op's JSON, exactly as it would enter the log.
--
-- Bookkeeping like the rest of sync_*: written directly, never derived from
-- or replayed into the op log, untouched by `pm doctor --rebuild`, never
-- pushed. Written idempotently (IF NOT EXISTS) so the migration can run
-- again against a database that already has it.
CREATE TABLE IF NOT EXISTS sync_quarantine (
    op_id       TEXT PRIMARY KEY CHECK (op_id <> ''),
    hub_seq     INTEGER NOT NULL,
    kind        TEXT NOT NULL,
    entity      TEXT NOT NULL,
    status      TEXT NOT NULL CHECK (status IN ('parked', 'refused')),
    wake        TEXT,
    attempts    INTEGER NOT NULL DEFAULT 0 CHECK (attempts >= 0),
    reason      TEXT NOT NULL,
    recorded_ms INTEGER NOT NULL,
    op          TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS sync_quarantine_status ON sync_quarantine (status, hub_seq);
