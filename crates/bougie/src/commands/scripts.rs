//! Bridge between the install lifecycle and `bougie-scripts`: builds a
//! [`ScriptContext`] from the project's resolved PHP + environment and
//! implements the resolver's [`ScriptHooks`] so opted-in root
//! `composer.json` scripts run at the right lifecycle points.
//!
//! The opt-in decision is [`enabled`]; everything else here only runs when
//! scripts are on. PHP-callback entries are warn-skipped by `bougie-scripts`
//! except for the small native allowlist registered in [`callback_registry`]
//! (Laravel's `clearCompiled`). The per-process timeout follows Composer's
//! `config.process-timeout` (default 300s); `disableProcessTimeout` is
//! handled inside `bougie-scripts`.

use std::path::{Path, PathBuf};
use std::time::Duration;

use bougie_composer_resolver::ScriptHooks;
use bougie_config::ProjectConfig;
use bougie_scripts::{dispatch, CallbackRegistry, ScriptContext, Scripts};
use eyre::Result;

/// Composer's default `config.process-timeout` in seconds.
const DEFAULT_PROCESS_TIMEOUT_SECS: u64 = 300;

/// Whether root-script execution is on: an explicit `--scripts` /
/// `--no-scripts` CLI flag wins; otherwise defer to `[scripts] run` in
/// bougie.toml / `extra.bougie` (off by default).
#[must_use]
pub fn enabled(cli_flag: Option<bool>, project: &ProjectConfig) -> bool {
    cli_flag.unwrap_or_else(|| project.bougie.scripts.enabled())
}

/// Which command's lifecycle is running — selects `install` vs `update`
/// event names.
#[derive(Debug, Clone, Copy)]
pub enum Lifecycle {
    Install,
    Update,
}

/// Owns the parsed scripts + the inputs a [`ScriptContext`] borrows, and
/// implements [`ScriptHooks`] for the resolver. Construct with [`new`] and
/// pass `Some(&hooks)` into `install_from_lock`.
///
/// [`new`]: LifecycleHooks::new
#[derive(Debug)]
pub struct LifecycleHooks<'a> {
    scripts: Scripts,
    callbacks: CallbackRegistry,
    project_root: &'a Path,
    php_bin: PathBuf,
    bin_dir: PathBuf,
    base_env: Vec<(String, String)>,
    dev_mode: bool,
    timeout: Option<Duration>,
    kind: Lifecycle,
}

impl<'a> LifecycleHooks<'a> {
    /// Build the hooks for a project. Reads `composer.json` for the
    /// `scripts` table and resolves the project PHP binary (falling back to
    /// the `.bougie/bin/php` shim, which is on `PATH` post-sync).
    pub fn new(project_root: &'a Path, dev_mode: bool, kind: Lifecycle) -> Result<Self> {
        let composer_json = project_root.join("composer.json");
        let value: serde_json::Value = std::fs::read(&composer_json)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or(serde_json::Value::Null);
        let scripts = Scripts::parse(&value);

        let php_bin = super::env::resolve_php_bin(project_root)
            .unwrap_or_else(|| project_root.join(".bougie").join("bin").join("php"));
        let bin_dir = project_root.join("vendor").join("bin");
        let base_env = super::env::project_script_env(project_root, dev_mode);
        let timeout = process_timeout(&value);

        Ok(Self {
            scripts,
            callbacks: callback_registry(),
            project_root,
            php_bin,
            bin_dir,
            base_env,
            dev_mode,
            timeout,
            kind,
        })
    }

    fn context(&self) -> ScriptContext<'_> {
        ScriptContext {
            project_root: self.project_root,
            php_bin: &self.php_bin,
            bin_dir: &self.bin_dir,
            base_env: self.base_env.clone(),
            dev_mode: self.dev_mode,
            timeout: self.timeout,
            callbacks: &self.callbacks,
        }
    }

    fn fire(&self, event: &str) -> Result<()> {
        dispatch(&self.scripts, event, &self.context()).map(|_| ())
    }
}

