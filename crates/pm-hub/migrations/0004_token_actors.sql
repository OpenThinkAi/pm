-- AGT-1450 (oaudit 2026-09-30): bind each token to the actors it may
-- author ops as.
--
-- `actors` is a list of actor patterns: an exact actor (`matt`), a
-- prefix ending in `*` (`claude:*`), or `*` alone (any actor). NULL is
-- a token minted before bindings existed: it stays unrestricted
-- ("legacy, any actor") until `pm-hub token bind` restricts it, so a
-- hub upgraded over live tokens keeps accepting every push it accepted
-- before. Additive only: no existing row changes.
ALTER TABLE tokens ADD COLUMN IF NOT EXISTS actors text[];
