//! Per-service store directory + binary resolution.
//!
//! Two callers need the same answer: the supervisor (to spawn the
//! service binary) and the per-service provisioners (mariadb's
//! `mariadb-install-db` needs the basedir; opensearch's bootstrap
//! needs to read `config/`). Keeping the logic in one place means
//! a future auto-fetch loop only has to slot in here.
//!
//! Layout (per CLI.md §2.1 / SERVICES.md §2):
//!
//! ```text
//! $BOUGIE_HOME/store/<tarball>[<-hash>]/
//!   bin/<binary>            # main executable
//!   share/...               # data files
//!   lib/...                 # bundled deps
//! ```
//!
//! The first-use auto-fetch lives in [`super::store_fetch`]; the
//! daemon's `service.up` dispatcher pre-populates the store via
//! that path before reaching `basedir()`. Tests pre-populate the
//! tarball directory directly via `tests/common/mariadb_fixture.rs`
//! (real tarball) or by laying out fake binaries (fake-redis fixture).

use super::catalog::CatalogEntry;
use crate::Paths;
use eyre::{eyre, Result, WrapErr};
use std::path::{Path, PathBuf};

/// Locate the service's basedir under `$BOUGIE_HOME/store/`. Prefers
/// the exact `<tarball>` name; falls back to any directory starting
/// with `<tarball>-` (the hash-suffixed form produced by the index).
pub fn basedir(paths: &Paths, entry: &CatalogEntry) -> Result<PathBuf> {
    if entry.tarball.is_empty() {
        // `server` has no tarball — it reuses the bougie binary itself.
        return Err(eyre!(
            "service `{}` has no tarball; use `current_exe()` instead",
            entry.name
        ));
    }
    let store = paths.store();
    let exact = store.join(entry.tarball);
    if exact.is_dir() {
        return Ok(exact);
    }
    let prefix = format!("{}-", entry.tarball);
    if let Ok(rd) = std::fs::read_dir(&store) {
        for ent in rd.flatten() {
            if ent
                .file_name()
                .to_str()
                .is_some_and(|s| s.starts_with(&prefix))
            {
                return Ok(ent.path());
            }
        }
    }
    Err(eyre!(
        "service `{}`: tarball `{}` not found under {}. \
         The daemon's auto-fetch path should have populated this \
         before any basedir() call — reaching here means the fetch \
         was skipped or its sibling rename to `{}` was rolled back.",
        entry.name,
        entry.tarball,
        store.display(),
        entry.tarball,
    ))
}

/// Create `outer_root/<link_into>` as a relative symlink pointing at
/// `inner_root`. Mirrors the posture of `materialize_closure_peer`:
/// idempotent (existing correct symlinks are left alone) and refuses
/// to overwrite a regular file.
///
/// `link_into = ""` is the explicit "install but don't link" case
/// — returns Ok without touching the filesystem. `link_into` may
/// contain `/` separators (e.g. `"runtime/jdk"`); the number of
/// `..` segments in the symlink target scales with the depth so
/// the link still resolves into the shared store sibling.
///
/// Both `outer_root` and `inner_root` are expected to be direct
/// children of `$BOUGIE_HOME/store/`, so the relative target is
/// `../<inner-dirname>` for a one-segment `link_into`.
///
/// Used by `requires_tools` resolution per `UNBUNDLE_PLAN.md` Phase 2:
/// opensearch's `bin/opensearch` invokes `${ES_HOME}/jdk/bin/java`,
/// so the outer install root needs a `jdk` symlink absorbing the
/// redirection into `$BOUGIE_HOME/store/jdk-<version>/`.
pub fn create_link_into(outer_root: &Path, link_into: &str, inner_root: &Path) -> Result<()> {
    if link_into.is_empty() {
        return Ok(());
    }
    let inner_name = inner_root
        .file_name()
        .ok_or_else(|| eyre!("inner_root has no file name component: {}", inner_root.display()))?;

    // Climb out of `link_into`'s segments and back into the shared
    // store. For `link_into = "jdk"`, depth=1 → target = `../<inner>`.
    // For `"runtime/jdk"`, depth=2 → `../../<inner>`.
    let depth = link_into.split('/').filter(|s| !s.is_empty()).count();
    let mut target = PathBuf::new();
    for _ in 0..depth {
        target.push("..");
    }
    target.push(inner_name);

    let link = outer_root.join(link_into);
    if let Some(parent) = link.parent() {
        std::fs::create_dir_all(parent)
            .wrap_err_with(|| format!("creating {}", parent.display()))?;
    }
    match std::fs::symlink_metadata(&link) {
        Ok(meta) if meta.file_type().is_symlink() => Ok(()),
        Ok(_) => Err(eyre!(
            "{} exists but isn't a symlink — refusing to overwrite",
            link.display()
        )),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            std::os::unix::fs::symlink(&target, &link).wrap_err_with(|| {
                format!("symlinking {} → {}", link.display(), target.display())
            })
        }
        Err(e) => Err(eyre!("stat {}: {e}", link.display())),
    }
}

