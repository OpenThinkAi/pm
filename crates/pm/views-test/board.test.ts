// Tests for the board's logic (crates/pm/views/lib/board.ts, AGT-1404).
// Run with Node >= 22.18 (built-in type stripping, no install, no browser):
//
//   node --test crates/pm/views-test/*.test.ts
//
// Not part of `cargo test`: the four required checks stay Rust-only.

import assert from "node:assert/strict";
import { test } from "node:test";

import {
  NO_FILTERS,
  UNASSIGNED,
  applyFilters,
  debounce,
  filterOptions,
  filtersKey,
  groupByState,
  loadFilters,
  localDate,
  markers,
  moved,
  orderStates,
  planMove,
  ref,
  saveFilters,
  type Ticket,
  type WorkflowState,
} from "../views/lib/board.ts";

// The oss workspace's states, deliberately out of order.
const states: WorkflowState[] = [
  { name: "done", category: "completed", position: 3 },
  { name: "todo", category: "unstarted", position: 1 },
  { name: "in-progress", category: "started", position: 2 },
  { name: "backlog", category: "backlog", position: 0 },
];

let n = 0;
function ticket(over: Partial<Ticket> = {}): Ticket {
  n += 1;
  return {
    id: `PM-${n}`,
    ulid: `01ULID${String(n).padStart(20, "0")}`,
    number: n,
    title: `ticket ${n}`,
    state: "todo",
    priority: "medium",
    project: null,
    assignee: null,
    labels: [],
    hold: null,
    parked: null,
    ...over,
  };
}

test("columns follow state position, not the order /workspace lists them", () => {
  assert.deepEqual(
    orderStates(states).map((s) => s.name),
    ["backlog", "todo", "in-progress", "done"],
  );
});

test("groupByState puts each ticket in its state's column, keeping list order", () => {
  const a = ticket({ state: "done" });
  const b = ticket({ state: "todo" });
  const c = ticket({ state: "todo" });
  const columns = groupByState([a, b, c], states);
  assert.deepEqual(
    columns.map((col) => [col.state.name, col.tickets.map((t) => t.id)]),
    [
      ["backlog", []],
      ["todo", [b.id, c.id]],
      ["in-progress", []],
      ["done", [a.id]],
    ],
  );
});

test("a ticket in a state the workspace no longer defines is not dropped", () => {
  const stray = ticket({ state: "review" });
  const columns = groupByState([stray], states);
  assert.equal(columns.length, 5);
  assert.equal(columns[4].state.name, "review");
  assert.deepEqual(columns[4].tickets, [stray]);
});

test("filters AND across project, label and assignee", () => {
  const a = ticket({ project: "pm", labels: ["ui"], assignee: "matt" });
  const b = ticket({ project: "pm", labels: ["cli"], assignee: null });
  const c = ticket({ project: "other", labels: ["ui"], assignee: "matt" });
  assert.deepEqual(applyFilters([a, b, c], NO_FILTERS), [a, b, c]);
  assert.deepEqual(applyFilters([a, b, c], { ...NO_FILTERS, project: "pm" }), [a, b]);
  assert.deepEqual(applyFilters([a, b, c], { ...NO_FILTERS, label: "ui" }), [a, c]);
  assert.deepEqual(applyFilters([a, b, c], { project: "pm", label: "ui", assignee: "" }), [a]);
  assert.deepEqual(applyFilters([a, b, c], { ...NO_FILTERS, assignee: UNASSIGNED }), [b]);
  assert.deepEqual(applyFilters([a, b, c], { ...NO_FILTERS, assignee: "matt" }), [a, c]);
});

test("filter options are sorted, include known projects and the current choice", () => {
  const a = ticket({ project: "zeta", labels: ["b", "a"], assignee: "matt" });
  const opts = filterOptions([a], { project: "", label: "gone", assignee: UNASSIGNED }, ["alpha"]);
  assert.deepEqual(opts.projects, ["alpha", "zeta"]);
  assert.deepEqual(opts.labels, ["a", "b", "gone"]);
  assert.deepEqual(opts.assignees, ["matt"]);
});

test("markers: held with its reason, active parks only, gate labels", () => {
  const t = ticket({
    hold: { reason: "needs Matt", by: "claude:x" },
    parked: { until: "2026-10-05" },
    labels: ["manual", "ui"],
  });
  const m = markers(t, ["manual"], "2026-09-30");
  assert.deepEqual(
    m.map((x) => [x.kind, x.text]),
    [
      ["held", "held"],
      ["parked", "parked"],
      ["gate", "manual"],
    ],
  );
  assert.match(m[0].title, /needs Matt/);
  assert.match(m[1].title, /until 2026-10-05/);
  // A park that ran out is no longer shown; `forever` always is.
  assert.deepEqual(markers(ticket({ parked: { until: "2026-09-29" } }), [], "2026-09-30"), []);
  assert.equal(markers(ticket({ parked: { until: "2026-09-30" } }), [], "2026-09-30").length, 1);
  assert.match(markers(ticket({ parked: { until: "forever" } }), [], "2026-09-30")[0].title, /indefinitely/);
  assert.deepEqual(markers(ticket(), ["manual"], "2026-09-30"), []);
});

