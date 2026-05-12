//! `bougie ext add` / `bougie ext remove` — extension lifecycle without
//! `composer require` / `composer remove` round-trips.
//!
//! Composer's `require` runs a full dependency-graph resolution and a
//! platform check (`get_loaded_extensions()`) against whatever PHP it
//! happens to be invoked with. For a PHP `ext-*` that bougie hasn't
//! yet installed, that platform check fails — the very situation the
//! old `delegate("require", …)` flow tripped on with `bougie ext add
//! redis`, prompting "Package 'ext-redis' does not exist but is
//! provided by 3 packages" before erroring out.
//!
//! New flow per CLI.md §3.2.1 / §3.2.2: bougie installs the `.so`
//! itself (content-addressed store), enables it via a conf.d fragment,
//! and edits composer.json + composer.lock directly. The next
//! `composer install` accepts the result: the lockfile's `content-hash`
//! matches the post-edit composer.json bytes, and the platform check
//! sees the now-loaded ext via the project's PHP shim.
//!
//! Zero composer subprocess invocations along this path.

use crate::cli::OutputFormat;
use crate::commands::sync::{ensure_synced, project_php_inputs};
use crate::composer::lockfile::{apply_require_change, RequireChange};
use crate::conf_d;
use crate::config::load_project;
use crate::install::install_extension;
use crate::output::{emit, Render};
use crate::paths::Paths;
use crate::request::Flavor;
use crate::resolve::ResolveOptions;
use crate::state::read_project_resolved;
use crate::version::PartialVersion;
use eyre::{eyre, Result, WrapErr};
use serde::Serialize;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

#[derive(Debug, Serialize)]
pub struct ExtAddRemoveResult {
    pub schema_version: u32,
    pub action: &'static str,
    pub items: Vec<ExtItem>,
}

#[derive(Debug, Serialize)]
pub struct ExtItem {
    pub name: String,
    pub version: Option<String>,
    pub conf_d_path: Option<PathBuf>,
    pub composer_lock_updated: bool,
    pub already_present: bool,
}

impl Render for ExtAddRemoveResult {
    fn render_text(&self, w: &mut dyn Write) -> io::Result<()> {
        for it in &self.items {
            match (self.action, &it.version) {
                ("add", Some(v)) => writeln!(w, "add ext-{} ({v})", it.name)?,
                ("add", None) => writeln!(w, "add ext-{}", it.name)?,
                ("remove", _) => writeln!(w, "remove ext-{}", it.name)?,
                _ => writeln!(w, "{} ext-{}", self.action, it.name)?,
            }
        }
        Ok(())
    }
}

#[allow(clippy::needless_pass_by_value, reason = "wired from clap-parsed CLI; ownership crosses the function boundary")]
pub fn add(
    format: OutputFormat,
    field: Option<&str>,
    names: Vec<String>,
    no_sync: bool,
) -> Result<ExitCode> {
    if names.is_empty() {
        return Err(eyre!("no extensions specified"));
    }
    let project_root = locate_project_root()?;
    let paths = Paths::from_env()?;
    let project = load_project(&project_root)?;
    let (spec, flavor) = project_php_inputs(&project)?;

    // Run the full project sync first unless --no-sync, so the project
    // ends up in a usable state (PHP installed, composer shim, bundled
    // conf.d in place). Idempotent — a re-sync of an already-synced
    // project is fast. The "Syncing…" line surfaces only when a sync
    // is actually being initiated for the first time.
    if !no_sync {
        if read_project_resolved(&project_root).is_err() {
            eprintln!("Syncing… (run `bougie sync` to do this explicitly)");
        }
        ensure_synced(&paths, &project_root, &project, spec, flavor)?;
    }

    let (php_minor, flavor) = resolved_php_for_ext_install(&project_root)?;

    let mut items = Vec::with_capacity(names.len());
    for raw in &names {
        let (name, version_pin) = parse_name_with_optional_version(raw)?;

        let installed = install_extension(
            &paths,
            &name,
            version_pin.as_deref(),
            php_minor,
            flavor,
            ResolveOptions::default(),
        )?;

        let conf_d_path = conf_d::write_ext_fragment(
            &project_root,
            &installed.name,
            &installed.so_path,
            installed.load,
        )?;

        let applied = apply_require_change(
            &project_root,
            &RequireChange::Add {
                key: format!("ext-{}", installed.name),
                constraint: version_pin.clone().unwrap_or_else(|| "*".into()),
                dev: false,
            },
        )?;

        items.push(ExtItem {
            name: installed.name,
            version: Some(installed.version.to_string()),
            conf_d_path: Some(conf_d_path),
            composer_lock_updated: applied.composer_lock_path.is_some(),
            already_present: installed.already_present,
        });
    }

    let result = ExtAddRemoveResult {
        schema_version: 1,
        action: "add",
        items,
    };
    emit(format, field, &result)?;
    Ok(ExitCode::SUCCESS)
}

