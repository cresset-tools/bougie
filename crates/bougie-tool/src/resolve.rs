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
use bougie_version::matches::version_satisfies;
use bougie_version::request::{Request, parse_request};
use bougie_version::version::Version;
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

/// What a tool package's own metadata (`require`) asks of the
/// platform. Fetched once per install/run and consumed twice: `php`
/// drives interpreter selection, `extensions` joins the effective
/// extension set alongside `--with` and any project-derived names.
#[derive(Debug, Clone, Default)]
pub struct ToolRequires {
    /// `require.php` verbatim (a composer-style constraint like
    /// `^7.2 || ^8.0`), or `None` when the tool doesn't pin PHP.
    pub php: Option<String>,
    /// `require.ext-*` short names (`ext-` stripped, lowercased).
    /// The callback pre-filters names that are always satisfied
    /// (builtins, baseline) so everything left is a real install
    /// candidate.
    pub extensions: Vec<String>,
}

/// Callback that fetches a tool package's platform requirements from
/// Packagist (cached). Hosted by the bougie binary so this crate
/// stays free of reqwest + the resolver's metadata machinery.
///
/// Arguments: `paths`, `package` (`vendor/name`), `user_constraint`
/// (the `@<constraint>` segment of the install request, or `*` when
/// unspecified). The fetcher picks the highest stable version
/// matching `user_constraint` and returns that version's
/// requirements. An unknown package yields the empty default; `Err`
/// is reserved for network / parse failures (callers warn and fall
/// back).
pub type ToolRequiresFetcher =
    dyn Fn(&Paths, &str, &str) -> Result<ToolRequires> + Send + Sync;

/// PHP-selection inputs derived from the project the user is standing
/// in. Assembled by the bougie binary (this crate stays
/// project-blind); consumed by [`select_php`].
#[derive(Debug, Clone, Default)]
pub struct ProjectPhp {
    /// Exact `(version, flavor)` a synced bougie project resolved to
    /// (`vendor/bougie/state/resolved`). Strongest signal: when it
    /// satisfies the tool's `require.php`, the tool runs the very
    /// interpreter the project runs.
    pub resolved: Option<(String, String)>,
    /// The project's PHP constraint (composer.json `require.php`,
    /// `bougie.toml [php]version`, or inferred), pre-parsed for
    /// matching.
    pub constraint: Option<composer_semver::Constraint>,
    /// Raw string form of `constraint` for the installer fallback
    /// (install specs travel as strings). `None` when the constraint
    /// was synthesized without a single written form (e.g. a
    /// lockfile-wide intersection).
    pub constraint_raw: Option<String>,
    /// Human-readable origin for notices ("./composer.json",
    /// "magento/product-community-edition 2.4.7", …).
    pub source: String,
}

/// Everything the project contributes to a tool run: PHP-selection
/// inputs plus the derived extension set (already filtered of
/// builtins/baseline by the binary side).
#[derive(Debug, Clone, Default)]
pub struct ProjectContext {
    pub php: ProjectPhp,
    pub extensions: Vec<String>,
}

/// Which lane [`select_php`] settled on — drives the one-line notice
/// the run path prints.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PhpSource {
    /// `--php` explicit spec.
    Spec,
    /// A synced bougie project's exact resolved interpreter.
    ProjectResolved,
    /// Highest version satisfying both the project constraint and
    /// the tool's `require.php`.
    ProjectIntersection,
    /// The tool's own `require.php` (no project signal, or the
    /// project turned out incompatible).
    ToolRequire,
    /// Highest installed NTS — no signal at all.
    DefaultHighest,
}

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
    let parsed = composer_semver::Constraint::parse(php_constraint)
        .map_err(|e| eyre::eyre!("parsing tool's require.php `{php_constraint}`: {e}"))?;
    if let Some(choice) = best_installed_nts(paths, &[&parsed])? {
        return Ok(choice);
    }
    installer(paths, php_constraint).wrap_err_with(|| {
        format!("auto-installing PHP for tool require.php `{php_constraint}`")
    })
}

