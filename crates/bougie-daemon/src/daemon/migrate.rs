//! Carrying a service's data from one instance version to another.
//!
//! Instances are keyed by exact version (`state/services/<name>/<version>/`),
//! so a project that moves to another version starts on an empty
//! instance unless its data comes along. This module does that move:
//! stop the old instance, copy its data directory into the new one, and
//! move the project's tenant row so every ledger scan finds it on the
//! new instance.
//!
//! The old instance's data is copied, never moved. `OpenSearch` 3.x opens
//! a 2.x data directory in place, but rewrites it so that 2.x can no
//! longer open it, so the untouched copy is what makes going back
//! possible.
//!
//! Only `OpenSearch` is supported for now. The database services need an
//! upgrade step of their own after the copy (`mariadb-upgrade`), which
//! is not done here.

use crate::daemon::catalog::{self, CatalogEntry, Tenancy};
use crate::daemon::instance::Instance;
use crate::daemon::state::DaemonState;
use crate::daemon::tenants;
use eyre::{Result, WrapErr, eyre};
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Which tenant rows follow the data to the new instance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rows {
    /// Only the requesting project's. Used for an upgrade to another
    /// release line, which each project asks for on its own; the old
    /// instance keeps serving everyone else.
    Project,
    /// Every row in the old ledger. Used for a patch bump on the same
    /// line, where the whole instance moves: every project sharing it
    /// resolves to the new patch on its next `up` anyway, and the copy
    /// already holds their indices.
    All,
}

/// What a migration did, returned to the CLI as is.
#[derive(Debug, Serialize)]
pub struct Migrated {
    pub service: String,
    pub from: String,
    pub to: String,
    /// The old instance was running and has been started again because
    /// other projects still use it.
    pub restarted_from: bool,
}

/// Whether `entry` can have its data carried between versions here.
#[must_use]
pub fn supported(entry: &CatalogEntry) -> bool {
    matches!(entry.tenancy, Tenancy::Opensearch)
}

/// The directory holding the service's own data (`path.data` for
/// `OpenSearch`), inside the instance's data root.
fn data_dir(state: &DaemonState, entry: &CatalogEntry, version: &str) -> PathBuf {
    state.paths.service_data(entry.name, version).join("data")
}

/// True when `dir` is missing or empty: an instance that has never
/// stored anything, which a migration may fill.
pub async fn is_fresh(dir: &Path) -> bool {
    match tokio::fs::read_dir(dir).await {
        Ok(mut rd) => matches!(rd.next_entry().await, Ok(None)),
        Err(_) => true,
    }
}

/// Copy `from`'s data into `to` and move tenant rows, per [`Rows`].
///
/// Refuses when `to` already holds data: merging two clusters' data
/// directories is not possible, and overwriting would destroy whatever
/// `to` has.
pub async fn migrate(
    state: &Arc<DaemonState>,
    entry: &'static CatalogEntry,
    project: &Path,
    from: &str,
    to: &str,
    rows: Rows,
) -> Result<Migrated> {
    if !supported(entry) {
        return Err(eyre!(
            "{}: carrying data between versions is not supported for this service yet",
            entry.name
        ));
    }
    if from == to {
        return Err(eyre!("{}: already on {to}", entry.name));
    }
    let src = data_dir(state, entry, from);
    let dst = data_dir(state, entry, to);
    if is_fresh(&src).await {
        return Err(eyre!(
            "{} {from} has no data at {} to carry over",
            entry.name,
            src.display()
        ));
    }
    if !is_fresh(&dst).await {
        return Err(eyre!(
            "{} {to} already has data at {}; remove it first if it can go \
             (`bougie service down --purge` from the projects using it)",
            entry.name,
            dst.display()
        ));
    }

    // The data directory is only consistent while nothing writes to it.
    let from_inst = Instance::new(entry.name, from);
    let was_running = state
        .supervisor
        .lock()
        .await
        .stop(&from_inst)
        .await
        .wrap_err_with(|| format!("stopping {} {from}", entry.name))?;

    let copied = copy_tree(&src, &dst).await;
    let moved = match &copied {
        Ok(()) => move_rows(state, entry, project, from, to, rows).await,
        Err(_) => Ok(0),
    };

    // Whatever happened, put the old instance back if others need it.
    let from_ledger = state.paths.service_tenants(entry.name, from);
    let others_remain = tenants::load_all(&from_ledger)
        .await
        .is_ok_and(|all| !all.is_empty());
    let restarted_from = was_running && others_remain;
    if restarted_from {
        crate::daemon::supervisor::start_service(&state.supervisor, &from_inst)
            .await
            .wrap_err_with(|| format!("restarting {} {from}", entry.name))?;
    }

    copied.wrap_err_with(|| {
        format!(
            "copying {} data {} → {}",
            entry.name,
            src.display(),
            dst.display()
        )
    })?;
    moved.wrap_err("moving tenant rows")?;
    tracing::info!(service = entry.name, from, to, "carried instance data over");
    Ok(Migrated {
        service: entry.name.to_owned(),
        from: from.to_owned(),
        to: to.to_owned(),
        restarted_from,
    })
}

