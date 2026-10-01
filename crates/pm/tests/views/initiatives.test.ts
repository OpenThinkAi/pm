// The initiatives view (AGT-1492) against a real `pm app`, under node — no
// browser: landing → initiative → project, driven through the same
// lib/initiatives.ts the view renders from and the reads `ProjectPage`
// makes, then a terminal `pm move` reaching the open view through
// `/events`. Run by crates/pm/tests/views_js.rs, which seeds initiative
// `launch` (kind initiative) with project `site` under it and `site-docs`
// under that, beside the unfiled project `design` (see body.test.ts for the
// environment).

import { test } from "node:test";
import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";

import {
  UNFILED,
  group,
  landingRows,
  projectRows,
  trail,
  wantsOp,
  type Initiatives,
} from "../../views/lib/initiatives.ts";

const env = (name: string): string => {
  const value = process.env[name];
  if (!value) throw new Error(`${name} is not set (run via cargo test --test views_js)`);
  return value;
};
const URL_ = env("PM_APP_URL");
const TOKEN = env("PM_APP_TOKEN");
const auth = { Authorization: `Bearer ${TOKEN}` };

async function get<T>(path: string): Promise<T> {
  const res = await fetch(URL_ + path, { headers: auth });
  const body = await res.json();
  if (!res.ok) throw new Error(`GET ${path}: ${res.status} ${JSON.stringify(body)}`);
  return body as T;
}

async function post<T>(path: string, body: unknown): Promise<T> {
  const res = await fetch(URL_ + path, {
    method: "POST",
    headers: { ...auth, "Content-Type": "application/json" },
    body: JSON.stringify(body),
  });
  const out = await res.json();
  if (!res.ok) throw new Error(`POST ${path}: ${res.status} ${JSON.stringify(out)}`);
  return out as T;
}

function pm(...args: string[]): string {
  return execFileSync(env("PM_BIN"), [...args, "--workspace", env("PM_WORKSPACE")], {
    env: { HOME: env("PM_TEST_HOME"), USER: "tester", PATH: "/usr/bin:/bin", UI_LEAF_NO_OPEN: "1" },
    encoding: "utf8",
  });
}

interface Ticket {
  id: string;
  ulid: string;
  title: string;
  project: string | null;
}

async function until(what: string, ok: () => Promise<boolean> | boolean, ms = 3000) {
  const start = Date.now();
  while (!(await ok())) {
    if (Date.now() - start > ms) assert.fail(`timed out after ${ms} ms waiting for ${what}`);
    await new Promise((r) => setTimeout(r, 20));
  }
}

/** initiatives.tsx's live loop: every wanted op refetches `GET /initiatives`
 * (newest fetch wins). */
function live(onData: (d: Initiatives, cause: "hello" | "op") => void) {
  const abort = new AbortController();
  let seq = 0;
  const refetch = (cause: "hello" | "op") => {
    const mine = ++seq;
    void get<Initiatives>("/initiatives").then((d) => mine === seq && onData(d, cause));
  };
  const done = (async () => {
    const res = await fetch(URL_ + "/events", { headers: auth, signal: abort.signal });
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
          if (name === "hello") refetch("hello");
          else if (name === "op" && wantsOp(JSON.parse(data))) refetch("op");
        }
      }
    } catch (e) {
      if (!abort.signal.aborted) throw e;
    }
  })();
  return {
    stop: async () => {
      abort.abort();
      await done.catch(() => {});
    },
  };
}

test("landing → initiative → project, and a terminal move reaches the view", async () => {
  // Tickets: one done and one open in `site`, one open in `site-docs`.
  const file = (title: string, project: string) => post<Ticket>("/tickets", { title, project });
  const shipped = await file("Site shipped", "site");
  await post(`/tickets/${encodeURIComponent(shipped.id)}/state`, { state: "done", keep_assignee: false });
  await file("Site polish", "site");
  const docs = await file("Write the site docs", "site-docs");

  let data = await get<Initiatives>("/initiatives");

  // Landing: the initiative, then Unfiled (which holds `design`).
  const rows = landingRows(data);
  assert.deepEqual(
    rows.map((r) => r.key),
    ["launch", UNFILED],
  );
  const launch = rows[0];
  assert.equal(launch.title, "Launch");
  assert.equal(launch.projects, 2, "site and its sub-project");
  assert.deepEqual([launch.unstarted, launch.started, launch.completed], [2, 0, 1]);
  assert.equal(launch.progress, 1 / 3);
  assert.ok(group(data, UNFILED)!.projects.some((p) => p.id === "design"));

  // The initiative: its projects as rows, its own documents by its id.
  const g = group(data, "launch")!;
  assert.equal(g.node?.kind, "initiative");
  assert.deepEqual(
    projectRows(g.projects).map((r) => [r.key, r.depth, r.completed, r.progress]),
    [
      ["site", 0, 1, 1 / 3],
      ["site-docs", 1, 0, 0],
    ],
  );
  const initiative = await get<{ id: string; title: string }>("/projects/launch");
  assert.equal(initiative.title, "Launch");

  // The project: the breadcrumb back, and ProjectPage's reads.
  assert.deepEqual(trail(data, "site-docs"), [
    { key: "launch", title: "Launch" },
    { key: "site", title: "Site" },
    { key: "site-docs", title: "Site docs" },
  ]);
  const project = await get<{ id: string; parent: string | null }>("/projects/site-docs");
  assert.equal(project.parent, "site");
  const listed = await get<Ticket[]>("/tickets?project=site-docs");
  assert.deepEqual(
    listed.map((t) => t.title),
    ["Write the site docs"],
  );

  // Live: `pm move` in a terminal refetches the rollups.
  let connected = false;
  let byOp = false;
  const events = live((d, cause) => {
    data = d;
    if (cause === "hello") connected = true;
    else byOp = true;
  });
  try {
    await until("the stream's hello", () => connected);
    pm("move", docs.id, "done");
    await until("the move in the rollups", () => byOp && data.initiatives[0]?.total.completed === 2);
    const after = landingRows(data)[0];
    assert.equal(after.progress, 2 / 3);
    assert.deepEqual(
      projectRows(group(data, "launch")!.projects).map((r) => [r.key, r.progress]),
      [
        ["site", 2 / 3],
        ["site-docs", 1],
      ],
    );
  } finally {
    await events.stop();
  }
});
