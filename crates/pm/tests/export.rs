//! `pm export md` and `pm import vault --report` through the built binary
//! (AGT-1348): the round trip over the fixture vault in
//! `tests/fixtures/vault`, and the parity report that proves it. Sandbox
//! as in `tests/import.rs`: temp HOME, cleared env, no stdin.

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use serde_json::{Value, json};
use tempfile::TempDir;

const FIXTURE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/vault");

/// 2026-09-01T00:00Z.
const SEP_1: u64 = 1_788_220_800_000;
const DAY: u64 = 86_400_000;

struct Sandbox {
    home: TempDir,
    ws: PathBuf,
}

impl Sandbox {
    fn initialized() -> Self {
        let home = tempfile::tempdir().unwrap();
        let ws = home.path().join("ws");
        let sb = Sandbox { home, ws };
        assert_ok(&sb.pm(&["init", "--prefix", "AGT", "--workspace", sb.ws_str()]));
        sb
    }

    fn ws_str(&self) -> &str {
        self.ws.to_str().unwrap()
    }

    fn path(&self, name: &str) -> PathBuf {
        self.home.path().join(name)
    }

    fn pm(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_pm"))
            .args(args)
            .env_clear()
            .env("HOME", self.home.path())
            .env("USER", "tester")
            .env("PATH", std::env::var("PATH").unwrap_or_default())
            .stdin(Stdio::null())
            .output()
            .unwrap()
    }

    fn import(&self, vault: &Path) -> Value {
        json(&self.pm(&["import", "vault", vault.to_str().unwrap(), "--json"]))
    }

    fn export(&self, dir: &Path, legacy: bool) -> Value {
        let mut args = vec!["export", "md", dir.to_str().unwrap(), "--json"];
        if legacy {
            args.push("--legacy-markers");
        }
        json(&self.pm(&args))
    }

    fn show(&self, id: &str) -> Value {
        json(&self.pm(&["show", id, "--json"]))
    }

    /// Every ticket, archived included, keyed by display id, with the
    /// values that legitimately differ between two imports removed:
    /// the ULID, the hold's op stamp, and `updated` (an ISO `updated`
    /// keeps its time of day in pm but exports as a date).
    fn tickets(&self) -> Vec<(String, Value)> {
        let list = json(&self.pm(&["list", "--archived", "--json"]));
        list.as_array()
            .unwrap()
            .iter()
            .map(|t| {
                let mut t = t.clone();
                let o = t.as_object_mut().unwrap();
                o.remove("ulid");
                o.remove("updated");
                if let Some(h) = o.get_mut("hold").and_then(Value::as_object_mut) {
                    h.remove("at");
                }
                // Relations list in ULID order, which no two imports share.
                if let Some(b) = o.get_mut("blocked_by").and_then(Value::as_array_mut) {
                    b.sort_by_key(|v| v.as_str().unwrap_or_default().to_string());
                }
                (t["id"].as_str().unwrap().to_string(), t)
            })
            .collect()
    }
}

fn stdout(out: &Output) -> String {
    String::from_utf8(out.stdout.clone()).unwrap()
}

fn stderr(out: &Output) -> String {
    String::from_utf8(out.stderr.clone()).unwrap()
}

fn assert_ok(out: &Output) {
    assert_eq!(
        out.status.code(),
        Some(0),
        "stdout: {}\nstderr: {}",
        stdout(out),
        stderr(out)
    );
}

fn json(out: &Output) -> Value {
    assert_ok(out);
    serde_json::from_slice(&out.stdout).unwrap()
}

fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

/// The one file under `dir` whose name starts with `prefix`.
fn find(dir: &Path, prefix: &str) -> PathBuf {
    let mut found = Vec::new();
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.starts_with(prefix))
        {
            found.push(path);
        }
    }
    assert_eq!(
        found.len(),
        1,
        "{prefix} under {}: {found:?}",
        dir.display()
    );
    found.remove(0)
}

fn write_vault(root: &Path, files: &[(&str, &str)]) {
    for (rel, text) in files {
        let path = root.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }
}

// ------------------------------------------------------------------ AC1

