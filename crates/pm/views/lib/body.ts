// The ticket editor's description binding (AGT-1403; docs/app-api.md
// §`GET /tickets/{id}/body`, §`POST /tickets/{id}/body`).
//
// A `BodySync` owns one `loro-crdt` document bound to a ticket's body:
//
// - **Open.** The document is a fresh `LoroDoc` — a fresh random peer id,
//   never a fixed one and never 0 (the server's materialized view is peer
//   0; two sessions sharing a peer silently drop each other's edits) —
//   with the body's snapshot imported.
// - **Local edits** (CodeMirror via loro-codemirror, or anything that edits
//   `text()` and commits) are exported as Loro updates and POSTed as
//   `{"update": "<base64>"}`, debounced: `debounceMs` after the last
//   keystroke, and never later than `maxWaitMs` after the first unsent
//   one, so a change reaches the op log inside a second even while typing.
//   Only this session's own ops travel — everything else it holds came
//   from the server — so an update never re-sends another window's edits.
//   One POST is in flight at a time; a failed one is retried (with backoff)
//   and nothing is lost, since what counts as sent only advances on a 200.
// - **Remote edits** (another window, a `pm edit` in a terminal, a sync)
//   arrive through `pull()`: it sends the document's version vector as
//   `GET /tickets/{id}/body?since=…` and imports the ops it lacked, which
//   loro-codemirror turns into editor changes. The view calls it on every
//   `body.edit` op event for the ticket and after every (re)connect.
//
// Framework-free and DOM-free so it runs under node for the tests
// (crates/pm/tests/views/): no parameter properties or other TypeScript
// that node's type stripping cannot erase.

import { LoroDoc, VersionVector } from "../vendor/loro.js";

/** The one text container in every pm body (`pm-core`'s `TEXT_ID`). */
export const TEXT = "body";

/** What a `BodySync` needs from the API; `apiTransport` is the real one. */
export interface BodyTransport {
  /** POSTs one update; resolves on a 2xx, rejects otherwise. */
  post(update: Uint8Array, opts?: { keepalive?: boolean }): Promise<void>;
  /** The ops the server holds that `version` lacks, as one update. */
  since(version: Uint8Array): Promise<Uint8Array>;
}

/** The subset of lib/pm.ts's `Api` the transport uses. */
export interface ApiLike {
  get<T>(path: string): Promise<T>;
  post<T>(path: string, body: unknown, opts?: { keepalive?: boolean }): Promise<T>;
}

/** `GET /tickets/{id}/body` (without `since`). */
export interface BodySnapshot {
  schema: number;
  id: string;
  ulid: string;
  text: string;
  snapshot: string;
}

/** Where the local edits stand, for the view's indicator. */
export type SyncStatus =
  | { kind: "saved" }
  | { kind: "pending" }
  | { kind: "saving" }
  | { kind: "error"; message: string };

export interface BodySyncOptions {
  /** Quiet time after the last local change before it is sent. */
  debounceMs?: number;
  /** Upper bound on how long a local change waits to be sent. */
  maxWaitMs?: number;
  /** Status changes. */
  onStatus?: (status: SyncStatus) => void;
}

// --------------------------------------------------------------- base64

/** Standard padded base64, as `pm_core::bytes` writes it. */
export function toBase64(bytes: Uint8Array): string {
  let binary = "";
  const chunk = 0x8000;
  for (let i = 0; i < bytes.length; i += chunk) {
    binary += String.fromCharCode(...bytes.subarray(i, i + chunk));
  }
  return btoa(binary);
}

export function fromBase64(text: string): Uint8Array {
  const binary = atob(text);
  const out = new Uint8Array(binary.length);
  for (let i = 0; i < binary.length; i++) out[i] = binary.charCodeAt(i);
  return out;
}

// ------------------------------------------------------------ transport

/** The body endpoints of ticket `id` over a connected client. */
export function apiTransport(api: ApiLike, id: string): BodyTransport {
  const path = `/tickets/${encodeURIComponent(id)}/body`;
  return {
    async post(update, opts) {
      await api.post(path, { update: toBase64(update) }, opts);
    },
    async since(version) {
      const res = await api.get<{ update: string }>(
        `${path}?since=${encodeURIComponent(toBase64(version))}`,
      );
      return fromBase64(res.update);
    },
  };
}

// ----------------------------------------------------------------- sync

/** A random non-zero 64-bit peer id (`pm edit` avoids 0 and MAX too). */
function freshPeer(): bigint {
  const words = new Uint32Array(2);
  crypto.getRandomValues(words);
  const peer = (BigInt(words[0]) << 32n) | BigInt(words[1]);
  return peer === 0n || peer === 0xffff_ffff_ffff_ffffn ? 1n : peer;
}

