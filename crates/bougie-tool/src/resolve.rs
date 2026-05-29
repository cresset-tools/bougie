//! PHP interpreter selection for tools.
//!
//! Two paths:
//!
//! - **`spec = None`**: pick the highest installed NTS PHP, the
//!   ergonomic default for `bougie tool install <pkg>`.
//! - **`spec = Some(_)`**: parse the user-supplied `--php <ver>` (a
//!   version `8.3`, an exact `8.3.12`, or a constraint `~8.3`),
//!   filter installed PHPs by match, return the highest. If nothing
//!   matches, fall back to the installer callback the bougie binary
//!   supplies — that wraps `bougie_installer::install::install_php`
//!   so a fresh install pays the index round-trip transparently.
//!
//! NTS is the default flavor: CLI tools like phpstan / php-cs-fixer
//! don't need threads, and a chunk of the extension ecosystem ships
//! NTS-only.

use bougie_fs::store;
use bougie_paths::Paths;
use bougie_version::request::{Request, VersionLike, parse_request};
use bougie_version::version::{PartialVersion, Version};
use eyre::{Result, WrapErr, bail};
use std::path::PathBuf;

/// The interpreter a tool will be pinned to.
#[derive(Debug, Clone)]
pub struct PhpChoice {
    pub version: String,
    pub flavor: String,
    /// Path to the `php` binary itself, ready to drop into the
    /// receipt's `php_resolved_path`.
    pub bin: PathBuf,
}

/// Callback supplied by the bougie binary that runs `install_php`.
/// Kept as a callback so this crate doesn't depend on
/// `bougie-installer` (which pulls a lot of the world in). The
/// callback receives the original user spec string verbatim — it
/// re-parses internally because `install_php` wants a `Request`, and
/// re-parsing is cheaper than smuggling the parsed value across the
/// crate boundary.
pub type PhpInstaller = dyn Fn(&Paths, &str) -> Result<PhpChoice> + Send + Sync;

/// Callback that fetches a tool package's `require.php` constraint
/// from Packagist (cached). Returns `None` when the package doesn't
/// pin PHP. Hosted by the bougie binary so this crate stays free of
/// reqwest + the resolver's metadata machinery.
///
/// Arguments: `paths`, `package` (`vendor/name`), `user_constraint`
/// (the `@<constraint>` segment of the install request, or `*` when
/// unspecified). The fetcher picks the highest stable version
/// matching `user_constraint` and returns its `require.php` verbatim
/// (a composer-style constraint like `^7.2 || ^8.0`).
pub type RequiredPhpFetcher =
    dyn Fn(&Paths, &str, &str) -> Result<Option<String>> + Send + Sync;

/// Callback that ensures the chosen PHP install has its baseline
/// extensions (phar, mbstring, tokenizer, dom, …) installed and
/// loadable. Newer PHP builds in bougie's index ship a *bare*
/// tarball — no conf.d entries, only builtins available. Without
/// this step a tool that calls `Phar::…` or `mb_…` crashes at
/// runtime. Idempotent: a no-op when the baseline is already
/// installed.
///
/// Hosted by the bougie binary so this crate stays free of
/// `bougie-installer::install::install_baseline_into`.
pub type BaselineEnsurer =
    dyn Fn(&Paths, &PhpChoice) -> Result<()> + Send + Sync;

/// Pick a PHP that satisfies `php_constraint` (a composer-style
/// constraint string, typically a package's `require.php`). Prefers
/// the highest installed NTS PHP that matches; falls through to
/// `installer` if none does, passing the same constraint verbatim so
/// `bougie php install` resolves the latest stable satisfying it.
pub fn pick_php_for_constraint(
    paths: &Paths,
    php_constraint: &str,
    installer: &PhpInstaller,
) -> Result<PhpChoice> {
    let parsed = bougie_semver::Constraint::parse(php_constraint)
        .map_err(|e| eyre::eyre!("parsing tool's require.php `{php_constraint}`: {e}"))?;
    let on_disk = store::list_installed(paths)
        .map_err(|e| eyre::eyre!("listing installed PHPs: {e}"))?;
    let mut candidates: Vec<(Version, String)> = on_disk
        .into_iter()
        .filter(|(_, flavor)| flavor == "nts")
        .filter_map(|(v, f)| v.parse::<Version>().ok().map(|p| (p, f)))
        .filter(|(v, _)| {
            bougie_semver::Version::parse(&v.to_string())
                .is_ok_and(|lifted| parsed.matches(&lifted))
        })
        .collect();
    if let Some((version, flavor)) = candidates.drain(..).max_by(|a, b| a.0.cmp(&b.0)) {
        let version_str = version.to_string();
        let bin = paths
            .installs()
            .join(format!("{version_str}-{flavor}"))
            .join("bin")
            .join("php");
        return Ok(PhpChoice {
            version: version_str,
            flavor,
            bin,
        });
    }
    installer(paths, php_constraint).wrap_err_with(|| {
        format!("auto-installing PHP for tool require.php `{php_constraint}`")
    })
}

