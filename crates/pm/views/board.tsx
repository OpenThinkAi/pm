// `pm app`'s board (AGT-1404; docs/app-api.md §Views): a column per
// workflow state in state order, a card per unarchived ticket (id, title,
// priority, labels, assignee, held/parked/gate markers), drag-and-drop —
// or each card's Move menu, the keyboard route — to `pm move` a ticket,
// filters by project/label/assignee remembered per viewer, and a live
// refetch whenever an op lands from anywhere (this board, a terminal, a
// sync pull). The policy (what a drop does, what a filter admits) lives in
// lib/board.ts, which `node --test crates/pm/views-test/*.test.ts` covers.
//
// Claims stay CLI-only: a drop into a started state is refused with a note
// to run `pm claim` (see `planMove`). Clicking a card opens its details in
// a side panel; ui-leaf cannot open a second view from inside one, so
// editing is `pm edit <ID>`, which the panel offers to copy.

import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import {
  NO_FILTERS,
  UNASSIGNED,
  applyFilters,
  debounce,
  filterOptions,
  filtersKey,
  groupByState,
  isFiltered,
  loadFilters,
  localDate,
  markers,
  moved,
  orderStates,
  planMove,
  ref,
  saveFilters,
  type Filters,
  type Ticket,
  type Workspace,
} from "./lib/board";
import { type Api, baseCss, useApi, useEvents, type ViewProps } from "./lib/pm";

interface Project {
  id: string;
}

interface Comment {
  author: string;
  at: string;
  body: string;
}

interface Detail extends Ticket {
  description: string;
  blocked_by: string[];
  comments: Comment[];
}

interface Toast {
  seq: number;
  text: string;
  error: boolean;
}

function storage(): Storage | null {
  try {
    return window.localStorage;
  } catch {
    return null;
  }
}

function message(e: unknown): string {
  return e instanceof Error ? e.message : String(e);
}

