# pm app HTTP API (v1)

The localhost API `pm app` serves for the ui-leaf views (AGT-1401;
projects/pm/README.md §Surfaces, decision A6: "the P4 editor talks to a
localhost API served by pm; the ui-leaf view never writes the store
directly"). Every JSON shape here is the CLI's — **Ticket**, **Project**,
`pm list`, `pm ready` — as `docs/cli-contract.md` defines them, and every
write is a normal op: it lands in the log through the same
`Store::commit_batch` a CLI verb uses, so it reaches the outbox and
`pm sync` pushes it like any other. Claims are not served (they need the
hub, AGT-1397): a view that wants one runs `pm claim`.

Enforced by `crates/pm/tests/app.rs`, which runs the built binary against
a temp workspace and compares each `GET` with the CLI's `--json` output.

## Launch

```sh
pm app --json [--idle SECS] [--allow-origin ORIGIN]...
```

prints **one line** and keeps running:

```json
{"schema":1,"url":"http://127.0.0.1:52341","token":"pma_…","pid":4242,"workspace":"/…/agt","actor":"matt","idle_secs":30,"allowed_origins":[]}
```

- `url` — the server: `127.0.0.1` only, on a port the OS picked.
- `token` — the bearer token, minted once per launch (`pma_` + 32 CSPRNG
  bytes as hex). It is printed here and nowhere else; the launcher
  (AGT-1402) hands it to the view. Keep it out of URLs, logs and argv.
- `actor` — who every op this server commits is attributed to: the usual
  resolution (`PM_ACTOR`, `--as`, `$USER`) at launch.
- `idle_secs` — see [Lifetime](#lifetime).
- `allowed_origins` — the `--allow-origin` values, normalized.

Without `--json` the same facts print as `url:` and `token:` lines.

## Access

Three checks run before any route, in this order; a request failing one
never reaches a handler.

1. **`Host`** must be `127.0.0.1:<port>` or `localhost:<port>` — this
   server's own address. Anything else is `403`. A DNS-rebinding page
   reaches the port with its own hostname in `Host`, which is exactly what
   this refuses.
2. **`Origin`**, when the request carries one (every browser `fetch`
   does), must be one the launcher allowed with `--allow-origin` — the
   view's own origin, e.g. `http://127.0.0.1:5173`. Any other origin is
   `403`. With no `--allow-origin`, no browser origin is answered at all;
   `curl` and other non-browser clients send no `Origin` and are unaffected.
   An allowed origin gets the CORS headers it needs and nothing more:
   `Access-Control-Allow-Origin: <that origin>` (never `*`), `Vary: Origin`,
   `Access-Control-Allow-Headers: authorization, content-type`,
   `Access-Control-Allow-Methods: GET, POST, OPTIONS`; no credentials. A
   preflight `OPTIONS` from an allowed origin is `204` with those headers.
3. **`Authorization: Bearer <token>`** on every request, compared in
   constant time. Missing or wrong is `401` with `WWW-Authenticate: Bearer`.

There are no cookies and no sessions: the token is the session.

## Errors

Every error body is `{"schema": 1, "error": "<message>"}`. The status is
the CLI's exit code, mapped: usage (`2`) is `400`, not found (`3`) is
`404`, taken (`75`) is `409`, anything else `500`. An unknown route is
`404` in the same shape.

## Reads

### `GET /workspace`

```jsonc
{
  "schema": 1,
  "id": "<ULID>",                 // the workspace's ULID
  "prefix": "AGT",
  "states": [{"name": "triage", "category": "unstarted", "position": 0}, ...],
  "gate_labels": ["manual", ...],
  "actor": "matt",                // who this server's ops are attributed to
  "workspace": "/path/to/dir"
}
```

### `GET /tickets[?…]`

`pm list --json`: a bare array of **Ticket**. Query keys are `pm list`'s
flags: `project`, `state`, `label`, `repo`, `assignee`, `github`
(comma-separated, OR within a key, AND across keys), `search`, and the
booleans `held` and `archived` (`true`/`false`).

### `GET /tickets/{id}`

`pm show --json`: **Ticket** plus its `comments`
(`[{"author", "at", "body"}, ...]`, AGT-1430). `{id}` is a display id
(`AGT-12`) or a ULID, as every CLI `<ID>` is; `AGT-?` is `400` (see
cli-contract §Ticket ids). The list and ready shapes above carry no
`comments`, exactly as `pm list`/`pm ready` do not.

### `GET /tickets/{id}/body`

The description as a CRDT document, for an editor that binds a
`loro-crdt` document to it:

```jsonc
{
  "schema": 1,
  "id": "AGT-12",
  "ulid": "<ULID>",
  "text": "…",                    // the materialized markdown
  "snapshot": "<base64>"          // the Loro snapshot; import it into a fresh LoroDoc
}
```

### `GET /projects[?status=…]`

`pm project list --json`: `{"schema": 1, "projects": [Project, ...]}`.

### `GET /projects/{id}`

**Project**.

### `GET /ready[?…]`

`pm ready --json`, with the same keys as its flags: `project`, `ids`
(comma-separated), `limit`, `model`, `exclude_label` (comma-separated).

## Writes

Every write is a JSON `POST` on a ticket, answers with the ticket as it
now reads in the `GET /tickets/{id}` shape (**Ticket** plus `comments`,
as `pm show --json` prints it), and commits its ops in one batch. A write that changes nothing commits nothing.

### `POST /tickets/{id}/fields`

```json
{"title": "New title", "priority": "high", "project": null, "linked-pr": ""}
```

One `field.set` per key. The keys are `pm set`'s (`title`, `priority`,
`project`, `repo`, `assignee`, `linked-github`, `linked-pr`, `linear`,
`not_before`, `parked`); `null` or `""` clears an optional field; an
unknown key lands in `ext` exactly as `pm set` puts it there. A project
that does not exist is `404` before anything is written.

### `POST /tickets/{id}/labels`

```json
{"add": ["x", "y"], "remove": ["z"]}
```

A `label.add` per add, a `label.remove` per remove citing the add-tags
this replica observes (OR-set, add-wins — cli-contract §`pm label`).
Either key may be omitted; both empty is `400`.

### `POST /tickets/{id}/state`

```json
{"state": "in-progress", "keep_assignee": false}
```

`pm move`: a `state.transition`, plus the assignee clear when the target
is an unstarted/backlog state unless `keep_assignee` is `true`. An
unknown state is `404`.

### `POST /tickets/{id}/body`

Either form; exactly one `body.edit` op results.

```json
{"update": "<base64>"}
```

A Loro update the editor's own document produced (the bytes
`doc.export({mode: "update", from: <version before the edit>})` gives),
base64 as `docs/cli-contract.md` §`pm log` spells byte payloads. The
server applies it to the ticket's current history first: bytes that are
not a Loro update, or that depend on history this replica does not have
(the editor was bound to a snapshot from elsewhere), are `400` and
nothing is written.

