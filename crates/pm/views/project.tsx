// `pm project edit <ID>`'s view (AGT-1405; docs/app-api.md §The project
// view): the project's design doc and named documents in the same CRDT
// editor as a ticket's description (lib/editor.tsx's `BodyEditor` bound by
// lib/body.ts's `BodySync` to `/projects/{id}/body` or
// `/projects/{id}/docs/{name}/body` — every edit a `body.edit` op on the
// document's `doc_id`), and the project's tickets, live.
//
// "New ticket" files through `POST /tickets` exactly as `pm new --title …
// --project <id>` would, and the new ticket opens in the editor. ui-leaf
// gives a view no way to open another view (its host-side `view` swap
// replaces the only window's view and takes no relative imports, so it
// could not carry the editor), so the ticket editor opens *inline*: the
// ticket list gives way to a `TicketEditor` pane — the same component
// `pm edit` shows — until "Back to tickets". Clicking any ticket in the
// list opens it the same way. A ticket filed while a hub is configured has
// no number until `pm sync`, so the pane names it by its ULID.
//
// The policy (tabs, what an op event triggers, the list's sections, the
// request "New ticket" sends) lives in lib/project.ts, which
// `node --test crates/pm/views-test/*.test.ts` covers.

import { useApi, type ViewProps } from "./lib/pm";
import { ProjectPage } from "./lib/projectpage";

export default function ProjectView({ data, mutate }: ViewProps) {
  const { api, error: connectError } = useApi(mutate);
  const id = data.project ?? "";
  if (connectError) return <p className="pm-error">pm: {connectError}</p>;
  if (!api) {
    return (
      <p className="pm-muted" style={{ padding: "1rem" }}>
        Loading {id}…
      </p>
    );
  }
  return <ProjectPage api={api} id={id} />;
}
