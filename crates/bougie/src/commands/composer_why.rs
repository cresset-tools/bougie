//! `bougie composer why` (alias `depends`) and `why-not` (alias
//! `prohibits`) — read-only reverse-dependency inspection over the
//! Phase-0 [`DependencyGraph`].
//!
//! - `why <pkg>` answers "what installed this?" — every package (and
//!   the root project) that directly requires `<pkg>`, with the
//!   constraint it imposes.
//! - `why-not <pkg> [version]` answers "what stops `<pkg>` (at
//!   `version`)?" — packages whose `require` constraint excludes that
//!   version, plus packages that `conflict` with it.
//!
//! `why-not`'s `version` argument is matched as a concrete version when
//! it parses as one; a non-concrete constraint (e.g. `^2`) falls back to
//! "any", so only `conflict` clauses are reported. This is a behavioral
//! approximation of Composer's full solver-driven `prohibits`, which is
//! noted in `COMPOSER_COMPAT_PLAN.md`.

use std::io::{self, Write};
use std::path::PathBuf;
use std::process::ExitCode;

use bougie_cli::OutputFormat;
use bougie_composer::lockfile::Lock;
use bougie_composer_resolver::DependencyGraph;
use bougie_output::output::{emit, Render};
use composer_semver::constraint::Constraint;
use composer_semver::version::Version;
use eyre::{eyre, Context, Result};
use serde::Serialize;

/// A require/conflict source row: `(name, version, require-map,
/// conflict-map)`. Includes the root project (with a `(root)` version).
type ReqMap = std::collections::BTreeMap<String, String>;
type Source<'a> = (String, String, &'a ReqMap, &'a ReqMap);

#[derive(Debug, Serialize)]
pub struct WhyResult {
    pub schema_version: u32,
    pub package: String,
    /// `"why"` or `"why-not"`.
    pub kind: &'static str,
    pub reasons: Vec<WhyReason>,
}

#[derive(Debug, Serialize)]
pub struct WhyReason {
    /// The requiring (or conflicting) package, or the root project.
    pub from: String,
    pub from_version: String,
    /// `"requires"` or `"conflicts with"`.
    pub relation: &'static str,
    pub constraint: String,
}

impl Render for WhyResult {
    fn render_text(&self, w: &mut dyn Write) -> io::Result<()> {
        if self.reasons.is_empty() {
            return match self.kind {
                "why" => writeln!(
                    w,
                    "There is no installed package depending on \"{}\"",
                    self.package
                ),
                _ => writeln!(
                    w,
                    "Nothing prevents \"{}\" from being installed.",
                    self.package
                ),
            };
        }
        let name_w = self
            .reasons
            .iter()
            .map(|r| r.from.len())
            .max()
            .unwrap_or(0);
        for r in &self.reasons {
            writeln!(
                w,
                "{:name_w$}  {}  {} {} ({})",
                r.from, r.from_version, r.relation, self.package, r.constraint
            )?;
        }
        Ok(())
    }
}

fn project_root(working_dir: Option<PathBuf>) -> Result<PathBuf> {
    match working_dir {
        Some(p) => Ok(p),
        None => std::env::current_dir().wrap_err("reading current directory"),
    }
}

fn load(project_root: &std::path::Path) -> Result<(Lock, serde_json::Value)> {
    let lock_path = project_root.join("composer.lock");
    if !lock_path.is_file() {
        return Err(eyre!(
            "no composer.lock in {} — run `bougie composer install` or `update` first",
            project_root.display()
        ));
    }
    let lock = Lock::read(&lock_path).wrap_err_with(|| format!("reading {}", lock_path.display()))?;
    let root = std::fs::read(project_root.join("composer.json"))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or(serde_json::Value::Null);
    Ok((lock, root))
}

fn json_map(root: &serde_json::Value, key: &str) -> std::collections::BTreeMap<String, String> {
    root.get(key)
        .and_then(serde_json::Value::as_object)
        .map(|o| {
            o.iter()
                .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                .collect()
        })
        .unwrap_or_default()
}

