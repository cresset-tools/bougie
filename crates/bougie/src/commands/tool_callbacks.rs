//! Shared callback wiring used by `tool install` / `inject` /
//! `uninject` / `upgrade`. Centralised here so the per-command
//! modules don't each re-derive the (`paths` × `PhpChoice` × index)
//! plumbing.

use bougie_installer::baseline::{BASELINE_EXTENSIONS, BUILTIN_EXTENSIONS};
use bougie_installer::conf_d::write_ext_fragment_into;
use bougie_installer::install::{DEFAULT_INDEX_URL, install_extension, install_php};
use bougie_paths::Paths;
use bougie_platform::target::Triple;
use bougie_resolver::ResolveOptions;
use bougie_tool::classify::ExtensionClassifier;
use bougie_tool::install::ExtInstaller;
use bougie_tool::resolve::{PhpChoice, PhpInstaller};
use bougie_version::request::Flavor;
use bougie_version::request::parse_request as parse_php_request;
use bougie_version::version::PartialVersion;
use eyre::{Result, WrapErr};
use std::path::PathBuf;

/// Resolver that calls back into the bougie binary's own
/// `composer_update::resolve_and_write_lock`. Constructed at the
/// call site because that helper lives in `super::composer_update`.
pub use bougie_tool::install::LockResolver;

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
/// Backend errors that look like "section not found" turn into
/// `Ok(false)` (so the classifier's call site can suggest the slash
/// form); other errors propagate.
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
            Err(e) if looks_like_not_found(&e) => Ok(false),
            Err(e) => Err(e).wrap_err_with(|| {
                format!("checking whether `{name}` is a known PHP extension")
            }),
        }
    })
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

/// Conservative heuristic for "the index doesn't know about this
/// extension." Backend errors that contain typical not-found phrases
/// classify as `Ok(false)` upstream; everything else propagates so
/// transient network or auth issues don't silently misclassify.
fn looks_like_not_found(e: &eyre::Report) -> bool {
    let msg = format!("{e:#}");
    let lower = msg.to_lowercase();
    lower.contains("no extension")
        || lower.contains("not found")
        || lower.contains("missing section")
        || lower.contains("404")
}