export default function Board({ mutate }: ViewProps) {
  const { api, error: connectError } = useApi(mutate);
  const [workspace, setWorkspace] = useState<Workspace | null>(null);
  const [tickets, setTickets] = useState<Ticket[]>([]);
  const [projects, setProjects] = useState<string[]>([]);
  const [error, setError] = useState<string | null>(null);
  const [filters, setFilters] = useState<Filters>(NO_FILTERS);
  const [dragging, setDragging] = useState<string | null>(null);
  const [over, setOver] = useState<string | null>(null);
  const [open, setOpen] = useState<string | null>(null);
  const [toast, setToast] = useState<Toast | null>(null);
  const fetchSeq = useRef(0);

  const say = useCallback((text: string, isError = false) => {
    setToast((prev) => ({ seq: (prev?.seq ?? 0) + 1, text, error: isError }));
  }, []);
  useEffect(() => {
    if (!toast) return;
    const t = setTimeout(() => setToast((cur) => (cur?.seq === toast.seq ? null : cur)), 6000);
    return () => clearTimeout(t);
  }, [toast]);

  const refresh = useCallback(() => {
    if (!api) return;
    // Only the newest fetch may land: a slow one must not overwrite a
    // fresher answer (or undo an optimistic move it predates).
    const seq = ++fetchSeq.current;
    Promise.all([
      api.get<Workspace>("/workspace"),
      api.get<Ticket[]>("/tickets"),
      api.get<{ projects: Project[] }>("/projects"),
    ]).then(
      ([ws, list, proj]) => {
        if (seq !== fetchSeq.current) return;
        setWorkspace(ws);
        setTickets(list);
        setProjects(proj.projects.map((p) => p.id));
        setError(null);
      },
      (e) => {
        if (seq === fetchSeq.current) setError(message(e));
      },
    );
  }, [api]);

  useEffect(refresh, [refresh]);

  const debounced = useMemo(() => debounce(refresh, 150), [refresh]);
  useEffect(() => debounced.cancel, [debounced]);
  // Any ticket op can move, add or change a card; workspace ops can change
  // the columns; project ops the project filter.
  useEvents(api, debounced.call, (op) => op.id !== null || /^(workspace|project)/.test(op.kind));

  // Filters persist per viewer and per workspace.
  const key = workspace ? filtersKey(workspace.id) : null;
  useEffect(() => {
    if (key) setFilters(loadFilters(storage(), key));
  }, [key]);
  const updateFilters = (next: Filters) => {
    setFilters(next);
    if (key) saveFilters(storage(), key, next);
  };

  const move = useCallback(
    (t: Ticket, target: string) => {
      if (!api || !workspace) return;
      const plan = planMove(t, target, workspace.states);
      if (plan.kind === "noop") return;
      if (plan.kind === "refused") {
        say(plan.reason, true);
        return;
      }
      // Optimistic: the card moves now; the op's event refetches the truth.
      fetchSeq.current++;
      setTickets((all) => all.map((x) => (x.ulid === t.ulid ? moved(x, target, workspace.states) : x)));
      api.post<Ticket>(plan.request.path, plan.request.body).then(
        (after) => {
          const cleared = t.assignee !== null && after.assignee === null;
          say(`${after.id} → ${after.state}${cleared ? ` (unassigned ${t.assignee})` : ""}`);
        },
        (e) => {
          say(`Moving ${t.id} failed: ${message(e)}`, true);
          refresh();
        },
      );
    },
    [api, workspace, say, refresh],
  );

  const failure = connectError ?? error;
  if (failure && !workspace) return <p className="pm-error">pm: {failure}</p>;
  if (!workspace) return <p className="pm-muted" style={{ padding: "1rem" }}>Loading…</p>;

  const states = orderStates(workspace.states);
  const visible = applyFilters(tickets, filters);
  const columns = groupByState(visible, states);
  const options = filterOptions(tickets, filters, projects);
  const today = localDate();
  const dragged = dragging ? tickets.find((t) => t.ulid === dragging) : undefined;
  const openTicket = open ? tickets.find((t) => t.ulid === open) : undefined;

  const filterSelect = (
    name: keyof Filters,
    label: string,
    values: string[],
    extra?: [string, string],
  ) => (
    <label className="pm-filter">
      {label}
      <select value={filters[name]} onChange={(e) => updateFilters({ ...filters, [name]: e.target.value })}>
        <option value="">any</option>
        {extra && <option value={extra[0]}>{extra[1]}</option>}
        {values.map((v) => (
          <option key={v} value={v}>
            {v}
          </option>
        ))}
      </select>
    </label>
  );

  return (
    <div className="pm-board">
      <style>{baseCss + boardCss}</style>
      <header className="pm-bar">
        <h1>
          {workspace.prefix} board{" "}
          <span className="pm-muted">
            · {visible.length}
            {visible.length !== tickets.length ? ` of ${tickets.length}` : ""} tickets
          </span>
        </h1>
        <div className="pm-filters" role="group" aria-label="Filters">
          {filterSelect("project", "Project", options.projects)}
          {filterSelect("label", "Label", options.labels)}
          {filterSelect("assignee", "Assignee", options.assignees, [UNASSIGNED, "unassigned"])}
          {isFiltered(filters) && (
            <button type="button" className="pm-link" onClick={() => updateFilters(NO_FILTERS)}>
              clear
            </button>
          )}
        </div>
        {failure && <span className="pm-error-inline">pm: {failure}</span>}
      </header>
      <main className="pm-columns">
        {columns.map(({ state, tickets: column }) => {
          const plan = dragged ? planMove(dragged, state.name, workspace.states) : null;
          const hot = over === state.name && plan !== null && plan.kind !== "noop";
          const refused = plan?.kind === "refused";
          return (
            <section
              key={state.name}
              data-state={state.name}
              aria-label={`${state.name}, ${column.length} tickets`}
              className={`pm-column${hot ? (refused ? " pm-refuse" : " pm-accept") : ""}`}
              onDragOver={(e) => {
                if (!dragged) return;
                e.preventDefault();
                // A refused target still takes the drop, so the drop can say
                // why (the column is outlined red meanwhile).
                e.dataTransfer.dropEffect = plan?.kind === "noop" ? "none" : "move";
                if (over !== state.name) setOver(state.name);
              }}
              onDragLeave={(e) => {
                if (!e.currentTarget.contains(e.relatedTarget as Node | null)) setOver(null);
              }}
              onDrop={(e) => {
                e.preventDefault();
                setOver(null);
                setDragging(null);
                if (dragged) move(dragged, state.name);
              }}
            >
              <h2>
                {state.name} <span className="pm-muted">{column.length}</span>
                {state.category === "started" && (
                  <span className="pm-muted pm-hint" title="Claims stay in the CLI: pm claim <ID>">
                    {" "}
                    · claim via CLI
                  </span>
                )}
              </h2>
              {column.map((t) => (
                <Card
                  key={t.ulid}
                  ticket={t}
                  workspace={workspace}
                  today={today}
                  dragging={dragging === t.ulid}
                  onDragStart={() => setDragging(t.ulid)}
                  onDragEnd={() => {
                    setDragging(null);
                    setOver(null);
                  }}
                  onOpen={() => setOpen(t.ulid)}
                  onMove={(target) => move(t, target)}
                />
              ))}
            </section>
          );
        })}
      </main>
      {open && api && (
        <DetailPanel
          api={api}
          ticketRef={openTicket ? ref(openTicket) : open}
          stamp={openTicket}
          onClose={() => setOpen(null)}
          say={say}
        />
      )}
      <div className={`pm-toast${toast?.error ? " pm-toast-error" : ""}`} role="status" aria-live="polite">
        {toast?.text ?? ""}
      </div>
    </div>
  );
}

