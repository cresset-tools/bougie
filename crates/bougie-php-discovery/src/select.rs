//! PHP source selection — pick a managed-installed, system, or
//! to-be-downloaded managed PHP per the user's preference.
//!
//! This is uv's system-Python model adapted to PHP. The policy here is
//! **pure**: it takes already-gathered candidate sets (installed
//! managed builds + probed system PHPs) and a [`PhpPreference`], and
//! returns a [`Selection`]. Gathering the candidates (scanning
//! `installs/`, running [`crate::discover`] + [`crate::probe`]) and
//! acting on a [`Selection::Download`] live one layer up, in the sync
//! command — keeping this layer trivially unit-testable.

use crate::SystemPhp;
use bougie_version::matches::version_satisfies;
use bougie_version::request::{Flavor, VersionLike};
use bougie_version::version::Version;
use eyre::{eyre, Result};

/// How to choose between managed and system PHPs — uv's
/// `PythonPreference`, three relevant states.
///
/// Derived from the `--managed-php` / `--no-managed-php` flags and the
/// `[php] managed` config key (see [`PhpPreference::resolve`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PhpPreference {
    /// Only ever use a bougie-managed PHP (installed or downloaded);
    /// never a system PHP. `--managed-php` / `[php] managed = true`.
    OnlyManaged,
    /// uv's default: prefer an already-installed managed PHP, then an
    /// adequate system PHP, then download a managed PHP.
    #[default]
    Managed,
    /// Only ever use a system PHP; never managed. `--no-managed-php` /
    /// `[php] managed = false`.
    OnlySystem,
}

impl PhpPreference {
    /// Resolve the preference from CLI flags (which win) falling back to
    /// the `[php] managed` config value.
    ///
    /// `managed` ← `--managed-php`, `no_managed` ← `--no-managed-php`
    /// (mutually exclusive). `config_managed` ← `[php] managed`
    /// (`Some(true)` ⇒ only-managed, `Some(false)` ⇒ only-system,
    /// `None` ⇒ default).
    pub fn resolve(managed: bool, no_managed: bool, config_managed: Option<bool>) -> Result<Self> {
        if managed && no_managed {
            return Err(eyre!(
                "`--managed-php` and `--no-managed-php` are mutually exclusive"
            ));
        }
        if managed {
            return Ok(Self::OnlyManaged);
        }
        if no_managed {
            return Ok(Self::OnlySystem);
        }
        Ok(match config_managed {
            Some(true) => Self::OnlyManaged,
            Some(false) => Self::OnlySystem,
            None => Self::Managed,
        })
    }
}

/// What the project requires of a PHP.
#[derive(Debug, Clone, Copy)]
pub struct Requirement<'a> {
    /// The `require.php` version constraint (or `None` = any version).
    pub spec: Option<&'a VersionLike>,
    /// The required thread-safety flavor.
    pub flavor: Flavor,
    /// Required `ext-<name>` short names (without the `ext-` prefix).
    /// A **system** PHP must already load every one of these to qualify.
    pub required_exts: &'a [String],
}

/// The chosen PHP source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Selection {
    /// Reuse an already-installed managed PHP at this version + flavor.
    ManagedInstalled { version: Version, flavor: Flavor },
    /// Use this probed system PHP.
    System(SystemPhp),
    /// No suitable PHP is present; the caller should download + install
    /// a managed PHP for the requirement.
    Download,
}

