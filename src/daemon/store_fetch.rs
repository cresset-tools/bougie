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
    fetch::{fetch_manifest, fetch_root, fetch_section, FetchedRoot},
    wire::{Artifact, Manifest, RequiresTool, Section},
};
use crate::install::{host_to_dirname, install_closure_peers, plan_closure_bytes, DEFAULT_INDEX_URL};
use crate::lock::ExclusiveGuard;
use crate::paths::Paths;
use crate::target::Triple;
use eyre::{eyre, Result, WrapErr};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
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

    let section = fetch_tool_section(&client, &fetched, &host, &cache_root, &target, entry.name)?;
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
    // Reject malformed closure / requires_tools entries up front, before
    // we fetch the main blob. A bad closure URL is cheaper to surface
    // here than after a 100MB download.
    manifest
        .validate()
        .wrap_err_with(|| format!("validating manifest for {}", manifest.tag))?;

    std::fs::create_dir_all(paths.store())
        .wrap_err_with(|| format!("creating {}", paths.store().display()))?;
    let dest = paths.store().join(entry.tarball);

    // Hidden bar: the daemon has no terminal of its own. The CLI
    // side surfaces "(starting bougied)" + a generic up spinner; a
    // proper IPC-streamed progress channel is future work, tracked
    // alongside the rest of the services UX.
    let bar = DownloadBar::hidden();

    // Seed the cycle-detection visited set with the outer tool so a
    // pathological `requires_tools[]` entry pointing back at the outer
    // (`opensearch → opensearch`) surfaces as a clear error rather than
    // silently re-linking the partial install to itself.
    let mut visited = HashSet::new();
    visited.insert((manifest.name.clone(), manifest.version.clone()));

    install_into(
        &client,
        paths,
        &manifest,
        &dest,
        &bar,
        &fetched,
        &host,
        &cache_root,
        &target,
        &mut visited,
    )?;

    bar.finish();
    Ok(())
}

/// The common install body: grow the bar's planned total, fetch the
/// main blob, walk `closure[]`, then recursively install every
/// `requires_tools[]` entry and link it under the outer install root.
///
/// Used from two sites with shared semantics: the outer catalog-driven
/// entry point ([`fetch_blocking`]) and the recursive resolver
/// ([`install_required_tool`]).
#[allow(clippy::too_many_arguments)]
fn install_into(
    client: &reqwest::blocking::Client,
    paths: &Paths,
    manifest: &Manifest,
    install_root: &Path,
    bar: &DownloadBar,
    fetched: &FetchedRoot,
    host: &str,
    cache_root: &Path,
    target: &str,
    visited: &mut HashSet<(String, String)>,
) -> Result<()> {
    bar.add_planned(manifest.blob.size);
    plan_closure_bytes(paths, manifest, bar);
    bar.set_current(manifest.tag.clone());

    let spec = BlobSpec {
        url: &manifest.blob.url,
        sha256: &manifest.blob.sha256,
        partial_dir: &paths.cache_blobs(),
        dest: install_root,
        // Tool tarballs ship their tree under `install/` — same
        // convention as the interpreter tarball.
        strip_prefix: "install",
    };
    fetch_blob(client, &spec, bar)?;

    install_closure_peers(client, paths, manifest, install_root, bar)?;

    for rt in &manifest.requires_tools {
        let inner_root = install_required_tool(
            client, paths, rt, bar, fetched, host, cache_root, target, visited,
        )?;
        store_layout::create_link_into(install_root, &rt.link_into, &inner_root)?;
    }
    Ok(())
}

/// Resolve and install a `requires_tools[]` entry. Returns the inner
/// tool's install root (caller is responsible for creating the
/// `<outer>/<link_into>` symlink — see [`store_layout::create_link_into`]).
///
/// Per `UNBUNDLE_PLAN.md` Phase 2:
///
/// 1. If `$BOUGIE_HOME/store/<name>-<version>/` already exists, return
///    it. The inner tool is already installed (either from a prior
///    `bougie services up` or from an earlier sibling in the same
///    recursive walk — the diamond case).
/// 2. Otherwise mark the (name, version) as in-progress in `visited`,
///    fetch the inner tool's section to resolve the manifest sha256
///    for `rt.tag`, fetch + validate the inner manifest, then recurse
///    into [`install_into`] for the inner tool.
///
/// The inner manifest's sha is sourced from the section row for
/// `tool/<inner-name>` rather than carried on the [`RequiresTool`]
/// itself; see the wire-format note in `UNBUNDLE_PLAN.md`. Costs one
/// extra small section fetch per recursive step, but keeps `index.nix`
/// single-pass.
#[allow(clippy::too_many_arguments)]
fn install_required_tool(
    client: &reqwest::blocking::Client,
    paths: &Paths,
    rt: &RequiresTool,
    bar: &DownloadBar,
    fetched: &FetchedRoot,
    host: &str,
    cache_root: &Path,
    target: &str,
    visited: &mut HashSet<(String, String)>,
) -> Result<PathBuf> {
    let install_root = paths.store().join(format!("{}-{}", rt.name, rt.version));
    if install_root.is_dir() {
        // Fully installed, or — under the global lock — at minimum
        // safely past `fetch_blob`'s atomic rename. Either way, the
        // caller can link into it. Skip the recursive walk entirely.
        return Ok(install_root);
    }

    let key = (rt.name.clone(), rt.version.clone());
    if !visited.insert(key) {
        return Err(eyre!(
            "cycle in requires_tools: `{}@{}` re-encountered before its install completed",
            rt.name,
            rt.version
        ));
    }

    let section = fetch_tool_section(client, fetched, host, cache_root, target, &rt.name)?;
    let row = section
        .artifacts
        .iter()
        .find(|a| a.tag == rt.tag && !a.yanked)
        .ok_or_else(|| {
            let published: Vec<&str> = section
                .artifacts
                .iter()
                .filter(|a| !a.yanked)
                .map(|a| a.tag.as_str())
                .collect();
            eyre!(
                "requires_tool `{}@{}` pinned to tag `{}` not found in section `tool/{}` \
                 for target {target}; the outer tool's manifest and the index are out of \
                 sync. Available tags: {}",
                rt.name,
                rt.version,
                rt.tag,
                rt.name,
                if published.is_empty() {
                    "(none)".into()
                } else {
                    published.join(", ")
                },
            )
        })?;

    let inner_manifest = fetch_manifest(
        client,
        host,
        cache_root,
        &row.manifest.path,
        &row.manifest.sha256,
    )?;
    inner_manifest
        .validate()
        .wrap_err_with(|| format!("validating manifest for {}", inner_manifest.tag))?;

    install_into(
        client,
        paths,
        &inner_manifest,
        &install_root,
        bar,
        fetched,
        host,
        cache_root,
        target,
        visited,
    )?;
    Ok(install_root)
}

