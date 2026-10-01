//! Every synced field reaches a human-readable output clean (AGT-1468,
//! oaudit r3): actor, assignee, labels, repo, project metadata, markers,
//! model labels, gate labels and database cells carry ESC/OSC/C1/bidi
//! text in crafted ops that are applied the way a pull applies them (not
//! through the CLI, whose own checks would refuse some), then every text
//! command's stdout and stderr is scanned for terminal-steering characters
//! and the clean form of each field is looked for where it is printed.
//! `--json` is not covered: it keeps the stored text.

use std::path::PathBuf;
use std::process::{Command, Output, Stdio};

use pm_core::op::{
    ActorUpsert, Claim, CommentAdd, FieldSet, HoldSet, LabelAdd, ProjectCreate, ProjectSet,
    TicketCreate, WorkspaceSet,
};
use pm_core::{ActorId, ActorKind, Clock, Hlc, Hold, Op, Payload, Priority, ProjectStatus};
use pm_store::Store;
use tempfile::TempDir;
use ulid::Ulid;

const EVIL: &str = "\x1b[2Jx\x1b]0;pwned\x07y\u{9b}31mz\rw\u{202e}v";
const MODEL: &str = "m\x1b[2Jx\x1b]0;pwned\x07y\u{9b}31mz\rw\u{202e}v";
const CLEAN: &str = "[2Jx]0;pwnedy31mzwv";

struct Sandbox {
    home: TempDir,
    ws: PathBuf,
}

impl Sandbox {
    fn new() -> Self {
        let home = tempfile::tempdir().unwrap();
        let ws = home.path().join("ws");
        let sb = Sandbox { home, ws };
        let out = sb.pm(&["init", "--prefix", "AGT", "--preset", "saltline"]);
        assert!(out.status.success(), "{}", text(&out.stderr));
        sb
    }

    fn store(&self) -> Store {
        Store::open(self.ws.join("pm.sqlite")).unwrap()
    }