#[test]
fn export_writes_every_ticket_and_project_as_vault_markdown() {
    let sb = Sandbox::initialized();
    sb.import(Path::new(FIXTURE));
    let dir = sb.path("out");
    let report = sb.export(&dir, true);
    assert_eq!(report["schema"], 1);
    assert_eq!(report["legacy_markers"], true);
    // 7 files → 7 tickets (the duplicate AGT-14 file is AGT-16), 3 of
    // them archived; 4 projects with 2 named documents between them.
    assert_eq!(report["tickets"], 7);
    assert_eq!(report["archived"], 3);
    assert_eq!(report["unnumbered"], 0);
    assert_eq!(report["projects"], 4);
    assert_eq!(report["documents"], 2);
    assert_eq!(report["files"], 13);

    // State folders, the archive month, a title-derived slug for the
    // slugless AGT-15.md, and the renumbered duplicate under its new id.
    assert!(dir.join("tickets/triage").is_dir());
    assert!(
        dir.join("tickets/done/AGT-15-approve-the-e2-copy.md")
            .is_file()
    );
    find(
        &dir.join("tickets/in-progress"),
        "AGT-12-weekly-compose-pass-",
    );
    find(&dir.join("archive/2026-07"), "AGT-13-isolate-fork-pr-ci-");
    find(
        &dir.join("archive/2026-08"),
        "AGT-14-outreach-assist-state-",
    );
    let dup = find(&dir.join("archive/2026-08"), "AGT-16-claim-flow-ux-polish-");
    assert!(read(&dup).contains("\nduplicate_of_number: AGT-14\n"));

    // Template key order, an ISO `updated` as a date, `team` after
    // `source`, and the frontmatter `waived:` plus the dated `parked:`
    // as prose lines at the end of the description.
    let agt13 = read(&find(&dir.join("archive/2026-07"), "AGT-13-"));
    assert_eq!(
        agt13,
        "---\n\
         id: AGT-13\n\
         title: Isolate fork PR CI from the signing fleet\n\
         state: done\n\
         created: 2026-07-10\n\
         updated: 2026-07-12\n\
         project: old-audit\n\
         repo: OpenThinkAi/wickd\n\
         blocked-by: []\n\
         linked-github:\n\
         linked-pr:\n\
         priority: critical\n\
         labels: [security]\n\
         source: { type: audit, url: \"\", id: \"\", fetched-at: \"\" }\n\
         team: qa\n\
         ---\n\
         \n\
         ## Problem Statement\n\
         \n\
         Fork PRs run on self-hosted runners.\n\
         \n\
         ## Acceptance Criteria\n\
         \n\
         1. They don't.\n\
         \n\
         waived: standalone — repo-health fix, not part of a product project\n\
         parked: 2026-11-01\n\
         \n\
         ## Comments\n\
         \n\
         ### 2026-07-12 — QA agent\n\
         Verified.\n"
    );

    // A bare title with `: ` and ` #` is quoted; the hold and the prose
    // `parked:` come back in their original idioms (the parked reason is
    // the line, not an ext key); blockers are written in number order.
    let agt11 = read(&find(&dir.join("tickets/done"), "AGT-11-deps-"));
    assert!(agt11.contains(
        "\ntitle: \"deps: security dependency pass for think-cli (dependabot PRs #62, #92)\"\n"
    ));
    assert!(agt11.contains("\nlinked-pr: https://github.com/OpenThinkAi/think-cli/pull/99\n"));
    assert!(agt11.contains("\nresolution: merged\nteam: engineering\n---\n"));
    assert!(!agt11.contains("parked_reason"));
    assert!(agt11.contains(
        "- Deliberately blocked on the wave.\n\n⚠ NEEDS-HUMAN: Matt approves the changelog before it is published.\nparked: until AGT-10 is done.\n\n## Comments\n\n### 2026-08-03 — Engineering agent (spike)\nLooked at the diff.\n\n### 2026-08-05 — Shipped\nMerged as abc123.\n\n### Shipped 2026-08-05\nNot a dated entry: stays with the comment above.\n"
    ));
    let agt12 = read(&find(&dir.join("tickets/in-progress"), "AGT-12-"));
    assert!(agt12.contains("\nblocked-by: [AGT-10, AGT-11]\n"));
    assert!(agt12.contains("\nlabels: [model:sonnet-5]\n"));
    // A section after Comments stays in the description; the entries go
    // under their own heading at the end.
    assert!(agt12.contains("## Outcome\n\nComments are not the last section here.\n\nwaived: T3 — no single repo; touches the vault and launchd.\n⚠ NEEDS-HUMAN: install the plist, then record the first run here and move the ticket to done.\n\n## Comments\n\n### 2026-09-07 — Decomposed from alpha via /decompose-into-tickets\n"), "{agt12}");

    // Projects: the README verbatim, documents beside it, a retired
    // project under archive/projects.
    assert_eq!(
        read(&dir.join("projects/alpha/README.md")),
        read(&Path::new(FIXTURE).join("projects/alpha/README.md"))
    );
    assert_eq!(
        read(&dir.join("projects/alpha/EXECUTION.md")),
        "# Execution notes\n\nStep one.\n"
    );
    assert!(
        dir.join("projects/alpha/ideation/IDEA-001-first-idea.md")
            .is_file()
    );
    assert_eq!(
        read(&dir.join("projects/beta/README.md")),
        read(&Path::new(FIXTURE).join("projects/beta/README.md"))
    );
    assert!(dir.join("archive/projects/old-audit/README.md").is_file());

    // Without --legacy-markers the markers are frontmatter keys.
    let plain = sb.path("plain");
    let report = sb.export(&plain, false);
    assert_eq!(report["legacy_markers"], false);
    let agt11 = read(&find(&plain.join("tickets/done"), "AGT-11-deps-"));
    assert!(agt11.contains(
        "\nsource: { type: github, url: \"https://github.com/OpenThinkAi/think-cli/issues/62\", id: \"62\", fetched-at: \"2026-08-02T10:00:00Z\" }\nhold: Matt approves the changelog before it is published.\nparked: forever\nparked_reason: until AGT-10 is done.\nresolution: merged\n"
    ), "{agt11}");
    assert!(!agt11.contains("NEEDS-HUMAN"));
    let agt13 = read(&find(&plain.join("archive/2026-07"), "AGT-13-"));
    assert!(agt13.contains("\nwaived: standalone — repo-health fix, not part of a product project\nparked: 2026-11-01\nteam: qa\n---\n"));
}

