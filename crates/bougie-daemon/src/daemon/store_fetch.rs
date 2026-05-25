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
use bougie_errors::BougieError;
use bougie_fetch::{fetch_blob, ArchiveKind, BlobSpec, DownloadBar, Hash};
use bougie_index::{
    build_verifier,
    fetch::{fetch_manifest, fetch_root, fetch_section, FetchedRoot},
    wire::{Artifact, Manifest, RequiresTool, Section},
};
use bougie_installer::install::{host_to_dirname, install_closure_peers, plan_closure_bytes, DEFAULT_INDEX_URL};
use bougie_fs::lock::ExclusiveGuard;
use bougie_paths::Paths;
use bougie_platform::target::Triple;
use eyre::{eyre, Result, WrapErr};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

/// Kind discriminator for [`ResolvedTool`]. Today only `tool` exists
/// (an entry resolved from a manifest's `requires_tools[]`); the enum
/// shape reserves room for `closure` if a future Phase decides to
/// surface closure peers in the JSON inventory too.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ResolvedKind {
    Tool,
}

/// One entry in `bougie up --format json-v1`'s per-service
/// `dependencies` inventory. Records each `requires_tools[]` entry
/// the resolver walked, whether it was fetched anew or already on
/// disk, and where it landed in the shared store.
///
/// Closure peers (the openssl/zlib/ncurses tarballs) are intentionally
/// not surfaced here today — they're an implementation detail of the
/// store layout, not a thing users typically need to debug. If a
/// future Phase wants closure-level granularity, add a
/// [`ResolvedKind::Closure`] variant.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ResolvedTool {
    pub kind: ResolvedKind,
    pub name: String,
    pub version: String,
    /// `true` if `fetch_blocking` downloaded this in the current call;
    /// `false` if it was already on disk from a prior install. Useful
    /// for CI dashboards verifying dedup worked.
    pub fetched: bool,
    /// Path relative to `$BOUGIE_HOME/store/` (e.g. `"jdk-21.0.11+10"`).
    pub install_path: String,
}

const LOCK_TIMEOUT: Duration = Duration::from_mins(1);

/// Make sure the service tarball named by `entry.tarball` is present
/// under `$BOUGIE_HOME/store`. No-op when already on disk; fetches
/// from the configured index otherwise.
///
/// Runs the blocking fetch + extract on the tokio blocking pool so
/// the daemon's main reactor isn't stalled across what can be a
/// multi-hundred-MB download.
pub async fn ensure_tarball(
    paths: &Paths,
    entry: &'static CatalogEntry,
    bar: Option<Arc<DownloadBar>>,
) -> Result<Vec<ResolvedTool>> {
    // The `server` catalog entry reuses the bougie binary itself —
    // nothing to fetch. Same fast-path as `store_layout::basedir`'s
    // empty-tarball check.
    if entry.tarball.is_empty() {
        return Ok(Vec::new());
    }
    if store_layout::basedir(paths, entry).is_ok() {
        return Ok(Vec::new());
    }
    let paths = paths.clone();
    tokio::task::spawn_blocking(move || fetch_blocking(&paths, entry, bar.as_deref()))
        .await
        .wrap_err("joining tarball-fetch task")?
}

