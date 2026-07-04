use bougie_cli::OutputFormat;
use crate::commands::sync;
use bougie_installer::conf_d;
#[cfg(unix)]
use bougie_config::read_composer_json;
use bougie_errors::BougieError;
use bougie_paths::Paths;
use bougie_fs::state::read_project_resolved;
use eyre::{eyre, Result, WrapErr};
#[cfg(unix)]
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::ExitCode;

#[cfg(unix)]
const PATH_SEP: &str = ":";
#[cfg(not(unix))]
const PATH_SEP: &str = ";";

/// `bougie run [--] <cmd> [args...]` — set `PATH` and `PHP_INI_SCAN_DIR`,
/// then exec the requested command. Per CLI.md §3.4, implicitly runs
/// `bougie sync` first unless `--no-sync` is passed.
///
/// Debug overlay: when `xdebug_flag` is set or the parent's
/// `XDEBUG_SESSION` env var is non-empty, `PHP_INI_SCAN_DIR` is
/// widened to include `vendor/bougie/conf.d-debug/` so server-installed
/// debug fragments load for this invocation too. With the explicit
/// `--xdebug` flag, bougie also exports `XDEBUG_SESSION=1` to the
/// child (if not already set) and lazily installs xdebug into
/// `conf.d-debug/` when no xdebug fragment exists in either dir.
pub fn run(
    with: &[String],
    argv: &[String],
    format: OutputFormat,
    no_sync: bool,
    xdebug_flag: bool,
    php_pref: bougie_cli::PhpPrefArgs,
    php_request: Option<&str>,
) -> Result<ExitCode> {
    if argv.is_empty() {
        return Err(eyre!("nothing to run"));
    }
    // uv-parity: a path-shaped argv[0] with an inline `# /// script` block
    // runs self-contained even without `--script` (`bougie run ./app.php`).
    if let Some(result) =
        try_inline_script(argv, format, php_request, with, xdebug_flag, php_pref)
    {
        return result;
    }
    // The ad-hoc extension overlay (`--with EXT=VER`) is not implemented
    // yet. It used to be silently ignored, which made the command lie:
    // the script ran without the requested extension while the CLI
    // reported success. Fail loudly until the overlay lands rather than
    // pretend it worked. For a persistent extension, use
    // `bougie ext add <name>` then `bougie run`.
    if !with.is_empty() {
        return Err(eyre!(
            "`bougie run --with` (ad-hoc per-invocation extensions) is not implemented yet; \
             add the extension to the project with `bougie ext add {}` and re-run",
            with.join(" "),
        ));
    }
    let cwd = std::env::current_dir()?;
    let project_root = resolve_project_root(&cwd);
    let ephemeral_php = implicit_sync(&project_root, format, no_sync, php_pref, php_request)?;

    // The xdebug overlay loads a bougie-managed `xdebug.so`, which a
    // foreign system build can't take — and the overlay rides on
    // `PHP_INI_SCAN_DIR`, which is skipped for an ephemeral system PHP.
    // Refuse loudly rather than silently running without xdebug.
    if xdebug_flag && let Some(php) = &ephemeral_php {
        return Err(eyre!(
            "`--xdebug` needs a managed PHP, but this run resolved to the system PHP at {} \
             (used for this run only); pass `--managed-php`, or run `bougie sync` first to \
             pin a managed PHP",
            php.display()
        ));
    }

    // composer.json `scripts.<name>` lookup. Skipped when the user
    // explicitly typed `--` after `run` (their signal to bypass the
    // script table) or when `argv[0]` looks like a path. Found
    // scripts execute as `/bin/sh -e -c` with the same PATH /
    // PHP_INI_SCAN_DIR / BOUGIE_SERVICE_* env the exec path injects;
    // extra positional args (`bougie run test foo bar`) flow into
    // `$@` so scripts can act on them.
    //
    // Unix-only: the runner is `/bin/sh`, and the daemon isn't built
    // on Windows in Phase 1. On Windows the argv falls through to the
    // direct-exec path below.
    #[cfg(unix)]
    if !explicit_passthrough() && !argv[0].contains('/')
        && let Some(steps) = lookup_composer_script(&project_root, &argv[0])? {
            return run_composer_script(
                &project_root,
                &argv[0],
                &steps,
                &argv[1..],
                xdebug_flag,
                ephemeral_php.as_deref(),
            );
        }

    let bougie_bin = bougie_paths::project::bin_dir(&project_root);
    // Needed to locate the durable `conf.d-local/` under `$BOUGIE_HOME`.
    let paths = Paths::from_env()?;

    let env_session_set = std::env::var_os("XDEBUG_SESSION")
        .is_some_and(|v| !v.is_empty());
    let debug_overlay = xdebug_flag || env_session_set;

    // Lazy-install xdebug into the debug overlay so `--xdebug` works
    // on a fresh project without the user having to `bougie ext add
    // xdebug` first. Only triggered by the explicit flag (the env-var
    // path is "the request was *already* set up for xdebug
    // elsewhere"; demanding bougie install something then would be a
    // surprise).
    if xdebug_flag && !conf_d::fragment_present_anywhere(&paths, &project_root, "xdebug") {
        install_xdebug_into_overlay(&project_root)
            .wrap_err("installing xdebug for `bougie run --xdebug`")?;
    }

    let scan_dir = conf_d::php_ini_scan_dir(&paths, &project_root, debug_overlay);

    let prev_path = std::env::var("PATH").unwrap_or_default();
    // Front-load the bougie shim dir, then any per-extension PATH
    // extras the conf.d fragments asked for. Windows imagick is the
    // canonical case: its store dir holds ~170 ImageMagick DLLs
    // (`CORE_RL_*.dll`, `IM_MOD_RL_*.dll`) the Windows loader needs to
    // find when `php_imagick.dll` initializes. Unix conf.d fragments
    // don't emit `bougie-path:` markers (RPATH handles deps there),
    // so the loop is a cheap no-op.
    let path_extras = conf_d::read_path_extras(&project_root);
    let mut new_path = String::new();
    if bougie_bin.exists() {
        new_path.push_str(&bougie_bin.display().to_string());
    }
    // Overlay the project's Node.js `bin/` (node/npm/npx) when the
    // project declares node use (package.json / .nvmrc / .node-version /
    // a node-build composer dep like Hyvä). No-op for pure-PHP projects,
    // so PATH is untouched there.
    if let Ok(paths) = Paths::from_env()
        && let Some(node_bin) = super::node::project_bin_dir(&project_root, &paths)
    {
        if !new_path.is_empty() {
            new_path.push_str(PATH_SEP);
        }
        new_path.push_str(&node_bin.display().to_string());
    }
    for extra in &path_extras {
        if !new_path.is_empty() {
            new_path.push_str(PATH_SEP);
        }
        new_path.push_str(&extra.display().to_string());
    }
    if !prev_path.is_empty() {
        if !new_path.is_empty() {
            new_path.push_str(PATH_SEP);
        }
        new_path.push_str(&prev_path);
    }

    let (program, rest) = argv.split_first().ok_or_else(|| eyre!("argv missing"))?;
    let mut cmd = std::process::Command::new(program);
    cmd.args(rest)
        .env("PATH", new_path)
        .env("BOUGIE_PROJECT_ROOT", &project_root);
    match &ephemeral_php {
        // One-off system PHP: hand the interpreter to the shim via env
        // (nothing is pinned in project state), and leave
        // PHP_INI_SCAN_DIR unset — the project's conf.d fragments load
        // bougie-managed `.so`s a foreign build can't take.
        Some(php) => {
            cmd.env("BOUGIE_RUN_SYSTEM_PHP", php);
        }
        None => {
            cmd.env("PHP_INI_SCAN_DIR", &scan_dir);
        }
    }
    if xdebug_flag && !env_session_set {
        cmd.env("XDEBUG_SESSION", "1");
    }
    // Layer in any per-tenant `BOUGIE_SERVICE_*` env vars the daemon
    // knows about. Only when `bougied` is already running — `bougie
    // run` deliberately does NOT auto-spawn the daemon (the user
    // explicitly chose `bougie service up` for that). When the
    // daemon isn't there the vars are absent; PHP code that depends
    // on them gets a connection error, which is the right surface.
    //
    // Unix-only: the daemon doesn't run on Windows in Phase 1.
    #[cfg(unix)]
    if let Ok(paths) = Paths::from_env()
        && paths.bougied_sock().exists() {
            for (k, v) in super::env::fetch_service_env(&paths, &project_root) {
                cmd.env(k, v);
            }
        }

    #[cfg(unix)]
    {
        // execve replaces this process; the only return is an error.
        let err = cmd.exec();
        Err(BougieError::Filesystem {
            operation: format!("execve {program}"),
            detail: err.to_string(),
        }
        .into())
    }
    #[cfg(not(unix))]
    {
        // Windows has no execve; spawn the child, wait, and propagate
        // its exit code so callers (and shells) see the same outcome.
        let status = cmd.status().map_err(|e| BougieError::Filesystem {
            operation: format!("spawning {program}"),
            detail: e.to_string(),
        })?;
        let code = status.code().unwrap_or(1);
        let code = u8::try_from(code).unwrap_or(1);
        Ok(ExitCode::from(code))
    }
}

