//! Host-injected execution context and the native callback registry.

use std::collections::HashMap;
use std::path::Path;
use std::time::Duration;

/// Everything [`dispatch`](crate::dispatch) needs from the host, injected by
/// the caller so the crate stays FS/PHP-agnostic and testable.
pub struct ScriptContext<'a> {
    /// Project root; scripts run with this as their working directory.
    pub project_root: &'a Path,
    /// The project's resolved PHP binary, used for `@php` entries.
    pub php_bin: &'a Path,
    /// `vendor/bin` (or `config.bin-dir`). Prepended onto `PATH` for the
    /// dispatch so scripts find installed CLIs (`phpunit`, `pint`, …). The
    /// host may already have folded this into `base_env`'s `PATH`; the
    /// prepend is idempotent (skipped if `PATH` already leads with it).
    pub bin_dir: &'a Path,
    /// Base environment overrides layered on top of the inherited process
    /// env: `PATH`, `COMPOSER_DEV_MODE`, `COMPOSER_BINARY`, `BOUGIE_*`, and
    /// any per-tenant `BOUGIE_SERVICE_*` vars.
    pub base_env: Vec<(String, String)>,
    /// Whether dev dependencies are in scope (`COMPOSER_DEV_MODE`).
    pub dev_mode: bool,
    /// Per-process wall-clock timeout (Composer's `config.process-timeout`,
    /// default 300s). Each spawned entry gets its own budget; on expiry the
    /// child is killed and the event aborts. The
    /// `Composer\Config::disableProcessTimeout` script callback flips it off
    /// for the rest of the dispatch. `None` = unlimited.
    pub timeout: Option<Duration>,
    /// Native handlers for the callbacks bougie reproduces (keyed by
    /// `"Class::method"`). A hit runs the handler instead of warn-skipping.
    pub callbacks: &'a CallbackRegistry,
}

impl std::fmt::Debug for ScriptContext<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ScriptContext")
            .field("project_root", &self.project_root)
            .field("php_bin", &self.php_bin)
            .field("bin_dir", &self.bin_dir)
            .field("dev_mode", &self.dev_mode)
            .field("timeout", &self.timeout)
            .field("callbacks", &self.callbacks)
            .finish_non_exhaustive()
    }
}

/// A native handler standing in for a PHP-callback entry. Returns `Err` to
/// abort the event (same as a non-zero process exit).
pub type CallbackHandler = Box<dyn Fn(&ScriptContext) -> eyre::Result<()> + Send + Sync>;

/// A curated allowlist of PHP callbacks bougie reproduces natively, mapping
/// `"Class::method"` → handler. This is **not** a general callback runner:
/// only the host-registered entries run; every other callback warn-skips.
#[derive(Default)]
pub struct CallbackRegistry(HashMap<String, CallbackHandler>);

impl CallbackRegistry {
    #[must_use]
    pub fn new() -> Self {
        Self(HashMap::new())
    }

    /// Register a handler under a `"Class::method"` key. The key is
    /// normalised (a single leading `\` on the class is stripped) to match
    /// how Composer entries may or may not carry the root-namespace slash.
    pub fn register(&mut self, key: &str, handler: CallbackHandler) {
        self.0.insert(normalize_key(key), handler);
    }

    /// Look up a handler for a classified `Class::method` callback.
    #[must_use]
    pub fn get(&self, class: &str, method: &str) -> Option<&CallbackHandler> {
        self.0.get(&normalize_key(&format!("{class}::{method}")))
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl std::fmt::Debug for CallbackRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CallbackRegistry").field("keys", &self.0.keys()).finish()
    }
}

/// Normalise a `Class::method` key: strip one leading namespace `\` so
/// `\Foo\Bar::baz` and `Foo\Bar::baz` collide.
fn normalize_key(key: &str) -> String {
    key.strip_prefix('\\').unwrap_or(key).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_lookup_is_leading_slash_insensitive() {
        let mut reg = CallbackRegistry::new();
        reg.register("\\Foo\\Bar::baz", Box::new(|_| Ok(())));
        assert!(reg.get("Foo\\Bar", "baz").is_some());
        assert!(reg.get("\\Foo\\Bar", "baz").is_some());
        assert!(reg.get("Foo\\Bar", "other").is_none());
    }
}