    fn pm(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_pm"))
            .args(args)
            .arg("--workspace")
            .arg(&self.ws)
            .env_clear()
            .env("HOME", self.home.path())
            .env("USER", "tester")
            .stdin(Stdio::null())
            .output()
            .unwrap()
    }
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

fn no_controls(what: &str, s: &str) {
    for c in s.chars() {
        let bidi = ('\u{202a}'..='\u{202e}').contains(&c) || ('\u{2066}'..='\u{2069}').contains(&c);
        let control = c.is_control() && c != '\n' && c != '\t';
        assert!(!bidi && !control, "{what}: terminal control {c:?} in {s:?}");
    }
}

/// Stamps ops the way a remote replica would, from a fresh clock.
struct Remote {
    clock: Clock,
    actor: ActorId,
}

impl Remote {
    fn new(actor: &str) -> Self {
        Remote {
            clock: Clock::from_latest(Hlc {
                wall_ms: 0,
                counter: 0,
            }),
            actor: ActorId::new(actor),
        }
    }
    fn op(&mut self, entity: Ulid, payload: Payload) -> Op {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        Op::new(
            Ulid::new(),
            self.clock.send(now),
            self.actor.clone(),
            entity,
            payload,
        )
    }
}

#[test]
fn every_synced_field_is_clean_in_text_output() {
    let sb = Sandbox::new();
    let store_ws = sb.store();
    let ws_id = store_ws.workspace().unwrap().unwrap().id;
    drop(store_ws);

    let ticket = Ulid::new();
    let project = Ulid::new();
    let evil = EVIL;
    let mut r = Remote::new(evil);
    let ops = vec![
        r.op(
            ticket,
            Payload::TicketCreate(TicketCreate {
                title: format!("T {evil}"),
                state: "triage".into(),
                priority: Priority::Medium,
                project: None,
                repo: Some(format!("org/{evil}")),
                source: None,
                ext: [(format!("k{evil}"), serde_json::json!(format!("v{evil}")))].into(),
            }),
        ),
        r.op(ticket, Payload::FieldSet(FieldSet::Number(1))),
        r.op(
            ticket,
            Payload::LabelAdd(LabelAdd {
                label: format!("l{evil}"),
            }),
        ),
        r.op(
            ticket,
            Payload::FieldSet(FieldSet::LinkedPr(Some(format!("pr{evil}")))),
        ),
        r.op(
            ticket,
            Payload::FieldSet(FieldSet::Linear(Some(format!("ln{evil}")))),
        ),
        r.op(
            ticket,
            Payload::CommentAdd(CommentAdd {
                body: format!("c {evil}"),
            }),
        ),
        r.op(
            ticket,
            Payload::HoldSet(HoldSet {
                hold: Hold {
                    reason: format!("h {evil}"),
                    by: ActorId::new(evil),
                    at: Hlc {
                        wall_ms: 1,
                        counter: 0,
                    },
                },
            }),
        ),
        r.op(
            ws_id,
            Payload::ActorUpsert(ActorUpsert {
                id: ActorId::new(evil),
                kind: ActorKind::Agent,
            }),
        ),
        r.op(
            ws_id,
            Payload::WorkspaceSet(WorkspaceSet::GateLabelAdd(format!("g{evil}"))),
        ),
        r.op(
            project,
            Payload::ProjectCreate(ProjectCreate {
                kind: Default::default(),
                id: "evilp".into(),
                title: format!("P {evil}"),
                status: ProjectStatus::InProgress,
                parent: None,
                doc_id: Some(Ulid::new()),
            }),
        ),
        r.op(
            project,
            Payload::ProjectSet(ProjectSet::RepoAdd(format!("repo{evil}"))),
        ),
    ];
    let mut store = sb.store();
    store.apply_pulled(&ops).unwrap();

    // A second ticket, started by the hostile actor: assignee + state.
    let claimed = Ulid::new();
    let ops = vec![
        r.op(
            claimed,
            Payload::TicketCreate(TicketCreate {
                title: "claimed".into(),
                state: "triage".into(),
                priority: Priority::Low,
                project: None,
                repo: None,
                source: None,
                ext: Default::default(),
            }),
        ),
        r.op(claimed, Payload::FieldSet(FieldSet::Number(2))),
        r.op(
            claimed,
            Payload::Claim(Claim {
                state: "in-progress".into(),
                assignee: ActorId::new(evil),
            }),
        ),
    ];
    store.apply_pulled(&ops).unwrap();
    let ready = Ulid::new();
    let ops = vec![
        r.op(
            ready,
            Payload::TicketCreate(TicketCreate {
                title: "ready".into(),
                state: "triage".into(),
                priority: Priority::Low,
                project: None,
                repo: None,
                source: None,
                ext: Default::default(),
            }),
        ),
        r.op(ready, Payload::FieldSet(FieldSet::Number(3))),
        r.op(
            ready,
            Payload::LabelAdd(LabelAdd {
                label: format!("model:{MODEL}"),
            }),
        ),
    ];
    store.apply_pulled(&ops).unwrap();
    drop(store);

    // (field, args, clean text that must appear in stdout)
    let table: &[(&str, &[&str], &str)] = &[
        ("title", &["show", "AGT-1"], CLEAN),
        ("repo", &["show", "AGT-1"], "org/[2J"),
        ("labels", &["show", "AGT-1"], "l[2J"),
        ("linked-pr", &["show", "AGT-1"], "pr[2J"),
        ("linear", &["show", "AGT-1"], "ln[2J"),
        ("hold marker", &["show", "AGT-1"], "h [2J"),
        ("comment author", &["show", "AGT-1"], CLEAN),
        ("hold reason", &["holds"], "h [2J"),
        ("actor", &["log", "AGT-1"], CLEAN),
        ("assignee", &["show", "AGT-2"], CLEAN),
        (
            "assignee field",
            &["show", "AGT-2", "--field", "assignee"],
            CLEAN,
        ),
        ("model label", &["ready", "--model", MODEL], "m[2J"),
        (
            "model label explain",
            &["ready", "--model", MODEL, "--explain"],
            "AGT-1",
        ),
        ("config ops", &["log"], "g[2J"),
        ("project title", &["project", "show", "evilp"], "P [2J"),
        ("project repos", &["project", "show", "evilp"], "repo[2J"),
        ("project list", &["project", "list"], CLEAN),
        ("gate labels", &["workspace", "gate-label", "list"], "g[2J"),
        ("unclaim assignee", &["unclaim", "AGT-2"], ""),
    ];
    for (field, args, want) in table {
        let out = sb.pm(args);
        let stdout = text(&out.stdout);
        let stderr = text(&out.stderr);
        no_controls(field, &stdout);
        no_controls(field, &stderr);
        assert!(
            stdout.contains(want) || stderr.contains(want),
            "{field}: {args:?} printed no clean {want:?}:\n{stdout}\n{stderr}"
        );
    }

    // Every other text command stays clean too.
    for args in [
        vec!["list"],
        vec!["list", "--project", "evilp"],
        vec!["status"],
        vec!["graph"],
        vec!["check"],
        vec!["doctor"],
        vec!["log", "AGT-2"],
        vec!["show", "AGT-2"],
        vec!["archive", "--auto", "--dry-run"],
        vec!["hub", "status"],
    ] {
        let out = sb.pm(&args);
        no_controls(&args.join(" "), &text(&out.stdout));
        no_controls(&args.join(" "), &text(&out.stderr));
    }
}