impl ScriptHooks for LifecycleHooks<'_> {
    fn pre_cmd(&self) -> Result<()> {
        self.fire(match self.kind {
            Lifecycle::Install => "pre-install-cmd",
            Lifecycle::Update => "pre-update-cmd",
        })
    }
    fn pre_autoload_dump(&self) -> Result<()> {
        self.fire("pre-autoload-dump")
    }
    fn post_autoload_dump(&self) -> Result<()> {
        self.fire("post-autoload-dump")
    }
    fn post_cmd(&self) -> Result<()> {
        self.fire(match self.kind {
            Lifecycle::Install => "post-install-cmd",
            Lifecycle::Update => "post-update-cmd",
        })
    }
}

/// The native callback allowlist: PHP callbacks bougie reproduces instead of
/// running. Everything else warn-skips.
fn callback_registry() -> CallbackRegistry {
    let mut reg = CallbackRegistry::new();
    // Laravel's `ComposerScripts::postAutoloadDump` → clearCompiled. The
    // discovery half (`packages.php`) is rebuilt by the real
    // `@php artisan package:discover` entry running alongside it.
    reg.register(
        "Illuminate\\Foundation\\ComposerScripts::postAutoloadDump",
        Box::new(|ctx: &ScriptContext| {
            bougie_installers::clear_compiled(ctx.project_root)
                .map_err(|e| eyre::eyre!("clearCompiled: {e}"))
        }),
    );
    // `Composer\Config::disableProcessTimeout` is not registered here: it
    // mutates the per-dispatch timeout, so `bougie-scripts` recognises it
    // directly rather than via this registry.
    reg
}

/// Composer's per-process timeout for the install lifecycle:
/// `COMPOSER_PROCESS_TIMEOUT` env wins, else `config.process-timeout` in
/// composer.json, else 300s. `0` means unlimited (`None`).
fn process_timeout(composer: &serde_json::Value) -> Option<Duration> {
    let secs = std::env::var("COMPOSER_PROCESS_TIMEOUT")
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .or_else(|| {
            composer
                .get("config")
                .and_then(|c| c.get("process-timeout"))
                .and_then(|v| v.as_u64().or_else(|| v.as_str().and_then(|s| s.parse().ok())))
        })
        .unwrap_or(DEFAULT_PROCESS_TIMEOUT_SECS);
    (secs > 0).then(|| Duration::from_secs(secs))
}

#[cfg(test)]
mod tests {
    use super::*;
    use bougie_config::{BougieConfig, ScriptsConfig};

    fn project(run: Option<bool>) -> ProjectConfig {
        ProjectConfig {
            composer: None,
            bougie: BougieConfig { scripts: ScriptsConfig { run }, ..Default::default() },
        }
    }

    #[test]
    fn enabled_precedence_cli_over_config() {
        // CLI flag wins over config in both directions.
        assert!(enabled(Some(true), &project(Some(false))));
        assert!(!enabled(Some(false), &project(Some(true))));
        // No CLI flag → defer to config.
        assert!(enabled(None, &project(Some(true))));
        assert!(!enabled(None, &project(Some(false))));
        // Unset everywhere → off by default.
        assert!(!enabled(None, &project(None)));
    }

    #[test]
    fn registry_has_native_laravel_callback() {
        let reg = callback_registry();
        assert!(reg.get("Illuminate\\Foundation\\ComposerScripts", "postAutoloadDump").is_some());
        // disableProcessTimeout is handled inside bougie-scripts, not here.
        assert!(reg.get("Composer\\Config", "disableProcessTimeout").is_none());
        assert!(reg.get("Acme\\Thing", "run").is_none());
    }

    #[test]
    fn process_timeout_default_and_config_override() {
        // Default 300s when unset.
        assert_eq!(process_timeout(&serde_json::Value::Null), Some(Duration::from_secs(300)));
        // config.process-timeout (numeric) overrides; 0 = unlimited.
        assert_eq!(
            process_timeout(&serde_json::json!({"config": {"process-timeout": 600}})),
            Some(Duration::from_secs(600))
        );
        assert_eq!(
            process_timeout(&serde_json::json!({"config": {"process-timeout": 0}})),
            None
        );
    }
}
