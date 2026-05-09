use crate::cli::OutputFormat;
use crate::config::{load_project, ProjectConfig};
use crate::errors::BougieError;
use crate::install::{install_php, InstalledPhp};
use crate::output::{emit, Render};
use crate::paths::Paths;
use crate::request::{Flavor, Request, VersionLike};
use crate::resolve::{intersect_php, ResolveOptions};
use crate::state::{write_project_resolved, GlobalState};
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

    let request = Request::VersionLike { spec, flavor: Some(flavor) };
    let installed: InstalledPhp =
        install_php(&paths, &request, Some(flavor), ResolveOptions::default())?;

    let resolved_path =
        write_project_resolved(&project_root, installed.version, installed.flavor)?;
    let shims_dir = write_shims(&project_root)?;

    let mut global = GlobalState::load(&paths)?;
    global.host_target = Some(Triple::detect()?.to_string());
    global.touch_project(&project_root);
    global.save(&paths)?;

    let result = SyncResult {
        schema_version: 1,
        php_version: installed.version.to_string(),
        php_flavor: installed.flavor.to_string(),
        install_path: installed.install_path,
        resolved_path,
        shims_dir,
    };
    emit(format, field, &result)?;
    Ok(ExitCode::SUCCESS)
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
            return Err(BougieError::Resolution(format!("unknown flavor: {other}")).into())
        }
    };
    Ok((spec, flavor))
}

fn write_shims(project_root: &std::path::Path) -> Result<PathBuf> {
    let bin_dir = project_root.join(".bougie").join("bin");
    std::fs::create_dir_all(&bin_dir)?;
    let bougie_bin =
        std::env::current_exe().map_err(|e| eyre!("locating current executable: {e}"))?;
    for name in ["php", "php-fpm"] {
        let link = bin_dir.join(name);
        if link.exists() || link.symlink_metadata().is_ok() {
            std::fs::remove_file(&link)?;
        }
        symlink(&bougie_bin, &link)?;
    }
    Ok(bin_dir)
}
