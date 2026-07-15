//! `bougie tool run <vendor/name>[@<constraint>] [--php <ver>]
//! [--with <pkg>...] -- args...`.
//!
//! Two paths:
//!
//! - **Reuse persistent install**: walk
//!   `paths.tools()/*/receipt.toml`. If any receipt matches the
//!   request exactly (same package, constraint, resolved PHP, sorted
//!   composer extras, and sorted extension set), exec that install's
//!   wrapper directly. No cache write.
//! - **Materialise in cache**: hash the request into a stable
//!   cache key, install into
//!   `paths.cache_tool_run_dir(<key>)`, then exec the cached
//!   wrapper. A second invocation with the same request short-
//!   circuits to the existing cache slot (mtime gets refreshed so
//!   `bougie cache prune` doesn't GC active tools).
//!
//! The cache materialisation reuses `install::install_prepared` with
//! `InstallTarget::Ephemeral { dir }` — same composer.json /
//! lock / vendor / wrapper / receipt write as a persistent install,
//! minus the PATH symlink step.
//!
//! The ephemeral lane is also where project context applies: when the
//! bougie binary detected a surrounding PHP project (and `--no-project`
//! wasn't passed), the derived PHP + extension set flows in through
//! [`ProjectContext`] and lands in the cache key, so the same tool run
//! from two differently-shaped projects gets two slots.

use crate::install::{InstallContext, InstallPlan, InstallTarget, install_prepared, plan_install};
use crate::list::{ListedTool, ToolStatus, list};
use crate::receipt::{ToolEntrypoint, ToolReceipt};
use crate::request::ToolRequest;
use crate::resolve::{PhpChoice, PhpSource, ProjectContext};
use crate::exec;
use bougie_paths::Paths;
use eyre::{Result, WrapErr, bail};
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
    project: Option<&ProjectContext>,
    bin: Option<&str>,
    user_args: Vec<OsString>,
) -> Result<std::convert::Infallible> {
    let plan = prepare(ctx, request, php_spec, with, project)?;

    let receipt = crate::receipt::read(&plan.tool_dir.join("receipt.toml"))?;
    let declared = crate::install::read_default_bin(&plan.tool_dir, &plan.package)?;
    let entry = pick_bin(&receipt.entrypoints, &plan.package, bin, declared.as_deref())?;
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
    project: Option<&ProjectContext>,
) -> Result<RunPlan> {
    // Resolve only — the baseline ensure runs inside
    // `install_prepared` when we materialise, so a cache hit doesn't
    // pay for (or fail on) it.
    let plan = plan_install(ctx, request, php_spec, with, project)?;
    announce_project_context(&plan, project);

    if let Some(dir) = find_persistent_match(ctx.paths, &plan)? {
        return Ok(RunPlan {
            package: plan.package,
            php: plan.php,
            tool_dir: dir,
            from_cache: false,
            materialised: false,
        });
    }

    let ext_names = plan.extension_names();
    let key = cache_key(
        &plan.package,
        &plan.constraint,
        &plan.php.version,
        &plan.php.flavor,
        &plan.composer_extras,
        &ext_names,
    );
    let cache_dir = ctx.paths.cache_tool_run_dir(&key);
    let materialised = !cache_dir.join("receipt.toml").is_file();
    if materialised {
        // The Ephemeral path skips PATH symlinks; everything else
        // (composer.json, vendor/, wrapper under bin/, conf.d/,
        // receipt.toml) is identical to a persistent install.
        std::fs::create_dir_all(&cache_dir)
            .wrap_err_with(|| format!("creating {}", cache_dir.display()))?;
        install_prepared(
            ctx,
            &plan,
            true,
            &InstallTarget::Ephemeral {
                dir: cache_dir.clone(),
            },
        )?;
    }
    Ok(RunPlan {
        package: plan.package,
        php: plan.php,
        tool_dir: cache_dir,
        from_cache: true,
        materialised,
    })
}