// ------------------------------------------------------ AC1 round trip

#[test]
fn an_export_re_imports_to_the_same_tickets_in_both_marker_forms() {
    let sb = Sandbox::initialized();
    sb.import(Path::new(FIXTURE));
    let expected = sb.tickets();
    assert_eq!(expected.len(), 7);

    for legacy in [true, false] {
        let dir = sb.path(if legacy { "legacy" } else { "plain" });
        sb.export(&dir, legacy);
        let fresh = Sandbox::initialized();
        let report = fresh.import(&dir);
        assert_eq!(report["tickets"]["created"], 7, "{report}");
        assert_eq!(report["renumbered"], json!([]));
        assert_eq!(fresh.tickets(), expected, "legacy: {legacy}");
        // A second import of the same export is a no-op: the export is
        // a fixed point.
        let again = fresh.import(&dir);
        assert_eq!(again["ops"], 0, "{again}");
        // And exporting the fresh workspace reproduces the files.
        let out = fresh.path("out");
        fresh.export(&out, legacy);
        for rel in [
            "tickets/done/AGT-15-approve-the-e2-copy.md",
            "projects/alpha/README.md",
        ] {
            assert_eq!(read(&out.join(rel)), read(&dir.join(rel)), "{rel}");
        }
        let a = find(&dir.join("tickets/done"), "AGT-11-");
        let b = find(&out.join("tickets/done"), "AGT-11-");
        assert_eq!(read(&a), read(&b));
    }
}

// ------------------------------------------------------------ AC2, AC3

