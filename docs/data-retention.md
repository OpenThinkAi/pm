# Data retention

pm is an append-only log of operations. That is what makes sync and
offline work conflict-free, and it means **deleting something in pm does
not erase it.** Read this before putting anything in a ticket, comment,
hold reason or project doc that you might later need to remove.

## What delete means

`pm delete` (tickets) and `pm project delete` set a tombstone: the record
stops appearing in `list`, `show`, `ready`, exports and the app. The
delete is itself one more op in the log. Nothing is rewritten or removed.

Editing has the same property. Replacing a title, changing a hold reason
or cutting text out of a ticket or project doc appends a new op; the
previous value stays in the history.

## Where the content persists

| Place | What it holds |
| --- | --- |
| The op log (`pm.sqlite` on every replica) | Every op ever written: titles, comments, hold reasons, field values, including those of deleted records and superseded values. |
| Loro body history (same database) | Ticket bodies and project docs are Loro documents. Text you removed from a body is still in the document's causal history and in every replica's copy of it. |
| The hub (Postgres `ops` table) | The same ops, for every workspace synced to it. Hub-side admin deletes of tokens or workspaces are separate from op content. |
| Git backups (`pm backup`) | Op shards (`ops/<prefix>/*.jsonl`) committed to the backup repo, so git history keeps every version of every shard, for as long as that repo and its clones exist. |
| Exports (`pm export md`) | Plain files of current, non-deleted state, owner-only (`0600` files in `0700` directories, AGT-1482). Already-written export files and any git history of them keep old content until you remove them yourself. |
| Sync quarantine (same database) | Pulled ops a replica refused (AGT-1467). A refused op's content is dropped 30 days after its refusal, or at once with `pm doctor --prune-quarantine` (AGT-1482); its op id, hub seq, kind, entity and reason stay. Parked ops keep theirs until they land or are refused. |

Each replica (every machine that ran `pm sync`) has its own full copy.
Removing data from one place does not remove it from the others.

## How to remove data today

There is no supported way. No command purges or redacts an op, a body
edit, a comment or a tombstoned record. The honest options are:

- **Do not write it.** Keep secrets, credentials and personal data out of
  tickets. This is the only reliable control.
- **Rebuild from a filtered log.** Export the op log, remove or rewrite
  the offending ops, and create a fresh workspace and hub database from
  the result; then discard every old replica, hub database, backup repo
  (including git history) and export. This is manual, changes op
  identities that other replicas rely on, and is only as complete as your
  filtering. It is a last resort, not a feature.

## Possible future design

A purge or redaction op, one that replicas and the hub apply by
blanking the targeted content (ticket fields, comments, Loro body history)
while keeping the op's identity so merge and sync keep working, has not
been designed or built. If you want one, say so; it needs decisions on
Loro history compaction, hub and backup rewrite, and who may issue it.
