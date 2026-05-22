use bougie_installer::baseline::{self, BaselineFilter};
use bougie_cli::OutputFormat;
use bougie_composer::{self, default_request as default_composer_request, parse_request as parse_composer_request, Installed as InstalledComposer};
use bougie_installer::conf_d;
use bougie_config::{load_project, ExtensionPin, ProjectConfig};
use bougie_errors::BougieError;
use bougie_fetch::DownloadBar;
use bougie_installer::install::{
    install_baseline_into, install_extension_with_bar, install_php, preinstall_into, InstalledExt,
    InstalledPhp,
};
use bougie_output::output::{emit, Render};
use bougie_paths::Paths;
use bougie_semver::Constraint;
use bougie_version::request::{Flavor, Request, VersionLike};
use bougie_resolver::{intersect_php, ResolveOptions};
use bougie_fs::state::{write_project_resolved, write_project_resolved_composer, GlobalState};
use bougie_fs::store::list_installed;
use bougie_platform::target::Triple;
use bougie_version::version::{PartialVersion, Version};
use eyre::{eyre, Result};
use serde::Serialize;
use std::collections::BTreeSet;
use std::io::{self, Write};
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
    /// Extensions auto-installed from `composer.json`'s `require.ext-*`
    /// — i.e. project-required extensions that weren't already provided
    /// by the core/baseline sets. Built-in (statically-linked) entries
    /// like `ext-pcre` are filtered out before this list is populated.
    pub installed_extensions: Vec<String>,
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
        if !self.installed_extensions.is_empty() {
            writeln!(
                w,
                "installed extensions from composer.json: {}",
                self.installed_extensions.join(", ")
            )?;
        }
        writeln!(w, "shims at {}", self.shims_dir.display())
    }
}

pub fn run(format: OutputFormat, dry_run: bool) -> Result<ExitCode> {
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
    emit(format, &result)?;
    Ok(ExitCode::SUCCESS)
}

/// Same as [`run`] but, when neither `composer.json` nor `bougie.toml`
/// pins a PHP version, falls back to the highest already-installed
/// interpreter (or `>=8.0` for a fresh machine) instead of erroring.
///
/// Mirrors uv's behavior for `uv run` outside a project: be useful with
/// whatever's lying around, defer the strict-constraint requirement to
/// the explicit `bougie sync` path. `bougie sync` itself still errors —
/// only `bougie run` opts in via this entry point.
pub fn run_with_default_fallback(format: OutputFormat, dry_run: bool) -> Result<ExitCode> {
    let paths = Paths::from_env()?;
    let project_root = std::env::current_dir()?;

    let project = load_project(&project_root)?;
    let (spec, flavor) = match resolve_php_inputs(&project) {
        Ok(inputs) => inputs,
        Err(err) if is_missing_php_constraint(&err) => default_php_inputs(&paths, &project)?,
        Err(err) => return Err(err),
    };

    if dry_run {
        eprintln!("Resolving…");
        eprintln!("would install php matching the resolved spec; flavor={flavor}");
        return Ok(ExitCode::SUCCESS);
    }

    let result = ensure_synced(&paths, &project_root, &project, spec, flavor)?;
    emit(format, &result)?;
    Ok(ExitCode::SUCCESS)
}

fn is_missing_php_constraint(err: &eyre::Report) -> bool {
    matches!(
        err.downcast_ref::<BougieError>(),
        Some(BougieError::Resolution { kind, .. }) if kind == "php"
    ) && format!("{err}").contains("no PHP version constraint set")
}

/// Resolve PHP inputs when no project constraint is set:
///   (1) prefer the highest already-installed interpreter (matching
///       any flavor pin), so repeat `bougie run` is fast and offline;
///   (2) else fall back to `>=8.0` — the resolver picks the latest
///       published artifact for the configured flavor.
fn default_php_inputs(paths: &Paths, project: &ProjectConfig) -> Result<(VersionLike, Flavor)> {
    let flavor = parse_flavor(project.bougie.php.flavor.as_deref())?;
    if let Some(v) = highest_installed(paths, flavor) {
        return Ok((
            VersionLike::Version(PartialVersion {
                major: v.major,
                minor: Some(v.minor),
                patch: Some(v.patch),
            }),
            flavor,
        ));
    }
    let c = Constraint::parse(">=8.0").map_err(|e| eyre!("default constraint: {e}"))?;
    Ok((VersionLike::Constraint(c), flavor))
}

