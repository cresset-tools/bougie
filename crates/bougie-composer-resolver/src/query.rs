//! Read-only query engine over a resolved `composer.lock`.
//!
//! This is the shared substrate for bougie's Composer inspection
//! subcommands — `show`, `why`, `why-not`, `outdated`, `licenses`,
//! `fund` — and the future top-level `tree` / `outdated` verbs. Every
//! type here is read-only: it consumes a parsed [`Lock`] (and, for
//! [`latest_versions`], the metadata fetcher) and never mutates project
//! state.
//!
//! Two capabilities live here:
//!
//! - [`DependencyGraph`] — forward + reverse adjacency over the locked
//!   set, with `provide`/`replace` resolved so a package that satisfies
//!   a virtual name is reachable under that name. Powers `show --tree`,
//!   `why` (reverse edges), `why-not` (conflict edges), and `tree`.
//! - [`latest_versions`] — wraps the resolver's existing Packagist
//!   metadata fetcher to return every published version of a set of
//!   packages, so `outdated` / `show --latest` can compare the locked
//!   version against what's available.
//!
//! License + funding extraction is trivial field access on
//! [`LockPackage`] (`license`, `funding`); thin helpers
//! ([`licenses`], [`funding`]) live here so callers have one import.

use std::collections::BTreeMap;
use std::path::Path;

use bougie_composer::lockfile::{Lock, LockFunding, LockPackage};
use bougie_composer::metadata::PackageMetadata;
use bougie_paths::Paths;
use eyre::{eyre, Result};
use serde_json::Value;

use crate::hash::FxHashMap;
use crate::metadata::{
    build_client, fetch_package_metadata_optional, fetch_package_metadata_v1_optional,
    load_v1_provider_table, probe_protocol, Repo, RepoProtocol, Variant,
};

/// A repository with its probed protocol and (for v1) preloaded
/// provider table, ready for per-package metadata fetches.
type ProbedRepo = (Repo, RepoProtocol, Option<FxHashMap<String, String>>);

/// Lowercase a `vendor/name` for case-insensitive lookups. Composer
/// treats package names case-insensitively on resolve but preserves the
/// declared casing in the lockfile; every map key here is the lowered
/// form so a `why Monolog/Monolog` query still finds `monolog/monolog`.
fn canon(name: &str) -> String {
    name.to_ascii_lowercase()
}

/// Which dependency section a package was locked under. Mirrors
/// Composer's `packages` vs `packages-dev` split so `--no-dev` filters
/// and `show`'s dev annotations are exact.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum Section {
    /// From `composer.lock` `packages`.
    Runtime,
    /// From `composer.lock` `packages-dev`.
    Dev,
}

/// One edge in the dependency graph: a `require` (or root-require) entry
/// pointing at another package by the name *as written* in the source
/// package's `require` map, plus the constraint string.
#[derive(Debug, Clone)]
pub struct Edge {
    /// The required package name, lowercased. May be a virtual name
    /// (something `provide`d/`replace`d rather than installed directly);
    /// resolve it through [`DependencyGraph::providers_of`].
    pub to: String,
    /// The required package name exactly as written in the requiring
    /// package's `require` map (case preserved).
    pub to_raw: String,
    /// The version constraint string, verbatim.
    pub constraint: String,
}

/// A node in the graph: one locked package plus its outgoing edges.
#[derive(Debug, Clone)]
pub struct Node {
    /// Package name, case as written in the lockfile.
    pub name: String,
    /// Selected version (pretty form, e.g. `3.5.0`, `dev-main`).
    pub version: String,
    pub section: Section,
    /// Outgoing `require` edges (not `require-dev` — Composer ignores
    /// transitive dev requires, and the lockfile rarely records them).
    pub requires: Vec<Edge>,
}

