//! `bougie composer require` / `bougie composer remove` — native,
//! Composer-compatible dependency mutation.
//!
//! The flow mirrors Composer exactly: edit `composer.json`'s
//! `require` / `require-dev`, re-resolve `composer.lock`, then install
//! into `vendor/`. `--no-update` stops after the JSON edit; `--no-install`
//! stops after writing the lock.
//!
//! Two parsing layers are kept strictly separate (see
//! `COMPOSER_COMPAT_PLAN.md`):
//!
//! 1. **Name↔version supply syntax** ([`parse_name_version_pairs`]) — a
//!    port of Composer's `VersionParser::parseNameVersionPairs`. The
//!    separators are `:`, `=`, or a space; `@` is *not* a separator
//!    (`vendor/pkg@^1.0` is invalid in Composer, and we reproduce that).
//! 2. **Constraint grammar** — never reparsed here; the constraint
//!    string is stored verbatim in `composer.json` and validated by
//!    `bougie_semver::constraint::Constraint::parse`, which already ports
//!    Composer's full `parseConstraints` grammar.
//!
//! When no constraint is supplied, the [`DefaultConstraint`] policy
//! decides what to write: `composer require` uses [`DefaultConstraint::Caret`]
//! (resolve latest stable → `^X.Y`, byte-for-byte Composer); the future
//! top-level `bougie add` will use [`DefaultConstraint::LowerBound`]
//! (`>=X.Y`, uv-style). Both share this module's derivation.

use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use bougie_cli::OutputFormat;
use bougie_composer::lockfile::{apply_require_change, Lock, RequireChange};
use bougie_composer_resolver::latest_versions;
use bougie_composer_resolver::verify::is_platform;
use bougie_composer_resolver::{install_from_lock, InstallOptions, PartialUpdate};
use bougie_output::output::{emit, Render};
use bougie_paths::Paths;
use bougie_semver::stability::Stability;
use bougie_semver::version::Version;
use eyre::{eyre, Context, Result};
use serde::Serialize;

/// Default constraint to write when the user supplies a bare package
/// name (no version). The two front-ends differ only in this policy
/// (and in the supply-syntax separator they accept).
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum DefaultConstraint {
    /// `^X.Y` — Composer's `require` rule (resolve latest stable, drop
    /// the patch segment, prepend `^`).
    Caret,
    /// `>=X.Y` — uv's `add` rule. For the future top-level `bougie add`.
    LowerBound,
}

/// A parsed name↔version pair from the CLI. `version` is `None` when the
/// user gave only a name, in which case [`DefaultConstraint`] applies.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct NameVersion {
    pub name: String,
    pub version: Option<String>,
}

/// Port of Composer's `Composer\Package\Version\VersionParser::parseNameVersionPairs`.
///
/// Separators are `:`, `=`, or a space. Each argument's *first*
/// separator splits name from constraint; everything after it (spaces,
/// commas, operators) is the constraint verbatim. A bare name can take
/// the *next* argument as its constraint, but only when that argument
/// can't itself be a package name or a glob:
/// - it contains no `/` (so `vendor/a vendor/b` stays two packages),
/// - it isn't a platform package (`php`, `ext-*`, `lib-*`, …),
/// - it isn't an adjacent-`*` glob (`1.*`, `*beta`).
#[must_use]
pub fn parse_name_version_pairs(args: &[String]) -> Vec<NameVersion> {
    let mut out = Vec::with_capacity(args.len());
    let mut i = 0;
    while i < args.len() {
        let mut pair = split_first_separator(args[i].trim());
        // Two-argv form: a bare name consumes the next argument as its
        // constraint when that argument can't be a name/glob itself.
        if !pair.contains(' ')
            && let Some(next) = args.get(i + 1)
            && !next.contains('/')
            && !is_adjacent_glob(next)
            && !is_platform(next)
        {
            pair = format!("{pair} {next}");
            i += 1;
        }
        if let Some((name, version)) = pair.split_once(' ') {
            out.push(NameVersion {
                name: name.to_string(),
                version: Some(version.to_string()),
            });
        } else {
            out.push(NameVersion {
                name: pair,
                version: None,
            });
        }
        i += 1;
    }
    out
}