/// Highest installed NTS PHP matching *every* constraint in `preds`,
/// or `None` when nothing on disk qualifies.
fn best_installed_nts(
    paths: &Paths,
    preds: &[&composer_semver::Constraint],
) -> Result<Option<PhpChoice>> {
    let on_disk = store::list_installed(paths)
        .map_err(|e| eyre::eyre!("listing installed PHPs: {e}"))?;
    let best = on_disk
        .into_iter()
        .filter(|(_, flavor)| flavor == "nts")
        .filter_map(|(v, f)| v.parse::<Version>().ok().map(|p| (p, f)))
        .filter(|(v, _)| {
            composer_semver::Version::parse(&v.to_string())
                .is_ok_and(|lifted| preds.iter().all(|p| p.matches(&lifted)))
        })
        .max_by(|a, b| a.0.cmp(&b.0));
    Ok(best.map(|(version, flavor)| {
        let version_str = version.to_string();
        let bin = paths
            .installs()
            .join(format!("{version_str}-{flavor}"))
            .join("bin")
            .join("php");
        PhpChoice {
            version: version_str,
            flavor,
            bin,
        }
    }))
}

/// True when the tool's constraint (if any) admits `version`.
/// Unparseable versions count as a mismatch — better to fall to a
/// lane that re-resolves than to exec a PHP the tool may reject.
fn tool_accepts(tool: Option<&composer_semver::Constraint>, version: &str) -> bool {
    match tool {
        None => true,
        Some(c) => composer_semver::Version::parse(version).is_ok_and(|v| c.matches(&v)),
    }
}

