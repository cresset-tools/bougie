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
/// `<install_root>/etc/php/conf.d/20-<name>.ini` so the interpreter
/// auto-loads them. Errors per extension are downgraded to entries
/// in [`BaselineReport::failed`] — the caller decides whether to
/// surface them or treat them as a hard failure.
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

/// Write `20-<name>.ini` under `<install>/etc/php/conf.d/` referencing
/// the content-addressed store path of the just-installed `.so`. PHP's
/// alphabetic conf.d scan loads `20-*` after `10-opcache.ini` but
/// before any user `50-*.ini` overrides — matching the prefix
/// php-build-standalone already uses for non-zend extensions.
fn write_install_conf_d(conf_d: &Path, installed: &InstalledExt) -> Result<()> {
    let path = conf_d.join(format!("20-{}.ini", installed.name));
    let body = format!(
        "; managed by bougie — baseline extension {name} {version}\n\
         {directive}={so}\n",
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
}
