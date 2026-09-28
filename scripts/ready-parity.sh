#!/usr/bin/env bash
#
# ready-parity.sh — does `pm ready` agree with the build loops' frontier?
# (AGT-1349; projects/pm/README.md P1 exit "pm ready parity").
#
# The loops compute their ready frontier from prose in
# ~/.claude/commands/scaffold-build-loop.md (the template every *-build.md
# is generated from). This script is a line-by-line transcription of that
# prose into Python, run against a markdown vault, and diffed against
# `pm ready --project <id> --json` on the same vault imported into pm.
# Every difference is classified; anything not in a documented class is
# printed as UNEXPLAINED and makes the script exit 1.
#
# The vault is only ever read. Without --workspace, the vault is imported
# into a fresh temp workspace with HOME/XDG_*/PM_WORKSPACE pointed at the
# temp dir, so the real ~/.config/pm and ~/.local/share/pm are never
# touched.
#
# Dependencies: bash, python3 (stdlib only), cargo unless --pm is given.

set -euo pipefail

usage() {
  # Prints the block between the sentinels below (see --help).
  sed -n '/^# BEGIN_USAGE/,/^# END_USAGE/p' "$0" | sed '1d;$d' | sed 's/^# \{0,1\}//'
  exit 2
}
# BEGIN_USAGE
# Usage:
#   scripts/ready-parity.sh [--vault DIR] [--pm BIN] [--workspace DIR]
#                           [--project ID]... [--all] [--json FILE] [--keep]
#
#   --vault DIR      the markdown vault (default ~/saltline-digital-vault).
#                    Only ever read.
#   --pm BIN         the pm binary (default: `cargo build --locked` in this
#                    repo, then target/debug/pm).
#   --workspace DIR  an existing pm workspace holding the imported vault.
#                    Default: a fresh temp workspace, `pm init` + `pm import
#                    vault` there (HOME/XDG_*/PM_WORKSPACE redirected).
#   --project ID     compare only these projects (repeatable). Default:
#                    every live project, i.e. every projects/<id>/README.md.
#   --all            also compare the unscoped frontier (`pm ready` with no
#                    --project vs every ticket under tickets/).
#   --json FILE      also write the machine-readable result to FILE.
#   --keep           keep the temp workspace (its path is printed).
#
# Exit: 0 when every difference is in a documented class, 1 when any is
# UNEXPLAINED (or the vault/pm could not be read), 2 on usage.
# END_USAGE

VAULT="${VAULT:-$HOME/saltline-digital-vault}"
PM_BIN=""
WORKSPACE=""
PROJECTS=()
ALL=0
JSON_OUT=""
KEEP=0

while [ $# -gt 0 ]; do
  case "$1" in
    --vault) VAULT="$2"; shift 2 ;;
    --pm) PM_BIN="$2"; shift 2 ;;
    --workspace) WORKSPACE="$2"; shift 2 ;;
    --project) PROJECTS+=("$2"); shift 2 ;;
    --all) ALL=1; shift ;;
    --json) JSON_OUT="$2"; shift 2 ;;
    --keep) KEEP=1; shift ;;
    -h|--help) usage ;;
    *) echo "ready-parity: unknown argument '$1'" >&2; usage ;;
  esac
done

if [ ! -d "$VAULT/tickets" ]; then
  echo "ready-parity: $VAULT is not a vault (no tickets/ directory)" >&2
  exit 1
fi

REPO="$(cd "$(dirname "$0")/.." && pwd)"
if [ -z "$PM_BIN" ]; then
  (cd "$REPO" && cargo build --locked --quiet)
  PM_BIN="$REPO/target/debug/pm"
fi
if [ ! -x "$PM_BIN" ]; then
  echo "ready-parity: pm binary $PM_BIN is not executable" >&2
  exit 1
fi

