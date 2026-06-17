//! Composer `type: path` repository support.
//!
//! A path repository points at a local directory (or a glob of them);
//! each matched directory's own `composer.json` *is* the package
//! definition. Unlike a Composer-protocol repo there is no network
//! metadata — candidates are read straight off disk and seeded into
//! the resolver cache before the solve runs (see
//! [`crate::update::ResolveProvider::seed_path_candidates`]).
//!
//! This module is the pure, `&self`-free machinery: glob expansion,
//! reading a directory into a [`LockPackage`], version inference, and
//! dist construction. It deliberately mirrors Composer's
//! `PathRepository` semantics:
//!
//! - `url` globs with `*`/`?`, expands a leading `~` and `$VAR` /
//!   `${VAR}` env vars, and resolves relative to the project root.
//! - The package version comes from (in priority order) an explicit
//!   `version` in the package's `composer.json`, an `options.versions`
//!   override, the local git branch/tag, then `dev-master`.
//! - The locked dist is `type: path`, carries no shasum, and its
//!   `reference` follows `options.reference` (auto/config/none).

use std::path::{Path, PathBuf};
use std::process::Command;

use bougie_composer::lockfile::{LockAutoload, LockDist, LockPackage};
use bougie_semver::Version;
use serde_json::Value;

use crate::metadata::{PathRepoConfig, ReferenceMode};

/// A path package read off disk, ready to seed into the resolver
/// cache: the parsed [`Version`] (so the cache doesn't re-parse) and
/// the fully-formed [`LockPackage`] (dist + transport options
/// included, so the lockfile writer needs no special-casing).
#[derive(Debug)]
pub(crate) struct SeededPathPackage {
    pub version: Version,
    pub package: LockPackage,
}

/// Expand a path-repo `url` into the set of matched package
/// directories, resolved against `project_root`.
///
/// Handles a leading `~` (home dir), `$VAR` / `${VAR}` env vars, and
/// `*`/`?` glob wildcards. A non-glob url resolves to a single
/// directory (if it exists). No matches yields an empty vec — a path
/// repo can legitimately match nothing yet (Composer behavior). Only
/// directories are returned; the caller filters for `composer.json`.
pub(crate) fn expand_url(url: &str, project_root: &Path) -> Vec<PathBuf> {
    let expanded = expand_tilde_and_env(url);
    let pattern_path = if Path::new(&expanded).is_absolute() {
        PathBuf::from(&expanded)
    } else {
        project_root.join(&expanded)
    };
    let pattern = pattern_path.to_string_lossy();

    if !expanded.contains('*') && !expanded.contains('?') && !expanded.contains('[') {
        // No glob metacharacters: a single concrete path.
        return if pattern_path.is_dir() {
            vec![pattern_path]
        } else {
            Vec::new()
        };
    }

    let mut out = Vec::new();
    match glob::glob(&pattern) {
        Ok(paths) => {
            for entry in paths.flatten() {
                if entry.is_dir() {
                    out.push(entry);
                }
            }
        }
        Err(e) => {
            tracing::warn!(url, error = %e, "invalid path-repository glob pattern; ignoring");
        }
    }
    out.sort();
    out
}

