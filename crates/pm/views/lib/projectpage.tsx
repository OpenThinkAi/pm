// The project page as a component (AGT-1490): `ProjectPage({api, id})` is what
// project.tsx renders for `pm project edit <ID>`, and what another view (the
// initiatives entry view) renders inline, since ui-leaf cannot open a second
// view. See project.tsx for the behaviour; the policy is in lib/project.ts.

import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { baseCss, type Api, type OpEvent } from "./pm";
import { BodySync, bodyTransport, fromBase64, type BodySnapshot, type SyncStatus } from "./body";
import { BodyEditor, SaveIndicator, TicketEditor, editorCss, message } from "./editor";
import { debounce, type Ticket, type WorkflowState } from "./board";
import {
  DESIGN_DOC,
  createdRef,
  docBodyPath,
  docTabs,
  newTicketRequest,
  opEffect,
  pickTab,
  ref,
  ticketSections,
  type DocTab,
  type Project,
} from "./project";

const css = `
.pm-proj { display: grid; grid-template-columns: minmax(0, 3fr) minmax(20rem, 2fr); gap: 1.25rem;
  padding: 1rem 1.25rem; box-sizing: border-box; min-height: 100vh; }
.pm-proj h1 { font-size: 1.3rem; margin: 0 0 .15rem; }
.pm-proj .pm-tabs { display: flex; flex-wrap: wrap; gap: .25rem; border-bottom: 1px solid var(--line);
  margin: .75rem 0 .5rem; }
.pm-proj .pm-tab { border: 1px solid transparent; border-bottom: 0; background: none; color: var(--muted);
  font: inherit; padding: .3rem .7rem; border-radius: 6px 6px 0 0; cursor: pointer; }
.pm-proj .pm-tab[aria-selected="true"] { color: var(--fg); border-color: var(--line); background: var(--card);
  margin-bottom: -1px; }
.pm-proj .pm-doc .pm-editor { min-height: 60vh; }
.pm-proj .pm-doc .pm-editor .cm-editor { min-height: 60vh; }
.pm-proj aside { border-left: 1px solid var(--line); padding-left: 1.25rem; min-width: 0; }
.pm-proj aside h2 { font-size: .95rem; margin: .25rem 0 .5rem; }
.pm-proj .pm-new { display: flex; gap: .4rem; margin-bottom: .75rem; }
.pm-proj .pm-new input { flex: 1; min-width: 0; font: inherit; color: var(--fg); background: var(--card);
  border: 1px solid var(--line); border-radius: 6px; padding: .3rem .45rem; }
.pm-proj button.pm-btn { font: inherit; color: var(--fg); background: var(--card); border: 1px solid var(--line);
  border-radius: 6px; padding: .3rem .6rem; cursor: pointer; }
.pm-proj .pm-section { font-size: 12px; color: var(--muted); margin: .75rem 0 .25rem; text-transform: uppercase;
  letter-spacing: .04em; }
.pm-proj .pm-row { display: block; width: 100%; text-align: left; font: inherit; color: var(--fg);
  background: none; border: 0; border-bottom: 1px solid var(--line); padding: .35rem .2rem; cursor: pointer; }
.pm-proj .pm-row:hover, .pm-proj .pm-row:focus { background: var(--card); }
.pm-proj .pm-row .pm-muted { font-size: 12px; margin-right: .4rem; }
.pm-proj aside .pm-ed { padding: 0; max-width: none; }
`;

interface WorkspaceInfo {
  states: WorkflowState[];
}

