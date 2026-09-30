// The ticket editor's description binding (crates/pm/views/lib/body.ts,
// AGT-1403) against a real `pm app`, under node — no browser. Run by
// crates/pm/tests/views_js.rs, which builds the workspace, starts the
// server and passes it in the environment:
//
//   PM_APP_URL, PM_APP_TOKEN  the server
//   PM_BIN, PM_WORKSPACE      `pm` and the workspace, for CLI writes
//   PM_TEST_HOME              HOME for those CLI runs
//
// AGT-1 starts with the description "line one\nline two".

import { test } from "node:test";
import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { writeFileSync, chmodSync } from "node:fs";
import { join } from "node:path";

import { BodySync, apiTransport, fromBase64, TEXT, type ApiLike, type BodySnapshot } from "../../views/lib/body.ts";

const env = (name: string): string => {
  const value = process.env[name];
  if (!value) throw new Error(`${name} is not set (run via cargo test --test views_js)`);
  return value;
};
const URL_ = env("PM_APP_URL");
const TOKEN = env("PM_APP_TOKEN");

/** lib/pm.ts's `Api`, minus the view-only parts. */
const api: ApiLike = {
  async get<T>(path: string): Promise<T> {
    const res = await fetch(URL_ + path, { headers: { Authorization: `Bearer ${TOKEN}` } });
    const body = await res.json();
    if (!res.ok) throw new Error(`GET ${path}: ${res.status} ${JSON.stringify(body)}`);
    return body as T;
  },
  async post<T>(path: string, body: unknown): Promise<T> {
    const res = await fetch(URL_ + path, {
      method: "POST",
      headers: { Authorization: `Bearer ${TOKEN}`, "Content-Type": "application/json" },
      body: JSON.stringify(body),
    });
    const out = await res.json();
    if (!res.ok) throw new Error(`POST ${path}: ${res.status} ${JSON.stringify(out)}`);
    return out as T;
  },
};

function pm(...args: string[]): string {
  return execFileSync(env("PM_BIN"), [...args, "--workspace", env("PM_WORKSPACE")], {
    env: {
      HOME: env("PM_TEST_HOME"),
      USER: "tester",
      PATH: "/usr/bin:/bin",
      UI_LEAF_NO_OPEN: "1",
      EDITOR: process.env.EDITOR ?? "",
    },
    encoding: "utf8",
  });
}

/** An editor session: what ticket.tsx builds for one window. */
async function open(id = "AGT-1"): Promise<BodySync> {
  const body = await api.get<BodySnapshot>(`/tickets/${id}/body`);
  return new BodySync(fromBase64(body.snapshot), apiTransport(api, id));
}

/** Types into a session the way loro-codemirror does: edit, commit. */
function type(s: BodySync, at: number | "end", text: string) {
  const t = s.doc.getText(TEXT);
  t.insert(at === "end" ? t.length : at, text);
  s.doc.commit();
}

const description = async (id = "AGT-1") =>
  (await api.get<{ description: string }>(`/tickets/${id}`)).description;

async function until(what: string, ok: () => Promise<boolean> | boolean, ms = 3000) {
  const start = Date.now();
  while (!(await ok())) {
    if (Date.now() - start > ms) assert.fail(`timed out after ${ms} ms waiting for ${what}`);
    await new Promise((r) => setTimeout(r, 20));
  }
  return Date.now() - start;
}

/**
 * The view's `/events` handling: every `body.edit` on AGT-1 pulls into
 * each session (and a hello, too). Returns the op events seen, and a stop.
 */