#[test]
fn the_parity_report_explains_every_diff_on_the_fixture_vault() {
    let sb = Sandbox::initialized();
    let report_path = sb.path("parity.md");
    let report = json(&sb.pm(&[
        "import",
        "vault",
        FIXTURE,
        "--report",
        report_path.to_str().unwrap(),
        "--json",
    ]));
    let parity = &report["parity"];
    assert_eq!(parity["report"], report_path.to_str().unwrap());
    assert_eq!(parity["compared"], 7);
    assert_eq!(parity["missing"], 0);
    assert_eq!(parity["unexplained"], 0, "{parity}");
    let explained = parity["explained"].as_object().unwrap();
    assert_eq!(explained["updated: ISO datetime"], 1, "{explained:?}");
    assert_eq!(explained["renumbered duplicate"], 1);
    assert_eq!(explained["filename: slugless"], 1);
    assert_eq!(explained["frontmatter marker → prose"], 1);
    assert_eq!(
        explained["key order"], 3,
        "AGT-11, AGT-12 and AGT-13 carry legacy orders"
    );
    // AGT-12's `updated` is 09-07; its last comment is dated 09-08.
    assert_eq!(explained["updated: stale (predates last comment)"], 1);
    assert!(
        explained
            .get("updated: re-imported change (stamped at import time)")
            .is_none()
    );

    let md = read(&report_path);
    assert!(md.starts_with("# Import parity report — "), "{md}");
    assert!(md.contains("- **unexplained diffs: 0**\n"));
    assert!(md.contains("## Unexplained diffs (0)\n\nNone.\n"));
    // The explained classes are in the header with their rationale.
    assert!(md.contains("| updated: ISO datetime | 1 |"));
    assert!(md.contains("| renumbered duplicate | 1 |"));
    // Non-template values are listed as round-tripping verbatim.
    assert!(md.contains("- source.type: audit: 1 file(s)\n"));
    assert!(md.contains("- priority: critical: 1 file(s)\n"));
    // The anomaly section names the duplicate pair and the slugless
    // file, each with where it went.
    assert!(md.contains("AGT-14 (archive/2026-08/AGT-14-claim-flow-polish.md): exported as archive/2026-08/AGT-16-claim-flow-ux-polish-"), "{md}");
    assert!(
        md.contains("0 unexplained diff(s); explained: id: AGT-14 → AGT-16 [renumbered duplicate]"),
        "{md}"
    );
    assert!(md.contains("AGT-15 (tickets/done/AGT-15.md): slugless filename; exported as tickets/done/AGT-15-approve-the-e2-copy.md; 0 unexplained diff(s)\n"));
    assert!(md.contains("- AGT-13 (`archive/2026-07/AGT-13-isolate-ci.md`): updated: 2026-07-12T21:17:00.000Z → 2026-07-12 [updated: ISO datetime]\n"));

    // The human report carries the summary line too.
    let out = sb.pm(&[
        "import",
        "vault",
        FIXTURE,
        "--report",
        report_path.to_str().unwrap(),
    ]);
    assert_ok(&out);
    assert!(
        stdout(&out).contains("parity:     7 compared, 0 missing, 0 unexplained diff(s), "),
        "{}",
        stdout(&out)
    );

    // Without --report the JSON has no `parity` key at all.
    let plain = sb.import(Path::new(FIXTURE));
    assert!(plain.get("parity").is_none());

    // A dry run on an empty workspace compares against nothing: every
    // file is missing, and that is an unexplained diff, not a pass.
    let empty = Sandbox::initialized();
    let report = json(&empty.pm(&[
        "import",
        "vault",
        FIXTURE,
        "--dry-run",
        "--report",
        empty.path("p.md").to_str().unwrap(),
        "--json",
    ]));
    assert_eq!(report["parity"]["missing"], 7);
    assert_eq!(report["parity"]["unexplained"], 7);
}

// ---------------------------------------------------- importer fix (AC3)

#[test]
fn file_dates_survive_an_import_into_a_workspace_with_newer_ops() {
    // The store's clock is already past every file date (a ticket filed
    // today); a first import must still read exactly as the files do,
    // and a re-imported field change must still win.
    let sb = Sandbox::initialized();
    json(&sb.pm(&["new", "--title", "filed today", "--json"]));
    let vault = sb.path("vault");
    std::fs::create_dir_all(&vault).unwrap();
    for entry in std::fs::read_dir(FIXTURE).unwrap() {
        let entry = entry.unwrap();
        let out = Command::new("cp")
            .arg("-R")
            .arg(entry.path())
            .arg(vault.join(entry.file_name()))
            .output()
            .unwrap();
        assert!(out.status.success());
    }
    let report_path = sb.path("p.md");
    let report = json(&sb.pm(&[
        "import",
        "vault",
        vault.to_str().unwrap(),
        "--report",
        report_path.to_str().unwrap(),
        "--json",
    ]));
    assert_eq!(report["tickets"]["created"], 7);
    assert_eq!(report["parity"]["unexplained"], 0, "{}", read(&report_path));
    assert_eq!(
        sb.show("AGT-10")["created"]["wall_ms"],
        SEP_1 - 31 * DAY,
        "2026-08-01"
    );

    // Re-import a changed priority: the LWW write is stamped after the
    // log's newest op and takes effect; the file's dates still read back.
    let p = vault.join("tickets/done/AGT-15.md");
    std::fs::write(&p, read(&p).replace("priority: high", "priority: low")).unwrap();
    let report = json(&sb.pm(&[
        "import",
        "vault",
        vault.to_str().unwrap(),
        "--report",
        report_path.to_str().unwrap(),
        "--json",
    ]));
    assert_eq!(report["tickets"]["changed"], 1);
    assert_eq!(sb.show("AGT-15")["priority"], "low");
    assert_eq!(report["parity"]["unexplained"], 0, "{}", read(&report_path));
    assert_eq!(
        report["parity"]["explained"]["updated: re-imported change (stamped at import time)"],
        1
    );
}