export function ProjectPage({ api, id }: { api: Api; id: string }) {
  const [project, setProject] = useState<Project | null>(null);
  const [tickets, setTickets] = useState<Ticket[]>([]);
  const [states, setStates] = useState<WorkflowState[]>([]);
  const [error, setError] = useState<string | null>(null);
  const [tabKey, setTabKey] = useState(DESIGN_DOC.key);
  const [open, setOpen] = useState<string | null>(null);

  // ---------------------------------------------------------------- reads

  const refreshProject = useCallback(() => {
    api.get<Project>(`/projects/${encodeURIComponent(id)}`).then(
      (p) => {
        setProject(p);
        setError(null);
      },
      (e) => setError(message(e)),
    );
  }, [api, id]);

  // Only the newest list fetch may land (a slow one must not overwrite a
  // newer answer).
  const listSeq = useRef(0);
  const refreshTickets = useCallback(() => {
    const seq = ++listSeq.current;
    api.get<Ticket[]>(`/tickets?project=${encodeURIComponent(id)}`).then(
      (list) => {
        if (seq === listSeq.current) setTickets(list);
      },
      (e) => setError(message(e)),
    );
  }, [api, id]);

  const refreshStates = useCallback(() => {
    api.get<WorkspaceInfo>("/workspace").then(
      (w) => setStates(w.states),
      (e) => setError(message(e)),
    );
  }, [api]);

  useEffect(() => {
    refreshProject();
    refreshTickets();
    refreshStates();
  }, [refreshProject, refreshTickets, refreshStates]);

  const tabs = useMemo(() => (project ? docTabs(project) : [DESIGN_DOC]), [project]);
  const tab = pickTab(tabs, tabKey);

  // ------------------------------------------------------- the document

  const [sync, setSync] = useState<BodySync | null>(null);
  const [docId, setDocId] = useState<string | null>(null);
  const [save, setSave] = useState<SyncStatus>({ kind: "saved" });
  const [docError, setDocError] = useState<string | null>(null);

  useEffect(() => {
    let cancelled = false;
    let created: BodySync | null = null;
    const path = docBodyPath(id, tab);
    setSync(null);
    setDocId(null);
    setDocError(null);
    setSave({ kind: "saved" });
    api.get<BodySnapshot>(path).then(
      (body) => {
        if (cancelled) return;
        created = new BodySync(fromBase64(body.snapshot), bodyTransport(api, path), {
          onStatus: setSave,
        });
        setDocId(body.doc_id ?? null);
        setSync(created);
      },
      (e) => setDocError(message(e)),
    );
    return () => {
      cancelled = true;
      if (created) {
        // Switching tabs (or closing): send what is unsent, then let go.
        void created.flush({ keepalive: true });
        created.dispose();
      }
    };
    // `tab.key` names the document.
  }, [api, id, tab.key]);

  // A sync that arrives after the stream's `hello` still needs one pull.
  useEffect(() => {
    sync?.pull().catch((e) => setDocError(message(e)));
  }, [sync]);

  // ----------------------------------------------------------- live ops

  const projectLater = useMemo(() => debounce(refreshProject, 150), [refreshProject]);
  const ticketsLater = useMemo(() => debounce(refreshTickets, 150), [refreshTickets]);
  const latest = useRef({ sync, docId, projectLater, ticketsLater, refreshStates });
  latest.current = { sync, docId, projectLater, ticketsLater, refreshStates };
  useEffect(() => {
    const abort = new AbortController();
    const pull = () => latest.current.sync?.pull().catch((e) => setDocError(message(e)));
    void api.events(
      (op: OpEvent) => {
        const effect = opEffect(op, latest.current.docId);
        if (effect.pullDoc) pull();
        if (effect.project) latest.current.projectLater.call();
        if (effect.tickets) latest.current.ticketsLater.call();
        if (op.kind.startsWith("workspace")) latest.current.refreshStates();
      },
      () => {
        latest.current.projectLater.call();
        latest.current.ticketsLater.call();
        latest.current.refreshStates();
        pull();
      },
      abort.signal,
    );
    return () => {
      abort.abort();
      latest.current.projectLater.cancel();
      latest.current.ticketsLater.cancel();
    };
  }, [api]);

  // ---------------------------------------------------------- new ticket

  const [newTitle, setNewTitle] = useState("");
  const [filing, setFiling] = useState(false);
  const fileTicket = () => {
    const request = newTicketRequest(newTitle, id);
    if (!request || filing) return;
    setFiling(true);
    api.post<Ticket>("/tickets", request).then(
      (created) => {
        setFiling(false);
        setNewTitle("");
        setError(null);
        refreshTickets();
        setOpen(createdRef(created));
      },
      (e) => {
        setFiling(false);
        setError(`New ticket: ${message(e)}`);
      },
    );
  };

  // --------------------------------------------------------------- render

  const sections = ticketSections(tickets, states);

  return (
    <div className="pm-proj">
      <style>{baseCss + editorCss + css}</style>
      <main className="pm-ed pm-doc" style={{ maxWidth: "none", margin: 0, padding: 0 }}>
        <div className="pm-muted" style={{ fontSize: 12 }}>
          project {id}
          {project ? ` · ${project.status}` : ""}
          {project?.parent ? ` · sub-project of ${project.parent}` : ""}
          {project && project.repos.length > 0 ? ` · ${project.repos.join(", ")}` : ""}
        </div>
        <h1>{project ? project.title : id}</h1>
        {error && <p className="pm-error" style={{ padding: 0 }}>pm: {error}</p>}
        <div className="pm-tabs" role="tablist" aria-label="Documents">
          {tabs.map((t) => (
            <button
              key={t.key}
              className="pm-tab"
              role="tab"
              aria-selected={t.key === tab.key}
              onClick={() => setTabKey(t.key)}
            >
              {t.label}
            </button>
          ))}
        </div>
        <div style={{ marginBottom: ".35rem", minHeight: "1.2em" }}>
          <span className="pm-muted" style={{ fontSize: 12 }}>
            {tab.name === null ? "Design doc" : tab.name}
          </span>
          <SaveIndicator status={save} />
        </div>
        {docError ? (
          <p className="pm-error" style={{ padding: 0 }}>pm: {docError}</p>
        ) : (
          <BodyEditor key={tab.key} sync={sync} placeholderText="Empty document." />
        )}
        <p className="pm-muted" style={{ fontSize: 12, marginTop: "1rem" }}>
          Every change is saved as you make it. New named documents: <code>pm project doc add {id} &lt;name&gt;</code>.
          Close this window to return to the terminal.
        </p>
      </main>
      <aside>
        {open !== null ? (
          <TicketEditor
            key={open}
            api={api}
            id={open}
            toolbar={
              <button className="pm-btn" style={{ marginBottom: ".5rem" }} onClick={() => setOpen(null)}>
                ← Back to tickets
              </button>
            }
          />
        ) : (
          <>
            <h2>Tickets</h2>
            <form
              className="pm-new"
              onSubmit={(e) => {
                e.preventDefault();
                fileTicket();
              }}
            >
              <input
                aria-label="New ticket title"
                placeholder="New ticket title…"
                value={newTitle}
                onChange={(e) => setNewTitle(e.target.value)}
              />
              <button className="pm-btn" type="submit" disabled={filing || newTitle.trim() === ""}>
                New ticket
              </button>
            </form>
            {sections.length === 0 && <p className="pm-muted">No tickets in this project.</p>}
            {sections.map((section) => (
              <section key={section.state.name}>
                <div className="pm-section">
                  {section.state.name} · {section.tickets.length}
                </div>
                {section.tickets.map((t) => (
                  <button key={t.ulid} className="pm-row" title={t.ulid} onClick={() => setOpen(ref(t))}>
                    <span className="pm-muted">{t.id}</span>
                    {t.title}
                    {t.assignee ? <span className="pm-muted"> · {t.assignee}</span> : null}
                  </button>
                ))}
              </section>
            ))}
          </>
        )}
      </aside>
    </div>
  );
}