/// Forward + reverse dependency adjacency over a locked package set,
/// with `provide`/`replace` virtual-name resolution.
///
/// Build with [`DependencyGraph::from_lock`]. The root package (the
/// project itself) is not present in `composer.lock`; pass its direct
/// requires via [`DependencyGraph::with_root`] when a query needs to
/// attribute a dependency to the root (e.g. `why` on a direct dep, or
/// `show --tree` rooted at the project).
#[derive(Debug, Default)]
pub struct DependencyGraph {
    /// canonical name -> node index in `nodes`.
    index: FxHashMap<String, usize>,
    nodes: Vec<Node>,
    /// canonical virtual name -> node indices that `provide`/`replace`
    /// it. A package always "provides" its own name.
    providers: FxHashMap<String, Vec<usize>>,
    /// Reverse adjacency: canonical name -> indices of nodes that
    /// require it (directly, by the name they wrote — resolved through
    /// providers so a requirer of a virtual name shows up under the
    /// providing package too).
    dependents: FxHashMap<String, Vec<usize>>,
    /// The root project's direct requires, if supplied. Indexed
    /// separately because the root has no node in the locked set.
    root: Option<RootNode>,
}

/// The root project, which `composer.lock` does not contain. Carries
/// only what the graph queries need: its direct require edges.
#[derive(Debug, Clone)]
pub struct RootNode {
    pub name: String,
    pub requires: Vec<Edge>,
    pub requires_dev: Vec<Edge>,
}

impl DependencyGraph {
    /// Build a graph from a parsed lockfile. Both `packages` and
    /// `packages-dev` become nodes; `provide` and `replace` register
    /// virtual names so a requirer of e.g. `psr/log-implementation`
    /// resolves to the package that provides it.
    pub fn from_lock(lock: &Lock) -> Self {
        let mut g = DependencyGraph::default();
        for pkg in &lock.packages {
            g.add_node(pkg, Section::Runtime);
        }
        for pkg in &lock.packages_dev {
            g.add_node(pkg, Section::Dev);
        }
        g.build_reverse();
        g
    }

    /// Attach the root project's direct requires. `requires` /
    /// `requires_dev` are the raw `composer.json` `require` /
    /// `require-dev` maps (name → constraint). Edges to platform
    /// packages (php, ext-*, lib-*) are kept; callers filter if they
    /// only want installed packages.
    #[must_use]
    pub fn with_root(
        mut self,
        name: impl Into<String>,
        requires: &BTreeMap<String, String>,
        requires_dev: &BTreeMap<String, String>,
    ) -> Self {
        let to_edges = |m: &BTreeMap<String, String>| {
            m.iter()
                .map(|(k, v)| Edge {
                    to: canon(k),
                    to_raw: k.clone(),
                    constraint: v.clone(),
                })
                .collect::<Vec<_>>()
        };
        self.root = Some(RootNode {
            name: name.into(),
            requires: to_edges(requires),
            requires_dev: to_edges(requires_dev),
        });
        self
    }

    fn add_node(&mut self, pkg: &LockPackage, section: Section) {
        let idx = self.nodes.len();
        let requires = pkg
            .require
            .iter()
            .map(|(k, v)| Edge {
                to: canon(k),
                to_raw: k.clone(),
                constraint: v.clone(),
            })
            .collect();
        self.nodes.push(Node {
            name: pkg.name.clone(),
            version: pkg.version.clone(),
            section,
            requires,
        });
        let key = canon(&pkg.name);
        self.index.insert(key.clone(), idx);
        // A package provides its own name plus everything it
        // `provide`s or `replace`s.
        self.providers.entry(key).or_default().push(idx);
        for virt in pkg.provide.keys().chain(pkg.replace.keys()) {
            self.providers.entry(canon(virt)).or_default().push(idx);
        }
    }

    /// Resolve a (canonical) required name to the node indices that
    /// satisfy it: the package itself if installed under that name, plus
    /// any packages that `provide`/`replace` it.
    fn resolve_targets(&self, canonical: &str) -> Vec<usize> {
        self.providers.get(canonical).cloned().unwrap_or_default()
    }

