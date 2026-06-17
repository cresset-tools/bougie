//! Shared project-execution environment: the `PATH` / `PHP_INI_SCAN_DIR` /
//! `BOUGIE_*` / service-env block used both by `bougie run` (composer
//! scripts) and the opt-in root-script bridge in the install lifecycle.
//!
//! One source of truth so a script run by `bougie sync --scripts` sees the
//! same environment as the same script run via `bougie run <name>`.

use std::path::{Path, PathBuf};

use bougie_fs::state::read_project_resolved;
use bougie_installer::conf_d;
use bougie_paths::Paths;

#[cfg(unix)]
const PATH_SEP: &str = ":";
#[cfg(not(unix))]
const PATH_SEP: &str = ";";

/// Resolve the project's pinned PHP binary from its `state/resolved`
/// marker. `None` when the project hasn't been synced (no marker) or the
/// install is missing — the caller decides whether that's fatal.
#[must_use]
pub fn resolve_php_bin(project_root: &Path) -> Option<PathBuf> {
    let paths = Paths::from_env().ok()?;
    let (version, flavor) = read_project_resolved(project_root).ok()?;
    let install = paths.installs().join(format!("{version}-{flavor}"));
    #[cfg(unix)]
    let php_bin = install.join("bin").join("php");
    #[cfg(not(unix))]
    let php_bin = install.join("bin").join("php.exe");
    php_bin.exists().then_some(php_bin)
}

/// Build the base environment for running project scripts: the bougie shim
/// dir + `vendor/bin` + per-extension PATH extras front-loaded onto `PATH`,
/// `PHP_INI_SCAN_DIR`, `BOUGIE_PROJECT_ROOT`, Composer's `COMPOSER_DEV_MODE`
/// / `COMPOSER_BINARY`, and any per-tenant `BOUGIE_SERVICE_*` vars the
/// daemon knows about.
///
/// Returned as override pairs layered on top of the inherited process env
/// by the caller (`Command::envs`), matching how `bougie run` injects them.
#[must_use]
pub fn project_script_env(project_root: &Path, dev_mode: bool) -> Vec<(String, String)> {
    let mut env: Vec<(String, String)> = Vec::new();

    // PATH: vendor/bougie/bin (php/composer/unzip shims) + vendor/bin
    // (composer bin proxies) + per-extension extras + the inherited PATH.
    let bougie_bin = bougie_paths::project::bin_dir(project_root);
    let vendor_bin = project_root.join("vendor").join("bin");
    let mut path = String::new();
    let mut push = |p: &Path| {
        if !p.is_dir() {
            return;
        }
        if !path.is_empty() {
            path.push_str(PATH_SEP);
        }
        path.push_str(&p.display().to_string());
    };
    push(&bougie_bin);
    push(&vendor_bin);
    for extra in conf_d::read_path_extras(project_root) {
        push(&extra);
    }
    if let Ok(prev) = std::env::var("PATH")
        && !prev.is_empty()
    {
        if !path.is_empty() {
            path.push_str(PATH_SEP);
        }
        path.push_str(&prev);
    }
    env.push(("PATH".into(), path));

    // `conf.d-local/` lives under `$BOUGIE_HOME`; if that's somehow
    // unresolvable, fall back to the project-local `conf.d/` only.
    let scan_dir = match Paths::from_env() {
        Ok(paths) => conf_d::php_ini_scan_dir(&paths, project_root, false),
        Err(_) => conf_d::project_confd_dir(project_root).into_os_string(),
    };
    env.push(("PHP_INI_SCAN_DIR".into(), scan_dir.to_string_lossy().into_owned()));
    env.push(("BOUGIE_PROJECT_ROOT".into(), project_root.display().to_string()));
    env.push(("COMPOSER_DEV_MODE".into(), if dev_mode { "1" } else { "0" }.into()));
    if let Ok(exe) = std::env::current_exe() {
        env.push(("COMPOSER_BINARY".into(), exe.display().to_string()));
    }
    for (k, v) in service_env(project_root) {
        env.push((k, v));
    }
    env
}

/// Per-tenant `BOUGIE_SERVICE_*` env vars from the daemon, if it's up.
/// Empty when the daemon isn't running, on Windows, or on any IPC error —
/// a script must never fail because the daemon was down.
#[must_use]
pub fn service_env(project_root: &Path) -> Vec<(String, String)> {
    #[cfg(unix)]
    {
        if let Ok(paths) = Paths::from_env()
            && paths.bougied_sock().exists()
        {
            return fetch_service_env(&paths, project_root);
        }
        Vec::new()
    }
    #[cfg(not(unix))]
    {
        let _ = project_root;
        Vec::new()
    }
}

/// Best-effort IPC call to `bougied` for the project's `BOUGIE_SERVICE_*`
/// env vars. Returns an empty Vec on any error — callers must never fail
/// because the daemon was down, slow, or speaking an old protocol.
///
/// Unix-only — the daemon isn't built on Windows in Phase 1.
#[cfg(unix)]
pub fn fetch_service_env(paths: &Paths, project: &Path) -> Vec<(String, String)> {
    use serde::Deserialize;
    #[derive(Deserialize)]
    struct EnvReply {
        #[serde(default)]
        vars: std::collections::BTreeMap<String, serde_json::Value>,
    }
    let args = serde_json::json!({ "project": project });
    let reply: eyre::Result<EnvReply> =
        crate::commands::services::client::call(paths, "service.env", args);
    match reply {
        Ok(r) => r
            .vars
            .into_iter()
            .map(|(k, v)| {
                let s = match v {
                    serde_json::Value::String(s) => s,
                    other => other.to_string(),
                };
                (k, s)
            })
            .collect(),
        Err(_) => Vec::new(),
    }
}
