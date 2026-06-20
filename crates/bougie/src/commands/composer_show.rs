//! `bougie composer show` (aliases `info`, `list`) — read-only package
//! inspection over the project's `composer.lock`.
//!
//! Native reimplementation of `composer show`. Covers the listing view,
//! single-package detail, the dependency `--tree`, the `--latest` /
//! `--outdated` columns (via the Phase-0 `latest_versions` lookup), and
//! the `--direct` / `--platform` / `--no-dev` / `--name-only` / `--path`
//! / `--self` filters. `--format json` emits a structured payload
//! independently of bougie's global `--format`.

use std::collections::BTreeMap;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use bougie_cli::OutputFormat;
use bougie_composer::lockfile::{Lock, LockPackage};
use bougie_composer_resolver::verify::is_platform;
use bougie_composer_resolver::{latest_versions, DependencyGraph};
use bougie_output::output::{emit, Render};
use bougie_paths::Paths;
use composer_semver::stability::Stability;
use composer_semver::version::Version;
use eyre::{eyre, Context, Result};
use serde::Serialize;

/// Flags for the `show` command, grouped so the dispatch arm stays
/// readable.
#[derive(Debug, Clone)]
#[allow(clippy::struct_excessive_bools, reason = "mirrors Composer's independent show flags")]
pub struct ShowOptions {
    pub package: Option<String>,
    pub tree: bool,
    /// Collapse already-expanded subtrees to a single `(*)`-marked line
    /// instead of re-rendering them (uv/`cargo tree` behavior). Set for
    /// `bougie tree`; left off for `composer show --tree`, which matches
    /// Composer's full-repeat output byte-for-byte.
    pub dedupe: bool,
    pub direct: bool,
    pub platform: bool,
    pub self_: bool,
    pub name_only: bool,
    pub path: bool,
    pub latest: bool,
    pub outdated: bool,
    pub no_dev: bool,
    pub working_dir: Option<PathBuf>,
}

/// One row in the listing view.
#[derive(Debug, Serialize)]
pub struct ShowRow {
    pub name: String,
    pub version: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub latest: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// `true` when a newer version than `version` is available.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub outdated: bool,
}

#[derive(Debug, Serialize)]
pub struct ShowResult {
    pub schema_version: u32,
    pub rows: Vec<ShowRow>,
    #[serde(skip)]
    name_only: bool,
    #[serde(skip)]
    show_latest: bool,
    #[serde(skip)]
    show_path: bool,
}

impl Render for ShowResult {
    fn render_text(&self, w: &mut dyn Write) -> io::Result<()> {
        if self.rows.is_empty() {
            return writeln!(w, "no packages found");
        }
        if self.name_only {
            for r in &self.rows {
                writeln!(w, "{}", r.name)?;
            }
            return Ok(());
        }
        let name_w = self.rows.iter().map(|r| r.name.len()).max().unwrap_or(0);
        let ver_w = self.rows.iter().map(|r| r.version.len()).max().unwrap_or(0);
        let latest_w = if self.show_latest {
            self.rows
                .iter()
                .map(|r| r.latest.as_deref().unwrap_or("").len())
                .max()
                .unwrap_or(0)
        } else {
            0
        };
        for r in &self.rows {
            write!(w, "{:name_w$}  {:ver_w$}", r.name, r.version)?;
            if self.show_latest {
                write!(w, "  {:latest_w$}", r.latest.as_deref().unwrap_or(""))?;
            }
            if self.show_path {
                write!(w, "  {}", r.path.as_deref().unwrap_or(""))?;
            } else if let Some(d) = &r.description {
                write!(w, "  {d}")?;
            }
            writeln!(w)?;
        }
        Ok(())
    }
}

/// Detail view for a single package (`composer show vendor/name`).
#[derive(Debug, Serialize)]
pub struct ShowDetail {
    pub schema_version: u32,
    pub name: String,
    pub version: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub package_type: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub license: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dist: Option<String>,
    pub requires: BTreeMap<String, String>,
}

impl Render for ShowDetail {
    fn render_text(&self, w: &mut dyn Write) -> io::Result<()> {
        writeln!(w, "name     : {}", self.name)?;
        writeln!(w, "version  : {}", self.version)?;
        if let Some(d) = &self.description {
            writeln!(w, "descrip. : {d}")?;
        }
        if let Some(t) = &self.package_type {
            writeln!(w, "type     : {t}")?;
        }
        if !self.license.is_empty() {
            writeln!(w, "license  : {}", self.license.join(", "))?;
        }
        if let Some(s) = &self.source {
            writeln!(w, "source   : {s}")?;
        }
        if let Some(d) = &self.dist {
            writeln!(w, "dist     : {d}")?;
        }
        if !self.requires.is_empty() {
            writeln!(w, "\nrequires")?;
            for (k, v) in &self.requires {
                writeln!(w, "{k} {v}")?;
            }
        }
        Ok(())
    }
}

