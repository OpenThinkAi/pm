// Tests for the initiatives view's logic (crates/pm/views/lib/initiatives.ts,
// AGT-1492). Run with Node >= 22.18 (built-in type stripping, no install,
// no browser):
//
//   node --test crates/pm/views-test/*.test.ts
//
// Not part of `cargo test`. The view against a real `pm app` (landing →
// initiative → project) is crates/pm/tests/views/initiatives.test.ts.

import assert from "node:assert/strict";
import { test } from "node:test";

import {
  UNFILED,
  group,
  landingRows,
  percent,
  progress,
  projectRows,
  trail,
  wantsOp,
  type Counts,
  type Initiatives,
  type Node,
} from "../views/lib/initiatives.ts";

const counts = (c: Partial<Counts> = {}): Counts => ({
  backlog: 0,
  unstarted: 0,
  started: 0,
  completed: 0,
  canceled: 0,
  ...c,
});

const node = (id: string, total: Partial<Counts>, children: Node[] = [], kind = "project"): Node => ({
  id,
  title: id.toUpperCase(),
  kind,
  status: "in-progress",
  tickets: counts(),
  total: counts(total),
  children,
});

const data: Initiatives = {
  initiatives: [
    node(
      "q4",
      { backlog: 1, unstarted: 1, started: 1, completed: 2, canceled: 5 },
      [node("pm", { unstarted: 1, completed: 2 }, [node("pm-app", { unstarted: 1 })]), node("site", {})],
      "initiative",
    ),
    node("empty", {}, [], "initiative"),
  ],
  unfiled: { total: counts({ started: 3 }), projects: [node("loose", { started: 3 })] },
};

test("progress is completed over non-canceled tickets", () => {
  assert.equal(progress(counts({ backlog: 1, unstarted: 1, started: 1, completed: 2, canceled: 5 })), 0.4);
  assert.equal(progress(counts({ canceled: 3 })), null);
  assert.equal(progress(counts()), null);
  assert.equal(percent(0.4), "40%");
  assert.equal(percent(0.999), "99%", "100% only when everything is done");
  assert.equal(percent(1), "100%");
  assert.equal(percent(null), "—");
});

test("the landing page lists initiatives, then Unfiled", () => {
  const rows = landingRows(data);
  assert.deepEqual(
    rows.map((r) => [r.key, r.title, r.projects, r.unstarted, r.started, r.completed, r.progress]),
    [
      ["q4", "Q4", 3, 2, 1, 2, 0.4],
      ["empty", "EMPTY", 0, 0, 0, 0, null],
      [UNFILED, "Unfiled", 1, 0, 3, 0, 0],
    ],
  );
  assert.equal(rows.at(-1)!.status, null);
});

test("an initiative's projects nest sub-projects after their parent", () => {
  const g = group(data, "q4")!;
  assert.equal(g.node?.id, "q4");
  assert.deepEqual(
    projectRows(g.projects).map((r) => [r.key, r.depth, r.projects]),
    [
      ["pm", 0, null],
      ["pm-app", 1, null],
      ["site", 0, null],
    ],
  );
  const u = group(data, UNFILED)!;
  assert.equal(u.node, null);
  assert.equal(u.title, "Unfiled");
  assert.deepEqual(u.projects.map((p) => p.id), ["loose"]);
  // A project id, or a vanished initiative, is not a group.
  assert.equal(group(data, "pm"), null);
  assert.equal(group(data, "gone"), null);
});

test("the breadcrumb runs from the group down to the project", () => {
  assert.deepEqual(trail(data, "pm-app"), [
    { key: "q4", title: "Q4" },
    { key: "pm", title: "PM" },
    { key: "pm-app", title: "PM-APP" },
  ]);
  assert.deepEqual(trail(data, "q4"), [{ key: "q4", title: "Q4" }]);
  assert.deepEqual(trail(data, "loose"), [
    { key: UNFILED, title: "Unfiled" },
    { key: "loose", title: "LOOSE" },
  ]);
  assert.deepEqual(trail(data, UNFILED), [{ key: UNFILED, title: "Unfiled" }]);
  assert.deepEqual(trail(data, "gone"), []);
});

test("ticket, project and workspace ops refetch; text edits do not", () => {
  assert.ok(wantsOp({ kind: "state.transition", id: "AGT-1" }));
  assert.ok(wantsOp({ kind: "ticket.create", id: "AGT-?" }));
  assert.ok(wantsOp({ kind: "field.set", id: "AGT-1" }));
  assert.ok(wantsOp({ kind: "project.set", id: null }));
  assert.ok(wantsOp({ kind: "project.create", id: null }));
  assert.ok(wantsOp({ kind: "workspace.set", id: null }));
  assert.ok(wantsOp({ kind: "state.upsert", id: null }), "a state's category can change");
  assert.ok(!wantsOp({ kind: "actor.upsert", id: null }));
  assert.ok(!wantsOp({ kind: "body.edit", id: "AGT-1" }));
  assert.ok(!wantsOp({ kind: "body.edit", id: null }));
});