export class BodySync {
  readonly doc: LoroDoc;
  private readonly transport: BodyTransport;
  private readonly debounceMs: number;
  private readonly maxWaitMs: number;
  private readonly onStatus: (status: SyncStatus) => void;
  private readonly peer: string;
  /** This session's op counter the server has acknowledged. */
  private sent = 0;
  private firstUnsentAt: number | null = null;
  private timer: ReturnType<typeof setTimeout> | null = null;
  private flushing: Promise<void> | null = null;
  private pulling: Promise<void> | null = null;
  private pullAgain = false;
  private failures = 0;
  private disposed = false;
  private readonly unsubscribe: () => void;

  constructor(snapshot: Uint8Array, transport: BodyTransport, options: BodySyncOptions = {}) {
    this.transport = transport;
    this.debounceMs = options.debounceMs ?? 250;
    this.maxWaitMs = options.maxWaitMs ?? 750;
    this.onStatus = options.onStatus ?? (() => {});
    this.doc = new LoroDoc();
    // loro-crdt already draws a random peer; set one explicitly anyway so
    // the "never 0, never shared" rule does not rest on a library default.
    this.doc.setPeerId(freshPeer());
    this.peer = this.doc.peerIdStr;
    if (snapshot.length > 0) this.doc.import(snapshot);
    this.unsubscribe = this.doc.subscribeLocalUpdates(() => this.schedule());
  }

  /** The body text as this session sees it. */
  text(): string {
    return this.doc.getText(TEXT).toString();
  }

  /** This session's peer id (decimal string). */
  peerId(): string {
    return this.peer;
  }

  /** Whether local edits are waiting to be (or being) sent. */
  hasUnsent(): boolean {
    return this.ownCounter() > this.sent;
  }

  private ownCounter(): number {
    return this.doc.oplogVersion().get(this.peer as `${number}`) ?? 0;
  }

  private schedule() {
    if (this.disposed) return;
    const now = Date.now();
    if (this.firstUnsentAt === null) this.firstUnsentAt = now;
    this.onStatus({ kind: "pending" });
    if (this.timer !== null) clearTimeout(this.timer);
    const wait = Math.max(0, Math.min(this.debounceMs, this.firstUnsentAt + this.maxWaitMs - now));
    this.timer = setTimeout(() => {
      this.timer = null;
      void this.flush();
    }, wait);
  }

  /**
   * Sends every unsent local op now (one POST at a time). `keepalive`
   * lets the request outlive the page (the window closing). Never
   * rejects: a failure is reported through `onStatus` and retried, and
   * `hasUnsent()` says whether anything is still waiting.
   */
  async flush(opts: { keepalive?: boolean } = {}): Promise<void> {
    while (this.flushing) await this.flushing;
    if (!this.hasUnsent()) return;
    if (this.timer !== null) {
      clearTimeout(this.timer);
      this.timer = null;
    }
    this.flushing = this.send(opts).finally(() => {
      this.flushing = null;
    });
    return this.flushing;
  }

  private async send(opts: { keepalive?: boolean }) {
    const upTo = this.ownCounter();
    // Everything this document holds except our own ops past `sent`: the
    // server already has all of that (it is where it came from).
    const from = new Map(this.doc.oplogVersion().toJSON());
    if (this.sent > 0) from.set(this.peer as `${number}`, this.sent);
    else from.delete(this.peer as `${number}`);
    const update = this.doc.export({ mode: "update", from: VersionVector.parseJSON(from) });
    this.firstUnsentAt = null;
    this.onStatus({ kind: "saving" });
    try {
      await this.transport.post(update, opts);
      this.sent = Math.max(this.sent, upTo);
      this.failures = 0;
      if (this.hasUnsent()) this.schedule();
      else this.onStatus({ kind: "saved" });
    } catch (e) {
      this.failures += 1;
      this.onStatus({ kind: "error", message: e instanceof Error ? e.message : String(e) });
      if (!this.disposed) {
        // Retry with backoff; the unsent ops stay unsent until a 200.
        if (this.timer !== null) clearTimeout(this.timer);
        const delay = Math.min(250 * 2 ** this.failures, 5_000);
        this.timer = setTimeout(() => {
          this.timer = null;
          void this.flush();
        }, delay);
      }
    }
  }

  /**
   * Imports whatever the server has that this document lacks. Concurrent
   * calls coalesce: one request in flight, and one more after it when a
   * call arrived meanwhile (an op may have landed after it was sent).
   */
  async pull(): Promise<void> {
    if (this.pulling) {
      this.pullAgain = true;
      return this.pulling;
    }
    this.pulling = (async () => {
      try {
        do {
          this.pullAgain = false;
          const update = await this.transport.since(this.doc.oplogVersion().encode());
          if (update.length > 0 && !this.disposed) this.doc.import(update);
        } while (this.pullAgain && !this.disposed);
      } finally {
        this.pulling = null;
      }
    })();
    return this.pulling;
  }

  /** Stops listening and cancels timers (unsent edits are not sent). */
  dispose() {
    this.disposed = true;
    if (this.timer !== null) clearTimeout(this.timer);
    this.timer = null;
    this.unsubscribe();
  }
}
