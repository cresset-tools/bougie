//! `bougie node {install,uninstall,list,find,dir}` — Node.js toolchain
//! management, mirroring `bougie php`.
//!
//! Node is provisioned from the official nodejs.org distribution via the
//! standalone [`NodejsOrgBackend`] (not the PHP-shaped `Backend` trait —
//! see that module's docs). Each version installs into its own
//! `node-installs/<version>/` tree with `node`/`npm`/`npx` under `bin/`.

use bougie_backend::{NodeRecipe, NodeRequest, NodeVersion, NodejsOrgBackend};
use bougie_cli::OutputFormat;
use bougie_fetch::{DownloadBar, fetch_blob};
use bougie_fs::lock::ExclusiveGuard;
use bougie_output::output::{Render, emit};
use bougie_paths::Paths;
use bougie_platform::target::Triple;
use eyre::{Result, WrapErr, eyre};
use serde::Serialize;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

/// Matches `bougie-installer`'s global-store lock timeout so node and PHP
/// installs serialize against the same lock.
const LOCK_TIMEOUT: Duration = Duration::from_mins(1);

// ---------- install ----------

#[derive(Debug, Serialize)]
pub struct InstallResult {
    pub schema_version: u32,
    pub installed: Vec<InstallEntry>,
}

#[derive(Debug, Serialize)]
pub struct InstallEntry {
    pub version: String,
    pub path: PathBuf,
    pub already_present: bool,
}

impl Render for InstallResult {
    fn render_text(&self, w: &mut dyn Write) -> io::Result<()> {
        for entry in &self.installed {
            let verb = if entry.already_present {
                "already"
            } else {
                "installed"
            };
            writeln!(
                w,
                "{verb} node {} at {}",
                entry.version,
                entry.path.display()
            )?;
        }
        Ok(())
    }
}

pub fn install(format: OutputFormat, request_strs: &[String]) -> Result<ExitCode> {
    let paths = Paths::from_env()?;
    let backend = backend(&paths)?;

    let requests: Vec<NodeRequest> = if request_strs.is_empty() {
        vec![NodeRequest::Latest]
    } else {
        request_strs
            .iter()
            .map(|s| s.parse())
            .collect::<Result<_>>()?
    };

    let mut installed = Vec::with_capacity(requests.len());
    for request in &requests {
        let recipe = backend.resolve(request)?;
        installed.push(install_recipe(&paths, &backend, &recipe)?);
    }

    emit(
        format,
        &InstallResult {
            schema_version: 1,
            installed,
        },
    )?;
    Ok(ExitCode::SUCCESS)
}

/// Fetch + extract one resolved recipe into its version tree, under the
/// global store lock. Idempotent: an already-present version is a no-op.
fn install_recipe(
    paths: &Paths,
    backend: &NodejsOrgBackend,
    recipe: &NodeRecipe,
) -> Result<InstallEntry> {
    let version = recipe.version.to_string();
    // Serialize against the same global lock PHP installs use.
    let _guard = ExclusiveGuard::acquire(&paths.global_lock(), LOCK_TIMEOUT)?;

    let dest = paths.node_install_dir(&version);
    let already_present = dest.exists();
    if !already_present {
        let bar = DownloadBar::new("downloading");
        bar.add_planned(recipe.blob.size);
        bar.set_current(format!("node-{version}"));
        let cache_blobs = paths.cache_blobs();
        let spec = recipe.blob.as_blob_spec(&cache_blobs, &dest);
        fetch_blob(backend.client(), &spec, &bar)
            .wrap_err_with(|| format!("installing node {version}"))?;
        bar.finish();
    }

    Ok(InstallEntry {
        version,
        path: dest,
        already_present,
    })
}

// ---------- uninstall ----------

#[derive(Debug, Serialize)]
pub struct UninstallResult {
    pub schema_version: u32,
    pub removed: Vec<UninstallEntry>,
}