/// `bougie composer why` / `depends`.
#[allow(
    clippy::needless_pass_by_value,
    reason = "wired from clap-parsed CLI; ownership crosses the boundary"
)]
pub fn why(
    format: OutputFormat,
    package: String,
    _recursive: bool,
    _tree: bool,
    working_dir: Option<PathBuf>,
) -> Result<ExitCode> {
    let root_dir = project_root(working_dir)?;
    let (lock, root) = load(&root_dir)?;
    let root_name = root
        .get("name")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("__root__")
        .to_string();
    let graph = DependencyGraph::from_lock(&lock).with_root(
        root_name.clone(),
        &json_map(&root, "require"),
        &json_map(&root, "require-dev"),
    );

    let mut reasons: Vec<WhyReason> = graph
        .dependents_of(&package)
        .into_iter()
        .map(|(node, constraint)| WhyReason {
            from: node.name.clone(),
            from_version: node.version.clone(),
            relation: "requires",
            constraint: constraint.to_string(),
        })
        .collect();

    // The root project is reported separately (it has no lock node).
    if let Some(c) = graph.root_requires(&package) {
        reasons.push(WhyReason {
            from: root_name,
            from_version: "(root)".to_string(),
            relation: "requires",
            constraint: c.to_string(),
        });
    }

    reasons.sort_by_key(|r| r.from.to_ascii_lowercase());

    let result = WhyResult {
        schema_version: 1,
        package,
        kind: "why",
        reasons,
    };
    emit(format, &result)?;
    Ok(ExitCode::SUCCESS)
}

/// `bougie composer why-not` / `prohibits`.
#[allow(
    clippy::needless_pass_by_value,
    reason = "wired from clap-parsed CLI; ownership crosses the boundary"
)]
pub fn why_not(
    format: OutputFormat,
    package: String,
    version: Option<String>,
    _recursive: bool,
    _tree: bool,
    working_dir: Option<PathBuf>,
) -> Result<ExitCode> {
    let root_dir = project_root(working_dir)?;
    let (lock, root) = load(&root_dir)?;
    let key = package.to_ascii_lowercase();

    // Parse the target version as a concrete version when possible;
    // otherwise we can only reason about conflicts.
    let target = version.as_deref().and_then(|v| Version::parse(v).ok());

    let mut reasons: Vec<WhyReason> = Vec::new();

    // Build a unified list of sources (each package + the root project).
    let mut sources: Vec<Source<'_>> = Vec::new();
    for pkg in lock.all_packages() {
        sources.push((pkg.name.clone(), pkg.version.clone(), &pkg.require, &pkg.conflict));
    }
    let root_require = json_map(&root, "require");
    let root_require_dev = json_map(&root, "require-dev");
    // Merge root require + require-dev into one map for scanning.
    let mut root_all = root_require.clone();
    root_all.extend(root_require_dev);
    let empty: ReqMap = ReqMap::new();
    let root_name = root
        .get("name")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("__root__")
        .to_string();
    sources.push((root_name, "(root)".to_string(), &root_all, &empty));

    for (from, from_version, requires, conflicts) in &sources {
        // A `require` on the target whose constraint excludes the
        // requested version is a prohibitor.
        if let Some(raw) = requires.get(&package).or_else(|| find_ci(requires, &key))
            && let (Some(t), Ok(c)) = (&target, Constraint::parse(raw))
            && !c.matches(t)
        {
            reasons.push(WhyReason {
                from: from.clone(),
                from_version: from_version.clone(),
                relation: "requires",
                constraint: raw.clone(),
            });
        }
        // A `conflict` on the target whose constraint matches the
        // requested version is a prohibitor.
        if let Some(raw) = conflicts.get(&package).or_else(|| find_ci(conflicts, &key)) {
            let matches = match (&target, Constraint::parse(raw)) {
                (Some(t), Ok(c)) => c.matches(t),
                // No concrete target → treat any conflict clause as
                // prohibiting (the broadest reading).
                (None, Ok(_)) => true,
                _ => false,
            };
            if matches {
                reasons.push(WhyReason {
                    from: from.clone(),
                    from_version: from_version.clone(),
                    relation: "conflicts with",
                    constraint: raw.clone(),
                });
            }
        }
    }

    reasons.sort_by_key(|r| r.from.to_ascii_lowercase());

    let result = WhyResult {
        schema_version: 1,
        package,
        kind: "why-not",
        reasons,
    };
    emit(format, &result)?;
    Ok(ExitCode::SUCCESS)
}

/// Case-insensitive lookup in a require/conflict map.
fn find_ci<'a>(
    map: &'a std::collections::BTreeMap<String, String>,
    key_lower: &str,
) -> Option<&'a String> {
    map.iter()
        .find(|(k, _)| k.to_ascii_lowercase() == key_lower)
        .map(|(_, v)| v)
}