/// The newest other instance on `to`'s release line that holds data and
/// lists `project`, i.e. the instance a patch bump should take over.
/// `None` when `to` already has data or there is nothing to take over.
pub async fn same_line_source(
    state: &DaemonState,
    entry: &CatalogEntry,
    project: &Path,
    to: &str,
) -> Option<String> {
    if !supported(entry) || !is_fresh(&data_dir(state, entry, to)).await {
        return None;
    }
    let line = catalog::release_line(to)?;
    for v in tenants::project_instance_versions(&state.paths, entry.name, project) {
        if v != to
            && catalog::release_line(&v) == Some(line)
            && !is_fresh(&data_dir(state, entry, &v)).await
        {
            return Some(v);
        }
    }
    None
}

/// Called on every `up` before the instance starts: when `to` is a
/// fresh instance on a release line where `project` already has data
/// (a patch bump), carry that instance over whole ([`Rows::All`]).
/// Moving between lines is never done here, only through
/// `service.migrate`, which the user asks for.
pub async fn take_over_same_line(
    state: &Arc<DaemonState>,
    entry: &'static CatalogEntry,
    project: &Path,
    to: &str,
) -> Result<()> {
    let Some(from) = same_line_source(state, entry, project, to).await else {
        return Ok(());
    };
    migrate(state, entry, project, &from, to, Rows::All)
        .await
        .wrap_err_with(|| format!("taking over {from}'s data for {to}"))?;
    Ok(())
}

/// Move rows from `from`'s ledger to `to`'s. Rows already in `to`'s
/// ledger for the same project are left alone.
async fn move_rows(
    state: &DaemonState,
    entry: &CatalogEntry,
    project: &Path,
    from: &str,
    to: &str,
    rows: Rows,
) -> Result<usize> {
    let from_ledger = state.paths.service_tenants(entry.name, from);
    let to_ledger = state.paths.service_tenants(entry.name, to);
    let moving = |t: &tenants::Tenant| rows == Rows::All || t.project == project;
    let existing = tenants::load_all(&to_ledger).await?;
    for t in tenants::load_all(&from_ledger).await? {
        if moving(&t) && !existing.iter().any(|e| e.project == t.project) {
            tenants::append(&to_ledger, &t).await?;
        }
    }
    tenants::rewrite(&from_ledger, |t| !moving(t)).await
}

