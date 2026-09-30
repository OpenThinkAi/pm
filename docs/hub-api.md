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

`503` with an empty body means the hub could not reach Postgres; retry.

## `GET /health` (open)

```json
{"status": "ok", "schema_version": 1, "op_version": 1}
```

`schema_version` is read live from the database; `op_version` is the
`pm_core::OP_VERSION` this build writes and understands.

## `GET /w/{workspace}/whoami`

The token's workspace and name — a probe for `pm hub login` — and the
workspace's sync mode.

```json
{"workspace": "saltline", "token_id": 3, "name": "studio", "seeded": false}
```

`seeded: false` means the workspace is still in **seed mode** (see
[Ticket numbers](#ticket-numbers-agt-1391)): the first sync (AGT-1396)
has yet to upload the client's log and end the seed. `true` means the
hub is the workspace's number authority.

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
| `400` | `invalid_op` | `index` (position in the batch), `op_id` (if the JSON had one) | an op does not parse as `pm_core::Op`, its `actor` is empty, or its `version` is newer than the hub's |
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
same shape as a push's); and marks the workspace seeded (`whoami` now
says `seeded: true`). An empty seed — a workspace with no log to bring —
ends the same way with `{"number_floor": 0}`: allocation then starts at 1.

Errors: `400 invalid_body` (not `{"number_floor": <n>}`), `409
already_seeded` (the seed already ended; the hub is the authority — a
client retrying a lost response treats this as done). Auth failures are
the usual bare `404`.
