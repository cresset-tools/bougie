use bougie_installer::baseline::{self, BaselineFilter};
use bougie_cli::OutputFormat;
use bougie_composer_resolver::{install_from_lock, InstallOptions, InstallSummary};
use bougie_installer::conf_d;
use bougie_config::{load_project, ExtensionPin, ProjectConfig};
use bougie_errors::BougieError;
use bougie_fetch::DownloadBar;
use bougie_installer::install::{
    backend_for, install_baseline_into_with_backend, install_extension_with_bar,
    install_php_with_backend, preinstall_into_with_backend, InstalledExt, InstalledPhp,
};
use bougie_output::output::{emit, Render};
use bougie_paths::Paths;
use bougie_semver::Constraint;
use bougie_version::request::{Flavor, Request, VersionLike};
use bougie_resolver::{intersect_php, ResolveOptions};
use bougie_fs::state::{
    clear_project_resolved_php_path, write_project_resolved, write_project_resolved_php_path,
    GlobalState,
};
use bougie_fs::store::list_installed;
use bougie_cli::PhpPrefArgs;
use bougie_php_discovery::{
    discover, probe, select, PhpPreference, Requirement, Selection, SystemPhp,
};
use bougie_platform::target::Triple;
use bougie_version::version::{PartialVersion, Version};
use eyre::{eyre, Result, WrapErr};
use serde::Serialize;
use std::collections::BTreeSet;
use std::io::{self, Write};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Instant;

/// How sync should choose the PHP interpreter — the resolved
/// preference (CLI flags over `[php] managed` config) plus whether
/// downloading a managed PHP is permitted.
#[derive(Debug, Clone, Copy)]
pub struct PhpResolution {
    pub preference: PhpPreference,
    pub downloads: bool,
}

impl PhpResolution {
    /// Resolve from the shared `--managed-php`/`--no-managed-php`/
    /// `--no-php-downloads` flags, falling back to `[php] managed` /
    /// `[php] downloads` config.
    pub fn from_args(args: PhpPrefArgs, project: &ProjectConfig) -> Result<Self> {
        let cfg = &project.bougie.php;
        let preference =
            PhpPreference::resolve(args.managed_php, args.no_managed_php, cfg.managed)?;
        let downloads = if args.no_php_downloads {
            false
        } else {
            cfg.downloads.unwrap_or(true)
        };
        Ok(Self { preference, downloads })
    }

    /// Force a bougie-managed PHP (installed or downloaded) — used by
    /// callers that must install bougie's ABI-controlled extensions
    /// (e.g. `bougie ext add`), which a foreign system build can't take.
    pub fn only_managed() -> Self {
        Self { preference: PhpPreference::OnlyManaged, downloads: true }
    }
}

/// Whether sync selected a bougie-managed PHP or a system one.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum PhpSourceKind {
    Managed,
    System,
}

#[derive(Debug, Serialize)]
pub struct SyncResult {
    pub schema_version: u32,
    /// Whether the resolved PHP is bougie-managed or a system install.
    pub php_source: PhpSourceKind,
    pub php_version: String,
    pub php_flavor: String,
    pub install_path: PathBuf,
    pub resolved_path: PathBuf,
    pub shims_dir: PathBuf,
    /// Extensions auto-installed from `composer.json`'s `require.ext-*`
    /// — i.e. project-required extensions that weren't already provided
    /// by the core/baseline sets. Built-in (statically-linked) entries
    /// like `ext-pcre` are filtered out before this list is populated.
    pub installed_extensions: Vec<String>,
    /// Vendor packages freshly downloaded during this sync. `None` when
    /// the project has no `composer.lock` packages (empty project).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vendor_packages_installed: Option<u32>,
    /// Vendor packages that were already present in `vendor/` and
    /// matched the lock (incremental no-op). `None` same as above.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vendor_packages_up_to_date: Option<u32>,
    /// Vendor packages removed (were present but no longer in lock).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vendor_packages_removed: Option<u32>,
    /// Total packages in the resolution (`composer.lock`). Drives the
    /// uv-style `Resolved N packages` line. Filled in by `run` /
    /// `run_with_default_fallback`; `ensure_synced` leaves it at 0.
    pub resolved_packages: usize,
    /// Wall-clock of the resolution phase (lock load / resolve), in
    /// milliseconds. Filled in by the entry points, 0 from `ensure_synced`.
    pub resolve_ms: f64,
    /// Wall-clock of the vendor materialize/audit phase, in milliseconds.
    pub audit_ms: f64,
}

