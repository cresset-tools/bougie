//! `bougie composer outdated` — list installed packages with a newer
//! version available. A focused `show --latest --outdated`: it reuses
//! the Phase-0 `latest_versions` lookup and shares its engine with the
//! future top-level `bougie outdated`.

use std::io::{self, Write};
use std::path::PathBuf;
use std::process::ExitCode;

use bougie_cli::OutputFormat;
use bougie_composer::lockfile::{Lock, LockPackage};
use bougie_composer_resolver::latest_versions;
use bougie_output::output::{emit, Render};
use bougie_paths::Paths;
use composer_semver::stability::Stability;
use composer_semver::version::Version;
use eyre::{eyre, Context, Result};
use serde::Serialize;

/// Severity of a version gap, used by the `--major/minor/patch-only`
/// filters.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Bump {
    Major,
    Minor,
    Patch,
}

#[derive(Debug, Clone)]
#[allow(clippy::struct_excessive_bools, reason = "mirrors Composer's independent outdated flags")]
pub struct OutdatedOptions {
    pub packages: Vec<String>,
    pub direct: bool,
    pub major_only: bool,
    pub minor_only: bool,
    pub patch_only: bool,
    pub no_dev: bool,
    pub strict: bool,
    pub working_dir: Option<PathBuf>,
}

#[derive(Debug, Serialize)]
pub struct OutdatedRow {
    pub name: String,
    pub version: String,
    pub latest: String,
    pub bump: Bump,
}

#[derive(Debug, Serialize)]
pub struct OutdatedResult {
    pub schema_version: u32,
    pub rows: Vec<OutdatedRow>,
}

impl Render for OutdatedResult {
    fn render_text(&self, w: &mut dyn Write) -> io::Result<()> {
        if self.rows.is_empty() {
            return writeln!(w, "All packages are up to date.");
        }
        let name_w = self.rows.iter().map(|r| r.name.len()).max().unwrap_or(0);
        let ver_w = self.rows.iter().map(|r| r.version.len()).max().unwrap_or(0);
        for r in &self.rows {
            let marker = match r.bump {
                Bump::Major => "!", // semver-major: review before upgrading
                Bump::Minor | Bump::Patch => "~",
            };
            writeln!(
                w,
                "{:name_w$}  {:ver_w$}  {} {}",
                r.name, r.version, marker, r.latest
            )?;
        }
        Ok(())
    }
}

/// Three numeric segments (major, minor, patch) of a normalized version.
fn segments(v: &Version) -> (u64, u64, u64) {
    let mut it = v
        .normalized
        .split(['.', '-'])
        .filter_map(|s| s.parse::<u64>().ok());
    (
        it.next().unwrap_or(0),
        it.next().unwrap_or(0),
        it.next().unwrap_or(0),
    )
}

/// Classify the gap between `current` and `latest`. `None` when latest
/// isn't strictly newer.
fn classify(current: &Version, latest: &Version) -> Option<Bump> {
    if latest <= current {
        return None;
    }
    let (cm, cn, _) = segments(current);
    let (lm, ln, _) = segments(latest);
    Some(if lm > cm {
        Bump::Major
    } else if ln > cn {
        Bump::Minor
    } else {
        Bump::Patch
    })
}

fn best_stable(versions: &[String]) -> Option<Version> {
    versions
        .iter()
        .filter_map(|v| Version::parse(v).ok())
        .filter(|v| v.stability() == Stability::Stable)
        .max()
}

#[allow(
    clippy::needless_pass_by_value,
    reason = "wired from clap-parsed CLI; ownership crosses the boundary"
)]
pub fn run(format: OutputFormat, opts: OutdatedOptions) -> Result<ExitCode> {
    let project_root = match &opts.working_dir {
        Some(p) => p.clone(),
        None => std::env::current_dir().wrap_err("reading current directory")?,
    };
    let paths = Paths::from_env()?;

    let lock_path = project_root.join("composer.lock");
    if !lock_path.is_file() {
        return Err(eyre!(
            "no composer.lock in {} — run `bougie composer install` or `update` first",
            project_root.display()
        ));
    }
    let lock = Lock::read(&lock_path).wrap_err_with(|| format!("reading {}", lock_path.display()))?;

    let mut packages: Vec<&LockPackage> = if opts.no_dev {
        lock.packages.iter().collect()
    } else {
        lock.all_packages().collect()
    };

    // `--direct`: restrict to the project's direct requires.
    if opts.direct {
        let direct = root_require_names(&project_root, !opts.no_dev);
        packages.retain(|p| direct.iter().any(|n| n.eq_ignore_ascii_case(&p.name)));
    }
    // Positional package filters.
    if !opts.packages.is_empty() {
        packages.retain(|p| opts.packages.iter().any(|n| n.eq_ignore_ascii_case(&p.name)));
    }

    let names: Vec<String> = packages.iter().map(|p| p.name.clone()).collect();
    let latest_map: std::collections::HashMap<String, Vec<String>> =
        latest_versions(&paths, &project_root, &names, false)
            .wrap_err("looking up latest versions")?
            .into_iter()
            .collect();

    // Only one of the bump filters is honored (Composer treats them as
    // mutually exclusive); narrowest wins if several are set.
    let wanted: Option<Bump> = if opts.patch_only {
        Some(Bump::Patch)
    } else if opts.minor_only {
        Some(Bump::Minor)
    } else if opts.major_only {
        Some(Bump::Major)
    } else {
        None
    };

    let mut rows = Vec::new();
    for p in &packages {
        let (Some(latest), Ok(current)) = (
            latest_map
                .get(&p.name.to_ascii_lowercase())
                .and_then(|v| best_stable(v)),
            Version::parse(&p.version),
        ) else {
            continue;
        };
        let Some(bump) = classify(&current, &latest) else {
            continue;
        };
        if let Some(w) = wanted
            && bump != w
        {
            continue;
        }
        rows.push(OutdatedRow {
            name: p.name.clone(),
            version: p.version.clone(),
            latest: latest.normalized.clone(),
            bump,
        });
    }
    rows.sort_by(|a, b| a.name.cmp(&b.name));

    let any = !rows.is_empty();
    emit(format, &OutdatedResult { schema_version: 1, rows })?;
    Ok(if opts.strict && any {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    })
}

fn root_require_names(project_root: &std::path::Path, include_dev: bool) -> Vec<String> {
    let Ok(bytes) = std::fs::read(project_root.join("composer.json")) else {
        return Vec::new();
    };
    let Ok(json) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
        return Vec::new();
    };
    let mut names = Vec::new();
    let keys: &[&str] = if include_dev { &["require", "require-dev"] } else { &["require"] };
    for key in keys {
        if let Some(obj) = json.get(*key).and_then(serde_json::Value::as_object) {
            names.extend(obj.keys().cloned());
        }
    }
    names
}