# A temp workspace when none was given. HOME and the XDG dirs are
# redirected so `pm init` writes its config.toml under the temp dir and
# nothing reaches the real ~/.config/pm or ~/.local/share/pm.
TMP=""
if [ -z "$WORKSPACE" ]; then
  TMP="$(mktemp -d "${TMPDIR:-/tmp}/ready-parity.XXXXXX")"
  WORKSPACE="$TMP/ws"
  export HOME="$TMP/home"
  export XDG_CONFIG_HOME="$TMP/home/.config"
  export XDG_DATA_HOME="$TMP/home/.local/share"
  mkdir -p "$HOME"
  "$PM_BIN" init --prefix AGT --preset saltline --workspace "$WORKSPACE" >/dev/null
  PM_WORKSPACE="$WORKSPACE" "$PM_BIN" import vault "$VAULT" >"$TMP/import.txt"
  echo "imported $VAULT into $WORKSPACE ($(grep -c . "$TMP/import.txt") report lines)"
fi
export PM_WORKSPACE="$WORKSPACE"

cleanup() {
  if [ -n "$TMP" ]; then
    if [ "$KEEP" = 1 ]; then
      echo "kept temp workspace: $TMP"
    else
      rm -rf "$TMP"
    fi
  fi
}
trap cleanup EXIT

# `${ARR[@]+"${ARR[@]}"}` expands an empty array to zero words under `set -u`
# (bash < 4.4 would otherwise abort on it).
PY_ARGS=("$VAULT" "$PM_BIN")
for p in ${PROJECTS[@]+"${PROJECTS[@]}"}; do
  PY_ARGS+=("--project" "$p")
done
[ "$ALL" = 1 ] && PY_ARGS+=("--all")
[ -n "$JSON_OUT" ] && PY_ARGS+=("--json" "$JSON_OUT")

