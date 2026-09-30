//! The ui-leaf views pm ships (AGT-1402; `docs/app-api.md` §Views).
//!
//! The views are TSX sources in `crates/pm/views/`, compiled into the `pm`
//! binary with `include_str!` ([`FILES`]) so an installed `pm` needs no
//! checkout. ui-leaf compiles a view from a `viewsRoot` directory on disk
//! at mount time, so on launch [`root`] unpacks them once into
//! `<cache>/pm/views/<fingerprint>/` (`$XDG_CACHE_HOME`, else `~/.cache`)
//! and reuses that directory until the sources change. A fingerprint names
//! the directory, and it only ever appears by an atomic rename of a fully
//! written temp directory, so a present directory is always complete and
//! two `pm`s unpacking at once cannot tear it.
//!
//! `views/vendor/` is third-party code the ticket editor bundles —
//! `loro-crdt` (its wasm inlined as base64, ~4.7 MB) and CodeMirror with
//! `loro-codemirror` — generated from pinned versions by
//! `crates/pm/views-vendor/build.ts` and committed, so neither building nor
//! running pm needs a JavaScript toolchain (ui-leaf bundles relative
//! imports; it resolves no npm packages but React).
//!
//! `PM_VIEWS_DIR=<dir>` mounts `<dir>` instead: edit a `.tsx`, re-run
//! `pm edit`/`pm app`, see it — no rebuild. (That is how AGT-1403..1405
//! develop the real views; a file they add must also be listed in
//! [`FILES`], which `every_view_file_is_embedded` checks.)

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::Context;

use crate::exit::{CliError, Result};
use crate::workspace::Env;

/// Every file under `crates/pm/views/`, by its path relative to it.
pub(crate) const FILES: &[(&str, &str)] = &[
    ("board.tsx", include_str!("../../views/board.tsx")),
    ("ticket.tsx", include_str!("../../views/ticket.tsx")),
    ("lib/board.ts", include_str!("../../views/lib/board.ts")),
    ("lib/pm.ts", include_str!("../../views/lib/pm.ts")),
    ("lib/body.ts", include_str!("../../views/lib/body.ts")),
    ("vendor/loro.js", include_str!("../../views/vendor/loro.js")),
    (
        "vendor/codemirror.js",
        include_str!("../../views/vendor/codemirror.js"),
    ),
];

/// FNV-1a over every file's path and bytes: the unpacked directory's name,
/// so a `pm` with different views never mounts a stale copy.
fn fingerprint() -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for (name, text) in FILES {
        for byte in name.bytes().chain([0]).chain(text.bytes()).chain([0]) {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
    hash
}

/// The absolute `viewsRoot` to mount: `PM_VIEWS_DIR`, else the built-in
/// views unpacked into the cache.
pub(crate) fn root(env: &Env) -> Result<PathBuf> {
    if let Some(dir) = &env.pm_views_dir {
        let dir = std::path::absolute(dir)
            .with_context(|| format!("resolving PM_VIEWS_DIR {}", dir.display()))?;
        if !dir.is_dir() {
            return Err(CliError::error(format!(
                "PM_VIEWS_DIR {} is not a directory",
                dir.display()
            )));
        }
        return Ok(dir);
    }
    let base = env.cache_dir()?.join("views");
    unpack(&base)
}

/// `base/<fingerprint>`, written if it is not there yet.
fn unpack(base: &Path) -> Result<PathBuf> {
    let dir = base.join(format!("{:016x}", fingerprint()));
    if dir.is_dir() {
        return Ok(dir);
    }
    fs::create_dir_all(base).with_context(|| format!("creating {}", base.display()))?;
    let nonce = ulid::Ulid::new();
    let tmp = base.join(format!(".tmp-{nonce}"));
    let written = (|| -> std::io::Result<()> {
        for (name, text) in FILES {
            let path = tmp.join(name);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::write(&path, text)?;
        }
        Ok(())
    })();
    if let Err(e) = written {
        let _ = fs::remove_dir_all(&tmp);
        return Err(anyhow::Error::new(e)
            .context(format!(
                "unpacking the ui-leaf views into {}",
                tmp.display()
            ))
            .into());
    }
    if let Err(e) = fs::rename(&tmp, &dir) {
        let _ = fs::remove_dir_all(&tmp);
        // Another pm unpacked the same views first: theirs is identical.
        if !dir.is_dir() {
            return Err(anyhow::Error::new(e)
                .context(format!("moving the ui-leaf views to {}", dir.display()))
                .into());
        }
    }
    Ok(dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Walks `dir`, returning every file's path relative to `root`.
    fn walk(root: &Path, dir: &Path, out: &mut Vec<String>) {
        for entry in fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                walk(root, &path, out);
            } else {
                let rel = path.strip_prefix(root).unwrap();
                out.push(rel.to_string_lossy().replace('\\', "/"));
            }
        }
    }

    #[test]
    fn every_view_file_is_embedded() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("views");
        let mut on_disk = Vec::new();
        walk(&root, &root, &mut on_disk);
        on_disk.sort();
        let mut embedded: Vec<String> = FILES.iter().map(|(n, _)| n.to_string()).collect();
        embedded.sort();
        assert_eq!(on_disk, embedded, "crates/pm/views and views::FILES differ");
    }

    #[test]
    fn unpacking_is_idempotent_and_complete() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().join("views");
        let first = unpack(&base).unwrap();
        let second = unpack(&base).unwrap();
        assert_eq!(first, second);
        for (name, text) in FILES {
            assert_eq!(fs::read_to_string(first.join(name)).unwrap(), *text);
        }
        // Only the fingerprinted directory: no temp directory left behind.
        assert_eq!(fs::read_dir(&base).unwrap().count(), 1);
    }

    #[test]
    fn pm_views_dir_wins_and_must_exist() {
        let tmp = tempfile::tempdir().unwrap();
        let env = Env {
            pm_views_dir: Some(tmp.path().to_path_buf()),
            ..Env::default()
        };
        assert_eq!(root(&env).unwrap(), tmp.path());
        let env = Env {
            pm_views_dir: Some(tmp.path().join("missing")),
            ..Env::default()
        };
        assert!(root(&env).is_err());
    }
}
