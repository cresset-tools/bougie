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
use crate::index::wire::LoadDirective;
use crate::install::{install_extension, install_local_so};
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
    /// `true` when the extension is already loaded by the install's
    /// bundled conf.d (a `00-*-<name>.ini` fragment is present, sync
    /// having mirrored it from `<install>/etc/php/conf.d/`). In that
    /// case `bougie ext add` skips the `.so` install and the would-be-
    /// duplicate `20-<name>.ini` write; only composer.json is updated.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub bundled: bool,
}

impl Render for ExtAddRemoveResult {
    fn render_text(&self, w: &mut dyn Write) -> io::Result<()> {
        for it in &self.items {
            match (self.action, it.bundled, &it.version) {
                ("add", true, _) => writeln!(
                    w,
                    "add ext-{} (already provided by php install; recorded in composer.json)",
                    it.name
                )?,
                ("add", false, Some(v)) => writeln!(w, "add ext-{} ({v})", it.name)?,
                ("add", false, None) => writeln!(w, "add ext-{}", it.name)?,
                ("remove", _, _) => writeln!(w, "remove ext-{}", it.name)?,
                _ => writeln!(w, "{} ext-{}", self.action, it.name)?,
            }
        }
        Ok(())
    }
}

#[allow(clippy::needless_pass_by_value, reason = "wired from clap-parsed CLI; ownership crosses the function boundary")]
pub fn add(
    format: OutputFormat,
        args: Vec<String>,
    no_sync: bool,
) -> Result<ExitCode> {
    if args.is_empty() {
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
            eprintln!("Syncing…");
        }
        ensure_synced(&paths, &project_root, &project, spec, flavor)?;
    }

    let (php_minor, flavor) = resolved_php_for_ext_install(&project_root)?;

    let mut items = Vec::with_capacity(args.len());
    for raw in &args {
        // Anything ending in `.so` is treated as a path to a local
        // extension binary: copy into the store, auto-detect the name
        // and kind from the ELF, write a fragment in `conf.d-local/`,
        // do not touch composer.json. PHP extension names never
        // contain a dot, so the `.so` suffix is an unambiguous
        // discriminator from index names like `redis` or `redis@6.0.2`.
        if raw.ends_with(".so") {
            items.push(install_local_arg(&paths, &project_root, raw, php_minor, flavor)?);
            continue;
        }
        let (name, version_pin) = parse_name_with_optional_version(raw)?;

        // If sync has already replicated a bundled `00-*-<name>.ini`
        // from the install's `etc/php/conf.d/`, the extension is
        // already loaded — installing again and writing a second
        // `20-<name>.ini` would yield PHP's "Module already loaded"
        // warning on every CLI invocation (issue #28). Skip the .so
        // install and the conf.d write; just record the requirement
        // in composer.json so the lockfile/platform check stay
        // accurate. Also drop any stale `<NN>-<name>.ini` left
        // behind by a buggy older bougie.
        if conf_d::installed_fragment_present(&project_root, &name) {
            conf_d::remove_user_ext_fragment(&project_root, &name)?;
            let applied = apply_require_change(
                &project_root,
                &RequireChange::Add {
                    key: format!("ext-{name}"),
                    constraint: version_pin.clone().unwrap_or_else(|| "*".into()),
                    dev: false,
                },
            )?;
            items.push(ExtItem {
                name,
                version: None,
                conf_d_path: None,
                composer_lock_updated: applied.composer_lock_path.is_some(),
                already_present: true,
                bundled: true,
            });
            continue;
        }

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
            bundled: false,
        });
    }

    let result = ExtAddRemoveResult {
        schema_version: 1,
        action: "add",
        items,
    };
    emit(format, &result)?;
    Ok(ExitCode::SUCCESS)
}

#[allow(clippy::needless_pass_by_value, reason = "wired from clap-parsed CLI; ownership crosses the function boundary")]
pub fn remove(
    format: OutputFormat,
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
            bundled: false,
        });
    }

    let result = ExtAddRemoveResult {
        schema_version: 1,
        action: "remove",
        items,
    };
    emit(format, &result)?;
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
pub fn resolved_php_for_ext_install(project_root: &Path) -> Result<(PartialVersion, Flavor)> {
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

/// Install a local `.so` file: ELF-probe it for the extension name
/// and `zend_extension=` vs `extension=` kind, copy into the
/// content-addressed store, and write a fragment in
/// `.bougie/conf.d-local/`. Does not modify `composer.json` — local
/// extensions are ad-hoc tooling (profilers, vendor binaries) and not
/// part of the portable project dependency set.
fn install_local_arg(
    paths: &Paths,
    project_root: &Path,
    raw_path: &str,
    php_minor: PartialVersion,
    flavor: Flavor,
) -> Result<ExtItem> {
    let source_so = PathBuf::from(raw_path);
    if !source_so.is_file() {
        return Err(eyre!(
            "`{raw_path}` ends in `.so` but isn't a file — \
             local `.so` installs need a path to an existing extension binary"
        ));
    }
    let detected = crate::elf::detect_php_extension(&source_so).wrap_err_with(|| {
        format!(
            "couldn't read PHP extension metadata from {} — \
             not an ELF64 PHP extension? (Mach-O / Windows DLLs aren't supported)",
            source_so.display()
        )
    })?;
    let load = if detected.zend {
        LoadDirective::ZendExtension
    } else {
        LoadDirective::Extension
    };
    let installed = install_local_so(paths, &detected.name, &source_so, php_minor, flavor)?;
    let conf_d_path = conf_d::write_local_ext_fragment(
        project_root,
        &detected.name,
        &installed.so_path,
        load,
    )?;
    Ok(ExtItem {
        name: detected.name,
        version: None,
        conf_d_path: Some(conf_d_path),
        composer_lock_updated: false,
        already_present: installed.already_present,
        bundled: false,
    })
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