python3 - "${PY_ARGS[@]}" <<'PY'
"""The loops' ready frontier, transcribed from prose, diffed against pm.

Every rule below cites the prose it transcribes:

  [T:n]  ~/.claude/commands/scaffold-build-loop.md line n (the template,
         between `=== TEMPLATE START ===` and `=== TEMPLATE END ===`, plus
         its "Vault layout" section) as of 2026-09-28
  [B:n]  ~/.claude/commands/pm-build.md line n (a generated instance)
  [C:n]  saltline-digital-vault/projects/pm/research/consumers.md line n

The prose, verbatim, and where each sentence lands in this file:

  [T:27]   `$VAULT/tickets/{triage,refined,in-progress,done}/AGT-NNN-<slug>.md
            # state = folder + frontmatter `state:``           -> read_vault(): state is the folder
  [T:30]   "A ticket's frontmatter carries `id`, `project`, `repo` (`owner/name`),
            `state`, `blocked-by: [AGT-…]`."                    -> blocked_by_of(): the `blocked-by:` line
  [T:31]   "A ticket is **done** iff it's in `tickets/done/` (and `state: done`)."
                                                              -> is_done(): folder == "done"
  [T:31-32] "It is **ready** iff it's not done/in-progress and every id in its
            `blocked-by` is done."                            -> frontier(): rules R1, R2
  [T:41]   `project:  rg -l "^project: <id>$" "$VAULT/tickets"`
                                                              -> in_scope(): the same anchored regex,
                                                                 over tickets/ only (never archive/)
  [T:114]  (repeats T:31-32 inside the generated skill)      -> frontier()
  [T:127]  rule 5: a `⚠ NEEDS-HUMAN: …` line in the body "tells the next tick's
            recovery (Phase 0.5) it's a deliberate hand-back — do NOT auto-retry
            it, and skip anything it blocks."                 -> frontier(): rules R3, R4
  [T:147-149] Phase 0.5: "**Body contains `NEEDS-HUMAN`** → deliberate hand-back.
            Do NOT touch it; exclude it and anything it blocks from this wave."
                                                              -> has_needs_human(): a substring test
                                                                 on the whole file (that is what
                                                                 "contains" means to the loop, and
                                                                 it is the quoted-marker false
                                                                 positive pm fixes)
  [T:169-170] "**Ready frontier** = every in-scope ticket that is **not**
            done/in-progress and whose **every `blocked-by` is done**,
            **excluding** `NEEDS-HUMAN` tickets and anything they block."
                                                              -> frontier(): R1..R4 together
  [T:171-172] the single-`AGT-NNN` argument form                -> not transcribed: it is a
                                                                 caller-side filter of the same set
  [T:56]   `MODELS`: the loop dispatches on the `model:` label but never
            filters the frontier by it                        -> pm ready is run without --model
  [B:75-76] pm-build adds "**not** `manual`" and "excluding … `manual` tickets,
            and anything they block" (rule 10)                -> NOT part of the template; pm's
                                                                 workspace `gate_labels` is the
                                                                 data form of that per-loop rule,
                                                                 so a pm `label` exclusion is the
                                                                 documented class "gate-label"
  [C:30-32] "Frontier: ready set minus NEEDS-HUMAN, label-gated, and everything
            they transitively block"                          -> transitive vs direct makes no
                                                                 difference to the READY SET: a
                                                                 transitively blocked ticket has a
                                                                 not-done direct blocker (R2)
                                                                 already. R4 is direct, as T:170
                                                                 reads; the reason text is not
                                                                 compared.

What is NOT in the prose (and so counts as a difference to classify):
  - archived tickets: the loop's done set is `tickets/done/` only; a blocker
    swept into `archive/YYYY-MM/` is "not done" to the loop (README "Archive
    blindness"). pm counts archived as done.          -> class "archive-as-done"
  - a `blocked-by` id that exists nowhere in the vault is likewise "not
    done" to the loop, forever. pm import skips the relation and reports
    it as an anomaly.                                 -> class "dangling-blocker"
  - `parked:` / `not_before` markers: the template never reads them; pm
    excludes an actively parked ticket.               -> class "parked-marker"
  - `waiting-human:` / `NEEDS-CLARIFICATION:` holds: other loops' spellings
    of NEEDS-HUMAN (vault-anatomy.md); the template greps only NEEDS-HUMAN.
    pm imports all three as holds.                    -> class "hold-spelling"
  - a ticket file with no frontmatter (AGT-806, README "Corruption and lost
    work"): to the loop it is a file in tickets/triage/ with no project,
    no blockers and no marker, i.e. ready in an unscoped frontier and
    invisible to every `rg "^project:"`. pm import reads it from the git
    object listed in `RECOVER_FROM_GIT` and folds it into its intact
    archived copy.                                    -> class "no-frontmatter"
"""

import collections
import json
import os
import re
import subprocess
import sys

argv = sys.argv[1:]
vault = argv[0]
pm_bin = argv[1]
projects_wanted = []
compare_all = False
json_out = None
i = 2
while i < len(argv):
    a = argv[i]
    if a == "--project":
        projects_wanted.append(argv[i + 1])
        i += 2
    elif a == "--all":
        compare_all = True
        i += 1
    elif a == "--json":
        json_out = argv[i + 1]
        i += 2
    else:
        sys.exit(f"ready-parity: bad internal argument {a!r}")

ID_RE = re.compile(r"AGT-\d+")
FILE_ID_RE = re.compile(r"^(AGT-\d+)")


class Ticket:
    __slots__ = ("id", "folder", "path", "text", "blocked_by")

    def __init__(self, id_, folder, path, text):
        self.id = id_
        self.folder = folder
        self.path = path
        self.text = text
        self.blocked_by = blocked_by_of(text)


def in_scope(t, project):
    # [T:41] exactly the rg pattern: line-anchored, the bare id, nothing else
    # on the line. A quoted `project: "pm"` would NOT match the loop's rg
    # either, so it does not match here.
    return re.search(r"^project: " + re.escape(project) + r"$", t.text, re.M) is not None