    fn build_reverse(&mut self) {
        // Collect first to avoid borrow conflicts with the mutation.
        let mut edges: Vec<(String, usize)> = Vec::new();
        for (idx, node) in self.nodes.iter().enumerate() {
            for e in &node.requires {
                for target in self.resolve_targets(&e.to) {
                    edges.push((self.nodes[target].name.to_ascii_lowercase(), idx));
                }
                // Even if nothing provides it (platform package, or a
                // not-installed virtual), register the key so lookups
                // return an empty list rather than `None`.
                edges.push((e.to.clone(), idx));
            }
        }
        for (key, requirer) in edges {
            let entry = self.dependents.entry(key).or_default();
            // The plain `e.to` registration above can duplicate a
            // resolved-target registration; keep it a set.
            if !entry.contains(&requirer) {
                entry.push(requirer);
            }
        }
    }

    /// Look up a node by name (case-insensitive). Returns `None` for
    /// platform packages, the root, or names not in the locked set.
    pub fn node(&self, name: &str) -> Option<&Node> {
        self.index.get(&canon(name)).map(|&i| &self.nodes[i])
    }

    /// All locked nodes, in lockfile order (`packages` then
    /// `packages-dev`).
    pub fn nodes(&self) -> &[Node] {
        &self.nodes
    }

    /// The root project node, if [`with_root`](Self::with_root) was
    /// called.
    pub fn root(&self) -> Option<&RootNode> {
        self.root.as_ref()
    }

    /// Packages that satisfy `name` (the package itself plus any
    /// `provide`/`replace` providers). Empty for platform/unknown names.
    pub fn providers_of(&self, name: &str) -> Vec<&Node> {
        self.resolve_targets(&canon(name))
            .into_iter()
            .map(|i| &self.nodes[i])
            .collect()
    }

    /// Packages that directly require `name` (reverse edges). The result
    /// pairs each dependent node with the constraint it imposes. Powers
    /// `composer why` / `depends`. The root project, if present and a
    /// direct requirer, is reported via [`root_requires`](Self::root_requires).
    pub fn dependents_of(&self, name: &str) -> Vec<(&Node, &str)> {
        let key = canon(name);
        let Some(idxs) = self.dependents.get(&key) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for &i in idxs {
            let node = &self.nodes[i];
            // Find the constraint this node imposes on `name` (matching
            // either the direct name or a virtual it resolves to).
            let constraint = node
                .requires
                .iter()
                .find(|e| e.to == key || self.resolves_to(&e.to, &key))
                .map_or("*", |e| e.constraint.as_str());
            out.push((node, constraint));
        }
        out
    }

    /// Whether the root project directly requires `name`, and under what
    /// constraint. `None` if no root attached or it doesn't require it.
    pub fn root_requires(&self, name: &str) -> Option<&str> {
        let key = canon(name);
        let root = self.root.as_ref()?;
        root.requires
            .iter()
            .chain(root.requires_dev.iter())
            .find(|e| e.to == key)
            .map(|e| e.constraint.as_str())
    }

    /// Whether the edge target `edge_to` resolves (through
    /// `provide`/`replace`) to the package named `target_canon`.
    fn resolves_to(&self, edge_to: &str, target_canon: &str) -> bool {
        self.resolve_targets(edge_to)
            .into_iter()
            .any(|i| canon(&self.nodes[i].name) == target_canon)
    }
}

/// Extract the SPDX license identifier(s) for a locked package.
/// Convenience over the [`LockPackage::license`] field so `composer
/// licenses` has one import; returns `["none"]`-style fallback handling
/// to the caller (empty vec means "not declared").
pub fn licenses(pkg: &LockPackage) -> &[String] {
    &pkg.license
}

/// Funding entries for a locked package, in declared order. Empty when
/// the package declares no funding.
pub fn funding(pkg: &LockPackage) -> &[LockFunding] {
    &pkg.funding
}

