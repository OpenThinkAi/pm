//! `pm import vault` driven through the built binary (AGT-1347), against
//! the fixture vault in `tests/fixtures/vault` — a miniature of
//! ~/saltline-digital-vault with one file per key order, marker idiom and
//! anomaly the real one has (projects/pm/research/vault-anatomy.md). The
//! live vault is never read here. Sandbox as in `tests/cli.rs`: temp HOME,
//! cleared env, no stdin.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use pm_store::Store;
use serde_json::{Value, json};
use tempfile::TempDir;

const FIXTURE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/vault");

/// 2026-08-01T00:00Z, AGT-10's `created`.
const AUG_1: u64 = 1_785_542_400_000;
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
        assert_ok(&sb.pm(&[
            "init",
            "--prefix",
            "AGT",
            "--preset",
            "saltline",
            "--workspace",
            sb.ws_str(),
        ]));
        sb
    }

    fn ws_str(&self) -> &str {
        self.ws.to_str().unwrap()
    }

    fn pm(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_pm"))
            .args(args)
            .env_clear()
            .env("HOME", self.home.path())
            .env("USER", "tester")
            // `--recover` shells out to `git`.
            .env("PATH", std::env::var("PATH").unwrap_or_default())
            .stdin(Stdio::null())
            .output()
            .unwrap()
    }

    fn store(&self) -> Store {
        Store::open(self.ws.join("pm.sqlite")).unwrap()
    }

    fn import(&self, vault: &Path) -> Value {
        json(&self.pm(&["import", "vault", vault.to_str().unwrap(), "--json"]))
    }

    fn show(&self, id: &str) -> Value {
        json(&self.pm(&["show", id, "--json"]))
    }

    fn project(&self, id: &str) -> Value {
        json(&self.pm(&["project", "show", id, "--json"]))
    }

    /// A private, writable copy of the fixture vault.
    fn vault_copy(&self) -> PathBuf {
        let dst = self.home.path().join("vault");
        copy_dir(Path::new(FIXTURE), &dst);
        dst
    }
}

