use crate::cli::OutputFormat;
use crate::composer::{self, default_request as default_composer_request, parse_request as parse_composer_request, Installed as InstalledComposer};
use crate::config::{load_project, ProjectConfig};
use crate::errors::BougieError;
use crate::install::{install_php, InstalledPhp};
use crate::output::{emit, Render};
use crate::paths::Paths;
use crate::request::{Flavor, Request, VersionLike};
use crate::resolve::{intersect_php, ResolveOptions};
use crate::state::{write_project_resolved, write_project_resolved_composer, GlobalState};
use crate::target::Triple;
use crate::version::Constraint;
use eyre::{eyre, Result};
use serde::Serialize;
use std::io::{self, Write};
use std::os::unix::fs::symlink;
use std::path::PathBuf;
use std::process::ExitCode;

#[derive(Debug, Serialize)]
pub struct SyncResult {
    pub schema_version: u32,
    pub php_version: String,
    pub php_flavor: String,
    pub install_path: PathBuf,
    pub resolved_path: PathBuf,
    pub shims_dir: PathBuf,
    pub composer_version: String,
    pub composer_path: PathBuf,
}

impl Render for SyncResult {
    fn render_text(&self, w: &mut dyn Write) -> io::Result<()> {
        writeln!(
            w,
            "synced php {}-{} from {}",
            self.php_version,
            self.php_flavor,
            self.install_path.display()
        )?;
        writeln!(
            w,
            "synced composer {} from {}",
            self.composer_version,
            self.composer_path.display()
        )?;
        writeln!(w, "shims at {}", self.shims_dir.display())
    }
}

pub fn run(format: OutputFormat, field: Option<&str>, dry_run: bool) -> Result<ExitCode> {
    let paths = Paths::from_env()?;
    let project_root = std::env::current_dir()?;

    let project = load_project(&project_root)?;
    let (spec, flavor) = resolve_php_inputs(&project)?;

    if dry_run {
        eprintln!("Resolving…");
        eprintln!("would install php matching the resolved spec; flavor={flavor}");
        return Ok(ExitCode::SUCCESS);
    }

    let result = ensure_synced(&paths, &project_root, &project, spec, flavor)?;
    emit(format, field, &result)?;
    Ok(ExitCode::SUCCESS)
}

/// The full sync pipeline minus argument parsing and result emission.
/// Used by `bougie sync` directly and by `bougie ext add/remove` for
/// the implicit-sync-on-demand behavior. Idempotent.
pub fn ensure_synced(
    paths: &Paths,
    project_root: &std::path::Path,
    project: &ProjectConfig,
    spec: VersionLike,
    flavor: Flavor,
) -> Result<SyncResult> {
    let request = Request::VersionLike { spec, flavor: Some(flavor) };
    let installed: InstalledPhp =
        install_php(paths, &request, Some(flavor), ResolveOptions::default())?;

    let resolved_path =
        write_project_resolved(project_root, installed.version, installed.flavor)?;

    let composer_request = match project.bougie.composer.version.as_deref() {
        Some(s) => parse_composer_request(s)?,
        None => default_composer_request(),
    };
    let composer_installed: InstalledComposer =
        composer::install_composer(paths, &composer_request)?;
    write_project_resolved_composer(project_root, &composer_installed.version)?;

    replicate_install_conf_d(&installed.install_path, project_root)?;

    let shims_dir = write_shims(project_root)?;

    let mut global = GlobalState::load(paths)?;
    global.host_target = Some(Triple::detect()?.to_string());
    global.touch_project(project_root);
    global.save(paths)?;

    Ok(SyncResult {
        schema_version: 1,
        php_version: installed.version.to_string(),
        php_flavor: installed.flavor.to_string(),
        install_path: installed.install_path,
        resolved_path,
        shims_dir,
        composer_version: composer_installed.version,
        composer_path: composer_installed.phar_path,
    })
}

/// Resolve the project's PHP inputs (constraint + flavor). Public so
/// callers like `ext add` can drive `ensure_synced` without re-parsing.
pub fn project_php_inputs(project: &ProjectConfig) -> Result<(VersionLike, Flavor)> {
    resolve_php_inputs(project)
}