/// Full PHP-selection ladder for tool installs and runs:
///
/// 1. `--php <spec>` — explicit, wins unconditionally.
/// 2. A synced bougie project's exact resolved interpreter, when it
///    satisfies the tool's `require.php` and is still on disk.
/// 3. The project constraint ∩ the tool constraint: highest installed
///    NTS matching both, else auto-install by the project's written
///    constraint and keep the result only if the tool accepts it.
/// 4. The tool's `require.php` alone (today's behaviour) — with a
///    warning when a project constraint had to be abandoned, since
///    the tool may then be unable to boot the project's code.
/// 5. Highest installed NTS.
///
/// The project side is best-effort by design: the tool must at
/// minimum be able to execute itself, so on any project/tool
/// incompatibility the tool's own requirement is authoritative and
/// the project's `platform_check.php` gets to report the mismatch in
/// its own words at runtime.
pub fn select_php(
    paths: &Paths,
    package: &str,
    php_spec: Option<&str>,
    tool_php: Option<&str>,
    project: Option<&ProjectPhp>,
    installer: &PhpInstaller,
) -> Result<(PhpChoice, PhpSource)> {
    if let Some(spec) = php_spec {
        return Ok((pick_php(paths, Some(spec), installer)?, PhpSource::Spec));
    }

    let tool_parsed = tool_php
        .map(|raw| {
            composer_semver::Constraint::parse(raw)
                .map_err(|e| eyre::eyre!("parsing tool's require.php `{raw}`: {e}"))
        })
        .transpose()?;

    let mut project_abandoned: Option<&str> = None;
    if let Some(p) = project {
        // Lane 2: exact resolved interpreter of a synced project.
        if let Some((version, flavor)) = &p.resolved
            && tool_accepts(tool_parsed.as_ref(), version)
        {
            let bin = paths
                .installs()
                .join(format!("{version}-{flavor}"))
                .join("bin")
                .join("php");
            if bin.is_file() {
                return Ok((
                    PhpChoice {
                        version: version.clone(),
                        flavor: flavor.clone(),
                        bin,
                    },
                    PhpSource::ProjectResolved,
                ));
            }
            // Stale marker (install pruned) — fall through to the
            // constraint lanes rather than exec a missing binary.
        }

        // Lane 3: intersection of project and tool constraints.
        if let Some(pc) = &p.constraint {
            let mut preds: Vec<&composer_semver::Constraint> = vec![pc];
            if let Some(tc) = &tool_parsed {
                preds.push(tc);
            }
            if let Some(choice) = best_installed_nts(paths, &preds)? {
                return Ok((choice, PhpSource::ProjectIntersection));
            }
            if let Some(raw) = &p.constraint_raw {
                match installer(paths, raw) {
                    Ok(choice) if tool_accepts(tool_parsed.as_ref(), &choice.version) => {
                        return Ok((choice, PhpSource::ProjectIntersection));
                    }
                    Ok(_) | Err(_) => {
                        // Either the freshly resolved project PHP
                        // violates the tool's constraint, or nothing
                        // resolvable satisfies the project. Tool wins.
                        project_abandoned = Some(p.source.as_str());
                    }
                }
            } else {
                project_abandoned = Some(p.source.as_str());
            }
        }
    }

    if let Some(source) = project_abandoned {
        eprintln!(
            "warning: couldn't select a PHP satisfying both the project ({source}) and \
             `{package}`'s require.php ({tool}); using the tool's requirement — it may not \
             be able to boot this project. Pass `--php <ver>` to pin, or `--no-project` \
             to ignore the project.",
            tool = tool_php.unwrap_or("*"),
        );
    }

    // Lanes 4 + 5: tool's own requirement, then the bare default.
    match tool_php {
        Some(tc) => Ok((
            pick_php_for_constraint(paths, tc, installer)?,
            PhpSource::ToolRequire,
        )),
        None => Ok((pick_php(paths, None, installer)?, PhpSource::DefaultHighest)),
    }
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
        .filter(|(v, _)| version_filter.is_none_or(|spec| version_satisfies(v, spec)))
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

    fn install_fake_with_bin(paths: &Paths, version: &str, flavor: &str) {
        let bin = paths
            .installs()
            .join(format!("{version}-{flavor}"))
            .join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::write(bin.join("php"), "").unwrap();
    }

    fn project_php(
        resolved: Option<(&str, &str)>,
        constraint: Option<&str>,
    ) -> ProjectPhp {
        ProjectPhp {
            resolved: resolved.map(|(v, f)| (v.to_string(), f.to_string())),
            constraint: constraint
                .map(|c| composer_semver::Constraint::parse(c).unwrap()),
            constraint_raw: constraint.map(str::to_string),
            source: "./composer.json".into(),
        }
    }

    #[test]
    fn select_spec_wins_over_project_and_tool() {
        let td = tempfile::TempDir::new().unwrap();
        let p = paths(td.path());
        install_fake(&p, "8.3.12", "nts");
        install_fake(&p, "8.4.0", "nts");
        let inst = fail_installer();
        let proj = project_php(Some(("8.4.0", "nts")), Some("~8.4.0"));
        let (choice, source) = select_php(
            &p, "v/p", Some("8.3"), Some(">=8.0"), Some(&proj), inst.as_ref(),
        )
        .unwrap();
        assert_eq!(choice.version, "8.3.12");
        assert_eq!(source, PhpSource::Spec);
    }

    #[test]
    fn select_uses_project_resolved_when_tool_accepts() {
        let td = tempfile::TempDir::new().unwrap();
        let p = paths(td.path());
        install_fake_with_bin(&p, "8.4.2", "nts");
        install_fake_with_bin(&p, "8.5.0", "nts");
        let inst = fail_installer();
        let proj = project_php(Some(("8.4.2", "nts")), Some("~8.4.0"));
        let (choice, source) = select_php(
            &p, "v/p", None, Some(">=8.0.0"), Some(&proj), inst.as_ref(),
        )
        .unwrap();
        // Exact project interpreter, not the higher installed 8.5.
        assert_eq!(choice.version, "8.4.2");
        assert_eq!(source, PhpSource::ProjectResolved);
    }

    #[test]
    fn select_project_resolved_zts_flavor_is_honoured() {
        let td = tempfile::TempDir::new().unwrap();
        let p = paths(td.path());
        install_fake_with_bin(&p, "8.4.2", "zts");
        let inst = fail_installer();
        let proj = project_php(Some(("8.4.2", "zts")), None);
        let (choice, source) =
            select_php(&p, "v/p", None, None, Some(&proj), inst.as_ref()).unwrap();
        assert_eq!((choice.version.as_str(), choice.flavor.as_str()), ("8.4.2", "zts"));
        assert_eq!(source, PhpSource::ProjectResolved);
    }

    #[test]
    fn select_intersects_project_constraint_with_tool() {
        let td = tempfile::TempDir::new().unwrap();
        let p = paths(td.path());
        install_fake(&p, "8.3.12", "nts");
        install_fake(&p, "8.4.0", "nts");
        install_fake(&p, "8.5.1", "nts");
        let inst = fail_installer();
        // Project allows 8.3/8.4; tool wants >=8.4 — intersection is 8.4.
        let proj = project_php(None, Some("~8.3.0 || ~8.4.0"));
        let (choice, source) = select_php(
            &p, "v/p", None, Some(">=8.4"), Some(&proj), inst.as_ref(),
        )
        .unwrap();
        assert_eq!(choice.version, "8.4.0");
        assert_eq!(source, PhpSource::ProjectIntersection);
    }

    #[test]
    fn select_skips_incompatible_resolved_but_keeps_constraint_lane() {
        let td = tempfile::TempDir::new().unwrap();
        let p = paths(td.path());
        install_fake_with_bin(&p, "8.1.30", "nts");
        install_fake(&p, "8.3.12", "nts");
        let inst = fail_installer();
        // Resolved 8.1 violates the tool's ^8.2; the broader project
        // constraint still intersects at 8.3.
        let proj = project_php(Some(("8.1.30", "nts")), Some(">=8.1"));
        let (choice, source) = select_php(
            &p, "v/p", None, Some("^8.2"), Some(&proj), inst.as_ref(),
        )
        .unwrap();
        assert_eq!(choice.version, "8.3.12");
        assert_eq!(source, PhpSource::ProjectIntersection);
    }

    #[test]
    fn select_empty_intersection_defaults_to_tool() {
        let td = tempfile::TempDir::new().unwrap();
        let p = paths(td.path());
        install_fake(&p, "8.1.30", "nts");
        install_fake(&p, "8.4.0", "nts");
        // Project pinned to the 8.1 series, tool needs >=8.3: the
        // intersection is empty. The installer is consulted for the
        // project constraint ("~8.1.0") and hands back an 8.1 — which
        // the tool rejects — so selection falls to the tool lane.
        let inst = dummy_installer("8.1.32", "nts");
        let proj = project_php(None, Some("~8.1.0"));
        let (choice, source) = select_php(
            &p, "v/p", None, Some(">=8.3"), Some(&proj), inst.as_ref(),
        )
        .unwrap();
        assert_eq!(choice.version, "8.4.0");
        assert_eq!(source, PhpSource::ToolRequire);
    }

    #[test]
    fn select_installer_error_on_project_constraint_falls_to_tool() {
        let td = tempfile::TempDir::new().unwrap();
        let p = paths(td.path());
        install_fake(&p, "8.4.0", "nts");
        // Nothing installed matches the project's 7.4 pin and the
        // installer can't provide one either → tool lane.
        let inst = fail_installer();
        let proj = project_php(None, Some("~7.4.0"));
        let (choice, source) = select_php(
            &p, "v/p", None, Some(">=8.0"), Some(&proj), inst.as_ref(),
        )
        .unwrap();
        assert_eq!(choice.version, "8.4.0");
        assert_eq!(source, PhpSource::ToolRequire);
    }

    #[test]
    fn select_without_project_or_tool_is_default_highest() {
        let td = tempfile::TempDir::new().unwrap();
        let p = paths(td.path());
        install_fake(&p, "8.3.12", "nts");
        install_fake(&p, "8.4.0", "nts");
        let inst = fail_installer();
        let (choice, source) =
            select_php(&p, "v/p", None, None, None, inst.as_ref()).unwrap();
        assert_eq!(choice.version, "8.4.0");
        assert_eq!(source, PhpSource::DefaultHighest);
    }

    #[test]
    fn select_stale_resolved_marker_falls_to_constraint_lane() {
        let td = tempfile::TempDir::new().unwrap();
        let p = paths(td.path());
        // Marker points at 8.4.1 but only 8.4.3 is actually installed
        // (e.g. `bougie php upgrade` pruned the old patch release).
        install_fake(&p, "8.4.3", "nts");
        let inst = fail_installer();
        let proj = project_php(Some(("8.4.1", "nts")), Some("~8.4.0"));
        let (choice, source) = select_php(
            &p, "v/p", None, Some(">=8.0"), Some(&proj), inst.as_ref(),
        )
        .unwrap();
        assert_eq!(choice.version, "8.4.3");
        assert_eq!(source, PhpSource::ProjectIntersection);
    }
}
