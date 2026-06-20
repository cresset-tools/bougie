//! Shared callback wiring used by `tool install` / `inject` /
//! `uninject` / `upgrade`. Centralised here so the per-command
//! modules don't each re-derive the (`paths` × `PhpChoice` × index)
//! plumbing.

use bougie_errors::BougieError;
use bougie_installer::baseline::{BASELINE_EXTENSIONS, BUILTIN_EXTENSIONS, BaselineFilter};
use bougie_installer::conf_d::write_ext_fragment_into;
use bougie_installer::install::{
    DEFAULT_INDEX_URL, install_baseline_into, install_extension, install_php,
};
use bougie_paths::Paths;
use bougie_platform::target::Triple;
use bougie_resolver::ResolveOptions;
use bougie_tool::classify::ExtensionClassifier;
use bougie_tool::install::ExtInstaller;
use bougie_tool::resolve::{BaselineEnsurer, PhpChoice, PhpInstaller, RequiredPhpFetcher};
use bougie_version::request::Flavor;
use bougie_version::request::parse_request as parse_php_request;
use bougie_version::version::PartialVersion;
use eyre::{Result, WrapErr};
use std::path::PathBuf;

/// Resolver that calls back into the bougie binary's own
/// `composer_update::resolve_and_write_lock`. Constructed at the
/// call site because that helper lives in `super::composer_update`.
pub use bougie_tool::install::LockResolver;

/// Build the `RequiredPhpFetcher` callback. Hits Packagist v2
/// metadata for `package`, picks the highest stable version matching
/// the user's `@<constraint>` (or `*` if unspecified), and returns
/// that version's `require.php` verbatim. `None` when the package
/// doesn't pin PHP; `Err` on network / parse failure (the install
/// flow surfaces those as a warning and falls back to the legacy
/// default).
pub fn required_php_fetcher() -> Box<RequiredPhpFetcher> {
    Box::new(
        |paths: &Paths, package: &str, user_constraint: &str| -> Result<Option<String>> {
            use bougie_composer_resolver::metadata::{
                Repo, Variant, fetch_package_metadata,
            };
            let client = reqwest::blocking::Client::new();
            let repo = Repo::packagist();
            let metadata = fetch_package_metadata(&client, paths, &repo, package, Variant::Stable)
                .wrap_err_with(|| format!("fetching Packagist metadata for `{package}`"))?;
            let Some(versions) = metadata.packages.get(package) else {
                return Ok(None);
            };
            let parsed_constraint = composer_semver::Constraint::parse(user_constraint)
                .map_err(|e| eyre::eyre!("parsing user constraint `{user_constraint}`: {e}"))?;
            // Versions are newest-first; pick the first one that
            // matches the user's @<constraint>.
            for v in versions {
                let Ok(ver) = composer_semver::Version::parse(&v.version) else {
                    continue;
                };
                if parsed_constraint.matches(&ver) {
                    return Ok(v.require.get("php").cloned());
                }
            }
            Ok(None)
        },
    )
}

/// Build the `BaselineEnsurer` callback. Calls
/// `install_baseline_into` so the chosen PHP has `phar`, `mbstring`,
/// `tokenizer`, `dom`, etc. loadable before the tool's composer
/// install + first run. Idempotent: the installer's per-extension
/// skip-if-installed check makes repeat calls cheap.
///
/// Per-extension failures land in `BaselineReport.failed`; surfaced
/// as warnings rather than hard errors so a single yanked baseline
/// extension doesn't block tool installs.
pub fn baseline_ensurer() -> Box<BaselineEnsurer> {
    Box::new(|paths: &Paths, php: &PhpChoice| -> Result<()> {
        let (php_minor, flavor) = parse_php_choice(php)?;
        let install_root = php
            .bin
            .parent()
            .and_then(std::path::Path::parent)
            .ok_or_else(|| {
                eyre::eyre!(
                    "php_resolved_path is too shallow to derive an install root: {}",
                    php.bin.display()
                )
            })?;
        let report = install_baseline_into(
            paths,
            install_root,
            php_minor,
            flavor,
            &BaselineFilter::All,
            ResolveOptions::default(),
        );
        for (name, err) in &report.failed {
            eprintln!("warning: baseline extension `{name}` failed: {err}");
        }
        Ok(())
    })
}

/// Build the `PhpInstaller` callback. Auto-installs the requested PHP
/// via `bougie_installer::install::install_php` when the resolved
/// triplet isn't on disk.
pub fn php_installer() -> Box<PhpInstaller> {
    Box::new(|paths: &Paths, spec: &str| -> Result<PhpChoice> {
        let request = parse_php_request(spec)
            .wrap_err_with(|| format!("parsing --php value `{spec}`"))?;
        let installed = install_php(paths, &request, None, ResolveOptions::default())
            .wrap_err_with(|| format!("installing PHP for --php {spec}"))?;
        Ok(PhpChoice {
            bin: installed.install_path.join("bin").join("php"),
            version: installed.version.to_string(),
            flavor: installed.flavor.as_str().to_string(),
        })
    })
}

