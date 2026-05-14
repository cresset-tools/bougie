//! `BOUGIE_HOME` / `BOUGIE_CACHE` resolution and subpath helpers.
//!
//! Bougie uses XDG base dirs on every platform (including macOS) — see
//! CLI.md §2.1. Override via `BOUGIE_HOME` / `BOUGIE_CACHE` env vars.

use etcetera::base_strategy::{BaseStrategy, Xdg};
use eyre::{Result, WrapErr};
use std::ffi::OsString;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct Paths {
    home: PathBuf,
    cache: PathBuf,
}

impl Paths {
    /// Resolve from environment + XDG defaults.
    pub fn from_env() -> Result<Self> {
        let xdg = Xdg::new().wrap_err("could not resolve XDG base dirs")?;
        Ok(Self::resolve(
            std::env::var_os("BOUGIE_HOME"),
            std::env::var_os("BOUGIE_CACHE"),
            &xdg.data_dir(),
            &xdg.cache_dir(),
        ))
    }

    /// Pure resolver, exposed for unit tests.
    pub fn resolve(
        env_home: Option<OsString>,
        env_cache: Option<OsString>,
        xdg_data: &Path,
        xdg_cache: &Path,
    ) -> Self {
        Self {
            home: env_home.map_or_else(|| xdg_data.join("bougie"), PathBuf::from),
            cache: env_cache.map_or_else(|| xdg_cache.join("bougie"), PathBuf::from),
        }
    }

    /// Construct directly from explicit paths.
    pub fn new(home: PathBuf, cache: PathBuf) -> Self {
        Self { home, cache }
    }

    pub fn home(&self) -> &Path {
        &self.home
    }
    pub fn cache(&self) -> &Path {
        &self.cache
    }

    pub fn installs(&self) -> PathBuf {
        self.home.join("installs")
    }
    pub fn store(&self) -> PathBuf {
        self.home.join("store")
    }
    pub fn state(&self) -> PathBuf {
        self.home.join("state")
    }
    pub fn locks(&self) -> PathBuf {
        self.state().join("locks")
    }
    pub fn global_lock(&self) -> PathBuf {
        self.locks().join("global.lock")
    }
    pub fn state_json(&self) -> PathBuf {
        self.state().join("state.json")
    }
    pub fn public_keys(&self) -> PathBuf {
        self.state().join("public-keys")
    }
    pub fn bin(&self) -> PathBuf {
        self.home.join("bin")
    }

    /// `$BOUGIE_HOME/composer/` — managed Composer installs, one
    /// directory per version.
    pub fn composer_root(&self) -> PathBuf {
        self.home.join("composer")
    }
    /// Path to the phar for a specific Composer version:
    /// `$BOUGIE_HOME/composer/<version>/composer.phar`.
    pub fn composer_phar(&self, version: &str) -> PathBuf {
        self.composer_root().join(version).join("composer.phar")
    }
    /// Cached snapshot of getcomposer.org's `/versions` JSON.
    pub fn composer_channels_json(&self) -> PathBuf {
        self.composer_root().join("channels.json")
    }
    /// Etag sidecar for the channels JSON.
    pub fn composer_channels_etag(&self) -> PathBuf {
        self.composer_root().join("channels.json.etag")
    }

    /// Per-origin index cache root: `$BOUGIE_CACHE/index/<host>/`.
    pub fn cache_index(&self, host: &str) -> PathBuf {
        self.cache.join("index").join(host)
    }
    /// In-flight blob downloads (`.partial` files only).
    pub fn cache_blobs(&self) -> PathBuf {
        self.cache.join("blobs")
    }

    // ---------- `bougied` daemon + service supervisor paths ----------
    //
    // See SERVICES.md and CLI.md §2.1 in the php-build-standalone repo
    // for the canonical layout. Socket and pid file live directly under
    // `state/`; everything else hangs under `state/services/<name>/`.