/// The implicit-sync step of `bougie run`. An explicit `--php` request
/// forces a re-sync to the requested interpreter, overriding whatever
/// the project would otherwise infer (`--no-sync` with `--php` is
/// rejected at the CLI layer); otherwise sync only when the resolved
/// environment isn't already present — uv-parity: `bougie run` outside
/// a project (no `require.php`, no `[php]version`) falls back to the
/// highest already-installed PHP, or the latest publishable >=8.0,
/// instead of erroring the way `bougie sync` does.
///
/// Returns the system PHP selected for this invocation only (default
/// preference, nothing pinned into project state), if any — the caller
/// hands it to the shims via the `BOUGIE_RUN_SYSTEM_PHP` env var
/// instead of relying on the `resolved*` markers.
fn implicit_sync(
    project_root: &Path,
    format: OutputFormat,
    no_sync: bool,
    php_pref: bougie_cli::PhpPrefArgs,
    php_request: Option<&str>,
) -> Result<Option<std::path::PathBuf>> {
    if let Some(req) = php_request {
        let request = bougie_version::request::parse_request(req)
            .wrap_err_with(|| format!("parsing --php {req:?}"))?;
        return Ok(
            sync::run_with_php_request(project_root, format, false, php_pref, &request)?
                .ephemeral_system_php,
        );
    }
    if !no_sync && !is_environment_present(project_root)? {
        return Ok(
            sync::run_with_default_fallback(project_root, format, false, php_pref)?
                .ephemeral_system_php,
        );
    }
    Ok(None)
}