/// Locate the main binary inside the service's store directory.
pub fn binary(paths: &Paths, entry: &CatalogEntry) -> Result<PathBuf> {
    if entry.tarball.is_empty() {
        let exe = std::env::current_exe()
            .map_err(|e| eyre!("locating current bougie binary: {e}"))?;
        return Ok(exe);
    }
    Ok(basedir(paths, entry)?.join(entry.binary))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::catalog;

    #[test]
    fn basedir_finds_exact_match() {
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = Paths::new(tmp.path().into(), tmp.path().into());
        std::fs::create_dir_all(paths.store().join("redis-8.6.3")).unwrap();
        let entry = catalog::find("redis").unwrap();
        let bd = basedir(&paths, entry).unwrap();
        assert!(bd.ends_with("redis-8.6.3"));
    }

    #[test]
    fn basedir_finds_hashed_match() {
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = Paths::new(tmp.path().into(), tmp.path().into());
        std::fs::create_dir_all(paths.store().join("redis-8.6.3-abc123")).unwrap();
        let entry = catalog::find("redis").unwrap();
        let bd = basedir(&paths, entry).unwrap();
        assert!(bd.to_string_lossy().contains("redis-8.6.3-abc123"));
    }

    #[test]
    fn basedir_errors_when_missing() {
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = Paths::new(tmp.path().into(), tmp.path().into());
        let entry = catalog::find("redis").unwrap();
        let err = basedir(&paths, entry).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("tarball"), "{msg}");
        assert!(msg.contains("redis-8.6.3"), "{msg}");
    }

    #[test]
    fn binary_appends_catalog_path() {
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = Paths::new(tmp.path().into(), tmp.path().into());
        std::fs::create_dir_all(paths.store().join("mariadb-11.4.4/bin")).unwrap();
        std::fs::write(paths.store().join("mariadb-11.4.4/bin/mariadbd"), "x").unwrap();
        let entry = catalog::find("mariadb").unwrap();
        let bin = binary(&paths, entry).unwrap();
        assert!(bin.ends_with("mariadb-11.4.4/bin/mariadbd"));
    }

    // ---------- create_link_into (Phase 2) ----------

    #[test]
    fn create_link_into_links_inner_at_one_segment() {
        let td = tempfile::TempDir::new().unwrap();
        let store = td.path();
        let outer = store.join("opensearch-2.19.5");
        let inner = store.join("jdk-21.0.11+10");
        std::fs::create_dir_all(&outer).unwrap();
        std::fs::create_dir_all(inner.join("bin")).unwrap();
        std::fs::write(inner.join("bin/java"), "fake-java").unwrap();

        create_link_into(&outer, "jdk", &inner).unwrap();

        let link = outer.join("jdk");
        let meta = std::fs::symlink_metadata(&link).unwrap();
        assert!(meta.file_type().is_symlink());
        let target = std::fs::read_link(&link).unwrap();
        assert_eq!(target, std::path::PathBuf::from("../jdk-21.0.11+10"));
        // And it actually resolves into the inner install.
        assert!(link.join("bin/java").exists());
    }

    #[test]
    fn create_link_into_handles_nested_link_path() {
        // `link_into = "runtime/jdk"` → link lives two levels deep
        // inside the outer install root, so the relative target needs
        // two `..` segments to climb back to the shared store.
        let td = tempfile::TempDir::new().unwrap();
        let store = td.path();
        let outer = store.join("weirdtool-1.0.0");
        let inner = store.join("jdk-21.0.11+10");
        std::fs::create_dir_all(&outer).unwrap();
        std::fs::create_dir_all(inner.join("bin")).unwrap();
        std::fs::write(inner.join("bin/java"), "fake").unwrap();

        create_link_into(&outer, "runtime/jdk", &inner).unwrap();

        let link = outer.join("runtime/jdk");
        let meta = std::fs::symlink_metadata(&link).unwrap();
        assert!(meta.file_type().is_symlink());
        let target = std::fs::read_link(&link).unwrap();
        assert_eq!(target, std::path::PathBuf::from("../../jdk-21.0.11+10"));
        assert!(link.join("bin/java").exists());
    }

    #[test]
    fn create_link_into_skips_empty_link() {
        let td = tempfile::TempDir::new().unwrap();
        let outer = td.path().join("outer");
        let inner = td.path().join("inner");
        std::fs::create_dir_all(&outer).unwrap();
        std::fs::create_dir_all(&inner).unwrap();
        create_link_into(&outer, "", &inner).unwrap();
        // No symlink, no directory side-effects on the outer root.
        let count = std::fs::read_dir(&outer).unwrap().count();
        assert_eq!(count, 0);
    }

    #[test]
    fn create_link_into_is_idempotent() {
        let td = tempfile::TempDir::new().unwrap();
        let store = td.path();
        let outer = store.join("opensearch-2.19.5");
        let inner = store.join("jdk-21.0.11+10");
        std::fs::create_dir_all(&outer).unwrap();
        std::fs::create_dir_all(&inner).unwrap();
        create_link_into(&outer, "jdk", &inner).unwrap();
        create_link_into(&outer, "jdk", &inner).unwrap();
        let entries: Vec<_> = std::fs::read_dir(&outer)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(entries.len(), 1);
    }

    #[test]
    fn create_link_into_refuses_to_overwrite_regular_file() {
        let td = tempfile::TempDir::new().unwrap();
        let outer = td.path().join("outer");
        let inner = td.path().join("inner");
        std::fs::create_dir_all(&outer).unwrap();
        std::fs::create_dir_all(&inner).unwrap();
        // A plain file occupies the path we'd want to symlink.
        std::fs::write(outer.join("jdk"), "junk").unwrap();
        let err = create_link_into(&outer, "jdk", &inner).unwrap_err();
        assert!(err.to_string().contains("isn't a symlink"), "got: {err}");
    }
}
