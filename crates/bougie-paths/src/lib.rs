//! `BOUGIE_HOME` / `BOUGIE_LOCAL` / `BOUGIE_CACHE` resolution and
//! subpath helpers.
//!
//! Three roots, not two:
//!
//! - **`home`** — small, user-shaped state that should follow the user.
//!   On Unix it's `XDG_DATA_HOME` (per CLI.md §2.1); on Windows it's
//!   `%APPDATA%/Roaming/bougie` so `state/state.json` (which composer
//!   /php versions are installed where) and `state/public-keys/` (TUF
//!   trust anchors) roam across machines in a domain environment.
//! - **`local`** — machine-local, easily re-downloadable artifacts.
//!   On Unix it's the same as `home` (XDG has no roaming/local split);
//!   on Windows it's `%LOCALAPPDATA%/bougie` so the multi-GB
//!   `installs/` + `store/` trees and the per-machine `bin/` shim
//!   directory stay out of roaming profiles.
//! - **`cache`** — transient: `XDG_CACHE_HOME` on Unix, `%LOCALAPPDATA%`
//!   on Windows. Index responses and in-flight blob downloads.
//!
//! Override the lot via `BOUGIE_HOME` / `BOUGIE_LOCAL` / `BOUGIE_CACHE`.
//! Setting `BOUGIE_HOME` alone also collapses `local` to that same
//! path — preserves the pre-split single-root layout for users who
//! never cared about the distinction.

#[cfg(unix)]
use etcetera::base_strategy::{BaseStrategy, Xdg};
#[cfg(windows)]
use etcetera::base_strategy::{BaseStrategy, Windows};
use eyre::{Result, WrapErr};
use std::ffi::OsString;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct Paths {
    home: PathBuf,
    local: PathBuf,
    cache: PathBuf,
}

impl Paths {
    /// Resolve from environment + native base-dir defaults.
    pub fn from_env() -> Result<Self> {
        #[cfg(unix)]
        let (data, local, cache) = {
            let xdg = Xdg::new().wrap_err("could not resolve XDG base dirs")?;
            // XDG has no roaming/local split — `local` == `data`.
            let data = xdg.data_dir();
            (data.clone(), data, xdg.cache_dir())
        };
        // On Windows, split `home` (roaming, small) from `local` and
        // `cache` (machine-local). The pre-split layout anchored both
        // home and cache under %LOCALAPPDATA% to keep bougie's multi-GB
        // installs/ tree out of roaming profiles; now we keep that
        // property for the heavy stuff (which lives under `local`)
        // while letting `state/` (small, user-investment) roam.
        #[cfg(windows)]
        let (data, local, cache) = {
            let win = Windows::new().wrap_err("could not resolve Windows base dirs")?;
            (win.data_dir(), win.cache_dir(), win.cache_dir())
        };
        let resolved = Self::resolve(
            std::env::var_os("BOUGIE_HOME"),
            std::env::var_os("BOUGIE_LOCAL"),
            std::env::var_os("BOUGIE_CACHE"),
            &data,
            &local,
            &cache,
        );
        // One-shot Windows migration: pre-split layouts wrote
        // `state/` under %LOCALAPPDATA%/bougie/state. The new layout
        // expects it under %APPDATA%/Roaming/bougie/state. If the old
        // location has data and the new one doesn't, move it across so
        // the developer's existing state.json + public-keys survive
        // the upgrade. Idempotent: once the new dir exists this is a
        // single stat that no-ops.
        #[cfg(windows)]
        migrate_state_to_roaming(&resolved);
        Ok(resolved)
    }