/// Expand a leading `~` to `$HOME` and `$VAR` / `${VAR}` references to
/// their environment values. Unset variables expand to empty (matching
/// shell-ish behavior); a literal `~` not at the start is left alone.
fn expand_tilde_and_env(input: &str) -> String {
    let mut s = input.to_owned();
    if (s == "~" || s.starts_with("~/"))
        && let Ok(home) = std::env::var("HOME")
    {
        s = format!("{home}{}", &s[1..]);
    }
    if !s.contains('$') {
        return s;
    }
    let mut out = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'$' {
            let (name, next) = read_var_name(&s, i + 1);
            if let Some(name) = name {
                out.push_str(&std::env::var(&name).unwrap_or_default());
                i = next;
                continue;
            }
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    out
}

/// Read a `$VAR` or `${VAR}` name starting just after the `$` at
/// `start`. Returns the variable name and the index just past it, or
/// `None` if there's no valid name there (a bare `$`).
fn read_var_name(s: &str, start: usize) -> (Option<String>, usize) {
    let bytes = s.as_bytes();
    if start < bytes.len() && bytes[start] == b'{' {
        if let Some(end) = s[start + 1..].find('}') {
            let name = s[start + 1..start + 1 + end].to_owned();
            return (Some(name), start + 1 + end + 1);
        }
        return (None, start);
    }
    let mut j = start;
    while j < bytes.len() && (bytes[j].is_ascii_alphanumeric() || bytes[j] == b'_') {
        j += 1;
    }
    if j == start {
        (None, start)
    } else {
        (Some(s[start..j].to_owned()), j)
    }
}

/// Read one path-package directory into a [`SeededPathPackage`].
///
/// Returns `Ok(None)` (with a warning) when the directory has no
/// `composer.json` or no `name` — Composer skips such directories
/// rather than failing the whole resolve.
pub(crate) fn read_path_package(
    dir: &Path,
    config: &PathRepoConfig,
    project_root: &Path,
) -> eyre::Result<Option<SeededPathPackage>> {
    let composer_path = dir.join("composer.json");
    if !composer_path.is_file() {
        tracing::warn!(
            dir = %dir.display(),
            "path repository directory has no composer.json; skipping",
        );
        return Ok(None);
    }
    let bytes = std::fs::read(&composer_path)?;
    let json: Value = serde_json::from_slice(&bytes)
        .map_err(|e| eyre::eyre!("parsing {}: {e}", composer_path.display()))?;
    let Some(obj) = json.as_object() else {
        tracing::warn!(path = %composer_path.display(), "composer.json is not an object; skipping");
        return Ok(None);
    };
    let Some(name) = obj.get("name").and_then(Value::as_str) else {
        tracing::warn!(
            path = %composer_path.display(),
            "path package composer.json has no `name`; skipping",
        );
        return Ok(None);
    };
    let name = name.to_owned();

    let version_str = infer_version(dir, obj, config, &name);
    let version = match Version::parse(&version_str) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(
                package = %name, version = %version_str, error = %e,
                "could not parse inferred version for path package; skipping",
            );
            return Ok(None);
        }
    };

    // We intentionally ignore a path package's own `repositories` key:
    // Composer only honors repositories declared in the *root*
    // package, never in a dependency.
    let autoload: LockAutoload = obj
        .get("autoload")
        .cloned()
        .map(serde_json::from_value)
        .transpose()
        .unwrap_or(None)
        .unwrap_or_default();
    let dist = build_path_dist(dir, config, &version_str, project_root);
    let transport_options = build_transport_options(config);

    let package = LockPackage {
        name,
        description: obj.get("description").and_then(Value::as_str).map(str::to_owned),
        version: version_str,
        version_normalized: Some(version.normalized.clone()),
        dist: Some(dist),
        source: None,
        transport_options,
        require: string_map(obj.get("require")),
        require_dev: string_map(obj.get("require-dev")),
        package_type: obj.get("type").and_then(Value::as_str).map(str::to_owned),
        autoload,
        autoload_dev: obj.get("autoload-dev").cloned().unwrap_or(Value::Null),
        replace: string_map(obj.get("replace")),
        provide: string_map(obj.get("provide")),
        conflict: string_map(obj.get("conflict")),
        bin: string_list(obj.get("bin")),
        extra: obj.get("extra").cloned().unwrap_or(Value::Null),
        time: None,
        license: string_list(obj.get("license")),
        funding: Vec::new(),
    };
    Ok(Some(SeededPathPackage { version, package }))
}

/// Infer a path package's version per Composer's ladder:
/// 1. explicit `version` in the package's composer.json,
/// 2. `options.versions[name]` override,
/// 3. the local git branch (`dev-<branch>`) or exact tag,
/// 4. fallback `dev-master`.
fn infer_version(
    dir: &Path,
    obj: &serde_json::Map<String, Value>,
    config: &PathRepoConfig,
    name: &str,
) -> String {
    if let Some(v) = obj.get("version").and_then(Value::as_str) {
        return v.to_owned();
    }
    if let Some(v) = config.versions.get(name) {
        return v.clone();
    }
    if let Some(v) = git_version(dir) {
        return v;
    }
    "dev-master".to_owned()
}

/// Infer a version from the package directory's git state: the current
/// branch becomes `dev-<branch>`; a detached HEAD on an exact tag uses
/// that tag. Returns `None` when the dir isn't a git repo or git isn't
/// available.
fn git_version(dir: &Path) -> Option<String> {
    if let Some(branch) = git(dir, &["symbolic-ref", "--short", "HEAD"])
        && !branch.is_empty()
    {
        return Some(format!("dev-{branch}"));
    }
    // Detached HEAD: use an exact tag if one points at HEAD.
    if let Some(tag) = git(dir, &["describe", "--tags", "--exact-match"])
        && !tag.is_empty()
    {
        return Some(tag);
    }
    None
}

