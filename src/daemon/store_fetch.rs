//! Auto-fetch service tarballs into the content-addressed store.
//!
//! Backstop for `bougie services up` when the catalog tarball isn't
//! yet on disk. Mirrors the extension fetch path
//! (`install::install_extension`): index root → tool section →
//! manifest → blob, sha-verified at every step.
//!
//! Two design points worth flagging:
//!
//! * **Catalog version wins over `artifacts[0]`.** The index may
//!   publish multiple versions per tool (e.g. `mariadb-11.4.10`
//!   alongside the catalog's `mariadb-11.4.4`). We resolve the
//!   artifact whose `version` equals the catalog pin so the extracted
//!   directory's name matches the one `store_layout::basedir` is going
//!   to look up. Picking `artifacts[0]` would silently install a
//!   different major version under a non-matching directory name and
//!   surface as a confusing "tarball not found" on the next start.
//!   See cresset-tools/bougie#29.
//!
//! * **Async-friendly.** The fetch + extract is CPU- and IO-bound and
//!   uses `reqwest::blocking`, so the whole thing runs on the blocking
//!   pool via `tokio::task::spawn_blocking`. The caller (`dispatch_up`
//!   in the IPC dispatcher) awaits a future and never blocks the
//!   tokio worker thread itself.

use crate::daemon::catalog::CatalogEntry;
use crate::daemon::store_layout;
use crate::errors::BougieError;
use crate::fetch::{fetch_blob, BlobSpec, DownloadBar};
use crate::index::{
    build_verifier,
    fetch::{fetch_manifest, fetch_root, fetch_section},
    wire::Artifact,
};
use crate::install::{host_to_dirname, DEFAULT_INDEX_URL};
use crate::lock::ExclusiveGuard;
use crate::paths::Paths;
use crate::target::Triple;
use eyre::{eyre, Result, WrapErr};
use std::time::Duration;

const LOCK_TIMEOUT: Duration = Duration::from_mins(1);

/// Make sure the service tarball named by `entry.tarball` is present
/// under `$BOUGIE_HOME/store`. No-op when already on disk; fetches
/// from the configured index otherwise.
///
/// Runs the blocking fetch + extract on the tokio blocking pool so
/// the daemon's main reactor isn't stalled across what can be a
/// multi-hundred-MB download.
pub async fn ensure_tarball(paths: &Paths, entry: &'static CatalogEntry) -> Result<()> {
    // The `server` catalog entry reuses the bougie binary itself —
    // nothing to fetch. Same fast-path as `store_layout::basedir`'s
    // empty-tarball check.
    if entry.tarball.is_empty() {
        return Ok(());
    }
    if store_layout::basedir(paths, entry).is_ok() {
        return Ok(());
    }
    let paths = paths.clone();
    tokio::task::spawn_blocking(move || fetch_blocking(&paths, entry))
        .await
        .wrap_err("joining tarball-fetch task")?
}

