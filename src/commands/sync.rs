use crate::baseline::{self, BaselineFilter};
use crate::cli::OutputFormat;
use crate::composer::{self, default_request as default_composer_request, parse_request as parse_composer_request, Installed as InstalledComposer};
use crate::config::{load_project, ExtensionPin, ProjectConfig};
use crate::errors::BougieError;
use crate::install::{install_baseline_into, install_php, InstalledPhp};
use crate::output::{emit, Render};
use crate::paths::Paths;
use crate::request::{Flavor, Request, VersionLike};
use crate::resolve::{intersect_php, ResolveOptions};
use crate::state::{write_project_resolved, write_project_resolved_composer, GlobalState};
use crate::target::Triple;
use crate::version::{Constraint, PartialVersion};
use eyre::{eyre, Result};
use serde::Serialize;
use std::collections::BTreeSet;
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

    // Ensure the baseline set is present on this interpreter. Idempotent:
    // already-installed extensions short-circuit at the blob fetch, and
    // overwriting `20-<name>.ini` is a no-op when the resolved version
    // hasn't moved. Failures here are non-fatal — they show up under
    // `baseline_failed` in `bougie php install --format json-v1` output,
    // and sync surfaces them as a warning so the project still moves
    // forward.
    let php_minor = PartialVersion {
        major: installed.version.major,
        minor: Some(installed.version.minor),
        patch: None,
    };
    let baseline_report = install_baseline_into(
        paths,
        &installed.install_path,
        php_minor,
        installed.flavor,
        &BaselineFilter::All,
        ResolveOptions::default(),
    );
    for (name, reason) in &baseline_report.failed {
        eprintln!("warning: baseline extension {name} not installed: {reason}");
    }

    let resolved_path =
        write_project_resolved(project_root, installed.version, installed.flavor)?;

    let composer_request = match project.bougie.composer.version.as_deref() {
        Some(s) => parse_composer_request(s)?,
        None => default_composer_request(),
    };
    let composer_installed: InstalledComposer =
        composer::install_composer(paths, &composer_request)?;
    write_project_resolved_composer(project_root, &composer_installed.version)?;

    let opt_out = baseline_opt_outs(project);
    replicate_install_conf_d(&installed.install_path, project_root, &opt_out)?;

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
/// Baseline fragments listed in `baseline_opt_out` are skipped at copy
/// time — the install-root fragment stays in place so other projects
/// sharing this interpreter still see the extension; only this
/// project's view is filtered (CLI.md §3.5.1.1 / §3.3 step 4).
///
/// Idempotent: existing `00-*` files are overwritten so sync stays the
/// canonical source of truth for them. User tunables belong in their
/// own fragment file (e.g. `15-mytunables.ini`), not by editing
/// bougie-managed `00-*` files.
fn replicate_install_conf_d(
    install: &std::path::Path,
    project_root: &std::path::Path,
    baseline_opt_out: &BTreeSet<String>,
) -> Result<()> {
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
        if let Some(ext_name) = ext_name_from_fragment(name)
            && baseline_opt_out.contains(ext_name)
        {
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

/// Parse `20-mbstring.ini` → `Some("mbstring")`. Returns `None` for
/// filenames that don't match the `NN-<name>.ini` shape PBS uses; the
/// caller treats those as un-opt-outable (they're either core or an
/// unrecognized fragment, neither of which is in the baseline set).
fn ext_name_from_fragment(filename: &str) -> Option<&str> {
    let stem = filename.strip_suffix(".ini")?;
    // `20-mbstring` — strip leading digits + dash.
    let dash = stem.find('-')?;
    let (prefix, rest) = stem.split_at(dash);
    if !prefix.chars().all(|c| c.is_ascii_digit()) || prefix.is_empty() {
        return None;
    }
    let name = &rest[1..];
    if baseline::is_baseline(name) {
        Some(name)
    } else {
        None
    }
}

/// Collect baseline-extension names this project has opted out of via
/// the `false` sentinel in `[extensions]` / `extra.bougie.extensions`.
/// Non-baseline names (e.g. `redis = false`) are silently dropped here
/// — they would have no replicated fragment to suppress, and we don't
/// want to retroactively forbid `false` in unrelated slots.
fn baseline_opt_outs(project: &ProjectConfig) -> BTreeSet<String> {
    project
        .bougie
        .extensions
        .iter()
        .filter_map(|(name, pin)| match pin {
            ExtensionPin::Disabled(_) if baseline::is_baseline(name) => Some(name.clone()),
            _ => None,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::BougieConfig;
    use std::collections::BTreeMap;
    use tempfile::TempDir;

    #[test]
    fn fragment_name_parsed_only_for_baseline_extensions() {
        // Only filenames in `<digits>-<name>.ini` shape with a name
        // that's in the baseline set return Some — core fragments
        // (e.g. `20-openssl.ini`) deliberately return None so they
        // can't be opted out.
        assert_eq!(ext_name_from_fragment("20-mbstring.ini"), Some("mbstring"));
        assert_eq!(ext_name_from_fragment("20-mysqli.ini"), Some("mysqli"));
        assert_eq!(ext_name_from_fragment("20-openssl.ini"), None); // core
        assert_eq!(ext_name_from_fragment("10-opcache.ini"), None); // core
        assert_eq!(ext_name_from_fragment("notfragment.txt"), None);
        assert_eq!(ext_name_from_fragment("custom.ini"), None);
    }

    #[test]
    fn baseline_opt_outs_filters_to_baseline_disabled_only() {
        let mut exts = BTreeMap::new();
        exts.insert("mysqli".into(), ExtensionPin::Disabled(false));
        exts.insert("redis".into(), ExtensionPin::Disabled(false)); // not baseline
        exts.insert("mbstring".into(), ExtensionPin::Version("1.0".into())); // pinned, not disabled
        let project = ProjectConfig {
            composer: None,
            bougie: BougieConfig { extensions: exts, ..Default::default() },
        };
        let out = baseline_opt_outs(&project);
        assert!(out.contains("mysqli"));
        assert!(!out.contains("redis"));
        assert!(!out.contains("mbstring"));
    }

    #[test]
    fn replicate_skips_opted_out_baseline_fragments() {
        let install = TempDir::new().unwrap();
        let project = TempDir::new().unwrap();
        let src = install.path().join("etc/php/conf.d");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("20-mbstring.ini"), "extension=mbstring\n").unwrap();
        std::fs::write(src.join("20-mysqli.ini"), "extension=mysqli\n").unwrap();
        std::fs::write(src.join("20-openssl.ini"), "extension=openssl\n").unwrap();

        let mut opt_out = BTreeSet::new();
        opt_out.insert("mysqli".into());
        replicate_install_conf_d(install.path(), project.path(), &opt_out).unwrap();

        let dst = project.path().join(".bougie/conf.d");
        assert!(dst.join("00-20-mbstring.ini").exists());
        assert!(dst.join("00-20-openssl.ini").exists()); // core, can't be opted out
        assert!(!dst.join("00-20-mysqli.ini").exists());
    }
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
