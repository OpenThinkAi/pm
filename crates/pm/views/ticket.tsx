// `pm edit <ID>`'s view (AGT-1403): the ticket editor. Title, priority,
// project, labels and state are edited through the API's field, label and
// state writes; the description is CodeMirror bound (loro-codemirror) to a
// loro-crdt document imported from the body's snapshot, its edits POSTed
// as Loro updates (lib/body.ts). Every change is an op in the log.
//
// Live: every op on this ticket — this window's own writes, another
// window, a `pm set`/`pm edit` in a terminal, a `pm sync` pull — refetches
// the ticket, and a `body.edit` also pulls the description ops this
// document lacks, which loro-codemirror applies to the open editor. Claims
// stay CLI-only (`pm claim`).

import { useCallback, useEffect, useRef, useState } from "react";
import { baseCss, useApi, type OpEvent, type ViewProps } from "./lib/pm";
import { BodySync, apiTransport, fromBase64, TEXT, type BodySnapshot, type SyncStatus } from "./lib/body";
import { UndoManager } from "./vendor/loro.js";
import {
  EditorState,
  EditorView,
  LoroExtensions,
  defaultKeymap,
  drawSelection,
  indentWithTab,
  keymap,
  placeholder,
} from "./vendor/codemirror.js";

interface Comment {
  author: string;
  at: string;
  body: string;
}

interface Ticket {
  id: string;
  ulid: string;
  title: string;
  state: string;
  priority: string;
  project: string | null;
  assignee: string | null;
  labels: string[];
  blocked_by: string[];
  description: string;
  comments: Comment[];
}

interface WorkspaceInfo {
  states: { name: string; category: string; position: number }[];
}

interface Project {
  id: string;
  title: string;
  status: string;
}

const PRIORITIES = ["low", "medium", "high", "critical"];

const css = `
.pm-ed { max-width: 50rem; margin: 0 auto; padding: 1.25rem; }
.pm-ed input, .pm-ed select { font: inherit; color: var(--fg); background: var(--card);
  border: 1px solid var(--line); border-radius: 6px; padding: .3rem .45rem; }
.pm-ed .pm-title { width: 100%; box-sizing: border-box; font-size: 1.3rem; font-weight: 600;
  border-color: transparent; background: transparent; padding: .2rem .3rem; margin: .25rem 0 .5rem -.3rem; }
.pm-ed .pm-title:hover, .pm-ed .pm-title:focus { border-color: var(--line); background: var(--card); }
.pm-ed .pm-fields { display: grid; grid-template-columns: max-content 1fr; gap: .4rem .75rem;
  align-items: center; margin-bottom: 1rem; }
.pm-ed .pm-fields label { color: var(--muted); font-size: 12px; }
.pm-ed .pm-chip button { border: 0; background: none; color: var(--muted); cursor: pointer;
  padding: 0 0 0 .25rem; font: inherit; }
.pm-ed .pm-label-add { width: 9rem; font-size: 12px; padding: .1rem .4rem; }
.pm-ed .pm-editor { border: 1px solid var(--line); border-radius: 6px; background: var(--card);
  min-height: 16rem; }
.pm-ed .pm-editor .cm-editor { min-height: 16rem; }
.pm-ed .pm-editor .cm-editor.cm-focused { outline: none; }
.pm-ed .pm-editor .cm-content { font: 13px/1.5 ui-monospace, SFMono-Regular, Menlo, monospace;
  padding: .6rem 0; caret-color: var(--fg); }
.pm-ed .pm-editor .cm-line { padding: 0 .75rem; }
.pm-ed .pm-editor .cm-cursor { border-left-color: var(--fg); }
.pm-ed .pm-save { font-size: 12px; float: right; }
.pm-ed .pm-save.error { color: #c0392b; }
`;

/** A write's failure, surfaced until the next success. */
function message(e: unknown): string {
  return e instanceof Error ? e.message : String(e);
}