/// Replace the first `:`/`=`/space in `s` with a space, mirroring
/// Composer's `^([^=: ]+)[=: ](.*)$` → `$1 $2`. With no separator (or a
/// separator at position 0, which the regex's `[^=: ]+` would reject)
/// the string is returned unchanged.
fn split_first_separator(s: &str) -> String {
    match s.find([':', '=', ' ']) {
        Some(0) | None => s.to_string(),
        Some(idx) => {
            let (name, rest) = s.split_at(idx);
            // Drop the single separator char and re-join with a space.
            format!("{name} {}", &rest[1..])
        }
    }
}

/// Composer's guard regex `(?<=[a-z0-9_/-])\*|\*(?=[a-z0-9_/-])`: a `*`
/// adjacent (before or after) to a word character — i.e. a version
/// glob like `1.*` or `*beta`, not a standalone `*` (which is a valid
/// bare-name-style "any version" two-argv constraint).
fn is_adjacent_glob(s: &str) -> bool {
    let b = s.as_bytes();
    let is_word = |c: u8| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'/' | b'-');
    b.iter().enumerate().any(|(i, &c)| {
        c == b'*'
            && ((i > 0 && is_word(b[i - 1]))
                || (i + 1 < b.len() && is_word(b[i + 1])))
    })
}

/// Derive the constraint to write for a package resolved to `version`
/// (the selected pretty version string, e.g. `3.5.2`) under `policy`.
///
/// `Caret` ports Composer's `VersionSelector::transformVersion`:
/// normalize to four segments and, when the build segment is a stable
/// `0`, drop the patch (and build) — `3.5.2` → `^3.5`, `0.3.2` →
/// `^0.3.2`, `1.0.0` → `^1.0`. Non-semver inputs fall back to the
/// pretty version verbatim.
///
/// `LowerBound` produces `>=` + the same truncated `major.minor`.
pub fn derive_constraint(version: &str, policy: DefaultConstraint) -> String {
    let prefix = match policy {
        DefaultConstraint::Caret => "^",
        DefaultConstraint::LowerBound => ">=",
    };
    format!("{prefix}{}", transform_version(version))
}

/// Composer's `transformVersion`: collapse a four-segment normalized
/// version to the upgrade-through-minor form. Returns the *body* of the
/// constraint (no operator). Falls back to the input pretty string when
/// it doesn't normalize to a stable four-segment numeric version.
fn transform_version(pretty: &str) -> String {
    let Ok(parsed) = Version::parse(pretty) else {
        return pretty.to_string();
    };
    // `normalized` is Composer's 4-segment form for numeric versions
    // (`3.5.2.0`); branch/dev versions don't match and fall back.
    let parts: Vec<&str> = parsed.normalized.split('.').collect();
    // The build segment must be a stable `0` (Composer: `^0\D?` on the
    // 4th part) — anything else (a stability suffix like `3.5.2.0-RC1`,
    // which `split('.')` leaves on the last element) falls back.
    if parts.len() == 4 && (parts[3] == "0") {
        if parts[0] == "0" {
            // 0.x.y stays 0.x.y (only the build segment dropped).
            format!("{}.{}.{}", parts[0], parts[1], parts[2])
        } else {
            // x.y.z → x.y (drop patch + build).
            format!("{}.{}", parts[0], parts[1])
        }
    } else {
        pretty.to_string()
    }
}

#[derive(Debug, Serialize)]
#[allow(clippy::struct_excessive_bools, reason = "mirrors Composer's independent require/remove flags")]
pub struct RequireResult {
    pub schema_version: u32,
    pub action: &'static str,
    pub project_root: PathBuf,
    pub dev: bool,
    pub dry_run: bool,
    pub no_update: bool,
    pub no_install: bool,
    pub packages: Vec<RequireItem>,
    /// Set when a re-resolve wrote the lock.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lock_path: Option<PathBuf>,
}

#[derive(Debug, Serialize)]
pub struct RequireItem {
    pub name: String,
    /// The constraint written to composer.json (for `require`), or the
    /// removed key (for `remove`, where it's `None`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub constraint: Option<String>,
}