/// Pick a PHP for a new tool install. `spec` is the user's
/// `--php <ver>` argument or `None` for the default.
pub fn pick_php(
    paths: &Paths,
    spec: Option<&str>,
    installer: &PhpInstaller,
) -> Result<PhpChoice> {
    let on_disk = store::list_installed(paths)
        .map_err(|e| eyre::eyre!("listing installed PHPs: {e}"))?;

    let parsed_spec = match spec {
        Some(s) => Some(
            parse_request(s).wrap_err_with(|| format!("parsing --php value `{s}`"))?,
        ),
        None => None,
    };
    let (target_flavor, version_filter) = match &parsed_spec {
        Some(Request::VersionLike { spec, flavor }) => (
            flavor.map_or_else(|| "nts".to_string(), |f| f.as_str().to_string()),
            Some(spec),
        ),
        Some(other) => {
            bail!("--php only accepts a version or constraint; got {other:?}");
        }
        None => ("nts".into(), None),
    };

    let mut candidates: Vec<(Version, String)> = on_disk
        .into_iter()
        .filter(|(_, flavor)| flavor == &target_flavor)
        .filter_map(|(v, f)| v.parse::<Version>().ok().map(|parsed| (parsed, f)))
        .filter(|(v, _)| version_filter.is_none_or(|spec| version_matches(v, spec)))
        .collect();

    if let Some((version, flavor)) = candidates
        .drain(..)
        .max_by(|a, b| a.0.cmp(&b.0))
    {
        let version_str = version.to_string();
        let bin = paths
            .installs()
            .join(format!("{version_str}-{flavor}"))
            .join("bin")
            .join("php");
        return Ok(PhpChoice {
            version: version_str,
            flavor,
            bin,
        });
    }

    match spec {
        Some(s) => installer(paths, s)
            .wrap_err_with(|| format!("auto-installing PHP for --php {s}")),
        None => bail!(
            "no NTS PHP installed. Install one with `bougie php install <version>` \
             (e.g. `bougie php install 8.3`), or rerun with `--php <ver>` to \
             auto-install."
        ),
    }
}

/// Mirror of `bougie-resolver`'s private `version_matches_spec`.
/// Inlined rather than crossing a crate boundary for one helper, and
/// rather than making the resolver helper `pub` (avoids surface bloat).
fn version_matches(v: &Version, spec: &VersionLike) -> bool {
    match spec {
        VersionLike::Version(pv) => matches_partial(v, pv),
        VersionLike::Constraint(c) => {
            // Constraint matching is defined against the semver-shaped
            // Version. Lift bougie's exact triple into a Composer-normalized
            // "X.Y.Z" — same trick `bougie-resolver` uses.
            let Ok(lifted) = bougie_semver::Version::parse(&v.to_string()) else {
                return false;
            };
            c.matches(&lifted)
        }
    }
}

