// `pm app`'s view (AGT-1492; docs/app-api.md §The initiatives view): the
// entry point is initiatives → projects → a project. The landing page is a
// compact rollup row per initiative, then Unfiled (projects under no
// initiative): its project count, its tickets not started / started / done,
// and a progress bar (done ÷ non-canceled). Clicking an initiative shows
// its projects as rollup rows above its own documents (`ProjectPage`);
// clicking a project opens `ProjectPage` for it. ui-leaf cannot open a
// second view, so everything is inline here, with a breadcrumb
// (Initiatives › <initiative> › <project>) back. The board stays, as the
// "Board" tab (`BoardPage`).
//
// All of it reads `GET /initiatives`, refetched — debounced, newest fetch
// wins, as the board does — whenever an op on `/events` can change it. The
// policy (rows, progress, breadcrumb, which ops refetch) lives in
// lib/initiatives.ts, which `node --test crates/pm/views-test/*.test.ts`
// covers.

import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { baseCss, useApi, useEvents, type Api, type ViewProps } from "./lib/pm";
import { BoardPage } from "./lib/boardpage";
import { ProjectPage } from "./lib/projectpage";
import { debounce } from "./lib/board";
import {
  UNFILED,
  group,
  landingRows,
  percent,
  projectRows,
  trail,
  wantsOp,
  type Initiatives as Tree,
  type Row,
} from "./lib/initiatives";

export default function InitiativesView({ mutate }: ViewProps) {
  const { api, error: connectError } = useApi(mutate);
  if (connectError) return <p className="pm-error">pm: {connectError}</p>;
  if (!api) return <p className="pm-muted" style={{ padding: "1rem" }}>Loading…</p>;
  return <InitiativesPage api={api} />;
}

function message(e: unknown): string {
  return e instanceof Error ? e.message : String(e);
}