impl Render for RequireResult {
    fn render_text(&self, w: &mut dyn Write) -> io::Result<()> {
        let section = if self.dev { " (dev)" } else { "" };
        for it in &self.packages {
            match (self.action, &it.constraint) {
                ("require", Some(c)) => writeln!(w, "require {} {c}{section}", it.name)?,
                ("remove", _) => writeln!(w, "remove {}{section}", it.name)?,
                _ => writeln!(w, "{} {}{section}", self.action, it.name)?,
            }
        }
        if self.dry_run {
            writeln!(w, "\n(dry run — composer.json, composer.lock, and vendor/ unchanged)")?;
        } else if self.no_update {
            writeln!(w, "\ncomposer.json updated (--no-update: lock and vendor/ untouched)")?;
        } else if let Some(p) = &self.lock_path {
            let tail = if self.no_install { " (--no-install: vendor/ untouched)" } else { "" };
            writeln!(w, "\nwrote {}{tail}", p.display())?;
        }
        Ok(())
    }
}

/// `bougie composer require`.
#[allow(
    clippy::too_many_arguments,
    clippy::fn_params_excessive_bools,
    clippy::needless_pass_by_value,
    reason = "wired from clap-parsed CLI; ownership crosses the function boundary"
)]
pub fn require(
    format: OutputFormat,
    packages: Vec<String>,
    dev: bool,
    no_update: bool,
    no_install: bool,
    with_dependencies: bool,
    with_all_dependencies: bool,
    prefer_lowest: bool,
    working_dir: Option<PathBuf>,
    dry_run: bool,
) -> Result<ExitCode> {
    let _ = prefer_lowest; // resolver prefer-lowest wiring is a follow-up; flag accepted for parity.
    let project_root = resolve_root(working_dir)?;
    let paths = Paths::from_env()?;

    let pairs = parse_name_version_pairs(&packages);

    // Resolve a constraint for every bare (no-version) real package via
    // the latest published stable; platform packages default to `*`.
    let needs_lookup: Vec<String> = pairs
        .iter()
        .filter(|p| p.version.is_none() && !is_platform(&p.name))
        .map(|p| p.name.clone())
        .collect();
    let mut latest: std::collections::HashMap<String, Vec<String>> =
        std::collections::HashMap::new();
    if !needs_lookup.is_empty() {
        latest = latest_versions(&paths, &project_root, &needs_lookup, false)
            .wrap_err("looking up latest versions for the requested packages")?
            .into_iter()
            .collect();
    }

    let mut items = Vec::with_capacity(pairs.len());
    let mut changes = Vec::with_capacity(pairs.len());
    for p in &pairs {
        let constraint = match &p.version {
            Some(v) => v.clone(),
            None if is_platform(&p.name) => "*".to_string(),
            None => {
                let versions = latest
                    .get(&p.name.to_ascii_lowercase())
                    .ok_or_else(|| {
                        eyre!(
                            "could not find package {} in any configured repository",
                            p.name
                        )
                    })?;
                let best = best_stable(versions).ok_or_else(|| {
                    eyre!("no stable version of {} found to require", p.name)
                })?;
                derive_constraint(&best, DefaultConstraint::Caret)
            }
        };
        // Validate the constraint grammar (don't reparse — just check).
        bougie_semver::constraint::Constraint::parse(&constraint)
            .map_err(|e| eyre!("invalid version constraint {constraint:?} for {}: {e}", p.name))?;
        items.push(RequireItem {
            name: p.name.clone(),
            constraint: Some(constraint.clone()),
        });
        changes.push(RequireChange::Add {
            key: p.name.clone(),
            constraint,
            dev,
        });
    }

    if dry_run {
        return finish(
            format,
            "require",
            project_root,
            dev,
            true,
            no_update,
            no_install,
            items,
            None,
        );
    }

    for change in &changes {
        apply_require_change(&project_root, change)
            .wrap_err("updating composer.json")?;
    }

    let lock_path = if no_update {
        None
    } else {
        let names: Vec<String> = pairs.iter().map(|p| p.name.clone()).collect();
        let path = relock(
            &paths,
            &project_root,
            &names,
            with_dependencies,
            with_all_dependencies,
        )?;
        if !no_install {
            install_from_lock(&paths, &project_root, InstallOptions { no_dev: false }, None)
                .wrap_err("installing packages")?;
        }
        Some(path)
    };

    finish(
        format,
        "require",
        project_root,
        dev,
        false,
        no_update,
        no_install,
        items,
        lock_path,
    )
}