fn fetch_blocking(paths: &Paths, entry: &CatalogEntry) -> Result<()> {
    let target = Triple::detect()?.to_string();
    let host = std::env::var("BOUGIE_INDEX_URL").unwrap_or_else(|_| DEFAULT_INDEX_URL.into());

    let _guard = ExclusiveGuard::acquire(&paths.global_lock(), LOCK_TIMEOUT)?;

    // Re-check under the lock: a concurrent `bougie services up`
    // could have populated the store while we were queued behind
    // it, in which case there's nothing left to do and skipping the
    // network round-trip is the polite choice.
    if store_layout::basedir(paths, entry).is_ok() {
        return Ok(());
    }

    let client = reqwest::blocking::Client::builder()
        .build()
        .map_err(|e| BougieError::Network {
            operation: "building HTTP client".into(),
            detail: e.to_string(),
        })?;

    let cache_root = paths.cache_index(&host_to_dirname(&host));
    let fetched = fetch_root(&client, &host, &cache_root, build_verifier)?;
    let section_name = format!("tool/{}", entry.name);
    let target_entry = fetched.root.targets.get(&target).ok_or_else(|| {
        let available: Vec<String> = fetched.root.targets.keys().cloned().collect();
        BougieError::UnknownTarget {
            triple: target.clone(),
            hint: format!("the index at {host} advertises: {}", available.join(", ")),
        }
    })?;
    let section_ref =
        target_entry.sections.get(&section_name).ok_or_else(|| BougieError::Resolution {
            kind: "service".into(),
            detail: format!(
                "the index at {host} has no `{section_name}` section under target {target}"
            ),
        })?;
    let section = fetch_section(
        &client,
        &host,
        &cache_root,
        &fetched.root.version,
        &target,
        &section_name,
        &section_ref.sha256,
    )?;

    let artifact = pick_pinned_artifact(&section.artifacts, entry.version).ok_or_else(|| {
        let published: Vec<&str> = section
            .artifacts
            .iter()
            .filter(|a| !a.yanked)
            .map(|a| a.version.as_str())
            .collect();
        eyre!(
            "the index at {host} doesn't publish `{}` at version `{}` for target {target}; \
             available versions: {}. The bougie catalog and the index are out of sync \
             — upgrade bougie (`bougie self upgrade`) or pin the catalog version that \
             the index still ships.",
            entry.name,
            entry.version,
            if published.is_empty() {
                "(none)".into()
            } else {
                published.join(", ")
            },
        )
    })?;

    let manifest = fetch_manifest(
        &client,
        &host,
        &cache_root,
        &artifact.manifest.path,
        &artifact.manifest.sha256,
    )?;

    std::fs::create_dir_all(paths.store())
        .wrap_err_with(|| format!("creating {}", paths.store().display()))?;
    let dest = paths.store().join(entry.tarball);

    // Hidden bar: the daemon has no terminal of its own. The CLI
    // side surfaces "(starting bougied)" + a generic up spinner; a
    // proper IPC-streamed progress channel is future work, tracked
    // alongside the rest of the services UX.
    let bar = DownloadBar::hidden();
    bar.add_planned(manifest.blob.size);
    bar.set_current(entry.tarball);

    let spec = BlobSpec {
        url: &manifest.blob.url,
        sha256: &manifest.blob.sha256,
        partial_dir: &paths.cache_blobs(),
        dest: &dest,
        // Tool tarballs ship their tree under `install/` — same
        // convention as the interpreter tarball.
        strip_prefix: "install",
    };
    fetch_blob(&client, &spec, &bar)?;
    bar.finish();
    Ok(())
}

/// Pick the artifact whose `version` equals the catalog pin. Yanked
/// artifacts are skipped; on ties between frozen + unfrozen, frozen
/// wins (the publisher's stamp of intent that this exact build is
/// reproducible).
fn pick_pinned_artifact<'a>(
    artifacts: &'a [Artifact],
    pinned_version: &str,
) -> Option<&'a Artifact> {
    let mut best: Option<&Artifact> = None;
    for a in artifacts {
        if a.yanked || a.version != pinned_version {
            continue;
        }
        match best {
            None => best = Some(a),
            Some(prev) if !prev.frozen && a.frozen => best = Some(a),
            _ => {}
        }
    }
    best
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::wire::ManifestRef;

    fn art(version: &str, yanked: bool, frozen: bool) -> Artifact {
        Artifact {
            tag: format!("{version}-tag"),
            version: version.into(),
            flavor: "default".into(),
            php_minor: None,
            manifest: ManifestRef {
                path: "/p".into(),
                sha256: "00".into(),
            },
            yanked,
            yanked_reason: None,
            frozen,
        }
    }

    #[test]
    fn pick_matches_catalog_pin_not_first_artifact() {
        // Mirrors the live index shape that prompted this fix: a
        // newer unfrozen artifact at position 0 must not shadow the
        // frozen, catalog-pinned older version.
        let arts = vec![art("11.4.10", false, false), art("11.4.4", false, true)];
        let pick = pick_pinned_artifact(&arts, "11.4.4").unwrap();
        assert_eq!(pick.version, "11.4.4");
        assert!(pick.frozen);
    }

    #[test]
    fn pick_skips_yanked() {
        let arts = vec![art("8.6.3", true, false)];
        assert!(pick_pinned_artifact(&arts, "8.6.3").is_none());
    }

    #[test]
    fn pick_no_match_returns_none() {
        let arts = vec![art("11.4.10", false, false)];
        assert!(pick_pinned_artifact(&arts, "11.4.4").is_none());
    }

    #[test]
    fn pick_prefers_frozen_on_duplicate_versions() {
        // Pathological-but-possible: same version with both frozen
        // and unfrozen flags (e.g. mid-republish race). The frozen
        // row is the publisher's stable intent.
        let arts = vec![
            art("8.6.3", false, false), // unfrozen first
            art("8.6.3", false, true),
        ];
        let pick = pick_pinned_artifact(&arts, "8.6.3").unwrap();
        assert!(pick.frozen);
    }

    #[test]
    fn pick_keeps_first_frozen_against_later_unfrozen() {
        let arts = vec![
            art("8.6.3", false, true),  // frozen first
            art("8.6.3", false, false),
        ];
        let pick = pick_pinned_artifact(&arts, "8.6.3").unwrap();
        assert!(pick.frozen);
    }
}
