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
  bytes as hex). It is printed here and nowhere else. Keep it out of URLs,
  logs and argv.
- `actor` — who every op this server commits is attributed to: the usual
  resolution (`PM_ACTOR`, `--as`, `$USER`) at launch.
- `idle_secs` — see [Lifetime](#lifetime).
- `allowed_origins` — the `--allow-origin` values, normalized.

`pm app` without `--json` launches the board instead (see
[Launching a view](#launching-a-view)) and prints neither; only when it
cannot (no display, no pinned ui-leaf) does it serve headless and print
the same facts as `url:` and `token:` lines.

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

### `GET /tickets/{id}/body[?since=<base64>]`

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

**Catching up.** With `since` — the editor document's version vector,
`doc.oplogVersion().encode()`, base64 (percent-encode it: `+`, `/` and
`=` are not query-safe; an unencoded `+` read back as a space is
tolerated) — the answer carries `"update"` instead of `"snapshot"`: every
op this replica holds that the version does not cover, as one Loro
update to `doc.import()`. A version *ahead* of the server (the editor's
own unsent edits) is fine; those ops are simply not in the answer, which
is how an open editor pulls another window's, the CLI's or a sync's edits
without a snapshot. When there is nothing new the update is valid and
changes nothing. `since=` (empty) means "from the beginning"; bytes that
are not base64 or not a version vector are `400`. The ticket editor pulls
on every `body.edit` op event for its ticket and after every (re)connect
(`crates/pm/views/lib/body.ts`).

### `GET /projects[?status=…]`

`pm project list --json`: `{"schema": 1, "projects": [Project, ...]}`.

### `GET /projects/{id}`

**Project**.

### `GET /projects/{id}/body[?since=<base64>]`, `GET /projects/{id}/docs/{name}/body[?since=<base64>]`

A project document as a CRDT document (AGT-1405): the design doc, or the
named document `{name}` (percent-encoded; `pm project doc add` creates
them). Exactly `GET /tickets/{id}/body` — the same `text`, the same
`snapshot` without `since` and `update` with it, the same `400`s — with
the document named instead of a ticket:

```jsonc
{
  "schema": 1,
  "project": "pm",
  "doc": null,                    // the design doc; a named document's name otherwise
  "doc_id": "<ULID>",             // the entity its body.edit ops (and their op events) carry
  "text": "…",
  "snapshot": "<base64>"          // or "update" with ?since=
}
```

A document nobody has edited yet is an empty body (its snapshot imports
as `""`). An unknown project or document is `404`; a document whose id
binding has not synced to this replica yet (only a project pulled from
another replica mid-sync) is `500`, as `pm project edit` exits `1` on it.

### `GET /ready[?…]`

`pm ready --json`, with the same keys as its flags: `project`, `ids`
(comma-separated), `limit`, `model`, `exclude_label` (comma-separated).

## Writes

Every write on a ticket is a JSON `POST`, answers with the ticket as it
now reads in the `GET /tickets/{id}` shape (**Ticket** plus `comments`,
as `pm show --json` prints it), and commits its ops in one batch. A write that changes nothing commits nothing.
Filing a ticket (`POST /tickets`) and writing a project document
(`POST /projects/{id}/body`) answer as described under each.

### `POST /tickets`

```json
{"title": "Write the parser", "project": "pm", "priority": "high",
 "labels": ["x"], "description": "…", "repo": "OpenThinkAi/pm", "blocked_by": ["AGT-12"]}
```

`pm new` with those flags (AGT-1405) — only `title` is required — through
the very same path (`verbs::file_ticket`): the workspace's initial state,
the project checked before anything is written (`404`), blockers resolved
(`404` for an unknown one), the same op set, and the same **numbering**:
this machine numbers the ticket when no hub is configured; with a `hub`
in config.toml — reachable or not, the API never contacts it — the
ticket is committed **pending**, reads `"id": "AGT-?"`, `"number": null`,
and is named by its `ulid` until `pm sync` brings the hub's number
(cli-contract §`pm new`, Numbering). Blank strings count as absent; an
unknown key, a bad `priority` or a non-array `labels`/`blocked_by` is
`400`, and nothing is written. Answers `201` with the ticket exactly as
`pm new --json` prints it (**Ticket**, no `comments`). `pm new` puts no
template sections in a description and neither does this. The ops reach
`GET /events` like any other (`ticket.create` first).

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

### `POST /projects/{id}/body`, `POST /projects/{id}/docs/{name}/body`

`{"update": "<base64>"}` or `{"text": "…"}`, exactly as
`POST /tickets/{id}/body` (the same peer-id rules, the same refusal of an
update whose history this replica lacks): one **`body.edit` op on the
document's `doc_id`** (AGT-1344/1413), committed through the same
`Store::commit_doc_edit` `pm project edit` and `pm project doc add` use —
so it joins the outbox, syncs, and `pm doctor --rebuild` rebuilds the
document from it. Text equal to the document commits nothing. Answers the
document header with its text: `{"schema", "project", "doc", "doc_id",
"text"}`.

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

When pm launched the view itself (below) the grace after a disconnect is
`--idle` (default `5` — it is how long `pm edit` lingers after its window
closes) and the first connection gets 60 s (a cold browser plus ui-leaf
compiling the view); ui-leaf exiting ends the server too.

## Launching a view

`pm edit <ID>` (the ticket view), `pm project edit <ID>` (the project
view, AGT-1405 — under exactly `pm edit`'s rules: `--view`, then
`edit.view`, and the default ui-leaf only when stdin and stdout are
terminals) and `pm app` without `--json` (the board) start this server in-process and mount a view in
[ui-leaf](https://github.com/OpenThinkAi/ui-leaf) over its stdio protocol
(`ui-leaf mount`, line-delimited JSON; `crates/pm/src/app/launch.rs`).

**Runtime.** `ui_leaf.path` in config.toml, else the first `ui-leaf` on
`PATH`. When that is the npm package's Node shim, pm runs the native
`ui-leaf-bin` beside it. It must report (`--version`) a version in
`>=1.6.0, <2.0.0` — 1.6.0 is the release the views were built against, and
ui-leaf's wire protocol `"1"` may only break at a new major. Otherwise pm
does not launch it (and says why). No display — `UI_LEAF_NO_OPEN` truthy,
an SSH session, or Linux/BSD without `DISPLAY`/`WAYLAND_DISPLAY` — means no
launch either; `UI_LEAF_NO_OPEN=0` forces one.

**The mount.** Config line: `view` (`ticket`, `board` or `project`), an absolute
`viewsRoot` (below), `data` = `{"schema":1,"view":…,"ticket":"AGT-12"}`
(`"ticket"` is the ULID while the number is pending; the project view's is
`{"schema":1,"view":"project","project":"pm"}`),
`mutations: ["session"]`, `port: 0`, `shell: "app"`, and `csp` = ui-leaf's
strict preset with this API's origin added to `connect-src` and
`'wasm-unsafe-eval'` added to `script-src`. The latter is the one
loosening, and only for WebAssembly: the editors' `loro-crdt`
compiles its wasm module from bytes inlined in the page (ui-leaf serves a
view as a single HTML page, so there is no `.wasm` URL to load), which
CSP otherwise refuses. It does not allow `eval` or `new Function`.

**How the view gets the URL and token: the `session` mutation.** Never a
URL: ui-leaf's `ready` URL carries no token, and its launch fragment
(`#token=`) is ui-leaf's own token, not pm's. Never `data`: ui-leaf
inlines `data` into the page HTML, which its `GET /` serves to any local
process without a token. The view calls `mutate("session")`; the request
travels ui-leaf's token-gated `/mutate` channel and pm replies
`{"schema":1,"url":"http://127.0.0.1:<port>","token":"pma_…"}`. Any other
mutation name is refused — views write through this API. When ui-leaf
reports `ready`, pm adds `http://127.0.0.1:<its port>` (and the
`localhost` spelling) to the allowed origins, before any view can hold
the token.

**Closing.** The view keeps `GET /events` open while it shows. When the
window closes the stream drops; after the grace pm sends ui-leaf
`{"type":"close"}` (killing it after 5 s) and returns. ui-leaf's
`disconnected` event is ignored: it is heartbeat silence, which a
minimized window also produces.

## Views

The views are TSX in `crates/pm/views/`, one file per view plus
`lib/pm.ts`, the shared client (`connect` → the `session` mutation;
`Api.get`/`post` with the bearer token (`post` takes `{keepalive}`); `Api.events`, the fetch-streamed
SSE reader; the `useApi`/`useEvents` hooks). ui-leaf passes each view
`{data, mutate}` and bundles relative imports, React included.

A view imports only relative files and `react`/`react-dom` (ui-leaf
aliases those two; it resolves no other npm package), so the views carry
no npm dependencies; third-party code a view needs is vendored as relative
files (the ticket editor's, below). Logic worth testing lives in plain `.ts` beside them
(`lib/board.ts`, `lib/project.ts`), written in erasable TypeScript so Node runs it
directly: `node --test crates/pm/views-test/*.test.ts` (Node ≥ 22.18; no
install, no browser). Those tests sit outside `crates/pm/views/` so they
are not shipped, and are not part of `cargo test`.

The editors (`lib/editor.tsx`: `BodyEditor`, CodeMirror bound to a
`BodySync`, and `TicketEditor`, the whole ticket editor — `ticket.tsx` is
one filling the window, and the project view hosts one inline) also
import `views/vendor/`: `loro.js` (`loro-crdt`'s
`base64` build — the wasm inlined, ~4.7 MB) and `codemirror.js`
(CodeMirror 6 and `loro-codemirror`, minified, importing `./loro.js` so
there is one Loro instance). ui-leaf bundles relative imports but
resolves no npm package except React, so pm vendors them: they are
generated from the exact versions pinned in
`crates/pm/views-vendor/package.json` by
`cd crates/pm/views-vendor && bun install --frozen-lockfile && bun build.ts`,
and committed (`.gitattributes` marks them `-diff`: a vendor bump reviews
as the pin change, and the build is reproducible — rerunning it on a clean
checkout leaves `git status` clean). Building or installing pm needs no
JavaScript toolchain.

The body binding (`lib/body.ts`, `BodySync`) is DOM-free and binds any
body endpoint — `bodyTransport(api, path)` with `ticketBodyPath(ref)`
(`apiTransport(api, ref)` is that) or a project document's
`docBodyPath(project, tab)` (`lib/project.ts`): a fresh `LoroDoc` (fresh random peer, never 0) imported from the
snapshot; local commits are exported as updates holding only this
session's own ops and POSTed 250 ms after the last keystroke (at most
750 ms after the first unsent one), one request at a time, retried until
a `200`; on close (`pagehide`, or the window going hidden) whatever is
unsent is flushed with `keepalive`. Its tests
(`crates/pm/tests/views/*.test.ts`: `body.test.ts` on a ticket,
`project.test.ts` on a project's documents and "New ticket") run under
node against a real `pm app` from `cargo test` (`crates/pm/tests/views_js.rs`); without node
>= 22 on `PATH` that test skips, and `PM_REQUIRE_NODE_TESTS=1` makes the
skip a failure.

They ship inside the `pm` binary (`include_str!`, listed in
`crates/pm/src/app/views.rs` — a unit test fails if a file under
`crates/pm/views/` is missing from that list) and are unpacked on launch
into `$XDG_CACHE_HOME/pm/views/<fingerprint>/` (else `~/.cache/…`),
reused until the sources change. `PM_VIEWS_DIR=<dir>` mounts `<dir>`
instead, for developing a view without rebuilding pm.

| View | Opened by | Today (AGT-1402) | Becomes |
|---|---|---|---|
| `ticket` | `pm edit <ID>` | the editor: title, priority, project, labels, state, and the description bound to the text CRDT; live (AGT-1403) | — |
| `board` | `pm app` | the board (AGT-1404, below) | — |
| `project` | `pm project edit <ID>` | the design doc and named documents in the CRDT editor, the project's tickets, "New ticket" (AGT-1405, below) | — |

### The board

`pm app`'s view (`board.tsx`; its policy is `lib/board.ts`):

- **Columns** — one per workflow state from `GET /workspace`, ordered by
  `position`; each keeps `pm list`'s ticket order. A ticket whose state the
  workspace no longer defines gets a trailing column rather than vanishing.
- **Cards** — display id (`AGT-?` while the number is pending; the ULID is
  its tooltip), priority, project, title, labels, assignee, and markers:
  **held** (reason and holder in the tooltip), **parked** (only while the
  park is active: `forever`, or `until` not yet past — pm-core's
  `Parked::is_active`), and each **gate label** it carries.
- **Moving** — drag a card onto a column, or pick a state from the card's
  **Move…** menu (the keyboard route). Either sends
  `POST /tickets/{ref}/state {"state": <target>, "keep_assignee": false}` —
  exactly `pm move`: into an unstarted/backlog state it clears the assignee
  (un-claims, AGT-1379), anywhere else it keeps it. `{ref}` is the display
  id, or the ULID while the ticket is pending. The card moves at once and
  the op's event refetches the truth; a failed write says so and refetches.
  **Dropping into a `started` state is refused** (unless the card is
  already in a started state): starting work is a claim, claims stay
  CLI-only, and a bare move would leave the ticket started with nobody —
  or a stale somebody — on it. The column outlines red, the drop shows a
  toast naming `pm claim <ref>`, and the menu lists that state disabled.
- **Filters** — project, label, assignee (or unassigned); AND across them.
  They persist per viewer and per workspace in `localStorage`
  (`pm.board.filters.<workspace ULID>`), and a blocked or broken storage
  just means no persisted filters.
- **Live** — every ticket, workspace or project op on `GET /events`
  schedules a refetch, debounced 150 ms, so a batch or a sync pull is one
  refetch; only the newest fetch may land.
- **Opening a ticket** — clicking a card's title opens a read-only side
  panel (`GET /tickets/{ref}`: description, comments, blockers, hold) that
  stays live. ui-leaf gives a view no way to open another view, so editing
  is `pm edit <ref>`, which the panel shows and copies.

### The project view

`pm project edit <ID>`'s view (`project.tsx`; its policy is
`lib/project.ts`, covered by `crates/pm/views-test/project.test.ts`):

- **Documents** — a tab for the design doc, then one per named document
  by name; the open one is a `BodyEditor` bound through `BodySync` to its
  body endpoint (`GET`/`POST /projects/{id}/body`, or
  `…/docs/{name}/body`), the same CRDT editor as a ticket's description:
  every edit is a `body.edit` op on the document's `doc_id` within a
  second, and switching tabs flushes what is unsent first. New named
  documents are `pm project doc add` (the view does not create them).
- **Tickets** — `GET /tickets?project={id}`, one section per workflow
  state in state order (empty ones dropped). Clicking a ticket opens it.
- **Live** — one `GET /events` stream: a `body.edit` on the open
  document's `doc_id` pulls it (`?since=`); a ticket op refetches the
  list; any other op with no ticket id (project metadata, a new document,
  a workspace change) refetches the project, and the list; each refetch
  debounced 150 ms.
- **New ticket** — a title, filed as `POST /tickets {"title", "project":
  <id>}`: `pm new --title … --project <id>`, numbering included. The new
  ticket then **opens in the editor**, by its ref — the display id, or the
  ULID while a configured hub has yet to number it (the pane shows `AGT-?`
  with a note until `pm sync`).
- **Opening a ticket** — ui-leaf gives a view no way to open another view:
  its host-side `view`/`patch` messages swap the *only* window's view and
  take a self-contained source with no relative imports, which could
  carry neither this view nor the vendored editor. So the ticket opens
  **inline**: the ticket list gives way to a `TicketEditor` pane — the
  component `pm edit` shows, fields, labels, state and the description
  CRDT, live — with "← Back to tickets" to return. (The board, which
  predates this, still points at `pm edit <ref>`.)