/// `bougie composer remove`.
#[allow(
    clippy::too_many_arguments,
    clippy::fn_params_excessive_bools,
    clippy::needless_pass_by_value,
    reason = "wired from clap-parsed CLI; ownership crosses the function boundary"
)]
pub fn remove(
    format: OutputFormat,
    packages: Vec<String>,
    dev: bool,
    no_update: bool,
    no_install: bool,
    no_dev: bool,
    working_dir: Option<PathBuf>,
    dry_run: bool,
) -> Result<ExitCode> {
    let project_root = resolve_root(working_dir)?;
    let paths = Paths::from_env()?;

    let items: Vec<RequireItem> = packages
        .iter()
        .map(|name| RequireItem {
            name: name.clone(),
            constraint: None,
        })
        .collect();

    if dry_run {
        return finish(
            format, "remove", project_root, dev, true, no_update, no_install, items, None,
        );
    }

    for name in &packages {
        apply_require_change(
            &project_root,
            &RequireChange::Remove {
                key: name.clone(),
                dev,
            },
        )
        .wrap_err("updating composer.json")?;
    }

    let lock_path = if no_update {
        None
    } else {
        let path = relock(&paths, &project_root, &packages, false, false)?;
        if !no_install {
            install_from_lock(&paths, &project_root, InstallOptions { no_dev }, None)
                .wrap_err("uninstalling packages")?;
        }
        Some(path)
    };

    finish(
        format, "remove", project_root, dev, false, no_update, no_install, items, lock_path,
    )
}

/// Re-resolve and write `composer.lock`. When a lock already exists,
/// this is a partial update scoped to `names` (the affected packages) so
/// unrelated packages stay pinned — matching Composer's `require`/`remove`
/// minimal-change behavior. Without a prior lock, a full resolve runs.
fn relock(
    paths: &Paths,
    project_root: &Path,
    names: &[String],
    with_dependencies: bool,
    with_all_dependencies: bool,
) -> Result<PathBuf> {
    let lock_path = project_root.join("composer.lock");
    if lock_path.is_file() {
        let lock = Lock::read(&lock_path)
            .wrap_err_with(|| format!("reading {}", lock_path.display()))?;
        let root_requires = read_root_require_names(project_root);
        let partial = PartialUpdate {
            names: names.to_vec(),
            with_dependencies,
            with_all_dependencies,
            root_requires,
            lock,
        };
        let (path, _outcome) = super::composer_update::resolve_and_write_lock_partial(
            paths,
            project_root,
            Some(&partial),
        )?;
        Ok(path)
    } else {
        let (path, _outcome) =
            super::composer_update::resolve_and_write_lock(paths, project_root)?;
        Ok(path)
    }
}

/// Pick the highest stable version from a Packagist version list.
/// Non-parseable and non-stable entries are skipped.
fn best_stable(versions: &[String]) -> Option<String> {
    versions
        .iter()
        .filter_map(|v| {
            let parsed = Version::parse(v).ok()?;
            (parsed.stability() == Stability::Stable).then_some((parsed, v.clone()))
        })
        .max_by(|a, b| a.0.cmp(&b.0))
        .map(|(_, pretty)| pretty)
}

/// Collect the project's root requirement names (keys of `require` +
/// `require-dev`). Mirrors `composer_update::read_root_require_names`.
fn read_root_require_names(project_root: &Path) -> Vec<String> {
    let path = project_root.join("composer.json");
    let Ok(bytes) = std::fs::read(&path) else {
        return Vec::new();
    };
    let Ok(json) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
        return Vec::new();
    };
    let mut names = Vec::new();
    for key in ["require", "require-dev"] {
        if let Some(obj) = json.get(key).and_then(serde_json::Value::as_object) {
            names.extend(obj.keys().cloned());
        }
    }
    names
}

fn resolve_root(working_dir: Option<PathBuf>) -> Result<PathBuf> {
    match working_dir {
        Some(p) => Ok(p),
        None => std::env::current_dir().wrap_err("reading current directory"),
    }
}