impl Render for SyncResult {
    fn render_text(&self, w: &mut dyn Write) -> io::Result<()> {
        use bougie_output::list_format::writeln_dim;

        // uv-style summary, printed grey (dimmed): the resolution size,
        // then what materializing vendor/ actually did. The
        // PHP/composer/shims toolchain detail is demoted to `--verbose`
        // so a steady-state sync stays terse. Color degrades to plain
        // automatically on a non-color TTY or under `NO_COLOR`.
        writeln_dim(
            w,
            &format!(
                "Resolved {} {} in {}",
                self.resolved_packages,
                plural(self.resolved_packages as u64, "package", "packages"),
                fmt_ms(self.resolve_ms),
            ),
        )?;
        match (self.vendor_packages_installed, self.vendor_packages_up_to_date) {
            (Some(installed), _) if installed > 0 => {
                let removed = self.vendor_packages_removed.unwrap_or(0);
                let removed_s = if removed > 0 {
                    format!(", {removed} removed")
                } else {
                    String::new()
                };
                writeln_dim(
                    w,
                    &format!(
                        "Installed {installed} {} in {}{removed_s}",
                        plural(u64::from(installed), "package", "packages"),
                        fmt_ms(self.audit_ms),
                    ),
                )?;
            }
            (Some(_), Some(up_to_date)) if up_to_date > 0 => {
                writeln_dim(
                    w,
                    &format!(
                        "Audited {up_to_date} {} in {}",
                        plural(u64::from(up_to_date), "package", "packages"),
                        fmt_ms(self.audit_ms),
                    ),
                )?;
            }
            _ => {}
        }

        if bougie_output::output::verbose() {
            let label = match self.php_source {
                PhpSourceKind::Managed => "php",
                PhpSourceKind::System => "system php",
            };
            writeln_dim(
                w,
                &format!(
                    "  {label} {}-{} at {}",
                    self.php_version,
                    self.php_flavor,
                    self.install_path.display()
                ),
            )?;
            if !self.installed_extensions.is_empty() {
                writeln_dim(
                    w,
                    &format!(
                        "  extensions from composer.json: {}",
                        self.installed_extensions.join(", ")
                    ),
                )?;
            }
            writeln_dim(w, &format!("  shims at {}", self.shims_dir.display()))?;
        }
        Ok(())
    }
}

/// Format an elapsed millisecond count uv-style: sub-millisecond keeps
/// two decimals (`0.55ms`), whole milliseconds print as integers
/// (`14ms`), and a second or more switches to `1.23s`.
fn fmt_ms(ms: f64) -> String {
    if ms >= 1000.0 {
        format!("{:.2}s", ms / 1000.0)
    } else if ms >= 1.0 {
        format!("{ms:.0}ms")
    } else {
        format!("{ms:.2}ms")
    }
}

fn plural(n: u64, one: &'static str, many: &'static str) -> &'static str {
    if n == 1 {
        one
    } else {
        many
    }
}

/// Ensure `composer.lock` exists before PHP inference runs. If the
/// lock is absent and `composer.json` has non-platform requires,
/// resolve and write the lock now. The lock must exist before
/// `resolve_php_inputs` so that `infer_php::infer()` can read it.
///
/// - `offline = true` + no lock → hard error.
/// - `offline = true` + lock exists → no-op (the lock is already
///   there, inference + install will use it).
/// - `dry_run = true` → report intent to stderr but skip the write.
fn ensure_lock(
    paths: &Paths,
    project_root: &std::path::Path,
    offline: bool,
    dry_run: bool,
) -> Result<()> {
    let lock_path = project_root.join("composer.lock");
    if lock_path.is_file() {
        return Ok(());
    }
    // No lock yet — check whether composer.json has any external
    // (non-platform) requires worth resolving.
    if !composer_json_has_external_requires(project_root) {
        return Ok(());
    }
    if offline {
        return Err(eyre!(
            "composer.lock not found and --offline is set; run `bougie sync` \
             online once to create it, or commit a composer.lock"
        ));
    }
    if dry_run {
        eprintln!(
            "would resolve composer.json and write composer.lock \
             (no lock file present)"
        );
        return Ok(());
    }
    eprintln!(
        "composer.lock not found; resolving composer.json \
         and writing a fresh composer.lock…"
    );
    super::composer_update::resolve_and_write_lock(paths, project_root)?;
    Ok(())
}

/// Returns `true` when `composer.json` has at least one non-platform
/// require (i.e. a `vendor/package` dependency). Platform keys (`php`,
/// `ext-*`, `lib-*`, `composer-*`) don't trigger a resolve because the
/// resolver only handles real packages.
fn composer_json_has_external_requires(project_root: &std::path::Path) -> bool {
    let Ok(text) = std::fs::read_to_string(project_root.join("composer.json")) else {
        return false;
    };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) else {
        return false;
    };
    let Some(require) = v.get("require").and_then(serde_json::Value::as_object) else {
        return false;
    };
    require.keys().any(|k| k.contains('/'))
}

/// Materialize `vendor/` from `composer.lock`. Returns `None` when
/// there is no lock file (project with no packages at all). `hooks` runs
/// opted-in root scripts during the install lifecycle (`None` = off).
fn install_vendor(
    paths: &Paths,
    project_root: &std::path::Path,
    hooks: Option<&dyn bougie_composer_resolver::ScriptHooks>,
) -> Result<Option<InstallSummary>> {
    let lock_path = project_root.join("composer.lock");
    if !lock_path.is_file() {
        return Ok(None);
    }
    let summary = install_from_lock(paths, project_root, InstallOptions { no_dev: false }, hooks)?;
    Ok(Some(summary))
}

/// Count the total number of packages in `composer.lock` (packages +
/// packages-dev). Returns 0 when the lock is absent or unparseable.
fn count_lock_packages(project_root: &std::path::Path) -> usize {
    let Ok(text) = std::fs::read_to_string(project_root.join("composer.lock")) else {
        return 0;
    };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) else {
        return 0;
    };
    let count_arr = |key: &str| -> usize {
        v.get(key)
            .and_then(serde_json::Value::as_array)
            .map_or(0, Vec::len)
    };
    count_arr("packages") + count_arr("packages-dev")
}

