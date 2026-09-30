// The client every pm ui-leaf view shares (AGT-1402; docs/app-api.md
// §Views). pm launches ui-leaf with this directory as `viewsRoot`; the view
// talks to `pm app`'s localhost API, never to the store.
//
// **How a view gets the API.** Not from the URL: ui-leaf's own token rides
// the launch URL's `#token=` fragment, and a view that reads `view.url` or
// `location` finds nothing of pm's there (the insieme lesson). Not from
// `data` either: ui-leaf inlines `data` into the page HTML, which it serves
// to any local process without a token. pm answers one declared mutation,
// `session`, over ui-leaf's token-gated `/mutate` channel with the API's URL
// and bearer token; `connect()` asks for it once and keeps both in memory.
//
// **Staying alive.** `pm app` exits once no `/events` stream has been open
// for its idle grace, and `pm edit` returns then. A view keeps one stream
// open for as long as it is showing (`useEvents`), so closing the window is
// what ends the session.

import { useEffect, useRef, useState } from "react";

/** ui-leaf's `mutate` prop. */
export type Mutate = (name: string, args?: unknown) => Promise<unknown>;

/** The `data` pm mounts every view with. Nothing secret lives here. */
export interface ViewData {
  schema: number;
  /** `board` or `ticket`. */
  view: string;
  /** The ticket a `ticket` view shows (a display id, e.g. `AGT-12`). */
  ticket?: string;
}

/** What ui-leaf hands a view. */
export interface ViewProps {
  data: ViewData;
  mutate: Mutate;
}

/** The `session` mutation's reply. */
interface Session {
  schema: number;
  url: string;
  token: string;
}

/** An `op` event from `GET /events` (docs/app-api.md §`GET /events`). */
export interface OpEvent {
  seq: number;
  kind: string;
  entity: string;
  /** The display id when the entity is a ticket, else null. */
  id: string | null;
  actor: string;
}

/** The API error body, `{"schema":1,"error":"…"}`, as a thrown Error. */
export class ApiError extends Error {
  constructor(
    readonly status: number,
    message: string,
  ) {
    super(message);
  }
}

/** A connected client: the URL and token live only in this object. */
export class Api {
  constructor(
    private readonly url: string,
    private readonly token: string,
  ) {}

  private headers(json: boolean): Record<string, string> {
    const h: Record<string, string> = { Authorization: `Bearer ${this.token}` };
    if (json) h["Content-Type"] = "application/json";
    return h;
  }

  private async parse<T>(res: Response): Promise<T> {
    const text = await res.text();
    const body = text ? JSON.parse(text) : null;
    if (!res.ok) {
      const message = body && typeof body.error === "string" ? body.error : `HTTP ${res.status}`;
      throw new ApiError(res.status, message);
    }
    return body as T;
  }

  async get<T>(path: string): Promise<T> {
    return this.parse<T>(await fetch(this.url + path, { headers: this.headers(false) }));
  }

  async post<T>(path: string, body: unknown): Promise<T> {
    return this.parse<T>(
      await fetch(this.url + path, {
        method: "POST",
        headers: this.headers(true),
        body: JSON.stringify(body),
      }),
    );
  }

  /**
   * Streams `GET /events` until `signal` aborts, calling `onOp` for every
   * `op` and `onResync` for `hello` (after a reconnect) and `lagged` —
   * both mean "refetch what you show". `EventSource` cannot send the
   * Authorization header, so this reads the stream with `fetch`.
   */
  async events(onOp: (op: OpEvent) => void, onResync: () => void, signal: AbortSignal) {
    let delay = 250;
    while (!signal.aborted) {
      try {
        const res = await fetch(this.url + "/events", { headers: this.headers(false), signal });
        if (!res.ok || !res.body) throw new ApiError(res.status, `events: HTTP ${res.status}`);
        delay = 250;
        const reader = res.body.getReader();
        const decoder = new TextDecoder();
        let buffer = "";
        for (;;) {
          const { value, done } = await reader.read();
          if (done) break;
          buffer += decoder.decode(value, { stream: true });
          let end: number;
          while ((end = buffer.indexOf("\n\n")) >= 0) {
            const frame = buffer.slice(0, end);
            buffer = buffer.slice(end + 2);
            let name = "message";
            let data = "";
            for (const line of frame.split("\n")) {
              if (line.startsWith("event:")) name = line.slice(6).trim();
              else if (line.startsWith("data:")) data += line.slice(5).trim();
            }
            if (name === "op" && data) onOp(JSON.parse(data) as OpEvent);
            else if (name === "hello" || name === "lagged") onResync();
          }
        }
      } catch {
        if (signal.aborted) return;
      }
      // The server went away (pm exited) or the stream broke: retry a few
      // times with backoff, which also rides out a reload.
      await new Promise((r) => setTimeout(r, delay));
      delay = Math.min(delay * 2, 5_000);
    }
  }
}

/** Asks pm for the API's URL and token (the `session` mutation). */
export async function connect(mutate: Mutate): Promise<Api> {
  const session = (await mutate("session")) as Session;
  if (!session || typeof session.url !== "string" || typeof session.token !== "string") {
    throw new Error("pm did not answer the session mutation with a url and token");
  }
  return new Api(session.url, session.token);
}

/** The connected client, or the error connecting raised. */
export function useApi(mutate: Mutate): { api: Api | null; error: string | null } {
  const [api, setApi] = useState<Api | null>(null);
  const [error, setError] = useState<string | null>(null);
  useEffect(() => {
    connect(mutate).then(setApi, (e) => setError(e instanceof Error ? e.message : String(e)));
  }, [mutate]);
  return { api, error };
}

/**
 * Keeps an `/events` stream open while the view is mounted and calls
 * `refresh` whenever `wants(op)` says an op concerns it (and after every
 * reconnect). The open stream is what keeps `pm app` alive.
 */
export function useEvents(api: Api | null, refresh: () => void, wants: (op: OpEvent) => boolean) {
  const latest = useRef({ refresh, wants });
  latest.current = { refresh, wants };
  useEffect(() => {
    if (!api) return;
    const abort = new AbortController();
    void api.events(
      (op) => {
        if (latest.current.wants(op)) latest.current.refresh();
      },
      () => latest.current.refresh(),
      abort.signal,
    );
    return () => abort.abort();
  }, [api]);
}

/** Shared page chrome: system font, light/dark. */
export const baseCss = `
:root { color-scheme: light dark; --fg: #1a1a1a; --muted: #666; --bg: #fafafa; --card: #fff; --line: #e2e2e2; }
@media (prefers-color-scheme: dark) {
  :root { --fg: #e8e8e8; --muted: #9a9a9a; --bg: #161616; --card: #202020; --line: #333; }
}
html, body { margin: 0; background: var(--bg); color: var(--fg);
  font: 14px/1.45 system-ui, -apple-system, "Segoe UI", sans-serif; }
.pm-muted { color: var(--muted); }
.pm-error { color: #c0392b; padding: 1rem; }
.pm-chip { display: inline-block; padding: 0 .45rem; border: 1px solid var(--line);
  border-radius: 999px; font-size: 12px; margin-right: .25rem; }
`;
