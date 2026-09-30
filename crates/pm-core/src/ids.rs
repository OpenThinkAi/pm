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
}

/// Longest document name, and the longest state name or name segment.
pub const NAME_MAX: usize = 255;

/// Whether `value` is safe as one path segment of a free-form name (a
/// workflow state, one `/`-separated part of a document name): 1 to
/// [`NAME_MAX`] bytes, not starting with `.` (so never `.` or `..`, and
/// never a hidden file), and no `/`, `\` or control character (NUL
/// included). Spaces and non-ASCII letters are fine.
pub fn is_safe_segment(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= NAME_MAX
        && !value.starts_with('.')
        && !value
            .chars()
            .any(|c| c == '/' || c == '\\' || c.is_control())
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

const ID_RULE: &str =
    "letters, digits, '-', '_', '.'; at most 64 characters; not starting with '.'";
const SEGMENT_RULE: &str =
    "at most 255 bytes; not starting with '.'; no '/', '\\' or control characters";
const DOC_NAME_RULE: &str = "'/'-separated names, each non-empty, not starting with '.' (no '.' or '..'), no '\\' or control characters; at most 255 bytes";

/// Checks every identifier and name `op` carries that becomes part of a
/// path: a `workspace.set prefix`; a `project.create`'s id and parent and
/// a `project.set parent`; a ticket's project (`ticket.create`,
/// `field.set project`); a workflow state's name (`state.upsert`,
/// `ticket.create`, `state.transition`); and a `project.doc_add` document
/// name (AGT-1464).
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
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{ActorId, Priority, ProjectStatus, StateCategory};
    use crate::hlc::Hlc;
    use crate::op::{ProjectCreate, ProjectDocAdd, StateTransition, StateUpsert, TicketCreate};
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
}