fn resolve_php_inputs(project: &ProjectConfig) -> Result<(VersionLike, Flavor)> {
    let public = match project.composer.as_ref().and_then(|c| c.require_php.clone()) {
        Some(s) => Some(Constraint::parse(&s)?),
        None => None,
    };
    let override_spec = project
        .bougie
        .php
        .version
        .as_deref()
        .map(|v| -> Result<VersionLike> {
            // Allow either a bare version or a constraint via the same
            // request grammar.
            let r = crate::request::parse_request(v)?;
            match r {
                Request::VersionLike { spec, .. } => Ok(spec),
                _ => Err(eyre!(
                    "[php]version must be a version or constraint, not a path/tag"
                )),
            }
        })
        .transpose()?;

    let spec = intersect_php(public.as_ref(), override_spec.as_ref())?;
    let flavor = match project.bougie.php.flavor.as_deref() {
        Some("nts") | None => Flavor::Nts,
        Some("nts-debug") => Flavor::NtsDebug,
        Some("zts") => Flavor::Zts,
        Some("zts-debug") => Flavor::ZtsDebug,
        Some(other) => {
            return Err(BougieError::Resolution {
                kind: "flavor".into(),
                detail: format!(
                    "[php]flavor = {other:?} is not one of nts | nts-debug | zts | zts-debug"
                ),
            }
            .into())
        }
    };
    Ok((spec, flavor))
}

/// Copy `<install>/etc/php/conf.d/*.ini` into `<project>/.bougie/conf.d/`
/// with a `00-` prefix per CLI.md §6.2 — `PHP_INI_SCAN_DIR` overrides
/// PHP's compiled-in scan dir, so without this step the always-shipped
/// extensions (`phar`, `mbstring`, `openssl`, `pdo_*`, ...) aren't
/// loaded inside the project. The `00-` prefix keeps user fragments
/// (10+ for opcache, 20+ for user extensions) loading after.
///
/// Idempotent: existing `00-*` files are overwritten so sync stays the
/// canonical source of truth for them. User tunables belong in their
/// own fragment file (e.g. `15-mytunables.ini`), not by editing
/// bougie-managed `00-*` files.
fn replicate_install_conf_d(install: &std::path::Path, project_root: &std::path::Path) -> Result<()> {
    let src = install.join("etc").join("php").join("conf.d");
    if !src.is_dir() {
        return Ok(());
    }
    let dst = project_root.join(".bougie").join("conf.d");
    std::fs::create_dir_all(&dst)
        .map_err(|e| eyre!("creating {}: {e}", dst.display()))?;

    // Drop any existing 00- fragments first so removed-from-install
    // extensions don't linger.
    if let Ok(entries) = std::fs::read_dir(&dst) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            if name.starts_with("00-") && name.ends_with(".ini") {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }

    for entry in std::fs::read_dir(&src).map_err(|e| eyre!("reading {}: {e}", src.display()))? {
        let entry = entry.map_err(|e| eyre!("dir entry: {e}"))?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if !name.ends_with(".ini") {
            continue;
        }
        let body = std::fs::read_to_string(entry.path())
            .map_err(|e| eyre!("reading {}: {e}", entry.path().display()))?;
        let dst_path = dst.join(format!("00-{name}"));
        let with_header = format!("; managed by bougie — do not edit\n{body}");
        std::fs::write(&dst_path, with_header)
            .map_err(|e| eyre!("writing {}: {e}", dst_path.display()))?;
    }
    Ok(())
}

fn write_shims(project_root: &std::path::Path) -> Result<PathBuf> {
    let bin_dir = project_root.join(".bougie").join("bin");
    std::fs::create_dir_all(&bin_dir)?;
    let bougie_bin =
        std::env::current_exe().map_err(|e| eyre!("locating current executable: {e}"))?;
    // `unzip` is here because Composer's ZipDownloader does a PATH
    // lookup for it and prefers it over PHP's ZipArchive (§3.7,
    // commands::unzip). Materialising it as a sibling shim keeps the
    // composer subprocess discovery path inside `.bougie/bin/`.
    for name in ["php", "php-fpm", "composer", "unzip"] {
        let link = bin_dir.join(name);
        if link.exists() || link.symlink_metadata().is_ok() {
            std::fs::remove_file(&link)?;
        }
        symlink(&bougie_bin, &link)?;
    }
    Ok(bin_dir)
}
