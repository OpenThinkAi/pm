// `pm app`'s board (AGT-1402 placeholder; the real board is AGT-1404): every
// unarchived ticket in a column per workflow state, read-only, refreshed as
// ops land. What it proves is the plumbing — session, reads, the live
// event stream — that the real board builds on.

import { useCallback, useEffect, useState } from "react";
import { baseCss, useApi, useEvents, type ViewProps } from "./lib/pm";

interface State {
  name: string;
  category: string;
  position: number;
}

interface Workspace {
  prefix: string;
  states: State[];
  actor: string;
}

interface Ticket {
  id: string;
  title: string;
  state: string;
  priority: string;
  project: string | null;
  assignee: string | null;
  labels: string[];
}

export default function Board({ mutate }: ViewProps) {
  const { api, error: connectError } = useApi(mutate);
  const [workspace, setWorkspace] = useState<Workspace | null>(null);
  const [tickets, setTickets] = useState<Ticket[]>([]);
  const [error, setError] = useState<string | null>(null);

  const refresh = useCallback(() => {
    if (!api) return;
    Promise.all([api.get<Workspace>("/workspace"), api.get<Ticket[]>("/tickets")]).then(
      ([ws, list]) => {
        setWorkspace(ws);
        setTickets(list);
        setError(null);
      },
      (e) => setError(e instanceof Error ? e.message : String(e)),
    );
  }, [api]);

  useEffect(refresh, [refresh]);
  // Any ticket op can move a card; workspace ops can change the columns.
  useEvents(api, refresh, (op) => op.id !== null || op.kind.startsWith("workspace"));

  const failure = connectError ?? error;
  if (failure) return <p className="pm-error">pm: {failure}</p>;
  if (!workspace) return <p className="pm-muted" style={{ padding: "1rem" }}>Loading…</p>;

  const states = [...workspace.states].sort((a, b) => a.position - b.position);
  return (
    <div style={{ padding: "1rem" }}>
      <style>{baseCss}</style>
      <h1 style={{ fontSize: "1.1rem", margin: "0 0 .75rem" }}>
        {workspace.prefix} board <span className="pm-muted">· {tickets.length} tickets</span>
      </h1>
      <div style={{ display: "flex", gap: ".75rem", alignItems: "flex-start", overflowX: "auto" }}>
        {states.map((s) => {
          const column = tickets.filter((t) => t.state === s.name);
          return (
            <section key={s.name} data-state={s.name} style={{ minWidth: "14rem", flex: "0 0 14rem" }}>
              <h2 style={{ fontSize: ".85rem", margin: "0 0 .5rem" }}>
                {s.name} <span className="pm-muted">{column.length}</span>
              </h2>
              {column.map((t) => (
                <article
                  key={t.id}
                  data-ticket={t.id}
                  style={{
                    background: "var(--card)",
                    border: "1px solid var(--line)",
                    borderRadius: 6,
                    padding: ".5rem .6rem",
                    marginBottom: ".5rem",
                  }}
                >
                  <div className="pm-muted" style={{ fontSize: 12 }}>
                    {t.id} · {t.priority}
                    {t.assignee ? ` · ${t.assignee}` : ""}
                  </div>
                  <div>{t.title}</div>
                </article>
              ))}
            </section>
          );
        })}
      </div>
    </div>
  );
}
