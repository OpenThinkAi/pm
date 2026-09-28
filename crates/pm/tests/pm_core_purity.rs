//! Mechanical check for AGT-1333 AC 3: `pm-core` must have no IO
//! dependencies (no `rusqlite`, no filesystem / network / process use).
//! This lives in the `pm` bin crate's tests (not in `pm-core` itself) so
//! the check's own use of `std::fs` to read `pm-core`'s manifest and source
//! never appears inside `pm-core`.

use std::fs;
use std::path::{Path, PathBuf};

/// Crate names that would make `pm-core` do IO if depended on.
const DENYLIST: &[&str] = &["rusqlite", "sqlx", "tokio", "reqwest", "mio", "hyper"];

/// `std` modules that perform IO. Any path into one of them, whether
/// written `std::fs::read` or `use std::{fs, path}`, fails the scan.
const DENIED_STD_MODULES: &[&str] = &["fs", "net", "process"];

fn workspace_root() -> PathBuf {
    // CARGO_MANIFEST_DIR = crates/pm -> crates -> workspace root
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("crates/pm sits two levels under the workspace root")
        .to_path_buf()
}

fn collect_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let entries = fs::read_dir(dir).unwrap_or_else(|e| panic!("listing {}: {e}", dir.display()));
    for entry in entries {
        let path = entry.unwrap().path();
        if path.is_dir() {
            collect_files(&path, out);
        } else {
            out.push(path);
        }
    }
}

/// The first path segment after `std::`, or, for a `std::{...}` group, the
/// first segment of every comma-separated item in it (nested groups
/// included, since their first segment is what matters).
fn std_modules_referenced(source: &str) -> Vec<String> {
    let mut found = Vec::new();
    let code: String = source
        .lines()
        .map(|line| line.split("//").next().unwrap_or(""))
        .collect::<Vec<_>>()
        .join("\n");
    for (idx, _) in code.match_indices("std::") {
        let rest = code[idx + "std::".len()..].trim_start();
        if let Some(group) = rest.strip_prefix('{') {
            // Walk the group to its closing brace, splitting items only at
            // top-level commas so `fs::{self, File}` stays one item.
            let mut depth = 1;
            let mut item_start = 0;
            for (i, c) in group.char_indices() {
                match c {
                    '{' => depth += 1,
                    '}' if depth > 1 => depth -= 1,
                    ',' if depth == 1 => {
                        found.push(first_segment(group[item_start..i].trim_start()));
                        item_start = i + 1;
                    }
                    '}' => {
                        found.push(first_segment(group[item_start..i].trim_start()));
                        break;
                    }
                    _ => {}
                }
            }
        } else {
            found.push(first_segment(rest));
        }
    }
    found.retain(|s| !s.is_empty());
    found
}

fn first_segment(path: &str) -> String {
    path.chars()
        .take_while(|c| c.is_alphanumeric() || *c == '_')
        .collect()
}

#[test]
fn std_module_scan_catches_grouped_imports() {
    let mods = std_modules_referenced(
        "use std::{collections::BTreeMap, fs::{self, File}};\nlet x = std::process::exit(1);\n// std::net in a comment is fine\n",
    );
    assert_eq!(mods, ["collections", "fs", "process"]);
    assert!(
        std_modules_referenced("use std::fmt;\nstd::{ net , io }").contains(&"net".to_string())
    );
}

#[test]
fn pm_core_manifest_has_no_io_dependencies() {
    let manifest = workspace_root().join("crates/pm-core/Cargo.toml");
    let text = fs::read_to_string(&manifest)
        .unwrap_or_else(|e| panic!("reading {}: {e}", manifest.display()));

    for dep in DENYLIST {
        assert!(
            !text.contains(dep),
            "crates/pm-core/Cargo.toml must not reference IO crate '{dep}': pm-core is pure (AGT-1333 AC 3)"
        );
    }
}

#[test]
fn pm_core_src_never_uses_std_io_modules() {
    let src_dir = workspace_root().join("crates/pm-core/src");
    let mut files = Vec::new();
    collect_files(&src_dir, &mut files);
    assert!(
        !files.is_empty(),
        "expected source files under {}",
        src_dir.display()
    );

    for path in files {
        let text =
            fs::read_to_string(&path).unwrap_or_else(|e| panic!("reading {}: {e}", path.display()));
        for module in std_modules_referenced(&text) {
            assert!(
                !DENIED_STD_MODULES.contains(&module.as_str()),
                "{} uses std::{module}: pm-core must never perform IO (AGT-1333 AC 3)",
                path.display()
            );
        }
    }
}
