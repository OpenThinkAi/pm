//! YAML parsing for `pm new --from-file` and `pm new --batch` (AGT-1346).
//!
//! Both a vault ticket file's frontmatter and a batch spec are YAML, so
//! they share the same value handling here: [`FileFrontmatter`] for one
//! ticket (00-meta/templates/ticket.md), [`BatchFile`] for several with
//! symbolic `@ref` blockers. `verbs::new` does the validation, op-building
//! and store I/O; this module only turns bytes into typed, still-unresolved
//! data.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::Context;
use pm_core::{Priority, Source, Workspace};
use pm_store::Store;
use serde::Deserialize;
use serde_json::Value;
use ulid::Ulid;

use crate::exit::{CliError, Result};

/// `source: { type: …, url: …, id: …, fetched-at: … }`, exactly as
/// 00-meta/templates/ticket.md spells it (inline-flow only; the vault
/// never uses the nested-block form, per
/// projects/pm/research/vault-anatomy.md).
#[derive(Debug, Clone, Default, Deserialize)]
pub struct SourceFm {
    #[serde(rename = "type", default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub id: Option<String>,
    #[serde(rename = "fetched-at", default)]
    pub fetched_at: Option<String>,
}

impl SourceFm {
    pub fn into_source(self) -> Source {
        Source {
            kind: self.kind.unwrap_or_default(),
            url: self.url.unwrap_or_default(),
            id: self.id.unwrap_or_default(),
            fetched_at: self.fetched_at.unwrap_or_default(),
        }
    }
}

/// Every frontmatter field `pm new --from-file` understands (AC1).
/// `id`/`state`/`created`/`updated` are read here — so they never leak
/// into `ext` — and ignored by `pm new`: pm computes all four itself (a
/// human number, the workspace's initial state, and both timestamps from
/// the op's own HLC). Only `pm import vault` (AGT-1347) reads them: the
/// vault's number, state and dates are exactly what it preserves.
/// Everything else unrecognized is preserved verbatim in `ext`.
#[derive(Debug, Default, Deserialize)]
pub struct FileFrontmatter {
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub id: Option<serde_yaml_ng::Value>,
    #[serde(default)]
    pub state: Option<serde_yaml_ng::Value>,
    #[serde(default)]
    pub created: Option<serde_yaml_ng::Value>,
    #[serde(default)]
    pub updated: Option<serde_yaml_ng::Value>,
    #[serde(default)]
    pub project: Option<String>,
    #[serde(default)]
    pub repo: Option<String>,
    #[serde(rename = "blocked-by", default)]
    pub blocked_by: Vec<String>,
    #[serde(rename = "linked-github", default)]
    pub linked_github: Option<String>,
    #[serde(rename = "linked-pr", default)]
    pub linked_pr: Option<String>,
    #[serde(default)]
    pub priority: Option<Priority>,
    #[serde(default)]
    pub labels: Vec<String>,
    #[serde(default)]
    pub source: Option<SourceFm>,
    /// Every frontmatter key not named above (AC1: "unknown keys go to
    /// `ext`").
    #[serde(flatten)]
    pub ext: BTreeMap<String, serde_yaml_ng::Value>,
}

/// `tickets:` — a `pm new --batch` spec (AC2).
#[derive(Debug, Deserialize)]
pub struct BatchFile {
    pub tickets: Vec<BatchEntry>,
}

/// One ticket in a batch file. `ref` (if present) is how another entry's
/// `blocked-by` names this one: `blocked-by: ["@core"]` for a `ref: core`
/// entry. A leading `@` on `ref` itself is tolerated and stripped, since
/// the `@` only ever means "this is a batch-local ref" at the point of
/// use.
#[derive(Debug, Deserialize)]
pub struct BatchEntry {
    #[serde(rename = "ref", default)]
    pub ticket_ref: Option<String>,
    pub title: String,
    #[serde(default)]
    pub project: Option<String>,
    #[serde(default)]
    pub repo: Option<String>,
    #[serde(default)]
    pub priority: Option<Priority>,
    #[serde(default)]
    pub labels: Vec<String>,
    /// `@ref` (resolved against other entries in this file) or an
    /// existing ticket id / ULID (resolved against the store) — see
    /// [`resolve_blocker`].
    #[serde(rename = "blocked-by", default)]
    pub blocked_by: Vec<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(rename = "linked-github", default)]
    pub linked_github: Option<String>,
    #[serde(rename = "linked-pr", default)]
    pub linked_pr: Option<String>,
    #[serde(default)]
    pub source: Option<SourceFm>,
    #[serde(flatten)]
    pub ext: BTreeMap<String, serde_yaml_ng::Value>,
}