pub fn run(
    format: OutputFormat,
    offline: bool,
    dry_run: bool,
    scripts: Option<bool>,
    php_pref: PhpPrefArgs,
) -> Result<ExitCode> {
    let paths = Paths::from_env()?;
    let project_root = std::env::current_dir()?;

    // Step 1 — ensure a composer.lock exists before PHP inference so
    // `infer()` / `infer_extensions()` can read it. This must happen
    // before `resolve_php_inputs` (which calls `infer()`). Timed as the
    // "resolution" phase: a no-op lock read when warm, a full resolve +
    // write when the lock is missing.
    let resolve_started = Instant::now();
    ensure_lock(&paths, &project_root, offline, dry_run)?;
    let resolved_packages = count_lock_packages(&project_root);
    let resolve_ms = elapsed_ms(resolve_started);

    let project = load_project(&project_root)?;
    let (spec, flavor) = resolve_php_inputs(&project_root, &project)?;
    let resolution = PhpResolution::from_args(php_pref, &project)?;

    if dry_run {
        let lock_count = count_lock_packages(&project_root);
        eprintln!("Resolving…");
        eprintln!("would install php matching the resolved spec; flavor={flavor}");
        if lock_count > 0 {
            eprintln!("would materialize {lock_count} packages into vendor/");
        }
        return Ok(ExitCode::SUCCESS);
    }

    // Step 2 — toolchain (PHP + extensions + composer + shims).
    let result =
        ensure_synced_with(&paths, &project_root, &project, spec, flavor, resolution)?;

    // Step 3 — materialize vendor/ from the lock, running opted-in root
    // scripts during the install lifecycle. The toolchain (incl. PHP) is
    // already synced above, so the resolved PHP binary is available.
    finish_with_vendor(
        &paths,
        &project_root,
        &project,
        result,
        resolved_packages,
        resolve_ms,
        scripts,
        format,
    )
}

/// Shared tail of every sync entry point: materialize `vendor/` from the
/// lock (firing opted-in root scripts), fold the vendor + timing stats
/// into `result`, then emit it. `scripts` is the CLI `--scripts` flag
/// (`None` on the implicit `bougie run` path, which is config-driven).
#[allow(clippy::too_many_arguments)]
fn finish_with_vendor(
    paths: &Paths,
    project_root: &std::path::Path,
    project: &ProjectConfig,
    mut result: SyncResult,
    resolved_packages: usize,
    resolve_ms: f64,
    scripts: Option<bool>,
    format: OutputFormat,
) -> Result<ExitCode> {
    let hooks = if super::scripts::enabled(scripts, project) {
        Some(super::scripts::LifecycleHooks::new(
            project_root,
            true,
            super::scripts::Lifecycle::Install,
        )?)
    } else {
        None
    };
    let audit_started = Instant::now();
    let vendor_summary = install_vendor(
        paths,
        project_root,
        hooks.as_ref().map(|h| h as &dyn bougie_composer_resolver::ScriptHooks),
    )?;
    result.audit_ms = elapsed_ms(audit_started);
    result.resolved_packages = resolved_packages;
    result.resolve_ms = resolve_ms;
    if let Some(s) = vendor_summary {
        // Soft preflight findings (skipped Composer plugins, a non-empty
        // `scripts` section, …) recur on every sync for a given project,
        // so they'd drown the two-line summary. Show them only under
        // `--verbose`.
        if bougie_output::output::verbose() {
            for warning in &s.warnings {
                eprintln!("warning: {warning}");
            }
        }
        result.vendor_packages_installed =
            Some(s.packages_installed + s.packages_already_present);
        result.vendor_packages_up_to_date = Some(s.packages_up_to_date);
        result.vendor_packages_removed = Some(s.packages_removed);
    }

    emit(format, &result)?;
    Ok(ExitCode::SUCCESS)
}

/// Milliseconds elapsed since `started`, as the `f64` the uv-style
/// summary lines carry.
fn elapsed_ms(started: Instant) -> f64 {
    started.elapsed().as_secs_f64() * 1000.0
}

/// Same as [`run`] but, when neither `composer.json` nor `bougie.toml`
/// pins a PHP version *and* no inference signal fires, falls back to
/// the highest already-installed interpreter (or `>=8.0` for a fresh
/// machine) instead of erroring.
///
/// Mirrors uv's behavior for `uv run` outside a project: be useful with
/// whatever's lying around, defer the strict-constraint requirement to
/// the explicit `bougie sync` path. `bougie sync` itself still errors
/// when inference can't help — only `bougie run` opts in via this
/// entry point for the install-state fallback.
pub fn run_with_default_fallback(
    format: OutputFormat,
    dry_run: bool,
    php_pref: PhpPrefArgs,
) -> Result<ExitCode> {
    let paths = Paths::from_env()?;
    let project_root = std::env::current_dir()?;

    // Ensure lock before PHP inference (same as `run`). Never errors
    // offline here: `bougie run` is forgiving by design.
    let resolve_started = Instant::now();
    ensure_lock(&paths, &project_root, false, dry_run)?;
    let resolved_packages = count_lock_packages(&project_root);
    let resolve_ms = elapsed_ms(resolve_started);

    let project = load_project(&project_root)?;
    let (spec, flavor) = match resolve_php_inputs(&project_root, &project) {
        Ok(inputs) => inputs,
        Err(err) if is_missing_php_constraint(&err) => default_php_inputs(&paths, &project)?,
        Err(err) => return Err(err),
    };
    let resolution = PhpResolution::from_args(php_pref, &project)?;

    if dry_run {
        eprintln!("Resolving…");
        eprintln!("would install php matching the resolved spec; flavor={flavor}");
        return Ok(ExitCode::SUCCESS);
    }

    // Toolchain sync.
    let result =
        ensure_synced_with(&paths, &project_root, &project, spec, flavor, resolution)?;

    // Vendor install. Root scripts are config-driven here (no CLI flag on
    // the `bougie run` implicit-sync path) — only `[scripts] run = true`
    // enables them.
    finish_with_vendor(
        &paths,
        &project_root,
        &project,
        result,
        resolved_packages,
        resolve_ms,
        None,
        format,
    )
}