#[derive(Debug, Serialize)]
pub struct UninstallEntry {
    pub version: String,
    pub path: PathBuf,
    pub was_present: bool,
}

impl Render for UninstallResult {
    fn render_text(&self, w: &mut dyn Write) -> io::Result<()> {
        for entry in &self.removed {
            if entry.was_present {
                writeln!(
                    w,
                    "removed node {} ({})",
                    entry.version,
                    entry.path.display()
                )?;
            } else {
                writeln!(w, "node {} was not installed", entry.version)?;
            }
        }
        Ok(())
    }
}

pub fn uninstall(format: OutputFormat, request_strs: &[String]) -> Result<ExitCode> {
    let paths = Paths::from_env()?;
    let mut removed = Vec::with_capacity(request_strs.len());
    for s in request_strs {
        // Uninstall needs an exact version — there's no index lookup to
        // resolve `lts`/`20` against a local set unambiguously.
        let version = parse_exact(s)?;
        let _guard = ExclusiveGuard::acquire(&paths.global_lock(), LOCK_TIMEOUT)?;
        let dir = paths.node_install_dir(&version);
        let was_present = dir.exists();
        if was_present {
            std::fs::remove_dir_all(&dir)
                .wrap_err_with(|| format!("removing {}", dir.display()))?;
        }
        removed.push(UninstallEntry {
            version,
            path: dir,
            was_present,
        });
    }
    emit(
        format,
        &UninstallResult {
            schema_version: 1,
            removed,
        },
    )?;
    Ok(ExitCode::SUCCESS)
}

// ---------- list ----------

#[derive(Debug, Serialize)]
pub struct ListResult {
    pub schema_version: u32,
    pub installed: Vec<String>,
}

impl Render for ListResult {
    fn render_text(&self, w: &mut dyn Write) -> io::Result<()> {
        if self.installed.is_empty() {
            writeln!(
                w,
                "no Node.js versions installed (try `bougie node install lts`)"
            )?;
        } else {
            for v in &self.installed {
                writeln!(w, "{v}")?;
            }
        }
        Ok(())
    }
}

pub fn list(format: OutputFormat) -> Result<ExitCode> {
    let paths = Paths::from_env()?;
    let installed = installed_node_versions(&paths)
        .iter()
        .map(ToString::to_string)
        .collect();
    emit(
        format,
        &ListResult {
            schema_version: 1,
            installed,
        },
    )?;
    Ok(ExitCode::SUCCESS)
}

// ---------- find ----------

#[derive(Debug, Serialize)]
pub struct FindResult {
    pub schema_version: u32,
    pub version: String,
    pub url: String,
    pub installed: bool,
}

impl Render for FindResult {
    fn render_text(&self, w: &mut dyn Write) -> io::Result<()> {
        let tag = if self.installed { " (installed)" } else { "" };
        writeln!(w, "node {}{tag}", self.version)?;
        writeln!(w, "  {}", self.url)?;
        Ok(())
    }
}

pub fn find(format: OutputFormat, request: Option<&str>) -> Result<ExitCode> {
    let paths = Paths::from_env()?;
    let backend = backend(&paths)?;
    let request: NodeRequest = request.unwrap_or("latest").parse()?;
    let recipe = backend.resolve(&request)?;
    let version = recipe.version.to_string();
    let installed = paths.node_install_dir(&version).exists();
    emit(
        format,
        &FindResult {
            schema_version: 1,
            version,
            url: recipe.blob.url,
            installed,
        },
    )?;
    Ok(ExitCode::SUCCESS)
}

// ---------- dir ----------

#[derive(Debug, Serialize)]
pub struct DirResult {
    pub schema_version: u32,
    pub path: PathBuf,
}

impl Render for DirResult {
    fn render_text(&self, w: &mut dyn Write) -> io::Result<()> {
        writeln!(w, "{}", self.path.display())
    }
}

