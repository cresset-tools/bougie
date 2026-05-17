//! Orchestrates a PHP interpreter installation: refresh index, resolve,
//! fetch + extract. Shared by `bougie php install` and `bougie sync`.

use crate::backend::{Backend, BougieIndexBackend};
use crate::baseline::{
    skip_for_platform, BaselineFilter, BASELINE_EXTENSIONS, PREINSTALLED_EXTENSIONS,
};
use crate::errors::BougieError;
use crate::fetch::{fetch_blob, ArchiveKind, BlobSpec, DownloadBar};
use crate::index::{
    build_verifier,
    fetch::{fetch_manifest, fetch_root, fetch_section},
    wire::{LoadDirective, Manifest},
};
use crate::lock::ExclusiveGuard;
use crate::paths::Paths;
use crate::request::{Flavor, Request};
use crate::resolve::{resolve_extension, ResolveOptions, Selected};
use crate::store::install_dir;
use crate::target::Triple;
use crate::version::{PartialVersion, Version};
use eyre::{eyre, Result, WrapErr};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

pub const DEFAULT_INDEX_URL: &str = "https://index.bougie.tools";
const LOCK_TIMEOUT: Duration = Duration::from_mins(1);

#[derive(Debug, Clone)]
pub struct InstalledPhp {
    pub version: Version,
    pub flavor: Flavor,
    pub install_path: PathBuf,
    pub already_present: bool,
    pub frozen_warning: bool,
}

pub fn install_php(
    paths: &Paths,
    request: &Request,
    flavor_override: Option<Flavor>,
    opts: ResolveOptions,
) -> Result<InstalledPhp> {
    let target = Triple::detect()?.to_string();
    let host = std::env::var("BOUGIE_INDEX_URL").unwrap_or_else(|_| DEFAULT_INDEX_URL.into());

    let (spec, in_request_flavor) = match request {
        Request::VersionLike { spec, flavor } => (spec.clone(), *flavor),
        Request::FullTag { .. } | Request::Path(_) | Request::Name(_) => {
            return Err(eyre!(
                "this request shape is not supported by `php install`; use a version or constraint"
            ));
        }
    };
    let flavor = pick_flavor(in_request_flavor, flavor_override)?;

    // Lock the global store before mutating anything.
    let _guard = ExclusiveGuard::acquire(&paths.global_lock(), LOCK_TIMEOUT)?;

    let backend = BougieIndexBackend::new(paths, &host, &target)?;
    let recipe = backend.resolve_php(&spec, flavor, opts)?;
    let dest = install_dir(paths, recipe.version, recipe.flavor);
    let already_present = dest.exists();
    if !already_present {
        // Interpreter is a monolithic blob (no closure walk), so the
        // bar grows by exactly the tarball's size. A pre-`size`
        // publisher emits `size: 0` which `add_planned` silently
        // drops; the bar still ticks bytes received but can't fill.
        let bar = DownloadBar::new("downloading");
        bar.add_planned(recipe.blob.size);
        bar.set_current(format!("php-{}", recipe.version));
        let cache_blobs = paths.cache_blobs();
        let blob_spec = recipe.blob.as_blob_spec(&cache_blobs, &dest);
        fetch_blob(backend.client(), &blob_spec, &bar)?;
        bar.finish();
    }

    Ok(InstalledPhp {
        version: recipe.version,
        flavor: recipe.flavor,
        install_path: dest,
        already_present,
        frozen_warning: recipe.frozen_warning,
    })
}

fn pick_flavor(in_request: Option<Flavor>, flag: Option<Flavor>) -> Result<Flavor> {
    match (in_request, flag) {
        (Some(a), Some(b)) if a != b => Err(BougieError::Resolution {
            kind: "flavor".into(),
            detail: format!(
                "request encodes flavor `{a}` but --flavor=`{b}` was passed; remove one or make them agree"
            ),
        }
        .into()),
        (Some(f), _) | (None, Some(f)) => Ok(f),
        (None, None) => Ok(Flavor::Nts),
    }
}

/// One installed extension, as produced by [`install_extension`].
#[derive(Debug, Clone)]
pub struct InstalledExt {
    pub name: String,
    pub version: Version,
    pub flavor: Flavor,
    pub php_minor: PartialVersion,
    /// Store directory the tarball was extracted into.
    pub store_path: PathBuf,
    /// Absolute path to the `.so` inside [`Self::store_path`], suitable
    /// for the right-hand side of the conf.d `extension=` /
    /// `zend_extension=` directive.
    pub so_path: PathBuf,
    /// Whether the conf.d fragment emits `extension=` or `zend_extension=`.
    pub load: LoadDirective,
    pub already_present: bool,
    pub frozen_warning: bool,
}