/// Toolchain + vendor sync driven by an explicit `--php` request
/// (`bougie run --php <ver|constraint|path>`), overriding whatever PHP
/// the project would otherwise infer. The uv `--python` analog.
///
/// - A version or constraint (`8.3`, `~8.3`, `php8.3`, `8.3z`) overrides
///   the resolved spec/flavor and goes through the normal managed/system
///   selection (honoring `--managed-php`/`--no-managed-php`).
/// - A path (`/opt/php/bin/php`, `~/php/bin/php`) is probed directly and
///   wired in as a system PHP, no download or install tree.
/// - Full tags and bare PATH names are rejected with a pointer to the
///   supported forms.
pub fn run_with_php_request(
    format: OutputFormat,
    dry_run: bool,
    php_pref: PhpPrefArgs,
    request: &Request,
) -> Result<ExitCode> {
    let paths = Paths::from_env()?;
    let project_root = std::env::current_dir()?;

    let resolve_started = Instant::now();
    ensure_lock(&paths, &project_root, false, dry_run)?;
    let resolved_packages = count_lock_packages(&project_root);
    let resolve_ms = elapsed_ms(resolve_started);

    let project = load_project(&project_root)?;

    if dry_run {
        eprintln!("Resolving…");
        eprintln!("would install php matching --php {request:?}");
        return Ok(ExitCode::SUCCESS);
    }

    let result = match request {
        Request::VersionLike { spec, flavor } => {
            // A flavor carried by the request (`8.3z`) wins; otherwise
            // fall back to the project's configured flavor.
            let flavor = match flavor {
                Some(f) => *f,
                None => parse_flavor(project.bougie.php.flavor.as_deref())?,
            };
            let resolution = PhpResolution::from_args(php_pref, &project)?;
            ensure_synced_with(
                &paths,
                &project_root,
                &project,
                spec.clone(),
                flavor,
                resolution,
            )?
        }
        Request::Path(path) => {
            let expanded = expand_tilde(path);
            let system = probe(&expanded)
                .wrap_err_with(|| format!("probing the PHP binary at {}", expanded.display()))?;
            ensure_synced_system(&paths, &project_root, &system)?
        }
        Request::FullTag { .. } | Request::Name(_) => {
            return Err(eyre!(
                "`bougie run --php` accepts a version (`8.3`, `8.3.12`), a \
                 constraint (`~8.3`, `>=8.2,<8.4`), or a path to a `php` binary; \
                 full tags and bare PATH names are not supported here"
            ));
        }
    };

    finish_with_vendor(
        &paths,
        &project_root,
        &project,
        result,
        resolved_packages,
        resolve_ms,
        None,
        format,
    )
}

/// Expand a leading `~` / `~/` to the user's home directory. Any other
/// path (including `~user`) is returned unchanged — `probe` will surface
/// a clear "can't execute" error if it doesn't resolve to a real binary.
fn expand_tilde(path: &std::path::Path) -> PathBuf {
    let Some(s) = path.to_str() else {
        return path.to_path_buf();
    };
    let Some(home) = std::env::var_os("HOME") else {
        return path.to_path_buf();
    };
    if s == "~" {
        PathBuf::from(home)
    } else if let Some(rest) = s.strip_prefix("~/") {
        PathBuf::from(home).join(rest)
    } else {
        path.to_path_buf()
    }
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
    // Default callers (the implicit `ext add` sync) keep the historical
    // managed-only behavior; `bougie sync`/`bougie run` pass an explicit
    // resolution via `ensure_synced_with`.
    ensure_synced_with(paths, project_root, project, spec, flavor, PhpResolution::only_managed())
}

/// [`ensure_synced`] with an explicit [`PhpResolution`]: pick a
/// managed-installed, system, or to-be-downloaded PHP per the
/// preference, then sync that source.
pub fn ensure_synced_with(
    paths: &Paths,
    project_root: &std::path::Path,
    project: &ProjectConfig,
    spec: VersionLike,
    flavor: Flavor,
    resolution: PhpResolution,
) -> Result<SyncResult> {
    migrate_legacy_layout(paths, project_root);
    let required_exts = required_ext_names(project);
    let managed_installed = gather_managed_installed(paths);
    // Probe system PHPs only when one could actually be selected: never
    // under OnlyManaged, and under the default (Managed) only when no
    // installed managed PHP already satisfies the request (managed-
    // installed wins, so a warm managed sync/run pays no `php` spawns).
    let need_system = match resolution.preference {
        PhpPreference::OnlySystem => true,
        PhpPreference::OnlyManaged => false,
        PhpPreference::Managed => {
            !managed_installed.iter().any(|(v, f)| {
                *f == flavor && bougie_version::matches::version_satisfies(v, &spec)
            })
        }
    };
    let system = if need_system {
        gather_system(resolution.preference)
    } else {
        Vec::new()
    };

    let requirement = Requirement {
        spec: Some(&spec),
        flavor,
        required_exts: &required_exts,
    };
    let selection = select(
        resolution.preference,
        resolution.downloads,
        requirement,
        &managed_installed,
        &system,
    )?;

    match selection {
        Selection::System(system_php) => {
            ensure_synced_system(paths, project_root, &system_php)
        }
        // Both managed tiers go through the same install path:
        // `install_php_with_backend` reuses an installed match and
        // downloads otherwise (selection already enforced the download
        // gate, so this only downloads when permitted).
        Selection::ManagedInstalled { .. } | Selection::Download => {
            ensure_synced_managed(paths, project_root, project, spec, flavor)
        }
    }
}