function Card(props: {
  ticket: Ticket;
  workspace: Workspace;
  today: string;
  dragging: boolean;
  onDragStart: () => void;
  onDragEnd: () => void;
  onOpen: () => void;
  onMove: (target: string) => void;
}) {
  const { ticket: t, workspace } = props;
  const marks = markers(t, workspace.gate_labels, props.today);
  const gate = new Set(marks.filter((m) => m.kind === "gate").map((m) => m.text));
  return (
    <article
      className={`pm-card${props.dragging ? " pm-dragging" : ""}${t.hold ? " pm-held" : ""}`}
      data-ticket={t.id}
      data-ulid={t.ulid}
      draggable
      onDragStart={(e) => {
        e.dataTransfer.effectAllowed = "move";
        e.dataTransfer.setData("text/plain", ref(t));
        props.onDragStart();
      }}
      onDragEnd={props.onDragEnd}
    >
      <div className="pm-card-top pm-muted">
        <span className="pm-id" title={t.number === null ? `pending number — ${t.ulid}` : t.ulid}>
          {t.id}
        </span>
        <span className={`pm-prio pm-prio-${t.priority}`}>{t.priority}</span>
        {t.project && <span>{t.project}</span>}
      </div>
      <button type="button" className="pm-title" onClick={props.onOpen}>
        {t.title}
      </button>
      {(marks.length > 0 || t.labels.length > 0) && (
        <div className="pm-marks">
          {marks.map((m) => (
            <span key={`${m.kind}:${m.text}`} className={`pm-chip pm-mark-${m.kind}`} title={m.title}>
              {m.text}
              <span className="pm-sr"> ({m.title})</span>
            </span>
          ))}
          {t.labels
            .filter((l) => !gate.has(l))
            .map((l) => (
              <span key={l} className="pm-chip">
                {l}
              </span>
            ))}
        </div>
      )}
      <div className="pm-card-bottom">
        <span className="pm-muted">{t.assignee ?? "unassigned"}</span>
        <select
          className="pm-move"
          aria-label={`Move ${t.id} to…`}
          value=""
          onChange={(e) => {
            if (e.target.value) props.onMove(e.target.value);
          }}
        >
          <option value="">Move…</option>
          {orderStates(workspace.states).map((s) => {
            const plan = planMove(t, s.name, workspace.states);
            return (
              <option key={s.name} value={s.name} disabled={plan.kind !== "move"}>
                {s.name}
                {plan.kind === "refused" ? " (pm claim)" : plan.kind === "noop" ? " (here)" : ""}
              </option>
            );
          })}
        </select>
      </div>
    </article>
  );
}

function DetailPanel(props: {
  api: Api;
  ticketRef: string;
  /** The card as the board last read it: a change refetches the details. */
  stamp: Ticket | undefined;
  onClose: () => void;
  say: (text: string, isError?: boolean) => void;
}) {
  const { api, ticketRef, stamp, onClose, say } = props;
  const [detail, setDetail] = useState<Detail | null>(null);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    let live = true;
    api.get<Detail>(`/tickets/${encodeURIComponent(ticketRef)}`).then(
      (d) => {
        if (live) {
          setDetail(d);
          setError(null);
        }
      },
      (e) => live && setError(message(e)),
    );
    return () => {
      live = false;
    };
  }, [api, ticketRef, stamp]);

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") onClose();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [onClose]);

  const command = `pm edit ${ticketRef}`;
  return (
    <aside className="pm-panel" aria-label="Ticket details">
      <div className="pm-panel-head">
        <strong>{detail?.id ?? ticketRef}</strong>
        <button type="button" className="pm-link" onClick={onClose} aria-label="Close details">
          ✕
        </button>
      </div>
      {error && <p className="pm-error">pm: {error}</p>}
      {detail && (
        <>
          <h2>{detail.title}</h2>
          <p className="pm-muted">
            {detail.state} · {detail.priority}
            {detail.project ? ` · ${detail.project}` : ""} · {detail.assignee ?? "unassigned"}
          </p>
          {detail.hold && (
            <p className="pm-mark-held">
              Held by {detail.hold.by}: {detail.hold.reason}
            </p>
          )}
          {detail.blocked_by.length > 0 && <p className="pm-muted">Blocked by {detail.blocked_by.join(", ")}</p>}
          <pre className="pm-description">{detail.description || "(no description)"}</pre>
          {detail.comments.length > 0 && (
            <>
              <h3>Comments</h3>
              {detail.comments.map((c, i) => (
                <div key={i} className="pm-comment">
                  <div className="pm-muted">
                    {c.author} · {c.at}
                  </div>
                  <pre className="pm-description">{c.body}</pre>
                </div>
              ))}
            </>
          )}
        </>
      )}
      <p className="pm-muted pm-edit-note">
        To edit, run <code>{command}</code>{" "}
        <button
          type="button"
          className="pm-link"
          onClick={() =>
            navigator.clipboard.writeText(command).then(
              () => say(`Copied: ${command}`),
              () => say(`Copy failed — run: ${command}`, true),
            )
          }
        >
          copy
        </button>
      </p>
    </aside>
  );
}

