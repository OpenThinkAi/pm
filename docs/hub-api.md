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

The token's workspace and name — a probe for `pm hub login`.

```json
{"workspace": "saltline", "token_id": 3, "name": "studio"}
```

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

Response `200`, one entry per op **in batch order**:

```json
{"ops": [{"op_id": "01K6…", "seq": 42, "stored": true},
         {"op_id": "01K6…", "seq": 17, "stored": false}]}
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
- An empty batch is `200 {"ops": []}`.

Errors (all JSON, `error` names the case, `reason` says what to fix):

| Status | `error` | Extra fields | When |
|---|---|---|---|
| `400` | `invalid_op` | `index` (position in the batch), `op_id` (if the JSON had one) | an op does not parse as `pm_core::Op`, its `actor` is empty, or its `version` is newer than the hub's |
| `400` | `invalid_batch` | — | the body is not UTF-8 / not JSON / not `{"ops": [...]}`, or the batch has more than 1000 ops |
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