/// One-line stderr notice when the surrounding project actually
/// shaped the run — the user typed no flags, so say what was chosen
/// on their behalf and how to turn it off. Quiet when the project
/// contributed nothing (no constraint hit, no extensions).
fn announce_project_context(plan: &InstallPlan, project: Option<&ProjectContext>) {
    let Some(p) = project else { return };
    let php_from_project = matches!(
        plan.php_source,
        PhpSource::ProjectResolved | PhpSource::ProjectIntersection
    );
    let parts = match (php_from_project, p.extensions.is_empty()) {
        (true, false) => format!(
            "PHP {}; extensions: {}",
            plan.php.version,
            p.extensions.join(", ")
        ),
        (true, true) => format!("PHP {}", plan.php.version),
        (false, false) => format!("extensions: {}", p.extensions.join(", ")),
        (false, true) => return,
    };
    eprintln!(
        "bougie: project context ({source}): {parts} (disable with --no-project)",
        source = p.php.source,
    );
}

/// Stable cache key for a `(package, constraint, php, composer
/// extras, extension set)` request. Order-invariant on both lists so
/// `--with A --with B` collides with `--with B --with A`.
pub fn cache_key(
    package: &str,
    constraint: &str,
    php_version: &str,
    php_flavor: &str,
    composer_extras: &[String],
    extensions: &[String],
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
    let mut sorted = composer_extras.to_vec();
    sorted.sort();
    for w in &sorted {
        hasher.update(w.as_bytes());
        hasher.update(b"\0");
    }
    // Divider so ["a"], [] can't collide with [], ["a"].
    hasher.update(b"\x01");
    let mut sorted_ext = extensions.to_vec();
    sorted_ext.sort();
    for e in &sorted_ext {
        hasher.update(e.as_bytes());
        hasher.update(b"\0");
    }
    let digest = hasher.finalize();
    hex::encode(&digest[..8]) // 16 hex chars; ~64 bits, plenty for cache slots
}

/// Walk persistent installs for an exact match against the resolved
/// plan. Skips broken / stale receipts.
fn find_persistent_match(paths: &Paths, plan: &InstallPlan) -> Result<Option<PathBuf>> {
    let mut sorted_with = plan.composer_extras.clone();
    sorted_with.sort();
    let mut sorted_ext = plan.extension_names();
    sorted_ext.sort();
    for tool in list(paths)? {
        let ListedTool { status, tool_dir, .. } = tool;
        let ToolStatus::Healthy(receipt) = status else {
            continue;
        };
        if receipt_matches(&receipt, plan, &sorted_with, &sorted_ext) {
            return Ok(Some(tool_dir));
        }
    }
    Ok(None)
}

fn receipt_matches(
    receipt: &ToolReceipt,
    plan: &InstallPlan,
    sorted_with: &[String],
    sorted_ext: &[String],
) -> bool {
    if receipt.package != plan.package
        || receipt.constraint != plan.constraint
        || receipt.php_version != plan.php.version
        || receipt.php_flavor != plan.php.flavor
    {
        return false;
    }
    let mut their_with = receipt.with.clone();
    their_with.sort();
    if their_with.as_slice() != sorted_with {
        return false;
    }
    let mut their_ext: Vec<String> =
        receipt.extensions.iter().map(|e| e.name.clone()).collect();
    their_ext.sort();
    their_ext.as_slice() == sorted_ext
}

