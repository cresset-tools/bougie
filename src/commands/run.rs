use crate::cli::OutputFormat;
use crate::commands::sync;
use crate::conf_d;
use crate::config::read_composer_json;
use crate::errors::BougieError;
use crate::paths::Paths;
use crate::state::{read_project_resolved, read_project_resolved_composer};
use eyre::{eyre, Result, WrapErr};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::ExitCode;

/// `bougie run [--] <cmd> [args...]` — set `PATH` and `PHP_INI_SCAN_DIR`,
/// then exec the requested command. Per CLI.md §3.4, implicitly runs
/// `bougie sync` first unless `--no-sync` is passed.
///
/// Debug overlay: when `xdebug_flag` is set or the parent's
/// `XDEBUG_SESSION` env var is non-empty, `PHP_INI_SCAN_DIR` is
/// widened to include `.bougie/conf.d-debug/` so server-installed
/// debug fragments load for this invocation too. With the explicit
/// `--xdebug` flag, bougie also exports `XDEBUG_SESSION=1` to the
/// child (if not already set) and lazily installs xdebug into
/// `conf.d-debug/` when no xdebug fragment exists in either dir.
pub fn run(
    _with: &[String],
    argv: &[String],
    format: OutputFormat,
    no_sync: bool,
    xdebug_flag: bool,
) -> Result<ExitCode> {
    if argv.is_empty() {
        return Err(eyre!("nothing to run"));
    }
    let project_root = std::env::current_dir()?;
    if !no_sync && !is_environment_present(&project_root)? {
        sync::run(format, false)?;
    }

    // composer.json `scripts.<name>` lookup. Skipped when the user
    // explicitly typed `--` after `run` (their signal to bypass the
    // script table) or when `argv[0]` looks like a path. Found
    // scripts execute as `/bin/sh -e -c` with the same PATH /
    // PHP_INI_SCAN_DIR / BOUGIE_SERVICE_* env the exec path injects;
    // extra positional args (`bougie run test foo bar`) flow into
    // `$@` so scripts can act on them.
    if !explicit_passthrough() && !argv[0].contains('/') {
        if let Some(steps) = lookup_composer_script(&project_root, &argv[0])? {
            return run_composer_script(
                &project_root,
                &argv[0],
                &steps,
                &argv[1..],
                xdebug_flag,
            );
        }
    }

    let bougie_bin = project_root.join(".bougie").join("bin");

    let env_session_set = std::env::var_os("XDEBUG_SESSION")
        .is_some_and(|v| !v.is_empty());
    let debug_overlay = xdebug_flag || env_session_set;

    // Lazy-install xdebug into the debug overlay so `--xdebug` works
    // on a fresh project without the user having to `bougie ext add
    // xdebug` first. Only triggered by the explicit flag (the env-var
    // path is "the request was *already* set up for xdebug
    // elsewhere"; demanding bougie install something then would be a
    // surprise).
    if xdebug_flag && !conf_d::fragment_present_anywhere(&project_root, "xdebug") {
        install_xdebug_into_overlay(&project_root)
            .wrap_err("installing xdebug for `bougie run --xdebug`")?;
    }

    let scan_dir = conf_d::php_ini_scan_dir(&project_root, debug_overlay);

    let prev_path = std::env::var("PATH").unwrap_or_default();
    let new_path = if bougie_bin.exists() {
        format!("{}:{prev_path}", bougie_bin.display())
    } else {
        prev_path
    };

    let (program, rest) = argv.split_first().ok_or_else(|| eyre!("argv missing"))?;
    let mut cmd = std::process::Command::new(program);
    cmd.args(rest)
        .env("PATH", new_path)
        .env("PHP_INI_SCAN_DIR", &scan_dir)
        .env("BOUGIE_PROJECT_ROOT", &project_root);
    if xdebug_flag && !env_session_set {
        cmd.env("XDEBUG_SESSION", "1");
    }
    // Layer in any per-tenant `BOUGIE_SERVICE_*` env vars the daemon
    // knows about. Only when `bougied` is already running — `bougie
    // run` deliberately does NOT auto-spawn the daemon (the user
    // explicitly chose `bougie services up` for that). When the
    // daemon isn't there the vars are absent; PHP code that depends
    // on them gets a connection error, which is the right surface.
    if let Ok(paths) = Paths::from_env() {
        if paths.bougied_sock().exists() {
            for (k, v) in fetch_service_env(&paths, &project_root) {
                cmd.env(k, v);
            }
        }
    }
    let err = cmd.exec();
    Err(BougieError::Filesystem {
        operation: format!("execve {program}"),
        detail: err.to_string(),
    }
    .into())
}