def blocked_by_of(text):
    # [T:30] `blocked-by: [AGT-…]` — the ids on the frontmatter's
    # `blocked-by:` line. Only the frontmatter block is read (the text
    # between the opening `---` and the next `---`); a file with no
    # frontmatter, or none closed, has no blockers the loop could read. A
    # YAML block list on the following `- AGT-N` lines is accepted too, in
    # case a hand-written ticket used it (the live vault uses flow lists).
    if not text.startswith("---"):
        return []
    lines = text.split("\n")
    end = next((n for n, line in enumerate(lines) if n > 0 and line.startswith("---")), None)
    if end is None:
        return []
    fm = lines[1:end]
    for n, line in enumerate(fm):
        if line.startswith("blocked-by:"):
            ids = ID_RE.findall(line)
            if not ids and line.strip() == "blocked-by:":
                for nxt in fm[n + 1:]:
                    if re.match(r"^\s+-\s+", nxt):
                        ids += ID_RE.findall(nxt)
                    else:
                        break
            return ids
    return []


def has_needs_human(t):
    # [T:147] "Body contains `NEEDS-HUMAN`": a substring test on the file, as
    # `grep -q NEEDS-HUMAN` (or an LLM reading the body) sees it. This is
    # deliberately the loop's naive test, quoted mentions included.
    return "NEEDS-HUMAN" in t.text


def read_vault(root):
    """Every ticket under tickets/<folder>/, keyed by the id in its filename
    ([T:27]: the loop names tickets by `AGT-NNN-<slug>.md` and `mv`s them by
    that name). archive/ is read separately: it is NOT part of the loop's
    world, only of the classification."""
    live = {}
    tickets_dir = os.path.join(root, "tickets")
    for folder in sorted(os.listdir(tickets_dir)):
        d = os.path.join(tickets_dir, folder)
        if not os.path.isdir(d):
            continue
        for name in sorted(os.listdir(d)):
            m = FILE_ID_RE.match(name)
            if not m or not name.endswith(".md"):
                continue
            path = os.path.join(d, name)
            with open(path, encoding="utf-8", errors="replace") as f:
                text = f.read()
            t = Ticket(m.group(1), folder, os.path.relpath(path, root), text)
            # Two files with one id: the loop would see both; keep the first
            # (pm gives the duplicate a fresh number) and say so.
            if t.id in live:
                print(f"warning: {t.path} duplicates {live[t.id].path}; using the first", file=sys.stderr)
            else:
                live[t.id] = t
    archived = {}
    archive_dir = os.path.join(root, "archive")
    if os.path.isdir(archive_dir):
        for month in sorted(os.listdir(archive_dir)):
            d = os.path.join(archive_dir, month)
            if not month.startswith("20") or not os.path.isdir(d):
                continue
            for dirpath, dirnames, filenames in os.walk(d):
                dirnames[:] = [x for x in dirnames if x != "assets"]
                for name in sorted(filenames):
                    m = FILE_ID_RE.match(name)
                    if m and name.endswith(".md"):
                        archived.setdefault(m.group(1), os.path.relpath(os.path.join(dirpath, name), root))
    return live, archived


def is_done(t):
    # [T:31] done iff in tickets/done/. The "(and `state: done`)" half is the
    # folder↔frontmatter invariant vault-sweep §4 checks; the loop reads the
    # folder ("read its folder (state)", T:165).
    return t.folder == "done"


def frontier(live, scope_ids):
    """[T:169-170] over the in-scope ids. Returns (ready, why) where why maps
    every non-ready in-scope id to the first rule that excluded it."""
    ready = []
    why = {}
    needs_human = {i for i in scope_ids if has_needs_human(live[i])}
    for i in scope_ids:
        t = live[i]
        # R1 [T:169] "not done/in-progress"
        if t.folder in ("done", "in-progress"):
            why[i] = ("state", t.folder)
            continue
        # R3 [T:170] "excluding NEEDS-HUMAN tickets"
        if i in needs_human:
            why[i] = ("needs-human", None)
            continue
        # R4 [T:170] "and anything they block" (direct dependents; see the
        # module docstring on transitive)
        gate = [b for b in t.blocked_by if b in needs_human]
        if gate:
            why[i] = ("blocked-by-needs-human", gate[0])
            continue
        # R2 [T:169-170] "whose every blocked-by is done" — done per [T:31],
        # so an id not present in tickets/done/ (archived, absent, or live
        # elsewhere) is not done.
        pending = [b for b in t.blocked_by if not (b in live and is_done(live[b]))]
        if pending:
            why[i] = ("blocked-by", pending)
            continue
        ready.append(i)
    return ready, why


