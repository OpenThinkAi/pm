// The initiatives entry view's logic (AGT-1492; docs/app-api.md §The
// initiatives view), kept free of React and the DOM so
// `node --test crates/pm/views-test/*.test.ts` can exercise it: the
// landing page's rows, an initiative's (or Unfiled's) project rows, the
// progress figure, the breadcrumb trail to a project, and which `/events`
// ops refetch `GET /initiatives`.
//
// Import-free, erasable TypeScript, so Node's built-in type stripping runs
// it as is.

/** A rollup: unarchived tickets counted by state category. */
export interface Counts {
  backlog: number;
  unstarted: number;
  started: number;
  completed: number;
  canceled: number;
}

/** One `GET /initiatives` node (docs/app-api.md §`GET /initiatives`). */
export interface Node {
  id: string;
  title: string;
  kind: string;
  status: string;
  /** The node's own tickets. */
  tickets: Counts;
  /** Its own plus every descendant's shown. */
  total: Counts;
  children: Node[];
}

/** The `GET /initiatives` answer. */
export interface Initiatives {
  initiatives: Node[];
  unfiled: { total: Counts; projects: Node[] };
}

/** The Unfiled group's key where a node id would go (ids are kebab-case, so
 * no project can be named this). */
export const UNFILED = " unfiled";

/** A compact rollup row: an initiative, Unfiled, or a project. */
export interface Row {
  /** A node id, or {@link UNFILED}. */
  key: string;
  title: string;
  /** Nesting under the page's group (0 for the group's own projects). */
  depth: number;
  /** Projects below it (every descendant); null for a project row. */
  projects: number | null;
  /** Not started yet: backlog + unstarted. */
  unstarted: number;
  started: number;
  completed: number;
  /** completed ÷ non-canceled, 0..1; null when nothing counts. */
  progress: number | null;
  status: string | null;
}

/** completed ÷ non-canceled tickets, or null with none. */
export function progress(c: Counts): number | null {
  const live = c.backlog + c.unstarted + c.started + c.completed;
  return live === 0 ? null : c.completed / live;
}

/** "40%", or "—" with nothing to count. Floors, so "100%" means all done. */
export function percent(p: number | null): string {
  return p === null ? "—" : `${Math.floor(p * 100)}%`;
}

function descendants(nodes: Node[]): number {
  return nodes.reduce((n, c) => n + 1 + descendants(c.children), 0);
}

function row(key: string, title: string, total: Counts, extra: Pick<Row, "depth" | "projects" | "status">): Row {
  return {
    key,
    title,
    unstarted: total.backlog + total.unstarted,
    started: total.started,
    completed: total.completed,
    progress: progress(total),
    ...extra,
  };
}

/** The landing page: every initiative, then Unfiled. */
export function landingRows(data: Initiatives): Row[] {
  return [
    ...data.initiatives.map((n) =>
      row(n.id, n.title, n.total, { depth: 0, projects: descendants(n.children), status: n.status }),
    ),
    row(UNFILED, "Unfiled", data.unfiled.total, {
      depth: 0,
      projects: descendants(data.unfiled.projects),
      status: null,
    }),
  ];
}

/** Projects as rows, sub-projects right after their parent, one deeper. */
export function projectRows(nodes: Node[], depth = 0): Row[] {
  return nodes.flatMap((n) => [
    row(n.id, n.title, n.total, { depth, projects: null, status: n.status }),
    ...projectRows(n.children, depth + 1),
  ]);
}

/** A group: an initiative, or Unfiled. */
export interface Group {
  key: string;
  title: string;
  /** The initiative itself, or null for Unfiled. */
  node: Node | null;
  projects: Node[];
}

/** The group `key` names, or null when it is not (or no longer) there. */
export function group(data: Initiatives, key: string): Group | null {
  if (key === UNFILED) return { key, title: "Unfiled", node: null, projects: data.unfiled.projects };
  const n = data.initiatives.find((i) => i.id === key);
  return n ? { key, title: n.title, node: n, projects: n.children } : null;
}

/** One breadcrumb step after "Initiatives". */
export interface Crumb {
  key: string;
  title: string;
}

/**
 * The breadcrumb after "Initiatives" to `key`: its group, then every
 * project down to it (`[]` for a key the tree does not show — a project
 * deleted, or filtered out — so the page still opens it, under the root).
 */
export function trail(data: Initiatives, key: string): Crumb[] {
  const walk = (nodes: Node[], path: Crumb[]): Crumb[] | null => {
    for (const n of nodes) {
      const here = [...path, { key: n.id, title: n.title }];
      if (n.id === key) return here;
      const below = walk(n.children, here);
      if (below) return below;
    }
    return null;
  };
  if (key === UNFILED) return [{ key, title: "Unfiled" }];
  return (
    walk(data.initiatives, []) ?? walk(data.unfiled.projects, [{ key: UNFILED, title: "Unfiled" }]) ?? []
  );
}

/** The `GET /events` op fields this view reads. */
export interface OpLike {
  kind: string;
  /** The display id when the op is on a ticket, else null. */
  id: string | null;
}

/**
 * Whether an op can change `GET /initiatives`: any ticket op (a count, a
 * project), any project op (title, status, parent, a new one) and a
 * workspace or state op (`state.upsert` can change a state's category).
 * Text edits (`body.edit`, on a ticket's description or a project
 * document) cannot.
 */
export function wantsOp(op: OpLike): boolean {
  if (op.kind === "body.edit") return false;
  return op.id !== null || /^(workspace|project)\./.test(op.kind) || op.kind === "state.upsert";
}