pub fn dir(format: OutputFormat) -> Result<ExitCode> {
    let paths = Paths::from_env()?;
    emit(
        format,
        &DirResult {
            schema_version: 1,
            path: paths.node_installs(),
        },
    )?;
    Ok(ExitCode::SUCCESS)
}

// ---------- shared ----------

fn backend(paths: &Paths) -> Result<NodejsOrgBackend> {
    let target = Triple::detect()?;
    NodejsOrgBackend::new(paths, &target)
}

// ---------- project PATH overlay (used by `bougie run`) ----------

/// A version constraint a project places on its node toolchain, narrow
/// enough to filter the set of installed versions. Node's `engines.node`
/// can express richer ranges than this; we model the shapes that matter
/// for "pick the best installed match" and treat anything fancier as
/// [`VersionFilter::Any`] (use the highest installed).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum VersionFilter {
    /// No usable constraint — any installed version is eligible.
    Any,
    /// `>=18` / `>18` style — any version at or above this major.
    AtLeastMajor(u32),
    /// `20` / `^20` / `20.x` — pinned to one major line.
    Major(u32),
    /// `20.11` / `~20.11` — pinned to one minor line.
    MajorMinor(u32, u32),
    /// `20.11.0` — one exact release.
    Exact(NodeVersion),
}

impl VersionFilter {
    fn matches(self, v: NodeVersion) -> bool {
        match self {
            Self::Any => true,
            Self::AtLeastMajor(m) => v.major >= m,
            Self::Major(m) => v.major == m,
            Self::MajorMinor(m, n) => v.major == m && v.minor == n,
            Self::Exact(want) => v == want,
        }
    }
}

/// What a project asks of node, discovered from its files. `None` from
/// [`detect_project_node`] means the project shows no sign of using node,
/// so `bougie run` leaves PATH untouched.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ProjectNode {
    filter: VersionFilter,
}

/// Detect whether (and which) node a project wants. Priority:
/// 1. `.nvmrc` / `.node-version` — the precise, ecosystem-standard pin.
/// 2. `package.json` `engines.node` — a range; parsed best-effort.
/// 3. bare `package.json` presence — node is used, no version constraint.
/// 4. a Composer dependency that requires a node build (e.g. Magento +
///    Hyvä's `hyva-themes/*`) — node is needed even with no root
///    `package.json` (the build lives in a theme subdir). No version pin.
///
/// Returns `None` only when none of those signals exist, so non-node PHP
/// projects keep an untouched PATH. (A `bougie.toml [node]` pin is a
/// planned future signal — see `NODE_PLAN.md`.)
fn detect_project_node(project_root: &Path) -> Option<ProjectNode> {
    if let Some(filter) = read_version_file(project_root) {
        return Some(ProjectNode { filter });
    }
    let pkg = project_root.join("package.json");
    if pkg.is_file() {
        let filter = std::fs::read_to_string(&pkg)
            .ok()
            .and_then(|t| engines_node_filter(&t))
            .unwrap_or(VersionFilter::Any);
        return Some(ProjectNode { filter });
    }
    if composer_requires_node_build(project_root) {
        return Some(ProjectNode {
            filter: VersionFilter::Any,
        });
    }
    None
}

/// Composer packages whose vendor prefix implies a node-driven frontend
/// build. The flagship case is Magento + Hyvä: `hyva-themes/*` themes
/// build their Tailwind CSS via npm, and the project has no root
/// `package.json` — the build lives in `app/design/frontend/.../web/tailwind/`.
const NODE_BUILD_PACKAGE_PREFIXES: &[&str] = &["hyva-themes/"];

/// Individually-named Composer packages (no useful vendor prefix) that
/// pull in a node toolchain — e.g. Snowdog's gulp-based Magento frontend
/// tooling.
const NODE_BUILD_PACKAGES: &[&str] = &["snowdog/frontools"];

