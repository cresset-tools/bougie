//! `bougie tool run <vendor/name>[@<constraint>] [--php <ver>]
//! [--with <pkg>...] -- args...`.
//!
//! Two paths:
//!
//! - **Reuse persistent install**: walk
//!   `paths.tools()/*/receipt.toml`. If any receipt matches the
//!   request exactly (same package, constraint, resolved PHP and
//!   sorted `with` list), exec that install's wrapper directly. No
//!   cache write.
//! - **Materialise in cache**: hash the request into a stable
//!   cache key, install into
//!   `paths.cache_tool_run_dir(<key>)`, then exec the cached
//!   wrapper. A second invocation with the same request short-
//!   circuits to the existing cache slot (mtime gets refreshed so
//!   `bougie cache prune` doesn't GC active tools).
//!
//! The cache materialisation reuses `install::install_into` with
//! `InstallTarget::Ephemeral { dir }` — same composer.json /
//! lock / vendor / wrapper / receipt write as a persistent install,
//! minus the PATH symlink step.

use crate::install::{InstallContext, InstallTarget, install_into};
use crate::list::{ListedTool, ToolStatus, list};
use crate::receipt::{ToolEntrypoint, ToolReceipt};
use crate::request::ToolRequest;
use crate::resolve::PhpChoice;
use crate::exec;
use bougie_paths::Paths;
use eyre::{Result, bail};
use sha2::{Digest, Sha256};
use std::ffi::OsString;
use std::fs::FileTimes;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

#[derive(Debug, Clone)]
pub struct RunPlan {
    pub package: String,
    pub php: PhpChoice,
    pub tool_dir: PathBuf,
    pub from_cache: bool,
    pub materialised: bool,
}

/// End-to-end: resolve PHP, look for a matching persistent install,
/// otherwise materialise into the cache, then exec the chosen bin
/// with `user_args` and the same env `tool-exec` sets. On Unix this
/// never returns on success.
pub fn run(
    ctx: &InstallContext<'_>,
    request: &ToolRequest,
    php_spec: Option<&str>,
    with: &[String],
    user_args: Vec<OsString>,
) -> Result<std::convert::Infallible> {
    let plan = prepare(ctx, request, php_spec, with)?;

    let receipt = crate::receipt::read(&plan.tool_dir.join("receipt.toml"))?;
    let entry = pick_bin(&receipt.entrypoints, &plan.package)?;
    let Some(wrapper) = entry_to_wrapper(&plan.tool_dir, entry) else {
        bail!(
            "tool dir {} is missing the wrapper for bin `{}`",
            plan.tool_dir.display(),
            entry.name
        );
    };

    if plan.from_cache && !plan.materialised {
        touch_cache_slot(&plan.tool_dir);
    }

    let prep = exec::prepare(ctx.paths, &wrapper, user_args)?;
    exec::execve_replace(&prep)
}

/// Find (or materialise) the tool dir for a request without yet
/// exec-ing anything. Public so the bougie binary's dispatcher can
/// compose extra logic (logging, dry-run, etc.) on top.
pub fn prepare(
    ctx: &InstallContext<'_>,
    request: &ToolRequest,
    php_spec: Option<&str>,
    with: &[String],
) -> Result<RunPlan> {
    let constraint = request
        .constraint
        .clone()
        .unwrap_or_else(|| crate::install::DEFAULT_CONSTRAINT.to_string());
    let package = request.package();
    // Drive PHP off the tool's `require.php` so the cache key reflects
    // what the tool actually needs (and matches what `tool install`
    // would write into a persistent receipt).
    let php = crate::install::pick_php_for_install(
        ctx.paths,
        &package,
        &constraint,
        php_spec,
        ctx.php_installer,
        ctx.php_requirement,
    )?;

    if let Some(dir) = find_persistent_match(ctx.paths, &package, &constraint, &php, with)? {
        return Ok(RunPlan {
            package,
            php,
            tool_dir: dir,
            from_cache: false,
            materialised: false,
        });
    }

    let key = cache_key(&package, &constraint, &php.version, &php.flavor, with);
    let cache_dir = ctx.paths.cache_tool_run_dir(&key);
    let materialised = !cache_dir.join("receipt.toml").is_file();
    if materialised {
        // The Ephemeral path skips PATH symlinks; everything else
        // (composer.json, vendor/, wrapper under bin/, conf.d/,
        // receipt.toml) is identical to a persistent install.
        std::fs::create_dir_all(&cache_dir)
            .map_err(|e| eyre::eyre!("creating {}: {e}", cache_dir.display()))?;
        install_into(
            ctx,
            request,
            php_spec,
            with,
            true,
            &InstallTarget::Ephemeral {
                dir: cache_dir.clone(),
            },
        )?;
    }
    Ok(RunPlan {
        package,
        php,
        tool_dir: cache_dir,
        from_cache: true,
        materialised,
    })
}