fn highest_installed(paths: &Paths, flavor: Flavor) -> Option<Version> {
    let want = flavor.as_str();
    list_installed(paths)
        .ok()?
        .into_iter()
        .filter(|(_, fl)| fl == want)
        .filter_map(|(v, _)| v.parse::<Version>().ok())
        .max()
}

fn parse_flavor(s: Option<&str>) -> Result<Flavor> {
    Ok(match s {
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
    })
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

    // Pre-download xdebug et al. into the store so the first
    // server-side debug request doesn't pay the download cost. No
    // conf.d fragment is written here — see
    // `bougie_installer::baseline::PREINSTALLED_EXTENSIONS`.
    let preinstall_report = preinstall_into(
        paths,
        &installed.install_path,
        php_minor,
        installed.flavor,
        ResolveOptions::default(),
    );
    for (name, reason) in &preinstall_report.failed {
        eprintln!("warning: pre-download of {name} failed: {reason}");
    }

    let resolved_path =
        write_project_resolved(project_root, installed.version, installed.flavor)?;

    let composer_request = match project.bougie.composer.version.as_deref() {
        Some(s) => parse_composer_request(s)?,
        None => default_composer_request(),
    };
    let composer_installed: InstalledComposer =
        bougie_composer::install_composer(paths, &composer_request)?;
    write_project_resolved_composer(project_root, &composer_installed.version)?;

    let opt_out = baseline_opt_outs(project);
    replicate_install_conf_d(&installed.install_path, project_root, &opt_out)?;

    let installed_extensions = install_required_extensions(
        paths,
        project_root,
        project,
        php_minor,
        installed.flavor,
    )?;

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
        installed_extensions,
    })
}

/// Install every `require.ext-*` from `composer.json` that isn't
/// already provided by the static PHP build (`ext-pcre`, `ext-spl`,
/// …), shipped in the install's `etc/php/conf.d/` (core), or
/// replicated from the install (baseline). Returns the names of
/// extensions that were actually installed and enabled here, ordered
/// by composer.json's iteration order.
///
/// Errors are fatal: if composer.json declares `ext-redis` and bougie
/// can't satisfy it, the project would `composer install` into a
/// broken state, so sync surfaces the resolution failure instead of
/// silently continuing.
fn install_required_extensions(
    paths: &Paths,
    project_root: &std::path::Path,
    project: &ProjectConfig,
    php_minor: PartialVersion,
    flavor: Flavor,
) -> Result<Vec<String>> {
    let Some(composer) = project.composer.as_ref() else {
        return Ok(Vec::new());
    };
    let project_conf_d = project_root.join(".bougie").join("conf.d");
    let mut installed_names = Vec::new();
    // One shared bar across every composer-required extension so the
    // user sees a single combined download bar even when the project
    // pulls in several non-baseline extensions.
    let bar = DownloadBar::new("downloading");
    for name in &composer.require_extensions {
        if baseline::is_builtin(name) {
            continue;
        }
        if is_ext_enabled_in_project(&project_conf_d, name) {
            continue;
        }
        // bougie.toml's `[extensions]` table can pin or disable an
        // ext. Disabled wins for baseline (sync filtered above), but
        // here composer.json *explicitly* requires it — treat
        // Disabled as "no pin," i.e. install at latest. We do not
        // error: composer.json is the project-state source of truth,
        // [extensions]=false is a hint, not a veto.
        let version_pin = project
            .bougie
            .extensions
            .get(name)
            .and_then(ExtensionPin::as_version);

        let installed: InstalledExt = install_extension_with_bar(
            paths,
            name,
            version_pin,
            php_minor,
            flavor,
            ResolveOptions::default(),
            &bar,
        )?;
        conf_d::write_ext_fragment(
            project_root,
            &installed.name,
            &installed.so_path,
            installed.load,
            &installed.path_extras,
        )?;
        installed_names.push(installed.name);
    }
    bar.finish();
    Ok(installed_names)
}

/// `true` if the project's `.bougie/conf.d/` already has a fragment
/// that ends with `-<name>.ini` — i.e. core or baseline replication
/// already enabled it, OR a previous `bougie ext add` / sync wrote a
/// `20-<name>.ini` for it. Either way the auto-install step should
/// skip it.
///
/// Matches on the filename's trailing `-<name>` segment rather than
/// reading file contents — a `15-<name>.ini` user tunable would be a
/// false positive in theory, but `<name>` would have to *exactly*
/// match an extension that's also in `composer.json`, which would
/// itself be a configuration smell worth surfacing rather than
/// papering over.
fn is_ext_enabled_in_project(conf_d: &std::path::Path, name: &str) -> bool {
    let Ok(entries) = std::fs::read_dir(conf_d) else {
        return false;
    };
    let target_suffix = format!("-{name}.ini");
    for entry in entries.flatten() {
        let fname = entry.file_name();
        let Some(fname) = fname.to_str() else { continue };
        if fname.ends_with(&target_suffix) {
            return true;
        }
    }
    false
}