fn fetch_blocking(
    paths: &Paths,
    entry: &CatalogEntry,
    external_bar: Option<&DownloadBar>,
) -> Result<Vec<ResolvedTool>> {
    let target = Triple::detect()?.to_string();
    let host = std::env::var("BOUGIE_INDEX_URL").unwrap_or_else(|_| DEFAULT_INDEX_URL.into());

    let _guard = ExclusiveGuard::acquire(&paths.global_lock(), LOCK_TIMEOUT)?;

    // Re-check under the lock: a concurrent `bougie services up`
    // could have populated the store while we were queued behind
    // it, in which case there's nothing left to do and skipping the
    // network round-trip is the polite choice.
    if store_layout::basedir(paths, entry).is_ok() {
        return Ok(Vec::new());
    }

    let client = bougie_fetch::default_client()?;

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

    // Surface any drift between the compiled-in catalog's runtime_deps
    // and the upstream manifest's requires_tools. Non-fatal — the
    // manifest is authoritative for the actual install — but warning
    // here means we notice when a bougie release lags behind the
    // index, and vice-versa.
    for w in audit_catalog_drift(entry, &manifest) {
        tracing::warn!(service = entry.name, "{w}");
    }

    std::fs::create_dir_all(paths.store())
        .wrap_err_with(|| format!("creating {}", paths.store().display()))?;
    let dest = paths.store().join(entry.tarball);

    // Caller-supplied bar with a sink that forwards events over IPC
    // (see `dispatch_up_streaming` in `ipc.rs`); fall back to a fully
    // hidden, no-sink bar so the codepath stays usable from tests
    // and direct daemon-internal callers that don't want progress
    // streaming.
    let owned;
    let bar: &DownloadBar = match external_bar {
        Some(b) => b,
        None => {
            owned = DownloadBar::hidden();
            &owned
        }
    };

    // Seed the cycle-detection visited set with the outer tool so a
    // pathological `requires_tools[]` entry pointing back at the outer
    // (`opensearch → opensearch`) surfaces as a clear error rather than
    // silently re-linking the partial install to itself.
    let mut visited = HashSet::new();
    visited.insert((manifest.name.clone(), manifest.version.clone()));

    let mut report: Vec<ResolvedTool> = Vec::new();
    install_into(
        &client,
        paths,
        &manifest,
        &dest,
        bar,
        &fetched,
        &host,
        &cache_root,
        &target,
        &mut visited,
        &mut report,
    )?;

    // Only finish bars we own. A caller-supplied bar (the IPC sink in
    // `dispatch_up_streaming`) is shared across multiple `ensure_tarball`
    // calls within one `service.up`, so finishing here would emit a
    // premature Finish event mid-walk.
    if external_bar.is_none() {
        bar.finish();
    }
    Ok(report)
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
    report: &mut Vec<ResolvedTool>,
) -> Result<()> {
    // Adapt wire closures to the backend-neutral shape the install
    // helpers now take. Cheap clone — closure lists are small (≤ a
    // dozen entries for the heaviest tools).
    let closure: Vec<bougie_backend::ClosureRef> =
        manifest.closure.iter().map(bougie_backend::ClosureRef::from).collect();

    bar.add_planned(manifest.blob.size);
    plan_closure_bytes(paths, &closure, bar);
    bar.set_current(manifest.tag.clone());

    let spec = BlobSpec {
        url: &manifest.blob.url,
        hash: Hash::sha256(&manifest.blob.sha256),
        partial_dir: &paths.cache_blobs(),
        dest: install_root,
        // Tool tarballs ship their tree under `install/` — same
        // convention as the interpreter tarball.
        strip_prefix: "install",
        archive: ArchiveKind::TarZst,
        auth_header: None,
    };
    fetch_blob(client, &spec, bar)?;

    install_closure_peers(client, paths, &closure, &manifest.name, &manifest.tag, install_root, bar)?;

    for rt in &manifest.requires_tools {
        let inner_root = install_required_tool(
            client, paths, rt, bar, fetched, host, cache_root, target, visited, report,
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
    report: &mut Vec<ResolvedTool>,
) -> Result<PathBuf> {
    let install_path_rel = format!("{}-{}", rt.name, rt.version);
    let install_root = paths.store().join(&install_path_rel);
    if install_root.is_dir() {
        // Fully installed, or — under the global lock — at minimum
        // safely past `fetch_blob`'s atomic rename. Either way, the
        // caller can link into it. Skip the recursive walk entirely.
        report.push(ResolvedTool {
            kind: ResolvedKind::Tool,
            name: rt.name.clone(),
            version: rt.version.clone(),
            fetched: false,
            install_path: install_path_rel,
        });
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
        report,
    )?;
    report.push(ResolvedTool {
        kind: ResolvedKind::Tool,
        name: rt.name.clone(),
        version: rt.version.clone(),
        fetched: true,
        install_path: install_path_rel,
    });
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

/// Compare the outer tool's compiled-in catalog `runtime_deps` against
/// the manifest's `requires_tools[]`. Returns one human-readable
/// warning per drift point (extra entry on either side, or matching
/// name with diverging version). Never fails — the manifest is the
/// authoritative source of truth at install time; this is purely a
/// drift signal so a stale catalog gets noticed before it confuses a
/// user.
///
/// Phase 3 of `UNBUNDLE_PLAN.md`. The catalog keeps its own
/// `runtime_deps` field (the supervisor uses it for ordering /
/// availability checks); the manifest gets `requires_tools[]` (the
/// installer uses it to lay down peer trees). They answer different
/// questions and should both exist, but they're describing the same
/// underlying tool-to-tool relationship and shouldn't diverge.
pub(crate) fn audit_catalog_drift(entry: &CatalogEntry, manifest: &Manifest) -> Vec<String> {
    let catalog: HashSet<&str> = entry.runtime_deps.iter().copied().collect();
    let from_manifest: HashSet<&str> = manifest
        .requires_tools
        .iter()
        .map(|rt| rt.name.as_str())
        .collect();
    let mut warnings = Vec::new();

    for missing in catalog.difference(&from_manifest) {
        warnings.push(format!(
            "catalog `runtime_deps` for `{outer}` lists `{missing}` but the upstream \
             manifest's `requires_tools[]` doesn't — bougie's catalog is likely ahead of \
             the index, or the index dropped the dep without a coordinated client release.",
            outer = entry.name,
        ));
    }
    for extra in from_manifest.difference(&catalog) {
        warnings.push(format!(
            "upstream manifest for `{outer}` declares `requires_tools[]` entry `{extra}` but \
             bougie's catalog doesn't list it under `runtime_deps` — install will still \
             succeed (the manifest is authoritative), but the supervisor's ordering / \
             availability checks won't see it.",
            outer = entry.name,
        ));
    }
    for rt in &manifest.requires_tools {
        if let Some(inner_entry) = crate::daemon::catalog::find(&rt.name)
            && inner_entry.version != rt.version
        {
            warnings.push(format!(
                "catalog pins inner tool `{name}` to `{cv}` but outer manifest's \
                 `requires_tools[]` pins it to `{mv}` — the actual install will use the \
                 manifest's pin; refresh the catalog if `{mv}` is the new target.",
                name = rt.name,
                cv = inner_entry.version,
                mv = rt.version,
            ));
        }
    }
    warnings
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
    use bougie_index::wire::ManifestRef;

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
        use bougie_index::fetch::FetchOutcome;
        use bougie_index::wire::Root;
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
        let mut report = Vec::new();
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
            &mut report,
        )
        .expect("early-return path should succeed without HTTP");

        assert_eq!(inner_root, paths.store().join("jdk-21.0.11+10"));
        // Crucially, visited must NOT have grown — the function
        // bailed before the cycle-detection insert.
        assert!(visited.is_empty(), "visited grew unexpectedly: {visited:?}");
        // The early-return path still records the dep with fetched=false
        // so the JSON inventory surfaces dedup hits.
        assert_eq!(report.len(), 1);
        assert_eq!(report[0].name, "jdk");
        assert_eq!(report[0].version, "21.0.11+10");
        assert!(!report[0].fetched);
        assert_eq!(report[0].install_path, "jdk-21.0.11+10");
    }

    // ---------- audit_catalog_drift (Phase 3) ----------

    fn tool_manifest_with_requires(outer_name: &str, inners: &[(&str, &str)]) -> Manifest {
        let rts: Vec<_> = inners
            .iter()
            .map(|(n, v)| {
                serde_json::json!({
                    "name": n,
                    "version": v,
                    "tag": format!("{n}-{v}-x86_64-unknown-linux-gnu-default"),
                    "manifest_url": format!("https://example.invalid/{n}.json"),
                    "link_into": n.to_string(),
                })
            })
            .collect();
        serde_json::from_value(serde_json::json!({
            "schema": 1, "kind": "tool", "name": outer_name,
            "tag": format!("{outer_name}-1.0.0-x86_64-unknown-linux-gnu-default"),
            "version": "1.0.0",
            "target": "x86_64-unknown-linux-gnu",
            "flavor": "default",
            "libc": {"family":"gnu","min":"2.17"},
            "blob": {"url":"https://example.invalid/o","sha256":"aa","size":0},
            "closure": [],
            "requires_tools": rts,
        }))
        .unwrap()
    }

    fn synthetic_entry(
        name: &'static str,
        runtime_deps: &'static [&'static str],
    ) -> CatalogEntry {
        use crate::daemon::catalog::{Binding, SandboxKind, Tenancy};
        CatalogEntry {
            name,
            version: "1.0.0",
            tarball: name,
            binary: "bin/x",
            binding: Binding::None,
            tenancy: Tenancy::None,
            requires: &[],
            after: &[],
            runtime_deps,
            user_facing: true,
            summary: "test",
            sandbox: SandboxKind::Strict,
        }
    }

    #[test]
    fn resolved_tool_serializes_kind_as_lowercase_tool() {
        // The plan's JSON example specifies `"kind": "tool"`. Locking
        // in the wire shape so a future rename of the enum variant
        // doesn't silently break the CLI's `--format json-v1`
        // consumers.
        let r = ResolvedTool {
            kind: ResolvedKind::Tool,
            name: "jdk".into(),
            version: "21.0.11+10".into(),
            fetched: true,
            install_path: "jdk-21.0.11+10".into(),
        };
        let v = serde_json::to_value(&r).unwrap();
        assert_eq!(v["kind"], "tool");
        assert_eq!(v["fetched"], true);
        assert_eq!(v["install_path"], "jdk-21.0.11+10");
        // And the same JSON round-trips into a ResolvedTool again.
        let parsed: ResolvedTool = serde_json::from_value(v).unwrap();
        assert_eq!(parsed, r);
    }

    #[test]
    fn audit_drift_quiet_when_sets_match() {
        let entry = synthetic_entry("rabbitmq", &["erlang"]);
        // The catalog's `erlang` entry pins 27.3.4.11 — match it in
        // the manifest to avoid a spurious version-drift warning.
        let manifest = tool_manifest_with_requires("rabbitmq", &[("erlang", "27.3.4.11")]);
        let warnings = audit_catalog_drift(&entry, &manifest);
        assert!(warnings.is_empty(), "{warnings:?}");
    }

    #[test]
    fn audit_drift_warns_when_catalog_has_extra_runtime_dep() {
        // Catalog claims an inner tool the manifest doesn't ask for.
        // Typical cause: bougie release shipped ahead of an index
        // republish that dropped the dep.
        let entry = synthetic_entry("opensearch", &["jdk"]);
        let manifest = tool_manifest_with_requires("opensearch", &[]);
        let warnings = audit_catalog_drift(&entry, &manifest);
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("catalog"), "{}", warnings[0]);
        assert!(warnings[0].contains("jdk"), "{}", warnings[0]);
    }

    #[test]
    fn audit_drift_warns_when_manifest_has_extra_requires_tool() {
        // Manifest declares an inner the catalog doesn't list. Install
        // still succeeds (manifest is authoritative); the supervisor
        // just won't include it in startup ordering.
        let entry = synthetic_entry("opensearch", &[]);
        let manifest = tool_manifest_with_requires("opensearch", &[("jdk", "21.0.11+10")]);
        let warnings = audit_catalog_drift(&entry, &manifest);
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("manifest"), "{}", warnings[0]);
        assert!(warnings[0].contains("jdk"), "{}", warnings[0]);
    }

    #[test]
    fn audit_drift_warns_on_version_pin_divergence() {
        // Both sides agree on the inner's name (no set-diff warning),
        // but pin it to different versions. The catalog's `jdk` entry
        // pins 21.0.11+10; declare 21.0.12 in the manifest.
        let entry = synthetic_entry("opensearch", &["jdk"]);
        let manifest = tool_manifest_with_requires("opensearch", &[("jdk", "21.0.12")]);
        let warnings = audit_catalog_drift(&entry, &manifest);
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("21.0.11+10"), "{}", warnings[0]);
        assert!(warnings[0].contains("21.0.12"), "{}", warnings[0]);
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
        let mut report = Vec::new();

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
            &mut report,
        )
        .expect_err("expected cycle detection to fire");
        let msg = err.to_string();
        assert!(msg.contains("cycle"), "{msg}");
        assert!(msg.contains("jdk"), "{msg}");
    }
}
