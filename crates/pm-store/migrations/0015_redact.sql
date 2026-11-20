-- AGT-2427: redaction.
--
-- `redact` ops are found by kind (`pm doctor` re-applies all of them) and
-- by entity (whether an arriving op is one a redact already names); the
-- partial index keeps both cheap without indexing every other op.
CREATE INDEX IF NOT EXISTS ops_redact ON ops (entity) WHERE kind = 'redact';

-- Whether this replica still owes the hub a one-off fetch of every
-- `redact` op (`pm sync`, `Store::redact_backfill_due`). A hub withholds
-- redact ops from a client that does not say it understands them, so a
-- replica that synced as an older build may have pulled past some. One
-- that has pulled anything (cursor > 0) when it gains this table owes the
-- fetch; a fresh or never-pulled one will pull every redact in order.
--
-- Bookkeeping like the rest of sync_*: written directly, never derived
-- from the op log, untouched by `pm doctor --rebuild`. Idempotent so the
-- migration can run again.
CREATE TABLE IF NOT EXISTS sync_redact_backfill (
    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
    due       INTEGER NOT NULL CHECK (due IN (0, 1))
);
INSERT OR IGNORE INTO sync_redact_backfill (singleton, due)
    SELECT 1, pulled_seq > 0 FROM sync_state;
INSERT OR IGNORE INTO sync_redact_backfill (singleton, due) VALUES (1, 0);
