//! A project document's merge state (projects/pm/README.md §Data model:
//! "project ... doc, extra named documents"): just a text CRDT keyed by a
//! stable Ulid, folded from `body.edit` ops the same way a ticket's
//! description is ([`crate::view::BodyState`]), without any of a ticket's
//! other CRDT-merged fields — a document has no title, state or labels.
//!
//! `op.entity` for a document op is the document's `doc_id`, a Ulid
//! pm-store stamps on the `project` row (the design doc) or a
//! `project_doc` row (a named document) — never the project's own
//! kebab-case `id`, which is not a Ulid. The op kind is `body.edit`, the
//! exact one ticket descriptions use ([`crate::op::Payload::BodyEdit`],
//! AGT-1338): there is exactly one body format in the op log, ever
//! (AGT-1344).

use serde::{Deserialize, Serialize};
use ulid::Ulid;

use crate::body::{BodyError, BodyUpdate};
use crate::hlc::Stamp;
use crate::op::{Op, Payload};
use crate::view::BodyState;

/// One document's merge state: its stable id and the text CRDT folded from
/// every `body.edit` op that targets it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DocView {
    pub id: Ulid,
    /// Greatest stamp of any op applied.
    pub updated: Option<Stamp>,
    pub body: BodyState,
}

impl DocView {
    /// An empty document.
    pub fn new(id: Ulid) -> Self {
        DocView {
            id,
            updated: None,
            body: BodyState::new(),
        }
    }

    /// The materialized text.
    pub fn text(&self) -> String {
        self.body.text()
    }
}

/// Why an op could not fold into a [`DocView`].
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum DocApplyError {
    #[error("op {op_id} targets entity {entity}, not document {doc}")]
    EntityMismatch {
        op_id: Ulid,
        entity: Ulid,
        doc: Ulid,
    },
    /// A document has no title, state or labels — only its body — so any
    /// op kind but `body.edit` is a caller bug; pm-store never sends one.
    #[error("op {op_id}: a document only accepts body.edit ops, not '{kind}'")]
    WrongKind { op_id: Ulid, kind: &'static str },
    #[error("op {op_id}: {source}")]
    BodyImport {
        op_id: Ulid,
        #[source]
        source: BodyError,
    },
}

/// Fold `op` into `view`. Pure and idempotent, the document analogue of
/// [`crate::view::apply`]: applying the same op again leaves `view`
/// unchanged, and replaying every op for a document in any order converges
/// (the text CRDT's own guarantee).
pub fn apply_doc(view: &mut DocView, op: &Op) -> Result<(), DocApplyError> {
    if op.entity != view.id {
        return Err(DocApplyError::EntityMismatch {
            op_id: op.op_id,
            entity: op.entity,
            doc: view.id,
        });
    }
    match &op.payload {
        Payload::BodyEdit(b) => {
            view.body
                .apply(&BodyUpdate::from_bytes(b.update.clone()))
                .map_err(|source| DocApplyError::BodyImport {
                    op_id: op.op_id,
                    source,
                })?;
        }
        other => {
            return Err(DocApplyError::WrongKind {
                op_id: op.op_id,
                kind: other.kind(),
            });
        }
    }
    let stamp = op.stamp();
    if view.updated.as_ref().is_none_or(|s| stamp > *s) {
        view.updated = Some(stamp);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::ActorId;
    use crate::hlc::Hlc;
    use crate::op::{BodyEdit, TicketCreate};
    use crate::{Body, Priority};

    fn op(doc: Ulid, wall_ms: u64, payload: Payload) -> Op {
        Op::new(
            Ulid::new(),
            Hlc::new(wall_ms, 0),
            ActorId::new("matt"),
            doc,
            payload,
        )
    }

    fn body_update(text: &str) -> Vec<u8> {
        Body::new().diff_from_text(text).unwrap().into_bytes()
    }

    fn body_edit(doc: Ulid, wall_ms: u64, text: &str) -> Op {
        op(
            doc,
            wall_ms,
            Payload::BodyEdit(BodyEdit {
                update: body_update(text),
            }),
        )
    }

    #[test]
    fn apply_doc_folds_body_edit_and_updates_the_stamp() {
        let doc = Ulid::new();
        let mut view = DocView::new(doc);
        assert_eq!(view.text(), "");
        let edit = body_edit(doc, 5, "# Design\n\nFirst draft.");
        apply_doc(&mut view, &edit).unwrap();
        assert_eq!(view.text(), "# Design\n\nFirst draft.");
        assert_eq!(view.updated, Some(edit.stamp()));
    }

    #[test]
    fn apply_doc_is_idempotent() {
        let doc = Ulid::new();
        let mut view = DocView::new(doc);
        let edit = body_edit(doc, 5, "stable");
        apply_doc(&mut view, &edit).unwrap();
        let once = view.clone();
        apply_doc(&mut view, &edit).unwrap();
        assert_eq!(view, once);
    }

    #[test]
    fn apply_doc_rejects_any_kind_but_body_edit() {
        let doc = Ulid::new();
        let mut view = DocView::new(doc);
        let bad = op(
            doc,
            1,
            Payload::TicketCreate(TicketCreate {
                title: "t".into(),
                state: "triage".into(),
                priority: Priority::Medium,
                project: None,
                repo: None,
                source: None,
                ext: Default::default(),
            }),
        );
        let err = apply_doc(&mut view, &bad).unwrap_err();
        assert!(
            matches!(&err, DocApplyError::WrongKind { kind, .. } if *kind == "ticket.create"),
            "{err:?}"
        );
        assert_eq!(
            view,
            DocView::new(doc),
            "rejected ops leave the view untouched"
        );
    }

    #[test]
    fn apply_doc_rejects_an_op_for_a_different_document() {
        let doc = Ulid::new();
        let mut view = DocView::new(doc);
        let foreign = body_edit(Ulid::new(), 1, "elsewhere");
        assert!(matches!(
            apply_doc(&mut view, &foreign),
            Err(DocApplyError::EntityMismatch { .. })
        ));
    }

    #[test]
    fn replaying_a_documents_ops_in_either_order_converges() {
        let doc = Ulid::new();
        let mut a = Body::new();
        let u1 = a.diff_from_text("v1").unwrap();
        let u2 = a.diff_from_text("v1 v2").unwrap();
        let first = op(
            doc,
            1,
            Payload::BodyEdit(BodyEdit {
                update: u1.into_bytes(),
            }),
        );
        let second = op(
            doc,
            2,
            Payload::BodyEdit(BodyEdit {
                update: u2.into_bytes(),
            }),
        );

        let mut forward = DocView::new(doc);
        apply_doc(&mut forward, &first).unwrap();
        apply_doc(&mut forward, &second).unwrap();

        let mut reverse = DocView::new(doc);
        apply_doc(&mut reverse, &second).unwrap();
        apply_doc(&mut reverse, &first).unwrap();

        assert_eq!(forward.text(), "v1 v2");
        assert_eq!(forward.text(), reverse.text());
    }

    #[test]
    fn doc_view_round_trips_through_serde() {
        let doc = Ulid::new();
        let mut view = DocView::new(doc);
        apply_doc(&mut view, &body_edit(doc, 3, "persisted")).unwrap();
        let json = serde_json::to_string(&view).unwrap();
        let back: DocView = serde_json::from_str(&json).unwrap();
        assert_eq!(back, view);
        assert_eq!(back.text(), "persisted");
    }
}
