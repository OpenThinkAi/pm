// `pm edit <ID>`'s view (AGT-1402 placeholder, read-only; the CRDT-bound
// editor is AGT-1403): the ticket as `pm show --json` returns it, refreshed
// whenever an op on it lands — from this machine's CLI or a sync.

import { useCallback, useEffect, useState } from "react";
import { baseCss, useApi, useEvents, type ViewProps } from "./lib/pm";

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

export default function TicketView({ data, mutate }: ViewProps) {
  const { api, error: connectError } = useApi(mutate);
  const [ticket, setTicket] = useState<Ticket | null>(null);
  const [error, setError] = useState<string | null>(null);
  const id = data.ticket ?? "";

  const refresh = useCallback(() => {
    if (!api) return;
    api.get<Ticket>(`/tickets/${encodeURIComponent(id)}`).then(
      (t) => {
        setTicket(t);
        setError(null);
      },
      (e) => setError(e instanceof Error ? e.message : String(e)),
    );
  }, [api, id]);

  useEffect(refresh, [refresh]);
  useEvents(api, refresh, (op) => op.id === id || (ticket !== null && op.entity === ticket.ulid));

  const failure = connectError ?? error;
  if (failure) return <p className="pm-error">pm: {failure}</p>;
  if (!ticket) return <p className="pm-muted" style={{ padding: "1rem" }}>Loading {id}…</p>;

  return (
    <article style={{ maxWidth: "46rem", margin: "0 auto", padding: "1.25rem" }}>
      <style>{baseCss}</style>
      <div className="pm-muted" style={{ fontSize: 12 }}>
        {ticket.id} · {ticket.state} · {ticket.priority}
        {ticket.project ? ` · ${ticket.project}` : ""}
        {ticket.assignee ? ` · ${ticket.assignee}` : ""}
      </div>
      <h1 style={{ fontSize: "1.35rem", margin: ".25rem 0 .5rem" }}>{ticket.title}</h1>
      <div style={{ marginBottom: ".75rem" }}>
        {ticket.labels.map((l) => (
          <span key={l} className="pm-chip">
            {l}
          </span>
        ))}
        {ticket.blocked_by.length > 0 && (
          <span className="pm-muted"> blocked by {ticket.blocked_by.join(", ")}</span>
        )}
      </div>
      <div style={{ whiteSpace: "pre-wrap", borderTop: "1px solid var(--line)", paddingTop: ".75rem" }}>
        {ticket.description || <span className="pm-muted">No description.</span>}
      </div>
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
        Read-only preview. Close this window to return to the terminal.
      </p>
    </article>
  );
}
