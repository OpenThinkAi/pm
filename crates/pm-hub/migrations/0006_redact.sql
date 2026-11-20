-- AGT-2427: `redact` ops. Every push looks up the redacts on the entities
-- it touches (`redact.rs`), a pull filters them out for clients that do
-- not ask for them, and an upgraded client fetches them all once
-- (`GET ops?kind=redact`); this partial index serves all three without
-- indexing every other op. No existing row changes: a hub upgraded over a
-- live log holds no redact ops yet. Additive only.
CREATE INDEX IF NOT EXISTS ops_workspace_redact
    ON ops (workspace_id, entity, seq) WHERE kind = 'redact';