fn is_node_build_package(name: &str) -> bool {
    NODE_BUILD_PACKAGE_PREFIXES
        .iter()
        .any(|p| name.starts_with(p))
        || NODE_BUILD_PACKAGES.contains(&name)
}

/// Does the project's Composer setup pull in a package that needs a node
/// build? Checks `composer.json`'s direct `require`/`require-dev` (cheap,
/// precise — Hyvä is normally a direct require), then falls back to a raw
/// substring scan of `composer.lock` to catch a transitively-pulled or
/// metapackage-bundled dependency without JSON-parsing a multi-MB lock.
fn composer_requires_node_build(project_root: &Path) -> bool {
    if let Ok(text) = std::fs::read_to_string(project_root.join("composer.json"))
        && let Ok(v) = serde_json::from_str::<serde_json::Value>(&text)
    {
        for section in ["require", "require-dev"] {
            if let Some(obj) = v.get(section).and_then(serde_json::Value::as_object)
                && obj.keys().any(|k| is_node_build_package(k))
            {
                return true;
            }
        }
    }
    if let Ok(lock) = std::fs::read_to_string(project_root.join("composer.lock")) {
        // composer.lock records names as `"name": "<vendor>/<pkg>"`. A
        // raw `contains` on the vendor prefix / package name is reliable
        // (these strings only appear as package identifiers) and avoids
        // parsing the whole lock on every `bougie run`.
        let hit = NODE_BUILD_PACKAGE_PREFIXES
            .iter()
            .chain(NODE_BUILD_PACKAGES.iter())
            .any(|needle| lock.contains(needle));
        if hit {
            return true;
        }
    }
    false
}

/// Read `.nvmrc` / `.node-version` (in that order) into a filter. These
/// hold a bare version (`20`, `20.11.0`, `v20`) or an alias (`lts/*`,
/// `lts/iron`, `latest`). Aliases that need the live index to resolve a
/// codename map to [`VersionFilter::Any`] — node is used, no precise pin
/// we can apply offline.
fn read_version_file(project_root: &Path) -> Option<VersionFilter> {
    for name in [".nvmrc", ".node-version"] {
        let path = project_root.join(name);
        let Ok(raw) = std::fs::read_to_string(&path) else {
            continue;
        };
        let s = raw.trim();
        if s.is_empty() {
            continue;
        }
        let lower = s.to_ascii_lowercase();
        if lower == "latest" || lower.starts_with("lts") || lower == "node" {
            return Some(VersionFilter::Any);
        }
        return Some(parse_version_filter(s));
    }
    None
}

/// Parse a bare `<major>[.<minor>[.<patch>]]` (with optional leading `v`)
/// into the tightest filter it supports. Unparseable → [`VersionFilter::Any`].
fn parse_version_filter(s: &str) -> VersionFilter {
    let body = s.trim().strip_prefix(['v', 'V']).unwrap_or(s.trim());
    let nums: Vec<Option<u32>> = body.split('.').map(|p| p.parse().ok()).collect();
    match nums.as_slice() {
        [Some(maj)] => VersionFilter::Major(*maj),
        [Some(maj), Some(min)] => VersionFilter::MajorMinor(*maj, *min),
        [Some(maj), Some(min), Some(pat)] => VersionFilter::Exact(NodeVersion {
            major: *maj,
            minor: *min,
            patch: *pat,
        }),
        _ => VersionFilter::Any,
    }
}

