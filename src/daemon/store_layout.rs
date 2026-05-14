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
//! Phase 6 expects the tarball directory to already exist; the
//! auto-fetch flow is a follow-up. Tests pre-populate it via
//! `tests/common/mariadb_fixture.rs` (real tarball) or by laying
//! out fake binaries (fake-redis fixture).

use super::catalog::CatalogEntry;
use crate::Paths;
use eyre::{eyre, Result};
use std::path::PathBuf;

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
         Tarball auto-fetch is not yet wired (Phase 3 follow-up).",
        entry.name,
        entry.tarball,
        store.display(),
    ))
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
}