/// Tree view (`--tree`).
#[derive(Debug, Serialize)]
pub struct ShowTree {
    pub schema_version: u32,
    pub lines: Vec<String>,
}

impl Render for ShowTree {
    fn render_text(&self, w: &mut dyn Write) -> io::Result<()> {
        for l in &self.lines {
            writeln!(w, "{l}")?;
        }
        Ok(())
    }
}

/// Pick the highest stable version from a Packagist version list.
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

fn load_lock(project_root: &Path) -> Result<Lock> {
    let lock_path = project_root.join("composer.lock");
    if !lock_path.is_file() {
        return Err(eyre!(
            "no composer.lock in {} — run `bougie composer install` or `update` first",
            project_root.display()
        ));
    }
    Lock::read(&lock_path).wrap_err_with(|| format!("reading {}", lock_path.display()))
}

fn read_root(project_root: &Path) -> serde_json::Value {
    std::fs::read(project_root.join("composer.json"))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or(serde_json::Value::Null)
}

fn root_require_names(root: &serde_json::Value, include_dev: bool) -> Vec<String> {
    let mut names = Vec::new();
    let keys: &[&str] = if include_dev { &["require", "require-dev"] } else { &["require"] };
    for key in keys {
        if let Some(obj) = root.get(*key).and_then(serde_json::Value::as_object) {
            names.extend(obj.keys().cloned());
        }
    }
    names
}

#[allow(
    clippy::needless_pass_by_value,
    reason = "wired from clap-parsed CLI; ownership crosses the boundary"
)]
pub fn run(format: OutputFormat, opts: ShowOptions) -> Result<ExitCode> {
    let project_root = match &opts.working_dir {
        Some(p) => p.clone(),
        None => std::env::current_dir().wrap_err("reading current directory")?,
    };
    let paths = Paths::from_env()?;
    let eff = format;

    // `--self`: render the root package from composer.json.
    if opts.self_ {
        return show_self(eff, &project_root);
    }

    let lock = load_lock(&project_root)?;
    let root = read_root(&project_root);

    // Single-package detail view.
    if let Some(pkg) = &opts.package {
        if opts.tree {
            return show_tree(eff, &lock, &root, Some(pkg), opts.dedupe);
        }
        return show_detail(eff, &lock, pkg);
    }

    if opts.tree {
        return show_tree(eff, &lock, &root, None, opts.dedupe);
    }

    // Listing view.
    let include_dev = !opts.no_dev;
    let mut packages: Vec<&LockPackage> = if include_dev {
        lock.all_packages().collect()
    } else {
        lock.packages.iter().collect()
    };

    // `--platform`: replace the package list with the platform requires.
    if opts.platform {
        return show_platform(eff, &root, include_dev);
    }

    // `--direct`: restrict to the project's direct requires.
    if opts.direct {
        let direct: std::collections::HashSet<String> = root_require_names(&root, include_dev)
            .into_iter()
            .map(|n| n.to_ascii_lowercase())
            .collect();
        packages.retain(|p| direct.contains(&p.name.to_ascii_lowercase()));
    }

    packages.sort_by_key(|p| p.name.to_ascii_lowercase());

    // `--latest` / `--outdated`: fetch available versions.
    let want_latest = opts.latest || opts.outdated;
    let mut latest_map: std::collections::HashMap<String, Vec<String>> =
        std::collections::HashMap::new();
    if want_latest {
        let names: Vec<String> = packages.iter().map(|p| p.name.clone()).collect();
        latest_map = latest_versions(&paths, &project_root, &names, false)
            .wrap_err("looking up latest versions")?
            .into_iter()
            .collect();
    }

    let mut rows = Vec::with_capacity(packages.len());
    for p in &packages {
        let latest = latest_map
            .get(&p.name.to_ascii_lowercase())
            .and_then(|v| best_stable(v));
        let is_outdated = match (&latest, Version::parse(&p.version)) {
            (Some(l), Ok(cur)) => Version::parse(l).is_ok_and(|lv| lv > cur),
            _ => false,
        };
        if opts.outdated && !is_outdated {
            continue;
        }
        let path = opts.path.then(|| {
            project_root
                .join("vendor")
                .join(&p.name)
                .display()
                .to_string()
        });
        rows.push(ShowRow {
            name: p.name.clone(),
            version: p.version.clone(),
            latest: if want_latest { latest } else { None },
            description: p.description.clone(),
            path,
            outdated: is_outdated,
        });
    }

    let result = ShowResult {
        schema_version: 1,
        rows,
        name_only: opts.name_only,
        show_latest: want_latest,
        show_path: opts.path,
    };
    emit(eff, &result)?;
    Ok(ExitCode::SUCCESS)
}

