# Data retention

pm is an append-only log of operations. That is what makes sync and
offline work conflict-free, and it means **deleting something in pm does
not erase it.** Erasing takes a redaction (`pm redact`, below), and even
that cannot reach every copy. Read this before putting anything in a
ticket, comment, hold reason or project doc that you might later need to
remove.

## What delete means

A ticket's deletion (a `tombstone` op) and `pm project delete` set a
tombstone: the record stops appearing in `list`, `show`, `ready`, exports and the app. The
delete is itself one more op in the log. Nothing is rewritten or removed.

Editing has the same property. Replacing a title, changing a hold reason
or cutting text out of a ticket or project doc appends a new op; the
previous value stays in the history.

## Where the content persists

| Place | What it holds |
| --- | --- |
| The op log (`pm.sqlite` on every replica) | Every op ever written: titles, comments, hold reasons, field values, including those of deleted records and superseded values. The database (and its `-wal`/`-shm`) is owner-only (`0600`; an older one is narrowed when opened), in a `0700` directory when `pm init` creates it (AGT-2434). |
| Loro body history (same database) | Ticket bodies and project docs are Loro documents. Text you removed from a body is still in the document's causal history and in every replica's copy of it. |
| The hub (Postgres `ops` table) | The same ops, for every workspace synced to it. Hub-side admin deletes of tokens or workspaces are separate from op content. |
| Git backups (`pm backup`) | Op shards (`ops/<prefix>/*.jsonl`) committed to the backup repo, so git history keeps every version of every shard, for as long as that repo and its clones exist. |
| Exports (`pm export md`) | Plain files of current, non-deleted state, owner-only (`0600` files in `0700` directories, AGT-1482). Already-written export files and any git history of them keep old content until you remove them yourself. |
| Sync quarantine (same database) | Pulled ops a replica refused (AGT-1467). A refused op's content is dropped 30 days after its refusal, or at once with `pm doctor --prune-quarantine` (AGT-1482); one refused for its size is never stored (AGT-2434); its op id, hub seq, kind, entity and reason stay. The database overwrites deleted content (`secure_delete`), so dropped content does not linger in free pages. Parked ops keep theirs until they land or are refused: after 32 retries, or once the hub log has moved 10,000 sequence numbers past them (AGT-2457, `pm_store::PARK_HORIZON_SEQS`), so parked rows stay bounded. |

Each replica (every machine that ran `pm sync`) has its own full copy.
Removing data from one place does not remove it from the others.

## How to erase data: redaction (AGT-2427)

`pm redact <ID>` erases a ticket's text; `pm project redact <ID>` a
project document's. Pick what to erase:

```sh
pm redact AGT-12 --comment 2          # the 2nd comment, as `pm show` numbers them
pm redact AGT-12 --title --hold       # every title and hold reason it has had
pm redact AGT-12 --description        # the description and its whole edit history
pm redact AGT-12 --all                # all of the above, every comment included
pm redact AGT-12 --op <OP-ID>         # one op from `pm log AGT-12 --json`
pm project redact pm --doc notes      # a named doc (default: the design doc)
```

Each writes one `redact` op naming the ops whose text goes. Every copy
that applies it **rewrites those ops in place**: a title, comment body or
hold reason becomes `[redacted]`; a description or document edit becomes
an edit that adds nothing, so the text, and its whole Loro history, reads
empty. Each op keeps its id, stamp, author and kind (and fields like
priority, or who held the ticket and when), so sync, merge and `pm doctor`
work as before. There is no undo. Afterwards a new title (`pm set
title=…`) or description (`pm edit`) starts clean.

Where it reaches:

| Place | After `pm redact` |
| --- | --- |
| This replica's database | Erased at once: the op log, the ticket or document rows, the stored views, and any copy in the sync quarantine. The database overwrites the old content (`secure_delete`) and checkpoints its write-ahead log after a redact, so the text does not linger in free pages or `pm.sqlite-wal` (a checkpoint another reader blocks completes later). `pm doctor` reports an op that still holds redacted text as unhealthy, and `--rebuild` erases it again. |
| The hub | Erased on the next `pm sync`: the hub rewrites the stored ops and its ticket view in the same transaction it stores the redact, and serves only the rewritten ops from then on. Postgres keeps the old row versions on disk until (auto)vacuum reclaims them, and its own backups and WAL archives keep whatever they captured. |
| Other replicas on pm 0.4.0 or later | Erased on their next `pm sync`, in whatever order the redact and the redacted ops reach them. |
| Replicas still on pm 0.3.0 or earlier | **Not erased.** They cannot read a redact, so the hub leaves redacts out of what it sends them; they keep any text they had already pulled (anything they pull afterwards arrives erased). When such a replica upgrades, its first `pm sync` fetches every redact once and erases the text. Upgrade every machine, or treat its copy as still holding the text. |
| Git backups (`pm backup`) | The next `pm backup` rewrites the redacted ops' lines in the shards, so that commit and later ones no longer hold the text. **Every earlier commit still does**, in the backup repo and in every clone of it. Erasing it there is a manual history rewrite (below). |
| Exports (`pm export md`) | Files written before the redaction keep the text until you re-export or delete them. |
| Hub database backups, disk snapshots, Time Machine | Whatever they captured before the redaction. Out of pm's reach. |

Who may redact: anyone who can write to the workspace may redact on their
own replica. At the hub, a token bound to actors (an agent's) may only
redact ops written by actors it is bound to; an unrestricted token may
redact anything. A refused redact stays in the outbox and `pm sync`
reports the refusal.

What a redaction does not cover: label names, project titles, workflow
state names, `ext` values, waiver reasons and actor ids are not erased by
any `pm redact` flag. Neither are the redact op itself (it names op ids,
never text) or the fact that the redacted ops existed, who wrote them and
when.

### Erasing from a backup's git history

`pm backup` keeps every version of every shard in git. After the backup
that follows the redaction has run (so its working tree no longer holds
the text), rewrite the repo's history, for example with
[`git filter-repo`](https://github.com/newren/git-filter-repo):

```sh
cd <backup repo>
# Replace the erased text in every historical version of every file.
printf '%s==>[redacted]\n' 'the exact text you erased' > /tmp/replacements.txt
git filter-repo --replace-text /tmp/replacements.txt
git push --force --all origin   # filter-repo removes `origin`; re-add it first
```

Then delete every other clone of the backup repo (and ask your git host to
purge cached views and pull-request refs, if it keeps them). The rewrite
changes commit ids; `pm backup` is unaffected, since it tracks its
progress in the workspace database, not in git. Do this on purpose and by
hand: pm never force-pushes.

### Rebuilding from a filtered log

For anything a redaction does not cover, the last resort remains: export
the op log, remove or rewrite the offending ops, and create a fresh
workspace and hub database from the result; then discard every old
replica, hub database, backup repo (including git history) and export.
This is manual, changes op identities other replicas rely on, and is only
as complete as your filtering.

### Still the best control

**Do not write it.** Keep secrets, credentials and personal data out of
tickets. A redaction shrinks the window; it cannot recall copies outside
pm, or ones made by machines you no longer control.
