//! Composer `type: vcs` (git) repository support.
//!
//! A vcs repository points at a git remote; each tag and branch's own
//! `composer.json` *is* a package version. Unlike a Composer-protocol
//! repo there is no HTTP metadata — candidates are discovered by cloning
//! the repo (into the shared mirror cache) and reading each ref, then
//! seeded into the resolver cache before the solve
//! ([`crate::update::ResolveProvider::seed_vcs_candidates`]).
//!
//! This is the git-only slice of Composer's `VcsRepository` (see
//! `RESOLVER_PLAN.md` Phase D):
//!
//! - a git **tag** becomes the package version (a leading `v` stripped,
//!   Composer-style), skipped if it isn't a valid version;
//! - a git **branch** becomes `dev-<branch>`, except a version-like
//!   branch (`2.4`, `v7`, `1.2.x`) which aliases to an `x-dev` version
//!   (`2.4.x-dev`, …) so ordinary constraints reach it;
//! - each version carries a `source: {type: git, url, reference}` block
//!   (the ref's commit sha) and no `dist`, so the install path clones it.

use bougie_composer::lockfile::{LockAutoload, LockPackage, LockSource};
use bougie_paths::Paths;
use composer_semver::Version;
use serde_json::Value;

use super::path_repo::{string_list, string_map};
use crate::metadata::VcsRepoConfig;
use crate::vcs::{self, GitRef};

/// A vcs package version read from one git ref, ready to seed into the
/// resolver cache — the parsed [`Version`] plus the fully-formed
/// [`LockPackage`] (source block included, so the lockfile writer needs
/// no special-casing).
#[derive(Debug)]
pub(crate) struct SeededVcsPackage {
    pub version: Version,
    pub package: LockPackage,
}

/// Clone/refresh the repo's mirror and read every tag/branch into a
/// [`SeededVcsPackage`]. Refs whose `composer.json` is missing, nameless,
/// or carries an unparseable version are warn-skipped (Composer does the
/// same rather than failing the whole repo).
pub(crate) fn read_vcs_packages(
    paths: &Paths,
    config: &VcsRepoConfig,
) -> eyre::Result<Vec<SeededVcsPackage>> {
    let mirror = vcs::refresh_mirror(paths, &config.url)?;
    let refs = vcs::list_refs(&mirror)?;
    let mut out = Vec::new();
    for git_ref in refs {
        let Some(bytes) = vcs::read_file_at(&mirror, &git_ref.sha, "composer.json")? else {
            // No composer.json at this ref — not a package version.
            continue;
        };
        match read_ref_package(&config.url, &git_ref, &bytes) {
            Ok(Some(sp)) => out.push(sp),
            Ok(None) => {} // already warned inside
            Err(e) => tracing::warn!(
                url = %config.url, git_ref = %git_ref.name, error = %e,
                "failed to read vcs ref; skipping",
            ),
        }
    }
    Ok(out)
}

/// Turn one ref's `composer.json` into a seeded package. Returns
/// `Ok(None)` (with a warning) when the ref can't yield a valid version.
fn read_ref_package(
    url: &str,
    git_ref: &GitRef,
    composer_json: &[u8],
) -> eyre::Result<Option<SeededVcsPackage>> {
    let json: Value = serde_json::from_slice(composer_json)?;
    let Some(obj) = json.as_object() else {
        tracing::warn!(git_ref = %git_ref.name, "vcs ref composer.json is not an object; skipping");
        return Ok(None);
    };
    let Some(name) = obj.get("name").and_then(Value::as_str) else {
        tracing::warn!(git_ref = %git_ref.name, "vcs ref composer.json has no `name`; skipping");
        return Ok(None);
    };
    let name = name.to_owned();

    // Version comes from the ref: a tag (leading `v` stripped) or a
    // branch (a version-like branch such as `2.4` aliases to `2.4.x-dev`,
    // else `dev-<branch>`). An explicit `version` in composer.json is
    // ignored — Composer derives VCS versions from the ref.
    let version_str = if git_ref.is_tag {
        tag_to_version(&git_ref.name)
    } else {
        normalize_branch(&git_ref.name)
    };
    let version = match Version::parse(&version_str) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(
                package = %name, git_ref = %git_ref.name, version = %version_str, error = %e,
                "vcs ref version is not a valid Composer version; skipping",
            );
            return Ok(None);
        }
    };

    let autoload: LockAutoload = obj
        .get("autoload")
        .cloned()
        .map(serde_json::from_value)
        .transpose()
        .unwrap_or(None)
        .unwrap_or_default();

    let package = LockPackage {
        name,
        description: obj.get("description").and_then(Value::as_str).map(str::to_owned),
        version: version_str,
        version_normalized: Some(version.normalized.clone()),
        dist: None,
        source: Some(LockSource {
            kind: "git".to_owned(),
            url: url.to_owned(),
            reference: git_ref.sha.clone(),
            mirrors: Vec::new(),
        }),
        transport_options: Value::Null,
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
    Ok(Some(SeededVcsPackage { version, package }))
}

