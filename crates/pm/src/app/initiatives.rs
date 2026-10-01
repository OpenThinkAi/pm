//! `GET /initiatives` (AGT-1491, `docs/app-api.md`): every initiative with
//! its projects as a tree, plus the **Unfiled** group, each node carrying
//! its own unarchived ticket counts by state category and a `total` rolled
//! up over the subtree it shows.
//!
//! The parent links come from synced data, so the tree is built
//! defensively: an initiative is always a root (its `parent`, which a
//! concurrent sync from an older client could still set, is ignored), a
//! parent that does not exist here is no parent, and a parent cycle —
//! possible when two replicas re-parent concurrently, each passing its own
//! cycle guard — is cut at its smallest id, so every project appears
//! exactly once and nothing is counted twice.

use std::collections::{HashMap, HashSet};

use pm_core::{Project, ProjectKind, ProjectStatus, StateCategory, Ticket, Workspace};
use serde_json::{Map, Value, json};

use crate::verbs::SCHEMA;

/// Ticket counts by state category, in the category order the JSON uses.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Counts([u64; 5]);

const CATEGORIES: [&str; 5] = ["backlog", "unstarted", "started", "completed", "canceled"];

impl Counts {
    fn add(&mut self, category: StateCategory) {
        let i = match category {
            StateCategory::Backlog => 0,
            StateCategory::Unstarted => 1,
            StateCategory::Started => 2,
            StateCategory::Completed => 3,
            StateCategory::Canceled => 4,
        };
        self.0[i] += 1;
    }

    fn merge(&mut self, other: Counts) {
        for (a, b) in self.0.iter_mut().zip(other.0) {
            *a += b;
        }
    }

    fn json(self) -> Value {
        let mut out = Map::new();
        for (name, n) in CATEGORIES.iter().zip(self.0) {
            out.insert((*name).into(), json!(n));
        }
        Value::Object(out)
    }
}

/// Each project's own tickets, counted by the category of their state.
/// `tickets` is the unarchived set; a ticket with no project, a project
/// unknown here, or a state the workspace does not define counts nowhere.
pub(crate) fn own_counts(ws: &Workspace, tickets: &[Ticket]) -> HashMap<String, Counts> {
    let mut out: HashMap<String, Counts> = HashMap::new();
    for t in tickets {
        let (Some(project), Some(state)) = (&t.project, ws.state(&t.state)) else {
            continue;
        };
        out.entry(project.clone()).or_default().add(state.category);
    }
    out
}

/// The `GET /initiatives` answer over `projects` (in the order the store
/// lists them, which orders siblings) and each one's `own` counts.
///
/// `status` keeps only projects with that status, as `GET /projects`
/// does. A kept project hangs under its nearest kept ancestor; one whose
/// initiative was filtered out is not shown at all (it is filed, just not
/// under anything shown); and the rollups cover the nodes shown.
pub(crate) fn tree(
    projects: &[Project],
    own: &HashMap<String, Counts>,
    status: Option<ProjectStatus>,
) -> Value {
    let index: HashMap<&str, usize> = projects
        .iter()
        .enumerate()
        .map(|(i, p)| (p.id.as_str(), i))
        .collect();
    let parent = effective_parents(projects, &index);
    let kept = |i: usize| status.is_none_or(|s| projects[i].status == s);

    // Where each kept project goes: under its nearest kept ancestor, or
    // at the top (an initiative, or Unfiled when no initiative is above
    // it). The parent graph is acyclic now, so every walk ends.
    let mut children: Vec<Vec<usize>> = vec![Vec::new(); projects.len()];
    let mut initiatives = Vec::new();
    let mut unfiled = Vec::new();
    for i in (0..projects.len()).filter(|&i| kept(i)) {
        let mut up = parent[i];
        let mut under_initiative = false;
        let mut placed = false;
        while let Some(a) = up {
            if kept(a) {
                children[a].push(i);
                placed = true;
                break;
            }
            under_initiative |= projects[a].kind == ProjectKind::Initiative;
            up = parent[a];
        }
        if placed {
            continue;
        }
        if projects[i].kind == ProjectKind::Initiative {
            initiatives.push(i);
        } else if !under_initiative {
            unfiled.push(i);
        }
    }

    let ctx = Ctx {
        projects,
        own,
        children: &children,
    };
    let mut unfiled_total = Counts::default();
    let unfiled_nodes: Vec<Value> = unfiled
        .iter()
        .map(|&i| {
            let (node, total) = ctx.node(i);
            unfiled_total.merge(total);
            node
        })
        .collect();
    json!({
        "schema": SCHEMA,
        "initiatives": initiatives.iter().map(|&i| ctx.node(i).0).collect::<Vec<_>>(),
        "unfiled": {
            "total": unfiled_total.json(),
            "projects": unfiled_nodes,
        },
    })
}

