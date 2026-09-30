//! Path-safety of the operator-chosen identifiers ops carry (AGT-1453,
//! AGT-1450): the workspace prefix and project ids end up in filesystem
//! paths (export, backup, editor temp files), and they can arrive in an
//! op from the hub or another replica, not only from this machine's CLI.
//!
//! [`is_safe_component`] is the rule for ids. It is deliberately looser
//! than the rules `pm init --prefix` / `pm project new` enforce at
//! creation, so every id that ever existed still passes; it refuses only
//! what could escape a directory. `pm`'s `ids::safe_component` checks it at
//! every filesystem sink, and [`check_op_ids`] at every trust boundary (the
//! hub's push, a replica's pull, and — AGT-1464 — every local commit and
//! `pm backup --restore`), so a hostile id is refused before it is stored
//! at all.
//!
//! Two free-form names also become paths (`pm export md` writes a state's
//! folder and a document's file): workflow state names and project
//! document names. They were never restricted to id characters — a vault
//! document can be `run notes`, a document name is `/`-separated
//! (`ideation/IDEA-1`) — so they get their own, looser rules
//! ([`is_safe_segment`], [`is_safe_doc_name`], AGT-1464) that refuse only
//! dot-segments, hidden names, separators and control characters.
//!
//! **Windows (AGT-1467).** Both rules also refuse what only Windows reads
//! as more than a name: a `:` (`C:x` joined onto a directory is a
//! drive-relative path that replaces it, and `name:stream` is an NTFS
//! alternate data stream) and the reserved device names (`CON`, `PRN`,
//! `AUX`, `NUL`, `COM1`–`COM9`, `LPT1`–`LPT9` and their superscript-digit
//! forms, `CONIN$`, `CONOUT$`) in any case and with any extension —
//! Windows maps `nul.txt` to the device too ([`is_windows_reserved`]).
//! On Unix these names are harmless, but a hub op reaches every platform.
//! Every id and name in the live workspace passed on 2026-09-30.

use crate::op::{FieldSet, Op, Payload, ProjectSet, WorkspaceSet};

/// Longest project id, and the longest path component accepted.
pub const ID_MAX: usize = 64;

/// Whether `value` is safe as one path component: ASCII letters, digits,
/// `-`, `_` and `.`, 1 to [`ID_MAX`] bytes, not starting with `.` (so
/// never `.` or `..`). No separators, NUL or other control characters.
pub fn is_safe_component(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= ID_MAX
        && !value.starts_with('.')
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        && !is_windows_reserved(value)
}

/// Whether Windows reads `value` as a device rather than a file: its stem
/// (the part before the first `.`, trailing spaces dropped — Windows
/// strips them) is `CON`, `PRN`, `AUX`, `NUL`, `COM1`–`COM9`,
/// `LPT1`–`LPT9` (or `COM`/`LPT` with a superscript `¹²³`), `CONIN$` or
/// `CONOUT$`, in any case. `nul`, `Com1.txt` and `aux.tar.gz` are
/// reserved; `console`, `nullable` and `com10` are not.
pub fn is_windows_reserved(value: &str) -> bool {
    let stem = value
        .split('.')
        .next()
        .unwrap_or(value)
        .trim_end_matches(' ');
    let upper = stem.to_uppercase();
    match upper.as_str() {
        "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$" => true,
        _ => {
            let port = upper
                .strip_prefix("COM")
                .or_else(|| upper.strip_prefix("LPT"));
            matches!(
                port,
                Some("1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³")
            )
        }
    }
}

/// Longest document name, and the longest state name or name segment.
pub const NAME_MAX: usize = 255;

/// Whether `value` is safe as one path segment of a free-form name (a
/// workflow state, one `/`-separated part of a document name): 1 to
/// [`NAME_MAX`] bytes, not starting with `.` (so never `.` or `..`, and
/// never a hidden file), no `/`, `\`, `:` or control character (NUL
/// included), and not a Windows device name ([`is_windows_reserved`],
/// AGT-1467). Spaces and non-ASCII letters are fine.
pub fn is_safe_segment(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= NAME_MAX
        && !value.starts_with('.')
        && !value
            .chars()
            .any(|c| matches!(c, '/' | '\\' | ':') || c.is_control())
        && !is_windows_reserved(value)
}