/// Every published version of each requested package, newest-first as
/// Packagist serves them, keyed by the *queried* name (lowercased).
///
/// Reuses the resolver's existing blocking metadata fetcher and on-disk
/// cache — no new network stack. Repositories and auth are read from
/// the project's `composer.json` exactly as a resolve would, so private
/// mirrors and `repositories` overrides are honored. A package not found
/// in any configured repo is simply absent from the returned map (not an
/// error) — `outdated` reports such packages as "could not determine
/// latest".
///
/// Versions are the pretty strings (`3.5.0`, `1.2.0-RC1`, `dev-main`);
/// the caller filters by stability / constraint via `composer-semver`.
pub fn latest_versions(
    paths: &Paths,
    project_root: &Path,
    names: &[String],
    include_dev: bool,
) -> Result<FxHashMap<String, Vec<String>>> {
    let composer_json_path = project_root.join("composer.json");
    let composer_json_bytes = std::fs::read(&composer_json_path)
        .map_err(|e| eyre!("reading {}: {e}", composer_json_path.display()))?;
    let composer_json: Value = serde_json::from_slice(&composer_json_bytes)
        .map_err(|e| eyre!("parsing composer.json: {e}"))?;

    let auth = crate::update::read_all_auth(&composer_json, project_root).map_err(|e| eyre!(e))?;
    let repos = crate::update::read_repositories(&composer_json, Repo::packagist(), &auth)
        .map_err(|e| eyre!(e))?;
    let client = build_client()?;

    // Probe each repo's protocol once; v1 repos also need their provider
    // table loaded up front.
    let mut probed: Vec<ProbedRepo> = Vec::new();
    for repo in repos {
        let (protocol, dist_mirrors) =
            probe_protocol(&client, &repo).unwrap_or((RepoProtocol::V2, Vec::new()));
        let table = match &protocol {
            RepoProtocol::V1(disc) => Some(load_v1_provider_table(&client, paths, &repo, disc)?),
            RepoProtocol::V2 => None,
        };
        probed.push((
            repo.clone()
                .with_protocol(Some(protocol.clone()))
                .with_dist_mirrors(dist_mirrors),
            protocol,
            table,
        ));
    }

    let mut out: FxHashMap<String, Vec<String>> = FxHashMap::default();
    for name in names {
        let key = canon(name);
        // First repo that carries the package wins (Composer repository
        // priority order). Aggregate that repo's stable + dev versions.
        for (repo, protocol, table) in &probed {
            let mut versions = fetch_versions(&client, paths, repo, protocol, table.as_ref(), name)?;
            if include_dev
                && matches!(protocol, RepoProtocol::V2)
                && let Some(md) =
                    fetch_package_metadata_optional(&client, paths, repo, name, Variant::Dev)?
            {
                push_versions(&mut versions, &md, name);
            }
            if !versions.is_empty() {
                out.insert(key.clone(), versions);
                break;
            }
        }
    }
    Ok(out)
}

/// Fetch the stable version list for one package from one repo,
/// dispatching on its probed protocol. Returns an empty vec when the
/// repo doesn't carry the package.
fn fetch_versions(
    client: &reqwest::blocking::Client,
    paths: &Paths,
    repo: &Repo,
    protocol: &RepoProtocol,
    table: Option<&FxHashMap<String, String>>,
    name: &str,
) -> Result<Vec<String>> {
    let mut versions = Vec::new();
    let md = match protocol {
        RepoProtocol::V2 => {
            fetch_package_metadata_optional(client, paths, repo, name, Variant::Stable)?
        }
        RepoProtocol::V1(disc) => {
            // `table` is always Some for a V1 protocol (built in
            // `latest_versions`); guard defensively.
            let Some(table) = table else { return Ok(versions) };
            fetch_package_metadata_v1_optional(client, paths, repo, disc, table, name)?
        }
    };
    if let Some(md) = md {
        push_versions(&mut versions, &md, name);
    }
    Ok(versions)
}