def pm_ready(project):
    cmd = [pm_bin, "--json", "ready"]
    if project is not None:
        cmd += ["--project", project]
    out = subprocess.run(cmd, capture_output=True, text=True)
    if out.returncode != 0:
        sys.exit(f"ready-parity: {' '.join(cmd)} exited {out.returncode}: {out.stderr.strip()}")
    return json.loads(out.stdout)


def pm_ticket_id(t):
    # The display id (`AGT-N`) of one Ticket object from `pm ready --json`.
    # A missing key means the JSON shape moved under us: fail loudly rather
    # than diff against None.
    i = t.get("id")
    if not isinstance(i, str) or not ID_RE.fullmatch(i):
        sys.exit(f"ready-parity: pm ready --json ticket has no display id: {json.dumps(t)[:200]}")
    return i


def blocker_status(b, live, archived):
    if b in live:
        return f"{b} in tickets/{live[b].folder}/"
    if b in archived:
        return f"{b} in {os.path.dirname(archived[b])}/"
    return f"{b} exists nowhere in the vault"


def classify(i, side, prose_why, pm_excluded, live, archived):
    """One ticket in the symmetric difference → (class, detail). `side` is
    "pm-only" (pm says ready, the loop does not) or "loop-only"."""
    if side == "pm-only":
        rule, arg = prose_why.get(i, (None, None))
        if rule == "blocked-by":
            # Every blocker the loop still waits on must be one pm counts
            # as done for a reason the loop lacks; one live pending blocker
            # would be a pm bug.
            detail = "blockers " + ", ".join(blocker_status(b, live, archived) for b in arg)
            if all(b in archived for b in arg):
                return "archive-as-done", detail
            if all(b in archived or b not in live for b in arg):
                return "dangling-blocker", detail
            return "UNEXPLAINED", f"loop: {detail}; pm: ready"
        if rule == "needs-human":
            return "quoted-marker", "file contains NEEDS-HUMAN but pm imported no hold (quoted mention)"
        if rule == "blocked-by-needs-human":
            return "quoted-marker", f"blocker {arg} contains NEEDS-HUMAN but pm imported no hold on it"
        return "UNEXPLAINED", f"loop excluded it as {rule} {arg or ''}; pm: ready"
    ex = pm_excluded.get(i)
    if ex is None:
        if not live[i].text.startswith("---"):
            return "no-frontmatter", f"{live[i].path} has no frontmatter; pm recovered it from git and merged it into its archived copy"
        return "UNEXPLAINED", "loop: ready; pm: not a candidate at all (not in ready or excluded)"
    reason = ex.get("reason")
    msg = ex.get("message", "")
    if reason == "label":
        return "gate-label", msg
    if reason == "parked":
        return "parked-marker", msg
    if reason == "not-before":
        return "parked-marker", msg
    if reason == "held":
        text = live[i].text
        if "waiting-human:" in text or "NEEDS-CLARIFICATION:" in text:
            return "hold-spelling", msg
        return "UNEXPLAINED", f"pm holds it ({msg}) but the file has no NEEDS-HUMAN/waiting-human/NEEDS-CLARIFICATION"
    if reason == "transitively-blocked" and ex.get("gate", {}).get("kind") == "label":
        return "gate-label", msg
    if reason == "blocked-by" and (ex.get("gate") or {}).get("kind") == "label":
        return "gate-label", msg
    return "UNEXPLAINED", f"loop: ready; pm: {reason}: {msg}"


live, archived = read_vault(vault)