const boardCss = `
.pm-board { padding: 1rem; }
.pm-bar { display: flex; flex-wrap: wrap; align-items: center; gap: .5rem 1.25rem; margin-bottom: .75rem; }
.pm-bar h1 { font-size: 1.1rem; margin: 0; }
.pm-filters { display: flex; flex-wrap: wrap; gap: .5rem; align-items: center; }
.pm-filter { display: inline-flex; gap: .3rem; align-items: center; font-size: 12px; color: var(--muted); }
.pm-filter select, .pm-move { font: inherit; font-size: 12px; color: var(--fg); background: var(--card);
  border: 1px solid var(--line); border-radius: 4px; padding: 1px 4px; }
.pm-link { font: inherit; font-size: 12px; background: none; border: 0; color: var(--muted);
  text-decoration: underline; cursor: pointer; padding: 0; }
.pm-error-inline { color: #c0392b; font-size: 12px; }
.pm-columns { display: flex; gap: .75rem; align-items: flex-start; overflow-x: auto; padding-bottom: 1rem; }
.pm-column { min-width: 15rem; flex: 0 0 15rem; min-height: 6rem; border-radius: 8px; padding: .35rem;
  border: 2px dashed transparent; }
.pm-column h2 { font-size: .85rem; margin: 0 0 .5rem; padding: 0 .2rem; }
.pm-hint { font-weight: normal; font-size: 11px; }
.pm-accept { border-color: #3b82f6; background: color-mix(in srgb, #3b82f6 8%, transparent); }
.pm-refuse { border-color: #c0392b; background: color-mix(in srgb, #c0392b 8%, transparent); cursor: not-allowed; }
.pm-card { background: var(--card); border: 1px solid var(--line); border-radius: 6px;
  padding: .45rem .55rem; margin-bottom: .5rem; cursor: grab; }
.pm-card.pm-dragging { opacity: .45; }
.pm-card.pm-held { border-left: 3px solid #d97706; }
.pm-card-top, .pm-card-bottom { display: flex; gap: .4rem; align-items: center; font-size: 12px; }
.pm-card-bottom { justify-content: space-between; margin-top: .3rem; }
.pm-id { font-variant-numeric: tabular-nums; }
.pm-prio-high { color: #d97706; }
.pm-prio-critical { color: #c0392b; font-weight: 600; }
.pm-title { display: block; width: 100%; text-align: left; font: inherit; color: inherit; background: none;
  border: 0; padding: .15rem 0; cursor: pointer; }
.pm-title:hover { text-decoration: underline; }
.pm-marks { margin-top: .2rem; }
.pm-mark-held { color: #d97706; border-color: #d97706; }
.pm-mark-parked { color: #6b7280; border-style: dashed; }
.pm-mark-gate { color: #7c3aed; border-color: #7c3aed; }
.pm-sr { position: absolute; width: 1px; height: 1px; overflow: hidden; clip: rect(0 0 0 0); white-space: nowrap; }
:focus-visible { outline: 2px solid #3b82f6; outline-offset: 1px; }
.pm-panel { position: fixed; top: 0; right: 0; bottom: 0; width: min(28rem, 100vw); overflow-y: auto;
  background: var(--card); border-left: 1px solid var(--line); padding: 1rem; box-shadow: -4px 0 16px rgba(0,0,0,.12); }
.pm-panel-head { display: flex; justify-content: space-between; align-items: center; }
.pm-panel h2 { font-size: 1rem; margin: .5rem 0 .25rem; }
.pm-panel h3 { font-size: .85rem; margin: 1rem 0 .25rem; }
.pm-description { white-space: pre-wrap; font: inherit; margin: .5rem 0; }
.pm-comment { border-top: 1px solid var(--line); padding-top: .4rem; }
.pm-edit-note code { font-size: 12px; }
.pm-toast { position: fixed; left: 50%; bottom: 1rem; transform: translateX(-50%); max-width: 90vw;
  background: var(--fg); color: var(--bg); border-radius: 6px; padding: .45rem .8rem; font-size: 13px; }
.pm-toast:empty { display: none; }
.pm-toast-error { background: #c0392b; color: #fff; }
`;