fn install_xdebug_into_overlay(project_root: &Path) -> Result<()> {
    let paths = Paths::from_env()?;
    let (php_minor, flavor) =
        crate::commands::ext_add_remove::resolved_php_for_ext_install(project_root)?;
    let installed = crate::install::install_extension(
        &paths,
        "xdebug",
        None,
        php_minor,
        flavor,
        crate::resolve::ResolveOptions::default(),
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

/// Best-effort IPC call to `bougied` for the project's
/// `BOUGIE_SERVICE_*` env vars. Returns an empty Vec if anything goes
/// wrong — `bougie run` must never fail because the daemon was down,
/// slow, or speaking an old protocol. A connection error here is a
/// signal to the user (PHP gets no DSN); not a CLI-level error.
fn fetch_service_env(
    paths: &Paths,
    project: &Path,
) -> Vec<(String, String)> {
    use serde::Deserialize;
    #[derive(Deserialize)]
    struct EnvReply {
        #[serde(default)]
        vars: std::collections::BTreeMap<String, serde_json::Value>,
    }
    let args = serde_json::json!({"project": project});
    let reply: Result<EnvReply> =
        crate::commands::services::client::call(paths, "service.env", args);
    match reply {
        Ok(r) => r
            .vars
            .into_iter()
            .filter_map(|(k, v)| {
                let s = match v {
                    serde_json::Value::String(s) => s,
                    other => other.to_string(),
                };
                Some((k, s))
            })
            .collect(),
        Err(_) => Vec::new(),
    }
}

/// True iff the project's resolved markers point at on-disk artifacts
/// that still exist. Used to decide whether the implicit-sync step is
/// needed — a missing marker, missing install dir, or missing composer
/// phar all warrant resyncing.
fn is_environment_present(project_root: &Path) -> Result<bool> {
    let paths = Paths::from_env()?;

    let Ok((version, flavor)) = read_project_resolved(project_root) else {
        return Ok(false);
    };
    let install = paths.installs().join(format!("{version}-{flavor}"));
    if !install.join("bin").join("php").exists() {
        return Ok(false);
    }

    let Ok(composer_version) = read_project_resolved_composer(project_root) else {
        return Ok(false);
    };
    if !paths.composer_phar(&composer_version).exists() {
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
fn run_composer_script(
    project_root: &Path,
    name: &str,
    steps: &[String],
    extras: &[String],
    xdebug_flag: bool,
) -> Result<ExitCode> {
    let bougie_bin = project_root.join(".bougie").join("bin");
    let prev_path = std::env::var("PATH").unwrap_or_default();
    let new_path = if bougie_bin.exists() {
        format!("{}:{prev_path}", bougie_bin.display())
    } else {
        prev_path
    };

    let env_session_set = std::env::var_os("XDEBUG_SESSION")
        .is_some_and(|v| !v.is_empty());
    let debug_overlay = xdebug_flag || env_session_set;
    let scan_dir = conf_d::php_ini_scan_dir(project_root, debug_overlay);

    let service_env: Vec<(String, String)> = Paths::from_env()
        .ok()
        .filter(|paths| paths.bougied_sock().exists())
        .map(|paths| fetch_service_env(&paths, project_root))
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
            .env("PHP_INI_SCAN_DIR", &scan_dir)
            .env("BOUGIE_PROJECT_ROOT", project_root);
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