/// uv-parity auto-detection for `bougie run <path>` (no `--script`): when
/// `argv[0]` is a path-shaped file carrying an inline `# /// script`
/// block, run it as a self-contained script. Returns `Some(result)` when
/// the script path was taken, `None` to fall through to the normal
/// project run. A bare command or composer-script name like `bougie run
/// test` never matches — only a path-shaped argv[0] for a readable file
/// with a metadata block.
fn try_inline_script(
    argv: &[String],
    format: OutputFormat,
    php_request: Option<&str>,
    with: &[String],
    xdebug_flag: bool,
    php_pref: bougie_cli::PhpPrefArgs,
) -> Option<Result<ExitCode>> {
    let first = argv.first()?;
    if !looks_like_script_path(first) {
        return None;
    }
    let source = std::fs::read_to_string(first).ok()?;
    bougie_composer::inline::parse_inline_metadata(&source)
        .ok()
        .flatten()?;
    Some(super::script::run(argv, format, php_request, with, xdebug_flag, php_pref))
}

/// Heuristic for "argv[0] is a script file, not a command or composer
/// script name": it contains a path separator or ends with `.php`. Keeps
/// `bougie run test` (a composer-script name) on the project path while
/// `bougie run ./app.php` / `bougie run app.php` auto-detect as scripts.
fn looks_like_script_path(arg: &str) -> bool {
    arg.contains('/')
        || arg.contains('\\')
        || std::path::Path::new(arg)
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("php"))
}

fn install_xdebug_into_overlay(project_root: &Path) -> Result<()> {
    let paths = Paths::from_env()?;
    let (php_minor, flavor) =
        crate::commands::ext_add_remove::resolved_php_for_ext_install(project_root)?;
    let installed = bougie_installer::install::install_extension(
        &paths,
        "xdebug",
        None,
        php_minor,
        flavor,
        bougie_resolver::ResolveOptions::default(),
    )?;
    if !installed.already_present {
        eprintln!("bougie: downloaded xdebug for --xdebug run");
    }
    conf_d::write_debug_overlay_fragment(
        project_root,
        &installed.name,
        &installed.so_path,
        installed.load,
    )?;
    Ok(())
}

/// True iff the project's resolved markers point at on-disk artifacts
/// that still exist. Used to decide whether the implicit-sync step is
/// needed — a missing marker or missing install dir warrants resyncing.
fn is_environment_present(project_root: &Path) -> Result<bool> {
    let paths = Paths::from_env()?;

    let Ok((version, flavor)) = read_project_resolved(project_root) else {
        return Ok(false);
    };
    let install = paths.installs().join(format!("{version}-{flavor}"));
    #[cfg(unix)]
    let php_bin = install.join("bin").join("php");
    #[cfg(not(unix))]
    let php_bin = install.join("bin").join("php.exe");
    if !php_bin.exists() {
        return Ok(false);
    }
    Ok(true)
}

