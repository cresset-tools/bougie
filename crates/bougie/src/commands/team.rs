//! Team-mode project state: which sconce registry a project is wired to, and
//! self-healing its Composer repo-config overlay after a `vendor/` wipe.
//!
//! When `bougie login` provisions a project's private repositories into the
//! `vendor/bougie/repositories.json` overlay, it also records *which registry*
//! the project logged in against, in a durable per-project file under
//! `$BOUGIE_HOME/state/projects/<hash>/team.json`. Like the tenant cache in
//! [`crate::commands::tenant`], that record lives outside `vendor/`, so it
//! survives `rm -rf vendor`.
//!
//! `bougie sync` reads it back: if the overlay was wiped along with `vendor/`,
//! it re-discovers the org's repositories from the recorded registry (reusing
//! the stored login token) and rewrites the overlay — so private packages
//! resolve again with no manual `bougie login`. Sync is where the overlay is
//! consumed (it feeds resolution), so it's the natural place to restore it;
//! `bougie start` heals for free via its sync prologue. The whole path is
//! best-effort: not a team project, overlay already intact, logged out, or the
//! registry unreachable all degrade to (at most) a one-line note and never
//! block the sync.
//!
//! Today the registry is the org the dev logged into (M4's `/api/v1/repos`); a
//! later step keys team config off the project's git remote via a served
//! manifest, at which point the record grows beyond a bare registry URL.

use bougie_composer_resolver::metadata::auth_origin;
use bougie_composer_resolver::update::{
    read_bougie_bearer, repositories_overlay_path, write_repositories_overlay,
};
use bougie_paths::Paths;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// The on-disk shape of `team.json`. Bumped if the layout changes.
const RECORD_SCHEMA_VERSION: u32 = 1;

/// The durable per-project record wiring a project to its sconce registry.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TeamRecord {
    /// On-disk schema version (see [`RECORD_SCHEMA_VERSION`]).
    pub schema_version: u32,
    /// The sconce registry base URL the project logged in against (no trailing
    /// slash), e.g. `https://packages.acme.example`. `bougie start`
    /// re-provisions the overlay from `<registry>/api/v1/repos`.
    pub registry: String,
}

/// `$BOUGIE_HOME/state/projects/<hash>/team.json` for `project_root`. `None`
/// when the state root can't be resolved (no `BOUGIE_HOME`/`HOME`), which the
/// best-effort callers treat as "no record".
fn record_path(project_root: &Path) -> Option<PathBuf> {
    let paths = Paths::from_env().ok()?;
    Some(paths.project_state_dir(project_root).join("team.json"))
}

/// Read the durable team record for `project_root`, if any. Best-effort: a
/// missing, unreadable, or malformed file is simply "no record".
#[must_use]
pub fn read_record(project_root: &Path) -> Option<TeamRecord> {
    read_record_at(&record_path(project_root)?)
}

fn read_record_at(path: &Path) -> Option<TeamRecord> {
    let bytes = std::fs::read(path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// Persist `registry` as `project_root`'s team record so `bougie start` can
/// self-heal the overlay after a `vendor/` wipe. Best-effort — a write failure
/// only costs a future self-heal, so it never propagates. No-ops when the file
/// already records the same registry.
pub fn write_record(project_root: &Path, registry: &str) {
    let Some(path) = record_path(project_root) else {
        return;
    };
    write_record_at(&path, registry);
}

fn write_record_at(path: &Path, registry: &str) {
    let record = TeamRecord {
        schema_version: RECORD_SCHEMA_VERSION,
        registry: registry.trim_end_matches('/').to_string(),
    };
    if read_record_at(path).as_ref() == Some(&record) {
        return;
    }
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(mut s) = serde_json::to_string_pretty(&record) {
        s.push('\n');
        let _ = std::fs::write(path, s);
    }
}

/// Re-provision `project_root`'s repositories overlay when it's missing — the
/// `rm -rf vendor` self-heal. Called at the top of `bougie sync`, before
/// resolution, so private packages resolve (and so `bougie start` heals via its
/// sync prologue). Entirely best-effort: not a team project, overlay already
/// present, logged out, or registry unreachable each degrade to (at most) a
/// one-line stderr note and never affect the sync's outcome.
pub fn heal_overlay(project_root: &Path) {
    // Not a team project (never logged in here in overlay mode) → nothing to do.
    let Some(record) = read_record(project_root) else {
        return;
    };
    // Overlay intact → keep sync fast and offline; only a wipe (missing file)
    // reaches for the network.
    if repositories_overlay_path(project_root).exists() {
        return;
    }
    match reprovision(project_root, &record.registry) {
        Ok(0) => {}
        Ok(n) => eprintln!(
            "Restored {n} Composer repositor{} from {} (vendor/ was wiped).",
            if n == 1 { "y" } else { "ies" },
            record.registry
        ),
        Err(e) => eprintln!(
            "warning: couldn't restore this project's Composer repositories from {} ({e}). \
             Private packages may fail to install — run `bougie login` to re-authenticate.",
            record.registry
        ),
    }
}

/// Discover the stored login token's repositories at `registry` and (re)write
/// the overlay in `root`; returns how many entries were written. Errors on a
/// missing token or failed discovery so the caller can surface a note.
fn reprovision(root: &Path, registry: &str) -> eyre::Result<usize> {
    use eyre::{WrapErr, eyre};
    let base = registry.trim_end_matches('/');
    let host = auth_origin(base);
    let token = read_bougie_bearer(&host).ok_or_else(|| eyre!("not logged in to {host}"))?;
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(30))
        .user_agent(format!("bougie/{}", env!("CARGO_PKG_VERSION")))
        .build()
        .wrap_err("building http client")?;
    let urls = crate::commands::login::fetch_repo_urls(&client, base, &token)?;
    let (_path, added) =
        write_repositories_overlay(root, &urls).map_err(|e| eyre!("writing overlay: {e}"))?;
    Ok(added)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_round_trips_and_strips_trailing_slash() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("team.json");
        write_record_at(&path, "https://packages.acme.example/");
        let got = read_record_at(&path).expect("record written");
        assert_eq!(got.schema_version, RECORD_SCHEMA_VERSION);
        assert_eq!(got.registry, "https://packages.acme.example");
    }

    #[test]
    fn read_absent_is_none() {
        let dir = tempfile::tempdir().unwrap();
        assert!(read_record_at(&dir.path().join("nope.json")).is_none());
    }

    #[test]
    fn read_malformed_is_none() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("team.json");
        std::fs::write(&path, b"not json").unwrap();
        assert!(read_record_at(&path).is_none());
    }

    #[test]
    fn write_is_idempotent_noop_when_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("team.json");
        write_record_at(&path, "https://reg.example");
        let first = std::fs::read_to_string(&path).unwrap();
        // A same-registry rewrite must not change the bytes on disk.
        write_record_at(&path, "https://reg.example/");
        let second = std::fs::read_to_string(&path).unwrap();
        assert_eq!(first, second);
    }

    #[test]
    fn write_updates_when_registry_changes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("team.json");
        write_record_at(&path, "https://old.example");
        write_record_at(&path, "https://new.example");
        assert_eq!(
            read_record_at(&path).unwrap().registry,
            "https://new.example"
        );
    }
}