/// Composer's branch→version guess (a port of
/// `VersionParser::normalizeBranch`'s pretty form). A branch whose name
/// looks like a version series becomes an `x-dev` version so ordinary
/// constraints reach it:
///
/// - `2.4` → `2.4.x-dev`, `7` → `7.x-dev`, `1.2.3` → `1.2.3.x-dev`,
///   `1.2.x` / `1.2.*` → `1.2.x-dev` (so `^2.4` / `2.4.*` match the branch);
/// - anything else (`main`, `feature/x`, `2.4-develop`) → `dev-<branch>`.
///
/// A version-like branch matches `v?N(.(N|x|X|*)){0,3}` with a numeric
/// first component; a leading `v` is dropped (the `x-dev` normalized form
/// is what the solver compares on, so the cosmetic prefix is immaterial —
/// and bougie locks are semantic-, not byte-, equal to Composer's).
fn normalize_branch(branch: &str) -> String {
    let b = branch.trim();
    let core = b.strip_prefix(['v', 'V']).unwrap_or(b);
    let parts: Vec<&str> = core.split('.').collect();

    let is_version_branch = (1..=4).contains(&parts.len())
        && parts.iter().enumerate().all(|(i, p)| {
            if i == 0 {
                !p.is_empty() && p.bytes().all(|c| c.is_ascii_digit())
            } else {
                (!p.is_empty() && p.bytes().all(|c| c.is_ascii_digit()))
                    || matches!(*p, "x" | "X" | "*")
            }
        });
    if !is_version_branch {
        return format!("dev-{b}");
    }

    // Pad to four components (`.x` for each missing one, `*`/`X` → `x`),
    // then collapse consecutive `x` components into one — Composer's
    // `(\.9999999)+ → .x` step. E.g. [2,4,x,x] → 2.4.x, [2,x,x,x] → 2.x,
    // [2,4,1,x] → 2.4.1.x, [2,4,1,5] → 2.4.1.5.
    let mut comps: Vec<String> = (0..4)
        .map(|i| match parts.get(i) {
            Some(&("x" | "X" | "*")) | None => "x".to_owned(),
            Some(p) => (*p).to_owned(),
        })
        .collect();
    // Drop trailing/adjacent duplicate `x` runs.
    comps.dedup_by(|a, b| a.as_str() == "x" && b.as_str() == "x");
    format!("{}-dev", comps.join("."))
}

/// Composer's tag→version guess: strip a single leading `v`/`V` when it
/// precedes a digit (`v1.2.3` → `1.2.3`); otherwise keep the tag as-is
/// (validity is checked by the caller's `Version::parse`).
fn tag_to_version(tag: &str) -> String {
    let t = tag.trim();
    if let Some(rest) = t.strip_prefix(['v', 'V'])
        && rest.starts_with(|c: char| c.is_ascii_digit())
    {
        return rest.to_owned();
    }
    t.to_owned()
}

#[cfg(test)]
mod tests;