/// Whether `name` is a safe project document name: at most [`NAME_MAX`]
/// bytes of `/`-separated segments, each [`is_safe_segment`] — so no
/// leading or trailing `/`, no empty, `.` or `..` segment. Every document
/// name the vault import produces (`notes`, `ideation/IDEA-1`) passes.
pub fn is_safe_doc_name(name: &str) -> bool {
    name.len() <= NAME_MAX && name.split('/').all(is_safe_segment)
}

/// An op carrying an identifier or name that is not safe in a path.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{what} {value:?} is not safe in a file path ({rule})")]
pub struct IdError {
    pub what: &'static str,
    pub value: String,
    /// The rule it broke, for the message.
    pub rule: &'static str,
}

const ID_RULE: &str = "letters, digits, '-', '_', '.'; at most 64 characters; not starting with '.'; not a Windows device name (CON, NUL, COM1, ...)";
const SEGMENT_RULE: &str = "at most 255 bytes; not starting with '.'; no '/', '\\', ':' or control characters; not a Windows device name (CON, NUL, COM1, ...)";
const DOC_NAME_RULE: &str = "'/'-separated names, each non-empty, not starting with '.' (no '.' or '..'), no '\\', ':' or control characters, not a Windows device name (CON, NUL, COM1, ...); at most 255 bytes";

/// [`is_safe_component`] as a typed check: `what` names the field in the
/// error (`"workspace prefix"`, `"project id"`).
pub fn check_component(what: &'static str, value: &str) -> Result<(), IdError> {
    if is_safe_component(value) {
        Ok(())
    } else {
        Err(IdError {
            what,
            value: value.to_string(),
            rule: ID_RULE,
        })
    }
}

