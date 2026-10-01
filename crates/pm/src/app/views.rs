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
//! `views/vendor/` is third-party code the editors (ticket and project) bundle —
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
    ("project.tsx", include_str!("../../views/project.tsx")),
    (
        "initiatives.tsx",
        include_str!("../../views/initiatives.tsx"),
    ),
    ("lib/board.ts", include_str!("../../views/lib/board.ts")),
    ("lib/project.ts", include_str!("../../views/lib/project.ts")),
    (
        "lib/initiatives.ts",
        include_str!("../../views/lib/initiatives.ts"),
    ),
    (
        "lib/projectpage.tsx",
        include_str!("../../views/lib/projectpage.tsx"),
    ),
    (
        "lib/boardpage.tsx",
        include_str!("../../views/lib/boardpage.tsx"),
    ),
    ("lib/editor.tsx", include_str!("../../views/lib/editor.tsx")),
    ("lib/pm.ts", include_str!("../../views/lib/pm.ts")),
    ("lib/body.ts", include_str!("../../views/lib/body.ts")),
    ("vendor/loro.js", include_str!("../../views/vendor/loro.js")),
    (
        "vendor/codemirror.js",
        include_str!("../../views/vendor/codemirror.js"),
    ),
];

/// SHA-256 over every file's path and bytes, in `FILES` order, each
/// length-prefixed so no two file sets share a framing. (AGT-1465: this
/// was FNV-1a, which is not collision-resistant, and [`verified`] leans on
/// it to decide whether a cache directory may be mounted.) Lowercase hex.
fn fingerprint_of<'a>(files: impl Iterator<Item = (&'a str, &'a [u8])>) -> String {
    use sha2::{Digest, Sha256};
    use std::fmt::Write;
    let mut hasher = Sha256::new();
    for (name, bytes) in files {
        hasher.update((name.len() as u64).to_le_bytes());
        hasher.update(name.as_bytes());
        hasher.update((bytes.len() as u64).to_le_bytes());
        hasher.update(bytes);
    }
    hasher.finalize().iter().fold(String::new(), |mut hex, b| {
        let _ = write!(hex, "{b:02x}");
        hex
    })
}

/// The embedded views' fingerprint: the unpacked directory's name, so a
/// `pm` with different views never mounts a stale copy.
fn fingerprint() -> String {
    fingerprint_of(FILES.iter().map(|(n, t)| (*n, t.as_bytes())))
}

/// True when `dir` holds exactly the embedded files: the same fingerprint
/// computed over what is on disk, and no extra files. The directory name
/// only proves which `pm` wrote it; this proves nobody (or a half-deleted
/// cache, or a hand edit) changed it since, before ui-leaf compiles and
/// serves it (AGT-1452).
fn verified(dir: &Path) -> bool {
    let mut contents = Vec::with_capacity(FILES.len());
    for (name, _) in FILES {
        match fs::read(dir.join(name)) {
            Ok(bytes) => contents.push((*name, bytes)),
            Err(_) => return false,
        }
    }
    let disk = fingerprint_of(contents.iter().map(|(n, b)| (*n, b.as_slice())));
    disk == fingerprint() && file_count(dir) == Some(FILES.len())
}

/// Number of files under `dir`, recursively (`None` on an I/O error).
fn file_count(dir: &Path) -> Option<usize> {
    let mut n = 0;
    for entry in fs::read_dir(dir).ok()? {
        let path = entry.ok()?.path();
        if path.is_dir() {
            n += file_count(&path)?;
        } else {
            n += 1;
        }
    }
    Some(n)
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
    let dir = base.join(fingerprint());
    if dir.is_dir() {
        if verified(&dir) {
            return Ok(dir);
        }
        // Tampered or damaged: set it aside (an atomic rename, so a
        // concurrent pm never sees a half-deleted directory), then
        // re-unpack below.
        let stale = base.join(format!(".stale-{}", ulid::Ulid::new()));
        if fs::rename(&dir, &stale).is_ok() {
            let _ = fs::remove_dir_all(&stale);
        } else {
            fs::remove_dir_all(&dir)
                .with_context(|| format!("removing the damaged view cache {}", dir.display()))?;
        }
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
    fn the_fingerprint_is_sha256_hex_with_unambiguous_framing() {
        let a = fingerprint_of([("ab", b"c".as_slice())].into_iter());
        let b = fingerprint_of([("a", b"bc".as_slice())].into_iter());
        assert_eq!(a.len(), 64);
        assert!(a.bytes().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b);
        // SHA-256 of the empty file set is the empty-input digest.
        assert_eq!(
            fingerprint_of(std::iter::empty()),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn a_tampered_cache_is_repacked() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().join("views");
        let dir = unpack(&base).unwrap();
        // Edited file, deleted file and an extra file each invalidate it.
        fs::write(dir.join("vendor/loro.js"), "evil()").unwrap();
        assert!(!verified(&dir));
        assert_eq!(unpack(&base).unwrap(), dir);
        assert!(verified(&dir));
        fs::remove_file(dir.join("board.tsx")).unwrap();
        assert!(!verified(&dir));
        unpack(&base).unwrap();
        assert!(verified(&dir));
        fs::write(dir.join("extra.js"), "x").unwrap();
        assert!(!verified(&dir));
        unpack(&base).unwrap();
        assert!(verified(&dir));
        for (name, text) in FILES {
            assert_eq!(fs::read_to_string(dir.join(name)).unwrap(), *text);
        }
        // No leftover temp or stale directories.
        assert_eq!(fs::read_dir(&base).unwrap().count(), 1);
    }

    /// `views-vendor/vendor.sha256` (written by `build.ts`) pins the
    /// committed bundles; a hand-edited one fails here, offline.
    #[test]
    fn vendored_bundles_match_manifest() {
        use sha2::{Digest, Sha256};
        let crate_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
        let manifest = fs::read_to_string(crate_dir.join("views-vendor/vendor.sha256")).unwrap();
        let mut pinned = Vec::new();
        for line in manifest.lines() {
            let (digest, name) = line.split_once("  ").expect("`<sha256>  <file>` lines");
            let bytes = fs::read(crate_dir.join("views/vendor").join(name))
                .unwrap_or_else(|e| panic!("reading vendor/{name}: {e}"));
            let actual: String = Sha256::digest(&bytes)
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect();
            assert_eq!(
                actual, digest,
                "vendor/{name} differs from views-vendor/vendor.sha256: regenerate it \
                 with `bun install --frozen-lockfile && bun build.ts` in \
                 crates/pm/views-vendor, never by hand"
            );
            pinned.push(name.to_string());
        }
        pinned.sort();
        let mut on_disk: Vec<String> = fs::read_dir(crate_dir.join("views/vendor"))
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        on_disk.sort();
        assert_eq!(
            pinned, on_disk,
            "vendor/ and vendor.sha256 list different files"
        );
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