struct Ctx<'a> {
    projects: &'a [Project],
    own: &'a HashMap<String, Counts>,
    children: &'a [Vec<usize>],
}

impl Ctx<'_> {
    /// One node and its subtree's total. Recursion depth is the tree's
    /// depth, which is bounded by the project count and acyclic.
    fn node(&self, i: usize) -> (Value, Counts) {
        let p = &self.projects[i];
        let tickets = self.own.get(&p.id).copied().unwrap_or_default();
        let mut total = tickets;
        let children: Vec<Value> = self.children[i]
            .iter()
            .map(|&c| {
                let (node, sub) = self.node(c);
                total.merge(sub);
                node
            })
            .collect();
        let node = json!({
            "id": p.id,
            "title": p.title,
            "kind": p.kind.as_str(),
            "status": p.status,
            "tickets": tickets.json(),
            "total": total.json(),
            "children": children,
        });
        (node, total)
    }
}

/// Each project's parent as the tree uses it: `None` for an initiative, a
/// parent not known here, or the one link that closes a cycle (cut at the
/// cycle's smallest id, so the cut does not depend on listing order).
fn effective_parents(projects: &[Project], index: &HashMap<&str, usize>) -> Vec<Option<usize>> {
    let mut parent: Vec<Option<usize>> = projects
        .iter()
        .map(|p| {
            if p.kind == ProjectKind::Initiative {
                return None;
            }
            p.parent.as_deref().and_then(|id| index.get(id).copied())
        })
        .collect();

    // Walk up from each project; a walk that meets its own path has found
    // a cycle. Each project is settled once, so this is linear.
    let mut settled = vec![false; projects.len()];
    for start in 0..projects.len() {
        let mut path: Vec<usize> = Vec::new();
        let mut on_path: HashSet<usize> = HashSet::new();
        let mut at = Some(start);
        while let Some(i) = at {
            if settled[i] {
                break;
            }
            if on_path.contains(&i) {
                let from = path.iter().position(|&p| p == i).expect("on the path");
                let cut = path[from..]
                    .iter()
                    .copied()
                    .min_by(|&a, &b| projects[a].id.cmp(&projects[b].id))
                    .expect("a cycle has a member");
                parent[cut] = None;
                break;
            }
            path.push(i);
            on_path.insert(i);
            at = parent[i];
        }
        for i in path {
            settled[i] = true;
        }
    }
    parent
}

#[cfg(test)]
mod tests {
    use super::*;

    fn project(id: &str, kind: ProjectKind, parent: Option<&str>) -> Project {
        Project {
            id: id.into(),
            title: id.to_uppercase(),
            kind,
            status: ProjectStatus::InProgress,
            parent: parent.map(str::to_string),
            repos: Default::default(),
            doc: "a design doc body".into(),
            documents: Default::default(),
        }
    }

    fn counts(started: u64) -> Counts {
        let mut c = Counts::default();
        for _ in 0..started {
            c.add(StateCategory::Started);
        }
        c
    }

    /// Every id in the answer, wherever it sits.
    fn ids(v: &Value, out: &mut Vec<String>) {
        if let Some(id) = v.get("id").and_then(Value::as_str) {
            out.push(id.to_string());
        }
        for key in ["initiatives", "projects", "children"] {
            if let Some(list) = v.get(key).and_then(Value::as_array) {
                for n in list {
                    ids(n, out);
                }
            }
        }
        if let Some(u) = v.get("unfiled") {
            ids(u, out);
        }
    }