/// Copy `src`'s contents into `dst`, cloning blocks where the
/// filesystem can (btrfs/XFS on Linux, APFS on macOS) so a multi-GB
/// index directory costs little time or space. Falls back to a plain
/// copy where cloning is unsupported.
async fn copy_tree(src: &Path, dst: &Path) -> Result<()> {
    tokio::fs::create_dir_all(dst)
        .await
        .wrap_err_with(|| format!("creating {}", dst.display()))?;
    // `src/.` copies the directory's contents, dot-files included, into
    // the existing `dst`.
    let from = src.join(".");
    let attempts: &[&[&str]] = if cfg!(target_os = "macos") {
        &[&["-c", "-R", "-p"], &["-R", "-p"]]
    } else {
        &[&["-a", "--reflink=auto"]]
    };
    let mut last = String::new();
    for args in attempts {
        let out = tokio::process::Command::new("cp")
            .args(*args)
            .arg(&from)
            .arg(dst)
            .output()
            .await
            .wrap_err("running cp")?;
        if out.status.success() {
            return Ok(());
        }
        String::from_utf8_lossy(&out.stderr)
            .trim()
            .clone_into(&mut last);
    }
    Err(eyre!("cp failed: {last}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn copy_tree_copies_contents_into_an_existing_dir() {
        let td = tempfile::TempDir::new().unwrap();
        let src = td.path().join("src");
        std::fs::create_dir_all(src.join("nodes/0/indices")).unwrap();
        std::fs::write(src.join("nodes/0/indices/seg"), b"lucene").unwrap();
        std::fs::write(src.join(".hidden"), b"x").unwrap();
        let dst = td.path().join("dst");
        std::fs::create_dir_all(&dst).unwrap();
        copy_tree(&src, &dst).await.unwrap();
        assert_eq!(
            std::fs::read(dst.join("nodes/0/indices/seg")).unwrap(),
            b"lucene"
        );
        assert!(dst.join(".hidden").is_file());
        // The source is left as it was.
        assert!(src.join("nodes/0/indices/seg").is_file());
    }

    fn state_with(tmp: &Path) -> Arc<DaemonState> {
        let paths = bougie_paths::Paths::new(tmp.into(), tmp.into());
        let (tx, _rx) = tokio::sync::watch::channel(false);
        Arc::new(DaemonState::new(paths, tx))
    }

    /// Give `version` an instance holding `seg` as data, with `projects`
    /// in its ledger.
    async fn instance(state: &DaemonState, version: &str, seg: &str, projects: &[&str]) {
        let data = state.paths.service_data("opensearch", version).join("data");
        std::fs::create_dir_all(data.join("nodes/0")).unwrap();
        std::fs::write(data.join("nodes/0/seg"), seg).unwrap();
        let ledger = state.paths.service_tenants("opensearch", version);
        for p in projects {
            tenants::append(
                &ledger,
                &tenants::Tenant::new(p.trim_start_matches('/'), *p),
            )
            .await
            .unwrap();
        }
    }

    async fn ledger(state: &DaemonState, version: &str) -> Vec<PathBuf> {
        tenants::load_all(&state.paths.service_tenants("opensearch", version))
            .await
            .unwrap()
            .into_iter()
            .map(|t| t.project)
            .collect()
    }

    fn seg(state: &DaemonState, version: &str) -> String {
        std::fs::read_to_string(
            state
                .paths
                .service_data("opensearch", version)
                .join("data/nodes/0/seg"),
        )
        .unwrap()
    }

    #[tokio::test]
    async fn an_upgrade_copies_the_data_and_moves_only_that_project() {
        let tmp = tempfile::TempDir::new().unwrap();
        let state = state_with(tmp.path());
        let entry = catalog::find("opensearch").unwrap();
        instance(&state, "2.19.5", "v2", &["/a", "/b"]).await;

        let a = Path::new("/a");
        let done = migrate(&state, entry, a, "2.19.5", "3.10.1", Rows::Project)
            .await
            .unwrap();
        assert_eq!((done.from.as_str(), done.to.as_str()), ("2.19.5", "3.10.1"));
        assert_eq!(seg(&state, "3.10.1"), "v2");
        // The old data stays for a rollback, and /b stays on it.
        assert_eq!(seg(&state, "2.19.5"), "v2");
        assert_eq!(ledger(&state, "2.19.5").await, [PathBuf::from("/b")]);
        assert_eq!(ledger(&state, "3.10.1").await, [PathBuf::from("/a")]);

        // Nothing is copied over an instance that has data.
        let b = Path::new("/b");
        let err = migrate(&state, entry, b, "2.19.5", "3.10.1", Rows::Project)
            .await
            .unwrap_err();
        assert!(format!("{err:#}").contains("already has data"), "{err:#}");
    }

    #[tokio::test]
    async fn a_patch_bump_takes_the_whole_instance_over() {
        let tmp = tempfile::TempDir::new().unwrap();
        let state = state_with(tmp.path());
        let entry = catalog::find("opensearch").unwrap();
        instance(&state, "2.19.4", "old", &["/a", "/b"]).await;
        instance(&state, "3.10.1", "v3", &["/c"]).await;

        // Another line is never taken over by `up`.
        take_over_same_line(&state, entry, Path::new("/c"), "2.19.5")
            .await
            .unwrap();
        let data = state
            .paths
            .service_data("opensearch", "2.19.5")
            .join("data");
        assert!(is_fresh(&data).await);

        take_over_same_line(&state, entry, Path::new("/a"), "2.19.5")
            .await
            .unwrap();
        assert_eq!(seg(&state, "2.19.5"), "old");
        assert_eq!(
            ledger(&state, "2.19.5").await,
            [PathBuf::from("/a"), PathBuf::from("/b")]
        );
        assert!(ledger(&state, "2.19.4").await.is_empty());
        // Once the new instance has data, a later `up` leaves it alone.
        take_over_same_line(&state, entry, Path::new("/b"), "2.19.5")
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn fresh_means_missing_or_empty() {
        let td = tempfile::TempDir::new().unwrap();
        assert!(is_fresh(&td.path().join("nope")).await);
        assert!(is_fresh(td.path()).await);
        std::fs::write(td.path().join("f"), b"").unwrap();
        assert!(!is_fresh(td.path()).await);
    }
}