export default function TicketView({ data, mutate }: ViewProps) {
  const { api, error: connectError } = useApi(mutate);
  const id = data.ticket ?? "";
  const [ticket, setTicket] = useState<Ticket | null>(null);
  const [states, setStates] = useState<WorkspaceInfo["states"]>([]);
  const [projects, setProjects] = useState<Project[]>([]);
  const [error, setError] = useState<string | null>(null);
  const [title, setTitle] = useState("");
  const [titleFocused, setTitleFocused] = useState(false);
  const [newLabel, setNewLabel] = useState("");
  const [save, setSave] = useState<SyncStatus>({ kind: "saved" });
  const [sync, setSync] = useState<BodySync | null>(null);
  const editorHost = useRef<HTMLDivElement | null>(null);

  // ---------------------------------------------------------------- reads

  const refresh = useCallback(() => {
    if (!api) return;
    api.get<Ticket>(`/tickets/${encodeURIComponent(id)}`).then(
      (t) => {
        setTicket(t);
        setError(null);
      },
      (e) => setError(message(e)),
    );
  }, [api, id]);

  useEffect(refresh, [refresh]);

  useEffect(() => {
    if (!api) return;
    api.get<WorkspaceInfo>("/workspace").then(
      (w) => setStates([...w.states].sort((a, b) => a.position - b.position)),
      (e) => setError(message(e)),
    );
    api.get<{ projects: Project[] }>("/projects").then(
      (p) => setProjects(p.projects),
      (e) => setError(message(e)),
    );
  }, [api]);

  // The title input follows the ticket unless it is being edited.
  useEffect(() => {
    if (ticket && !titleFocused) setTitle(ticket.title);
  }, [ticket, titleFocused]);

  // ----------------------------------------------------- the description

  useEffect(() => {
    if (!api) return;
    let cancelled = false;
    let created: BodySync | null = null;
    api.get<BodySnapshot>(`/tickets/${encodeURIComponent(id)}/body`).then(
      (body) => {
        if (cancelled) return;
        created = new BodySync(fromBase64(body.snapshot), apiTransport(api, id), {
          onStatus: setSave,
        });
        setSync(created);
      },
      (e) => setError(message(e)),
    );
    return () => {
      cancelled = true;
      created?.dispose();
    };
  }, [api, id]);

  useEffect(() => {
    const host = editorHost.current;
    if (!sync || !host) return;
    const undo = new UndoManager(sync.doc, {});
    const view = new EditorView({
      parent: host,
      state: EditorState.create({
        doc: sync.text(),
        extensions: [
          keymap.of([...defaultKeymap, indentWithTab]),
          drawSelection(),
          EditorView.lineWrapping,
          placeholder("No description."),
          LoroExtensions(sync.doc, undefined, undo, (doc) => doc.getText(TEXT)),
        ],
      }),
    });
    // Closing the window: send what is still unsent, outliving the page.
    const leave = () => void sync.flush({ keepalive: true });
    const hidden = () => {
      if (document.visibilityState === "hidden") leave();
    };
    window.addEventListener("pagehide", leave);
    document.addEventListener("visibilitychange", hidden);
    return () => {
      window.removeEventListener("pagehide", leave);
      document.removeEventListener("visibilitychange", hidden);
      view.destroy();
    };
  }, [sync]);

  // ----------------------------------------------------------- live ops

  // One /events stream for as long as the view shows (it is also what
  // keeps `pm app` alive). An op on this ticket refetches it; a
  // `body.edit` also pulls the description; a (re)connect does both.
  const latest = useRef({ refresh, sync, ulid: ticket?.ulid });
  latest.current = { refresh, sync, ulid: ticket?.ulid };
  useEffect(() => {
    if (!api) return;
    const abort = new AbortController();
    const pull = () => {
      latest.current.sync?.pull().catch((e) => setError(`description: ${message(e)}`));
    };
    void api.events(
      (op: OpEvent) => {
        const { ulid } = latest.current;
        if (op.id !== id && op.entity !== ulid) return;
        latest.current.refresh();
        if (op.kind === "body.edit") pull();
      },
      () => {
        latest.current.refresh();
        pull();
      },
      abort.signal,
    );
    return () => abort.abort();
  }, [api, id]);

  // A sync that arrives after the stream's `hello` still needs one pull.
  useEffect(() => {
    sync?.pull().catch((e) => setError(`description: ${message(e)}`));
  }, [sync]);

  // --------------------------------------------------------------- writes

  const write = useCallback(
    (path: string, body: unknown) => {
      if (!api) return;
      api.post<Ticket>(`/tickets/${encodeURIComponent(id)}/${path}`, body).then(
        (t) => {
          setTicket(t);
          setError(null);
        },
        (e) => {
          setError(message(e));
          refresh();
        },
      );
    },
    [api, id, refresh],
  );

  const commitTitle = () => {
    const next = title.trim();
    if (ticket && next && next !== ticket.title) write("fields", { title: next });
    else if (ticket) setTitle(ticket.title);
  };

  const addLabels = () => {
    const add = newLabel
      .split(",")
      .map((l) => l.trim())
      .filter((l) => l && !ticket?.labels.includes(l));
    setNewLabel("");
    if (add.length > 0) write("labels", { add });
  };

  // --------------------------------------------------------------- render

  if (connectError) return <p className="pm-error">pm: {connectError}</p>;
  if (!ticket) {
    return error ? (
      <p className="pm-error">pm: {error}</p>
    ) : (
      <p className="pm-muted" style={{ padding: "1rem" }}>
        Loading {id}…
      </p>
    );
  }

  const stateNames = states.map((s) => s.name);
  const projectIds = projects.filter((p) => p.status === "in-progress" || p.id === ticket.project);

  return (
    <article className="pm-ed">
      <style>{baseCss + css}</style>
      <div className="pm-muted" style={{ fontSize: 12 }}>
        {ticket.id}
        {ticket.assignee ? ` · ${ticket.assignee}` : ""}
      </div>
      <input
        className="pm-title"
        aria-label="Title"
        value={title}
        onChange={(e) => setTitle(e.target.value)}
        onFocus={() => setTitleFocused(true)}
        onBlur={() => {
          setTitleFocused(false);
          commitTitle();
        }}
        onKeyDown={(e) => {
          if (e.key === "Enter") (e.target as HTMLInputElement).blur();
          if (e.key === "Escape") {
            setTitle(ticket.title);
            setTitleFocused(false);
          }
        }}
      />
      {error && <p className="pm-error" style={{ padding: 0 }}>pm: {error}</p>}
      <div className="pm-fields">
        <label htmlFor="pm-state">State</label>
        <div>
          <select
            id="pm-state"
            value={ticket.state}
            onChange={(e) => write("state", { state: e.target.value })}
          >
            {(stateNames.includes(ticket.state) ? stateNames : [ticket.state, ...stateNames]).map(
              (s) => (
                <option key={s} value={s}>
                  {s}
                </option>
              ),
            )}
          </select>
        </div>
        <label htmlFor="pm-priority">Priority</label>
        <div>
          <select
            id="pm-priority"
            value={ticket.priority}
            onChange={(e) => write("fields", { priority: e.target.value })}
          >
            {PRIORITIES.map((p) => (
              <option key={p} value={p}>
                {p}
              </option>
            ))}
          </select>
        </div>
        <label htmlFor="pm-project">Project</label>
        <div>
          <select
            id="pm-project"
            value={ticket.project ?? ""}
            onChange={(e) => write("fields", { project: e.target.value || null })}
          >
            <option value="">(none)</option>
            {ticket.project && !projectIds.some((p) => p.id === ticket.project) && (
              <option value={ticket.project}>{ticket.project}</option>
            )}
            {projectIds.map((p) => (
              <option key={p.id} value={p.id}>
                {p.id}
              </option>
            ))}
          </select>
        </div>
        <label>Labels</label>
        <div>
          {ticket.labels.map((l) => (
            <span key={l} className="pm-chip">
              {l}
              <button title={`Remove ${l}`} onClick={() => write("labels", { remove: [l] })}>
                ×
              </button>
            </span>
          ))}
          <input
            className="pm-label-add"
            placeholder="add label…"
            aria-label="Add label"
            value={newLabel}
            onChange={(e) => setNewLabel(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === "Enter") addLabels();
            }}
            onBlur={addLabels}
          />
        </div>
        {ticket.blocked_by.length > 0 && (
          <>
            <label>Blocked by</label>
            <div className="pm-muted">{ticket.blocked_by.join(", ")}</div>
          </>
        )}
      </div>
      <div style={{ marginBottom: ".35rem" }}>
        <span className="pm-muted" style={{ fontSize: 12 }}>
          Description
        </span>
        <span className={`pm-save pm-muted${save.kind === "error" ? " error" : ""}`}>
          {save.kind === "saved"
            ? "Saved"
            : save.kind === "error"
              ? `Not saved: ${save.message} (retrying)`
              : "Saving…"}
        </span>
      </div>
      {!sync && (
        <p className="pm-muted" style={{ fontSize: 12 }}>
          Loading description…
        </p>
      )}
      {/* CodeMirror owns this element's children; React renders none. */}
      <div className="pm-editor" ref={editorHost} />
      {ticket.comments.length > 0 && (
        <section style={{ marginTop: "1.25rem" }}>
          <h2 style={{ fontSize: ".95rem" }}>Comments</h2>
          {ticket.comments.map((c, i) => (
            <div key={i} style={{ borderTop: "1px solid var(--line)", padding: ".5rem 0" }}>
              <div className="pm-muted" style={{ fontSize: 12 }}>
                {c.author} · {c.at}
              </div>
              <div style={{ whiteSpace: "pre-wrap" }}>{c.body}</div>
            </div>
          ))}
        </section>
      )}
      <p className="pm-muted" style={{ fontSize: 12, marginTop: "1.5rem" }}>
        Every change is saved as you make it. Close this window to return to the terminal.
      </p>
    </article>
  );
}