function InitiativesPage({ api }: { api: Api }) {
  const [tab, setTab] = useState<"initiatives" | "board">("initiatives");
  // null: the landing page; else an initiative's id, UNFILED, or a project's id.
  const [at, setAt] = useState<string | null>(null);
  const [data, setData] = useState<Tree | null>(null);
  const [error, setError] = useState<string | null>(null);
  const fetchSeq = useRef(0);

  const refresh = useCallback(() => {
    // Only the newest fetch may land: a slow one must not overwrite a
    // fresher answer.
    const seq = ++fetchSeq.current;
    api.get<Tree>("/initiatives").then(
      (d) => {
        if (seq !== fetchSeq.current) return;
        setData(d);
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
  useEvents(api, debounced.call, wantsOp);

  const tabButton = (key: typeof tab, label: string) => (
    <button type="button" role="tab" className="pm-itab" aria-selected={tab === key} onClick={() => setTab(key)}>
      {label}
    </button>
  );

  return (
    <div className="pm-init">
      <style>{baseCss + css}</style>
      <nav className="pm-inav" role="tablist" aria-label="pm">
        {tabButton("initiatives", "Initiatives")}
        {tabButton("board", "Board")}
        {error && <span className="pm-ierr">pm: {error}</span>}
      </nav>
      {tab === "board" ? (
        <div className="pm-init-board">
          <BoardPage api={api} />
        </div>
      ) : (
        <Body api={api} data={data} at={at} go={setAt} />
      )}
    </div>
  );
}

function Body({ api, data, at, go }: { api: Api; data: Tree | null; at: string | null; go: (key: string | null) => void }) {
  if (!data) return <p className="pm-muted pm-ipad">Loading…</p>;

  if (at === null) {
    return (
      <main className="pm-ipad">
        <h1 className="pm-ih1">Initiatives</h1>
        {data.initiatives.length === 0 && (
          <p className="pm-muted">
            No initiatives yet: <code>pm project new &lt;id&gt; --kind initiative</code>, then{" "}
            <code>pm project set &lt;project&gt; parent=&lt;id&gt;</code>.
          </p>
        )}
        <Rows rows={landingRows(data)} go={go} label="Initiatives" />
      </main>
    );
  }

  const crumbs = trail(data, at);
  const g = group(data, at);
  return (
    <>
      <nav className="pm-crumbs pm-ipad" aria-label="Breadcrumb">
        <ol>
          <li>
            <button type="button" className="pm-crumb" onClick={() => go(null)}>
              Initiatives
            </button>
          </li>
          {(crumbs.length > 0 ? crumbs : [{ key: at, title: at }]).map((c, i, all) => (
            <li key={c.key}>
              {i === all.length - 1 ? (
                <span aria-current="page">{c.title}</span>
              ) : (
                <button type="button" className="pm-crumb" onClick={() => go(c.key)}>
                  {c.title}
                </button>
              )}
            </li>
          ))}
        </ol>
      </nav>
      {g ? (
        <>
          <section className="pm-ipad pm-igroup">
            <h2 className="pm-ih2">
              Projects <span className="pm-muted">{projectRows(g.projects).length}</span>
            </h2>
            {g.projects.length === 0 ? (
              <p className="pm-muted">
                No projects{g.node ? ` in ${g.title}: pm project set <project> parent=${g.key}` : ""}.
              </p>
            ) : (
              <Rows rows={projectRows(g.projects)} go={go} label={`${g.title} projects`} />
            )}
          </section>
          {/* An initiative is a project: its documents (and own tickets) below. */}
          {g.node && <ProjectPage key={g.key} api={api} id={g.key} />}
        </>
      ) : (
        <ProjectPage key={at} api={api} id={at} />
      )}
    </>
  );
}

function Rows({ rows, go, label }: { rows: Row[]; go: (key: string) => void; label: string }) {
  return (
    <ul className="pm-irows" aria-label={label}>
      {rows.map((r) => (
        <li key={r.key} className="pm-irow" data-key={r.key} onClick={() => go(r.key)}>
          <button
            type="button"
            className="pm-ititle"
            style={{ paddingLeft: `${r.depth * 1.1}em` }}
            onClick={(e) => {
              e.stopPropagation();
              go(r.key);
            }}
          >
            {r.depth > 0 && <span className="pm-muted" aria-hidden="true">↳ </span>}
            {r.title}
          </button>
          {r.status && r.status !== "in-progress" && <span className="pm-chip pm-muted">{r.status}</span>}
          {r.projects !== null && (
            <span className="pm-muted pm-inum">
              {r.projects} project{r.projects === 1 ? "" : "s"}
            </span>
          )}
          <span className="pm-muted pm-inum" title="Tickets not started (backlog + unstarted) · started · completed">
            {r.unstarted} to do · {r.started} started · {r.completed} done
          </span>
          <Progress row={r} />
        </li>
      ))}
    </ul>
  );
}

/** A bar plus its percentage as text, so progress is never colour alone. */
function Progress({ row }: { row: Row }) {
  const pct = percent(row.progress);
  const live = row.unstarted + row.started + row.completed;
  return (
    <span className="pm-iprog">
      <span
        className="pm-ibar"
        role="progressbar"
        aria-label={`${row.title} progress`}
        aria-valuemin={0}
        aria-valuemax={100}
        aria-valuenow={row.progress === null ? undefined : Math.floor(row.progress * 100)}
        aria-valuetext={row.progress === null ? "no tickets" : `${row.completed} of ${live} done`}
      >
        <span className="pm-ifill" style={{ width: row.progress === null ? 0 : `${row.progress * 100}%` }} />
      </span>
      <span className="pm-ipct">{pct}</span>
    </span>
  );
}

const css = `
.pm-init { min-height: 100vh; }
.pm-inav { display: flex; align-items: center; gap: .25rem; height: 2.25rem; box-sizing: border-box;
  padding: 0 1rem; border-bottom: 1px solid var(--line); }
.pm-itab { font: inherit; color: var(--muted); background: none; border: 0; border-bottom: 2px solid transparent;
  padding: .45rem .6rem .35rem; cursor: pointer; }
.pm-itab[aria-selected="true"] { color: var(--fg); border-bottom-color: var(--fg); font-weight: 600; }
.pm-ierr { color: #c0392b; font-size: 12px; margin-left: auto; }
.pm-init-board .pm-board { height: calc(100vh - 2.25rem); }
.pm-ipad { padding: .75rem 1.25rem 0; }
.pm-ih1 { font-size: 1.1rem; margin: 0 0 .5rem; }
.pm-ih2 { font-size: .95rem; margin: 0 0 .4rem; }
.pm-igroup { padding-bottom: .5rem; }
.pm-irows { list-style: none; margin: 0; padding: 0; max-width: 60rem; }
.pm-irow { display: flex; align-items: center; gap: .75rem; height: 2rem; padding: 0 .5rem; font-size: 13px;
  background: var(--card); border: 1px solid var(--line); border-radius: 4px; margin-bottom: 3px;
  cursor: pointer; white-space: nowrap; }
.pm-irow:hover { border-color: color-mix(in srgb, var(--fg) 30%, var(--line)); }
.pm-ititle { flex: 1; min-width: 0; overflow: hidden; text-overflow: ellipsis; text-align: left; font: inherit;
  font-weight: 600; color: inherit; background: none; border: 0; padding: 0; cursor: pointer; }
.pm-ititle:hover { text-decoration: underline; }
.pm-irow .pm-chip { flex: none; font-size: 11px; }
.pm-inum { flex: none; font-size: 12px; font-variant-numeric: tabular-nums; }
.pm-iprog { flex: none; display: inline-flex; align-items: center; gap: .4rem; }
.pm-ibar { display: inline-block; width: 6rem; height: .55rem; border: 1px solid var(--muted);
  border-radius: 3px; overflow: hidden; box-sizing: border-box; }
.pm-ifill { display: block; height: 100%; background: var(--fg); }
.pm-ipct { font-size: 12px; font-variant-numeric: tabular-nums; min-width: 2.6em; text-align: right; }
.pm-crumbs ol { display: flex; flex-wrap: wrap; gap: .35rem; list-style: none; margin: 0; padding: 0; font-size: 13px; }
.pm-crumbs li + li::before { content: "›"; color: var(--muted); margin-right: .35rem; }
.pm-crumb { font: inherit; color: var(--muted); background: none; border: 0; padding: 0; cursor: pointer;
  text-decoration: underline; }
:focus-visible { outline: 2px solid #3b82f6; outline-offset: 1px; }
`;
