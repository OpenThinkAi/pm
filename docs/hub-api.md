# pm-hub HTTP API (v1)

The sync hub (`crates/pm-hub`, README §Sync & hub). Clients push their
local op log, pull everyone else's ops, and merge with the same `pm-core`
rules; the hub stores ops opaquely and hands out the one transport order.
JSON in, JSON out.

## Auth

Every route except `GET /health` is under `/w/{workspace}/` and needs
`Authorization: Bearer <token>` with a token minted for that workspace
(`pm-hub token create <name> --workspace <id>`, AGT-1388). Any auth
failure — no header, a malformed one, an unknown or revoked token, a
token for another workspace, a workspace that does not exist — is the
same bare `404` an unknown route gets, so a caller without a grant learns
nothing (think-hub precedent). A wrong method on a real route is that
404 too.

### Transport security

- **Client to hub.** The bearer token rides every request, so `pm hub
  login` refuses an `http://` URL unless the host is loopback (`localhost`,
  `127.0.0.0/8`, `::1`) and exits 2. A non-loopback `http://` URL already in
  config.toml is refused by `pm hub status` and every sync/claim call with
  a message to re-login over `https://`.
- **Hub to Postgres.** `DATABASE_URL`'s `sslmode` picks the transport
  (AGT-1451): absent, `disable` or `prefer` is plaintext — correct for
  Railway's private network (`postgres.railway.internal`) and loopback, and
  what production uses. `require`, `verify-ca` and `verify-full` all use TLS
  via rustls with the bundled Mozilla roots (`webpki-roots`) and always
  verify the certificate chain and host name; there is no
  accept-any-certificate mode, so a private-CA or self-signed server is
  refused. Use TLS for any `pm-hub token ...` run from outside the private
  network or any deployment reaching Postgres over a public address. The
  same applies to the admin CLI, which shares the connect path.

`503` with an empty body means the hub could not reach Postgres; retry.

### Token actor bindings (AGT-1450)

A token authenticates a *machine*, and one machine pushes for several
actors (README decision A7: the owner, `pm-sync`, its `claude:*`
agents). Each token therefore carries a set of **actor patterns** it may
author ops as (`tokens.actors`, schema version 4):

| Pattern | Matches |
|---|---|
| `matt` | exactly the actor `matt` |
| `claude:*` | any actor starting `claude:` (a trailing `*` is a prefix match; `*` may appear only at the end) |
| `*` | any actor |

- `pm-hub token create <name> --workspace <id> --actor <pattern> …`
  (repeat `--actor` or comma-separate) mints a bound token. Without
  `--actor` the token is recorded as `*` and `create` prints a note —
  binding is opt-in.
- `pm-hub token bind <id> --actor <pattern> …` replaces a live token's
  patterns (effective on its next request, no restart). `--actor '*'`
  makes it explicitly unrestricted.
- A token minted before bindings existed has `actors = NULL`: it stays
  **unrestricted** ("legacy") and `pm-hub token list` shows it as `any
  (legacy, unbound)` until `token bind` restricts it. `token list` has
  an `ACTORS` column: `ID WORKSPACE NAME ACTORS CREATED REVOKED`.

On push, every **fresh** op's `actor` must match one of the token's
patterns (`400 actor_not_allowed` otherwise, batch refused whole). An op
the workspace already has is acknowledged whoever authored it — nothing
is stored for it. The actor `hub` is reserved for the hub's own ops:
once the workspace is seeded no client may push it (`400
reserved_actor`, whatever the token); while seeding, only an
unrestricted token (legacy or `*`) may — a hub-to-hub reseed carries the
old hub's number ops. **Seeding** is otherwise ordinary pushing: a
seed's historical ops keep their original actors, so seed with a token
whose patterns cover every actor in the log (an unrestricted one, or
e.g. `matt,pm-sync,claude:*`).