/// Extract a coarse filter from `package.json`'s `engines.node` range.
/// Best-effort: `>=`/`>` yield [`VersionFilter::AtLeastMajor`]; a bare or
/// `^`/`~`/`=` prefixed version yields the major/minor it names; anything
/// with no leading integer (`*`, `||` unions we don't model) → `Any`.
fn engines_node_filter(package_json: &str) -> Option<VersionFilter> {
    let v: serde_json::Value = serde_json::from_str(package_json).ok()?;
    let range = v.get("engines")?.get("node")?.as_str()?.trim();
    if range.is_empty() || range == "*" {
        return Some(VersionFilter::Any);
    }
    let at_least = range.starts_with(">=") || range.starts_with('>');
    // Strip a single leading comparator/caret/tilde, then read the first
    // numeric segment(s) up to the next non-version character.
    let rest = range.trim_start_matches(['>', '=', '<', '^', '~', 'v', 'V', ' ']);
    let token: String = rest
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '.')
        .collect();
    // `20.x` collects as `20.`; drop the trailing separator so it reads
    // as `Major(20)` rather than an unparseable two-segment token.
    let token = token.trim_end_matches('.');
    if token.is_empty() {
        return Some(VersionFilter::Any);
    }
    if at_least {
        let major = token.split('.').next()?.parse().ok()?;
        return Some(VersionFilter::AtLeastMajor(major));
    }
    Some(parse_version_filter(token))
}

/// Resolve the project's declared node to a `bin/` directory to prepend
/// onto `PATH`, or `None` when the project doesn't use node. When node
/// *is* wanted but no matching version is installed, prints a one-line
/// hint to stderr and returns `None` (the run still proceeds — not every
/// command in a node-using project actually needs node).
pub fn project_bin_dir(project_root: &Path, paths: &Paths) -> Option<PathBuf> {
    let need = detect_project_node(project_root)?;
    let installed = installed_node_versions(paths);
    let chosen = installed
        .iter()
        .copied()
        .filter(|v| need.filter.matches(*v))
        .max();
    let Some(v) = chosen else {
        eprintln!(
            "bougie: this project wants Node.js{} but no matching version is installed — \
             run `bougie node install{}`",
            describe_filter(need.filter),
            install_hint(need.filter),
        );
        return None;
    };
    let bin = paths.node_install_dir(&v.to_string()).join("bin");
    bin.is_dir().then_some(bin)
}

fn describe_filter(f: VersionFilter) -> String {
    match f {
        VersionFilter::Any => String::new(),
        VersionFilter::AtLeastMajor(m) => format!(" (>={m})"),
        VersionFilter::Major(m) => format!(" ({m})"),
        VersionFilter::MajorMinor(m, n) => format!(" ({m}.{n})"),
        VersionFilter::Exact(v) => format!(" ({v})"),
    }
}

fn install_hint(f: VersionFilter) -> String {
    match f {
        VersionFilter::Any => " lts".into(),
        VersionFilter::AtLeastMajor(m) | VersionFilter::Major(m) => format!(" {m}"),
        VersionFilter::MajorMinor(m, n) => format!(" {m}.{n}"),
        VersionFilter::Exact(v) => format!(" {v}"),
    }
}

/// Typed sibling of [`installed_versions`] returning parsed versions
/// (newest-first) for selection logic.
fn installed_node_versions(paths: &Paths) -> Vec<NodeVersion> {
    let dir = paths.node_installs();
    let mut versions: Vec<NodeVersion> = match std::fs::read_dir(&dir) {
        Ok(entries) => entries
            .flatten()
            .filter_map(|e| e.file_name().to_str().and_then(|n| n.parse().ok()))
            .collect(),
        Err(_) => Vec::new(),
    };
    versions.sort_unstable_by(|a, b| b.cmp(a));
    versions
}