/// Stable cache key for a `(package, constraint, php, with)` request.
/// Order-invariant on `with` so `--with A --with B` collides with
/// `--with B --with A`.
pub fn cache_key(
    package: &str,
    constraint: &str,
    php_version: &str,
    php_flavor: &str,
    with: &[String],
) -> String {
    let mut hasher = Sha256::new();
    hasher.update(package.as_bytes());
    hasher.update(b"\0");
    hasher.update(constraint.as_bytes());
    hasher.update(b"\0");
    hasher.update(php_version.as_bytes());
    hasher.update(b"\0");
    hasher.update(php_flavor.as_bytes());
    hasher.update(b"\0");
    let mut sorted = with.to_vec();
    sorted.sort();
    for w in &sorted {
        hasher.update(w.as_bytes());
        hasher.update(b"\0");
    }
    let digest = hasher.finalize();
    hex::encode(&digest[..8]) // 16 hex chars; ~64 bits, plenty for cache slots
}

/// Walk persistent installs for an exact match against the supplied
/// request. Skips broken / stale receipts.
fn find_persistent_match(
    paths: &Paths,
    package: &str,
    constraint: &str,
    php: &PhpChoice,
    with: &[String],
) -> Result<Option<PathBuf>> {
    let mut sorted_with = with.to_vec();
    sorted_with.sort();
    for tool in list(paths)? {
        let ListedTool { status, tool_dir, .. } = tool;
        let ToolStatus::Healthy(receipt) = status else {
            continue;
        };
        if receipt_matches(&receipt, package, constraint, php, &sorted_with) {
            return Ok(Some(tool_dir));
        }
    }
    Ok(None)
}

fn receipt_matches(
    receipt: &ToolReceipt,
    package: &str,
    constraint: &str,
    php: &PhpChoice,
    sorted_with: &[String],
) -> bool {
    if receipt.package != package
        || receipt.constraint != constraint
        || receipt.php_version != php.version
        || receipt.php_flavor != php.flavor
    {
        return false;
    }
    let mut their_with = receipt.with.clone();
    their_with.sort();
    their_with.as_slice() == sorted_with
}

/// Pick the bin to run for a `tool run <pkg>` request: prefer one
/// whose name matches the package's `<name>` segment
/// (`phpstan/phpstan` → `phpstan`); else if there's exactly one bin,
/// use it; else error with a hint to install + run from PATH.
pub fn pick_bin<'a>(
    entries: &'a [ToolEntrypoint],
    package: &str,
) -> Result<&'a ToolEntrypoint> {
    let preferred = package.rsplit_once('/').map_or(package, |(_, n)| n);
    if let Some(e) = entries.iter().find(|e| e.name == preferred) {
        return Ok(e);
    }
    match entries {
        [single] => Ok(single),
        [] => bail!("tool `{package}` has no bin entries"),
        many => {
            let names: Vec<&str> = many.iter().map(|e| e.name.as_str()).collect();
            bail!(
                "tool `{package}` exposes {n} bins ({list}); none match the package name `{preferred}`.\n\
                 Use `bougie tool install {package}` and run the bin from PATH.",
                n = many.len(),
                list = names.join(", "),
            )
        }
    }
}

