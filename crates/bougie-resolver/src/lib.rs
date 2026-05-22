//! Resolve a [`Request`] (or extension pin) against a loaded [`Section`].
//!
//! Selection rules per CLI.md §3.3 step 2 / 4:
//! - Filter yanked unless explicitly allowed.
//! - Filter by flavor (PHP) or by PHP minor + flavor (extension).
//! - Filter by version constraint or exact pin.
//! - Pick the highest non-yanked version.

use bougie_errors::BougieError;
use bougie_index::wire::{Artifact, Section};
use bougie_semver::Constraint;
use bougie_version::request::{Flavor, VersionLike};
use bougie_version::version::{PartialVersion, Version};
use eyre::Result;

/// Lift bougie's exact-triple Version into a Composer-flavor
/// `bougie_semver::Version` so semver constraints can be matched
/// against it. The triple `8.3.12` becomes the normalized `8.3.12.0`
/// (Stable). Round-trips through Composer's parse since the canonical
/// shape carries 4 segments + a stability suffix.
fn lift(v: Version) -> bougie_semver::Version {
    bougie_semver::Version::parse(&v.to_string())
        .expect("triple version is always semver-parseable")
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ResolveOptions {
    pub allow_yanked: bool,
}

#[derive(Debug, Clone)]
pub struct Selected<'a> {
    pub artifact: &'a Artifact,
    pub version: Version,
    pub frozen_warning: bool,
}

pub fn resolve_php<'a>(
    section: &'a Section,
    spec: &VersionLike,
    flavor: Flavor,
    opts: ResolveOptions,
) -> Result<Selected<'a>> {
    let candidates = section.artifacts.iter().filter(|a| {
        if a.yanked && !opts.allow_yanked {
            return false;
        }
        if a.flavor != flavor.as_str() {
            return false;
        }
        match parse_artifact_version(&a.version) {
            Ok(v) => version_matches_spec(v, spec),
            Err(_) => false,
        }
    });
    pick_highest(candidates, "php interpreter", &request_diag(spec, flavor))
}

pub fn resolve_extension<'a>(
    section: &'a Section,
    php_minor: PartialVersion,
    flavor: Flavor,
    version_pin: Option<&str>,
    opts: ResolveOptions,
) -> Result<Selected<'a>> {
    let php_minor_string = format!("{}.{}", php_minor.major, php_minor.minor.unwrap_or(0));
    let candidates = section.artifacts.iter().filter(|a| {
        if a.yanked && !opts.allow_yanked {
            return false;
        }
        if a.flavor != flavor.as_str() {
            return false;
        }
        // Section rows for extensions carry php_minor explicitly
        // (DISTRIBUTION.md §Section-index); the full ABI lives in the
        // manifest. Skip rows that omit it as a publisher-side bug.
        if a.php_minor.as_deref() != Some(&php_minor_string) {
            return false;
        }
        if let Some(pin) = version_pin
            && a.version != pin
        {
            return false;
        }
        true
    });
    let label = format!(
        "{} {} (php={php_minor_string} flavor={flavor})",
        section.name,
        version_pin.unwrap_or("latest"),
    );
    pick_highest(candidates, "extension", &label)
}

fn pick_highest<'a, I>(candidates: I, kind: &str, label: &str) -> Result<Selected<'a>>
where
    I: Iterator<Item = &'a Artifact>,
{
    let mut best: Option<(&Artifact, Version)> = None;
    for a in candidates {
        let Ok(v) = parse_artifact_version(&a.version) else {
            continue;
        };
        match best {
            None => best = Some((a, v)),
            Some((_, prev)) if v > prev => best = Some((a, v)),
            _ => {}
        }
    }
    let (artifact, version) = best.ok_or_else(|| BougieError::Resolution {
        kind: kind.to_owned(),
        detail: format!("no candidate satisfies {label}"),
    })?;
    Ok(Selected { artifact, version, frozen_warning: artifact.frozen })
}

fn parse_artifact_version(s: &str) -> Result<Version> {
    s.parse::<Version>()
}

fn version_matches_spec(v: Version, spec: &VersionLike) -> bool {
    match spec {
        VersionLike::Version(pv) => version_matches_partial(v, *pv),
        VersionLike::Constraint(c) => c.matches(&lift(v)),
    }
}