#[test]
fn comments_keep_their_own_dates_before_created_and_after_updated() {
    let sb = Sandbox::initialized();
    let vault = sb.path("vault");
    write_vault(
        &vault,
        &[(
            "tickets/triage/AGT-40-re-filed-with-its-history.md",
            "---\nid: AGT-40\ntitle: Re-filed with its history\nstate: triage\ncreated: 2026-09-05\nupdated: 2026-09-05\nproject: \nrepo: \nblocked-by: []\nlinked-github: \nlinked-pr: \npriority: medium\nlabels: []\nsource: { type: manual, url: \"\", id: \"\", fetched-at: \"\" }\n---\n\n## Problem Statement\n\nP.\n\n## Comments\n\n### 2026-09-10 — Later\nAdded without bumping updated.\n\n### 2026-09-01 — Carried over\nFrom the original filing, before this file existed.\n\n### 2026-09-05 — Filed\nRe-filed.\n",
        )],
    );
    let report = json(&sb.pm(&[
        "import",
        "vault",
        vault.to_str().unwrap(),
        "--report",
        sb.path("p.md").to_str().unwrap(),
        "--json",
    ]));
    assert_eq!(report["tickets"]["created"], 1);
    let t = sb.show("AGT-40");
    assert_eq!(
        t["created"]["wall_ms"],
        SEP_1 + 4 * DAY,
        "created stays 09-05"
    );
    assert_eq!(
        t["updated"]["wall_ms"],
        SEP_1 + 9 * DAY,
        "the last op is the 09-10 comment"
    );
    let store = pm_store::Store::open(sb.ws.join("pm.sqlite")).unwrap();
    let ulid: ulid::Ulid = t["ulid"].as_str().unwrap().parse().unwrap();
    let dates: Vec<u64> = store
        .comments(ulid)
        .unwrap()
        .iter()
        .map(|c| c.hlc.wall_ms)
        .collect();
    assert_eq!(
        dates,
        [SEP_1, SEP_1 + 4 * DAY, SEP_1 + 9 * DAY],
        "each comment carries its entry's date, in date order"
    );
    // The create is the first op in the log even though a comment's
    // stamp is older; the database replays cleanly.
    let ops = store.ops(ulid).unwrap();
    assert_eq!(ops[0].kind(), "ticket.create");
    assert!(
        ops.iter()
            .any(|o| o.kind() == "comment.add" && o.hlc < ops[0].hlc)
    );
    assert_ok(&sb.pm(&["doctor"]));
    assert_ok(&sb.pm(&["doctor", "--rebuild"]));

    // The export gives the dates back; the parity report explains the
    // only two differences.
    let dir = sb.path("out");
    sb.export(&dir, true);
    let text = read(&find(&dir.join("tickets/triage"), "AGT-40-"));
    assert!(text.contains("\nupdated: 2026-09-10\n"));
    assert!(text.ends_with("## Comments\n\n### 2026-09-01 — Carried over\nFrom the original filing, before this file existed.\n\n### 2026-09-05 — Filed\nRe-filed.\n\n### 2026-09-10 — Later\nAdded without bumping updated.\n"), "{text}");
    let parity = &report["parity"];
    assert_eq!(parity["unexplained"], 0, "{parity}");
    assert_eq!(
        parity["explained"],
        json!({
            "body text (parses equal)": 1,
            "comments: reordered by date": 1,
            "updated: stale (predates last comment)": 1,
        }),
        "{parity}"
    );
}