/// Append every version string for `name` from a parsed metadata
/// document into `out`.
fn push_versions(out: &mut Vec<String>, md: &PackageMetadata, name: &str) {
    let key = canon(name);
    for (pkg_name, list) in &md.packages {
        if canon(pkg_name) == key {
            out.extend(list.iter().map(|p| p.version.clone()));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn lock(value: serde_json::Value) -> Lock {
        serde_json::from_value(value).unwrap()
    }

    fn names(nodes: Vec<&Node>) -> Vec<String> {
        let mut v: Vec<String> = nodes.into_iter().map(|n| n.name.clone()).collect();
        v.sort();
        v
    }

    #[test]
    fn forward_and_reverse_edges() {
        let l = lock(json!({
            "packages": [
                {"name": "acme/app", "version": "1.0.0", "require": {"acme/lib": "^2.0", "php": ">=8.1"}},
                {"name": "acme/lib", "version": "2.3.0", "require": {"psr/log": "^3.0"}},
                {"name": "psr/log", "version": "3.0.0"}
            ]
        }));
        let g = DependencyGraph::from_lock(&l);

        // Forward: acme/app requires acme/lib + php.
        let app = g.node("acme/app").unwrap();
        assert_eq!(app.requires.len(), 2);

        // Reverse: who depends on psr/log? acme/lib (case-insensitive lookup).
        let deps = g.dependents_of("PSR/Log");
        assert_eq!(names(deps.iter().map(|(n, _)| *n).collect()), vec!["acme/lib"]);
        assert_eq!(deps[0].1, "^3.0");

        // Platform packages aren't nodes but resolve to empty dependents.
        assert!(g.node("php").is_none());
        assert_eq!(g.dependents_of("php").len(), 1); // acme/app requires php
    }

    #[test]
    fn provide_replace_resolution() {
        // acme/app requires the virtual `psr/log-implementation`, which
        // monolog/monolog provides. `why` on monolog should attribute
        // acme/app through the virtual edge.
        let l = lock(json!({
            "packages": [
                {"name": "acme/app", "version": "1.0.0", "require": {"psr/log-implementation": "^3.0"}},
                {"name": "monolog/monolog", "version": "3.5.0",
                 "provide": {"psr/log-implementation": "3.0.0"}}
            ]
        }));
        let g = DependencyGraph::from_lock(&l);

        // monolog provides the virtual name.
        let provs = g.providers_of("psr/log-implementation");
        assert_eq!(names(provs), vec!["monolog/monolog"]);

        // Reverse edge reaches monolog via the virtual.
        let deps = g.dependents_of("monolog/monolog");
        assert_eq!(names(deps.iter().map(|(n, _)| *n).collect()), vec!["acme/app"]);
        assert_eq!(deps[0].1, "^3.0");
    }

    #[test]
    fn dev_section_and_root() {
        let l = lock(json!({
            "packages": [{"name": "acme/lib", "version": "1.0.0"}],
            "packages-dev": [{"name": "phpunit/phpunit", "version": "10.5.0"}]
        }));
        let g = DependencyGraph::from_lock(&l).with_root(
            "acme/app",
            &BTreeMap::from([("acme/lib".into(), "^1.0".into())]),
            &BTreeMap::from([("phpunit/phpunit".into(), "^10.5".into())]),
        );

        assert_eq!(g.node("acme/lib").unwrap().section, Section::Runtime);
        assert_eq!(g.node("phpunit/phpunit").unwrap().section, Section::Dev);
        assert_eq!(g.root_requires("acme/lib"), Some("^1.0"));
        assert_eq!(g.root_requires("phpunit/phpunit"), Some("^10.5"));
        assert_eq!(g.root_requires("psr/log"), None);
    }

    #[test]
    fn license_and_funding_extraction() {
        let l = lock(json!({
            "packages": [{
                "name": "acme/lib", "version": "1.0.0",
                "license": ["MIT", "Apache-2.0"],
                "funding": [{"type": "github", "url": "https://github.com/sponsors/acme"}]
            }]
        }));
        let pkg = &l.packages[0];
        assert_eq!(licenses(pkg), &["MIT", "Apache-2.0"]);
        let f = funding(pkg);
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].kind, "github");
        assert_eq!(f[0].url, "https://github.com/sponsors/acme");
    }
}
