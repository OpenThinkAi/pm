//! AGT-1436: persisted-view compatibility rule.
//!
//! `WorkspaceView`, `ProjectView`, `TicketView` and `DocView` are stored as
//! JSON by every client (`*_view` tables) and by the hub (`workspace_views`,
//! `ticket_views`), which panics on a row it cannot decode (the AGT-1406
//! outage). Rows written by an older binary lack any field added since.
//!
//! THE RULE: a field added to a persisted view MUST carry
//! `#[serde(default)]`. The test removes each top-level key from a current
//! serialization and requires the view to still decode, except for the keys
//! listed in `REQUIRED` — the fields every stored row has always carried.
//! Adding a field without a default fails here; do not extend `REQUIRED`
//! for a new field (old rows will not have it).

use pm_core::{ProjectView, WorkspaceView};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;
use ulid::Ulid;

fn check<T: Serialize + DeserializeOwned + PartialEq + std::fmt::Debug>(
    view: T,
    required: &[&str],
) {
    let full = serde_json::to_value(&view).unwrap();
    let keys: Vec<String> = full.as_object().unwrap().keys().cloned().collect();
    for required in required {
        assert!(
            keys.iter().any(|k| k == required),
            "stale REQUIRED: {required}"
        );
    }
    for key in keys.iter().filter(|k| !required.contains(&k.as_str())) {
        let mut stripped: Value = full.clone();
        stripped.as_object_mut().unwrap().remove(key);
        let decoded: T = serde_json::from_value(stripped).unwrap_or_else(|e| {
            panic!(
                "{} without `{key}` no longer decodes ({e}): a new persisted view field needs #[serde(default)]",
                std::any::type_name::<T>()
            )
        });
        // A defaulted field reads back as a fresh view's value.
        assert_eq!(decoded, view, "{key}");
    }
}

#[test]
fn workspace_view_decodes_without_fields_added_since_v1() {
    // `docs_owned_by` (AGT-1406) is the field added so far.
    check(
        WorkspaceView::new(Ulid::new()),
        &[
            "id",
            "updated",
            "prefix",
            "gate_labels",
            "model_labels",
            "template_sections",
            "stale_days",
            "states",
            "actors",
        ],
    );
}

#[test]
fn project_view_decodes_without_fields_added_since_v1() {
    // `deleted_at`, `design_doc`, `documents` (AGT-1413) are defaulted.
    check(
        ProjectView::new(Ulid::new()),
        &[
            "id", "created", "updated", "slug", "title", "status", "parent", "repos",
        ],
    );
}

// `TicketView` and `DocView` have never gained a field, so every key is
// required today. When one does, add a test like the two above for it,
// listing the keys that were always there.