projects_dir = os.path.join(vault, "projects")
live_projects = sorted(
    p for p in os.listdir(projects_dir)
    if os.path.isfile(os.path.join(projects_dir, p, "README.md"))
) if os.path.isdir(projects_dir) else []
if projects_wanted:
    live_projects = projects_wanted

results = []
totals = collections.Counter()
unexplained = 0


def compare(label, scope_ids, pm_json):
    """Diffs one scope and prints its row; accumulates into the module-level
    `results`, `totals` and `unexplained` (a script, run once, top to bottom)."""
    global unexplained
    prose_ready, prose_why = frontier(live, scope_ids)
    pm_ready_ids = [pm_ticket_id(t) for t in pm_json["ready"]]
    pm_excluded = {e["id"]: e for e in pm_json["excluded"]}
    # Candidate sets: the loop's in-scope not-done tickets vs pm's verdicts
    # (pm judges every live, not-done ticket in scope). in-progress tickets
    # are candidates on both sides (excluded by state).
    loop_candidates = {i for i in scope_ids if not is_done(live[i])}
    pm_candidates = set(pm_ready_ids) | set(pm_excluded)
    scope_diff = sorted(loop_candidates ^ pm_candidates, key=lambda s: int(s.split("-")[1]))
    diffs = []
    seen = set()
    for i in sorted(set(prose_ready) ^ set(pm_ready_ids), key=lambda s: int(s.split("-")[1])):
        side = "pm-only" if i in pm_ready_ids else "loop-only"
        cls, detail = classify(i, side, prose_why, pm_excluded, live, archived)
        diffs.append({"id": i, "side": side, "class": cls, "detail": detail})
        seen.add(i)
        totals[cls] += 1
        if cls == "UNEXPLAINED":
            unexplained += 1
    # A ticket one side does not judge at all: reported once, even when it
    # was already a ready-verdict difference above.
    for i in scope_diff:
        if i in seen:
            continue
        where = "loop" if i in loop_candidates else "pm"
        if where == "loop" and not live[i].text.startswith("---"):
            cls, detail = "no-frontmatter", f"{live[i].path} is a candidate to the loop only: no frontmatter"
        else:
            cls, detail = "UNEXPLAINED", f"in scope for {where} only"
        diffs.append({"id": i, "side": f"candidate-{where}-only", "class": cls, "detail": detail})
        totals[cls] += 1
        if cls == "UNEXPLAINED":
            unexplained += 1
    row = {
        "project": label,
        "in_scope": len(scope_ids),
        "loop_ready": prose_ready,
        "pm_ready": pm_ready_ids,
        "agree": sorted(set(prose_ready) & set(pm_ready_ids), key=lambda s: int(s.split("-")[1])),
        "diffs": diffs,
    }
    results.append(row)
    status = "OK" if not diffs else ("EXPLAINED" if all(d["class"] != "UNEXPLAINED" for d in diffs) else "UNEXPLAINED")
    print(f"{label:<28} scope {len(scope_ids):>3}  loop-ready {len(prose_ready):>2}  pm-ready {len(pm_ready_ids):>2}  "
          f"agree {len(row['agree']):>2}  diffs {len(diffs):>2}  {status}")
    for d in diffs:
        print(f"    {d['id']:<9} {d['side']:<10} {d['class']:<16} {d['detail']}")

for p in live_projects:
    scope = [i for i, t in live.items() if in_scope(t, p)]
    compare(p, scope, pm_ready(p))
if compare_all:
    compare("(all tickets/)", list(live), pm_ready(None))

print()
print("difference classes:", dict(sorted(totals.items())) or "none")
print(f"projects compared: {len(live_projects)}{' + all' if compare_all else ''}; "
      f"tickets under tickets/: {len(live)}; archived: {len(archived)}")

if json_out:
    with open(json_out, "w", encoding="utf-8") as f:
        json.dump({"vault": vault, "projects": results, "classes": dict(totals),
                   "unexplained": unexplained}, f, indent=2)
    print(f"wrote {json_out}")

sys.exit(1 if unexplained else 0)
PY