/// Did the user type `--` after the `run` subcommand?
///
/// clap consumes the literal `--` token from the parsed `argv`, but
/// it's still visible in `std::env::args_os()`. The presence of `--`
/// is the user's "don't look at composer scripts, just exec this"
/// signal — same convention as `npm run -- <cmd>` and a clean way to
/// disambiguate when a script name shadows a binary.
#[cfg(unix)]
fn explicit_passthrough() -> bool {
    let mut after_run = false;
    for a in std::env::args_os() {
        if after_run && a == "--" {
            return true;
        }
        if a == "run" {
            after_run = true;
        }
    }
    false
}

/// Read `composer.json` from the project root and return the named
/// script's steps, if defined. Missing or malformed composer.json
/// resolves to `Ok(None)` — script lookup is best-effort, the exec
/// fall-through always works.
#[cfg(unix)]
fn lookup_composer_script(project_root: &Path, name: &str) -> Result<Option<Vec<String>>> {
    let path = project_root.join("composer.json");
    if !path.exists() {
        return Ok(None);
    }
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(_) => return Ok(None),
    };
    let Ok(parsed) = read_composer_json(&text) else {
        return Ok(None);
    };
    Ok(parsed.scripts.get(name).cloned())
}

/// Execute a composer script. Each step runs as one
/// `/bin/sh -e -c <body>` with the script name as `$0` and any
/// trailing `bougie run <name> <extras…>` args available as
/// `$1`/`$2`/`$@`. Stops on the first non-zero step.
///
/// `ephemeral_php` is the one-off system PHP selected by the implicit
/// sync, if any — handed to the steps via `BOUGIE_RUN_SYSTEM_PHP` (in
/// place of `PHP_INI_SCAN_DIR`) so a step invoking `php` hits the same
/// interpreter as the direct-exec path.
#[cfg(unix)]
fn run_composer_script(
    project_root: &Path,
    name: &str,
    steps: &[String],
    extras: &[String],
    xdebug_flag: bool,
    ephemeral_php: Option<&Path>,
) -> Result<ExitCode> {
    let bougie_bin = bougie_paths::project::bin_dir(project_root);
    let prev_path = std::env::var("PATH").unwrap_or_default();
    let mut new_path = String::new();
    if bougie_bin.exists() {
        new_path.push_str(&bougie_bin.display().to_string());
    }
    // Same node overlay as the direct-exec path, so composer scripts
    // that shell out to `npm`/`node` find the project's toolchain.
    if let Ok(paths) = Paths::from_env()
        && let Some(node_bin) = super::node::project_bin_dir(project_root, &paths)
    {
        if !new_path.is_empty() {
            new_path.push(':');
        }
        new_path.push_str(&node_bin.display().to_string());
    }
    if !prev_path.is_empty() {
        if !new_path.is_empty() {
            new_path.push(':');
        }
        new_path.push_str(&prev_path);
    }

    let env_session_set = std::env::var_os("XDEBUG_SESSION")
        .is_some_and(|v| !v.is_empty());
    let debug_overlay = xdebug_flag || env_session_set;
    let paths = Paths::from_env()?;
    let scan_dir = conf_d::php_ini_scan_dir(&paths, project_root, debug_overlay);

    let service_env: Vec<(String, String)> = Paths::from_env()
        .ok()
        .filter(|paths| paths.bougied_sock().exists())
        .map(|paths| super::env::fetch_service_env(&paths, project_root))
        .unwrap_or_default();

    for step in steps {
        let mut cmd = std::process::Command::new("/bin/sh");
        cmd.arg("-e")
            .arg("-c")
            .arg(step)
            // `$0` = script name (shows up in shell error messages,
            // matching how composer presents script failures).
            .arg(name);
        for e in extras {
            cmd.arg(e);
        }
        cmd.current_dir(project_root)
            .env("PATH", &new_path)
            .env("BOUGIE_PROJECT_ROOT", project_root);
        match ephemeral_php {
            // Same handoff as the direct-exec path — see `run`.
            Some(php) => {
                cmd.env("BOUGIE_RUN_SYSTEM_PHP", php);
            }
            None => {
                cmd.env("PHP_INI_SCAN_DIR", &scan_dir);
            }
        }
        if xdebug_flag && !env_session_set {
            cmd.env("XDEBUG_SESSION", "1");
        }
        for (k, v) in &service_env {
            cmd.env(k, v);
        }
        let status = cmd
            .status()
            .wrap_err_with(|| format!("spawning /bin/sh for composer script `{name}`"))?;
        if !status.success() {
            let code = status.code().unwrap_or(1);
            return Ok(ExitCode::from(u8::try_from(code).unwrap_or(1)));
        }
    }
    Ok(ExitCode::SUCCESS)
}