fn version_matches_partial(v: Version, pv: PartialVersion) -> bool {
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

fn request_diag(spec: &VersionLike, flavor: Flavor) -> String {
    match spec {
        VersionLike::Version(pv) => format!("{pv} ({flavor})"),
        VersionLike::Constraint(_) => format!("constraint ({flavor})"),
    }
}

/// If both `composer.json require.php` and a bougie pin are set, the
/// override must satisfy the public constraint. Returns the effective
/// resolved spec (the override if present, else the constraint), or
/// errors with `BougieError::Resolution` on conflict.
pub fn intersect_php(
    public: Option<&Constraint>,
    override_spec: Option<&VersionLike>,
) -> Result<VersionLike> {
    match (public, override_spec) {
        (None, None) => Err(BougieError::Resolution {
            kind: "php".into(),
            detail:
                "no PHP version constraint set — add `require.php` to composer.json or `[php]version` to bougie.toml"
                    .into(),
        }
        .into()),
        (Some(c), None) => Ok(VersionLike::Constraint(c.clone())),
        (None, Some(o)) => Ok(o.clone()),
        (Some(c), Some(o)) => {
            // The override must satisfy the public constraint.
            let probe = match o {
                VersionLike::Version(pv) => pv.pad(),
                VersionLike::Constraint(_) => return Ok(o.clone()),
            };
            if c.matches(&lift(probe)) {
                Ok(o.clone())
            } else {
                Err(BougieError::Resolution {
                    kind: "php".into(),
                    detail: format!(
                        "bougie pin {probe} does not satisfy composer.json's require.php constraint — change one of them to bring them in line"
                    ),
                }
                .into())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bougie_index::wire::{ManifestRef, SectionKind};

    fn art(version: &str, flavor: &str, php: &str, yanked: bool, frozen: bool) -> Artifact {
        Artifact {
            tag: format!("test-{version}"),
            version: version.into(),
            flavor: flavor.into(),
            php_minor: Some(php.into()),
            manifest: ManifestRef {
                path: format!("/targets/x/manifests/test/{version}.json"),
                sha256: "0".repeat(64),
            },
            yanked,
            yanked_reason: None,
            frozen,
        }
    }

    fn section(name: &str, kind: SectionKind, artifacts: Vec<Artifact>) -> Section {
        Section {
            schema: 1,
            name: name.into(),
            kind,
            target: "x86_64-unknown-linux-gnu".into(),
            artifacts,
        }
    }

    #[test]
    fn picks_highest_satisfying_php() {
        let s = section(
            "interpreter/php",
            SectionKind::Interpreter,
            vec![
                art("8.3.10", "nts", "8.3", false, false),
                art("8.3.12", "nts", "8.3", false, false),
                art("8.4.0", "nts", "8.4", false, false),
            ],
        );
        let spec = VersionLike::Constraint(Constraint::parse("^8.3").unwrap());
        let sel = resolve_php(&s, &spec, Flavor::Nts, ResolveOptions::default()).unwrap();
        assert_eq!(sel.version, Version::new(8, 4, 0));
    }

    #[test]
    fn skips_yanked_by_default() {
        let s = section(
            "interpreter/php",
            SectionKind::Interpreter,
            vec![
                art("8.3.12", "nts", "8.3", false, false),
                art("8.3.13", "nts", "8.3", true, false),
            ],
        );
        let spec = VersionLike::Constraint(Constraint::parse("^8.3").unwrap());
        let sel = resolve_php(&s, &spec, Flavor::Nts, ResolveOptions::default()).unwrap();
        assert_eq!(sel.version, Version::new(8, 3, 12));
    }

    #[test]
    fn allow_yanked_includes_yanked() {
        let s = section(
            "interpreter/php",
            SectionKind::Interpreter,
            vec![art("8.3.13", "nts", "8.3", true, false)],
        );
        let spec = VersionLike::Constraint(Constraint::parse("^8.3").unwrap());
        let sel = resolve_php(
            &s,
            &spec,
            Flavor::Nts,
            ResolveOptions { allow_yanked: true },
        )
        .unwrap();
        assert_eq!(sel.version, Version::new(8, 3, 13));
    }

    #[test]
    fn flavor_mismatch_excludes() {
        let s = section(
            "interpreter/php",
            SectionKind::Interpreter,
            vec![art("8.3.12", "zts", "8.3", false, false)],
        );
        let spec = VersionLike::Constraint(Constraint::parse("^8.3").unwrap());
        let err =
            resolve_php(&s, &spec, Flavor::Nts, ResolveOptions::default()).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("php interpreter"), "msg: {msg}");
        assert!(msg.contains("no candidate"), "msg: {msg}");
    }

    #[test]
    fn frozen_artifact_surfaces_warning() {
        let s = section(
            "interpreter/php",
            SectionKind::Interpreter,
            vec![art("8.3.12", "nts", "8.3", false, true)],
        );
        let spec = VersionLike::Constraint(Constraint::parse("^8.3").unwrap());
        let sel = resolve_php(&s, &spec, Flavor::Nts, ResolveOptions::default()).unwrap();
        assert!(sel.frozen_warning);
    }

    #[test]
    fn extension_filters_by_php_minor() {
        let s = section(
            "extension/xdebug",
            SectionKind::Extension,
            vec![
                art("3.5.1", "nts", "8.3", false, false),
                art("3.5.1", "nts", "8.4", false, false),
            ],
        );
        let pv = PartialVersion { major: 8, minor: Some(3), patch: None };
        let sel = resolve_extension(&s, pv, Flavor::Nts, None, ResolveOptions::default()).unwrap();
        assert_eq!(sel.artifact.php_minor.as_deref(), Some("8.3"));
    }

    #[test]
    fn extension_pin_must_match() {
        let s = section(
            "extension/xdebug",
            SectionKind::Extension,
            vec![
                art("3.5.0", "nts", "8.3", false, false),
                art("3.5.1", "nts", "8.3", false, false),
            ],
        );
        let pv = PartialVersion { major: 8, minor: Some(3), patch: None };
        let sel = resolve_extension(&s, pv, Flavor::Nts, Some("3.5.0"), ResolveOptions::default())
            .unwrap();
        assert_eq!(sel.artifact.version, "3.5.0");
    }

    #[test]
    fn intersect_override_must_satisfy_public() {
        let public = Constraint::parse("^8.3").unwrap();
        let bad = VersionLike::Version(PartialVersion { major: 7, minor: Some(4), patch: None });
        assert!(intersect_php(Some(&public), Some(&bad)).is_err());

        let good = VersionLike::Version(PartialVersion { major: 8, minor: Some(3), patch: Some(12) });
        let resolved = intersect_php(Some(&public), Some(&good)).unwrap();
        assert!(matches!(resolved, VersionLike::Version(_)));
    }
}