/// Checks every identifier and name `op` carries that becomes part of a
/// path: a `workspace.set prefix`; a `project.create`'s id and parent and
/// a `project.set parent`; a ticket's project (`ticket.create`,
/// `field.set project`); a workflow state's name (`state.upsert`,
/// `ticket.create`, `state.transition`, and — AGT-1482 — `claim`, which
/// writes its state into the ticket's `state` register exactly as a
/// transition does); and a `project.doc_add` document name (AGT-1464).
pub fn check_op_ids(op: &Op) -> Result<(), IdError> {
    let check = |what: &'static str, value: &str, ok: fn(&str) -> bool, rule: &'static str| {
        if ok(value) {
            Ok(())
        } else {
            Err(IdError {
                what,
                value: value.to_string(),
                rule,
            })
        }
    };
    let id = |what: &'static str, value: &str| check(what, value, is_safe_component, ID_RULE);
    let state = |value: &str| check("state name", value, is_safe_segment, SEGMENT_RULE);
    match &op.payload {
        Payload::WorkspaceSet(WorkspaceSet::Prefix(prefix)) => id("workspace prefix", prefix),
        Payload::ProjectCreate(create) => {
            id("project id", &create.id)?;
            match &create.parent {
                Some(parent) => id("project parent", parent),
                None => Ok(()),
            }
        }
        Payload::ProjectSet(ProjectSet::Parent(Some(parent))) => id("project parent", parent),
        Payload::ProjectDocAdd(add) => match &add.name {
            Some(name) => check("document name", name, is_safe_doc_name, DOC_NAME_RULE),
            None => Ok(()),
        },
        Payload::TicketCreate(create) => {
            state(&create.state)?;
            match &create.project {
                Some(project) => id("ticket project", project),
                None => Ok(()),
            }
        }
        Payload::FieldSet(FieldSet::Project(Some(project))) => id("ticket project", project),
        Payload::StateUpsert(s) => state(&s.name),
        Payload::StateTransition(t) => state(&t.state),
        Payload::Claim(c) => state(&c.state),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{ActorId, Priority, ProjectStatus, StateCategory};
    use crate::hlc::Hlc;
    use crate::op::{
        Claim, ProjectCreate, ProjectDocAdd, StateTransition, StateUpsert, TicketCreate,
    };
    use ulid::Ulid;

    fn op(payload: Payload) -> Op {
        Op::new(
            Ulid::new(),
            Hlc::new(1, 0),
            ActorId::new("matt"),
            Ulid::new(),
            payload,
        )
    }

    fn create(id: &str, parent: Option<&str>) -> Op {
        op(Payload::ProjectCreate(ProjectCreate {
            id: id.into(),
            title: "t".into(),
            status: ProjectStatus::InProgress,
            parent: parent.map(str::to_string),
            doc_id: None,
        }))
    }

    const BAD: &[&str] = &[
        "", ".", "..", "../x", "a/b", "a\\b", "/abs", "a\0b", "a\nb", "a b", ".hidden", "é",
    ];

    #[test]
    fn components() {
        for ok in ["pm", "think-3", "AGT", "a_b.c", &"a".repeat(ID_MAX)] {
            assert!(is_safe_component(ok), "{ok}");
        }
        for bad in BAD {
            assert!(!is_safe_component(bad), "{bad:?}");
        }
        assert!(!is_safe_component(&"a".repeat(ID_MAX + 1)));
    }

    #[test]
    fn ops_carrying_path_ids_are_checked() {
        assert_eq!(check_op_ids(&create("pm", Some("parent"))), Ok(()));
        assert_eq!(
            check_op_ids(&op(Payload::WorkspaceSet(WorkspaceSet::Prefix(
                "AGT".into()
            )))),
            Ok(())
        );
        for bad in BAD {
            let e = check_op_ids(&create(bad, None)).unwrap_err();
            assert_eq!(e.what, "project id");
            let e = check_op_ids(&create("pm", Some(bad))).unwrap_err();
            assert_eq!(e.what, "project parent");
            let e = check_op_ids(&op(Payload::WorkspaceSet(WorkspaceSet::Prefix(
                bad.to_string(),
            ))))
            .unwrap_err();
            assert_eq!(e.what, "workspace prefix");
            let e = check_op_ids(&op(Payload::ProjectSet(ProjectSet::Parent(Some(
                bad.to_string(),
            )))))
            .unwrap_err();
            assert_eq!(e.what, "project parent");
        }
        // Other ops, and a cleared parent, carry no path ids.
        assert_eq!(
            check_op_ids(&op(Payload::ProjectSet(ProjectSet::Parent(None)))),
            Ok(())
        );
        assert_eq!(
            check_op_ids(&op(Payload::ProjectSet(ProjectSet::Title("../x".into())))),
            Ok(())
        );
    }

    /// Every document name in the live workspace and the vault importer's
    /// shapes pass (AGT-1464: 110 names on 2026-09-30, one or two segments,
    /// at most 72 bytes, segments of at most 63).
    #[test]
    fn document_names() {
        for ok in [
            "notes",
            "BRIEF",
            "drafts-2026-09-07-wave2",
            "ideation/IDEA-1",
            "research/2026-09-01.competitors",
            "run notes",
            "a/b?c",
            "é",
            &"a".repeat(NAME_MAX),
        ] {
            assert!(is_safe_doc_name(ok), "{ok:?}");
        }
        for bad in [
            "",
            ".",
            "..",
            "../x",
            "x/..",
            "x/../y",
            "./x",
            "/abs",
            "x/",
            "x//y",
            ".hidden",
            "x/.git",
            "a\\b",
            "a\0b",
            "a\nb",
            &"a".repeat(NAME_MAX + 1),
        ] {
            assert!(!is_safe_doc_name(bad), "{bad:?}");
        }
        assert!(is_safe_segment("in review"));
        for bad in ["", ".", "..", "a/b", "a\\b", ".x", "a\tb"] {
            assert!(!is_safe_segment(bad), "{bad:?}");
        }
    }

    /// AGT-1467: `:` and the Windows device names are refused by both
    /// rules, in any case, with or without an extension; names that only
    /// start like one are fine.
    #[test]
    fn windows_drive_prefixes_and_device_names() {
        for reserved in [
            "CON",
            "con",
            "Con.txt",
            "nul",
            "NUL.tar.gz",
            "aux",
            "PRN",
            "com1",
            "COM9",
            "lpt1",
            "LPT9.md",
            "COM\u{b9}",
            "lpt\u{b3}",
            "CONIN$",
            "conout$",
            "NUL ",
            "nul .txt",
        ] {
            assert!(is_windows_reserved(reserved), "{reserved:?}");
            assert!(!is_safe_segment(reserved), "{reserved:?}");
            assert!(
                !is_safe_doc_name(&format!("ideation/{reserved}")),
                "{reserved:?}"
            );
        }
        for fine in [
            "console",
            "nullable",
            "com10",
            "com0",
            "lpt",
            "auxiliary",
            "CONTRIBUTING",
            "prn-x",
            "a.con",
            "notes",
        ] {
            assert!(!is_windows_reserved(fine), "{fine:?}");
            assert!(is_safe_segment(fine), "{fine:?}");
        }
        // Components: device names are refused; ':' never was allowed.
        for bad in ["con", "NUL", "com1", "Lpt2.x", "C:x", "a:b"] {
            assert!(!is_safe_component(bad), "{bad:?}");
            assert_eq!(
                check_component("project id", bad).unwrap_err().what,
                "project id"
            );
        }
        assert_eq!(check_component("workspace prefix", "AGT"), Ok(()));
        // ':' in a free-form name: a drive prefix or an NTFS stream.
        for bad in ["C:x", "C:", "notes:stream", "a:b"] {
            assert!(!is_safe_segment(bad), "{bad:?}");
            assert!(!is_safe_doc_name(bad), "{bad:?}");
            assert!(!is_safe_doc_name(&format!("ideation/{bad}")), "{bad:?}");
        }
        let e = check_op_ids(&doc_add(Some("C:evil"))).unwrap_err();
        assert!(e.to_string().contains("':'"), "{e}");
        let upsert = op(Payload::StateUpsert(StateUpsert {
            name: "NUL".into(),
            category: StateCategory::Started,
            position: 0,
        }));
        assert_eq!(check_op_ids(&upsert).unwrap_err().what, "state name");
        assert_eq!(
            check_op_ids(&create("con", None)).unwrap_err().what,
            "project id"
        );
    }

    fn doc_add(name: Option<&str>) -> Op {
        op(Payload::ProjectDocAdd(ProjectDocAdd {
            name: name.map(str::to_string),
            doc_id: Ulid::new(),
        }))
    }

    fn ticket(state: &str, project: Option<&str>) -> Op {
        op(Payload::TicketCreate(TicketCreate {
            title: "t".into(),
            state: state.into(),
            priority: Priority::Medium,
            project: project.map(str::to_string),
            repo: None,
            source: None,
            ext: Default::default(),
        }))
    }

    /// AGT-1464: the document name, a ticket's project and every state
    /// name are checked too, not only the workspace prefix and project ids.
    #[test]
    fn document_names_ticket_projects_and_states_are_checked() {
        assert_eq!(check_op_ids(&doc_add(None)), Ok(()));
        assert_eq!(check_op_ids(&doc_add(Some("ideation/IDEA-1"))), Ok(()));
        assert_eq!(check_op_ids(&ticket("in-progress", Some("pm"))), Ok(()));
        assert_eq!(check_op_ids(&ticket("triage", None)), Ok(()));
        for bad in ["..", "../x", "a/../b", "/abs", ".x", "a\0b"] {
            let e = check_op_ids(&doc_add(Some(bad))).unwrap_err();
            assert_eq!(e.what, "document name");
            assert!(e.to_string().contains("not safe in a file path"), "{e}");
        }
        for bad in BAD {
            let e = check_op_ids(&ticket("triage", Some(bad))).unwrap_err();
            assert_eq!(e.what, "ticket project");
            let e = check_op_ids(&op(Payload::FieldSet(FieldSet::Project(Some(
                bad.to_string(),
            )))))
            .unwrap_err();
            assert_eq!(e.what, "ticket project");
        }
        assert_eq!(
            check_op_ids(&op(Payload::FieldSet(FieldSet::Project(None)))),
            Ok(())
        );
        for bad in ["", "..", "a/b", ".x"] {
            assert_eq!(
                check_op_ids(&ticket(bad, None)).unwrap_err().what,
                "state name"
            );
            let upsert = op(Payload::StateUpsert(StateUpsert {
                name: bad.into(),
                category: StateCategory::Started,
                position: 0,
            }));
            assert_eq!(check_op_ids(&upsert).unwrap_err().what, "state name");
            let to = op(Payload::StateTransition(StateTransition {
                state: bad.into(),
            }));
            assert_eq!(check_op_ids(&to).unwrap_err().what, "state name");
        }
    }

    /// AGT-1482 (oaudit r4, high): a `claim` carries a state name the
    /// fold writes into the ticket's `state` register, so it is held to
    /// the same rule as every other state-bearing op.
    #[test]
    fn claim_state_names_are_checked() {
        let claim = |state: &str| {
            op(Payload::Claim(Claim {
                state: state.into(),
                assignee: ActorId::new("claude:a"),
            }))
        };
        assert_eq!(check_op_ids(&claim("in-progress")), Ok(()));
        assert_eq!(check_op_ids(&claim("in review")), Ok(()));
        for bad in [
            "", ".", "..", "../../x", "a/b", "a\\b", ".x", "a\0b", "a\nb", "C:x", "NUL", "com1.md",
        ] {
            let e = check_op_ids(&claim(bad)).unwrap_err();
            assert_eq!(e.what, "state name", "{bad:?}");
            assert_eq!(e.value, bad);
        }
    }
}