/// Apply the preference to the candidate sets.
///
/// - `managed_installed`: `(version, flavor)` pairs already under
///   `installs/` (from `bougie_fs::store::list_installed`, parsed).
/// - `system`: probed system PHPs.
/// - `downloads`: whether downloading a managed PHP is permitted
///   (`--no-php-downloads` ⇒ `false`).
///
/// Ordering per [`PhpPreference`]:
/// - `Managed` (default): managed-installed → fpm-capable system →
///   download → any qualifying system (fpm-less, last resort).
/// - `OnlyManaged`: managed-installed → download.
/// - `OnlySystem`: system only (fpm-agnostic).
///
/// The default path prefers a system PHP that ships `php-fpm` over one
/// that doesn't, falling through to a managed *download* when the only
/// qualifying system PHP is CLI-only — so `bougie server` (which needs
/// fpm) works out of the box. The fpm-less system PHP is still chosen as
/// a last resort when downloads are disabled (`--no-php-downloads`),
/// since it remains usable for CLI (`bougie run`).
pub fn select(
    pref: PhpPreference,
    downloads: bool,
    req: Requirement<'_>,
    managed_installed: &[(Version, Flavor)],
    system: &[SystemPhp],
) -> Result<Selection> {
    let managed = || best_managed(req, managed_installed);
    let sys = || best_system(req, system);
    let sys_with_fpm = || best_system_matching(req, system, true);

    let selection = match pref {
        PhpPreference::OnlyManaged => managed()
            .map(|(version, flavor)| Selection::ManagedInstalled { version, flavor })
            .or_else(|| downloads.then_some(Selection::Download)),
        PhpPreference::Managed => managed()
            .map(|(version, flavor)| Selection::ManagedInstalled { version, flavor })
            .or_else(|| sys_with_fpm().cloned().map(Selection::System))
            .or_else(|| downloads.then_some(Selection::Download))
            .or_else(|| sys().cloned().map(Selection::System)),
        PhpPreference::OnlySystem => sys().cloned().map(Selection::System),
    };

    selection.ok_or_else(|| no_candidate_error(pref, downloads, req, system))
}

/// Highest-versioned installed managed build matching flavor + spec.
fn best_managed(req: Requirement<'_>, installed: &[(Version, Flavor)]) -> Option<(Version, Flavor)> {
    installed
        .iter()
        .filter(|(_, flavor)| *flavor == req.flavor)
        .filter(|(v, _)| req.spec.is_none_or(|spec| version_satisfies(v, spec)))
        .max_by(|a, b| a.0.cmp(&b.0))
        .copied()
}

/// Highest-versioned system PHP matching flavor + spec **and** loading
/// every required extension.
fn best_system<'a>(req: Requirement<'_>, system: &'a [SystemPhp]) -> Option<&'a SystemPhp> {
    best_system_matching(req, system, false)
}

/// As [`best_system`], but optionally also require a `php-fpm` alongside
/// the interpreter (`require_fpm`). Applying fpm as a candidate filter —
/// rather than filtering the single best match after the fact — lets a
/// lower-versioned fpm-capable PHP win over a higher-versioned CLI-only
/// one when fpm is required.
fn best_system_matching<'a>(
    req: Requirement<'_>,
    system: &'a [SystemPhp],
    require_fpm: bool,
) -> Option<&'a SystemPhp> {
    system
        .iter()
        .filter(|php| php.flavor == req.flavor)
        .filter(|php| req.spec.is_none_or(|spec| version_satisfies(&php.version, spec)))
        .filter(|php| req.required_exts.iter().all(|ext| php.has_extension(ext)))
        .filter(|php| !require_fpm || php.has_fpm)
        .max_by(|a, b| a.version.cmp(&b.version))
}

