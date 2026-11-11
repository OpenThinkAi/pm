// The board's persisted filters (AGT-2434): no filter set means no stored
// key at all. Pure, no `pm app` needed; run by crates/pm/tests/views_js.rs
// with the other view tests.

import { test } from "node:test";
import assert from "node:assert/strict";

import { NO_FILTERS, loadFilters, saveFilters } from "../../views/lib/board.ts";

function memory() {
  const map = new Map<string, string>();
  return {
    map,
    getItem: (k: string) => map.get(k) ?? null,
    setItem: (k: string, v: string) => void map.set(k, v),
    removeItem: (k: string) => void map.delete(k),
  };
}

test("saving no filters removes the stored key", () => {
  const s = memory();
  saveFilters(s, "board:pm", { project: "pm", label: "", assignee: "" });
  assert.deepEqual(loadFilters(s, "board:pm"), { project: "pm", label: "", assignee: "" });
  saveFilters(s, "board:pm", NO_FILTERS);
  assert.equal(s.map.has("board:pm"), false);
  assert.deepEqual(loadFilters(s, "board:pm"), NO_FILTERS);
});

test("storage that throws is not an error", () => {
  const broken = {
    getItem: () => { throw new Error("blocked"); },
    setItem: () => { throw new Error("blocked"); },
    removeItem: () => { throw new Error("blocked"); },
  };
  saveFilters(broken, "k", NO_FILTERS);
  saveFilters(broken, "k", { project: "x", label: "", assignee: "" });
  assert.deepEqual(loadFilters(broken, "k"), NO_FILTERS);
});