/// Locate the wrapper file inside a tool dir given an entrypoint.
/// Persistent installs record the PATH symlink as `install_path`;
/// the real wrapper lives at `<tool_dir>/bin/<name>`. Ephemeral
/// installs record the wrapper itself as `install_path`. Either way
/// the wrapper is at the same on-disk location.
fn entry_to_wrapper(tool_dir: &Path, entry: &ToolEntrypoint) -> Option<PathBuf> {
    let candidate = tool_dir.join("bin").join(&entry.name);
    candidate.is_file().then_some(candidate)
}

/// Best-effort mtime refresh for a cache slot we just hit. Failure
/// isn't fatal — the prune walk runs explicitly and only deletes
/// entries that are demonstrably stale.
fn touch_cache_slot(tool_dir: &Path) {
    let Ok(file) = std::fs::OpenOptions::new()
        .write(true)
        .open(tool_dir.join("receipt.toml"))
    else {
        return;
    };
    let times = FileTimes::new().set_modified(SystemTime::now());
    let _ = file.set_times(times);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ep(name: &str) -> ToolEntrypoint {
        ToolEntrypoint {
            name: name.into(),
            install_path: PathBuf::from(format!("/x/{name}")),
            from: "v/p".into(),
        }
    }

    #[test]
    fn cache_key_is_deterministic() {
        let a = cache_key("phpstan/phpstan", "^1.10", "8.3.12", "nts", &[]);
        let b = cache_key("phpstan/phpstan", "^1.10", "8.3.12", "nts", &[]);
        assert_eq!(a, b);
    }

    #[test]
    fn cache_key_order_independent_on_with() {
        let one = cache_key(
            "v/p",
            "*",
            "8.3.12",
            "nts",
            &["a/b".into(), "c/d".into()],
        );
        let two = cache_key(
            "v/p",
            "*",
            "8.3.12",
            "nts",
            &["c/d".into(), "a/b".into()],
        );
        assert_eq!(one, two);
    }

    #[test]
    fn cache_key_changes_with_php_version() {
        let a = cache_key("v/p", "*", "8.3.12", "nts", &[]);
        let b = cache_key("v/p", "*", "8.3.13", "nts", &[]);
        assert_ne!(a, b);
    }

    #[test]
    fn pick_bin_prefers_package_name_match() {
        let entries = [ep("phpstan"), ep("phpstan.phar")];
        let chosen = pick_bin(&entries, "phpstan/phpstan").unwrap();
        assert_eq!(chosen.name, "phpstan");
    }

    #[test]
    fn pick_bin_falls_back_to_single_when_no_name_match() {
        let entries = [ep("rector")];
        let chosen = pick_bin(&entries, "vendor/something-else").unwrap();
        assert_eq!(chosen.name, "rector");
    }

    #[test]
    fn pick_bin_errors_on_ambiguous_multi() {
        let entries = [ep("a"), ep("b"), ep("c")];
        let err = pick_bin(&entries, "v/x").unwrap_err().to_string();
        assert!(err.contains("exposes 3 bins"), "{err}");
        assert!(err.contains("tool install"), "{err}");
    }

    #[test]
    fn pick_bin_errors_on_empty_entries() {
        let err = pick_bin(&[], "v/p").unwrap_err().to_string();
        assert!(err.contains("no bin entries"), "{err}");
    }

    #[test]
    fn receipt_matches_full_tuple() {
        let receipt = ToolReceipt {
            package: "v/p".into(),
            constraint: "^1".into(),
            php_version: "8.3.12".into(),
            php_flavor: "nts".into(),
            composer_version: "2.8.12".into(),
            with: vec!["a/b".into()],
            php_resolved_path: PathBuf::from("/x"),
            entrypoints: vec![],
            extensions: vec![],
        };
        let php = PhpChoice {
            version: "8.3.12".into(),
            flavor: "nts".into(),
            bin: PathBuf::from("/x"),
        };
        assert!(receipt_matches(&receipt, "v/p", "^1", &php, &["a/b".into()]));
        assert!(!receipt_matches(&receipt, "v/p", "^2", &php, &["a/b".into()]));
        assert!(!receipt_matches(&receipt, "v/p", "^1", &php, &[]));
    }
}