function live(sessions: BodySync[]) {
  const seen: { kind: string; id: string | null }[] = [];
  const abort = new AbortController();
  const done = (async () => {
    const res = await fetch(URL_ + "/events", {
      headers: { Authorization: `Bearer ${TOKEN}` },
      signal: abort.signal,
    });
    const reader = res.body!.getReader();
    const decoder = new TextDecoder();
    let buffer = "";
    try {
      for (;;) {
        const { value, done } = await reader.read();
        if (done) return;
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
          if (name === "op") {
            const op = JSON.parse(data);
            seen.push(op);
            if (op.id === "AGT-1" && op.kind === "body.edit") for (const s of sessions) void s.pull();
          } else if (name === "hello") {
            for (const s of sessions) void s.pull();
          }
        }
      }
    } catch (e) {
      if (!abort.signal.aborted) throw e;
    }
  })();
  return {
    seen,
    stop: async () => {
      abort.abort();
      await done.catch(() => {});
    },
  };
}

test("each session has its own fresh, non-zero peer", async () => {
  const a = await open();
  const b = await open();
  assert.notEqual(a.peerId(), b.peerId());
  assert.notEqual(a.peerId(), "0");
  assert.equal(a.text(), "line one\nline two");
  a.dispose();
  b.dispose();
});

test("two editor windows converge live, each edit in the op log within a second", async () => {
  const a = await open();
  const b = await open();
  const events = live([a, b]);
  try {
    type(a, 0, "A: ");
    const tookA = await until("A's edit in the log", async () => (await description()).startsWith("A: "));
    assert.ok(tookA < 1000, `A's edit took ${tookA} ms`);

    type(b, "end", "\nB was here");
    const tookB = await until("B's edit in the log", async () => (await description()).endsWith("B was here"));
    assert.ok(tookB < 1000, `B's edit took ${tookB} ms`);

    const want = "A: line one\nline two\nB was here";
    await until("both windows to converge", () => a.text() === want && b.text() === want);
    assert.equal(await description(), want);
    assert.ok(!a.hasUnsent() && !b.hasUnsent());

    // Concurrent: both type before either has sent — nothing is lost.
    type(a, 0, "[a]");
    type(b, 0, "[b]");
    await until("the concurrent edits to converge", async () => {
      const d = await description();
      return d.includes("[a]") && d.includes("[b]") && a.text() === d && b.text() === d;
    });
  } finally {
    await events.stop();
    a.dispose();
    b.dispose();
  }
});

test("a CLI pm edit and pm set reach the open window", async () => {
  const a = await open();
  const events = live([a]);
  try {
    // An unsent local edit in the window while the terminal edits too.
    const editor = join(env("PM_TEST_HOME"), "append.sh");
    writeFileSync(editor, "#!/bin/sh\nprintf '\\nfrom the CLI' >> \"$1\"\n");
    chmodSync(editor, 0o755);
    type(a, "end", "\nfrom the window");
    process.env.EDITOR = editor;
    pm("edit", "AGT-1", "--view=editor");
    await until("the CLI edit in the window", () => a.text().includes("from the CLI"));
    await until("the window's edit in the log", async () => (await description()).includes("from the window"));
    await until("convergence", async () => a.text() === (await description()));

    // A field change from the CLI is an op event on the ticket: the view
    // refetches on it and shows the new title.
    const seenBefore = events.seen.length;
    pm("set", "AGT-1", "title=Set from the CLI");
    await until("the field.set event", () =>
      events.seen.slice(seenBefore).some((op) => op.id === "AGT-1" && op.kind === "field.set"),
    );
    const t = await api.get<{ title: string }>("/tickets/AGT-1");
    assert.equal(t.title, "Set from the CLI");
  } finally {
    await events.stop();
    a.dispose();
  }
});

test("a failed send is retried and nothing is lost", async () => {
  const body = await api.get<BodySnapshot>("/tickets/AGT-1/body");
  const real = apiTransport(api, "AGT-1");
  let failures = 1;
  const s = new BodySync(fromBase64(body.snapshot), {
    post: (u, o) => (failures-- > 0 ? Promise.reject(new Error("offline")) : real.post(u, o)),
    since: real.since,
  }, { debounceMs: 10, maxWaitMs: 50 });
  try {
    type(s, 0, "retried: ");
    await until("the retried edit in the log", async () => (await description()).startsWith("retried: "));
    assert.ok(!s.hasUnsent());
  } finally {
    s.dispose();
  }
});