    #[test]
    fn a_parent_cycle_is_cut_once_and_counted_once() {
        use ProjectKind::Project as P;
        // a -> b -> c -> a, and d hangs off the cycle.
        let projects = vec![
            project("b", P, Some("c")),
            project("c", P, Some("a")),
            project("a", P, Some("b")),
            project("d", P, Some("b")),
        ];
        let own = HashMap::from([
            ("a".to_string(), counts(1)),
            ("b".to_string(), counts(2)),
            ("c".to_string(), counts(4)),
            ("d".to_string(), counts(8)),
        ]);
        let v = tree(&projects, &own, None);
        let mut seen = Vec::new();
        ids(&v, &mut seen);
        seen.sort();
        assert_eq!(seen, ["a", "b", "c", "d"], "{v}");
        // Cut at `a`, the smallest id: a <- c <- b <- d.
        let root = &v["unfiled"]["projects"][0];
        assert_eq!(root["id"], "a", "{v}");
        assert_eq!(root["total"]["started"], 15, "{v}");
        assert_eq!(v["unfiled"]["total"]["started"], 15, "{v}");
        assert_eq!(root["children"][0]["id"], "c");
        assert_eq!(root["children"][0]["children"][0]["id"], "b");
        assert_eq!(root["children"][0]["children"][0]["children"][0]["id"], "d");
    }

    #[test]
    fn a_self_parent_and_a_dangling_parent_are_roots() {
        use ProjectKind::Project as P;
        let projects = vec![project("x", P, Some("x")), project("y", P, Some("gone"))];
        let v = tree(&projects, &HashMap::new(), None);
        let roots: Vec<&str> = v["unfiled"]["projects"]
            .as_array()
            .unwrap()
            .iter()
            .map(|n| n["id"].as_str().unwrap())
            .collect();
        assert_eq!(roots, ["x", "y"]);
    }

    #[test]
    fn an_initiative_is_always_a_root_and_nodes_carry_no_doc() {
        use ProjectKind::{Initiative as I, Project as P};
        // An initiative with a parent (an older client's sync) and a
        // cycle through it: still a root, its project under it.
        let projects = vec![project("i", I, Some("p")), project("p", P, Some("i"))];
        let v = tree(&projects, &HashMap::new(), None);
        assert_eq!(v["initiatives"][0]["id"], "i", "{v}");
        assert_eq!(v["initiatives"][0]["children"][0]["id"], "p", "{v}");
        assert_eq!(v["unfiled"]["projects"], json!([]));
        let node = v["initiatives"][0].as_object().unwrap();
        let keys: Vec<&str> = node.keys().map(String::as_str).collect();
        assert_eq!(
            keys,
            [
                "children", "id", "kind", "status", "tickets", "title", "total"
            ]
        );
    }

    #[test]
    fn a_status_filter_rehangs_under_the_nearest_kept_ancestor() {
        use ProjectKind::{Initiative as I, Project as P};
        let mut mid = project("mid", P, Some("init"));
        mid.status = ProjectStatus::Complete;
        let mut done_init = project("old", I, None);
        done_init.status = ProjectStatus::Complete;
        let projects = vec![
            project("init", I, None),
            mid,
            project("leaf", P, Some("mid")),
            done_init,
            project("orphan", P, Some("old")),
        ];
        let own = HashMap::from([
            ("mid".to_string(), counts(5)),
            ("leaf".to_string(), counts(1)),
        ]);
        let v = tree(&projects, &own, Some(ProjectStatus::InProgress));
        assert_eq!(v["initiatives"].as_array().unwrap().len(), 1, "{v}");
        let init = &v["initiatives"][0];
        assert_eq!(init["children"][0]["id"], "leaf", "{v}");
        assert_eq!(init["total"]["started"], 1, "{v}");
        // `orphan` is filed under a filtered-out initiative: not Unfiled.
        assert_eq!(v["unfiled"]["projects"], json!([]), "{v}");
    }
}