**Peer ids.** Loro identifies every text op by `(peer, counter)`. The
editor's document must use a **fresh random peer id per editing session**
(the `loro-crdt` default) and never a fixed one — two sessions sharing a
peer silently drop each other's edits (see `crates/pm/src/edit.rs`,
AGT-1345). Never set the peer to `0`: that is the materialized view's.

```json
{"text": "the whole description"}
```

The description as a whole. The server rebuilds the body from the
ticket's history under a fresh session peer and diffs to `text`, exactly
as `pm edit` diffs a save, so only the changed span travels and a
concurrent edit merges rather than being reverted. Text equal to the
current description commits nothing.

## `GET /events`

Server-sent events (`text/event-stream`), for as long as the client keeps
the response open:

```
event: hello
data: {"schema":1,"seq":1042}

event: op
id: 1043
data: {"schema":1,"seq":1043,"op_id":"<ULID>","hlc":{"wall_ms":0,"counter":0},"actor":"matt","kind":"field.set","entity":"<ULID>","id":"AGT-12"}

: keep-alive
```

- `hello` — the log's newest `seq` when the stream opened; everything
  after it arrives as `op` events.
- `op` — one per op committed to this replica's log **from anywhere**:
  this API, a `pm set` in a terminal, a `pm sync` pull. `seq` is the
  local log sequence (also the SSE `id`); `entity` the op's entity ULID;
  `id` its display id when the entity is a ticket, else `null` (project,
  document and workspace ops). The payload is never included: refetch
  the ticket. The watcher polls the log every 250 ms and is nudged at
  once after a local commit, so an op shows up well inside a second.
- `lagged` — `{"missed": n}`: the client fell more than 1024 ops behind;
  refetch what it shows.
- A `: keep-alive` comment every 15 s.

A browser `EventSource` cannot send `Authorization`; use `fetch` with a
streaming body reader instead (the ui-leaf views are React and do).

## Lifetime

Every open `/events` stream is a connected view. When none is open, the
server waits `--idle` seconds (default `30`) and exits `0`, with a note on
stderr — measured from launch too, so a launch nobody connects to does
not linger. A view that reloads within the grace simply reconnects.
`--idle 0` never exits (for `curl`-driven use). Killing the process at
any point is safe: every write was its own transaction.