/// Resolve the `tool/<name>` section under `target` from the cached
/// root, then fetch + sha-verify its body. Shared by the outer
/// catalog-driven path and the recursive `requires_tools` resolver.
fn fetch_tool_section(
    client: &reqwest::blocking::Client,
    fetched: &FetchedRoot,
    host: &str,
    cache_root: &Path,
    target: &str,
    tool_name: &str,
) -> Result<Section> {
    let section_name = format!("tool/{tool_name}");
    let target_entry = fetched.root.targets.get(target).ok_or_else(|| {
        let available: Vec<String> = fetched.root.targets.keys().cloned().collect();
        BougieError::UnknownTarget {
            triple: target.to_owned(),
            hint: format!("the index at {host} advertises: {}", available.join(", ")),
        }
    })?;
    let section_ref =
        target_entry
            .sections
            .get(&section_name)
            .ok_or_else(|| BougieError::Resolution {
                kind: "service".into(),
                detail: format!(
                    "the index at {host} has no `{section_name}` section under target {target}"
                ),
            })?;
    fetch_section(
        client,
        host,
        cache_root,
        &fetched.root.version,
        target,
        &section_name,
        &section_ref.sha256,
    )
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

    // ---------- install_required_tool / install_into (Phase 2) ----------

    /// A minimal [`FetchedRoot`] for tests that exercise the
    /// early-return branch (no HTTP touched).
    fn empty_fetched_root() -> FetchedRoot {
        use crate::index::fetch::FetchOutcome;
        use crate::index::wire::Root;
        let json = r#"{
            "schema": 1,
            "version": "test",
            "generated": "2026-05-16T00:00:00Z",
            "targets": {}
        }"#;
        let root: Root = serde_json::from_str(json).unwrap();
        FetchedRoot { root, outcome: FetchOutcome::Cached }
    }

    fn valid_requires_tool() -> RequiresTool {
        RequiresTool {
            name: "jdk".into(),
            version: "21.0.11+10".into(),
            tag: "jdk-21.0.11_10-x86_64-unknown-linux-gnu-default".into(),
            manifest_url: "https://example.invalid/jdk.json".into(),
            link_into: "jdk".into(),
        }
    }

    #[test]
    fn install_required_tool_short_circuits_when_inner_root_exists() {
        // Diamond happy path: the inner tool is already on disk from a
        // sibling install (or a previous `bougie services up`). The
        // function must early-return without touching the network — we
        // verify this by passing a `FetchedRoot` whose targets map is
        // empty, so any attempted section lookup would fail loudly.
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = Paths::new(tmp.path().into(), tmp.path().join("cache"));
        std::fs::create_dir_all(paths.store().join("jdk-21.0.11+10/bin")).unwrap();

        let client = reqwest::blocking::Client::new();
        let bar = DownloadBar::hidden();
        let rt = valid_requires_tool();
        let fetched = empty_fetched_root();
        let mut visited = HashSet::new();
        let inner_root = install_required_tool(
            &client,
            &paths,
            &rt,
            &bar,
            &fetched,
            "https://example.invalid",
            &tmp.path().join("cache"),
            "x86_64-unknown-linux-gnu",
            &mut visited,
        )
        .expect("early-return path should succeed without HTTP");

        assert_eq!(inner_root, paths.store().join("jdk-21.0.11+10"));
        // Crucially, visited must NOT have grown — the function
        // bailed before the cycle-detection insert.
        assert!(visited.is_empty(), "visited grew unexpectedly: {visited:?}");
    }

    #[test]
    fn install_required_tool_detects_in_progress_cycle() {
        // Direct cycle: we manually seed `visited` to simulate "we're
        // already installing X higher up the call stack." The
        // function must refuse to re-enter rather than recursing
        // forever. is_dir() is false (no pre-existing install root)
        // so the short-circuit doesn't mask the check.
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = Paths::new(tmp.path().into(), tmp.path().join("cache"));

        let client = reqwest::blocking::Client::new();
        let bar = DownloadBar::hidden();
        let rt = valid_requires_tool();
        let fetched = empty_fetched_root();
        let mut visited = HashSet::new();
        visited.insert(("jdk".into(), "21.0.11+10".into()));

        let err = install_required_tool(
            &client,
            &paths,
            &rt,
            &bar,
            &fetched,
            "https://example.invalid",
            &tmp.path().join("cache"),
            "x86_64-unknown-linux-gnu",
            &mut visited,
        )
        .expect_err("expected cycle detection to fire");
        let msg = err.to_string();
        assert!(msg.contains("cycle"), "{msg}");
        assert!(msg.contains("jdk"), "{msg}");
    }
}