/// Run a read-only `git -C <dir> <args...>` and return trimmed stdout
/// on success, `None` on any failure (git missing, not a repo, etc.).
fn git(dir: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

/// Build the locked `path` dist for a package directory.
///
/// `url` is the package directory expressed relative to the project
/// root (Composer writes the relative path). `reference` follows
/// `options.reference`. No shasum — path dists have none.
fn build_path_dist(
    dir: &Path,
    config: &PathRepoConfig,
    version: &str,
    project_root: &Path,
) -> LockDist {
    let url = relative_path(project_root, dir);
    let reference = match config.reference {
        ReferenceMode::None => None,
        ReferenceMode::Config => Some(config_hash(dir, version)),
        ReferenceMode::Auto => git(dir, &["rev-parse", "HEAD"])
            .filter(|h| !h.is_empty())
            .or_else(|| Some(config_hash(dir, version))),
    };
    LockDist {
        kind: "path".to_owned(),
        url,
        shasum: None,
        reference,
        transport_options: Value::Null,
    }
}

/// The `config`-mode dist reference: a deterministic sha1 over the
/// package's `composer.json` bytes plus its resolved version. This is
/// *not* byte-identical to Composer's PHP `serialize($options)` hash —
/// bougie reads and writes its own lock, so what matters is that the
/// reference is stable for a given (composer.json, version) and
/// changes when they do. Use `reference: none` for the lowest churn.
fn config_hash(dir: &Path, version: &str) -> String {
    use sha1::{Digest, Sha1};
    let mut hasher = Sha1::new();
    if let Ok(bytes) = std::fs::read(dir.join("composer.json")) {
        hasher.update(&bytes);
    }
    hasher.update(version.as_bytes());
    format!("{:x}", hasher.finalize())
}

/// Composer's package-level `transport-options` for a path package:
/// the `symlink` / `relative` install hints. Only keys the user set in
/// the repo `options` are recorded (matching Composer, which copies the
/// option through verbatim); the install-time defaults — prefer
/// symlink, `relative: true` — are applied by the materializer, not
/// stored here. Returns `Null` when neither key was set.
fn build_transport_options(config: &PathRepoConfig) -> Value {
    if config.symlink.is_none() && config.relative.is_none() {
        return Value::Null;
    }
    let mut map = serde_json::Map::new();
    if let Some(symlink) = config.symlink {
        map.insert("symlink".to_owned(), Value::Bool(symlink));
    }
    if let Some(relative) = config.relative {
        map.insert("relative".to_owned(), Value::Bool(relative));
    }
    Value::Object(map)
}

/// Express `target` relative to `base` using `..` segments where
/// needed. Falls back to the absolute `target` path when no relative
/// form is computable (e.g. different roots). Pure string/component
/// math — does not touch the filesystem, so it works for dirs that
/// have already been canonicalized by the glob walk.
fn relative_path(base: &Path, target: &Path) -> String {
    use std::path::Component;
    let base_comps: Vec<Component> = base.components().collect();
    let target_comps: Vec<Component> = target.components().collect();
    let common = base_comps
        .iter()
        .zip(&target_comps)
        .take_while(|(a, b)| a == b)
        .count();
    // If they share no common prefix at all, a relative path would be
    // all `..` up to the root and back down — just use the absolute.
    if common == 0 {
        return target.to_string_lossy().into_owned();
    }
    let ups = base_comps.len() - common;
    let mut rel = PathBuf::new();
    for _ in 0..ups {
        rel.push("..");
    }
    for comp in &target_comps[common..] {
        rel.push(comp.as_os_str());
    }
    if rel.as_os_str().is_empty() {
        ".".to_owned()
    } else {
        rel.to_string_lossy().into_owned()
    }
}

/// Deserialize a `{name: constraint}` object into a sorted string map,
/// tolerating a missing / non-object value (→ empty).
fn string_map(value: Option<&Value>) -> std::collections::BTreeMap<String, String> {
    let mut out = std::collections::BTreeMap::new();
    if let Some(obj) = value.and_then(Value::as_object) {
        for (k, v) in obj {
            if let Some(s) = v.as_str() {
                out.insert(k.clone(), s.to_owned());
            }
        }
    }
    out
}

/// Read a `bin` / `license`-style field that is either a single string
/// or an array of strings into a vec.
fn string_list(value: Option<&Value>) -> Vec<String> {
    match value {
        Some(Value::String(s)) => vec![s.clone()],
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(|v| v.as_str().map(str::to_owned))
            .collect(),
        _ => Vec::new(),
    }
}