/// The text between a vault ticket file's frontmatter fences, and
/// everything after the closing fence (trimmed), which becomes the
/// ticket's description verbatim. `pub(crate)`: `crate::import` splits
/// the same way, then normalizes the frontmatter before parsing it.
pub(crate) fn split_frontmatter(text: &str) -> Result<(&str, &str)> {
    let rest = text.strip_prefix("---\n").ok_or_else(|| {
        CliError::usage("file has no frontmatter block (expected a leading '---' line)")
    })?;
    let (fm, body) = rest
        .split_once("\n---")
        .ok_or_else(|| CliError::usage("file frontmatter is not closed with a '---' line"))?;
    let body = body.strip_prefix('\n').unwrap_or(body);
    Ok((fm, body.trim()))
}

/// Reads and parses a vault-format ticket file: its frontmatter, and its
/// body (everything after the closing fence) as the description text.
pub fn load_frontmatter(path: &Path) -> Result<(FileFrontmatter, String)> {
    let text =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    parse_frontmatter(&text)
        .map_err(|e| CliError::usage(format!("{} in {}", e.error, path.display())))
}

/// [`load_frontmatter`] on text already in memory (`pm edit` parses the
/// saved editor buffer with it).
pub fn parse_frontmatter(text: &str) -> Result<(FileFrontmatter, String)> {
    let (fm_text, body) = split_frontmatter(text)?;
    let fm: FileFrontmatter = serde_yaml_ng::from_str(fm_text)
        .map_err(|e| CliError::usage(format!("parsing frontmatter: {e}")))?;
    Ok((fm, body.to_string()))
}

/// Reads and parses a `pm new --batch` spec.
pub fn load_batch_file(path: &Path) -> Result<BatchFile> {
    let text =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    serde_yaml_ng::from_str(&text)
        .map_err(|e| CliError::usage(format!("parsing batch file {}: {e}", path.display())))
}

/// Converts a frontmatter/batch-entry `ext` map to the JSON values
/// [`pm_core::op::TicketCreate::ext`] stores. Infallible for any value a
/// YAML parser can produce (every YAML scalar/sequence/mapping has a JSON
/// equivalent).
pub fn ext_to_json(ext: BTreeMap<String, serde_yaml_ng::Value>) -> Result<BTreeMap<String, Value>> {
    ext.into_iter()
        .map(|(k, v)| match serde_json::to_value(&v) {
            Ok(v) => Ok((k, v)),
            Err(e) => Err(CliError::error(format!("field '{k}': {e}"))),
        })
        .collect()
}

/// Resolves one `blocked-by` entry (AC2, AC4): `@name` against `refs`
/// (other tickets minted in the same batch); anything else against the
/// store, as an existing ticket id (`AGT-N`) or ULID. Always fails as a
/// usage error (exit 2) naming the raw entry, per AC4 — batch input
/// validation is a usage error in either case, unlike a plain `pm show`
/// lookup (which is exit 3).
pub fn resolve_blocker(
    store: &Store,
    ws: &Workspace,
    refs: &BTreeMap<String, Ulid>,
    raw: &str,
) -> Result<Ulid> {
    let raw = raw.trim();
    if let Some(name) = raw.strip_prefix('@') {
        refs.get(name).copied().ok_or_else(|| {
            CliError::usage(format!(
                "unknown ref '{raw}': no ticket in this batch has ref '{name}'"
            ))
        })
    } else {
        crate::verbs::find(store, ws, raw)
            .map(|t| t.id)
            .map_err(|_| CliError::usage(format!("blocked-by '{raw}' is not an existing ticket")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frontmatter_splits_fences_from_body() {
        let text = "---\ntitle: T\n---\n\n## Problem Statement\n\nBody.\n";
        let (fm, body) = split_frontmatter(text).unwrap();
        assert_eq!(fm, "title: T");
        assert_eq!(body, "## Problem Statement\n\nBody.");
    }

    #[test]
    fn frontmatter_requires_both_fences() {
        assert!(split_frontmatter("no fences here").is_err());
        assert!(split_frontmatter("---\ntitle: T\n").is_err());
    }

    #[test]
    fn frontmatter_parses_inline_lists_and_maps_like_the_vault() {
        let fm: FileFrontmatter = serde_yaml_ng::from_str(
            "id: AGT-7\ntitle: \"Quoted title\"\nstate: triage\nproject: pm\nblocked-by: [AGT-1, AGT-2]\nlabels: [a, b]\nsource: { type: manual, url: \"\", id: \"\", fetched-at: \"\" }\nteam: engineering\n",
        )
        .unwrap();
        assert_eq!(fm.title.as_deref(), Some("Quoted title"));
        assert_eq!(fm.blocked_by, ["AGT-1", "AGT-2"]);
        assert_eq!(fm.labels, ["a", "b"]);
        assert_eq!(fm.source.unwrap().kind.as_deref(), Some("manual"));
        assert_eq!(fm.ext.len(), 1);
        assert!(fm.ext.contains_key("team"));
    }

    #[test]
    fn frontmatter_treats_blank_scalars_as_absent() {
        let fm: FileFrontmatter =
            serde_yaml_ng::from_str("title: T\nproject:\nlinked-github:\n").unwrap();
        assert_eq!(fm.project, None);
        assert_eq!(fm.linked_github, None);
    }
}