#[allow(clippy::too_many_arguments, clippy::fn_params_excessive_bools)]
fn finish(
    format: OutputFormat,
    action: &'static str,
    project_root: PathBuf,
    dev: bool,
    dry_run: bool,
    no_update: bool,
    no_install: bool,
    packages: Vec<RequireItem>,
    lock_path: Option<PathBuf>,
) -> Result<ExitCode> {
    let result = RequireResult {
        schema_version: 1,
        action,
        project_root,
        dev,
        dry_run,
        no_update,
        no_install,
        packages,
        lock_path,
    };
    emit(format, &result)?;
    Ok(ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nv(name: &str, version: Option<&str>) -> NameVersion {
        NameVersion {
            name: name.to_string(),
            version: version.map(str::to_string),
        }
    }

    #[test]
    fn colon_equals_space_separators() {
        assert_eq!(
            parse_name_version_pairs(&["monolog/monolog:^3.5".into()]),
            vec![nv("monolog/monolog", Some("^3.5"))]
        );
        assert_eq!(
            parse_name_version_pairs(&["monolog/monolog=^3.5".into()]),
            vec![nv("monolog/monolog", Some("^3.5"))]
        );
        // Quoted single arg with an embedded space + multi-part AND.
        assert_eq!(
            parse_name_version_pairs(&["monolog/monolog >=1.0 <2.0".into()]),
            vec![nv("monolog/monolog", Some(">=1.0 <2.0"))]
        );
    }

    #[test]
    fn at_is_not_a_separator() {
        // `@` stays part of the name — Composer would then fail to find
        // the package, exactly as bougie does downstream.
        assert_eq!(
            parse_name_version_pairs(&["monolog/monolog@^3.5".into()]),
            vec![nv("monolog/monolog@^3.5", None)]
        );
    }

    #[test]
    fn bare_name_derives() {
        assert_eq!(
            parse_name_version_pairs(&["monolog/monolog".into()]),
            vec![nv("monolog/monolog", None)]
        );
    }

    #[test]
    fn two_argv_form_and_guards() {
        // `name ^1.0` → consume next as constraint.
        assert_eq!(
            parse_name_version_pairs(&["monolog/monolog".into(), "^3.5".into()]),
            vec![nv("monolog/monolog", Some("^3.5"))]
        );
        // Next arg with a slash is a separate package, not a constraint.
        assert_eq!(
            parse_name_version_pairs(&["acme/a".into(), "acme/b".into()]),
            vec![nv("acme/a", None), nv("acme/b", None)]
        );
        // A version glob like `1.*` (the `*` is adjacent to `.`, not a
        // word char) IS consumed — Composer's guard only fires for
        // package-name globs adjacent to word chars.
        assert_eq!(
            parse_name_version_pairs(&["acme/a".into(), "1.*".into()]),
            vec![nv("acme/a", Some("1.*"))]
        );
        // A package-name glob (`*` next to a word char) is NOT consumed
        // as a constraint — it's treated as its own arg.
        assert_eq!(
            parse_name_version_pairs(&["acme/a".into(), "mono*".into()]),
            vec![nv("acme/a", None), nv("mono*", None)]
        );
        // A platform package as the next arg is not consumed.
        assert_eq!(
            parse_name_version_pairs(&["acme/a".into(), "ext-redis".into()]),
            vec![nv("acme/a", None), nv("ext-redis", None)]
        );
    }

    #[test]
    fn constraint_after_first_separator_is_verbatim() {
        // Only the FIRST separator splits; the rest (commas, operators)
        // is the constraint untouched.
        assert_eq!(
            parse_name_version_pairs(&["acme/a:>=1.0,<2.0".into()]),
            vec![nv("acme/a", Some(">=1.0,<2.0"))]
        );
    }

    #[test]
    fn caret_transform_matches_composer() {
        assert_eq!(derive_constraint("3.5.2", DefaultConstraint::Caret), "^3.5");
        assert_eq!(derive_constraint("1.0.0", DefaultConstraint::Caret), "^1.0");
        // 0.x keeps the minor + patch.
        assert_eq!(derive_constraint("0.3.2", DefaultConstraint::Caret), "^0.3.2");
        // Two-segment input normalizes to x.y.0.0 → x.y.
        assert_eq!(derive_constraint("3.5", DefaultConstraint::Caret), "^3.5");
    }

    #[test]
    fn lower_bound_policy() {
        assert_eq!(derive_constraint("3.5.2", DefaultConstraint::LowerBound), ">=3.5");
        assert_eq!(derive_constraint("0.3.2", DefaultConstraint::LowerBound), ">=0.3.2");
    }

    #[test]
    fn non_semver_falls_back_to_pretty() {
        // A dev/branch version isn't transformed; the pretty string
        // is used as-is (caret-prefixed).
        assert_eq!(derive_constraint("dev-main", DefaultConstraint::Caret), "^dev-main");
    }
}