/// Resolve the project's PHP inputs (constraint + flavor). Public so
/// callers like `ext add` can drive `ensure_synced` without re-parsing.
pub fn project_php_inputs(project: &ProjectConfig) -> Result<(VersionLike, Flavor)> {
    resolve_php_inputs(project)
}

fn resolve_php_inputs(project: &ProjectConfig) -> Result<(VersionLike, Flavor)> {
    let public = match project.composer.as_ref().and_then(|c| c.require_php.clone()) {
        Some(s) => Some(
            Constraint::parse(&s)
                .map_err(|e| eyre!("composer.json require.php {s:?}: {e}"))?,
        ),
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
            let r = bougie_version::request::parse_request(v)?;
            match r {
                Request::VersionLike { spec, .. } => Ok(spec),
                _ => Err(eyre!(
                    "[php]version must be a version or constraint, not a path/tag"
                )),
            }
        })
        .transpose()?;

    let spec = intersect_php(public.as_ref(), override_spec.as_ref())?;
    let flavor = parse_flavor(project.bougie.php.flavor.as_deref())?;
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
            if name.starts_with("00-")
                && std::path::Path::new(name).extension().is_some_and(|e| e == "ini")
            {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }

    for entry in std::fs::read_dir(&src).map_err(|e| eyre!("reading {}: {e}", src.display()))? {
        let entry = entry.map_err(|e| eyre!("dir entry: {e}"))?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if std::path::Path::new(name).extension().is_none_or(|e| e != "ini") {
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

        // If an earlier sync wrote a non-00 `<NN>-<ext>.ini` for this
        // ext (back when it wasn't in the install's baseline set), it
        // now shadows the replicated 00-fragment we just wrote: both
        // files load on every PHP invocation and the second loader hits
        // `Module "<ext>" is already loaded` — opcache's variant aborts
        // with `Cannot load Zend OPcache`. Identify bougie's own
        // composer-written fragments by their header and remove them;
        // leave user-authored fragments (no header) alone.
        if let Some(ext_name) = ext_name_only(name) {
            remove_stale_composer_write_fragment(&dst, ext_name)?;
        }
    }
    Ok(())
}

/// Remove any `<NN>-<name>.ini` fragment (other than the `00-` baseline
/// replication slot) whose first line marks it as written by bougie's
/// `conf_d::write_ext_fragment` path — i.e. an `ext add`/composer-require
/// fragment that pre-dates the ext joining the install's baseline set.
/// Files without that header (user tunables, hand-authored fragments)
/// are kept.
fn remove_stale_composer_write_fragment(dir: &std::path::Path, name: &str) -> Result<()> {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(eyre!("reading {}: {e}", dir.display())),
    };
    let target_suffix = format!("-{name}.ini");
    for entry in entries.flatten() {
        let fname = entry.file_name();
        let Some(fname_str) = fname.to_str() else { continue };
        if !fname_str.ends_with(&target_suffix) {
            continue;
        }
        if fname_str.starts_with("00-") {
            // The 00-prefix slot is owned by replicate; we're about
            // to (re)write it and the cleanup loop above already
            // drops orphans there.
            continue;
        }
        let path = entry.path();
        let Ok(body) = std::fs::read_to_string(&path) else { continue };
        // `write_ext_fragment` always emits the same header prefix; use
        // a substring that doesn't appear in [`replicate_install_conf_d`]'s
        // own `; managed by bougie — do not edit` header so the two are
        // distinguishable.
        if body.contains("regenerated by `bougie ext add") {
            let _ = std::fs::remove_file(&path);
        }
    }
    Ok(())
}