fn matches_partial(v: &Version, pv: &PartialVersion) -> bool {
    if v.major != pv.major {
        return false;
    }
    if let Some(m) = pv.minor
        && v.minor != m
    {
        return false;
    }
    if let Some(p) = pv.patch
        && v.patch != p
    {
        return false;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths(td: &std::path::Path) -> Paths {
        Paths::new(td.to_path_buf(), td.join("cache"))
    }

    fn fail_installer() -> Box<PhpInstaller> {
        Box::new(|_: &Paths, spec: &str| -> Result<PhpChoice> {
            bail!("test installer should not be called; received `{spec}`")
        })
    }

    fn dummy_installer(version: &'static str, flavor: &'static str) -> Box<PhpInstaller> {
        let v = version.to_string();
        let f = flavor.to_string();
        Box::new(move |paths: &Paths, _spec: &str| -> Result<PhpChoice> {
            Ok(PhpChoice {
                version: v.clone(),
                flavor: f.clone(),
                bin: paths
                    .installs()
                    .join(format!("{v}-{f}"))
                    .join("bin")
                    .join("php"),
            })
        })
    }

    fn install_fake(paths: &Paths, version: &str, flavor: &str) {
        let dir = paths.installs().join(format!("{version}-{flavor}"));
        std::fs::create_dir_all(&dir).unwrap();
    }

    #[test]
    fn picks_highest_installed_nts_when_no_spec() {
        let td = tempfile::TempDir::new().unwrap();
        let p = paths(td.path());
        install_fake(&p, "8.3.12", "nts");
        install_fake(&p, "8.4.0", "nts");
        install_fake(&p, "8.4.0", "zts");
        let inst = fail_installer();
        let choice = pick_php(&p, None, inst.as_ref()).unwrap();
        assert_eq!(choice.version, "8.4.0");
        assert_eq!(choice.flavor, "nts");
    }

    #[test]
    fn no_installed_php_without_spec_is_user_error() {
        let td = tempfile::TempDir::new().unwrap();
        let p = paths(td.path());
        let inst = fail_installer();
        let err = pick_php(&p, None, inst.as_ref()).unwrap_err().to_string();
        assert!(err.contains("no NTS PHP installed"), "{err}");
    }

    #[test]
    fn matches_partial_version_spec() {
        let td = tempfile::TempDir::new().unwrap();
        let p = paths(td.path());
        install_fake(&p, "8.3.12", "nts");
        install_fake(&p, "8.4.0", "nts");
        let inst = fail_installer();
        let choice = pick_php(&p, Some("8.3"), inst.as_ref()).unwrap();
        assert_eq!(choice.version, "8.3.12");
    }

    #[test]
    fn falls_back_to_installer_when_no_match() {
        let td = tempfile::TempDir::new().unwrap();
        let p = paths(td.path());
        install_fake(&p, "8.3.12", "nts");
        let inst = dummy_installer("8.4.5", "nts");
        let choice = pick_php(&p, Some("8.4"), inst.as_ref()).unwrap();
        assert_eq!(choice.version, "8.4.5");
        assert!(choice.bin.ends_with("installs/8.4.5-nts/bin/php"));
    }

    #[test]
    fn exact_patch_match_succeeds() {
        let td = tempfile::TempDir::new().unwrap();
        let p = paths(td.path());
        install_fake(&p, "8.3.12", "nts");
        install_fake(&p, "8.3.13", "nts");
        let inst = fail_installer();
        let choice = pick_php(&p, Some("8.3.12"), inst.as_ref()).unwrap();
        assert_eq!(choice.version, "8.3.12");
    }

    #[test]
    fn for_constraint_picks_highest_installed_matching() {
        let td = tempfile::TempDir::new().unwrap();
        let p = paths(td.path());
        install_fake(&p, "7.4.33", "nts");
        install_fake(&p, "8.0.30", "nts");
        install_fake(&p, "8.3.12", "nts");
        install_fake(&p, "8.4.0", "nts");
        let inst = fail_installer();
        // "^7.2 || ^8.0" — accepts everything we installed; should
        // pick 8.4.0.
        let choice = pick_php_for_constraint(&p, "^7.2 || ^8.0", inst.as_ref()).unwrap();
        assert_eq!(choice.version, "8.4.0");
    }

    #[test]
    fn for_constraint_picks_older_when_upper_bound_excludes_newer() {
        let td = tempfile::TempDir::new().unwrap();
        let p = paths(td.path());
        install_fake(&p, "8.0.30", "nts");
        install_fake(&p, "8.4.0", "nts");
        let inst = fail_installer();
        let choice = pick_php_for_constraint(&p, "^8.0,<8.1", inst.as_ref()).unwrap();
        assert_eq!(choice.version, "8.0.30");
    }

    #[test]
    fn for_constraint_falls_back_to_installer_when_none_match() {
        let td = tempfile::TempDir::new().unwrap();
        let p = paths(td.path());
        install_fake(&p, "8.4.0", "nts");
        let inst = dummy_installer("7.4.33", "nts");
        // ^7.2 — 8.4 doesn't match; installer should be called with
        // the constraint verbatim.
        let choice = pick_php_for_constraint(&p, "^7.2", inst.as_ref()).unwrap();
        assert_eq!(choice.version, "7.4.33");
    }
}