**Rollout.** Deploying this hub runs migration 4 (`ALTER TABLE tokens
ADD COLUMN IF NOT EXISTS actors text[]`), which only adds a nullable
column: every existing token, including the Studio's `studio` token,
keeps authenticating and keeps pushing as any actor exactly as before.
To restrict it afterwards, run `pm-hub token list` to confirm the
actors in use, then `pm-hub token bind <id> --actor matt --actor pm-sync
--actor 'claude:*'` (adjusting to that machine's actors). The admin
subcommands refuse to run against a schema other than the one they were
built for, so upgrade (start) the hub before using a newer `pm-hub token
…` locally.

## `GET /health` (open)

```json
{"status": "ok", "schema_version": 4, "op_version": 1, "build": "<git sha>"}
```

`schema_version` is read live from the database; `op_version` is the
`pm_core::OP_VERSION` this build writes and understands. `build` is the
git sha the binary was built from (`PM_HUB_BUILD_SHA` at build time;
`"unknown"` when unset). `scripts/deploy-hub.sh` writes the sha to
`.build-sha` in the upload (`railway up` ships no `.git` and takes no
build args), the Dockerfile passes it to cargo, and the script waits for
`/health` to report it.

## `GET /w/{workspace}/whoami`

The token's workspace and name — a probe for `pm hub login` — and the
workspace's sync mode.

```json
{"workspace": "saltline", "token_id": 3, "name": "studio", "seeded": false}
```

`seeded: false` means the workspace is still in **seed mode** (see
[Ticket numbers](#ticket-numbers-agt-1391)): the first sync (AGT-1396)
has yet to upload the client's log and end the seed. `true` means the
hub is the workspace's authority for numbers and claims. `seeded` is read from the
database, so this route can answer `503` after the token authenticated
(a transient database error; retry).

## `POST /w/{workspace}/ops` — push (AGT-1389)

Request: `{"ops": [<op>, ...]}`, each op in the op log's wire format,
exactly the JSON `pm backup` writes one per line:

```json
{"op_id": "01K6…", "hlc": {"wall_ms": 1790000000000, "counter": 0},
 "actor": "studio", "entity": "01K6…", "kind": "label.add",
 "payload": {"label": "x"}, "version": 1}
```

Byte payloads (`body.edit`, `project.doc_add`) are base64 in `payload`
(AGT-1378). The hub parses each op as a `pm_core::Op` to reject a
malformed one and to fill its indexed columns (`op_id`, `hlc`, `actor`,
`entity`, `kind`), and stores the op's JSON text exactly as received — a
pull serves the same bytes back. An op's `version` may not be newer than
the hub's `op_version`.

**Stamps (oaudit 2026-09-30).** Every op's `hlc` is checked before
anything is stored or folded, so a forged stamp can neither crash a
replica nor win every last-writer-wins register forever:

- `wall_ms` must be at most `i64::MAX` (both stores keep it in a signed
  64-bit column) and `counter` at most `u32::MAX - 1` (so a clock that
  receives it can still advance) — else `400 invalid_stamp`;
- `wall_ms` may be at most **one day** (`pm_core::MAX_FUTURE_SKEW_MS`,
  86 400 000 ms) ahead of the hub's own wall clock at the time of the
  push — else `400 future_stamp`. A client whose clock runs more than a
  day fast must fix it before it can push.

There is no lower bound: stamps from the past are always accepted, so a
seed's historical ops (years old, or `wall_ms` near 0 from an import)
land as before. A replica applies the same range checks to every op it
pulls (`pm_store::Store::apply_pulled` → `StoreError::InvalidStamp`,
never a panic), with a far looser future bound of 365 days
(`pm_store::PULL_MAX_FUTURE_SKEW_MS`) so a replica whose own clock runs
behind still pulls honest ops. `pm_core::Clock` never overflows its
counter: a spent counter rolls into the next millisecond.

Response `200`, one entry per op **in batch order**, plus the number of
every ticket a `ticket.create` in the batch made (see [Ticket
numbers](#ticket-numbers-agt-1391)):

```json
{"ops": [{"op_id": "01K6…", "seq": 42, "stored": true},
         {"op_id": "01K6…", "seq": 17, "stored": false}],
 "numbers": [{"entity": "01K6…", "number": 1378, "seq": 43, "allocated": true,
              "op": {"op_id": "01K6…", "hlc": {"wall_ms": 1790000000100, "counter": 0},
                     "actor": "hub", "entity": "01K6…", "kind": "field.set",
                     "payload": {"field": "number", "value": 1378}, "version": 1}}]}
```

- `seq` is the hub's sequence number for the op: the cursor `GET
  /ops?since=` (AGT-1390) pages by. Per workspace, seqs are assigned in
  commit order — an op with a smaller seq is never committed after one
  with a larger seq is visible — so a puller that records the last seq it
  saw misses nothing. Within a batch, seq order is batch order and the
  batch's seqs are contiguous. Seqs are not contiguous *across* the
  hub (one sequence serves every workspace).
- `stored: false` means the workspace already had that `op_id` (from an
  earlier push, or earlier in this batch): the entry carries the existing
  seq and nothing was written. `op_id` is an op's identity; the content
  of a repeat is neither compared nor overwritten. Re-pushing an
  acknowledged batch is therefore a no-op, which is what makes an
  interrupted push safe to retry.
- A batch is all-or-nothing: on any `4xx` nothing in it was stored.
  The one per-op outcome inside a `200` is a refused `claim` (see
  [Claims](#claims-agt-1392)): its entry is `{"op_id": …, "seq": null,
  "stored": false, "rejected": {…}}`, nothing is stored for it, and the
  rest of the batch lands as usual. `rejected` is present only on such
  an entry.
- `numbers` has one entry per ticket created by a `ticket.create` in the
  batch that has a number, in the order the numbers were issued:
  `allocated: true` when this push allocated it, `false` when the ticket
  already had one (a re-pushed create, or a number the seed carried).
  `op` is the `field.set number` op exactly as the log stores it and
  `seq` its seq, so a client can apply it at once rather than wait for
  the pull that will also deliver it. Empty when nothing in the batch
  is a numbered create.
- An empty batch is `200 {"ops": [], "numbers": []}`.

Errors (all JSON, `error` names the case, `reason` says what to fix):

| Status | `error` | Extra fields | When |
|---|---|---|---|
| `400` | `invalid_op` | `index` (position in the batch), `op_id` (if the JSON had one) | an op does not parse as `pm_core::Op`, its `actor` is empty, its `version` is newer than the hub's, a `field.set number` carries a number outside `1..=2^53-1`, or it does not fold into its ticket with `pm_core::apply` (a relation that does not touch the ticket) |
| `400` | `invalid_stamp` | `index`, `op_id` | the op's `hlc.wall_ms` exceeds `i64::MAX` or its `hlc.counter` is `u32::MAX` (see Stamps above) |
| `400` | `future_stamp` | `index`, `op_id` | the op's `hlc.wall_ms` is more than one day ahead of the hub's clock |
| `400` | `invalid_id` | `index`, `op_id` | a `workspace.set prefix`, `project.create` (id or parent) or `project.set parent` whose value is not a safe file-path component (`pm_core::ids::is_safe_component`: ASCII letters, digits, `-`, `_`, `.`, at most 64 bytes, not starting with `.`) — clients use these in export/backup paths (AGT-1453); a replica refuses the same on pull (`StoreError::InvalidId`) |
| `400` | `actor_not_allowed` | `index`, `op_id` | a fresh op's `actor` matches none of the token's actor patterns; `reason` names the token and its patterns (see [Token actor bindings](#token-actor-bindings-agt-1450)) |
| `400` | `reserved_actor` | `index`, `op_id` | a fresh op authored as `hub` — from any token once the workspace is seeded, from a bound token while seeding |
| `400` | `foreign_workspace` | `index`, `op_id` | a config op (`workspace.set`, `state.upsert`, `actor.upsert`) for a workspace Ulid other than the one this hub workspace's config already belongs to |
| `400` | `invalid_batch` | — | the body is not UTF-8 / not JSON / not `{"ops": [...]}`, or the batch has more than 1000 ops |
| `400` | `number_not_allowed` | `index`, `op_id` | a `field.set number` pushed to a seeded workspace (only the hub numbers tickets then, whatever the op's `actor`) |
| `400` | `duplicate_number` | `index`, `op_id` | in seed mode, a `field.set number` whose number another ticket already holds, or whose ticket is already numbered |
| `413` | `too_large` | — | the body exceeds 64 MiB (by `Content-Length`, answered before reading; or discovered while reading) |

Limits: **1000 ops per batch, 64 MiB per request body**
(`pm_hub::ops::{MAX_BATCH_OPS, MAX_BODY_BYTES}`). The byte limit leaves
room for a single large op — the Studio's seed log holds one 23 MB
`body.edit` — plus a batch around it; a client should size batches by
both count and bytes.

## `GET /w/{workspace}/ops?since=<seq>&limit=<n>` — pull (AGT-1390)

The workspace's ops with `seq > since`, in seq order, at most `limit` of
them. Both parameters are optional: `since` defaults to `0` (the whole
log), `limit` to **500** and is clamped to **1000**
(`pm_hub::pull::{DEFAULT_PAGE_OPS, MAX_PAGE_OPS}`, the push batch cap).
Any other parameter is a `400`.

Response `200`:

```json
{"ops": [{"seq": 41, "op": {"op_id": "01K6…", "hlc": {"wall_ms": 1790000000000, "counter": 0},
                              "actor": "studio", "entity": "01K6…", "kind": "label.add",
                              "payload": {"label": "x"}, "version": 1}},
         {"seq": 42, "op": {…}}],
 "next": 42,
 "head": 1207}
```

- Each `op` is the op's JSON text **exactly as the hub stored it** — the
  bytes a client pushed, spacing and key order included (the hub never
  re-serializes an op). Ops the hub authored itself (number allocation,
  AGT-1391) are in the log like any other and come back the same way.
- `next` is the seq to pass as `since` on the next request: the last
  `seq` in `ops`, or the request's own `since` when `ops` is empty.
- `head` is the workspace's largest seq (`0` for an empty log), read in
  the same snapshot as the page. `next < head` means more ops were
  already waiting; `next >= head` means the client had everything as of
  that snapshot. A client pages `since = next` until `next >= head`.
- The cursor is gap-safe: seqs are handed out and committed in order
  per workspace (see "How the order is kept"), so every seq at or below
  `head` is committed when `head` is read, and no op with a seq at or
  below a `next` the client has seen can appear later. Seqs are **not**
  contiguous (one sequence serves every workspace; a rolled-back push
  burns its values) — never count them or infer a missing op from a
  hole.
- `since` at or beyond `head` is `200 {"ops": [], "next": <since>,
  "head": <head>}`.
- Pulls run on a connection pushes never use and read only committed
  rows, so a pull never waits for an in-flight push and never sees a
  partial batch.

Errors:

| Status | `error` | When |
|---|---|---|
| `400` | `invalid_query` | `since` is not an integer `>= 0`, `limit` is not an integer `>= 1`, the query names another parameter, or it is not decodable |

The body is `{"error": "invalid_query", "reason": "…"}`; `reason` quotes
the offending value.

### How the order is kept

Postgres's `bigserial` on its own does not commit in the order it hands
out values: two concurrent transactions can take 5 and 6 and commit 6
first, and a puller that saw 6 would skip 5 forever. Each push runs in
one transaction that first locks the workspace's row (`SELECT … FROM
workspaces … FOR NO KEY UPDATE`, held to commit) and only then inserts,
so a workspace's pushes take sequence values and commit one after
another. Pushes share one dedicated database connection behind a mutex
(a transaction needs a connection to itself); reads, auth and pulls use
another connection and never wait for a push.

## Ticket numbers (AGT-1391)

Human numbers (`AGT-1391`) are allocated by one authority and never
merged (README §Conflict semantics), so a number is never issued twice
across machines and never collides with one the vault or the Studio
minted before the hub existed (the workspace's `number_floor`,
AGT-1347). On the wire a number is a `field.set number` op on the
ticket, separate from its `ticket.create` — the same op `pm new` logs
locally when no hub is configured.

A workspace is in one of two modes:

- **Seed mode** — every workspace starts here (`token create` makes it
  so; `whoami` reports `seeded: false`). The client bringing an existing
  log pushes it as is, its own `field.set number` ops included; the hub
  records those numbers and allocates none. A number that collides with
  one already recorded, or a second number for one ticket, is a `400
  duplicate_number` (the client enforces the same rules locally). A
  create the seed leaves unnumbered — a ticket made after `pm hub login`,
  pending a hub number — stays unnumbered until the seed ends.
- **Authoritative** — after `POST /w/{workspace}/seeded`. Every fresh
  `ticket.create` in a push is numbered **in the push's transaction**:
  the hub takes the workspace's `next_number`, appends its own
  `field.set number` op (actor `hub`, fresh `op_id`) to the log right
  after the batch — the batch's seqs stay contiguous and the hub's ops
  follow them — and reports it in the response's `numbers`. A pushed
  `field.set number` is refused with `400 number_not_allowed`. A
  re-pushed create (`stored: false`) allocates nothing: its ticket is
  already numbered, and the response repeats the existing number.

Allocation runs under the same per-workspace row lock that orders seqs,
so pushes to one workspace allocate one at a time, and the `numbers`
table's unique `(workspace, number)` key makes a double issue a database
error rather than a silent duplicate.

**Stamps.** The hub keeps one `pm_core::Clock` per workspace. Before
allocating it observes the greatest HLC in the batch; each hub op is
stamped with `Clock::receive` of the create's HLC, so it is strictly
later than the create, than every op in the batch, and than every op the
hub issued before. Actor `hub` is reserved for the hub, so its `(hlc,
actor)` stamps are unique and a client's LWW register for `number` takes
the hub's op as the ticket's number. Pull (`GET /ops`, AGT-1390) serves
hub ops like any other, so a client that missed a push response learns
its numbers on the next pull (pm-store clears `pending_number` when the
op arrives).

## Claims (AGT-1392)

A `claim` is the one op that is not a CRDT (README §Conflict semantics):
`claim if state is unstarted and unassigned`, decided by the authority
first-come. Once a workspace is seeded, the hub is that authority.

### The hub's view

The hub keeps, per workspace, a materialized `pm_core::TicketView` per
ticket and a `pm_core::WorkspaceView` (the states) — tables
`ticket_views` / `workspace_views` (schema version 3) — and folds every
op it stores into them **inside the push transaction, in batch order**,
with pm-core's own `apply` / `apply_workspace`: the functions pm-store's
commit path runs. The hub applies no merge logic of its own; its view of
a ticket is what a client's `pm doctor --rebuild` of the same ops
produces (state, assignee, deleted, number, title, labels, …), and
`crates/pm-hub/tests/claims.rs` checks exactly that. Hub-authored ops
(numbers) and the ops a seed carries fold like any other. Two kinds are
stored but not folded: `body.edit` (the description is a Loro document
nothing conditional reads, and the Studio's log holds a 23 MB one) and
the `project.*` kinds. Ops fold in any order (a `field.set` ahead of its
`ticket.create` starts a fresh view, as on a client). A hub upgraded over
an existing log rebuilds the views from it in the migration to version 3.

The states come only from the workspace's own `state.upsert` ops: a
workspace that has pushed no config admits no claim (nothing is
`unstarted`), as a client with no states cannot claim either. The first
config op fixes the workspace's Ulid; config for another Ulid is a `400
foreign_workspace` (pm-store's `ForeignWorkspace`).

### Arbitration

**Seeded workspace.** A pushed `claim` is admitted only if
`TicketView::claim_admissible` holds against the hub's view at that point
in the batch — the check pm-store runs locally, unchanged. An admitted
claim is stored and folded. A refused one is **not stored** (no seq, no
`ops` row, never served by a pull) and its ack is:

```json
{"op_id": "01K6…", "seq": null, "stored": false,
 "rejected": {"taken_by": "claude:pm-build",
              "at": {"wall_ms": 1790000000200, "counter": 0},
              "state": "in-progress",
              "code": "not_unstarted",
              "reason": "ticket is in state 'in-progress', which is not unstarted"}}
```

- `taken_by`: the ticket's assignee, or `null` when it left `unstarted`
  without one (done, canceled) or is deleted.
- `at`: when the ticket entered the refusing condition — the stamp of the
  write that set its assignee (`already_assigned`), its state
  (`not_unstarted`) or its tombstone (`deleted`). For a claimed ticket
  that is the admitted claim's HLC, which is what `pm claim --json`
  reports locally.
- `state`: the ticket's state at the hub.
- `code`: `not_unstarted` | `already_assigned` | `deleted`, in
  `claim_admissible`'s order of precedence (a deleted ticket is
  `deleted` whatever else holds; a claimed ticket is `not_unstarted`,
  since the claim moved it to a started state).
- `reason`: `pm_core::ClaimRejected`'s message, the string `pm claim
  --json` puts in `reason`.

Everything else in the batch is stored (a `200`; a rejected claim is not
an error — the op was understood and decided). Within a batch, views
advance op by op: of two claims on one ticket the first is admitted and
the second refused. Across pushes, the per-workspace lock serialises
every decision: of any number of concurrent claims on one ticket exactly
one is admitted. A re-push of an admitted claim is idempotent (`stored:
false`, the existing seq; not re-arbitrated). A re-push of a refused one
is a fresh claim — arbitrated again, and admitted if the ticket has since
been freed. The hub admits; pm-core's rules then decide what the admitted
claim's write does, so a client stamps a claim with its current clock: a
claim stamped before the unclaim it follows would be admitted, stored,
and lose the LWW register everywhere.

**Seeding workspace.** Every claim in a seeding push is stored and folded
as a plain write, never arbitrated. The seed is history: each claim in it
was admitted by the then-authority (the client's own database, README
§Authority) when it happened, and the hub "becomes the authority only
after the seed is fully acknowledged". Re-judging would also be wrong on
its face — the seed's `state.upsert` ops may arrive after the claims that
depend on them, and a claim admitted months ago is not void because the
ticket has since been done and re-claimed. This is pm-store's own
replay/pull behaviour (`Fold::Replay` / `Fold::Foreign`).

### What a client does with `rejected` (AGT-1395 `pm sync`, AGT-1397 `pm claim`)

- **`pm claim` against the hub (AGT-1397)** pushes the `claim` op *alone*,
  before logging anything locally. `stored: true` → commit the op locally
  as a pulled (foreign) op — it is already admitted — together with any
  companion write (`--branch`'s `field.set ext`), which then goes out
  through the normal outbox; `rejected` → log nothing, print
  `{taken_by, at, state, reason}` and exit 75 (`code: deleted` → exit 3,
  as the local path does). Do not batch the companion write with the
  claim: it would be stored even when the claim is refused.
- **`pm sync` (AGT-1395)** treats a `rejected` entry as *acknowledged and
  dropped*: call `mark_pushed` for its `op_id` like any other ack, so it
  leaves the outbox and is never re-pushed. Nothing else is required for
  a well-behaved client, because a claim only reaches the outbox when the
  local database was the authority — `pm claim` refuses to claim locally
  while a hub is configured — so a rejected claim in the outbox means a
  claim made before the workspace switched to the hub, i.e. a seed, and
  seeds are never arbitrated. Should one still occur (a log joined after
  another machine's seed), the op stays in the local log — the log is
  append-only and `pm doctor --rebuild` replays it as admitted — so the
  client reconciles by logging a compensating `state.transition` to
  `rejected.state` and `field.set assignee` to `rejected.taken_by`,
  stamped now: both replicas then converge on the hub's answer, and the
  compensation pushes as an ordinary op. `pm sync` should report the
  rejection either way.

### `POST /w/{workspace}/seeded` — end the seed

Request: `{"number_floor": <n>}` — the client's allocator floor
(`workspace.number_floor` in pm-store), the greatest number the seed may
have used.

Response `200`:

```json
{"number_floor": 1376,
 "numbers": [{"entity": "01K6…", "number": 1377, "seq": 12631, "allocated": true, "op": {…}}]}
```

In one transaction under the workspace lock the hub: adopts as the floor
the greater of the request's `number_floor` and the largest number the
seed recorded; sets `next_number` to `floor + 1`; numbers, in log order,
every `ticket.create` the seed left unnumbered (these are `numbers`, the
same shape as a push's, and fold into the hub's views like any other
op); and marks the workspace seeded (`whoami` now says `seeded: true`
and the hub arbitrates claims — see [Claims](#claims-agt-1392)). An empty seed — a workspace with no log to bring —
ends the same way with `{"number_floor": 0}`: allocation then starts at 1.

`number_floor` may be at most 2^53 − 1 (`pm_hub::numbers::MAX_NUMBER`,
the largest integer the views' JavaScript represents exactly), the same
cap a seeded `field.set number` has; all number arithmetic is checked,
so neither can wrap the allocator or leave the workspace stuck in seed
mode.

Errors: `400 invalid_body` (not `{"number_floor": <n>}`, or a floor
above the cap), `409
already_seeded` (the seed already ended; the hub is the authority — a
client retrying a lost response treats this as done). Auth failures are
the usual bare `404`.

### How `pm sync` seeds (AGT-1396)

The client's first sync (`crates/pm/src/seed.rs`; `docs/cli-contract.md`
§`pm sync`) drives the seed with the routes above and nothing else:

1. `GET /whoami` — `seeded: false` means seed (or finish seeding);
   `true` means the hub is the authority and the client only pushes and
   pulls. A client that has never pushed yet holds a log refuses to sync
   into a seeded workspace (that log would be a second seed).
2. `GET /ops?since=0&limit=1000` — the probe. `head == 0`: a fresh seed.
   `head > 0`: an interrupted seed being resumed, **only if every op in
   that page is in the client's log**; a page holding an op the client
   lacks means the workspace was seeded from another log, and the client
   refuses. Ops the probe finds count as pushed.
3. `POST /ops` for the whole outbox in batches, exactly as a push — the
   **config ops first** (`workspace.set`, `state.upsert`, `actor.upsert`,
   `project.*`), then the rest in the client's own log order, so the
   hub's seq order has every state, project and document binding ahead
   of the ops that need them (a legacy log's config ops were backfilled
   at its end by pm-store's migrations 0007/0008) and a replica joining
   later can apply the log page by page. The client's own `field.set
   number` ops go up as they are. Re-pushing after an interruption
   relies on `stored: false` being a no-op.
4. `POST /seeded {"number_floor": <client floor>}` — the client applies
   the returned `numbers[].op` at once and raises its floor to the
   answer's `number_floor`; `409 already_seeded` is treated as done.
   Only now does the client mark itself seeded; its first pull then runs
   from `since=0` and skips its own ops.

An empty replica joining a seeded workspace (`pm init --join`) runs step
1, is marked seeded, and pulls from `since=0`.