    /// Unix socket the `bougied` daemon listens on (mode 0600).
    pub fn bougied_sock(&self) -> PathBuf {
        self.state().join("bougied.sock")
    }
    /// Pid file for the running daemon (also flock'd for singleton).
    pub fn bougied_pid(&self) -> PathBuf {
        self.state().join("bougied.pid")
    }
    /// Root for all per-service state (`$BOUGIE_HOME/state/services/`).
    pub fn services_dir(&self) -> PathBuf {
        self.state().join("services")
    }
    /// Per-service root: `$BOUGIE_HOME/state/services/<name>/`.
    pub fn service_dir(&self, name: &str) -> PathBuf {
        self.services_dir().join(name)
    }
    /// Per-service durable data (mariadb datadir, redis dump, …).
    pub fn service_data(&self, name: &str) -> PathBuf {
        self.service_dir(name).join("data")
    }
    /// Per-service runtime dir (unix socket, pid file).
    pub fn service_run(&self, name: &str) -> PathBuf {
        self.service_dir(name).join("run")
    }
    /// Per-service log dir (rotated logs land here).
    pub fn service_log(&self, name: &str) -> PathBuf {
        self.service_dir(name).join("log")
    }
    /// Per-service rendered config (read-only to the service via sandbox).
    pub fn service_conf(&self, name: &str) -> PathBuf {
        self.service_dir(name).join("conf")
    }
    /// Per-service tenant ledger (JSON Lines, see SERVICES.md §3.3).
    pub fn service_tenants(&self, name: &str) -> PathBuf {
        self.service_dir(name).join("tenants.json")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_home_wins_over_xdg() {
        let p = Paths::resolve(
            Some(OsString::from("/tmp/bh")),
            None,
            Path::new("/data"),
            Path::new("/cache"),
        );
        assert_eq!(p.home(), Path::new("/tmp/bh"));
        assert_eq!(p.cache(), Path::new("/cache/bougie"));
    }

    #[test]
    fn env_cache_wins_over_xdg() {
        let p = Paths::resolve(
            None,
            Some(OsString::from("/tmp/bc")),
            Path::new("/data"),
            Path::new("/cache"),
        );
        assert_eq!(p.home(), Path::new("/data/bougie"));
        assert_eq!(p.cache(), Path::new("/tmp/bc"));
    }

    #[test]
    fn xdg_default_when_no_env() {
        let p = Paths::resolve(None, None, Path::new("/data"), Path::new("/cache"));
        assert_eq!(p.home(), Path::new("/data/bougie"));
        assert_eq!(p.cache(), Path::new("/cache/bougie"));
    }

    #[test]
    fn subpath_helpers_compose_correctly() {
        let p = Paths::new(PathBuf::from("/h"), PathBuf::from("/c"));
        assert_eq!(p.installs(), Path::new("/h/installs"));
        assert_eq!(p.store(), Path::new("/h/store"));
        assert_eq!(p.global_lock(), Path::new("/h/state/locks/global.lock"));
        assert_eq!(p.state_json(), Path::new("/h/state/state.json"));
        assert_eq!(p.cache_index("origin.example"), Path::new("/c/index/origin.example"));
        assert_eq!(p.cache_blobs(), Path::new("/c/blobs"));
        assert_eq!(p.composer_root(), Path::new("/h/composer"));
        assert_eq!(p.composer_phar("2.8.5"), Path::new("/h/composer/2.8.5/composer.phar"));
        assert_eq!(p.composer_channels_json(), Path::new("/h/composer/channels.json"));
    }

    #[test]
    fn bougied_and_service_paths_compose() {
        let p = Paths::new(PathBuf::from("/h"), PathBuf::from("/c"));
        assert_eq!(p.bougied_sock(), Path::new("/h/state/bougied.sock"));
        assert_eq!(p.bougied_pid(), Path::new("/h/state/bougied.pid"));
        assert_eq!(p.services_dir(), Path::new("/h/state/services"));
        assert_eq!(p.service_dir("redis"), Path::new("/h/state/services/redis"));
        assert_eq!(p.service_data("redis"), Path::new("/h/state/services/redis/data"));
        assert_eq!(p.service_run("redis"), Path::new("/h/state/services/redis/run"));
        assert_eq!(p.service_log("redis"), Path::new("/h/state/services/redis/log"));
        assert_eq!(p.service_conf("redis"), Path::new("/h/state/services/redis/conf"));
        assert_eq!(
            p.service_tenants("redis"),
            Path::new("/h/state/services/redis/tenants.json")
        );
    }
}
