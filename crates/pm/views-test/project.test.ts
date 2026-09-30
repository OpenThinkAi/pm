// Tests for the project view's logic (crates/pm/views/lib/project.ts,
// AGT-1405). Run with Node >= 22.18 (built-in type stripping, no install,
// no browser):
//
//   node --test crates/pm/views-test/*.test.ts
//
// Not part of `cargo test`: the four required checks stay Rust-only. The
// binding itself (a project document's BodySync against a real `pm app`)
// is crates/pm/tests/views/project.test.ts, which `cargo test` runs.

import assert from "node:assert/strict";
import { test } from "node:test";

import {
  DESIGN_DOC,
  createdRef,
  docBodyPath,
  docTabs,
  newTicketRequest,
  opEffect,
  pickTab,
  ticketSections,
} from "../views/lib/project.ts";
import type { Ticket, WorkflowState } from "../views/lib/board.ts";

test("the design doc comes first, then named documents by name", () => {
  const tabs = docTabs({ documents: { zeta: "", "run notes": "", alpha: "" } });
  assert.deepEqual(
    tabs.map((t) => [t.key, t.name, t.label]),
    [
      ["", null, "Design doc"],
      ["doc:alpha", "alpha", "alpha"],
      ["doc:run notes", "run notes", "run notes"],
      ["doc:zeta", "zeta", "zeta"],
    ],
  );
  assert.deepEqual(docTabs({ documents: {} }), [DESIGN_DOC]);
});

test("a vanished document falls back to the design doc", () => {
  const tabs = docTabs({ documents: { notes: "" } });
  assert.equal(pickTab(tabs, "doc:notes").name, "notes");
  assert.equal(pickTab(tabs, "doc:deleted"), DESIGN_DOC);
  assert.equal(pickTab(tabs, ""), DESIGN_DOC);
});

test("each tab binds its own body endpoint, names percent-encoded", () => {
  assert.equal(docBodyPath("pm", DESIGN_DOC), "/projects/pm/body");
  assert.equal(docBodyPath("pm", { name: "run notes" }), "/projects/pm/docs/run%20notes/body");
  assert.equal(docBodyPath("pm", { name: "a/b?c" }), "/projects/pm/docs/a%2Fb%3Fc/body");
});

test("an op event pulls the open document, refetches the list or the project", () => {
  const doc = "01DOC";
  // The open document's edit: pull, nothing else.
  assert.deepEqual(opEffect({ kind: "body.edit", entity: doc, id: null }, doc), {
    pullDoc: true,
    project: false,
    tickets: false,
  });
  // Another document's edit: the project (tabs) may care, the list not.
  assert.deepEqual(opEffect({ kind: "body.edit", entity: "01OTHER", id: null }, doc), {
    pullDoc: false,
    project: true,
    tickets: false,
  });
  // Before the document is bound, nothing is pulled.
  assert.equal(opEffect({ kind: "body.edit", entity: doc, id: null }, null).pullDoc, false);
  // A ticket op (a description edit included) refetches the list only.
  for (const kind of ["ticket.create", "field.set", "state.transition", "body.edit"]) {
    assert.deepEqual(opEffect({ kind, entity: "01T", id: "AGT-3" }, doc), {
      pullDoc: false,
      project: false,
      tickets: true,
    });
  }
  // Project metadata, a new document, a workspace change: both.
  for (const kind of ["project.set", "project.doc_add", "project.delete", "state.upsert"]) {
    assert.deepEqual(opEffect({ kind, entity: "01P", id: null }, doc), {
      pullDoc: false,
      project: true,
      tickets: true,
    });
  }
});

const states: WorkflowState[] = [
  { name: "done", category: "completed", position: 2 },
  { name: "triage", category: "unstarted", position: 0 },
  { name: "in-progress", category: "started", position: 1 },
];

function ticket(id: string, state: string, number: number | null = 1): Ticket {
  return {
    id,
    ulid: `01${id.replace(/\W/g, "")}`,
    number,
    title: id,
    state,
    priority: "medium",
    project: "pm",
    assignee: null,
    labels: [],
    hold: null,
    parked: null,
  };
}

test("the ticket list is sectioned by state in workflow order, empty states dropped", () => {
  const sections = ticketSections(
    [ticket("AGT-3", "done"), ticket("AGT-1", "triage"), ticket("AGT-2", "triage"), ticket("AGT-9", "gone")],
    states,
  );
  assert.deepEqual(
    sections.map((s) => [s.state.name, s.tickets.map((t) => t.id)]),
    [
      ["triage", ["AGT-1", "AGT-2"]],
      ["done", ["AGT-3"]],
      ["gone", ["AGT-9"]],
    ],
  );
});

test("New ticket sends pm new's title and project, and opens by ref", () => {
  assert.deepEqual(newTicketRequest("  Write it  ", "pm"), { title: "Write it", project: "pm" });
  assert.equal(newTicketRequest("   ", "pm"), null);
  // Numbered: its display id. Pending a hub number: its ULID, never AGT-?.
  assert.equal(createdRef({ id: "AGT-7", ulid: "01ABC", number: 7 }), "AGT-7");
  assert.equal(createdRef({ id: "AGT-?", ulid: "01ABC", number: null }), "01ABC");
});