#[allow(clippy::needless_pass_by_value, reason = "wired from clap-parsed CLI; ownership crosses the function boundary")]
pub fn remove(
    format: OutputFormat,
    field: Option<&str>,
    names: Vec<String>,
    no_sync: bool,
) -> Result<ExitCode> {
    if names.is_empty() {
        return Err(eyre!("no extensions specified"));
    }
    let project_root = locate_project_root()?;
    let paths = Paths::from_env()?;
    let project = load_project(&project_root)?;
    let (spec, flavor) = project_php_inputs(&project)?;

    if !no_sync {
        ensure_synced(&paths, &project_root, &project, spec, flavor)?;
    }

    let mut items = Vec::with_capacity(names.len());
    for raw in &names {
        // `remove` ignores any @version suffix — we drop the require
        // and conf.d entry regardless of which version was pinned.
        let (name, _pin) = parse_name_with_optional_version(raw)?;

        let applied = apply_require_change(
            &project_root,
            &RequireChange::Remove {
                key: format!("ext-{name}"),
                dev: false,
            },
        )?;
        let fragment_removed = conf_d::remove_ext_fragment(&project_root, &name)?;

        items.push(ExtItem {
            name,
            version: None,
            conf_d_path: None,
            composer_lock_updated: applied.composer_lock_path.is_some(),
            // We don't reuse the `already_present` field semantically
            // here — set it to true when nothing was actually touched.
            already_present: !applied.change_applied && !fragment_removed,
        });
    }

    let result = ExtAddRemoveResult {
        schema_version: 1,
        action: "remove",
        items,
    };
    emit(format, field, &result)?;
    Ok(ExitCode::SUCCESS)
}

/// Parse `redis` or `redis@6.0.2` into `(name, version?)`.
/// CLI.md §3.2.1 reserves the `@<version>` suffix for an exact-version
/// pin; other constraint shapes go through `bougie.toml`.
fn parse_name_with_optional_version(raw: &str) -> Result<(String, Option<String>)> {
    if let Some((name, ver)) = raw.split_once('@') {
        if name.is_empty() {
            return Err(eyre!("ext name cannot be empty: {raw:?}"));
        }
        if ver.is_empty() {
            return Err(eyre!("ext version cannot be empty: {raw:?}"));
        }
        Ok((name.to_string(), Some(ver.to_string())))
    } else {
        Ok((raw.to_string(), None))
    }
}

/// Read the project's resolved PHP from `.bougie/state/resolved` —
/// that's the single source of truth for which `(php_minor, flavor)`
/// the extension must match. Falls out of `ensure_synced`; absent
/// only if `--no-sync` was passed against an unsynced project.
///
/// Returning the *resolved* version (not the user's constraint) is
/// what frees us from having to compute a "dominant minor" from
/// open-ended constraints like `>=8.3` — the resolver already picked
/// a concrete patch + flavor at sync time.
fn resolved_php_for_ext_install(project_root: &Path) -> Result<(PartialVersion, Flavor)> {
    let (version_str, flavor_str) = read_project_resolved(project_root).wrap_err(
        "project's resolved PHP isn't recorded yet — run `bougie sync` (or drop --no-sync) first",
    )?;
    let version = version_str
        .parse::<crate::version::Version>()
        .map_err(|e| eyre!("malformed .bougie/state/resolved: {version_str:?}: {e}"))?;
    let flavor = match flavor_str.as_str() {
        "nts" => Flavor::Nts,
        "nts-debug" => Flavor::NtsDebug,
        "zts" => Flavor::Zts,
        "zts-debug" => Flavor::ZtsDebug,
        other => return Err(eyre!("malformed .bougie/state/resolved flavor: {other:?}")),
    };
    let php_minor = PartialVersion {
        major: version.major,
        minor: Some(version.minor),
        patch: None,
    };
    Ok((php_minor, flavor))
}

fn locate_project_root() -> Result<PathBuf> {
    let cwd = std::env::current_dir()?;
    cwd.ancestors()
        .find(|p| p.join(".bougie").is_dir())
        .map(Path::to_path_buf)
        .ok_or_else(|| {
            eyre!(
                "no bougie project here (no `.bougie/` in {} or any parent) — \
                 run `bougie init` first",
                cwd.display()
            )
        })
}