/// Walk up from `cwd` looking for the project root, marked by any of
/// `bougie.toml`, `composer.json`, or `vendor/bougie/`. Falls back to `cwd`
/// itself if no marker is found in any ancestor — that keeps uv-parity
/// for `bougie run python` invoked outside any project, where
/// [`sync::run_with_default_fallback`] still does the right thing.
///
/// Shared by the forgiving entry points `bougie run` (here) and
/// `bougie sync` (dispatched from `lib.rs`): both thread the resolved
/// root into `sync::*` so the toolchain materializes at the real root,
/// not wherever the cwd happens to be (e.g. a Hyvä theme's
/// `web/tailwind/`). Mirrors `service::config_mut::locate_project_root`
/// but with a fallback instead of an error; `bougie run`/`bougie sync`
/// must remain usable outside a project, while `service::*` requires a
/// real project.
pub(crate) fn resolve_project_root(cwd: &Path) -> std::path::PathBuf {
    for anc in cwd.ancestors() {
        if anc.join("bougie.toml").is_file()
            || anc.join("composer.json").is_file()
            || bougie_paths::project::is_root(anc)
        {
            return anc.to_path_buf();
        }
    }
    cwd.to_path_buf()
}

#[cfg(test)]
mod tests {
    use super::resolve_project_root;
    use std::fs;

    #[test]
    fn walks_up_to_composer_json() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        fs::write(root.join("composer.json"), "{}").unwrap();
        let sub = root.join("dev").join("tests").join("integration");
        fs::create_dir_all(&sub).unwrap();
        assert_eq!(resolve_project_root(&sub), root);
    }

    #[test]
    fn walks_up_to_bougie_toml() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        fs::write(root.join("bougie.toml"), "").unwrap();
        let sub = root.join("a").join("b");
        fs::create_dir_all(&sub).unwrap();
        assert_eq!(resolve_project_root(&sub), root);
    }

    #[test]
    fn walks_up_to_dot_bougie() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        fs::create_dir_all(root.join("vendor").join("bougie")).unwrap();
        let sub = root.join("x");
        fs::create_dir_all(&sub).unwrap();
        assert_eq!(resolve_project_root(&sub), root);
    }

    #[test]
    fn falls_back_to_cwd_when_no_marker() {
        let tmp = tempfile::tempdir().unwrap();
        let sub = tmp.path().join("nothing").join("here");
        fs::create_dir_all(&sub).unwrap();
        // No marker anywhere in the temp tree, so fallback returns cwd
        // verbatim — preserves uv-parity for `bougie run python` outside
        // any project.
        assert_eq!(resolve_project_root(&sub), sub);
    }

    #[test]
    fn walks_up_past_hyva_tailwind_subdir() {
        // Regression: running `bougie run`/`bougie sync` from a Hyvä
        // theme's `web/tailwind/` folder (has package.json, no
        // composer.json) must resolve to the Magento project root, so the
        // toolchain materializes there — not as a stray `vendor/bougie/`
        // inside the theme. The resolved root is threaded into `sync::*`,
        // which no longer falls back to the cwd.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        fs::write(root.join("composer.json"), "{}").unwrap();
        let tailwind = root.join("app/design/frontend/Acme/theme/web/tailwind");
        fs::create_dir_all(&tailwind).unwrap();
        fs::write(tailwind.join("package.json"), "{}").unwrap();
        assert_eq!(resolve_project_root(&tailwind), root);
    }

    #[test]
    fn prefers_closest_ancestor() {
        let tmp = tempfile::tempdir().unwrap();
        let outer = tmp.path();
        let inner = outer.join("inner");
        fs::create_dir_all(&inner).unwrap();
        fs::write(outer.join("composer.json"), "{}").unwrap();
        fs::write(inner.join("composer.json"), "{}").unwrap();
        let sub = inner.join("deep");
        fs::create_dir_all(&sub).unwrap();
        assert_eq!(resolve_project_root(&sub), inner);
    }
}