/// Build a specific error explaining why nothing qualified.
fn no_candidate_error(
    pref: PhpPreference,
    downloads: bool,
    req: Requirement<'_>,
    system: &[SystemPhp],
) -> eyre::Report {
    let spec = req
        .spec
        .map_or_else(|| "any version".to_string(), |s| format!("{s:?}"));

    match pref {
        PhpPreference::OnlySystem => {
            // Prefer naming a near-miss system PHP's missing extension —
            // that is the actionable cause under `--no-managed-php`.
            for php in system {
                if php.flavor == req.flavor
                    && req.spec.is_none_or(|s| version_satisfies(&php.version, s))
                    && let Some(missing) =
                        req.required_exts.iter().find(|ext| !php.has_extension(ext))
                {
                    return eyre!(
                        "system PHP at {} ({}-{}) is missing required extension `ext-{}`; \
                         install it (OS package / PECL), or allow a managed PHP",
                        php.path.display(),
                        php.version,
                        php.flavor,
                        missing
                    );
                }
            }
            eyre!(
                "no system PHP matching {spec} ({}) was found; \
                 install one, or allow a managed PHP",
                req.flavor
            )
        }
        PhpPreference::OnlyManaged if !downloads => eyre!(
            "no installed managed PHP matching {spec} ({}), and downloads are disabled \
             (`--no-php-downloads`)",
            req.flavor
        ),
        PhpPreference::Managed if !downloads => eyre!(
            "no installed managed or qualifying system PHP matching {spec} ({}), and downloads \
             are disabled (`--no-php-downloads`)",
            req.flavor
        ),
        // With downloads on, the managed tiers always yield `Download`,
        // so this is unreachable in practice.
        _ => eyre!("no PHP matching {spec} ({})", req.flavor),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bougie_version::version::PartialVersion;
    use std::path::PathBuf;

    fn spec(major: u32, minor: Option<u32>) -> VersionLike {
        VersionLike::Version(PartialVersion { major, minor, patch: None })
    }

    fn sys(version: Version, flavor: Flavor, exts: &[&str]) -> SystemPhp {
        sys_fpm(version, flavor, exts, true)
    }

    fn sys_fpm(version: Version, flavor: Flavor, exts: &[&str], has_fpm: bool) -> SystemPhp {
        SystemPhp {
            path: PathBuf::from(format!("/usr/bin/php-{version}")),
            version,
            flavor,
            extensions: exts.iter().map(|e| e.to_string()).collect(),
            has_fpm,
        }
    }

    fn req<'a>(s: &'a VersionLike, exts: &'a [String]) -> Requirement<'a> {
        Requirement { spec: Some(s), flavor: Flavor::Nts, required_exts: exts }
    }

    #[test]
    fn resolve_from_flags_and_config() {
        assert_eq!(PhpPreference::resolve(false, false, None).unwrap(), PhpPreference::Managed);
        assert_eq!(PhpPreference::resolve(true, false, None).unwrap(), PhpPreference::OnlyManaged);
        assert_eq!(PhpPreference::resolve(false, true, None).unwrap(), PhpPreference::OnlySystem);
        // Flags win over config.
        assert_eq!(
            PhpPreference::resolve(true, false, Some(false)).unwrap(),
            PhpPreference::OnlyManaged
        );
        // Config fallback.
        assert_eq!(PhpPreference::resolve(false, false, Some(true)).unwrap(), PhpPreference::OnlyManaged);
        assert_eq!(PhpPreference::resolve(false, false, Some(false)).unwrap(), PhpPreference::OnlySystem);
        // Conflicting flags error.
        assert!(PhpPreference::resolve(true, true, None).is_err());
    }

    #[test]
    fn default_prefers_installed_managed_over_system() {
        let s = spec(8, Some(3));
        let exts: Vec<String> = vec![];
        let installed = [(Version::new(8, 3, 12), Flavor::Nts)];
        let system = [sys(Version::new(8, 3, 20), Flavor::Nts, &[])];
        let got = select(PhpPreference::Managed, true, req(&s, &exts), &installed, &system).unwrap();
        assert_eq!(got, Selection::ManagedInstalled { version: Version::new(8, 3, 12), flavor: Flavor::Nts });
    }

    #[test]
    fn default_uses_system_before_download() {
        let s = spec(8, Some(3));
        let exts: Vec<String> = vec![];
        let system = [sys(Version::new(8, 3, 12), Flavor::Nts, &[])];
        let got = select(PhpPreference::Managed, true, req(&s, &exts), &[], &system).unwrap();
        assert_eq!(got, Selection::System(system[0].clone()));
    }

    #[test]
    fn default_downloads_when_nothing_present() {
        let s = spec(8, Some(3));
        let exts: Vec<String> = vec![];
        let got = select(PhpPreference::Managed, true, req(&s, &exts), &[], &[]).unwrap();
        assert_eq!(got, Selection::Download);
    }

    #[test]
    fn system_disqualified_by_missing_ext_falls_to_download() {
        let s = spec(8, Some(3));
        let exts = vec!["redis".to_string()];
        let system = [sys(Version::new(8, 3, 12), Flavor::Nts, &["curl"])];
        let got = select(PhpPreference::Managed, true, req(&s, &exts), &[], &system).unwrap();
        assert_eq!(got, Selection::Download);
    }

    #[test]
    fn system_qualifies_when_ext_present() {
        let s = spec(8, Some(3));
        let exts = vec!["redis".to_string()];
        let system = [sys(Version::new(8, 3, 12), Flavor::Nts, &["curl", "redis"])];
        let got = select(PhpPreference::Managed, true, req(&s, &exts), &[], &system).unwrap();
        assert_eq!(got, Selection::System(system[0].clone()));
    }

    #[test]
    fn default_skips_fpmless_system_for_download() {
        // The only qualifying system PHP is CLI-only (no fpm). With
        // downloads on, the server-needing default prefers a managed
        // download over the fpm-less system PHP.
        let s = spec(8, Some(3));
        let exts: Vec<String> = vec![];
        let system = [sys_fpm(Version::new(8, 3, 12), Flavor::Nts, &[], false)];
        let got = select(PhpPreference::Managed, true, req(&s, &exts), &[], &system).unwrap();
        assert_eq!(got, Selection::Download);
    }

    #[test]
    fn default_uses_fpmless_system_when_downloads_disabled() {
        // No download escape hatch → the fpm-less system PHP is still
        // chosen (usable for CLI) rather than failing outright.
        let s = spec(8, Some(3));
        let exts: Vec<String> = vec![];
        let system = [sys_fpm(Version::new(8, 3, 12), Flavor::Nts, &[], false)];
        let got = select(PhpPreference::Managed, false, req(&s, &exts), &[], &system).unwrap();
        assert_eq!(got, Selection::System(system[0].clone()));
    }

    #[test]
    fn default_prefers_lower_fpm_system_over_higher_cli_only() {
        // A lower-versioned fpm-capable PHP beats a higher-versioned
        // CLI-only one when fpm is the deciding factor.
        let s = spec(8, None);
        let exts: Vec<String> = vec![];
        let system = [
            sys_fpm(Version::new(8, 4, 1), Flavor::Nts, &[], false),
            sys_fpm(Version::new(8, 3, 12), Flavor::Nts, &[], true),
        ];
        let got = select(PhpPreference::Managed, true, req(&s, &exts), &[], &system).unwrap();
        assert_eq!(got, Selection::System(system[1].clone()));
    }

    #[test]
    fn only_system_uses_fpmless_php() {
        // Explicit `--no-managed-php`: fpm is not required (CLI use is
        // valid); the server errors later if fpm is actually needed.
        let s = spec(8, Some(3));
        let exts: Vec<String> = vec![];
        let system = [sys_fpm(Version::new(8, 3, 12), Flavor::Nts, &[], false)];
        let got = select(PhpPreference::OnlySystem, true, req(&s, &exts), &[], &system).unwrap();
        assert_eq!(got, Selection::System(system[0].clone()));
    }

    #[test]
    fn only_managed_never_uses_system() {
        let s = spec(8, Some(3));
        let exts: Vec<String> = vec![];
        let system = [sys(Version::new(8, 3, 12), Flavor::Nts, &[])];
        let got = select(PhpPreference::OnlyManaged, true, req(&s, &exts), &[], &system).unwrap();
        assert_eq!(got, Selection::Download);
    }

    #[test]
    fn only_system_errors_on_missing_ext_naming_it() {
        let s = spec(8, Some(3));
        let exts = vec!["redis".to_string()];
        let system = [sys(Version::new(8, 3, 12), Flavor::Nts, &["curl"])];
        let err = select(PhpPreference::OnlySystem, true, req(&s, &exts), &[], &system).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("ext-redis"), "{msg}");
    }

    #[test]
    fn no_downloads_errors_instead_of_downloading() {
        let s = spec(8, Some(3));
        let exts: Vec<String> = vec![];
        let err = select(PhpPreference::Managed, false, req(&s, &exts), &[], &[]).unwrap_err();
        assert!(err.to_string().contains("downloads are disabled"), "{err}");
    }

    #[test]
    fn flavor_mismatch_disqualifies_system() {
        let s = spec(8, Some(3));
        let exts: Vec<String> = vec![];
        // ZTS system PHP can't satisfy an NTS requirement.
        let system = [sys(Version::new(8, 3, 12), Flavor::Zts, &[])];
        let got = select(PhpPreference::Managed, true, req(&s, &exts), &[], &system).unwrap();
        assert_eq!(got, Selection::Download);
    }
}