fn show_detail(format: OutputFormat, lock: &Lock, name: &str) -> Result<ExitCode> {
    let key = name.to_ascii_lowercase();
    let pkg = lock
        .all_packages()
        .find(|p| p.name.to_ascii_lowercase() == key)
        .ok_or_else(|| eyre!("package {name} is not installed (not in composer.lock)"))?;
    let detail = ShowDetail {
        schema_version: 1,
        name: pkg.name.clone(),
        version: pkg.version.clone(),
        description: pkg.description.clone(),
        package_type: pkg.package_type.clone(),
        license: pkg.license.clone(),
        source: pkg.source.as_ref().map(|s| format!("{} {}", s.kind, s.url)),
        dist: pkg.dist.as_ref().map(|d| format!("{} {}", d.kind, d.url)),
        requires: pkg.require.clone(),
    };
    emit(format, &detail)?;
    Ok(ExitCode::SUCCESS)
}

fn show_self(format: OutputFormat, project_root: &Path) -> Result<ExitCode> {
    let root = read_root(project_root);
    let name = root
        .get("name")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("__root__")
        .to_string();
    let detail = ShowDetail {
        schema_version: 1,
        name,
        version: root
            .get("version")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("no version set")
            .to_string(),
        description: root
            .get("description")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string),
        package_type: root
            .get("type")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string),
        license: root
            .get("license")
            .and_then(serde_json::Value::as_str)
            .map(|s| vec![s.to_string()])
            .unwrap_or_default(),
        source: None,
        dist: None,
        requires: root
            .get("require")
            .and_then(serde_json::Value::as_object)
            .map(|o| {
                o.iter()
                    .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                    .collect()
            })
            .unwrap_or_default(),
    };
    emit(format, &detail)?;
    Ok(ExitCode::SUCCESS)
}

fn show_platform(
    format: OutputFormat,
    root: &serde_json::Value,
    include_dev: bool,
) -> Result<ExitCode> {
    let mut rows = Vec::new();
    let keys: &[&str] = if include_dev { &["require", "require-dev"] } else { &["require"] };
    for key in keys {
        if let Some(obj) = root.get(*key).and_then(serde_json::Value::as_object) {
            for (name, constraint) in obj {
                if is_platform(name) {
                    rows.push(ShowRow {
                        name: name.clone(),
                        version: constraint.as_str().unwrap_or("*").to_string(),
                        latest: None,
                        description: None,
                        path: None,
                        outdated: false,
                    });
                }
            }
        }
    }
    rows.sort_by(|a, b| a.name.cmp(&b.name));
    let result = ShowResult {

        schema_version: 1,
        rows,
        name_only: false,
        show_latest: false,
        show_path: false,
    };
    emit(format, &result)?;
    Ok(ExitCode::SUCCESS)
}

fn show_tree(
    format: OutputFormat,
    lock: &Lock,
    root: &serde_json::Value,
    single: Option<&str>,
    dedupe: bool,
) -> Result<ExitCode> {
    let root_name = root
        .get("name")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("__root__")
        .to_string();
    let root_requires = json_map(root, "require");
    let root_requires_dev = json_map(root, "require-dev");
    let graph = DependencyGraph::from_lock(lock).with_root(
        root_name.clone(),
        &root_requires,
        &root_requires_dev,
    );

    // `expanded` tracks (canonical) names whose subtree has already been
    // rendered, so a repeated dependency collapses to a `(*)` line rather
    // than re-expanding. Without it a graph with shared transitive deps
    // (the norm: psr/*, symfony/polyfill-*, …) re-renders every shared
    // subtree once per path to it — combinatorial blow-up that hangs on
    // real lockfiles. Only used in `dedupe` mode (`bougie tree`); the
    // Composer-compatible `composer show --tree` keeps the full repeat.
    let mut expanded: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut collapsed = false;

    let mut lines = Vec::new();
    if let Some(name) = single {
        // Tree rooted at the named package.
        let Some(node) = graph.node(name) else {
            return Err(eyre!("package {name} is not installed (not in composer.lock)"));
        };
        lines.push(format!("{} {}", node.name, node.version));
        if dedupe {
            expanded.insert(node.name.to_ascii_lowercase());
            render_children_deduped(&graph, name, "", &mut lines, &mut expanded, &mut collapsed);
        } else {
            let mut seen = vec![node.name.to_ascii_lowercase()];
            render_children(&graph, name, "", &mut lines, &mut seen);
        }
    } else {
        // Tree rooted at the project: each direct require is a top-level line.
        lines.push(root_name);
        let directs: Vec<(String, String)> = root_requires
            .iter()
            .chain(root_requires_dev.iter())
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        let n = directs.len();
        for (i, (dep, constraint)) in directs.iter().enumerate() {
            let last = i + 1 == n;
            let branch = if last { "└──" } else { "├──" };
            let prefix = if last { "   " } else { "│  " };
            if dedupe {
                let key = dep.to_ascii_lowercase();
                let child = graph.node(dep);
                let has_children = child.is_some_and(|c| !c.requires.is_empty());
                if expanded.contains(&key) && has_children {
                    collapsed = true;
                    lines.push(format!("{branch}{dep} {constraint} (*)"));
                    continue;
                }
                lines.push(format!("{branch}{dep} {constraint}"));
                if child.is_none() {
                    continue;
                }
                expanded.insert(key);
                render_children_deduped(&graph, dep, prefix, &mut lines, &mut expanded, &mut collapsed);
            } else {
                lines.push(format!("{branch}{dep} {constraint}"));
                let mut seen = vec![dep.to_ascii_lowercase()];
                render_children(&graph, dep, prefix, &mut lines, &mut seen);
            }
        }
    }

    if collapsed {
        lines.push("(*) Package tree already displayed".to_string());
    }

    let tree = ShowTree {
        schema_version: 1,
        lines,
    };
    emit(format, &tree)?;
    Ok(ExitCode::SUCCESS)
}