/// Same shape as [`ext_name_from_fragment`] but does NOT filter by
/// `baseline::is_baseline`. Used by the stale-composer-write cleanup
/// during replicate, where we want to find the ext name for any
/// `<NN>-<name>.ini` file regardless of baseline membership (older
/// installs shipped names that aren't in today's baseline; cleanup
/// must still hit those).
fn ext_name_only(filename: &str) -> Option<&str> {
    let stem = filename.strip_suffix(".ini")?;
    let dash = stem.find('-')?;
    let (prefix, rest) = stem.split_at(dash);
    if !prefix.chars().all(|c| c.is_ascii_digit()) || prefix.is_empty() {
        return None;
    }
    Some(&rest[1..])
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

fn write_shims(project_root: &std::path::Path) -> Result<PathBuf> {
    let bin_dir = project_root.join(".bougie").join("bin");
    std::fs::create_dir_all(&bin_dir)?;
    let bougie_bin =
        std::env::current_exe().map_err(|e| eyre!("locating current executable: {e}"))?;
    // On Windows the shim is an NTFS hard link, which can't cross
    // volumes — the bougie binary may live on a different drive from
    // the project (`bougie.exe` on `C:\`, project tempdir on `D:\` is
    // the common GH-Actions shape). Stage a same-volume copy first
    // and hard-link the four shim names to *that*, so the link
    // creation is always intra-volume. The copy is skipped when
    // bytes already match — bougie ships with a stable exe size, so
    // a re-sync of an unchanged binary is just four `metadata()`
    // calls. Unix doesn't need this (`symlink` is happy across
    // mounts).
    #[cfg(not(unix))]
    let bougie_bin = stage_local_bougie(&bin_dir, &bougie_bin)?;
    // `unzip` is here because Composer's ZipDownloader does a PATH
    // lookup for it and prefers it over PHP's ZipArchive (§3.7,
    // commands::unzip). Materialising it as a sibling shim keeps the
    // composer subprocess discovery path inside `.bougie/bin/`.
    for name in ["php", "php-fpm", "composer", "unzip"] {
        // On Windows, PATH resolution wants `.exe`; on Unix the bare
        // name is what Composer's ExecutableFinder searches for.
        #[cfg(unix)]
        let link = bin_dir.join(name);
        #[cfg(not(unix))]
        let link = bin_dir.join(format!("{name}.exe"));
        if link.exists() || link.symlink_metadata().is_ok() {
            std::fs::remove_file(&link)?;
        }
        link_shim(&bougie_bin, &link)?;
    }
    Ok(bin_dir)
}

/// Copy `bougie.exe` into `<bin_dir>/_bougie-shim.exe` so the four
/// shim hard links land on the same volume as the staged binary
/// (cross-volume NTFS hard links fail with ERROR_NOT_SAME_DEVICE).
/// Returns the staged path; the four shim links point at that file.
///
/// Refreshes when either the size OR the mtime indicates the
/// canonical `bougie.exe` has moved on (`cargo install --force`,
/// `bougie self upgrade`, …). Size alone misses same-length
/// rebuilds — the symptom is `.bougie/bin/unzip.EXE` running stale
/// code after a binary upgrade and Composer's `ZipDownloader`
/// reporting "the argument '--quiet' cannot be used multiple times"
/// because the older shim didn't strip `.EXE` case-insensitively.
/// `fs::copy` to the existing path truncates in place (CREATE_ALWAYS
/// on Windows preserves the inode), so the refresh propagates to
/// every hard link transparently.
#[cfg(not(unix))]
fn stage_local_bougie(
    bin_dir: &std::path::Path,
    bougie_bin: &std::path::Path,
) -> Result<PathBuf> {
    let staged = bin_dir.join("_bougie-shim.exe");
    let needs_copy = match (std::fs::metadata(&staged), std::fs::metadata(bougie_bin)) {
        (Ok(s), Ok(b)) => {
            s.len() != b.len() || s.modified().ok() < b.modified().ok()
        }
        _ => true,
    };
    if needs_copy {
        std::fs::copy(bougie_bin, &staged)
            .map_err(|e| eyre!("copying bougie shim to {}: {e}", staged.display()))?;
    }
    Ok(staged)
}

/// Materialize a shim that re-enters the bougie binary under a
/// different `argv[0]` (see [`crate::shim`]).
///
/// Unix: symlink — cheap, role-detected from the link path's basename.
/// Windows: hard link — `std::os::windows::fs::symlink_file` requires
/// Developer Mode or admin, while NTFS hard links don't. The bougie
/// binary uses `std::env::args_os().next()` to recover `argv[0]`, and
/// Windows passes the invoked path (including `.exe`) verbatim — so
/// hardlinking `php.exe` to `bougie.exe` is enough for the shim
/// dispatcher to detect the `Role::Php` invocation.
#[cfg(unix)]
fn link_shim(target: &std::path::Path, link: &std::path::Path) -> io::Result<()> {
    std::os::unix::fs::symlink(target, link)
}

#[cfg(test)]
mod tests {
    use super::*;
    use bougie_config::BougieConfig;
    use std::collections::BTreeMap;
    use tempfile::TempDir;

    #[test]
    fn fragment_name_parsed_only_for_baseline_extensions() {
        // Only filenames in `<digits>-<name>.ini` shape with a name
        // that's in the baseline set return Some — core fragments
        // (statically built into bin/php — e.g. openssl) deliberately
        // return None so they can't be opted out via `ext-X = false`.
        assert_eq!(ext_name_from_fragment("20-readline.ini"), Some("readline"));
        assert_eq!(ext_name_from_fragment("20-ctype.ini"), Some("ctype"));
        assert_eq!(ext_name_from_fragment("20-mbstring.ini"), Some("mbstring"));
        assert_eq!(ext_name_from_fragment("20-openssl.ini"), None); // static core
        assert_eq!(ext_name_from_fragment("20-curl.ini"), None); // per-ext, not baseline
        assert_eq!(ext_name_from_fragment("notfragment.txt"), None);
        assert_eq!(ext_name_from_fragment("custom.ini"), None);
    }

    #[test]
    fn baseline_opt_outs_filters_to_baseline_disabled_only() {
        let mut exts = BTreeMap::new();
        exts.insert("readline".into(), ExtensionPin::Disabled(false));
        exts.insert("redis".into(), ExtensionPin::Disabled(false)); // not baseline
        exts.insert("ctype".into(), ExtensionPin::Version("1.0".into())); // pinned, not disabled
        let project = ProjectConfig {
            composer: None,
            bougie: BougieConfig { extensions: exts, ..Default::default() },
        };
        let out = baseline_opt_outs(&project);
        assert!(out.contains("readline"));
        assert!(!out.contains("redis"));
        assert!(!out.contains("ctype"));
    }

    #[test]
    fn is_ext_enabled_in_project_finds_replicated_and_user_fragments() {
        let td = TempDir::new().unwrap();
        let dir = td.path().join(".bougie/conf.d");
        std::fs::create_dir_all(&dir).unwrap();
        // Replicated core fragment.
        std::fs::write(dir.join("00-20-openssl.ini"), "extension=openssl\n").unwrap();
        // Replicated baseline fragment.
        std::fs::write(dir.join("00-20-mbstring.ini"), "extension=mbstring\n").unwrap();
        // User-added via ext add.
        std::fs::write(dir.join("20-redis.ini"), "extension=redis\n").unwrap();

        assert!(is_ext_enabled_in_project(&dir, "openssl"));
        assert!(is_ext_enabled_in_project(&dir, "mbstring"));
        assert!(is_ext_enabled_in_project(&dir, "redis"));
        assert!(!is_ext_enabled_in_project(&dir, "xdebug"));
        assert!(!is_ext_enabled_in_project(&dir, "pdo_mysql"));
    }

    /// Regression for #106: bougie run -- composer update used to
    /// fail with `invalid patch version component: "*"` whenever a
    /// project's composer.json declared a Composer-style wildcard in
    /// `require.php`. `bougie-semver`'s parser handles the full
    /// Composer grammar (wildcards, hyphen ranges, unions, ...), so
    /// these now parse and lower to a constraint the resolver can
    /// satisfy against bougie's exact-triple PHP versions.
    #[test]
    fn require_php_accepts_composer_wildcards_and_unions() {
        use bougie_config::ComposerJson;
        use std::collections::{BTreeMap, BTreeSet};
        for require_php in [
            "8.3.*",
            "8.*",
            "^8.0 || 8.*",
            ">=8.0",
            "^8.3",
            "7.4 - 8.4",
            ">=8.0,<9",
            "*",
        ] {
            let project = ProjectConfig {
                composer: Some(ComposerJson {
                    require_php: Some(require_php.to_string()),
                    require_extensions: BTreeSet::new(),
                    extra_bougie: None,
                    scripts: BTreeMap::new(),
                }),
                bougie: BougieConfig::default(),
            };
            let (spec, _flavor) = resolve_php_inputs(&project)
                .unwrap_or_else(|e| panic!("resolve_php_inputs({require_php:?}) failed: {e}"));
            // The wildcard cases must produce a Constraint, not a
            // bare-version VersionLike — they're ranges, not pins.
            assert!(
                matches!(spec, VersionLike::Constraint(_)),
                "{require_php:?} should produce a Constraint, got {spec:?}"
            );
        }
    }

    #[test]
    fn is_ext_enabled_handles_missing_conf_d_dir() {
        // Sync's auto-install step runs after replicate, which creates
        // the conf.d dir, but a malformed project (no .bougie dir) is
        // possible — must not panic.
        let td = TempDir::new().unwrap();
        assert!(!is_ext_enabled_in_project(
            &td.path().join("nonexistent"),
            "redis"
        ));
    }

    #[test]
    fn replicate_skips_opted_out_baseline_fragments() {
        let install = TempDir::new().unwrap();
        let project = TempDir::new().unwrap();
        let src = install.path().join("etc/php/conf.d");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("20-readline.ini"), "extension=readline\n").unwrap();
        std::fs::write(src.join("20-ctype.ini"), "extension=ctype\n").unwrap();
        std::fs::write(src.join("20-mbstring.ini"), "extension=mbstring\n").unwrap();

        let mut opt_out = BTreeSet::new();
        opt_out.insert("readline".into());
        replicate_install_conf_d(install.path(), project.path(), &opt_out).unwrap();

        let dst = project.path().join(".bougie/conf.d");
        assert!(dst.join("00-20-ctype.ini").exists());
        // mbstring isn't baseline, so it can't be opted out via this
        // path — replicate copies it through. (User-added per-ext
        // fragments are out of scope for baseline opt-out.)
        assert!(dst.join("00-20-mbstring.ini").exists());
        assert!(!dst.join("00-20-readline.ini").exists());
    }

    /// Reproduces the `Module "<ext>" is already loaded` warning seen
    /// after an ext migrates from "composer-required, not in baseline"
    /// to "in the install's baseline set." Setup: a project that has a
    /// stale `20-ftp.ini` bougie wrote on an earlier sync (composer
    /// requires ftp, ftp not yet baseline). New sync replicates the
    /// install's `20-ftp.ini` to `00-20-ftp.ini`. Without the cleanup
    /// added in [`replicate_install_conf_d`], both load and PHP warns
    /// on every invocation. With the cleanup, the stale composer-write
    /// fragment is removed and only the baseline replication survives.
    #[test]
    fn replicate_drops_stale_composer_write_fragments() {
        let install = TempDir::new().unwrap();
        let project = TempDir::new().unwrap();
        let src = install.path().join("etc/php/conf.d");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("20-ftp.ini"), "extension=ftp\n").unwrap();
        std::fs::write(src.join("10-opcache.ini"), "zend_extension=opcache\n").unwrap();

        // Project state from an earlier sync: bougie's composer-write
        // path wrote 20-ftp.ini and 10-opcache.ini (both with the
        // `regenerated by` header) when neither was in the baseline.
        let dst = project.path().join(".bougie/conf.d");
        std::fs::create_dir_all(&dst).unwrap();
        std::fs::write(
            dst.join("20-ftp.ini"),
            "; managed by bougie — do not edit; regenerated by `bougie ext add ftp`\nextension=/path/to/ftp.so\n",
        )
        .unwrap();
        std::fs::write(
            dst.join("10-opcache.ini"),
            "; managed by bougie — do not edit; regenerated by `bougie ext add opcache`\nzend_extension=/path/to/opcache.so\n",
        )
        .unwrap();
        // A user-authored tunable for ftp at a different prefix —
        // must survive cleanup because it has no bougie header.
        std::fs::write(
            dst.join("25-ftp.ini"),
            "; user-authored\nftp.timeout=120\n",
        )
        .unwrap();

        replicate_install_conf_d(install.path(), project.path(), &BTreeSet::new()).unwrap();

        // Baseline replication ran for both names.
        assert!(dst.join("00-20-ftp.ini").exists());
        assert!(dst.join("00-10-opcache.ini").exists());
        // Stale bougie composer-write fragments cleaned up — no double
        // load on the next PHP invocation.
        assert!(!dst.join("20-ftp.ini").exists());
        assert!(!dst.join("10-opcache.ini").exists());
        // User-authored fragment at a different prefix is preserved.
        assert!(dst.join("25-ftp.ini").exists());
    }
}
#[cfg(not(unix))]
fn link_shim(target: &std::path::Path, link: &std::path::Path) -> io::Result<()> {
    std::fs::hard_link(target, link)
}