fn copy_dir(src: &Path, dst: &Path) {
    std::fs::create_dir_all(dst).unwrap();
    for entry in std::fs::read_dir(src).unwrap() {
        let entry = entry.unwrap();
        let to = dst.join(entry.file_name());
        if entry.path().is_dir() {
            copy_dir(&entry.path(), &to);
        } else {
            std::fs::copy(entry.path(), to).unwrap();
        }
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

fn assert_code(out: &Output, code: i32) {
    assert_eq!(
        out.status.code(),
        Some(code),
        "stdout: {}\nstderr: {}",
        stdout(out),
        stderr(out)
    );
}

fn json(out: &Output) -> Value {
    assert_ok(out);
    serde_json::from_slice(&out.stdout).unwrap()
}

fn strings(v: &Value) -> BTreeSet<String> {
    v.as_array()
        .unwrap()
        .iter()
        .map(|s| s.as_str().unwrap().to_string())
        .collect()
}

fn comments(sb: &Sandbox, id: &str) -> Vec<(String, String)> {
    let ulid: ulid::Ulid = sb.show(id)["ulid"].as_str().unwrap().parse().unwrap();
    sb.store()
        .comments(ulid)
        .unwrap()
        .into_iter()
        .map(|c| (c.author.to_string(), c.body))
        .collect()
}

// -------------------------------------------------------- AC1, AC2, AC6

#[test]
fn imports_every_file_field_comment_and_doc() {
    let sb = Sandbox::initialized();
    let report = sb.import(Path::new(FIXTURE));
    assert_eq!(report["schema"], 1);
    assert_eq!(report["files"], 7);
    assert_eq!(
        report["tickets"],
        json!({"created": 7, "changed": 0, "unchanged": 0, "skipped": 0})
    );
    assert_eq!(report["archived"], 3);
    assert_eq!(report["projects"], 4);
    assert_eq!(report["project_stubs"], json!([]));
    assert_eq!(
        report["docs"],
        json!({"created": 6, "updated": 0, "unchanged": 0, "skipped": []})
    );
    assert_eq!(report["max_number"], 15);
    assert_eq!(report["number_floor"], 15);
    assert_eq!(report["comments"], 8);
    assert_eq!(
        report["markers"],
        json!({"hold": 2, "parked": 2, "waiver": 3})
    );
    assert_eq!(report["renumbered"], json!(["AGT-16 (was AGT-14)"]));

    // AC6: every non-template value is listed, naming path:line.
    let nt = &report["non_template"];
    assert_eq!(
        nt["priority: critical"],
        json!(["archive/2026-07/AGT-13-isolate-ci.md:13"])
    );
    assert_eq!(
        nt["source.type: audit"],
        json!(["archive/2026-07/AGT-13-isolate-ci.md:15"])
    );
    assert_eq!(
        nt["updated: ISO datetime"],
        json!(["archive/2026-07/AGT-13-isolate-ci.md:7"])
    );
    assert_eq!(
        nt["linked-pr: quoted"],
        json!(["tickets/done/AGT-11-deps-pass.md:12"])
    );
    assert_eq!(
        nt["frontmatter: bare value containing ': '"],
        json!(["tickets/done/AGT-11-deps-pass.md:3"])
    );
    assert_eq!(
        nt["project status: active"],
        json!(["projects/alpha/README.md"])
    );
    assert_eq!(
        nt["project status: shipped"],
        json!(["archive/projects/old-audit/README.md"])
    );
    assert_eq!(
        report["ext_keys"],
        json!({"team": 3, "resolution": 1, "pr-head-sha": 1, "pr-head-ref": 1})
    );

    // AGT-10: a `waived:` inside backticks spanning three lines, in a
    // comment; the line is gone from the comment, the waiver is a field.
    let t = sb.show("AGT-10");
    assert_eq!(t["state"], "triage");
    assert_eq!(t["project"], Value::Null);
    assert_eq!(t["repo"], "shallow-alchemy/budget-cli");
    assert_eq!(
        t["waivers"],
        json!([{"rule": "standalone", "reason": "no live project; the old one is archived and no replacement exists. Self-contained; not worth a project folder for one ticket."}])
    );
    assert_eq!(t["created"]["wall_ms"], AUG_1);
    assert_eq!(t["updated"]["wall_ms"], AUG_1);
    assert_eq!(
        comments(&sb, "AGT-10"),
        [(
            "Filed (re-file of AGT-5)".to_string(),
            "Re-filed fresh per the archival note.".to_string()
        )]
    );
    let desc = t["description"].as_str().unwrap();
    assert!(desc.starts_with("## Problem Statement\n\nImporting still requires a hand-run command.\n\n## Acceptance Criteria"), "{desc}");
    assert!(
        !desc.contains("Comments") && !desc.contains("waived"),
        "{desc}"
    );

    // AGT-11: legacy key order, bare title with `: ` and ` #`, quoted
    // linked-pr, `resolution`/`team` → ext, a NEEDS-HUMAN AC item → hold,
    // a mid-line prose `parked:` → parked forever + ext.parked_reason.
    let t = sb.show("AGT-11");
    assert_eq!(
        t["title"],
        "deps: security dependency pass for think-cli (dependabot PRs #62, #92)"
    );
    assert_eq!(t["state"], "done");
    assert_eq!(t["priority"], "high");
    assert_eq!(t["project"], "alpha");
    assert_eq!(
        t["linked_pr"],
        "https://github.com/OpenThinkAi/think-cli/pull/99"
    );
    assert_eq!(
        t["linked_github"],
        "https://github.com/OpenThinkAi/think-cli/issues/62"
    );
    assert_eq!(t["source"]["type"], "github");
    assert_eq!(t["source"]["fetched_at"], "2026-08-02T10:00:00Z");
    assert_eq!(
        strings(&t["labels"]),
        ["deps", "model:sonnet-5"].map(String::from).into()
    );
    assert_eq!(
        t["ext"],
        json!({"resolution": "merged", "team": "engineering", "parked_reason": "until AGT-10 is done."})
    );
    assert_eq!(
        t["hold"]["reason"],
        "Matt approves the changelog before it is published."
    );
    assert_eq!(t["hold"]["by"], "import");
    assert_eq!(t["parked"], json!({"until": "forever"}));
    assert_eq!(t["updated"]["wall_ms"], AUG_1 + 4 * DAY);
    assert_eq!(t["archived_at"], Value::Null);
    let desc = t["description"].as_str().unwrap();
    assert_eq!(
        desc,
        "## Problem Statement\n\nBumps are pending.\n\n## Acceptance Criteria\n\n1. All bumps land.\n\n## Implementation notes\n\n- Keep the flag name.\n- Deliberately blocked on the wave."
    );
    assert_eq!(
        comments(&sb, "AGT-11"),
        [
            ("Engineering agent (spike)".to_string(), "Looked at the diff.".to_string()),
            (
                "Shipped".to_string(),
                "Merged as abc123.\n\n### Shipped 2026-08-05\nNot a dated entry: stays with the comment above.".to_string()
            ),
        ]
    );

    // AGT-12: blocked-by two tickets, `waiting-human:` → hold, a bullet
    // `waived:` in backticks, ext keys after `source`, a section after
    // Comments kept, and a marker-only comment kept verbatim.
    let t = sb.show("AGT-12");
    assert_eq!(t["state"], "in-progress");
    assert_eq!(
        strings(&t["blocked_by"]),
        ["AGT-10", "AGT-11"].map(String::from).into()
    );
    assert_eq!(
        t["waivers"],
        json!([{"rule": "T3", "reason": "no single repo; touches the vault and launchd."}])
    );
    assert_eq!(
        t["hold"]["reason"],
        "install the plist, then record the first run here and move the ticket to done."
    );
    assert_eq!(
        t["ext"],
        json!({"pr-head-sha": "1234567", "pr-head-ref": "agt-12", "team": "product"})
    );
    let desc = t["description"].as_str().unwrap();
    assert!(
        desc.ends_with("## Outcome\n\nComments are not the last section here."),
        "{desc}"
    );
    assert!(desc.contains("- Slack: reuse the helper."), "{desc}");
    assert!(!desc.contains("waived"), "{desc}");
    assert_eq!(
        comments(&sb, "AGT-12"),
        [
            (
                "Decomposed from alpha via /decompose-into-tickets".to_string(),
                "Design: `projects/alpha/README.md`.".to_string()
            ),
            (
                "Built".to_string(),
                "waiting-human: install the plist, then record the first run here\nand move the ticket to done.".to_string()
            ),
        ]
    );

    // AGT-13: archived (folder month), ISO `updated`, critical, audit
    // source, frontmatter `waived:` key, dated `parked:`.
    let t = sb.show("AGT-13");
    assert_eq!(t["archived_at"]["wall_ms"], AUG_1 - 31 * DAY);
    assert_eq!(t["priority"], "critical");
    assert_eq!(t["source"]["type"], "audit");
    assert_eq!(t["project"], "old-audit");
    assert_eq!(
        t["updated"]["wall_ms"],
        AUG_1 - 20 * DAY + 21 * 3_600_000 + 17 * 60_000,
        "2026-07-12T21:17:00.000Z"
    );
    assert_eq!(
        t["waivers"],
        json!([{"rule": "standalone", "reason": "repo-health fix, not part of a product project"}])
    );
    assert_eq!(t["parked"], json!({"until": "2026-11-01"}));
    assert_eq!(t["ext"], json!({"team": "qa"}));
    assert!(!t["description"].as_str().unwrap().contains("parked"));

    // AC4: the duplicate id — the earlier-created file keeps AGT-14, the
    // other gets a fresh number and remembers what it duplicated.
    let t = sb.show("AGT-14");
    assert_eq!(
        t["title"],
        "Outreach assist state persists across panel reloads"
    );
    assert_eq!(t["ext"], json!({}));
    let dup = sb.show("AGT-16");
    assert_eq!(
        dup["title"],
        "Claim-flow UX polish — professional gate/confirmation design"
    );
    assert_eq!(dup["ext"], json!({"duplicate_of_number": "AGT-14"}));
    assert_eq!(dup["state"], "done");
    assert!(dup["archived_at"].is_object());
    let anomalies = report["anomalies"].as_array().unwrap();
    assert!(
        anomalies.iter().any(|a| a.as_str().unwrap().starts_with(
            "archive/2026-08/AGT-14-claim-flow-polish.md: duplicate id AGT-14 (also archive/2026-08/AGT-14-outreach-state.md)"
        )),
        "{anomalies:?}"
    );

    // Slugless AGT-15.md imports normally; its blocker resolves to the
    // canonical AGT-14.
    let t = sb.show("AGT-15");
    assert_eq!(t["title"], "Approve the E2 copy");
    assert_eq!(t["blocked_by"], json!(["AGT-14"]));
    assert_eq!(
        strings(&t["labels"]),
        ["copy", "outreach", "vault-only"].map(String::from).into()
    );
    assert_eq!(comments(&sb, "AGT-15"), []);

    // Projects: READMEs verbatim as the design doc, siblings and
    // ideation as named docs, statuses mapped, parent linked, a retired
    // project imported for R2, a README without frontmatter.
    let alpha = sb.project("alpha");
    assert_eq!(alpha["title"], "Alpha — the first project");
    assert_eq!(alpha["status"], "in-progress");
    assert_eq!(
        alpha["repos"],
        json!(["OpenThinkAi/alpha", "OpenThinkAi/think-cli"])
    );
    assert_eq!(
        alpha["doc"],
        std::fs::read_to_string(Path::new(FIXTURE).join("projects/alpha/README.md")).unwrap()
    );
    assert_eq!(
        alpha["documents"]
            .as_object()
            .unwrap()
            .keys()
            .collect::<Vec<_>>(),
        ["EXECUTION", "ideation/IDEA-001-first-idea"]
    );
    assert_eq!(
        alpha["documents"]["EXECUTION"],
        "# Execution notes\n\nStep one.\n"
    );
    assert_eq!(sb.project("beta")["title"], "beta — no frontmatter at all");
    assert_eq!(sb.project("gamma")["parent"], "alpha");
    assert_eq!(sb.project("old-audit")["status"], "complete");

    // Every op is dated from the file and by the import actor except
    // comments, which keep their vault author and their entry's own date
    // (AGT-1348: a comment is a dated log entry, so it may be stamped
    // before or after the record's ops); the record's ops are
    // HLC-ordered per ticket, the create first.
    let store = sb.store();
    let ulid: ulid::Ulid = sb.show("AGT-12")["ulid"].as_str().unwrap().parse().unwrap();
    let ops = store.ops(ulid).unwrap();
    assert_eq!(ops[0].kind(), "ticket.create");
    assert_eq!(ops[0].hlc.wall_ms, AUG_1 + 37 * DAY, "2026-09-07");
    let record: Vec<&pm_core::Op> = ops.iter().filter(|o| o.kind() != "comment.add").collect();
    assert!(
        record.windows(2).all(|w| w[0].hlc < w[1].hlc),
        "record ops must be HLC-ordered"
    );
    let dates: Vec<u64> = ops
        .iter()
        .filter(|o| o.kind() == "comment.add")
        .map(|o| o.hlc.wall_ms)
        .collect();
    assert_eq!(
        dates,
        [AUG_1 + 37 * DAY, AUG_1 + 38 * DAY],
        "each entry's own date"
    );
    for op in &ops {
        match op.kind() {
            "comment.add" => assert_ne!(op.actor.as_str(), "import"),
            _ => assert_eq!(op.actor.as_str(), "import"),
        }
    }
    // The hold's `at` is its op's HLC.
    let hold_op = ops.iter().find(|o| o.kind() == "hold.set").unwrap();
    assert_eq!(
        sb.show("AGT-12")["hold"]["at"]["wall_ms"],
        hold_op.hlc.wall_ms
    );

    assert_ok(&sb.pm(&["doctor"]));
    // R1 fires only for AGT-15 (no project, no waiver); AGT-10 is waived.
    let check = sb.pm(&["check", "--json"]);
    let v: Value = serde_json::from_slice(&check.stdout).unwrap();
    let r1: Vec<&str> = v["findings"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|f| f["rule"] == "R1")
        .map(|f| f["tickets"][0].as_str().unwrap())
        .collect();
    assert_eq!(r1, ["AGT-15"]);

    // AC5: the allocator continues above the vault's numbers.
    let out = json(&sb.pm(&["new", "--title", "after import", "--json"]));
    assert_eq!(out["id"], "AGT-17");
}

// ------------------------------------------------------------------ AC5

#[test]
fn re_import_of_an_unchanged_vault_commits_nothing() {
    let sb = Sandbox::initialized();
    sb.import(Path::new(FIXTURE));
    let before = sb.store().doctor().unwrap().op_count;
    let report = sb.import(Path::new(FIXTURE));
    assert_eq!(report["ops"], 0);
    assert_eq!(report["tickets"]["unchanged"], 7);
    assert_eq!(report["docs"]["unchanged"], 6);
    assert_eq!(report["renumbered"], json!([]));
    assert_eq!(sb.store().doctor().unwrap().op_count, before);
    // The duplicate is still matched to its renumbered ticket, not re-created.
    assert_eq!(sb.show("AGT-16")["ext"]["duplicate_of_number"], "AGT-14");
}

#[test]
fn re_import_emits_only_the_ops_for_what_changed() {
    let sb = Sandbox::initialized();
    let vault = sb.vault_copy();
    sb.import(&vault);
    let before = sb.store().doctor().unwrap().op_count;

    // A folder move (triage → done) with the state field to match.
    let from = vault.join("tickets/triage/AGT-10-auto-drain-inbox.md");
    let text = std::fs::read_to_string(&from)
        .unwrap()
        .replace("state: triage", "state: done");
    std::fs::remove_file(&from).unwrap();
    std::fs::write(vault.join("tickets/done/AGT-10-auto-drain-inbox.md"), text).unwrap();
    // An edited field.
    let p = vault.join("tickets/done/AGT-11-deps-pass.md");
    std::fs::write(
        &p,
        std::fs::read_to_string(&p)
            .unwrap()
            .replace("priority: high", "priority: low"),
    )
    .unwrap();
    // A new comment and a new marker on a ticket without a Comments section.
    let p = vault.join("tickets/done/AGT-15.md");
    let mut text = std::fs::read_to_string(&p).unwrap();
    text.push_str("\n- `waived: standalone — approval record only`\n\n## Comments\n\n### 2026-09-28 — Matt\nApproved.\n");
    std::fs::write(&p, text).unwrap();
    // Labels, body and blockers on one ticket.
    let p = vault.join("tickets/in-progress/AGT-12-weekly-pass.md");
    let text = std::fs::read_to_string(&p)
        .unwrap()
        .replace("labels: [model:sonnet-5]", "labels: [model:opus-5]")
        .replace("blocked-by: [AGT-11, AGT-10]", "blocked-by: [AGT-11]")
        .replace("Nothing puts", "Nothing ever puts");
    std::fs::write(&p, text).unwrap();

    let report = sb.import(&vault);
    assert_eq!(
        report["tickets"],
        json!({"created": 0, "changed": 4, "unchanged": 3, "skipped": 0})
    );
    assert_eq!(
        report["ops_by_kind"],
        json!({
            "state.transition": 1,
            "field.set": 2,
            "comment.add": 1,
            "label.add": 1,
            "label.remove": 1,
            "body.edit": 1,
            "relation.remove": 1,
        })
    );
    assert_eq!(
        report["changes"],
        json!([
            "AGT-10: state",
            "AGT-11: priority",
            "AGT-12: label.add, label.remove, body, blocked-by",
            "AGT-15: waivers, comment",
        ])
    );
    assert_eq!(sb.store().doctor().unwrap().op_count, before + 8);

    assert_eq!(sb.show("AGT-10")["state"], "done");
    assert_eq!(sb.show("AGT-11")["priority"], "low");
    let t = sb.show("AGT-12");
    assert_eq!(
        strings(&t["labels"]),
        ["model:opus-5"].map(String::from).into()
    );
    assert_eq!(t["blocked_by"], json!(["AGT-11"]));
    assert!(
        t["description"]
            .as_str()
            .unwrap()
            .contains("Nothing ever puts")
    );
    // A concurrent edit elsewhere in the body would have merged: this is
    // a CRDT diff against the ticket's own history, not a replacement.
    let t = sb.show("AGT-15");
    assert_eq!(
        t["waivers"],
        json!([{"rule": "standalone", "reason": "approval record only"}])
    );
    assert_eq!(
        comments(&sb, "AGT-15"),
        [("Matt".to_string(), "Approved.".to_string())]
    );
    assert!(!t["description"].as_str().unwrap().contains("waived"));

    // And once more: nothing.
    let report = sb.import(&vault);
    assert_eq!(report["ops"], 0);
    assert_ok(&sb.pm(&["doctor"]));
}

/// AGT-1381: a comment an incremental import appends must sort after
/// every existing same-day comment, not before it. Reproduced as a
/// fixture (the frozen real vault can't be re-imported live): two
/// same-day comments land on the first import, a third same-day comment
/// is appended to the file, and a re-import must read them back in file
/// order — with the parity report showing 0 unexplained.
#[test]
fn a_same_day_comment_appended_by_a_later_import_sorts_after_the_existing_ones() {
    let sb = Sandbox::initialized();
    let vault = sb.vault_copy();
    let rel = "tickets/triage/AGT-21-comment-order.md";
    let ticket = |comments: &str| {
        format!(
            "---\n\
             id: AGT-21\n\
             title: Comment order across incremental imports\n\
             state: triage\n\
             created: 2026-09-01\n\
             updated: 2026-09-28\n\
             project: alpha\n\
             repo: \n\
             blocked-by: []\n\
             linked-github: \n\
             linked-pr: \n\
             priority: low\n\
             labels: []\n\
             source: {{ type: manual, url: \"\", id: \"\", fetched-at: \"\" }}\n\
             ---\n\
             \n\
             ## Problem Statement\n\
             \n\
             P.\n\
             \n\
             ## Comments\n\
             \n\
             {comments}"
        )
    };
    std::fs::write(
        vault.join(rel),
        ticket(
            "### 2026-09-28 — Alice\n\
             First same-day comment.\n\
             \n\
             ### 2026-09-28 — Bob\n\
             Second same-day comment.\n",
        ),
    )
    .unwrap();
    let report = sb.import(&vault);
    assert_eq!(report["tickets"]["created"], 8, "{report}");
    assert_eq!(
        comments(&sb, "AGT-21"),
        [
            ("Alice".to_string(), "First same-day comment.".to_string()),
            ("Bob".to_string(), "Second same-day comment.".to_string()),
        ]
    );

    // Append a third same-day comment, as a live incremental re-import
    // would see after the vault gained a new entry for the same day.
    std::fs::write(
        vault.join(rel),
        ticket(
            "### 2026-09-28 — Alice\n\
             First same-day comment.\n\
             \n\
             ### 2026-09-28 — Bob\n\
             Second same-day comment.\n\
             \n\
             ### 2026-09-28 — Carol\n\
             Third same-day comment, appended later.\n",
        ),
    )
    .unwrap();
    let report_path = sb.home.path().join("parity.md");
    let report = json(&sb.pm(&[
        "import",
        "vault",
        vault.to_str().unwrap(),
        "--report",
        report_path.to_str().unwrap(),
        "--json",
    ]));
    assert_eq!(report["changes"], json!(["AGT-21: comment"]), "{report}");

    // pm's order matches the file's order — the bug appended Carol
    // ahead of Alice and Bob because the day's counter restarted at 0.
    assert_eq!(
        comments(&sb, "AGT-21"),
        [
            ("Alice".to_string(), "First same-day comment.".to_string()),
            ("Bob".to_string(), "Second same-day comment.".to_string()),
            (
                "Carol".to_string(),
                "Third same-day comment, appended later.".to_string()
            ),
        ]
    );

    // The HLCs themselves are strictly increasing: Carol's stamp for the
    // shared day continues past Bob's rather than resetting to 0.
    let ulid: ulid::Ulid = sb.show("AGT-21")["ulid"].as_str().unwrap().parse().unwrap();
    let hlcs: Vec<_> = sb
        .store()
        .comments(ulid)
        .unwrap()
        .into_iter()
        .map(|c| c.hlc)
        .collect();
    assert!(
        hlcs.windows(2).all(|w| w[0] < w[1]),
        "comment stamps not increasing: {hlcs:?}"
    );
    assert_eq!(hlcs[0].wall_ms, hlcs[1].wall_ms);
    assert_eq!(hlcs[1].wall_ms, hlcs[2].wall_ms);

    // The parity report has nothing unexplained: file order and pm's
    // order agree exactly.
    assert_eq!(report["parity"]["unexplained"], 0, "{report}");
}

// ------------------------------------------------------------- errors

#[test]
fn a_parse_error_names_the_file_and_line_and_writes_nothing() {
    let sb = Sandbox::initialized();
    let vault = sb.vault_copy();
    let bad = vault.join("tickets/triage/AGT-99-bad.md");

    std::fs::write(&bad, "---\ntitle: no id\nstate: triage\n---\n\nbody\n").unwrap();
    let out = sb.pm(&["import", "vault", vault.to_str().unwrap()]);
    assert_code(&out, 2);
    assert!(
        stderr(&out).contains("tickets/triage/AGT-99-bad.md:1: missing id"),
        "{}",
        stderr(&out)
    );

    std::fs::write(&bad, "---\nid: AGT-99\ntitle: t\nstate: triage\ncreated: 2026-09-28\nupdated: 2026-09-28\nlabels: [a, b\n---\n").unwrap();
    let out = sb.pm(&["import", "vault", vault.to_str().unwrap()]);
    assert_code(&out, 2);
    assert!(
        stderr(&out).contains("tickets/triage/AGT-99-bad.md:7: parsing frontmatter"),
        "{}",
        stderr(&out)
    );

    std::fs::write(
        &bad,
        "---\nid: AGT-99\ntitle: t\nstate: qa\ncreated: 2026-09-28\nupdated: 2026-09-28\n---\n",
    )
    .unwrap();
    let out = sb.pm(&["import", "vault", vault.to_str().unwrap()]);
    assert_code(&out, 2);
    assert!(
        // AGT-1518: the pre-flight reports it (every unknown state, with
        // counts) before any file is read in full.
        stderr(&out).contains("ticket state(s) this workspace lacks")
            && stderr(&out).contains("  qa (1 ticket)"),
        "{}",
        stderr(&out)
    );

    std::fs::write(&bad, "\n### 2026-09-28 — Stray\nclobbered\n").unwrap();
    let out = sb.pm(&["import", "vault", vault.to_str().unwrap()]);
    assert_code(&out, 2);
    assert!(
        stderr(&out).contains("AGT-99-bad.md:1: file has no frontmatter block"),
        "{}",
        stderr(&out)
    );

    assert!(
        sb.store().all_tickets().unwrap().is_empty(),
        "a parse error writes nothing"
    );
    assert!(sb.store().projects().unwrap().is_empty());

    let out = sb.pm(&["import", "vault", sb.home.path().to_str().unwrap()]);
    assert_code(&out, 2);
    assert!(stderr(&out).contains("no tickets/ directory"));
}

#[test]
fn dry_run_reports_without_writing() {
    let sb = Sandbox::initialized();
    let report = json(&sb.pm(&["import", "vault", FIXTURE, "--dry-run", "--json"]));
    assert_eq!(report["dry_run"], true);
    assert_eq!(report["tickets"]["created"], 7);
    assert_eq!(report["docs"]["created"], 6);
    assert!(report["ops"].as_u64().unwrap() > 0);
    assert!(sb.store().all_tickets().unwrap().is_empty());
    assert!(sb.store().projects().unwrap().is_empty());
    let human = stdout(&sb.pm(&["import", "vault", FIXTURE, "--dry-run"]));
    assert!(human.contains("(dry run: nothing written)"), "{human}");
    assert!(human.contains("migrated markers (7):"), "{human}");
}

// -------------------------------------------------------------- AC4

/// AGT-806's shape: a file that lost its frontmatter is read from the
/// git object that last had it, with the stray comment the working tree
/// still holds appended. The built-in table names the real vault's file;
/// `--recover` covers this one.
#[test]
fn recovers_a_clobbered_file_from_a_git_object() {
    if Command::new("git").arg("--version").output().is_err() {
        eprintln!("git not available; skipping");
        return;
    }
    let sb = Sandbox::initialized();
    let vault = sb.vault_copy();
    let git = |args: &[&str]| {
        let out = Command::new("git")
            .args([
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@example.com",
                "-c",
                "commit.gpgsign=false",
            ])
            .args(args)
            .current_dir(&vault)
            .env("HOME", sb.home.path())
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap()
    };
    git(&["init", "-q"]);
    let rel = "tickets/triage/AGT-20-good.md";
    std::fs::write(
        vault.join(rel),
        "---\nid: AGT-20\ntitle: Recovered from git\nstate: triage\ncreated: 2026-07-28\nupdated: 2026-07-28\nproject: alpha\nrepo: \nblocked-by: []\nlinked-github: \nlinked-pr: \npriority: high\nlabels: []\nsource: { type: manual, url: \"\", id: \"\", fetched-at: \"\" }\n---\n\n## Problem Statement\n\nP.\n\n## Comments\n\n### 2026-07-28 — Found\nEvidence.\n",
    )
    .unwrap();
    git(&["add", "-A"]);
    git(&["commit", "-q", "-m", "good"]);
    let sha = git(&["rev-parse", "--short", "HEAD"]).trim().to_string();
    std::fs::write(
        vault.join(rel),
        "\n### 2026-07-27 — The fix must update the record\nStray note.\n",
    )
    .unwrap();

    let out = sb.pm(&["import", "vault", vault.to_str().unwrap()]);
    assert_code(&out, 2);
    assert!(stderr(&out).contains("AGT-20-good.md:1: file has no frontmatter block"));

    let report = json(&sb.pm(&[
        "import",
        "vault",
        vault.to_str().unwrap(),
        "--recover",
        &format!("{rel}={sha}"),
        "--json",
    ]));
    assert_eq!(report["tickets"]["created"], 8);
    let t = sb.show("AGT-20");
    assert_eq!(t["title"], "Recovered from git");
    assert_eq!(t["project"], "alpha");
    // The fragment's entry is dated 07-27, the recovered file's 07-28:
    // comments read in date order (AGT-1348).
    assert_eq!(
        comments(&sb, "AGT-20"),
        [
            (
                "The fix must update the record".to_string(),
                "Stray note.".to_string()
            ),
            ("Found".to_string(), "Evidence.".to_string()),
        ]
    );
    assert!(report["anomalies"].as_array().unwrap().iter().any(|a| {
        a.as_str()
            .unwrap()
            .contains(&format!("imported from git object {sha}"))
    }));

    let out = sb.pm(&[
        "import",
        "vault",
        vault.to_str().unwrap(),
        "--recover",
        "nope",
    ]);
    assert_code(&out, 2);
    assert!(stderr(&out).contains("PATH=REV"));
}

// ------------------------------------------------------------ AGT-1406

/// `docs_owned_by = pm`: projects and tickets still import, but no
/// README or sibling doc is read into pm; each is reported as skipped.
#[test]
fn docs_owned_by_pm_skips_project_docs_and_reports_them() {
    let sb = Sandbox::initialized();
    let shown = |out: &Output| stdout(out).trim().to_string();
    assert_eq!(shown(&sb.pm(&["workspace", "docs-owned-by"])), "vault");
    assert_ok(&sb.pm(&["workspace", "docs-owned-by", "pm"]));
    assert_eq!(shown(&sb.pm(&["workspace", "docs-owned-by"])), "pm");

    let report = sb.import(Path::new(FIXTURE));
    assert_eq!(report["tickets"]["created"], 7);
    assert_eq!(report["projects"], 4);
    let docs = &report["docs"];
    assert_eq!(docs["created"], 0);
    assert_eq!(docs["updated"], 0);
    assert_eq!(docs["unchanged"], 0);
    let skipped = strings(&docs["skipped"]);
    assert_eq!(skipped.len(), 6, "{skipped:?}");
    assert!(skipped.iter().all(|p| p.starts_with("projects/")));
    assert!(skipped.contains("projects/alpha/README.md"), "{skipped:?}");
    // Metadata and tickets landed; document text did not.
    let alpha = sb.project("alpha");
    assert_eq!(alpha["id"], "alpha");
    assert!(sb.store().project("alpha").unwrap().unwrap().doc.is_empty());
    assert_eq!(sb.store().all_tickets().unwrap().len(), 7);

    let human = stdout(&sb.pm(&["import", "vault", FIXTURE]));
    assert!(human.contains("6 skipped (owned by pm)"), "{human}");
    assert!(human.contains("projects/alpha/README.md"), "{human}");

    // Flipping back resumes refreshing the docs.
    assert_ok(&sb.pm(&["workspace", "docs-owned-by", "vault"]));
    let report = sb.import(Path::new(FIXTURE));
    assert_eq!(report["docs"]["created"], 6);
    assert_eq!(report["docs"]["skipped"], json!([]));

    // And a bad value is a usage error.
    assert_code(&sb.pm(&["workspace", "docs-owned-by", "nobody"]), 2);
    assert_ok(&sb.pm(&["doctor"]));
    assert_ok(&sb.pm(&["doctor", "--rebuild"]));
    assert_eq!(shown(&sb.pm(&["workspace", "docs-owned-by"])), "vault");
}

/// An owner set with `pm` survives `doctor --rebuild`: the column is
/// re-derived from the `workspace.set` op.
#[test]
fn docs_owned_by_survives_doctor_rebuild() {
    let sb = Sandbox::initialized();
    assert_ok(&sb.pm(&["workspace", "docs-owned-by", "pm"]));
    assert_ok(&sb.pm(&["doctor", "--rebuild"]));
    let v = json(&sb.pm(&["workspace", "docs-owned-by", "--json"]));
    assert_eq!(v["docs_owned_by"], "pm");
    assert_ok(&sb.pm(&["doctor"]));
}

// ------------------------------------------------------------ AGT-1485

/// Nested markdown under a project folder (and a retired one) imports as
/// named docs named by relative path; the resource subtrees produce
/// neither docs nor anomalies; non-markdown files are reported, never
/// imported; `ideation/` keeps its `IDEA-*` rule.
#[test]
fn nested_project_docs_import_by_relative_path() {
    let sb = Sandbox::initialized();
    let vault = sb.vault_copy();
    let write = |rel: &str, text: &str| {
        let path = vault.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    };
    write("projects/alpha/sub/x.md", "# x\n");
    write("projects/alpha/cutover/README.md", "# cutover\n");
    write("projects/alpha/deep/er/y.md", "# y\n");
    write("projects/alpha/sub/run.sh", "echo hi\n");
    write("projects/alpha/plan.json", "{}\n");
    write("projects/alpha/.hidden/z.md", "# hidden\n");
    write("projects/alpha/research/notes.md", "# research\n");
    write("projects/alpha/research/data.json", "{}\n");
    write("projects/alpha/design-qa/AGT-1/qa.md", "# qa\n");
    write("projects/alpha/design-qa/AGT-1/shot.png", "png");
    write("projects/alpha/assets/logo.png", "png");
    write("projects/alpha/ideation/notes.md", "# not an idea\n");
    write("archive/projects/old-audit/drafts/d.md", "# draft\n");
    // A symlinked folder (here a cycle back to the project) is not followed.
    #[cfg(unix)]
    std::os::unix::fs::symlink(
        vault.join("projects/alpha"),
        vault.join("projects/alpha/loop"),
    )
    .unwrap();

    let report = sb.import(&vault);
    assert_eq!(report["docs"]["created"], 10, "{report}");

    let alpha = sb.project("alpha");
    let names: Vec<&String> = alpha["documents"].as_object().unwrap().keys().collect();
    assert_eq!(
        names,
        [
            "EXECUTION",
            "cutover/README",
            "deep/er/y",
            "ideation/IDEA-001-first-idea",
            "sub/x"
        ]
    );
    assert_eq!(alpha["documents"]["sub/x"], "# x\n");
    let old = sb.project("old-audit");
    assert_eq!(old["documents"], json!({"drafts/d": "# draft\n"}));

    // One anomaly lists alpha's non-markdown files; nothing names an
    // excluded subtree or the ideation file outside the IDEA-* rule.
    let anomalies: Vec<String> = report["anomalies"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a.as_str().unwrap().to_string())
        .collect();
    let non_md: Vec<&String> = anomalies
        .iter()
        .filter(|a| a.contains("non-markdown"))
        .collect();
    assert_eq!(
        non_md,
        [
            &"projects/alpha: 2 non-markdown file(s) left in the vault, not imported: \
           projects/alpha/plan.json, projects/alpha/sub/run.sh"
                .to_string()
        ]
    );
    for a in &anomalies {
        for noise in ["research", "design-qa", "assets", "ideation", ".hidden"] {
            assert!(!a.contains(noise), "{a}");
        }
    }

    // Re-importing the same vault changes nothing.
    let again = sb.import(&vault);
    assert_eq!(again["docs"]["created"], 0);
    assert_eq!(again["docs"]["updated"], 0);

    // Once pm owns the docs (AGT-1406), a nested doc is skipped and
    // reported like any other, and pm's copy is left alone.
    assert_ok(&sb.pm(&["workspace", "docs-owned-by", "pm"]));
    write("projects/alpha/sub/x.md", "# x, edited in the vault\n");
    let owned = sb.import(&vault);
    assert!(
        strings(&owned["docs"]["skipped"]).contains("projects/alpha/sub/x.md"),
        "{owned}"
    );
    assert_eq!(sb.project("alpha")["documents"]["sub/x"], "# x\n");
}

// ------------------------------------------ unknown vault states (AGT-1518)

/// A vault ticket file in `state`, under `tickets/<dir>/`.
fn state_ticket(vault: &Path, number: u32, state: &str, dir: &str, blocked_by: &str) {
    let path = vault
        .join("tickets")
        .join(dir)
        .join(format!("AGT-{number}-t.md"));
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(
        path,
        format!(
            "---\nid: AGT-{number}\ntitle: T{number}\nstate: {state}\ncreated: 2026-08-01\nupdated: 2026-08-15\nblocked-by: [{blocked_by}]\npriority: medium\nlabels: []\n---\n\n## Problem Statement\n\nx\n"
        ),
    )
    .unwrap();
}

/// triage 1, refined 2, qa 1, blocked 2 (one with a blocker), archived 1.
fn seven_state_vault(sb: &Sandbox) -> PathBuf {
    let vault = sb.home.path().join("states-vault");
    state_ticket(&vault, 1, "triage", "triage", "");
    state_ticket(&vault, 2, "refined", "refined", "");
    state_ticket(&vault, 3, "refined", "refined", "");
    state_ticket(&vault, 4, "qa", "qa", "");
    state_ticket(&vault, 5, "blocked", "blocked", "AGT-1");
    state_ticket(&vault, 6, "blocked", "blocked", "");
    state_ticket(&vault, 7, "archived", "archive", "");
    vault
}

#[test]
fn unknown_vault_states_fail_a_preflight_that_lists_every_one_with_counts() {
    let sb = Sandbox::initialized();
    let vault = seven_state_vault(&sb);
    let out = sb.pm(&["import", "vault", vault.to_str().unwrap()]);
    assert_code(&out, 2);
    let err = stderr(&out);
    // Every unknown state, with its count — not just the first file's.
    // `archived` and `blocked` have defaults, so they are not listed.
    assert!(err.contains("qa (1 ticket)"), "{err}");
    assert!(err.contains("refined (2 tickets)"), "{err}");
    assert!(!err.contains("archived ("), "{err}");
    assert!(!err.contains("blocked ("), "{err}");
    assert!(err.contains("pm workspace state add"), "{err}");
    assert!(err.contains("--map-state"), "{err}");
    // Nothing was written.
    assert!(sb.store().ticket_by_number(1).unwrap().is_none());
}

#[test]
fn map_state_and_the_archived_blocked_defaults_import_and_re_import_idempotently() {
    let sb = Sandbox::initialized();
    let vault = seven_state_vault(&sb);
    assert_ok(&sb.pm(&["workspace", "state", "add", "qa", "--category", "started"]));
    let args = [
        "import",
        "vault",
        vault.to_str().unwrap(),
        "--map-state",
        "refined=triage",
        "--json",
    ];
    let report = json(&sb.pm(&args));
    assert_eq!(report["tickets"]["created"], 7);
    assert_eq!(
        report["state_map"],
        serde_json::json!([
            {"vault": "archived", "state": "done", "archive": true, "hold": "never", "default": true, "tickets": 1},
            {"vault": "blocked", "state": "triage", "archive": false, "hold": "no-blockers", "default": true, "tickets": 2},
            {"vault": "refined", "state": "triage", "archive": false, "hold": "never", "default": false, "tickets": 2},
        ])
    );
    assert_eq!(sb.show("AGT-2")["state"], "triage");
    assert_eq!(sb.show("AGT-4")["state"], "qa");
    // archived → done, archived at the first of its `updated` month.
    let archived = sb.show("AGT-7");
    assert_eq!(archived["state"], "done");
    assert_eq!(archived["archived_at"]["wall_ms"], AUG_1);
    // blocked with a blocker: not held (the relation keeps it off ready);
    // blocked with none: held.
    let with_blocker = sb.show("AGT-5");
    assert_eq!(with_blocker["state"], "triage");
    assert!(with_blocker["hold"].is_null());
    let held = sb.show("AGT-6");
    assert_eq!(held["hold"]["reason"], "vault state 'blocked'");
    assert_eq!(held["hold"]["by"], "import");

    // The same flags again: nothing to do.
    let again = json(&sb.pm(&args));
    assert_eq!(again["ops"], 0, "{again}");
    assert_eq!(again["tickets"]["unchanged"], 7);

    // Overriding a default re-imports the affected tickets as changes.
    let changed = json(&sb.pm(&[
        "import",
        "vault",
        vault.to_str().unwrap(),
        "--map-state",
        "refined=triage",
        "--map-state",
        "blocked=qa+hold",
        "--json",
    ]));
    assert_eq!(changed["tickets"]["changed"], 2, "{changed}");
    assert_eq!(sb.show("AGT-5")["state"], "qa");
    assert_eq!(sb.show("AGT-5")["hold"]["reason"], "vault state 'blocked'");
}

#[test]
fn a_bad_map_state_is_a_usage_error() {
    let sb = Sandbox::initialized();
    let vault = seven_state_vault(&sb);
    for bad in [
        "refined",
        "refined=",
        "=triage",
        "refined=nope",
        "refined=triage+later",
    ] {
        let out = sb.pm(&[
            "import",
            "vault",
            vault.to_str().unwrap(),
            "--map-state",
            bad,
        ]);
        assert_code(&out, 2);
    }
    let out = sb.pm(&[
        "import",
        "vault",
        vault.to_str().unwrap(),
        "--map-state",
        "qa=triage",
        "--map-state",
        "qa=done",
    ]);
    assert_code(&out, 2);
    assert!(stderr(&out).contains("twice"));
}

/// AGT-1635 AC3: a vault project whose `status:` is `parked` imports as a
/// parked project, without a "project status" note; re-importing it
/// unchanged commits nothing.
#[test]
fn a_parked_vault_project_imports_as_parked() {
    let sb = Sandbox::initialized();
    let vault = sb.vault_copy();
    let readme = vault.join("projects/gamma/README.md");
    std::fs::write(
        &readme,
        std::fs::read_to_string(&readme)
            .unwrap()
            .replace("status: in-progress", "status: parked"),
    )
    .unwrap();
    let report = sb.import(&vault);
    assert_eq!(sb.project("gamma")["status"], "parked");
    assert!(
        !report.to_string().contains("project status: parked"),
        "{report}"
    );
    let before = sb.store().doctor().unwrap().op_count;
    sb.import(&vault);
    assert_eq!(sb.store().doctor().unwrap().op_count, before);
}