    /// Pure resolver, exposed for unit tests.
    ///
    /// Resolution rules:
    /// - `env_home` overrides the `xdg_data`-derived default for `home`.
    /// - `env_local` overrides the `xdg_local`-derived default for `local`.
    ///   If `env_local` is unset *but* `env_home` is set, `local`
    ///   collapses to the same path — preserves pre-split single-root
    ///   layouts where the user set `BOUGIE_HOME` once.
    /// - `env_cache` overrides the `xdg_cache`-derived default for `cache`.
    pub fn resolve(
        env_home: Option<OsString>,
        env_local: Option<OsString>,
        env_cache: Option<OsString>,
        xdg_data: &Path,
        xdg_local: &Path,
        xdg_cache: &Path,
    ) -> Self {
        let home = env_home
            .as_ref()
            .map_or_else(|| xdg_data.join("bougie"), PathBuf::from);
        // BOUGIE_HOME alone collapses local→home (back-compat for
        // single-root users). An explicit BOUGIE_LOCAL always wins.
        let local = match (env_local, env_home.as_ref()) {
            (Some(l), _) => PathBuf::from(l),
            (None, Some(_)) => home.clone(),
            (None, None) => xdg_local.join("bougie"),
        };
        let cache = env_cache.map_or_else(|| xdg_cache.join("bougie"), PathBuf::from);
        Self { home, local, cache }
    }

    /// Construct directly from explicit `home` + `cache` paths, with
    /// `local` collapsed to `home`. Kept as the two-arg shorthand the
    /// test suite uses; production code goes through [`from_env`] or
    /// [`with_local`].
    pub fn new(home: PathBuf, cache: PathBuf) -> Self {
        Self {
            local: home.clone(),
            home,
            cache,
        }
    }

    /// Construct directly from all three roots — for tests that exercise
    /// the home/local split explicitly.
    pub fn with_local(home: PathBuf, local: PathBuf, cache: PathBuf) -> Self {
        Self { home, local, cache }
    }

    pub fn home(&self) -> &Path {
        &self.home
    }
    pub fn local(&self) -> &Path {
        &self.local
    }
    pub fn cache(&self) -> &Path {
        &self.cache
    }

    // ---------- machine-local artifacts (under `local`) ----------

    pub fn installs(&self) -> PathBuf {
        self.local.join("installs")
    }
    pub fn store(&self) -> PathBuf {
        self.local.join("store")
    }
    pub fn bin(&self) -> PathBuf {
        self.local.join("bin")
    }

    /// `$BOUGIE_LOCAL/composer/` — managed Composer installs, one
    /// directory per version. Local because composer phars are
    /// re-downloadable from getcomposer.org on demand.
    pub fn composer_root(&self) -> PathBuf {
        self.local.join("composer")
    }
    /// Path to the phar for a specific Composer version:
    /// `$BOUGIE_LOCAL/composer/<version>/composer.phar`.
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

    // ---------- user state (under `home`) ----------

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

    // ---------- transient cache (under `cache`) ----------

    /// Per-origin index cache root: `$BOUGIE_CACHE/index/<host>/`.
    pub fn cache_index(&self, host: &str) -> PathBuf {
        self.cache.join("index").join(host)
    }
    /// In-flight blob downloads (`.partial` files only).
    pub fn cache_blobs(&self) -> PathBuf {
        self.cache.join("blobs")
    }
    /// Persistent, content-addressed cache of Composer package dist
    /// archives at `$BOUGIE_CACHE/composer-dist/<sha1>.<ext>`. Unlike
    /// [`cache_blobs`] (which holds in-flight `.partial` files and is
    /// purged after each fetch), this directory keeps the verified
    /// archives so a second `bougie composer install` in another
    /// project that needs the same `monolog/monolog 3.2.0` reuses the
    /// existing copy without a network round-trip. Mirrors what
    /// Composer's own `~/.composer/cache/files/` does, just under
    /// bougie's XDG-strict layout.
    pub fn cache_composer_dist(&self) -> PathBuf {
        self.cache.join("composer-dist")
    }
    /// Persistent cache of Packagist v2 metadata documents at
    /// `$BOUGIE_CACHE/composer-metadata/p2/<vendor>/<name>.json` (plus
    /// `~dev.json`) with `ETag` sidecars alongside each. Shared across
    /// projects: a `monolog/monolog` metadata fetch from project A is
    /// reused by project B via conditional GET, avoiding the multi-MB
    /// re-download Composer is forced into without a warm cache.
    pub fn cache_composer_metadata(&self) -> PathBuf {
        self.cache.join("composer-metadata")
    }

