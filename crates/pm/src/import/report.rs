//! The import report (AGT-1347 AC3, AC4, AC6): counts, every migrated
//! marker line, every anomaly, and the non-template values found —
//! printed after the import, or as `--json` (`{schema, …}`).

use std::collections::BTreeMap;

use serde::Serialize;

use super::plan::TicketOutcome;
use crate::verbs::{SCHEMA, print_json};

#[derive(Clone, Debug, Default, Serialize)]
pub struct Report {
    pub schema: u32,
    pub dry_run: bool,
    pub vault: String,
    /// The workspace's id prefix (`AGT`).
    pub prefix: String,
    pub files: usize,
    pub tickets: TicketOutcome,
    /// Tickets under `archive/20*/` (imported with `archived_at`).
    pub archived: usize,
    pub projects: usize,
    /// Projects a ticket named that no README exists for; created empty.
    pub project_stubs: Vec<String>,
    pub docs: DocOutcome,
    pub ops: usize,
    pub ops_by_kind: BTreeMap<String, usize>,
    pub comments: usize,
    pub markers: BTreeMap<String, usize>,
    /// `AGT-N <kind>: <marker text>`, one per migrated marker.
    pub migrated: Vec<String>,
    /// `AGT-N: <what changed>`, one per re-imported ticket with changes.
    pub changes: Vec<String>,
    pub anomalies: Vec<String>,
    /// Fresh numbers handed to duplicate-id files: `AGT-1377 (was AGT-846)`.
    pub renumbered: Vec<String>,
    /// Category → `path:line` occurrences.
    pub non_template: BTreeMap<String, Vec<String>>,
    /// Frontmatter keys preserved in `ext`, with file counts.
    pub ext_keys: BTreeMap<String, usize>,
    pub max_number: u64,
    pub number_floor: u64,
    /// `--report`: the parity comparison's summary (AGT-1348); absent
    /// without the flag.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parity: Option<super::parity::Summary>,
    pub elapsed_ms: u128,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct DocOutcome {
    pub created: usize,
    pub updated: usize,
    pub unchanged: usize,
    /// Documents left alone because the workspace's `docs_owned_by` is
    /// `pm` (AGT-1406): `projects/<id>/README.md`, `projects/<id>/<name>.md`.
    pub skipped: Vec<String>,
}

impl Report {
    pub fn new(vault: String, prefix: &str, dry_run: bool) -> Self {
        Report {
            schema: SCHEMA,
            dry_run,
            vault,
            prefix: prefix.to_string(),
            ..Report::default()
        }
    }

    pub fn print(&self, json: bool) {
        if json {
            print_json(&serde_json::to_value(self).expect("a report serializes"));
            return;
        }
        let mode = if self.dry_run {
            " (dry run: nothing written)"
        } else {
            ""
        };
        println!("import vault {}{mode}", self.vault);
        println!(
            "files:      {} ticket files → {} created, {} changed, {} unchanged, {} skipped ({} archived)",
            self.files,
            self.tickets.created,
            self.tickets.changed,
            self.tickets.unchanged,
            self.tickets.skipped,
            self.archived
        );
        println!(
            "projects:   {} ({} stubs); docs: {} created, {} updated, {} unchanged, {} skipped (owned by pm)",
            self.projects,
            self.project_stubs.len(),
            self.docs.created,
            self.docs.updated,
            self.docs.unchanged,
            self.docs.skipped.len()
        );
        let kinds: Vec<String> = self
            .ops_by_kind
            .iter()
            .map(|(k, n)| format!("{k} {n}"))
            .collect();
        println!("ops:        {} ({})", self.ops, kinds.join(", "));
        println!("comments:   {}", self.comments);
        let markers: Vec<String> = self
            .markers
            .iter()
            .map(|(k, n)| format!("{k} {n}"))
            .collect();
        println!(
            "markers:    {} migrated ({})",
            self.migrated.len(),
            markers.join(", ")
        );
        println!(
            "numbers:    max {}-{}, allocator floor {}",
            self.prefix, self.max_number, self.number_floor
        );
        if let Some(p) = &self.parity {
            println!(
                "parity:     {} compared, {} missing, {} unexplained diff(s), {} explained class(es) → {}",
                p.compared,
                p.missing,
                p.unexplained,
                p.explained.len(),
                p.report
            );
        }
        println!("time:       {} ms", self.elapsed_ms);

        section("non-template values", &flatten(&self.non_template));
        let ext: Vec<String> = self
            .ext_keys
            .iter()
            .map(|(k, n)| format!("{k} ({n})"))
            .collect();
        section("ext keys", &ext);
        section("project stubs", &self.project_stubs);
        section("skipped docs (docs_owned_by = pm)", &self.docs.skipped);
        section("renumbered", &self.renumbered);
        section("anomalies", &self.anomalies);
        section("changes", &self.changes);
        section("migrated markers", &self.migrated);
    }
}

fn flatten(map: &BTreeMap<String, Vec<String>>) -> Vec<String> {
    map.iter()
        .map(|(category, at)| {
            let shown: Vec<&str> = at.iter().take(5).map(String::as_str).collect();
            let more = if at.len() > 5 {
                format!(", … {} more", at.len() - 5)
            } else {
                String::new()
            };
            format!("{category} ({}): {}{more}", at.len(), shown.join(", "))
        })
        .collect()
}

fn section(title: &str, lines: &[String]) {
    if lines.is_empty() {
        return;
    }
    println!();
    println!("{title} ({}):", lines.len());
    for line in lines {
        println!("  {line}");
    }
}
