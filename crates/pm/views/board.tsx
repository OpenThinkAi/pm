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

import { useApi, type ViewProps } from "./lib/pm";
import { BoardPage } from "./lib/boardpage";

export default function Board({ mutate }: ViewProps) {
  const { api, error: connectError } = useApi(mutate);
  if (connectError) return <p className="pm-error">pm: {connectError}</p>;
  if (!api) return <p className="pm-muted" style={{ padding: "1rem" }}>Loading…</p>;
  return <BoardPage api={api} />;
}
