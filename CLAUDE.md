# CLAUDE.md

Project-specific instructions for Claude Code (auto-loaded into the model's context).

<!-- stamp:begin (managed by `stamp init` — do not edit between markers) -->

## Stamp-protected repository — read AGENTS.md before any git operation

This repository is gated by [stamp-cli](https://github.com/OpenThinkAi/stamp-cli).
**Do not `git commit` directly to protected branches** (typically `main`)
**and do not `git push origin main`** of any commit you didn't produce via
`stamp merge`. The required flow is:

```sh
git checkout -b feature
# ... edit, commit on the feature branch ...
stamp review --diff main..feature       # all reviewers run in parallel
stamp status --diff main..feature       # gate check (exit 0 = open)
git checkout main
stamp merge feature --into main         # signs the merge
git push origin main                    # OR `stamp push main` if origin is a stamp server
```

Key commands: `stamp provision` — provision a new repo; `stamp review` — run reviewers; `stamp merge` — sign a merge; `stamp push` — push to a stamp server.

**The full reference is at [`AGENTS.md`](./AGENTS.md) at the repo root** —
read it before any git command. It covers the mode (server-gated vs.
local-only), what NOT to do, where things live, and how to recover when stamp
blocks you.

**One exception:** the very first commit that ADDS `.stamp/` + `AGENTS.md` +
`CLAUDE.md` to a fresh repo is allowed to land directly on the current branch
(there's nothing to review against). Recent `stamp init` runs do this commit
automatically. Every subsequent change goes through the stamp flow.

<!-- stamp:end -->
