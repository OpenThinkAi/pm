// `pm edit <ID>`'s view (AGT-1403): the ticket editor filling the window.
// The editor itself — fields, labels, state, and the description bound to
// the text CRDT, all live — is `TicketEditor` in lib/editor.tsx, which the
// project view (AGT-1405) also hosts inline.

import { baseCss, useApi, type ViewProps } from "./lib/pm";
import { TicketEditor } from "./lib/editor";

export default function TicketView({ data, mutate }: ViewProps) {
  const { api, error } = useApi(mutate);
  const id = data.ticket ?? "";
  if (error) return <p className="pm-error">pm: {error}</p>;
  return (
    <>
      <style>{baseCss}</style>
      {api ? (
        <TicketEditor api={api} id={id} />
      ) : (
        <p className="pm-muted" style={{ padding: "1rem" }}>
          Loading {id}…
        </p>
      )}
      <p className="pm-muted" style={{ fontSize: 12, textAlign: "center", marginTop: "1rem" }}>
        Every change is saved as you make it. Close this window to return to the terminal.
      </p>
    </>
  );
}
