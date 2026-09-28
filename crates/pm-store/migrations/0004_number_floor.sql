-- AGT-1347: the number allocator's floor. `pm import vault` raises it to the
-- greatest human number the vault has minted, so pm never re-issues a
-- number the vault handed out during the cutover window even if the ticket
-- that carried it is not (yet) in this database. Allocation reads
-- MAX(MAX(ticket.number), number_floor) + 1 (commit.rs). Workspace
-- configuration, written directly like the rest of the row; never derived
-- from the op log.
ALTER TABLE workspace ADD COLUMN number_floor INTEGER NOT NULL DEFAULT 0 CHECK (number_floor >= 0);
