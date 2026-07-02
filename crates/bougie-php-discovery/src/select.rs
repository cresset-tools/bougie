//! PHP source selection — pick a managed-installed, system, or
//! to-be-downloaded managed PHP per the user's preference.
//!
//! This is uv's system-Python model adapted to PHP. The policy here is
//! **pure**: it takes already-gathered candidate sets (installed
//! managed builds + probed system PHPs), a [`PhpPreference`], and the
//! [`SelectionContext`] of the invocation, and returns a [`Selection`].
//! Gathering the candidates (scanning `installs/`, running
//! [`crate::discover`] + [`crate::probe`]) and acting on a
//! [`Selection::Download`] live one layer up, in the sync command —
//! keeping this layer trivially unit-testable.

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
    /// The default. What it means depends on the [`SelectionContext`]:
    /// a project-configuring sync prefers an already-installed managed
    /// PHP and otherwise downloads one — it never silently pins a
    /// system PHP; a one-off run additionally reaches for an adequate
    /// system PHP before downloading.
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

/// What the selected PHP is *for* — whether the choice gets pinned
/// into project state or used for a single invocation.
///
/// This only changes the default ([`PhpPreference::Managed`]) policy;
/// the explicit `OnlyManaged` / `OnlySystem` preferences behave the
/// same in both contexts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelectionContext {
    /// A project-configuring sync (`bougie sync`, `bougie server`,
    /// `bougie php pin`, …): the selection is written to
    /// `vendor/bougie/state/` and replayed by the shims, so the default
    /// preference never picks a system PHP — configuring a project
    /// against one requires the explicit `--no-managed-php` /
    /// `[php] managed = false` opt-in.
    Project,
    /// A one-off run (`bougie run`): the selection is used for this
    /// invocation only and never pinned, so an adequate system PHP may
    /// be used before falling back to a download.
    OneOff,
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
/// - `Managed` (default) in a [`SelectionContext::Project`]:
///   managed-installed → download. Never a system PHP — see
///   [`SelectionContext`].
/// - `Managed` in a [`SelectionContext::OneOff`]: managed-installed →
///   any qualifying system PHP → download.
/// - `OnlyManaged`: managed-installed → download.
/// - `OnlySystem`: system only.
pub fn select(
    pref: PhpPreference,
    downloads: bool,
    ctx: SelectionContext,
    req: Requirement<'_>,
    managed_installed: &[(Version, Flavor)],
    system: &[SystemPhp],
) -> Result<Selection> {
    let managed = || {
        best_managed(req, managed_installed)
            .map(|(version, flavor)| Selection::ManagedInstalled { version, flavor })
    };
    let sys = || best_system(req, system).cloned().map(Selection::System);

    let selection = match (pref, ctx) {
        (PhpPreference::OnlyManaged, _) | (PhpPreference::Managed, SelectionContext::Project) => {
            managed().or_else(|| downloads.then_some(Selection::Download))
        }
        (PhpPreference::Managed, SelectionContext::OneOff) => managed()
            .or_else(sys)
            .or_else(|| downloads.then_some(Selection::Download)),
        (PhpPreference::OnlySystem, _) => sys(),
    };

    selection.ok_or_else(|| no_candidate_error(pref, downloads, ctx, req, system))
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
/// every required extension. Whether the build ships `php-fpm` is not a
/// selection criterion: a system PHP is only ever selected for a
/// one-off run or under the explicit only-system opt-in — the server
/// checks for a usable fpm itself when it actually needs one.
fn best_system<'a>(req: Requirement<'_>, system: &'a [SystemPhp]) -> Option<&'a SystemPhp> {
    system
        .iter()
        .filter(|php| php.flavor == req.flavor)
        .filter(|php| req.spec.is_none_or(|spec| version_satisfies(&php.version, spec)))
        .filter(|php| req.required_exts.iter().all(|ext| php.has_extension(ext)))
        .max_by(|a, b| a.version.cmp(&b.version))
}

/// Build a specific error explaining why nothing qualified.
fn no_candidate_error(
    pref: PhpPreference,
    downloads: bool,
    ctx: SelectionContext,
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
        PhpPreference::Managed if !downloads => match ctx {
            // A project sync never falls back to a system PHP, so point
            // at the two ways out: install a managed PHP, or opt into a
            // system one explicitly.
            SelectionContext::Project => eyre!(
                "no installed managed PHP matching {spec} ({}), and downloads are disabled \
                 (`--no-php-downloads`); install one with `bougie php install`, or pass \
                 `--no-managed-php` to use a system PHP",
                req.flavor
            ),
            SelectionContext::OneOff => eyre!(
                "no installed managed or qualifying system PHP matching {spec} ({}), and \
                 downloads are disabled (`--no-php-downloads`)",
                req.flavor
            ),
        },
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
            extensions: exts.iter().map(ToString::to_string).collect(),
            has_fpm,
        }
    }

    fn req<'a>(s: &'a VersionLike, exts: &'a [String]) -> Requirement<'a> {
        Requirement { spec: Some(s), flavor: Flavor::Nts, required_exts: exts }
    }

    use SelectionContext::{OneOff, Project};

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
        for ctx in [Project, OneOff] {
            let got =
                select(PhpPreference::Managed, true, ctx, req(&s, &exts), &installed, &system)
                    .unwrap();
            assert_eq!(
                got,
                Selection::ManagedInstalled { version: Version::new(8, 3, 12), flavor: Flavor::Nts }
            );
        }
    }

    #[test]
    fn project_downloads_instead_of_using_system() {
        // A project-configuring sync under the default preference never
        // selects a system PHP, even a fully-qualifying one: it
        // downloads a managed PHP instead.
        let s = spec(8, Some(3));
        let exts: Vec<String> = vec![];
        let system = [sys(Version::new(8, 3, 12), Flavor::Nts, &[])];
        let got = select(PhpPreference::Managed, true, Project, req(&s, &exts), &[], &system).unwrap();
        assert_eq!(got, Selection::Download);
    }

    #[test]
    fn project_errors_with_guidance_instead_of_using_system() {
        // …and with downloads disabled it errors — pointing at
        // `bougie php install` and the `--no-managed-php` opt-in —
        // rather than silently pinning the system PHP.
        let s = spec(8, Some(3));
        let exts: Vec<String> = vec![];
        let system = [sys(Version::new(8, 3, 12), Flavor::Nts, &[])];
        let err =
            select(PhpPreference::Managed, false, Project, req(&s, &exts), &[], &system).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("bougie php install"), "{msg}");
        assert!(msg.contains("--no-managed-php"), "{msg}");
    }

    #[test]
    fn oneoff_uses_system_before_download() {
        let s = spec(8, Some(3));
        let exts: Vec<String> = vec![];
        let system = [sys(Version::new(8, 3, 12), Flavor::Nts, &[])];
        let got = select(PhpPreference::Managed, true, OneOff, req(&s, &exts), &[], &system).unwrap();
        assert_eq!(got, Selection::System(system[0].clone()));
    }

    #[test]
    fn default_downloads_when_nothing_present() {
        let s = spec(8, Some(3));
        let exts: Vec<String> = vec![];
        for ctx in [Project, OneOff] {
            let got = select(PhpPreference::Managed, true, ctx, req(&s, &exts), &[], &[]).unwrap();
            assert_eq!(got, Selection::Download);
        }
    }

    #[test]
    fn oneoff_system_disqualified_by_missing_ext_falls_to_download() {
        let s = spec(8, Some(3));
        let exts = vec!["redis".to_string()];
        let system = [sys(Version::new(8, 3, 12), Flavor::Nts, &["curl"])];
        let got = select(PhpPreference::Managed, true, OneOff, req(&s, &exts), &[], &system).unwrap();
        assert_eq!(got, Selection::Download);
    }

    #[test]
    fn oneoff_system_qualifies_when_ext_present() {
        let s = spec(8, Some(3));
        let exts = vec!["redis".to_string()];
        let system = [sys(Version::new(8, 3, 12), Flavor::Nts, &["curl", "redis"])];
        let got = select(PhpPreference::Managed, true, OneOff, req(&s, &exts), &[], &system).unwrap();
        assert_eq!(got, Selection::System(system[0].clone()));
    }

    #[test]
    fn oneoff_ignores_fpm_when_picking_a_system_php() {
        // A one-off run is CLI-only, so fpm is not a selection
        // criterion: the higher-versioned CLI-only build wins over a
        // lower fpm-capable one, and a fpm-less PHP beats a download.
        let s = spec(8, None);
        let exts: Vec<String> = vec![];
        let system = [
            sys_fpm(Version::new(8, 4, 1), Flavor::Nts, &[], false),
            sys_fpm(Version::new(8, 3, 12), Flavor::Nts, &[], true),
        ];
        let got = select(PhpPreference::Managed, true, OneOff, req(&s, &exts), &[], &system).unwrap();
        assert_eq!(got, Selection::System(system[0].clone()));
    }

    #[test]
    fn only_system_uses_fpmless_php() {
        // Explicit `--no-managed-php`: fpm is not required (CLI use is
        // valid); the server errors later if fpm is actually needed.
        let s = spec(8, Some(3));
        let exts: Vec<String> = vec![];
        let system = [sys_fpm(Version::new(8, 3, 12), Flavor::Nts, &[], false)];
        let got =
            select(PhpPreference::OnlySystem, true, Project, req(&s, &exts), &[], &system).unwrap();
        assert_eq!(got, Selection::System(system[0].clone()));
    }

    #[test]
    fn only_managed_never_uses_system() {
        let s = spec(8, Some(3));
        let exts: Vec<String> = vec![];
        let system = [sys(Version::new(8, 3, 12), Flavor::Nts, &[])];
        for ctx in [Project, OneOff] {
            let got =
                select(PhpPreference::OnlyManaged, true, ctx, req(&s, &exts), &[], &system).unwrap();
            assert_eq!(got, Selection::Download);
        }
    }

    #[test]
    fn only_system_errors_on_missing_ext_naming_it() {
        let s = spec(8, Some(3));
        let exts = vec!["redis".to_string()];
        let system = [sys(Version::new(8, 3, 12), Flavor::Nts, &["curl"])];
        let err =
            select(PhpPreference::OnlySystem, true, Project, req(&s, &exts), &[], &system).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("ext-redis"), "{msg}");
    }

    #[test]
    fn no_downloads_errors_instead_of_downloading() {
        let s = spec(8, Some(3));
        let exts: Vec<String> = vec![];
        for ctx in [Project, OneOff] {
            let err = select(PhpPreference::Managed, false, ctx, req(&s, &exts), &[], &[]).unwrap_err();
            assert!(err.to_string().contains("downloads are disabled"), "{err}");
        }
    }

    #[test]
    fn oneoff_flavor_mismatch_disqualifies_system() {
        let s = spec(8, Some(3));
        let exts: Vec<String> = vec![];
        // ZTS system PHP can't satisfy an NTS requirement.
        let system = [sys(Version::new(8, 3, 12), Flavor::Zts, &[])];
        let got = select(PhpPreference::Managed, true, OneOff, req(&s, &exts), &[], &system).unwrap();
        assert_eq!(got, Selection::Download);
    }
}