/// Pick the bin to run for a `tool run <pkg>` request.
///
/// Selection precedence:
/// 1. `wanted` — the user's explicit `--bin <name>`. Returns that entry
///    or errors listing the bins the tool actually exposes (an explicit
///    request that can't be honoured is a hard error).
/// 2. `declared` — the package's own `extra.bougie.default-bin`. Used
///    when it resolves to a real bin; a stale/typo'd value falls
///    through rather than breaking the run.
/// 3. A bin whose name matches the package's `<name>` segment
///    (`phpstan/phpstan` → `phpstan`).
/// 4. The sole bin, if there's exactly one.
/// 5. Otherwise error with a hint to pass `--bin` (or install + run
///    from PATH).
pub fn pick_bin<'a>(
    entries: &'a [ToolEntrypoint],
    package: &str,
    wanted: Option<&str>,
    declared: Option<&str>,
) -> Result<&'a ToolEntrypoint> {
    if let Some(name) = wanted {
        if let Some(e) = entries.iter().find(|e| e.name == name) {
            return Ok(e);
        }
        let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        if names.is_empty() {
            bail!("tool `{package}` has no bin entries, so `--bin {name}` can't be satisfied");
        }
        bail!(
            "tool `{package}` has no bin `{name}`; available bins: {list}",
            list = names.join(", "),
        );
    }
    if let Some(e) = declared.and_then(|name| entries.iter().find(|e| e.name == name)) {
        return Ok(e);
    }
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
                 Select one with `--bin <NAME>`, or `bougie tool install {package}` to run the bin from PATH.",
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
        let a = cache_key("phpstan/phpstan", "^1.10", "8.3.12", "nts", &[], &[]);
        let b = cache_key("phpstan/phpstan", "^1.10", "8.3.12", "nts", &[], &[]);
        assert_eq!(a, b);
    }

    #[test]
    fn cache_key_order_independent_on_lists() {
        let one = cache_key(
            "v/p",
            "*",
            "8.3.12",
            "nts",
            &["a/b".into(), "c/d".into()],
            &["intl".into(), "zip".into()],
        );
        let two = cache_key(
            "v/p",
            "*",
            "8.3.12",
            "nts",
            &["c/d".into(), "a/b".into()],
            &["zip".into(), "intl".into()],
        );
        assert_eq!(one, two);
    }

    #[test]
    fn cache_key_changes_with_php_version() {
        let a = cache_key("v/p", "*", "8.3.12", "nts", &[], &[]);
        let b = cache_key("v/p", "*", "8.3.13", "nts", &[], &[]);
        assert_ne!(a, b);
    }

    #[test]
    fn cache_key_changes_with_extension_set() {
        let a = cache_key("v/p", "*", "8.3.12", "nts", &[], &[]);
        let b = cache_key("v/p", "*", "8.3.12", "nts", &[], &["intl".into()]);
        assert_ne!(a, b);
    }

    #[test]
    fn cache_key_lists_do_not_bleed_into_each_other() {
        // A name moving between the composer list and the extension
        // list must produce a different slot.
        let a = cache_key("v/p", "*", "8.3.12", "nts", &["x".into()], &[]);
        let b = cache_key("v/p", "*", "8.3.12", "nts", &[], &["x".into()]);
        assert_ne!(a, b);
    }

    #[test]
    fn pick_bin_prefers_package_name_match() {
        let entries = [ep("phpstan"), ep("phpstan.phar")];
        let chosen = pick_bin(&entries, "phpstan/phpstan", None, None).unwrap();
        assert_eq!(chosen.name, "phpstan");
    }

    #[test]
    fn pick_bin_falls_back_to_single_when_no_name_match() {
        let entries = [ep("rector")];
        let chosen = pick_bin(&entries, "vendor/something-else", None, None).unwrap();
        assert_eq!(chosen.name, "rector");
    }

    #[test]
    fn pick_bin_errors_on_ambiguous_multi() {
        let entries = [ep("a"), ep("b"), ep("c")];
        let err = pick_bin(&entries, "v/x", None, None).unwrap_err().to_string();
        assert!(err.contains("exposes 3 bins"), "{err}");
        assert!(err.contains("--bin"), "{err}");
        assert!(err.contains("tool install"), "{err}");
    }

    #[test]
    fn pick_bin_errors_on_empty_entries() {
        let err = pick_bin(&[], "v/p", None, None).unwrap_err().to_string();
        assert!(err.contains("no bin entries"), "{err}");
    }

    #[test]
    fn pick_bin_selects_explicit_bin_among_many() {
        let entries = [ep("bricklayer"), ep("bricklayer-mcp")];
        let chosen =
            pick_bin(&entries, "inchoo/magento-bricklayer", Some("bricklayer-mcp"), None)
                .unwrap();
        assert_eq!(chosen.name, "bricklayer-mcp");
    }

    #[test]
    fn pick_bin_explicit_bin_overrides_name_match() {
        // `--bin` wins even when a bin matches the package name.
        let entries = [ep("phpstan"), ep("phpstan-secondary")];
        let chosen =
            pick_bin(&entries, "phpstan/phpstan", Some("phpstan-secondary"), None).unwrap();
        assert_eq!(chosen.name, "phpstan-secondary");
    }

    #[test]
    fn pick_bin_errors_on_unknown_explicit_bin() {
        let entries = [ep("bricklayer"), ep("bricklayer-mcp")];
        let err = pick_bin(&entries, "inchoo/magento-bricklayer", Some("nope"), None)
            .unwrap_err()
            .to_string();
        assert!(err.contains("no bin `nope`"), "{err}");
        assert!(err.contains("bricklayer, bricklayer-mcp"), "{err}");
    }

    #[test]
    fn pick_bin_honours_declared_default_over_ambiguity() {
        // No `--bin`, no name match, but the package declares a default.
        let entries = [ep("bricklayer"), ep("bricklayer-mcp")];
        let chosen = pick_bin(
            &entries,
            "inchoo/magento-bricklayer",
            None,
            Some("bricklayer"),
        )
        .unwrap();
        assert_eq!(chosen.name, "bricklayer");
    }

    #[test]
    fn pick_bin_explicit_bin_beats_declared_default() {
        let entries = [ep("bricklayer"), ep("bricklayer-mcp")];
        let chosen = pick_bin(
            &entries,
            "inchoo/magento-bricklayer",
            Some("bricklayer-mcp"),
            Some("bricklayer"),
        )
        .unwrap();
        assert_eq!(chosen.name, "bricklayer-mcp");
    }

    #[test]
    fn pick_bin_stale_declared_default_falls_through() {
        // A declared default that no longer names a real bin must not
        // break the run — fall through to the heuristics (here: the
        // package-name match).
        let entries = [ep("phpstan"), ep("phpstan.phar")];
        let chosen =
            pick_bin(&entries, "phpstan/phpstan", None, Some("gone")).unwrap();
        assert_eq!(chosen.name, "phpstan");
    }

    fn plan_for(
        constraint: &str,
        composer_extras: &[&str],
        with_extensions: &[&str],
        derived_extensions: &[&str],
    ) -> InstallPlan {
        InstallPlan {
            package: "v/p".into(),
            constraint: constraint.into(),
            php: PhpChoice {
                version: "8.3.12".into(),
                flavor: "nts".into(),
                bin: PathBuf::from("/x"),
            },
            php_source: PhpSource::ToolRequire,
            composer_extras: composer_extras.iter().map(|s| (*s).into()).collect(),
            with_extensions: with_extensions.iter().map(|s| (*s).into()).collect(),
            derived_extensions: derived_extensions.iter().map(|s| (*s).into()).collect(),
        }
    }

    fn sorted(plan: &InstallPlan) -> (Vec<String>, Vec<String>) {
        let mut w = plan.composer_extras.clone();
        w.sort();
        let mut e = plan.extension_names();
        e.sort();
        (w, e)
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
        let plan = plan_for("^1", &["a/b"], &[], &[]);
        let (w, e) = sorted(&plan);
        assert!(receipt_matches(&receipt, &plan, &w, &e));

        let wrong_constraint = plan_for("^2", &["a/b"], &[], &[]);
        let (w, e) = sorted(&wrong_constraint);
        assert!(!receipt_matches(&receipt, &wrong_constraint, &w, &e));

        let missing_with = plan_for("^1", &[], &[], &[]);
        let (w, e) = sorted(&missing_with);
        assert!(!receipt_matches(&receipt, &missing_with, &w, &e));
    }

    #[test]
    fn receipt_matches_compares_extension_sets() {
        let receipt = ToolReceipt {
            package: "v/p".into(),
            constraint: "^1".into(),
            php_version: "8.3.12".into(),
            php_flavor: "nts".into(),
            composer_version: "2.8.12".into(),
            with: vec![],
            php_resolved_path: PathBuf::from("/x"),
            entrypoints: vec![],
            extensions: vec![
                crate::receipt::ToolExtension {
                    name: "intl".into(),
                    ini_path: PathBuf::from("/c/20-intl.ini"),
                },
                crate::receipt::ToolExtension {
                    name: "zip".into(),
                    ini_path: PathBuf::from("/c/20-zip.ini"),
                },
            ],
        };
        // A run asking for the same set — via --with or derived,
        // in any mix — matches this persistent install.
        let plan = plan_for("^1", &[], &["zip"], &["intl"]);
        let (w, e) = sorted(&plan);
        assert!(receipt_matches(&receipt, &plan, &w, &e));

        // A run asking for fewer extensions does not.
        let narrower = plan_for("^1", &[], &["zip"], &[]);
        let (w, e) = sorted(&narrower);
        assert!(!receipt_matches(&receipt, &narrower, &w, &e));
    }

    #[test]
    fn extension_names_dedupes_explicit_and_derived() {
        let plan = plan_for("^1", &[], &["intl", "zip"], &["intl", "bcmath"]);
        assert_eq!(plan.extension_names(), vec!["intl", "zip", "bcmath"]);
    }
}