/// Sync against a **system** PHP: no download, no install tree, no
/// extension install (selection already guaranteed every required
/// `ext-*` is loaded). Write both resolved markers and the shims.
fn ensure_synced_system(
    paths: &Paths,
    project_root: &std::path::Path,
    system_php: &SystemPhp,
) -> Result<SyncResult> {
    let resolved_path =
        write_project_resolved(project_root, system_php.version, system_php.flavor)?;
    write_project_resolved_php_path(project_root, &system_php.path)?;

    let shims_dir = write_shims(project_root)?;
    seed_global_composer_shim(paths);

    let mut global = GlobalState::load(paths)?;
    global.host_target = Some(Triple::detect()?.to_string());
    global.touch_project(project_root);
    global.save(paths)?;

    Ok(SyncResult {
        schema_version: 3,
        php_source: PhpSourceKind::System,
        php_version: system_php.version.to_string(),
        php_flavor: system_php.flavor.to_string(),
        install_path: system_php.path.clone(),
        resolved_path,
        shims_dir,
        installed_extensions: Vec::new(),
        vendor_packages_installed: None,
        vendor_packages_up_to_date: None,
        vendor_packages_removed: None,
        resolved_packages: 0,
        resolve_ms: 0.0,
        audit_ms: 0.0,
    })
}