test("localDate is YYYY-MM-DD in local time", () => {
  assert.equal(localDate(new Date(2026, 0, 5, 23, 59)), "2026-01-05");
});

test("ref: the display id once numbered, the ULID while pending (never AGT-?)", () => {
  const numbered = ticket();
  assert.equal(ref(numbered), numbered.id);
  const pending = ticket({ id: "PM-?", number: null });
  assert.equal(ref(pending), pending.ulid);
});

test("a drop is pm move with keep_assignee false, addressed by ref", () => {
  const t = ticket({ state: "in-progress", assignee: "matt" });
  assert.deepEqual(planMove(t, "done", states), {
    kind: "move",
    request: { path: `/tickets/${t.id}/state`, body: { state: "done", keep_assignee: false } },
  });
  const pending = ticket({ id: "PM-?", number: null, state: "backlog" });
  const plan = planMove(pending, "todo", states);
  assert.equal(plan.kind, "move");
  assert.equal(plan.kind === "move" && plan.request.path, `/tickets/${pending.ulid}/state`);
});

test("dropping into a started state is refused: claims stay CLI-only", () => {
  for (const from of ["backlog", "todo", "done"]) {
    const t = ticket({ state: from });
    const plan = planMove(t, "in-progress", states);
    assert.equal(plan.kind, "refused", from);
    assert.match(plan.kind === "refused" ? plan.reason : "", new RegExp(`pm claim ${t.id}`));
  }
  const pending = ticket({ id: "PM-?", number: null });
  const plan = planMove(pending, "in-progress", states);
  assert.match(plan.kind === "refused" ? plan.reason : "", new RegExp(`pm claim ${pending.ulid}`));
});

test("started to started moves (no new claim), same state is a no-op, unknown refused", () => {
  const two: WorkflowState[] = [...states, { name: "review", category: "started", position: 2.5 }];
  const t = ticket({ state: "in-progress", assignee: "matt" });
  assert.equal(planMove(t, "review", two).kind, "move");
  assert.deepEqual(planMove(t, "in-progress", two), { kind: "noop" });
  assert.equal(planMove(t, "nope", two).kind, "refused");
});

test("the optimistic card mirrors pm move's assignee clear", () => {
  const t = ticket({ state: "in-progress", assignee: "matt" });
  assert.equal(moved(t, "todo", states).assignee, null);
  assert.equal(moved(t, "backlog", states).assignee, null);
  assert.equal(moved(t, "done", states).assignee, "matt");
  assert.equal(moved(t, "done", states).state, "done");
});

test("filters persist per workspace and survive broken storage", () => {
  const map = new Map<string, string>();
  const store = { getItem: (k: string) => map.get(k) ?? null, setItem: (k: string, v: string) => void map.set(k, v) };
  const key = filtersKey("01WS");
  assert.deepEqual(loadFilters(store, key), NO_FILTERS);
  saveFilters(store, key, { project: "pm", label: "", assignee: UNASSIGNED });
  assert.deepEqual(loadFilters(store, key), { project: "pm", label: "", assignee: UNASSIGNED });
  assert.deepEqual(loadFilters(store, filtersKey("01OTHER")), NO_FILTERS);

  map.set(key, "{not json");
  assert.deepEqual(loadFilters(store, key), NO_FILTERS);
  map.set(key, JSON.stringify({ project: 7, label: "ui" }));
  assert.deepEqual(loadFilters(store, key), { project: "", label: "ui", assignee: "" });

  const throwing = {
    getItem(): string | null {
      throw new Error("SecurityError");
    },
    setItem(): void {
      throw new Error("QuotaExceeded");
    },
  };
  assert.deepEqual(loadFilters(throwing, key), NO_FILTERS);
  saveFilters(throwing, key, NO_FILTERS);
  assert.deepEqual(loadFilters(null, key), NO_FILTERS);
});

test("debounce collapses a burst of ops into one refetch", async () => {
  let calls = 0;
  const d = debounce(() => calls++, 20);
  for (let i = 0; i < 50; i++) d.call();
  await new Promise((r) => setTimeout(r, 60));
  assert.equal(calls, 1);
  d.call();
  d.cancel();
  await new Promise((r) => setTimeout(r, 40));
  assert.equal(calls, 1);
});