/// Recursively render a node's `require` children with box-drawing
/// prefixes. `seen` guards against dependency cycles. This is the
/// Composer-compatible renderer: shared subtrees are repeated in full,
/// matching `composer show --tree` byte-for-byte.
fn render_children(
    graph: &DependencyGraph,
    name: &str,
    prefix: &str,
    lines: &mut Vec<String>,
    seen: &mut Vec<String>,
) {
    let Some(node) = graph.node(name) else {
        return;
    };
    let edges = &node.requires;
    let n = edges.len();
    for (i, edge) in edges.iter().enumerate() {
        let last = i + 1 == n;
        let branch = if last { "└──" } else { "├──" };
        lines.push(format!("{prefix}{branch}{} {}", edge.to_raw, edge.constraint));
        let key = edge.to.clone();
        if seen.contains(&key) {
            continue; // cycle guard
        }
        seen.push(key);
        let child_prefix = format!("{prefix}{}", if last { "   " } else { "│  " });
        render_children(graph, &edge.to_raw, &child_prefix, lines, seen);
        seen.pop();
    }
}

/// uv/`cargo tree`-style renderer for `bougie tree`: each package's
/// subtree is expanded the first time it is seen and recorded in
/// `expanded`; later occurrences render a single line suffixed with
/// `(*)` (and set `collapsed` so the caller can print the legend). A
/// node is marked into `expanded` *before* descending, so cycles and
/// diamonds alike collapse on their second encounter — no path-local
/// stack, and no combinatorial re-expansion.
fn render_children_deduped(
    graph: &DependencyGraph,
    name: &str,
    prefix: &str,
    lines: &mut Vec<String>,
    expanded: &mut std::collections::HashSet<String>,
    collapsed: &mut bool,
) {
    let Some(node) = graph.node(name) else {
        return;
    };
    let edges = &node.requires;
    let n = edges.len();
    for (i, edge) in edges.iter().enumerate() {
        let last = i + 1 == n;
        let branch = if last { "└──" } else { "├──" };
        let child = graph.node(&edge.to_raw);
        // Only collapse with `(*)` when re-expansion would actually omit
        // children; a leaf (or platform/non-installed edge) is shown in
        // full every time, matching `cargo tree`.
        let has_children = child.is_some_and(|c| !c.requires.is_empty());
        if expanded.contains(&edge.to) && has_children {
            *collapsed = true;
            lines.push(format!("{prefix}{branch}{} {} (*)", edge.to_raw, edge.constraint));
            continue;
        }
        lines.push(format!("{prefix}{branch}{} {}", edge.to_raw, edge.constraint));
        if child.is_none() {
            continue;
        }
        expanded.insert(edge.to.clone());
        let child_prefix = format!("{prefix}{}", if last { "   " } else { "│  " });
        render_children_deduped(graph, &edge.to_raw, &child_prefix, lines, expanded, collapsed);
    }
}

fn json_map(root: &serde_json::Value, key: &str) -> BTreeMap<String, String> {
    root.get(key)
        .and_then(serde_json::Value::as_object)
        .map(|o| {
            o.iter()
                .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                .collect()
        })
        .unwrap_or_default()
}
