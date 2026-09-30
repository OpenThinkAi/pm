//! Path-safety of the operator-chosen identifiers ops carry (AGT-1453,
//! AGT-1450): the workspace prefix and project ids end up in filesystem
//! paths (export, backup, editor temp files), and they can arrive in an
//! op from the hub or another replica, not only from this machine's CLI.
//!
//! [`is_safe_component`] is the one rule. It is deliberately looser than
//! the rules `pm init --prefix` / `pm project new` enforce at creation, so
//! every id that ever existed still passes; it refuses only what could
//! escape a directory. `pm`'s `ids::safe_component` checks it at every
//! filesystem sink, and [`check_op_ids`] at every trust boundary (the
//! hub's push, a replica's pull), so a hostile id is refused before it is
//! stored at all.

use crate::op::{Op, Payload, ProjectSet, WorkspaceSet};

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

/// An op carrying an identifier that is not [`is_safe_component`].
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error(
    "{what} {value:?} is not a safe identifier (letters, digits, '-', '_', '.'; at most {ID_MAX} characters; not starting with '.')"
)]
pub struct IdError {
    pub what: &'static str,
    pub value: String,
}

/// Checks every identifier `op` carries that becomes part of a path: a
/// `workspace.set prefix`, a `project.create`'s id and parent, and a
/// `project.set parent`.
pub fn check_op_ids(op: &Op) -> Result<(), IdError> {
    let check = |what: &'static str, value: &str| {
        if is_safe_component(value) {
            Ok(())
        } else {
            Err(IdError {
                what,
                value: value.to_string(),
            })
        }
    };
    match &op.payload {
        Payload::WorkspaceSet(WorkspaceSet::Prefix(prefix)) => check("workspace prefix", prefix),
        Payload::ProjectCreate(create) => {
            check("project id", &create.id)?;
            match &create.parent {
                Some(parent) => check("project parent", parent),
                None => Ok(()),
            }
        }
        Payload::ProjectSet(ProjectSet::Parent(Some(parent))) => check("project parent", parent),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{ActorId, ProjectStatus};
    use crate::hlc::Hlc;
    use crate::op::ProjectCreate;
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
}