/// Build the extension classifier. Cheap baseline check first (no
/// I/O), then a per-PHP-minor index lookup for non-baseline names.
///
/// Distinguishes "name not in index" from "name in index but no
/// compatible artifact" by inspecting the structured error's detail
/// string: only the former (the bougie-index backend's "no
/// `extension/X` section under target" path, or the windows backend's
/// "no compile-time `WINDOWS_PECL_VERSIONS` entry") truly means
/// not-an-extension. The latter — `intl` is published but not for
/// the tool's PHP — should classify as `Extension` so the user gets
/// the precise "no compatible artifact" error from the install step,
/// not a misleading "use vendor/name" hint.
///
/// Network / signature / unknown-target errors come back as different
/// `BougieError` variants and propagate, so a flaky connection can't
/// silently reclassify a real extension as not-an-extension.
pub fn extension_classifier() -> Box<ExtensionClassifier> {
    Box::new(|name: &str, php: &PhpChoice| -> Result<bool> {
        if BASELINE_EXTENSIONS.contains(&name) || BUILTIN_EXTENSIONS.contains(&name) {
            return Ok(true);
        }
        let paths = Paths::from_env()?;
        let target = Triple::detect()?;
        let host = std::env::var("BOUGIE_INDEX_URL")
            .unwrap_or_else(|_| DEFAULT_INDEX_URL.into());
        let backend = bougie_backend::select(&target, &host, &paths)?;
        let (php_minor, flavor) = parse_php_choice(php)?;
        match backend.resolve_extension(name, php_minor, flavor, None, ResolveOptions::default())
        {
            Ok(_) => Ok(true),
            Err(e) => match e.downcast_ref::<BougieError>() {
                Some(BougieError::Resolution { kind, detail }) if kind == "extension"
                    && is_name_unknown_detail(detail) =>
                {
                    Ok(false)
                }
                _ => Err(e).wrap_err_with(|| {
                    format!("checking whether `{name}` is a known PHP extension")
                }),
            },
        }
    })
}

/// True when a `Resolution { kind: "extension", detail }` is the
/// backend saying "this name isn't in my index" as opposed to "name
/// is in my index but the requested artifact doesn't exist."
///
/// Detail wording matches the two emit sites:
/// - bougie-index backend, `bougie_index_backend.rs`:
///   "the index at HOST has no `extension/X` section under target T"
/// - windows.php.net backend, `windows_php_net.rs`:
///   "no compile-time `WINDOWS_PECL_VERSIONS` entry for ext-X"
///
/// The resolver's "no candidate satisfies LABEL" (artifact-selection
/// failure for a known section) is intentionally NOT matched here.
fn is_name_unknown_detail(detail: &str) -> bool {
    detail.contains(" section under target ")
        || detail.contains("`WINDOWS_PECL_VERSIONS` entry")
}

/// Build the extension installer. Calls `install_extension` for the
/// `(php_minor, flavor)` the receipt records, then emits the matching
/// conf.d fragment under the tool's `conf.d/` dir via the
/// installer's existing fragment-writer.
pub fn extension_installer() -> Box<ExtInstaller> {
    Box::new(|paths: &Paths, name: &str, php: &PhpChoice, conf_d: &std::path::Path| -> Result<PathBuf> {
        let (php_minor, flavor) = parse_php_choice(php)?;
        let installed = install_extension(
            paths,
            name,
            None,
            php_minor,
            flavor,
            ResolveOptions::default(),
        )
        .wrap_err_with(|| format!("installing extension `{name}` for tool"))?;
        let ini = write_ext_fragment_into(
            conf_d,
            &installed.name,
            &installed.so_path,
            installed.load,
            &installed.path_extras,
        )
        .wrap_err_with(|| format!("writing conf.d fragment for `{name}`"))?;
        Ok(ini)
    })
}

/// Split a [`PhpChoice`] into `(php_minor, flavor)` shapes the
/// installer + backend expect.
fn parse_php_choice(php: &PhpChoice) -> Result<(PartialVersion, Flavor)> {
    let pv = PartialVersion::parse(&php.version)
        .wrap_err_with(|| format!("parsing receipt php_version `{}`", php.version))?;
    let flavor = parse_flavor(&php.flavor)
        .ok_or_else(|| eyre::eyre!("parsing receipt php_flavor `{}`", php.flavor))?;
    Ok((pv, flavor))
}

fn parse_flavor(s: &str) -> Option<Flavor> {
    Some(match s {
        "nts" => Flavor::Nts,
        "nts-debug" => Flavor::NtsDebug,
        "zts" => Flavor::Zts,
        "zts-debug" => Flavor::ZtsDebug,
        _ => return None,
    })
}

