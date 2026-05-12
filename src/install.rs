//! Orchestrates a PHP interpreter installation: refresh index, resolve,
//! fetch + extract. Shared by `bougie php install` and `bougie sync`.

use crate::baseline::{BaselineFilter, BASELINE_EXTENSIONS};
use crate::errors::BougieError;
use crate::fetch::{fetch_blob, BlobSpec};
use crate::index::{
    build_verifier,
    fetch::{fetch_manifest, fetch_root, fetch_section},
    wire::LoadDirective,
};
use crate::lock::ExclusiveGuard;
use crate::paths::Paths;
use crate::request::{Flavor, Request};
use crate::resolve::{resolve_extension, resolve_php, ResolveOptions, Selected};
use crate::store::install_dir;
use crate::target::Triple;
use crate::version::{PartialVersion, Version};
use eyre::{eyre, Result, WrapErr};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

pub const DEFAULT_INDEX_URL: &str = "https://index.bougie.tools";
const SECTION_NAME: &str = "interpreter/php";
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
            hint: format!(
                "the index at {host} advertises: {}",
                available.join(", ")
            ),
        }
    })?;
    let section_ref =
        target_entry.sections.get(SECTION_NAME).ok_or_else(|| BougieError::Resolution {
            kind: "section".into(),
            detail: format!("the index at {host} has no `{SECTION_NAME}` section under target {target}"),
        })?;
    let section = fetch_section(
        &client,
        &host,
        &cache_root,
        &fetched.root.version,
        &target,
        SECTION_NAME,
        &section_ref.sha256,
    )?;

    let selected: Selected<'_> = resolve_php(&section, &spec, flavor, opts)?;
    let dest = install_dir(paths, selected.version, flavor);
    let already_present = dest.exists();
    if !already_present {
        let manifest = fetch_manifest(
            &client,
            &host,
            &cache_root,
            &selected.artifact.manifest.path,
            &selected.artifact.manifest.sha256,
        )?;
        let blob_spec = BlobSpec {
            url: &manifest.blob.url,
            sha256: &manifest.blob.sha256,
            partial_dir: &paths.cache_blobs(),
            dest: &dest,
        };
        fetch_blob(&client, &blob_spec)?;
    }

    Ok(InstalledPhp {
        version: selected.version,
        flavor,
        install_path: dest,
        already_present,
        frozen_warning: selected.frozen_warning,
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
pub fn install_extension(
    paths: &Paths,
    name: &str,
    version_pin: Option<&str>,
    php_minor: PartialVersion,
    flavor: Flavor,
    opts: ResolveOptions,
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

    if !already_present {
        let blob_spec = BlobSpec {
            url: &manifest.blob.url,
            sha256: &manifest.blob.sha256,
            partial_dir: &paths.cache_blobs(),
            dest: &dest,
        };
        fetch_blob(&client, &blob_spec)?;
    }

    // Walk the manifest's bundled-C-lib closure and fetch any
    // store-paths the consumer doesn't have yet. Mandatory: the
    // extension `.so` was built with `$ORIGIN/../../<storeName>/lib`
    // RPATHs into peers in the same store; without these tarballs,
    // dlopen falls back to the system loader and surfaces errors like
    // `libicuuc.so.77: cannot open shared object file` for intl or
    // `libcurl: undefined symbol: ENGINE_init` for curl (system libcurl
    // built against a different OpenSSL).
    //
    // Run unconditionally — `dest.exists()` only tells us the .so
    // blob is present; the closure may still be partial from an
    // earlier bougie release that didn't walk it.
    for closure in &manifest.closure {
        let store_path = store_dir_for_closure(paths, &closure.name, &closure.version, &closure.hash);
        if store_path.exists() {
            continue;
        }
        let blob_spec = BlobSpec {
            url: &closure.url,
            sha256: &closure.sha256,
            partial_dir: &paths.cache_blobs(),
            dest: &store_path,
        };
        fetch_blob(&client, &blob_spec).wrap_err_with(|| {
            format!(
                "fetching closure entry `{}-{}-{}` for {}",
                closure.name, closure.version, closure.hash, manifest.tag
            )
        })?;
    }

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

    for &name in BASELINE_EXTENSIONS {
        if !filter.includes(name) {
            continue;
        }
        match install_extension(paths, name, None, php_minor, flavor, resolve_opts) {
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

/// `$BOUGIE_HOME/store/<name>-<version>-<hash>/` for a closure entry.
/// Thin wrapper over [`crate::store::store_dir`] kept here so callers
/// don't need to import the store module just to compute closure
/// destinations.
fn store_dir_for_closure(paths: &Paths, name: &str, version: &str, hash: &str) -> PathBuf {
    crate::store::store_dir(paths, name, version, hash)
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
}