/// `uninstall` resolves only exact `major.minor.patch` (the on-disk dir
/// name) — a fuzzy `20`/`lts` could ambiguously match several installed
/// trees, so we require the user to name the exact version.
fn parse_exact(s: &str) -> Result<String> {
    let v: bougie_backend::NodeVersion = s.parse().map_err(|_| {
        eyre!(
            "`bougie node uninstall` needs an exact version (e.g. `20.11.0`); \
             got `{s}`. Run `bougie node list` to see installed versions."
        )
    })?;
    Ok(v.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_exact_requires_full_version() {
        assert_eq!(parse_exact("20.11.0").unwrap(), "20.11.0");
        assert_eq!(parse_exact("v20.11.0").unwrap(), "20.11.0");
        assert!(parse_exact("20").is_err());
        assert!(parse_exact("lts").is_err());
    }

    fn paths_with_versions(names: &[&str]) -> (tempfile::TempDir, Paths) {
        let td = tempfile::TempDir::new().unwrap();
        let paths = Paths::new(td.path().into(), td.path().join("cache"));
        let base = paths.node_installs();
        for name in names {
            std::fs::create_dir_all(base.join(name)).unwrap();
        }
        (td, paths)
    }

    fn v(s: &str) -> NodeVersion {
        s.parse().unwrap()
    }

    #[test]
    fn installed_versions_sorts_newest_first_and_skips_junk() {
        let (_td, paths) = paths_with_versions(&["20.11.0", "18.20.3", "22.3.0", "not-a-version"]);
        let got: Vec<String> = installed_node_versions(&paths)
            .iter()
            .map(ToString::to_string)
            .collect();
        assert_eq!(got, vec!["22.3.0", "20.11.0", "18.20.3"]);
    }

    #[test]
    fn installed_versions_empty_when_no_tree() {
        let td = tempfile::TempDir::new().unwrap();
        let paths = Paths::new(td.path().into(), td.path().join("cache"));
        assert!(installed_node_versions(&paths).is_empty());
    }

    #[test]
    fn version_filter_matches_expected_shapes() {
        assert!(VersionFilter::Any.matches(v("18.0.0")));
        assert!(VersionFilter::AtLeastMajor(18).matches(v("20.1.0")));
        assert!(!VersionFilter::AtLeastMajor(20).matches(v("18.9.9")));
        assert!(VersionFilter::Major(20).matches(v("20.99.0")));
        assert!(!VersionFilter::Major(20).matches(v("22.0.0")));
        assert!(VersionFilter::MajorMinor(20, 11).matches(v("20.11.5")));
        assert!(!VersionFilter::MajorMinor(20, 11).matches(v("20.12.0")));
        assert!(VersionFilter::Exact(v("20.11.0")).matches(v("20.11.0")));
    }

    #[test]
    fn parse_version_filter_tightens_to_segments() {
        assert_eq!(parse_version_filter("20"), VersionFilter::Major(20));
        assert_eq!(parse_version_filter("v20"), VersionFilter::Major(20));
        assert_eq!(
            parse_version_filter("20.11"),
            VersionFilter::MajorMinor(20, 11)
        );
        assert_eq!(
            parse_version_filter("20.11.0"),
            VersionFilter::Exact(v("20.11.0"))
        );
        assert_eq!(parse_version_filter("garbage"), VersionFilter::Any);
    }

    #[test]
    fn engines_node_filter_parses_common_ranges() {
        let f =
            |range: &str| engines_node_filter(&format!(r#"{{"engines":{{"node":"{range}"}}}}"#));
        assert_eq!(f(">=18"), Some(VersionFilter::AtLeastMajor(18)));
        assert_eq!(f(">=18.17.0"), Some(VersionFilter::AtLeastMajor(18)));
        assert_eq!(f("^20.10.0"), Some(VersionFilter::Exact(v("20.10.0"))));
        assert_eq!(f("~20.11"), Some(VersionFilter::MajorMinor(20, 11)));
        assert_eq!(f("20.x"), Some(VersionFilter::Major(20)));
        assert_eq!(f("*"), Some(VersionFilter::Any));
        // No engines.node key at all → None (caller falls back to Any).
        assert_eq!(engines_node_filter(r#"{"name":"x"}"#), None);
    }

    #[test]
    fn detect_prefers_version_file_then_package_json() {
        // .nvmrc wins over package.json engines.
        let td = tempfile::TempDir::new().unwrap();
        let root = td.path();
        std::fs::write(root.join(".nvmrc"), "20.11.0\n").unwrap();
        std::fs::write(root.join("package.json"), r#"{"engines":{"node":">=18"}}"#).unwrap();
        assert_eq!(
            detect_project_node(root),
            Some(ProjectNode {
                filter: VersionFilter::Exact(v("20.11.0"))
            })
        );
    }

    #[test]
    fn detect_bare_package_json_is_any() {
        let td = tempfile::TempDir::new().unwrap();
        std::fs::write(td.path().join("package.json"), "{}").unwrap();
        assert_eq!(
            detect_project_node(td.path()),
            Some(ProjectNode {
                filter: VersionFilter::Any
            })
        );
    }

    #[test]
    fn detect_none_for_non_node_project() {
        let td = tempfile::TempDir::new().unwrap();
        std::fs::write(td.path().join("composer.json"), "{}").unwrap();
        assert_eq!(detect_project_node(td.path()), None);
    }

    #[test]
    fn detect_hyva_via_composer_require() {
        // Magento + Hyvä: no root package.json, but the theme is a
        // direct composer require → node is needed.
        let td = tempfile::TempDir::new().unwrap();
        std::fs::write(
            td.path().join("composer.json"),
            r#"{"require":{"php":"~8.3.0","hyva-themes/magento2-default-theme":"^1.3"}}"#,
        )
        .unwrap();
        assert_eq!(
            detect_project_node(td.path()),
            Some(ProjectNode {
                filter: VersionFilter::Any
            })
        );
    }

    #[test]
    fn detect_hyva_via_composer_lock_transitive() {
        // Hyvä pulled in transitively (only present in the lock) is still
        // caught by the raw lock scan.
        let td = tempfile::TempDir::new().unwrap();
        std::fs::write(
            td.path().join("composer.json"),
            r#"{"require":{"php":"~8.3.0"}}"#,
        )
        .unwrap();
        std::fs::write(
            td.path().join("composer.lock"),
            r#"{"packages":[{"name":"hyva-themes/magento2-theme-module","version":"1.1.0"}]}"#,
        )
        .unwrap();
        assert!(composer_requires_node_build(td.path()));
        assert_eq!(
            detect_project_node(td.path()),
            Some(ProjectNode {
                filter: VersionFilter::Any
            })
        );
    }

    #[test]
    fn detect_plain_magento_is_not_node() {
        // A Magento project with no node-build dependency (headless /
        // Hyvä-less) must NOT trigger the node overlay.
        let td = tempfile::TempDir::new().unwrap();
        std::fs::write(
            td.path().join("composer.json"),
            r#"{"require":{"php":"~8.3.0","magento/product-community-edition":"2.4.7"}}"#,
        )
        .unwrap();
        std::fs::write(
            td.path().join("composer.lock"),
            r#"{"packages":[{"name":"magento/magento2-base","version":"2.4.7"}]}"#,
        )
        .unwrap();
        assert_eq!(detect_project_node(td.path()), None);
    }

    #[test]
    fn project_bin_dir_picks_highest_matching_installed() {
        let (_td, paths) = paths_with_versions(&["18.20.3", "20.11.0", "20.14.0"]);
        // Materialize the bin dirs so the `is_dir` check passes.
        for ver in ["18.20.3", "20.11.0", "20.14.0"] {
            std::fs::create_dir_all(paths.node_install_dir(ver).join("bin")).unwrap();
        }
        let proj = tempfile::TempDir::new().unwrap();
        std::fs::write(proj.path().join(".nvmrc"), "20").unwrap();
        let bin = project_bin_dir(proj.path(), &paths).unwrap();
        assert_eq!(bin, paths.node_install_dir("20.14.0").join("bin"));
    }

    #[test]
    fn project_bin_dir_none_when_no_node_used() {
        let (_td, paths) = paths_with_versions(&["20.14.0"]);
        let proj = tempfile::TempDir::new().unwrap();
        std::fs::write(proj.path().join("composer.json"), "{}").unwrap();
        assert!(project_bin_dir(proj.path(), &paths).is_none());
    }
}
