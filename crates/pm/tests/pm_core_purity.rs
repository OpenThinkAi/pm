//! Mechanical check for AGT-1333 AC 3: `pm-core` must have no IO
//! dependencies (no `rusqlite`, no `std::fs`). This lives in the `pm` bin
//! crate's tests (not in `pm-core` itself) so the check's own use of
//! `std::fs` to read `pm-core`'s manifest and source never appears inside
//! `pm-core`.

use std::fs;
use std::path::{Path, PathBuf};

/// Crate names that would make `pm-core` do IO if depended on.
const DENYLIST: &[&str] = &[
    "rusqlite", "sqlx", "tokio", "reqwest", "mio", "std::fs", "hyper",
];

fn workspace_root() -> PathBuf {
    // crates/pm/tests/ -> crates/pm -> crates -> workspace root
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("crates/pm/tests sits two levels under the workspace root")
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
fn pm_core_src_never_uses_std_fs() {
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
        assert!(
            !text.contains("std::fs"),
            "{} uses std::fs: pm-core must never perform IO (AGT-1333 AC 3)",
            path.display()
        );
    }
}
