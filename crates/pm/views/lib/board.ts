// The board's logic (AGT-1404; docs/app-api.md §Views), kept free of React
// and the DOM so `node --test crates/pm/views-test/*.test.ts` can exercise it:
// column order, filters, card markers, and the move a drop becomes.
//
// Imports nothing, and uses only erasable TypeScript (no enums, no
// parameter properties), so Node's built-in type stripping runs it as is.

/** One workflow state, as `GET /workspace` lists it. */
export interface WorkflowState {
  name: string;
  /** `backlog`, `unstarted`, `started`, `completed` (or `canceled`). */
  category: string;
  position: number;
}

/** `GET /workspace`. */
export interface Workspace {
  id: string;
  prefix: string;
  states: WorkflowState[];
  gate_labels: string[];
  actor: string;
}

/** The fields of cli-contract's **Ticket** the board reads. */
export interface Ticket {
  /** Display id: `AGT-12`, or `AGT-?` while the hub has yet to number it. */
  id: string;
  /** Permanent identity; the only way to name a pending ticket. */
  ulid: string;
  number: number | null;
  title: string;
  state: string;
  priority: string;
  project: string | null;
  assignee: string | null;
  labels: string[];
  hold: { reason: string; by: string } | null;
  parked: { until: string } | null;
}

/**
 * What to hand the API (or a `pm` command) to name `t`: the display id
 * once numbered, the ULID while pending — never `AGT-?`, which is not an
 * id (cli-contract §Ticket ids).
 */
export function ref(t: Pick<Ticket, "id" | "ulid" | "number">): string {
  return t.number === null || t.id.endsWith("-?") ? t.ulid : t.id;
}

/** States in workflow order (by `position`, then name for stability). */
export function orderStates(states: WorkflowState[]): WorkflowState[] {
  return [...states].sort((a, b) => a.position - b.position || a.name.localeCompare(b.name));
}

export interface Column {
  state: WorkflowState;
  tickets: Ticket[];
}

/**
 * One column per workflow state, in state order, each keeping the order
 * the tickets came in (`pm list`'s). A ticket whose state the workspace no
 * longer defines is not dropped: it gets a trailing column of its own.
 */
export function groupByState(tickets: Ticket[], states: WorkflowState[]): Column[] {
  const columns = orderStates(states).map((state) => ({ state, tickets: [] as Ticket[] }));
  const byName = new Map(columns.map((c) => [c.state.name, c]));
  for (const t of tickets) {
    let column = byName.get(t.state);
    if (!column) {
      column = {
        state: { name: t.state, category: "unknown", position: Number.MAX_SAFE_INTEGER },
        tickets: [],
      };
      byName.set(t.state, column);
      columns.push(column);
    }
    column.tickets.push(t);
  }
  return columns;
}

/** The value of the "unassigned" choice in the assignee filter. */
export const UNASSIGNED = "\u0000unassigned";

/** The board's filters; `""` means "any". */
export interface Filters {
  project: string;
  label: string;
  assignee: string;
}

export const NO_FILTERS: Filters = { project: "", label: "", assignee: "" };

export function isFiltered(f: Filters): boolean {
  return f.project !== "" || f.label !== "" || f.assignee !== "";
}

/** The tickets every set filter admits (AND across filters). */
export function applyFilters(tickets: Ticket[], f: Filters): Ticket[] {
  return tickets.filter(
    (t) =>
      (f.project === "" || t.project === f.project) &&
      (f.label === "" || t.labels.includes(f.label)) &&
      (f.assignee === "" ||
        (f.assignee === UNASSIGNED ? t.assignee === null : t.assignee === f.assignee)),
  );
}

export interface FilterOptions {
  projects: string[];
  labels: string[];
  assignees: string[];
}

/**
 * The choices each filter offers: every value the loaded tickets carry,
 * plus `known` projects (so an empty project is still pickable) and the
 * current selection (so a persisted filter whose tickets all left stays
 * visible and can be cleared).
 */
export function filterOptions(tickets: Ticket[], current: Filters, known: string[] = []): FilterOptions {
  const projects = new Set(known);
  const labels = new Set<string>();
  const assignees = new Set<string>();
  for (const t of tickets) {
    if (t.project) projects.add(t.project);
    for (const l of t.labels) labels.add(l);
    if (t.assignee) assignees.add(t.assignee);
  }
  if (current.project) projects.add(current.project);
  if (current.label) labels.add(current.label);
  if (current.assignee && current.assignee !== UNASSIGNED) assignees.add(current.assignee);
  const sorted = (s: Set<string>) => [...s].sort((a, b) => a.localeCompare(b));
  return { projects: sorted(projects), labels: sorted(labels), assignees: sorted(assignees) };
}

/** A marker on a card. */
export interface Marker {
  kind: "held" | "parked" | "gate";
  text: string;
  /** The longer explanation, for a tooltip / accessible description. */
  title: string;
}

