// The project view's document binding (AGT-1405): lib/body.ts's BodySync
// bound to a project document's endpoints (crates/pm/views/lib/project.ts's
// `docBodyPath`) against a real `pm app`, under node — no browser — plus
// "New ticket" through `POST /tickets` and the new ticket opened by the
// ref the view opens it by. Run by crates/pm/tests/views_js.rs, which sets
// up the workspace (project `design`, its design doc "# Design\n" and a
// named document `notes`, "first note\n") and passes the server in the
// environment (see body.test.ts).

import { test } from "node:test";
import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { writeFileSync, chmodSync } from "node:fs";
import { join } from "node:path";

import {
  BodySync,
  apiTransport,
  bodyTransport,
  fromBase64,
  TEXT,
  type ApiLike,
  type BodySnapshot,
} from "../../views/lib/body.ts";
import {
  DESIGN_DOC,
  createdRef,
  docBodyPath,
  docTabs,
  newTicketRequest,
  opEffect,
  pickTab,
  type Project,
} from "../../views/lib/project.ts";

const env = (name: string): string => {
  const value = process.env[name];
  if (!value) throw new Error(`${name} is not set (run via cargo test --test views_js)`);
  return value;
};
const URL_ = env("PM_APP_URL");
const TOKEN = env("PM_APP_TOKEN");

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

function pm(extraEnv: Record<string, string>, ...args: string[]): string {
  return execFileSync(env("PM_BIN"), [...args, "--workspace", env("PM_WORKSPACE")], {
    env: { HOME: env("PM_TEST_HOME"), USER: "tester", PATH: "/usr/bin:/bin", UI_LEAF_NO_OPEN: "1", ...extraEnv },
    encoding: "utf8",
  });
}

const project = () => api.get<Project>("/projects/design");

/** A document session: what project.tsx builds for the open tab. */
async function open(tab = DESIGN_DOC): Promise<{ sync: BodySync; docId: string }> {
  const path = docBodyPath("design", tab);
  const body = await api.get<BodySnapshot>(path);
  assert.ok(body.doc_id, "a document body names its doc_id");
  return { sync: new BodySync(fromBase64(body.snapshot), bodyTransport(api, path)), docId: body.doc_id! };
}

function type(s: BodySync, at: number | "end", text: string) {
  const t = s.doc.getText(TEXT);
  t.insert(at === "end" ? t.length : at, text);
  s.doc.commit();
}

async function until(what: string, ok: () => Promise<boolean> | boolean, ms = 3000) {
  const start = Date.now();
  while (!(await ok())) {
    if (Date.now() - start > ms) assert.fail(`timed out after ${ms} ms waiting for ${what}`);
    await new Promise((r) => setTimeout(r, 20));
  }
  return Date.now() - start;
}

/** project.tsx's `/events` handling, through lib/project.ts's `opEffect`. */
function live(sessions: { sync: BodySync; docId: string }[]) {
  const seen: { kind: string; entity: string; id: string | null }[] = [];
  const abort = new AbortController();
  const done = (async () => {
    const res = await fetch(URL_ + "/events", { headers: { Authorization: `Bearer ${TOKEN}` }, signal: abort.signal });
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
            for (const s of sessions) if (opEffect(op, s.docId).pullDoc) void s.sync.pull();
          } else if (name === "hello") {
            for (const s of sessions) void s.sync.pull();
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

test("two windows on the design doc converge live through the API", async () => {
  const a = await open();
  const b = await open();
  assert.equal(a.docId, b.docId);
  assert.notEqual(a.sync.peerId(), b.sync.peerId());
  assert.equal(a.sync.text(), "# Design\n");
  const events = live([a, b]);
  try {
    type(a.sync, "end", "\nfrom window A\n");
    const took = await until("A's edit in the project", async () => (await project()).doc.includes("from window A"));
    assert.ok(took < 1000, `A's edit took ${took} ms`);
    type(b.sync, 0, "[b] ");
    await until("both to converge", async () => {
      const doc = (await project()).doc;
      return doc.startsWith("[b] # Design") && a.sync.text() === doc && b.sync.text() === doc;
    });
    // The edits were body.edit ops on the doc's own id, no ticket named.
    assert.ok(events.seen.some((op) => op.kind === "body.edit" && op.entity === a.docId && op.id === null));
  } finally {
    await events.stop();
    a.sync.dispose();
    b.sync.dispose();
  }
});

test("a pm project edit in a terminal reaches the open window", async () => {
  const a = await open();
  const events = live([a]);
  try {
    const editor = join(env("PM_TEST_HOME"), "append-doc.sh");
    writeFileSync(editor, "#!/bin/sh\nprintf 'from the CLI\\n' >> \"$1\"\n");
    chmodSync(editor, 0o755);
    type(a.sync, "end", "unsent in the window\n");
    pm({ EDITOR: editor }, "project", "edit", "design", "--view=editor");
    await until("the CLI edit in the window", () => a.sync.text().includes("from the CLI"));
    await until("convergence", async () => {
      const doc = (await project()).doc;
      return doc.includes("unsent in the window") && a.sync.text() === doc;
    });
  } finally {
    await events.stop();
    a.sync.dispose();
  }
});

test("a named document binds its own endpoint and doc_id", async () => {
  const tabs = docTabs(await project());
  const notesTab = pickTab(tabs, "doc:notes");
  assert.equal(notesTab.name, "notes");
  const design = await open();
  const notes = await open(notesTab);
  assert.notEqual(notes.docId, design.docId);
  assert.equal(notes.sync.text(), "first note\n");
  design.sync.dispose();
  const events = live([notes]);
  try {
    type(notes.sync, "end", "second note\n");
    await until("the note saved", async () => (await project()).documents.notes === "first note\nsecond note\n");
    const shown = JSON.parse(pm({}, "project", "show", "design", "--doc", "notes", "--json"));
    assert.equal(shown.body, "first note\nsecond note\n");
  } finally {
    await events.stop();
    notes.sync.dispose();
  }
});

test("New ticket files like pm new and opens in the editor by its ref", async () => {
  const request = newTicketRequest("  Filed from the project view ", "design");
  assert.ok(request);
  const made = await api.post<{ id: string; ulid: string; number: number | null; project: string; state: string }>(
    "/tickets",
    request,
  );
  assert.equal(made.project, "design");
  assert.equal(made.state, "triage");
  const ref = createdRef(made);
  // The inline editor pane: the ticket's own description binding.
  const body = await api.get<BodySnapshot>(`/tickets/${encodeURIComponent(ref)}/body`);
  const s = new BodySync(fromBase64(body.snapshot), apiTransport(api, ref));
  try {
    type(s, 0, "written in the pane");
    await until("the description saved", async () => {
      const t = await api.get<{ description: string }>(`/tickets/${encodeURIComponent(ref)}`);
      return t.description === "written in the pane";
    });
    const listed = await api.get<{ ulid: string }[]>("/tickets?project=design");
    assert.ok(listed.some((t) => t.ulid === made.ulid), "the project's list shows it");
  } finally {
    s.dispose();
  }
});