fn ensure_synced_managed(
    paths: &Paths,
    project_root: &std::path::Path,
    project: &ProjectConfig,
    spec: VersionLike,
    flavor: Flavor,
) -> Result<SyncResult> {
    // Drop any stale system-PHP marker so a project switching back to a
    // managed PHP stops resolving the old system binary in the shim.
    clear_project_resolved_php_path(project_root)?;
    let request = Request::VersionLike { spec, flavor: Some(flavor) };
    // Build one backend for the whole toolchain phase. It memoizes the
    // signed index root on first use, so the interpreter, baseline, and
    // preinstall resolves below share a single conditional GET instead
    // of one per resolve — a warm sync used to stall on ~30 sequential
    // `If-None-Match` round-trips (the "stuck installing exif" symptom).
    let backend = backend_for(paths)?;
    let installed: InstalledPhp =
        install_php_with_backend(&*backend, paths, &request, Some(flavor), ResolveOptions::default())?;

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
    let baseline_report = install_baseline_into_with_backend(
        &*backend,
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
    let preinstall_report = preinstall_into_with_backend(
        &*backend,
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
    // Make Composer a default tool: seed a global `composer` on the
    // user's PATH (tool bin dir) so `composer …` works from any shell,
    // routed through bougie's project-aware shim. Best-effort and
    // collision-safe — see `seed_global_composer_shim`.
    seed_global_composer_shim(paths);

    let mut global = GlobalState::load(paths)?;
    global.host_target = Some(Triple::detect()?.to_string());
    global.touch_project(project_root);
    global.save(paths)?;

    Ok(SyncResult {
        schema_version: 3,
        php_source: PhpSourceKind::Managed,
        php_version: installed.version.to_string(),
        php_flavor: installed.flavor.to_string(),
        install_path: installed.install_path,
        resolved_path,
        shims_dir,
        installed_extensions,
        // Vendor stats + resolution timing are filled in by the `run` /
        // `run_with_default_fallback` callers after they call
        // `install_vendor`. `ensure_synced` is reused by `ext add` which
        // does NOT touch vendor/, so we leave these zero/None here and
        // let the two entry-point functions set them.
        vendor_packages_installed: None,
        vendor_packages_up_to_date: None,
        vendor_packages_removed: None,
        resolved_packages: 0,
        resolve_ms: 0.0,
        audit_ms: 0.0,
    })
}

/// The project's declared `require.ext-*` short names (without the
/// `ext-` prefix) — the set a **system** PHP must already load to
/// qualify (the ext-gate). Inferred/baseline extensions are not part of
/// the gate: a system PHP can't take bougie's prebuilt `.so`, so the
/// honest contract is "the system PHP already has everything declared."
fn required_ext_names(project: &ProjectConfig) -> Vec<String> {
    project
        .composer
        .as_ref()
        .map(|c| c.require_extensions.iter().cloned().collect())
        .unwrap_or_default()
}

/// Installed managed `(version, flavor)` pairs, parsed from the
/// `installs/` directory layout.
fn gather_managed_installed(paths: &Paths) -> Vec<(Version, Flavor)> {
    list_installed(paths)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|(v, f)| {
            let version = v.parse::<Version>().ok()?;
            let flavor = parse_flavor(Some(&f)).ok()?;
            Some((version, flavor))
        })
        .collect()
}

/// Discover + probe system PHPs. Skipped entirely under `OnlyManaged`
/// (no need to spawn `php` when system PHPs can never be chosen). Probe
/// failures (a non-PHP binary on PATH) are dropped silently.
fn gather_system(preference: PhpPreference) -> Vec<SystemPhp> {
    if preference == PhpPreference::OnlyManaged {
        return Vec::new();
    }
    discover().iter().filter_map(|p| probe(p).ok()).collect()
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
    let project_conf_d = bougie_paths::project::confd(project_root);
    let mut installed_names = Vec::new();

    // Inferred extensions (Magento recommended set ∪ ext-* listed in
    // composer.lock) are added on top of whatever the project already
    // declared. User-declared entries always take precedence:
    //
    // - explicit `require.ext-*` in composer.json is honored as-is
    //   (the loop below picks up version pins from `[extensions]`);
    // - `[extensions] = false` opts a name out, so we filter inferred
    //   names against it.
    let (inferred_names, sources) = super::infer_php::infer_extensions(project_root);
    let inferred: BTreeSet<String> = inferred_names
        .into_iter()
        .filter(|n| !composer.require_extensions.contains(n))
        .filter(|n| {
            project
                .bougie
                .extensions
                .get(n)
                .is_none_or(|p| !p.is_disabled())
        })
        .collect();
    // Diagnostic only — the actual install outcome is reported by
    // `SyncResult.installed_extensions`. This line restates the *inferred*
    // set on every sync (even when all are already enabled), so gate it
    // behind `--verbose` to keep steady-state `run`/`make` quiet.
    if !inferred.is_empty() && bougie_output::output::verbose() {
        eprintln!(
            "adding inferred extensions from {}: {}",
            sources.join(" + "),
            inferred
                .iter()
                .filter(|n| !baseline::is_builtin(n))
                .cloned()
                .collect::<Vec<_>>()
                .join(", "),
        );
    }
    let mut effective: BTreeSet<String> = composer.require_extensions.clone();
    effective.extend(inferred);

    // One shared bar across every composer-required extension so the
    // user sees a single combined download bar even when the project
    // pulls in several non-baseline extensions.
    let bar = DownloadBar::new("downloading");
    for name in &effective {
        if baseline::is_builtin(name) {
            continue;
        }
        // Baseline extensions that are static in this PHP minor (e.g.
        // opcache on 8.5+) have no downloadable artifact and are
        // already active — no conf.d fragment needed.
        if baseline::skip_for_php_minor(name, php_minor) {
            continue;
        }
        // Skip only when the ext is already enabled *and* its fragment
        // targets the active interpreter minor. A fragment left over from
        // a previous PHP pin points at an ABI-incompatible per-minor store
        // path, so re-resolve and rewrite it instead of skipping (the
        // `bougie sync` re-pin path — issue #360).
        if is_ext_enabled_in_project(&project_conf_d, name)
            && !ext_fragment_is_stale(&project_conf_d, name, php_minor)
        {
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

/// `true` if the project's `vendor/bougie/conf.d/` already has a fragment
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

/// `true` if a conf.d fragment enabling `<name>` points at a per-PHP-minor
/// store path that doesn't match `php_minor`. Extension `.so`s are
/// ABI-specific per interpreter minor — the store dir is keyed
/// `…+php<major><minor>…` (see `install_extension_resolved`) — so a
/// fragment left behind by a previous PHP pin fails to `dlopen` on the
/// newly-active interpreter (Zend ABI mismatch). When that happens
/// `install_required_extensions` must re-resolve the ext against the
/// active minor and rewrite the fragment rather than skip it (issue #360).
///
/// Only fragments carrying a `+php<NN>` store token are flagged.
/// Baseline-replicated (`00-*`) fragments point into the install tree and
/// are regenerated every sync, and core fragments carry no such token —
/// both read as "not stale" and stay skipped.
fn ext_fragment_is_stale(
    conf_d: &std::path::Path,
    name: &str,
    php_minor: PartialVersion,
) -> bool {
    let want = format!("+php{}{}", php_minor.major, php_minor.minor.unwrap_or(0));
    let Ok(entries) = std::fs::read_dir(conf_d) else {
        return false;
    };
    let target_suffix = format!("-{name}.ini");
    for entry in entries.flatten() {
        let fname = entry.file_name();
        let Some(fname) = fname.to_str() else { continue };
        if !fname.ends_with(&target_suffix) {
            continue;
        }
        let Ok(body) = std::fs::read_to_string(entry.path()) else {
            continue;
        };
        for line in body.lines() {
            let line = line.trim();
            if !(line.starts_with("extension=") || line.starts_with("zend_extension=")) {
                continue;
            }
            if let Some(token) = php_minor_token(line)
                && token != want
            {
                return true;
            }
        }
    }
    false
}

/// Extract the `+php<digits>` per-minor store token from an `extension=`
/// directive line, if present. Returns `None` for fragments whose `.so`
/// path carries no such token (core / baseline-replicated fragments).
fn php_minor_token(line: &str) -> Option<String> {
    let idx = line.find("+php")?;
    let digits: String = line[idx + "+php".len()..]
        .chars()
        .take_while(char::is_ascii_digit)
        .collect();
    if digits.is_empty() {
        return None;
    }
    Some(format!("+php{digits}"))
}

/// Resolve the project's PHP inputs (constraint + flavor). Public so
/// callers like `ext add` can drive `ensure_synced` without re-parsing.
pub fn project_php_inputs(
    project_root: &std::path::Path,
    project: &ProjectConfig,
) -> Result<(VersionLike, Flavor)> {
    resolve_php_inputs(project_root, project)
}

fn resolve_php_inputs(
    project_root: &std::path::Path,
    project: &ProjectConfig,
) -> Result<(VersionLike, Flavor)> {
    let mut public = match project.composer.as_ref().and_then(|c| c.require_php.clone()) {
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

    // When neither side pins PHP, try to infer a reasonable constraint
    // from on-disk signals (Magento recipe, then composer.lock). If
    // anything fires, treat it as the public constraint so the rest of
    // the pipeline behaves identically to a user-written `require.php`.
    if public.is_none() && override_spec.is_none() {
        if let Some((inferred, source)) = super::infer_php::infer(project_root) {
            // Diagnostic only; fires on every sync that infers PHP. Quiet
            // by default, shown with `--verbose`.
            if bougie_output::output::verbose() {
                eprintln!("inferred php constraint from {source}");
            }
            public = Some(inferred);
        }
    }

    let spec = intersect_php(public.as_ref(), override_spec.as_ref())?;
    let flavor = parse_flavor(project.bougie.php.flavor.as_deref())?;
    Ok((spec, flavor))
}

/// Copy `<install>/etc/php/conf.d/*.ini` into `<project>/vendor/bougie/conf.d/`
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
    let dst = bougie_paths::project::confd(project_root);
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

/// Seed (or refresh) a global `composer` entry in the tool bin dir
/// (`~/.local/bin` by default) so a bare `composer` resolves to bougie
/// from any shell — i.e. Composer behaves as a default-installed tool,
/// routed through the project-aware shim (`shim::run_composer`).
///
/// Best-effort: a failure here must never fail `bougie sync`.
/// Collision-safe: only ever refreshes a symlink bougie itself placed
/// (one whose target is a `bougie` binary); a user's own `composer`
/// (a real phar, or a symlink they made) is left untouched.
#[cfg(unix)]
fn seed_global_composer_shim(paths: &Paths) {
    if let Err(e) = try_seed_global_composer_shim(paths) {
        eprintln!("warning: could not seed global `composer` shim: {e}");
    }
}

#[cfg(not(unix))]
fn seed_global_composer_shim(_paths: &Paths) {}

#[cfg(unix)]
fn try_seed_global_composer_shim(paths: &Paths) -> Result<()> {
    let bin_dir = paths.tool_bin_dir();
    let link = bin_dir.join("composer");
    if let Ok(meta) = link.symlink_metadata() {
        let owned = meta.file_type().is_symlink()
            && std::fs::read_link(&link)
                .ok()
                .and_then(|target| {
                    target
                        .file_stem()
                        .map(|stem| stem.eq_ignore_ascii_case("bougie"))
                })
                .unwrap_or(false);
        if !owned {
            // A recurring, benign condition (the user has their own
            // `composer` on PATH) — keep it out of the steady-state
            // summary; surface it only under `--verbose`.
            if bougie_output::output::verbose() {
                eprintln!(
                    "warning: not seeding a global `composer` — {} already exists \
                     and was not created by bougie",
                    link.display()
                );
            }
            return Ok(());
        }
        std::fs::remove_file(&link)?;
    }
    std::fs::create_dir_all(&bin_dir)?;
    let bougie_bin =
        std::env::current_exe().map_err(|e| eyre!("locating current executable: {e}"))?;
    std::os::unix::fs::symlink(&bougie_bin, &link)?;
    Ok(())
}

/// One-shot migration from the pre-vendor layout where the project
/// toolchain lived in a top-level `<root>/.bougie/`. Everything in that
/// tree is regenerated by this very sync under `vendor/bougie/`, with
/// one exception: `conf.d-local/` holds machine-local `--so` fragments
/// that aren't recorded anywhere else, and now lives under
/// `$BOUGIE_HOME`. Rescue it (if not already migrated), then drop the
/// stale top-level dir so project-root detection and tooling don't see
/// two layouts. Best-effort throughout — a failure to migrate leaves
/// the old dir untouched rather than risking data loss.
fn migrate_legacy_layout(paths: &Paths, project_root: &std::path::Path) {
    let legacy = project_root.join(".bougie");
    if !legacy.is_dir() {
        return;
    }
    let legacy_local = legacy.join("conf.d-local");
    let dest_local = paths.project_confd_local(project_root);
    // Whether conf.d-local is safely preserved (nothing to rescue, the
    // destination already holds it, or we copied it across just now).
    let rescued = if legacy_local.is_dir() {
        dest_local.exists() || copy_flat_dir(&legacy_local, &dest_local).is_ok()
    } else {
        true
    };
    if rescued && std::fs::remove_dir_all(&legacy).is_ok() {
        eprintln!("bougie: migrated project to vendor/bougie/ (removed legacy .bougie/)");
    }
}

/// Copy every regular file from a flat directory into `dst`, creating
/// `dst` (and parents). Used to move `conf.d-local/` across filesystems
/// where a plain rename would fail with `EXDEV`. The dir is flat (only
/// `<NN>-<name>.ini` fragments), so no recursion is needed.
fn copy_flat_dir(src: &std::path::Path, dst: &std::path::Path) -> Result<()> {
    std::fs::create_dir_all(dst)
        .map_err(|e| eyre!("creating {}: {e}", dst.display()))?;
    for entry in std::fs::read_dir(src).map_err(|e| eyre!("reading {}: {e}", src.display()))? {
        let entry = entry.map_err(|e| eyre!("reading entry in {}: {e}", src.display()))?;
        if entry.file_type().is_ok_and(|t| t.is_file()) {
            let to = dst.join(entry.file_name());
            std::fs::copy(entry.path(), &to)
                .map_err(|e| eyre!("copying {} → {}: {e}", entry.path().display(), to.display()))?;
        }
    }
    Ok(())
}

fn write_shims(project_root: &std::path::Path) -> Result<PathBuf> {
    let bin_dir = bougie_paths::project::bin_dir(project_root);
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
    // composer subprocess discovery path inside `vendor/bougie/bin/`.
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
/// rebuilds — the symptom is `vendor/bougie/bin/unzip.EXE` running stale
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

    #[test]
    fn expand_tilde_rewrites_home_prefix_only() {
        use std::path::Path;
        // Absolute and relative paths pass through untouched (no HOME
        // dependency), as does `~user` — we don't resolve other users.
        assert_eq!(
            expand_tilde(Path::new("/opt/php/bin/php")),
            PathBuf::from("/opt/php/bin/php")
        );
        assert_eq!(expand_tilde(Path::new("~bob/php")), PathBuf::from("~bob/php"));
        // Reads ambient HOME rather than mutating it (which would race
        // other tests in this binary). CI always sets HOME.
        if let Some(home) = std::env::var_os("HOME") {
            let home = PathBuf::from(home);
            assert_eq!(expand_tilde(Path::new("~")), home);
            assert_eq!(
                expand_tilde(Path::new("~/php8.3/bin/php")),
                home.join("php8.3/bin/php")
            );
        }
    }
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
        assert_eq!(ext_name_from_fragment("20-curl.ini"), Some("curl"));
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
    fn migrate_legacy_layout_rescues_conf_d_local_and_clears_old_dir() {
        let proj = TempDir::new().unwrap();
        let home = TempDir::new().unwrap();
        let paths = Paths::new(home.path().to_path_buf(), home.path().to_path_buf());

        // Old layout: a top-level `.bougie/` with a machine-local
        // fragment (the one thing that can't be regenerated) plus some
        // disposable state.
        let legacy_local = proj.path().join(".bougie").join("conf.d-local");
        std::fs::create_dir_all(&legacy_local).unwrap();
        std::fs::write(legacy_local.join("20-tideways.ini"), "extension=/x/tideways.so\n").unwrap();
        std::fs::create_dir_all(proj.path().join(".bougie").join("bin")).unwrap();

        migrate_legacy_layout(&paths, proj.path());

        // conf.d-local moved under $BOUGIE_HOME, keyed by project hash.
        let dest = paths.project_confd_local(proj.path());
        assert!(dest.join("20-tideways.ini").is_file(), "fragment must survive");
        // The stale top-level dir is gone.
        assert!(!proj.path().join(".bougie").exists(), "legacy .bougie/ must be removed");
    }

    #[test]
    fn migrate_legacy_layout_is_noop_without_old_dir() {
        let proj = TempDir::new().unwrap();
        let home = TempDir::new().unwrap();
        let paths = Paths::new(home.path().to_path_buf(), home.path().to_path_buf());
        // No top-level `.bougie/` → nothing happens, no panic.
        migrate_legacy_layout(&paths, proj.path());
        assert!(!paths.project_confd_local(proj.path()).exists());
    }

    #[test]
    fn is_ext_enabled_in_project_finds_replicated_and_user_fragments() {
        let td = TempDir::new().unwrap();
        let dir = td.path().join("vendor/bougie/conf.d");
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

    fn pv(major: u32, minor: u32) -> PartialVersion {
        PartialVersion { major, minor: Some(minor), patch: None }
    }

    #[test]
    fn php_minor_token_extracts_store_label() {
        assert_eq!(
            php_minor_token("extension=/s/ext-protobuf-5.35.1+php85-nts-abc1234/protobuf.so"),
            Some("+php85".to_string())
        );
        assert_eq!(
            php_minor_token("zend_extension=/s/ext-xdebug-3.4.0+php81-nts-deadbeef/xdebug.so"),
            Some("+php81".to_string())
        );
        // Core / baseline-replicated fragments carry no store token.
        assert_eq!(php_minor_token("extension=protobuf"), None);
        assert_eq!(php_minor_token("extension=/install/etc/redis.so"), None);
    }

    #[test]
    fn ext_fragment_is_stale_detects_mismatched_minor() {
        // The #360 repro: a `20-protobuf.ini` written for php85 lingers
        // after the project repins to ~8.1 — its `.so` is ABI-incompatible
        // with the 8.1 interpreter and must be re-resolved.
        let td = TempDir::new().unwrap();
        let dir = td.path().to_path_buf();
        std::fs::write(
            dir.join("20-protobuf.ini"),
            "; managed by bougie\nextension=/s/ext-protobuf-5.35.1+php85-nts-abc1234/protobuf.so\n",
        )
        .unwrap();

        assert!(ext_fragment_is_stale(&dir, "protobuf", pv(8, 1)));
        // Same fragment is *not* stale against the minor it was built for.
        assert!(!ext_fragment_is_stale(&dir, "protobuf", pv(8, 5)));
    }

    #[test]
    fn ext_fragment_is_stale_ignores_tokenless_fragments() {
        // Baseline-replicated (`00-*`) and core fragments point into the
        // install tree with no `+php<NN>` token — they're regenerated each
        // sync, so they must never be flagged stale (which would force a
        // pointless re-resolve).
        let td = TempDir::new().unwrap();
        let dir = td.path().to_path_buf();
        std::fs::write(dir.join("00-20-mbstring.ini"), "extension=mbstring\n").unwrap();
        std::fs::write(dir.join("00-20-intl.ini"), "extension=/install/etc/intl.so\n").unwrap();

        assert!(!ext_fragment_is_stale(&dir, "mbstring", pv(8, 1)));
        assert!(!ext_fragment_is_stale(&dir, "intl", pv(8, 1)));
        // No fragment at all → not stale.
        assert!(!ext_fragment_is_stale(&dir, "redis", pv(8, 1)));
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
            let td = TempDir::new().unwrap();
            let (spec, _flavor) = resolve_php_inputs(td.path(), &project)
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

        let dst = project.path().join("vendor/bougie/conf.d");
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
        let dst = project.path().join("vendor/bougie/conf.d");
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
