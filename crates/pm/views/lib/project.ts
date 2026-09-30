// The project view's logic (AGT-1405; docs/app-api.md §The project view),
// kept free of React and the DOM so `node --test crates/pm/views-test/*.test.ts`
// can exercise it: the document tabs, which endpoint each one binds, how
// an op event maps to a refetch or a pull, the ticket list's sections, and
// what "New ticket" sends and then opens.
//
// Imports only lib/board.ts (itself import-free), and uses only erasable
// TypeScript, so Node's built-in type stripping runs it as is.

import { groupByState, ref, type Column, type Ticket, type WorkflowState } from "./board.ts";

/** The fields of cli-contract's **Project** the view reads. */
export interface Project {
  id: string;
  title: string;
  status: string;
  parent: string | null;
  repos: string[];
  /** The design doc's text. */
  doc: string;
  /** Named documents: name -> text. */
  documents: Record<string, string>;
}

/** One document tab: the design doc (`name` null) or a named document. */
export interface DocTab {
  /** Stable across refetches; `""` for the design doc. */
  key: string;
  name: string | null;
  label: string;
}

export const DESIGN_DOC: DocTab = { key: "", name: null, label: "Design doc" };

/**
 * Whether `name` is a safe document name — pm-core's `ids::is_safe_doc_name`
 * (AGT-1464), which every ingest path enforces: at most 255 bytes of
 * `/`-separated segments, each non-empty, not starting with `.` (so never
 * a `.` or `..` dot-segment) and free of `\` and control characters.
 * `encodeURIComponent` leaves `.` alone, so a tab named `..` would bind
 * `/projects/{id}/docs/../body` — which `fetch` resolves to the design
 * doc's endpoint.
 */
export function isSafeDocName(name: string): boolean {
  return (
    new TextEncoder().encode(name).length <= 255 &&
    name
      .split("/")
      .every((seg) => seg !== "" && !seg.startsWith(".") && !/[\\\u0000-\u001f\u007f-\u009f]/.test(seg))
  );
}

/** The design doc first, then every named document by name (a name that is
 * not {@link isSafeDocName} gets no tab). */
export function docTabs(project: Pick<Project, "documents">): DocTab[] {
  const names = Object.keys(project.documents ?? {})
    .filter(isSafeDocName)
    .sort((a, b) => a.localeCompare(b));
  return [DESIGN_DOC, ...names.map((name) => ({ key: `doc:${name}`, name, label: name }))];
}

/**
 * The tab to show: `wanted` while the project still has it, else the
 * design doc (a named document deleted, or a stale remembered key).
 */
export function pickTab(tabs: DocTab[], wanted: string): DocTab {
  return tabs.find((t) => t.key === wanted) ?? DESIGN_DOC;
}

/**
 * A document's body endpoint, for lib/body.ts's `bodyTransport`: the
 * design doc is `/projects/{id}/body`, a named document
 * `/projects/{id}/docs/{name}/body`.
 */
export function docBodyPath(project: string, tab: Pick<DocTab, "name">): string {
  const base = `/projects/${encodeURIComponent(project)}`;
  if (tab.name === null) return `${base}/body`;
  // Never let a name resolve to another document's endpoint.
  if (!isSafeDocName(tab.name)) throw new Error(`unsafe document name ${JSON.stringify(tab.name)}`);
  return `${base}/docs/${encodeURIComponent(tab.name)}/body`;
}

/** The `GET /events` op fields the view reads. */
export interface OpLike {
  kind: string;
  entity: string;
  /** The display id when the op is on a ticket, else null. */
  id: string | null;
}

/** What an op means for an open project view. */
export interface OpEffect {
  /** Pull the open document's CRDT ops (a `body.edit` on its `doc_id`). */
  pullDoc: boolean;
  /** Refetch the project (its metadata or its document list may have changed). */
  project: boolean;
  /** Refetch the ticket list. */
  tickets: boolean;
}

/**
 * - A `body.edit` on the open document's `doc_id` pulls it — and nothing
 *   else: document text is not shown anywhere but the editor.
 * - Any ticket op (`id` set) refetches the list: a ticket may have joined
 *   or left the project, or changed state or title.
 * - Any other op without a ticket id — `project.set`, `project.doc_add`,
 *   `project.delete`, a workspace change, another document's edit —
 *   refetches the project (tabs, title, status); a workspace change
 *   (states) can reorder the list, so it refetches that too.
 */
export function opEffect(op: OpLike, openDocId: string | null): OpEffect {
  if (op.kind === "body.edit" && openDocId !== null && op.entity === openDocId) {
    return { pullDoc: true, project: false, tickets: false };
  }
  if (op.id !== null) return { pullDoc: false, project: false, tickets: true };
  if (op.kind === "body.edit") return { pullDoc: false, project: true, tickets: false };
  return { pullDoc: false, project: true, tickets: true };
}

/**
 * The ticket list: one section per workflow state in state order, empty
 * sections dropped (board.ts's `groupByState`, so a state the workspace
 * no longer defines still shows).
 */
export function ticketSections(tickets: Ticket[], states: WorkflowState[]): Column[] {
  return groupByState(tickets, states).filter((c) => c.tickets.length > 0);
}

/**
 * `POST /tickets`'s body for "New ticket": `pm new --title <title>
 * --project <project>`. `null` for a blank title (nothing to file).
 */
export function newTicketRequest(title: string, project: string): { title: string; project: string } | null {
  const t = title.trim();
  return t === "" ? null : { title: t, project };
}

/**
 * The ref to open a ticket `POST /tickets` just made: its display id, or
 * — with a hub configured, until `pm sync` brings its number — its ULID,
 * since `AGT-?` names nothing.
 */
export function createdRef(created: Pick<Ticket, "id" | "ulid" | "number">): string {
  return ref(created);
}

export { ref };