/** Today as `YYYY-MM-DD` in local time — how `parked` dates are written. */
export function localDate(now: Date = new Date()): string {
  const pad = (n: number) => String(n).padStart(2, "0");
  return `${now.getFullYear()}-${pad(now.getMonth() + 1)}-${pad(now.getDate())}`;
}

/**
 * The card's markers: held (with the reason), parked (while the park is
 * active — `forever`, or `until` not yet past, as pm-core's
 * `Parked::is_active` reads it), and each gate label it carries.
 */
export function markers(t: Ticket, gateLabels: string[], today: string): Marker[] {
  const out: Marker[] = [];
  if (t.hold) {
    out.push({ kind: "held", text: "held", title: `held by ${t.hold.by}: ${t.hold.reason}` });
  }
  if (t.parked && (t.parked.until === "forever" || t.parked.until >= today)) {
    const until = t.parked.until === "forever" ? "indefinitely" : `until ${t.parked.until}`;
    out.push({ kind: "parked", text: "parked", title: `parked ${until}` });
  }
  for (const label of t.labels) {
    if (gateLabels.includes(label)) {
      out.push({ kind: "gate", text: label, title: `gate label: ${label} (pm ready skips it)` });
    }
  }
  return out;
}

/** The request a board move sends: `POST /tickets/{ref}/state`. */
export interface MoveRequest {
  path: string;
  body: { state: string; keep_assignee: boolean };
}

export type MovePlan =
  | { kind: "move"; request: MoveRequest }
  | { kind: "noop" }
  | { kind: "refused"; reason: string };

/**
 * What dropping `t` on `target` does — the one policy for drag-and-drop
 * and the keyboard move menu alike:
 *
 * - Into its own state: nothing.
 * - Into a **started** state from anything but another started state:
 *   refused. Starting work is a claim, and claims stay CLI-only (they need
 *   the hub; `pm claim` is exit 75 when someone else won). A `pm move`
 *   there would leave the ticket started with nobody on it, or keep a
 *   stale assignee — neither is what a drag means — so the board says to
 *   run `pm claim` instead.
 * - Anything else: `pm move` exactly — `keep_assignee: false`, so moving
 *   into an unstarted/backlog state clears the assignee (un-claims, as
 *   AGT-1379's `pm move` does) and every other move keeps it.
 */
export function planMove(t: Ticket, target: string, states: WorkflowState[]): MovePlan {
  if (t.state === target) return { kind: "noop" };
  const to = states.find((s) => s.name === target);
  if (!to) return { kind: "refused", reason: `${target} is not a state of this workspace` };
  const from = states.find((s) => s.name === t.state);
  if (to.category === "started" && from?.category !== "started") {
    return {
      kind: "refused",
      reason: `Starting work is a claim, and claims stay in the CLI: run \`pm claim ${ref(t)}\`.`,
    };
  }
  return {
    kind: "move",
    request: {
      path: `/tickets/${encodeURIComponent(ref(t))}/state`,
      body: { state: target, keep_assignee: false },
    },
  };
}

/** `t` as it reads once the move lands — the optimistic card. */
export function moved(t: Ticket, target: string, states: WorkflowState[]): Ticket {
  const to = states.find((s) => s.name === target);
  const clears = to !== undefined && (to.category === "unstarted" || to.category === "backlog");
  return { ...t, state: target, assignee: clears ? null : t.assignee };
}

/** The localStorage key a workspace's filters persist under. */
export function filtersKey(workspaceId: string): string {
  return `pm.board.filters.${workspaceId}`;
}

/** Minimal `Storage`, so tests can pass a map. */
export interface KeyValue {
  getItem(key: string): string | null;
  setItem(key: string, value: string): void;
}

/**
 * The persisted filters, or none. Storage can be missing or throw (a
 * private window, blocked site data), and a stored value can be garbage:
 * every such case is "no filters", never an error.
 */
export function loadFilters(storage: KeyValue | null | undefined, key: string): Filters {
  try {
    const raw = storage?.getItem(key);
    if (!raw) return NO_FILTERS;
    const v = JSON.parse(raw) as Record<string, unknown>;
    const str = (x: unknown) => (typeof x === "string" ? x : "");
    return { project: str(v.project), label: str(v.label), assignee: str(v.assignee) };
  } catch {
    return NO_FILTERS;
  }
}

export function saveFilters(storage: KeyValue | null | undefined, key: string, f: Filters): void {
  try {
    storage?.setItem(key, JSON.stringify(f));
  } catch {
    // Not persisting is fine: filters are a per-viewer convenience.
  }
}

/**
 * `fn`, called once `ms` after the last of a burst of calls — so a
 * `pm new --batch` or a sync pull of fifty ops is one refetch, not fifty.
 */
export function debounce(fn: () => void, ms: number): { call: () => void; cancel: () => void } {
  let timer: ReturnType<typeof setTimeout> | null = null;
  return {
    call() {
      if (timer !== null) clearTimeout(timer);
      timer = setTimeout(() => {
        timer = null;
        fn();
      }, ms);
    },
    cancel() {
      if (timer !== null) clearTimeout(timer);
      timer = null;
    },
  };
}
