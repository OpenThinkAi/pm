-- AGT-1393: client sync state (projects/pm/README.md §Sync & hub, "Client
-- state"): which local ops the hub has not seen, where the last pull left
-- off, and which tickets still await a hub-issued number. Like
-- backup_target, this is bookkeeping written directly — none of it is
-- derived from, or replayed from, the op log, and `pm doctor --rebuild`
-- leaves it alone.
--
-- Written idempotently (IF NOT EXISTS / OR IGNORE) so re-running the file
-- against a database that already has it is a no-op.

-- One row per workspace (one workspace per database, as `workspace`).
--   pushed_through: every op with seq <= this is known to the hub. The
--                   outbox is the ops after it, less those in sync_pushed.
--   pulled_seq:     the hub's sequence number this replica last pulled
--                   through (`pull since <seq>`); 0 = never pulled.
CREATE TABLE IF NOT EXISTS sync_state (
    singleton      INTEGER NOT NULL DEFAULT 1 UNIQUE CHECK (singleton = 1),
    pushed_through INTEGER NOT NULL DEFAULT 0 CHECK (pushed_through >= 0),
    pulled_seq     INTEGER NOT NULL DEFAULT 0 CHECK (pulled_seq >= 0)
);
INSERT OR IGNORE INTO sync_state (singleton) VALUES (1);

-- Ops above pushed_through that the hub already has: acknowledged out of
-- order, or pulled from the hub in the first place (a foreign op is never
-- outbox). Rows fold into pushed_through as soon as the gap below them
-- closes, so this stays small.
CREATE TABLE IF NOT EXISTS sync_pushed (
    seq INTEGER PRIMARY KEY REFERENCES ops(seq)
);

-- Tickets created while a hub is configured: they carry no number (`AGT-?`)
-- until the hub allocates one. Deliberately no foreign key to ticket —
-- `pm doctor --rebuild` empties and refills the ticket tables, and this row
-- must survive that. Cleared once the ticket has a number.
CREATE TABLE IF NOT EXISTS pending_number (
    ticket TEXT PRIMARY KEY CHECK (ticket <> '')
);