/// Install a PHP extension into the content-addressed store.
///
/// Mirrors [`install_php`] but targets the extension's own section
/// (`extension/<name>`) and uses [`resolve_extension`] for selection.
/// The destination is `$BOUGIE_HOME/store/ext-<name>-<version>+php<minor>-<flavor>-<sha8>/`,
/// content-addressed by the first 8 hex chars of the blob sha256 so
/// different ABI builds of the same `<name>-<version>` don't collide.
///
/// Single-extension entry point: creates its own [`DownloadBar`] for
/// this call. Use [`install_extension_with_bar`] when multiple
/// extensions are being installed in a loop and you want one bar
/// across all of them.
pub fn install_extension(
    paths: &Paths,
    name: &str,
    version_pin: Option<&str>,
    php_minor: PartialVersion,
    flavor: Flavor,
    opts: ResolveOptions,
) -> Result<InstalledExt> {
    let bar = DownloadBar::new("downloading");
    let out = install_extension_with_bar(paths, name, version_pin, php_minor, flavor, opts, &bar);
    bar.finish();
    out
}

/// Same as [`install_extension`] but draws progress against a caller-
/// owned bar so the orchestrator (e.g. [`install_baseline_into`],
/// [`preinstall_into`], `sync::install_required_extensions`) can show
/// a single combined bar across many extensions instead of one bar
/// per artifact.
pub fn install_extension_with_bar(
    paths: &Paths,
    name: &str,
    version_pin: Option<&str>,
    php_minor: PartialVersion,
    flavor: Flavor,
    opts: ResolveOptions,
    bar: &DownloadBar,
) -> Result<InstalledExt> {
    let target = Triple::detect()?.to_string();
    let host = std::env::var("BOUGIE_INDEX_URL").unwrap_or_else(|_| DEFAULT_INDEX_URL.into());
    let section_name = format!("extension/{name}");

    let _guard = ExclusiveGuard::acquire(&paths.global_lock(), LOCK_TIMEOUT)?;

    let client = reqwest::blocking::Client::builder()
        .build()
        .map_err(|e| BougieError::Network {
            operation: "building HTTP client".into(),
            detail: e.to_string(),
        })?;

    let cache_root = paths.cache_index(&host_to_dirname(&host));
    let fetched = fetch_root(&client, &host, &cache_root, build_verifier)?;
    let target_entry = fetched.root.targets.get(&target).ok_or_else(|| {
        let available: Vec<String> = fetched.root.targets.keys().cloned().collect();
        BougieError::UnknownTarget {
            triple: target.clone(),
            hint: format!("the index at {host} advertises: {}", available.join(", ")),
        }
    })?;
    let section_ref = target_entry
        .sections
        .get(&section_name)
        .ok_or_else(|| BougieError::Resolution {
            kind: "extension".into(),
            detail: format!(
                "the index at {host} has no `{section_name}` section under target {target} — \
                 run `bougie ext list --only-available` to see what's published"
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

    let selected: Selected<'_> = resolve_extension(&section, php_minor, flavor, version_pin, opts)?;

    let manifest = fetch_manifest(
        &client,
        &host,
        &cache_root,
        &selected.artifact.manifest.path,
        &selected.artifact.manifest.sha256,
    )?;
    let ext_ref = manifest.extension.as_ref().ok_or_else(|| {
        eyre!(
            "manifest for {} is missing the `extension` field — \
             publisher bug: an extension-kind manifest must declare its `.so` path",
            manifest.tag
        )
    })?;

    let sha8: String = manifest.blob.sha256.chars().take(8).collect();
    let php_minor_label = format!("php{}{}", php_minor.major, php_minor.minor.unwrap_or(0));
    let dirname = format!(
        "ext-{}-{}+{php_minor_label}-{flavor}-{sha8}",
        manifest.name, manifest.version,
    );
    let dest = paths.store().join(&dirname);
    let already_present = dest.exists();

    // Grow the shared bar's planned total by this extension's bytes:
    // the main `.so` blob (if not already on disk) plus every closure
    // entry whose store_path is missing. Sizes come from the manifest
    // so we don't pay a HEAD round-trip per file. A `size: 0` from
    // an older publisher contributes nothing — the bar still ticks
    // bytes received but can't fill for that artifact.
    if !already_present {
        bar.add_planned(manifest.blob.size);
    }
    plan_closure_bytes(paths, &manifest, bar);

    if !already_present {
        let blob_spec = BlobSpec {
            url: &manifest.blob.url,
            sha256: &manifest.blob.sha256,
            partial_dir: &paths.cache_blobs(),
            dest: &dest,
            // Per-extension tarballs ship `lib/extensions/<api>/<name>.so`
            // at the top level — no wrapping directory to strip.
            strip_prefix: "",
            archive: ArchiveKind::TarZst,
        };
        bar.set_current(format!("{}-{}", manifest.name, manifest.version));
        fetch_blob(&client, &blob_spec, bar)?;
    }

    // Walk the manifest's bundled-C-lib closure and fetch any
    // store-paths the consumer doesn't have yet. Mandatory for ext:
    // the `.so` was built with `$ORIGIN/../../../store/<storeName>/lib`
    // RPATHs that assume an install-shaped layout (matching the
    // interpreter tarball, whose `<install>/store/<storeName>/lib`
    // directly resolves). Without these tarballs *and* the
    // corresponding `store/` peer inside the ext root, dlopen falls
    // back to the system loader and surfaces errors like
    // `libicuuc.so.77: cannot open shared object file` for intl or
    // `libcurl: undefined symbol: ENGINE_init` for curl (system
    // libcurl against a different OpenSSL).
    //
    // Run unconditionally — `dest.exists()` only tells us the .so
    // blob is present; the closure may still be partial from an
    // earlier bougie release that didn't walk it.
    install_closure_peers(&client, paths, &manifest, &dest, bar)?;

    let so_path = dest.join(&ext_ref.path);
    if !so_path.exists() {
        return Err(eyre!(
            "extracted ext bundle is missing the declared `.so` at {} \
             — blob {} may be corrupt or the manifest is wrong",
            so_path.display(),
            manifest.tag
        ));
    }

    Ok(InstalledExt {
        name: manifest.name.clone(),
        version: selected.version,
        flavor,
        php_minor,
        store_path: dest,
        so_path,
        load: ext_ref.load,
        already_present,
        frozen_warning: selected.frozen_warning,
    })
}

/// Outcome of [`install_baseline_into`]. `installed` and `failed`
/// together cover every name the filter admitted; `skipped` is empty
/// here and carried only so the JSON shape stays stable when the
/// caller passes [`BaselineFilter::None`] or a narrowed `Only` (the
/// difference between "didn't try" vs "tried and failed" matters for
/// CI dashboards).
#[derive(Debug, Default, Clone)]
pub struct BaselineReport {
    pub installed: Vec<String>,
    pub failed: Vec<(String, String)>,
}

/// Install every baseline extension admitted by `filter` into the
/// content-addressed store, then write a conf.d fragment under
/// `<install_root>/etc/php/conf.d/<NN>-<name>.ini` so the interpreter
/// auto-loads them. The numeric prefix mirrors `php-build-standalone`'s
/// `php/build-php.sh` convention so that `pdo_*` loads after `pdo` and
/// `mysqli` / `sqlite3` / `pgsql` load after their PDO siblings — see
/// [`conf_d_prefix_for`]. Errors per extension are downgraded to
/// entries in [`BaselineReport::failed`].
///
/// Before iterating, any pre-existing bougie-baseline-managed fragments
/// in `conf_d` are deleted. This keeps a re-run idempotent across
/// changes to [`conf_d_prefix_for`] or to [`BASELINE_EXTENSIONS`]: an
/// old `20-pdo_mysql.ini` from a previous bougie release won't linger
/// alongside the new `35-pdo_mysql.ini`, which would otherwise
/// trigger `undefined symbol: pdo_dbh_ce` (pdo_mysql loaded before
/// pdo) or `Module "pdo_mysql" is already loaded` (both fragments
/// loaded in sequence).
///
/// This is intentionally separate from [`install_php`] and must be
/// called *after* `install_php` returns: both functions acquire the
/// same global lock, and nesting them would deadlock.
pub fn install_baseline_into(
    paths: &Paths,
    install_root: &Path,
    php_minor: PartialVersion,
    flavor: Flavor,
    filter: &BaselineFilter,
    resolve_opts: ResolveOptions,
) -> BaselineReport {
    let mut report = BaselineReport::default();
    let conf_d = install_root.join("etc").join("php").join("conf.d");
    if let Err(e) = std::fs::create_dir_all(&conf_d) {
        // If we can't even create conf.d, every entry will fail the
        // same way — record one synthetic failure and bail.
        report.failed.push((
            "<conf.d>".into(),
            format!("creating {}: {e}", conf_d.display()),
        ));
        return report;
    }

    if let Err(e) = clean_stale_baseline_fragments(&conf_d) {
        // Non-fatal: leave the loop a chance to overwrite each
        // canonical-prefix file. Surface as a synthetic failure so
        // CI dashboards see something happened.
        report
            .failed
            .push(("<conf.d-cleanup>".into(), format!("{e:#}")));
    }

    // One shared download bar across the whole baseline loop. The
    // bar's planned-total grows as each extension's manifest reveals
    // its blob+closure sizes, so the user sees a single combined bar
    // instead of one per extension.
    let bar = DownloadBar::new("downloading");
    for &name in BASELINE_EXTENSIONS {
        if !filter.includes(name) {
            continue;
        }
        if skip_for_platform(name) {
            // gettext on macOS is the canonical case — Apple's libc has
            // no real libintl, so php-build-standalone emits no
            // gettext.so on Darwin and the index has no entry to fetch.
            // Silently skip; the conf.d cleanup loop above won't try
            // to delete a fragment that was never written.
            continue;
        }
        match install_extension_with_bar(
            paths,
            name,
            None,
            php_minor,
            flavor,
            resolve_opts,
            &bar,
        ) {
            Ok(installed) => {
                if let Err(e) = write_install_conf_d(&conf_d, &installed) {
                    report
                        .failed
                        .push((name.into(), format!("writing conf.d: {e:#}")));
                } else {
                    report.installed.push(name.into());
                }
            }
            Err(e) => report.failed.push((name.into(), format!("{e:#}"))),
        }
    }
    bar.finish();
    report
}

/// Outcome of [`preinstall_into`]. Same shape as
/// [`BaselineReport`] so the two can be surfaced side-by-side in the
/// `bougie php install` JSON output.
#[derive(Debug, Default, Clone)]
pub struct PreinstallReport {
    pub installed: Vec<String>,
    pub failed: Vec<(String, String)>,
}

/// Pre-download (but don't activate) every extension in
/// [`PREINSTALLED_EXTENSIONS`] for the given interpreter. Each `.so`
/// lands in the content-addressed store under `paths.store()` — same
/// location `install_extension` would use anyway — so a later
/// `bougie ext add xdebug` (or the server's lazy activation path)
/// finds it already on disk and only writes a conf.d fragment.
///
/// `_install_root` is currently unused (no conf.d fragment is written
/// here) but kept in the signature for symmetry with
/// [`install_baseline_into`] and to leave room for future
/// per-interpreter bookkeeping. Errors are per-name and non-fatal —
/// caller surfaces them as warnings, same pattern as
/// [`BaselineReport`].
pub fn preinstall_into(
    paths: &Paths,
    _install_root: &Path,
    php_minor: PartialVersion,
    flavor: Flavor,
    resolve_opts: ResolveOptions,
) -> PreinstallReport {
    let mut report = PreinstallReport::default();
    // Same single-bar pattern as install_baseline_into.
    let bar = DownloadBar::new("downloading");
    for &name in PREINSTALLED_EXTENSIONS {
        match install_extension_with_bar(
            paths,
            name,
            None,
            php_minor,
            flavor,
            resolve_opts,
            &bar,
        ) {
            Ok(_) => report.installed.push(name.into()),
            Err(e) => report.failed.push((name.into(), format!("{e:#}"))),
        }
    }
    bar.finish();
    report
}

/// Numeric conf.d prefix for a baseline (or user) extension. Mirrors
/// `php-build-standalone`'s `php/build-php.sh` numbering so that PHP's
/// alphabetic conf.d scan honors load-order dependencies:
///
/// - `35-pdo_*.ini` loads after `30-pdo.ini` (the core PDO base must
///   be initialized before any driver can register against it).
/// - `40-mysqli.ini` / `40-sqlite3.ini` / `40-pgsql.ini` load after
///   the `35-pdo_*` siblings, matching the conventional grouping
///   even though `mysqli` doesn't have a hard pdo dependency.
/// - Everything else loads at `20-` — right after `10-opcache.ini`
///   but before any user `50-*.ini` overrides.
pub fn conf_d_prefix_for(name: &str) -> u32 {
    if name.starts_with("pdo_") {
        35
    } else if matches!(name, "mysqli" | "sqlite3" | "pgsql") {
        40
    } else {
        20
    }
}

const BASELINE_FRAGMENT_HEADER: &str = "; managed by bougie — baseline extension";

/// Delete any `.ini` fragment under `conf_d` that starts with the
/// bougie-baseline marker. Run before re-writing baseline fragments
/// so a prefix change between bougie releases doesn't leave orphans
/// (see [`install_baseline_into`] docstring for why this matters).
///
/// Only files whose first line begins with [`BASELINE_FRAGMENT_HEADER`]
/// are removed — user-authored `15-mytunables.ini` and shipped
/// interpreter fragments stay put.
fn clean_stale_baseline_fragments(conf_d: &Path) -> Result<()> {
    let entries = match std::fs::read_dir(conf_d) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(eyre!("reading {}: {e}", conf_d.display())),
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("ini") {
            continue;
        }
        let Ok(body) = std::fs::read_to_string(&path) else {
            continue;
        };
        if body.starts_with(BASELINE_FRAGMENT_HEADER) {
            let _ = std::fs::remove_file(&path);
        }
    }
    Ok(())
}

/// Write `<NN>-<name>.ini` under `<install>/etc/php/conf.d/` referencing
/// the content-addressed store path of the just-installed `.so`. The
/// `<NN>` prefix is chosen by [`conf_d_prefix_for`] so the
/// load-order dependencies PHP build conventions encode in the prefix
/// (`30-pdo` → `35-pdo_*` → `40-mysqli/sqlite3/pgsql`) are preserved.
fn write_install_conf_d(conf_d: &Path, installed: &InstalledExt) -> Result<()> {
    let prefix = conf_d_prefix_for(&installed.name);
    let path = conf_d.join(format!("{prefix}-{}.ini", installed.name));
    let body = format!(
        "{header} {name} {version}\n\
         {directive}={so}\n",
        header = BASELINE_FRAGMENT_HEADER,
        name = installed.name,
        version = installed.version,
        directive = installed.load.ini_directive(),
        so = installed.so_path.display(),
    );
    // Use a tempfile+rename for the same atomicity reasons as
    // conf_d::write_ext_fragment; baseline install happens under the
    // global lock so concurrent writers are already serialized, but
    // a partial-write left behind by a kill -9 would still wedge the
    // next `php` invocation.
    let mut tf = tempfile::NamedTempFile::new_in(conf_d)
        .wrap_err_with(|| format!("creating tempfile in {}", conf_d.display()))?;
    tf.as_file_mut()
        .write_all(body.as_bytes())
        .wrap_err_with(|| format!("writing {}", tf.path().display()))?;
    tf.as_file_mut()
        .sync_all()
        .wrap_err_with(|| format!("fsyncing {}", tf.path().display()))?;
    tf.persist(&path)
        .map_err(|e| eyre!("renaming temp to {}: {e}", path.display()))?;
    Ok(())
}

/// Walk `manifest.closure[]`, fetching every missing shared store
/// entry and materializing the install-shaped `store/<closureName>`
/// peer symlinks inside `install_root`. Idempotent: closure entries
/// already on disk are skipped; existing peer symlinks are left alone.
///
/// Used from two sites with the same semantics:
///
/// - [`install_extension_with_bar`] (extension installs): the .so was
///   built with `$ORIGIN/../../../store/<storeName>/lib` RPATHs that
///   need both the global shared-store entry AND the per-ext
///   `store/<storeName>` peer.
/// - [`crate::daemon::store_fetch::fetch_blocking`] (tool installs):
///   tool tarballs (mariadb, redis, …) have the same RPATH shape, so
///   the same machinery applies. Phase 1 of `UNBUNDLE_PLAN.md` flips
///   these tarballs from "ships its closure under `install/store/`"
///   to "publishes a non-empty `closure[]` and relies on this peer
///   layout"; this helper is the client-side hinge.
///
/// Caller is responsible for pre-planning bytes on the bar via
/// [`plan_closure_bytes`] if a visible progress total is wanted. The
/// daemon path uses [`DownloadBar::hidden`] so it skips planning.
pub(crate) fn install_closure_peers(
    client: &reqwest::blocking::Client,
    paths: &Paths,
    manifest: &Manifest,
    install_root: &Path,
    bar: &DownloadBar,
) -> Result<()> {
    for closure in &manifest.closure {
        let store_path =
            store_dir_for_closure(paths, &closure.name, &closure.version, &closure.hash);
        if !store_path.exists() {
            // Closure tarballs wrap their contents in `<storeName>/`
            // per shared/tarball-store-path.nix; strip it so the
            // tarball's `<storeName>/lib/lib*.so` lands at
            // `<store_path>/lib/lib*.so` (matching the interpreter's
            // `<install>/store/<storeName>/lib/lib*.so` layout the
            // RPATHs were compiled to expect).
            let storename = format!("{}-{}-{}", closure.name, closure.version, closure.hash);
            let blob_spec = BlobSpec {
                url: &closure.url,
                sha256: &closure.sha256,
                partial_dir: &paths.cache_blobs(),
                dest: &store_path,
                strip_prefix: &storename,
                archive: ArchiveKind::TarZst,
            };
            bar.set_current(format!("{} ({})", manifest.name, closure.name));
            fetch_blob(client, &blob_spec, bar).wrap_err_with(|| {
                format!(
                    "fetching closure entry `{}-{}-{}` for {}",
                    closure.name, closure.version, closure.hash, manifest.tag
                )
            })?;
        }
        materialize_closure_peer(install_root, &closure.name, &closure.version, &closure.hash)
            .wrap_err_with(|| {
                format!(
                    "linking closure peer `{}-{}-{}` for {}",
                    closure.name, closure.version, closure.hash, manifest.tag
                )
            })?;
    }
    Ok(())
}

/// Grow `bar`'s planned-total by the byte size of every closure entry
/// whose `store_path` is missing. Cheap stat-only loop; intended to be
/// called immediately before the main blob fetch so the caller can
/// show an accurate aggregate from the first byte.
pub(crate) fn plan_closure_bytes(paths: &Paths, manifest: &Manifest, bar: &DownloadBar) {
    for closure in &manifest.closure {
        let store_path =
            store_dir_for_closure(paths, &closure.name, &closure.version, &closure.hash);
        if !store_path.exists() {
            bar.add_planned(closure.size);
        }
    }
}

/// `$BOUGIE_HOME/store/<name>-<version>-<hash>/` for a closure entry.
/// Thin wrapper over [`crate::store::store_dir`] kept here so callers
/// don't need to import the store module just to compute closure
/// destinations.
fn store_dir_for_closure(paths: &Paths, name: &str, version: &str, hash: &str) -> PathBuf {
    crate::store::store_dir(paths, name, version, hash)
}

/// Create the install-shaped `store/<closureName>` peer inside an
/// extension's content-addressed root, as a relative symlink back at
/// the actual shared-store entry.
///
/// The interpreter tarball ships its closure under
/// `<install>/store/<storeName>/` so an extension `.so`'s
/// `$ORIGIN/../../../store/<storeName>/lib` RPATH resolves directly.
/// Standalone-installed extensions don't get that layout for free
/// (their root is just `$BOUGIE_HOME/store/ext-<name>-…/`, with no
/// `store/` peer), so we synthesize it: from inside the ext root,
/// `store/<closureName>` symlinks two levels up to the shared-store
/// sibling, which materializes the path the `.so` was compiled to
/// expect.
///
/// The symlink target is a *relative* `../../<closureName>`, so the
/// whole `$BOUGIE_HOME` tree relocates if the user moves it. Already-
/// correct symlinks are left alone; conflicting plain files would
/// indicate corruption and surface as an error.
fn materialize_closure_peer(ext_root: &Path, name: &str, version: &str, hash: &str) -> Result<()> {
    let dirname = format!("{name}-{version}-{hash}");
    let store_peer = ext_root.join("store");
    std::fs::create_dir_all(&store_peer)
        .wrap_err_with(|| format!("creating {}", store_peer.display()))?;
    let link = store_peer.join(&dirname);
    let target = PathBuf::from("../..").join(&dirname);

    match std::fs::symlink_metadata(&link) {
        Ok(meta) if meta.file_type().is_symlink() => {
            // Already a symlink — leave it. If a stale link points
            // elsewhere, deletion-then-recreate would be racy without
            // the lock we already hold, but the existing link is
            // accurate by construction (same closure entry hash).
            return Ok(());
        }
        Ok(_) => {
            return Err(eyre!(
                "{} exists but isn't a symlink — refusing to overwrite",
                link.display()
            ));
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(eyre!("stat {}: {e}", link.display())),
    }
    symlink_dir(&target, &link)
        .wrap_err_with(|| format!("symlinking {} → {}", link.display(), target.display()))?;
    Ok(())
}

/// Cross-platform directory symlink.
///
/// Unix: `std::os::unix::fs::symlink` (no perm requirement).
/// Windows: `std::os::windows::fs::symlink_dir`. On Windows this
/// requires either Developer Mode to be enabled (Windows 10 1703+) or
/// the process to run with `SeCreateSymbolicLinkPrivilege` (typically
/// an elevated/admin shell). The error message is propagated so the
/// hint surfaces to the user.
#[cfg(unix)]
fn symlink_dir(target: &Path, link: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(target, link)
}
#[cfg(not(unix))]
fn symlink_dir(target: &Path, link: &Path) -> std::io::Result<()> {
    std::os::windows::fs::symlink_dir(target, link)
}

pub fn host_to_dirname(host: &str) -> String {
    // Strip scheme + sanitize: "https://idx.example.com" → "idx.example.com".
    let h = host.trim_end_matches('/');
    let stripped = h
        .strip_prefix("https://")
        .or_else(|| h.strip_prefix("http://"))
        .unwrap_or(h);
    stripped.replace(['/', ':'], "_")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pick_flavor_resolves_consistent_pair() {
        assert_eq!(
            pick_flavor(Some(Flavor::Zts), Some(Flavor::Zts)).unwrap(),
            Flavor::Zts
        );
        assert_eq!(
            pick_flavor(Some(Flavor::Zts), None).unwrap(),
            Flavor::Zts
        );
        assert_eq!(
            pick_flavor(None, Some(Flavor::ZtsDebug)).unwrap(),
            Flavor::ZtsDebug
        );
        assert_eq!(pick_flavor(None, None).unwrap(), Flavor::Nts);
    }

    #[test]
    fn pick_flavor_rejects_conflict() {
        assert!(pick_flavor(Some(Flavor::Nts), Some(Flavor::Zts)).is_err());
    }

    #[test]
    fn host_to_dirname_strips_scheme() {
        assert_eq!(host_to_dirname("https://idx.example.com"), "idx.example.com");
        assert_eq!(host_to_dirname("http://idx:8080"), "idx_8080");
        assert_eq!(host_to_dirname("idx.example.com/"), "idx.example.com");
    }

    #[test]
    fn conf_d_prefix_handles_pdo_drivers_and_db_drivers() {
        assert_eq!(conf_d_prefix_for("pdo_mysql"), 35);
        assert_eq!(conf_d_prefix_for("pdo_sqlite"), 35);
        assert_eq!(conf_d_prefix_for("pdo_pgsql"), 35);
        assert_eq!(conf_d_prefix_for("mysqli"), 40);
        assert_eq!(conf_d_prefix_for("sqlite3"), 40);
        assert_eq!(conf_d_prefix_for("pgsql"), 40);
        assert_eq!(conf_d_prefix_for("mbstring"), 20);
        assert_eq!(conf_d_prefix_for("curl"), 20);
        assert_eq!(conf_d_prefix_for("redis"), 20);
    }

    #[test]
    fn clean_stale_baseline_fragments_removes_only_marked_files() {
        let td = tempfile::TempDir::new().unwrap();
        let conf_d = td.path();
        // Baseline-managed: should go.
        std::fs::write(
            conf_d.join("20-pdo_mysql.ini"),
            format!("{BASELINE_FRAGMENT_HEADER} pdo_mysql 1.0\nextension=x\n"),
        )
        .unwrap();
        std::fs::write(
            conf_d.join("20-mbstring.ini"),
            format!("{BASELINE_FRAGMENT_HEADER} mbstring 1.0\nextension=y\n"),
        )
        .unwrap();
        // Shipped by the PHP build — bare extension= line, must stay.
        std::fs::write(conf_d.join("35-pdo_pgsql.ini"), "extension=pdo_pgsql\n").unwrap();
        // User tunable, must stay.
        std::fs::write(conf_d.join("15-mytunables.ini"), "memory_limit=2G\n").unwrap();

        clean_stale_baseline_fragments(conf_d).unwrap();

        assert!(!conf_d.join("20-pdo_mysql.ini").exists());
        assert!(!conf_d.join("20-mbstring.ini").exists());
        assert!(conf_d.join("35-pdo_pgsql.ini").exists());
        assert!(conf_d.join("15-mytunables.ini").exists());
    }

    #[test]
    fn clean_stale_baseline_fragments_on_missing_dir_is_ok() {
        let td = tempfile::TempDir::new().unwrap();
        clean_stale_baseline_fragments(&td.path().join("does-not-exist")).unwrap();
    }

    #[test]
    fn materialize_closure_peer_creates_relative_symlink() {
        let td = tempfile::TempDir::new().unwrap();
        // Set up a fake shared store with the closure entry…
        let store = td.path();
        std::fs::create_dir_all(store.join("libcurl-8.20.0-abcdef01/lib")).unwrap();
        let ext_root = store.join("ext-curl-8.5.6+php85-nts-deadbeef");
        std::fs::create_dir_all(&ext_root).unwrap();

        materialize_closure_peer(&ext_root, "libcurl", "8.20.0", "abcdef01").unwrap();

        let link = ext_root.join("store/libcurl-8.20.0-abcdef01");
        let meta = std::fs::symlink_metadata(&link).unwrap();
        assert!(meta.file_type().is_symlink());
        // Resolves to the real shared-store entry.
        let target = std::fs::read_link(&link).unwrap();
        assert_eq!(target, PathBuf::from("../../libcurl-8.20.0-abcdef01"));
        assert!(std::fs::canonicalize(&link).unwrap().ends_with("libcurl-8.20.0-abcdef01"));
    }

    #[test]
    fn materialize_closure_peer_is_idempotent() {
        let td = tempfile::TempDir::new().unwrap();
        let store = td.path();
        std::fs::create_dir_all(store.join("libcurl-8.20.0-abcdef01")).unwrap();
        let ext_root = store.join("ext-curl-x");
        std::fs::create_dir_all(&ext_root).unwrap();
        materialize_closure_peer(&ext_root, "libcurl", "8.20.0", "abcdef01").unwrap();
        // Second call must not error or duplicate.
        materialize_closure_peer(&ext_root, "libcurl", "8.20.0", "abcdef01").unwrap();
    }

    #[test]
    fn materialize_closure_peer_refuses_to_overwrite_regular_file() {
        let td = tempfile::TempDir::new().unwrap();
        let ext_root = td.path().join("ext-x");
        std::fs::create_dir_all(ext_root.join("store")).unwrap();
        // A plain file occupies the path we'd want to symlink to.
        std::fs::write(ext_root.join("store/libcurl-1.0-aaaa"), "junk").unwrap();
        let err = materialize_closure_peer(&ext_root, "libcurl", "1.0", "aaaa").unwrap_err();
        assert!(err.to_string().contains("isn't a symlink"), "got: {err}");
    }

    // ---------- install_closure_peers (Phase 1) ----------

    /// Build a tool manifest with `entries` closure rows. URLs point at
    /// `https://example.invalid/...` so any attempted network fetch
    /// would fail noisily — the test must pre-populate every
    /// `store_path` so the helper's fetch branch is never taken.
    fn tool_manifest_with_closure(entries: &[(&str, &str, &str)]) -> crate::index::wire::Manifest {
        let closure_json: Vec<_> = entries
            .iter()
            .enumerate()
            .map(|(i, (name, ver, hash))| {
                serde_json::json!({
                    "name": name,
                    "version": ver,
                    "hash": hash,
                    "sha256": format!("{:0>64}", i),
                    "url": format!("https://example.invalid/{name}.tar.zst"),
                    "size": 0,
                })
            })
            .collect();
        serde_json::from_value(serde_json::json!({
            "schema": 1,
            "kind": "tool",
            "name": "mariadb",
            "tag": "mariadb-11.4.10-x86_64-unknown-linux-gnu-default",
            "version": "11.4.10",
            "target": "x86_64-unknown-linux-gnu",
            "flavor": "default",
            "libc": {"family":"gnu","min":"2.17"},
            "blob": {"url":"https://x/blob","sha256":"aa","size":0},
            "closure": closure_json,
        }))
        .unwrap()
    }

    fn pre_create_store_entry(paths: &Paths, name: &str, ver: &str, hash: &str) {
        // Pretend the closure blob has already been extracted into the
        // global store. The helper's fetch branch should skip; only
        // materialize_closure_peer should run.
        std::fs::create_dir_all(
            paths
                .store()
                .join(format!("{name}-{ver}-{hash}"))
                .join("lib"),
        )
        .unwrap();
    }

    #[test]
    fn install_closure_peers_creates_one_symlink_per_entry() {
        let td = tempfile::TempDir::new().unwrap();
        let paths = Paths::new(td.path().into(), td.path().join("cache"));
        // Three closure entries — a small but representative tool
        // closure (openssl + zlib + ncurses is exactly what redis and
        // mariadb need).
        let entries = [
            ("openssl", "3.5.6", "99c0f6e8"),
            ("zlib", "1.3.2", "jbmj2bcm"),
            ("ncurses", "6.6", "grpadm5y"),
        ];
        for (n, v, h) in &entries {
            pre_create_store_entry(&paths, n, v, h);
        }
        let manifest = tool_manifest_with_closure(&entries);

        let install_root = paths.store().join("mariadb-11.4.10");
        std::fs::create_dir_all(&install_root).unwrap();

        let client = reqwest::blocking::Client::new();
        let bar = DownloadBar::hidden();
        install_closure_peers(&client, &paths, &manifest, &install_root, &bar).unwrap();

        for (n, v, h) in &entries {
            let link = install_root.join("store").join(format!("{n}-{v}-{h}"));
            let meta = std::fs::symlink_metadata(&link)
                .unwrap_or_else(|e| panic!("expected symlink at {}: {e}", link.display()));
            assert!(meta.file_type().is_symlink(), "{} not a symlink", link.display());
            let target = std::fs::read_link(&link).unwrap();
            assert_eq!(target, PathBuf::from("../..").join(format!("{n}-{v}-{h}")));
            // And it actually resolves into the global store.
            let canon = std::fs::canonicalize(&link).unwrap();
            assert!(canon.ends_with(format!("{n}-{v}-{h}")), "canon = {}", canon.display());
        }
    }

    #[test]
    fn install_closure_peers_is_idempotent() {
        // Second invocation must not error (the existing peer symlinks
        // are left alone) and must not double-link.
        let td = tempfile::TempDir::new().unwrap();
        let paths = Paths::new(td.path().into(), td.path().join("cache"));
        let entries = [("openssl", "3.5.6", "99c0f6e8")];
        pre_create_store_entry(&paths, "openssl", "3.5.6", "99c0f6e8");
        let manifest = tool_manifest_with_closure(&entries);

        let install_root = paths.store().join("redis-8.6.3");
        std::fs::create_dir_all(&install_root).unwrap();
        let client = reqwest::blocking::Client::new();
        let bar = DownloadBar::hidden();
        install_closure_peers(&client, &paths, &manifest, &install_root, &bar).unwrap();
        install_closure_peers(&client, &paths, &manifest, &install_root, &bar).unwrap();

        // Exactly one entry under store/, still a symlink.
        let entries: Vec<_> = std::fs::read_dir(install_root.join("store"))
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(entries.len(), 1);
    }

    #[test]
    fn install_closure_peers_noop_on_empty_closure() {
        // Pre-split tool tarballs ship empty closure[]; the helper must
        // be a no-op so the daemon's fetch_blocking can call it
        // unconditionally without breaking old artifacts. This is the
        // backward-compat hinge in UNBUNDLE_PLAN.md §"Phase 1".
        let td = tempfile::TempDir::new().unwrap();
        let paths = Paths::new(td.path().into(), td.path().join("cache"));
        let manifest = tool_manifest_with_closure(&[]);
        let install_root = paths.store().join("mariadb-11.4.10");
        std::fs::create_dir_all(&install_root).unwrap();

        let client = reqwest::blocking::Client::new();
        let bar = DownloadBar::hidden();
        install_closure_peers(&client, &paths, &manifest, &install_root, &bar).unwrap();
        // No `store/` subdir is created when there's nothing to link.
        assert!(!install_root.join("store").exists());
    }

    #[test]
    fn plan_closure_bytes_only_counts_missing() {
        // The bar's planned total reflects what we'll actually need to
        // download. If half the closure is already on disk from an
        // earlier install (the dedup happy path), planning must skip
        // those entries — otherwise the bar overshoots and never fills.
        let td = tempfile::TempDir::new().unwrap();
        let paths = Paths::new(td.path().into(), td.path().join("cache"));
        let entries = [
            ("openssl", "3.5.6", "99c0f6e8"),
            ("zlib", "1.3.2", "jbmj2bcm"),
        ];
        // Only openssl is already on disk.
        pre_create_store_entry(&paths, "openssl", "3.5.6", "99c0f6e8");

        // Hand-build a manifest where each entry advertises a distinct
        // non-zero size so we can assert exactly which one was counted.
        let manifest: crate::index::wire::Manifest = serde_json::from_value(serde_json::json!({
            "schema": 1, "kind": "tool", "name": "mariadb",
            "tag": "mariadb-11.4.10-x", "version": "11.4.10",
            "target": "x86_64-unknown-linux-gnu", "flavor": "default",
            "libc": {"family":"gnu","min":"2.17"},
            "blob": {"url":"https://x","sha256":"aa","size":0},
            "closure": [
                {"name":"openssl","version":"3.5.6","hash":"99c0f6e8",
                 "sha256":format!("{:0>64}",0),"url":"https://x/o","size": 1_000},
                {"name":"zlib","version":"1.3.2","hash":"jbmj2bcm",
                 "sha256":format!("{:0>64}",1),"url":"https://x/z","size": 500},
            ],
        })).unwrap();

        let bar = DownloadBar::hidden();
        plan_closure_bytes(&paths, &manifest, &bar);
        // openssl (1000) is on disk → skipped. zlib (500) is missing →
        // counted. So planned == 500.
        assert_eq!(bar.planned(), 500, "expected only the missing entry to be counted");
        let _ = entries;
    }
}