    // ---------- `bougied` daemon + service supervisor paths ----------
    //
    // See SERVICES.md and CLI.md §2.1 in the php-build-standalone repo
    // for the canonical layout. Socket and pid file live directly under
    // `state/`; everything else hangs under `state/services/<name>/`.
    //
    // These all sit under `state` (= `home`). On Unix that's
    // XDG_DATA_HOME, matching today's behaviour; on Windows the daemon
    // doesn't run so the paths are computed but unused.

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

/// If a pre-split `state/` directory exists under `local` but not
/// under `home`, rename it into place. Best-effort: a failure leaves
/// the user with no `state/` at the new location which startup will
/// then materialize as empty — surfacing as "state.json missing"
/// rather than silently mixing old + new layouts.
#[cfg(windows)]
fn migrate_state_to_roaming(paths: &Paths) {
    let new_state = paths.state();
    let old_state = paths.local.join("state");
    if new_state.exists() || !old_state.exists() {
        return;
    }
    if let Some(parent) = new_state.parent() {
        if std::fs::create_dir_all(parent).is_err() {
            return;
        }
    }
    let _ = std::fs::rename(&old_state, &new_state);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_home_collapses_local_for_single_root_users() {
        // Pre-split callers set just BOUGIE_HOME — the resolver must
        // collapse `local` to the same path so installs/, store/ etc.
        // continue landing under the override rather than splitting
        // back to the XDG default.
        let p = Paths::resolve(
            Some(OsString::from("/tmp/bh")),
            None,
            None,
            Path::new("/data"),
            Path::new("/local"),
            Path::new("/cache"),
        );
        assert_eq!(p.home(), Path::new("/tmp/bh"));
        assert_eq!(p.local(), Path::new("/tmp/bh"));
        assert_eq!(p.cache(), Path::new("/cache/bougie"));
    }

    #[test]
    fn env_local_splits_home_and_local() {
        let p = Paths::resolve(
            Some(OsString::from("/tmp/bh")),
            Some(OsString::from("/tmp/bl")),
            None,
            Path::new("/data"),
            Path::new("/local"),
            Path::new("/cache"),
        );
        assert_eq!(p.home(), Path::new("/tmp/bh"));
        assert_eq!(p.local(), Path::new("/tmp/bl"));
    }

    #[test]
    fn env_cache_overrides_xdg() {
        let p = Paths::resolve(
            None,
            None,
            Some(OsString::from("/tmp/bc")),
            Path::new("/data"),
            Path::new("/local"),
            Path::new("/cache"),
        );
        assert_eq!(p.home(), Path::new("/data/bougie"));
        assert_eq!(p.local(), Path::new("/local/bougie"));
        assert_eq!(p.cache(), Path::new("/tmp/bc"));
    }

    #[test]
    fn xdg_defaults_when_no_env() {
        let p = Paths::resolve(
            None,
            None,
            None,
            Path::new("/data"),
            Path::new("/local"),
            Path::new("/cache"),
        );
        assert_eq!(p.home(), Path::new("/data/bougie"));
        assert_eq!(p.local(), Path::new("/local/bougie"));
        assert_eq!(p.cache(), Path::new("/cache/bougie"));
    }

    /// Downloads + shims compose under `local`; state composes under
    /// `home`. `With_local` exposes both so the assertions can split them
    /// cleanly.
    #[test]
    fn download_paths_use_local_state_paths_use_home() {
        let p = Paths::with_local(
            PathBuf::from("/h"),
            PathBuf::from("/l"),
            PathBuf::from("/c"),
        );
        // Local (re-downloadable / machine-specific).
        assert_eq!(p.installs(), Path::new("/l/installs"));
        assert_eq!(p.store(), Path::new("/l/store"));
        assert_eq!(p.bin(), Path::new("/l/bin"));
        assert_eq!(p.composer_root(), Path::new("/l/composer"));
        assert_eq!(p.composer_phar("2.8.5"), Path::new("/l/composer/2.8.5/composer.phar"));
        assert_eq!(p.composer_channels_json(), Path::new("/l/composer/channels.json"));
        // State (small, user-investment, may roam).
        assert_eq!(p.state(), Path::new("/h/state"));
        assert_eq!(p.global_lock(), Path::new("/h/state/locks/global.lock"));
        assert_eq!(p.state_json(), Path::new("/h/state/state.json"));
        assert_eq!(p.public_keys(), Path::new("/h/state/public-keys"));
        // Cache (transient).
        assert_eq!(p.cache_index("origin.example"), Path::new("/c/index/origin.example"));
        assert_eq!(p.cache_blobs(), Path::new("/c/blobs"));
        assert_eq!(p.cache_composer_dist(), Path::new("/c/composer-dist"));
        assert_eq!(p.cache_composer_metadata(), Path::new("/c/composer-metadata"));
    }

    /// Two-arg `new(home, cache)` shorthand keeps the legacy single-root
    /// layout: downloads and state both compose under `home`. Existing
    /// tests rely on this — splitting them would touch ~20 call sites
    /// for no behavioural win.
    #[test]
    fn two_arg_new_collapses_local_to_home() {
        let p = Paths::new(PathBuf::from("/h"), PathBuf::from("/c"));
        assert_eq!(p.installs(), Path::new("/h/installs"));
        assert_eq!(p.state(), Path::new("/h/state"));
        assert_eq!(p.bin(), Path::new("/h/bin"));
        assert_eq!(p.cache_blobs(), Path::new("/c/blobs"));
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

    /// One-shot migration: if pre-split state/ exists under `local` and
    /// nothing exists at the new `home` location, rename it across.
    #[cfg(windows)]
    #[test]
    fn migrate_state_to_roaming_moves_pre_split_layout() {
        let td = tempfile::TempDir::new().unwrap();
        let home = td.path().join("roaming");
        let local = td.path().join("local");
        std::fs::create_dir_all(local.join("state").join("locks")).unwrap();
        std::fs::write(local.join("state").join("state.json"), b"{}").unwrap();
        let paths = Paths::with_local(home.clone(), local.clone(), local.clone());
        migrate_state_to_roaming(&paths);
        assert!(home.join("state").join("state.json").exists(),
            "state.json must land under new home");
        assert!(!local.join("state").exists(),
            "old state/ must be moved, not copied");
    }

    /// Migration is idempotent: with the new state/ already in place,
    /// `migrate` must not touch it even if an empty old state/ also
    /// exists (the "user re-ran bougie after the first migration"
    /// case).
    #[cfg(windows)]
    #[test]
    fn migrate_state_to_roaming_skips_when_new_state_exists() {
        let td = tempfile::TempDir::new().unwrap();
        let home = td.path().join("roaming");
        let local = td.path().join("local");
        std::fs::create_dir_all(home.join("state")).unwrap();
        std::fs::write(home.join("state").join("state.json"), b"new").unwrap();
        std::fs::create_dir_all(local.join("state")).unwrap();
        std::fs::write(local.join("state").join("state.json"), b"OLD").unwrap();
        let paths = Paths::with_local(home.clone(), local.clone(), local.clone());
        migrate_state_to_roaming(&paths);
        assert_eq!(
            std::fs::read(home.join("state").join("state.json")).unwrap(),
            b"new"
        );
    }
}
