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
        // An env var that is *set but empty* (`BOUGIE_HOME=`) is not a
        // valid override — `PathBuf::from("")` would make all derived
        // paths relative to the cwd. Treat empty as unset.
        let env_home = env_home.filter(|s| !s.is_empty());
        let env_local = env_local.filter(|s| !s.is_empty());
        let env_cache = env_cache.filter(|s| !s.is_empty());
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
    /// `$BOUGIE_LOCAL/node-installs/` — per-version Node.js install trees.
    /// Separate from `installs/` (PHP) because node has no flavor axis: the
    /// layout is `node-installs/<version>/{bin,lib,...}` (the official
    /// tarball's `node-v<ver>-<plat>/` wrapper is stripped on extract), so
    /// it doesn't share PHP's `<version>-<flavor>` naming.
    pub fn node_installs(&self) -> PathBuf {
        self.local.join("node-installs")
    }
    /// `$BOUGIE_LOCAL/node-installs/<version>/` — one Node.js version's
    /// root. `node` / `npm` / `npx` live under its `bin/`.
    pub fn node_install_dir(&self, version: &str) -> PathBuf {
        self.node_installs().join(version)
    }
    pub fn store(&self) -> PathBuf {
        self.local.join("store")
    }
    pub fn bin(&self) -> PathBuf {
        self.local.join("bin")
    }

    /// `$BOUGIE_LOCAL/tools/` — per-tool install trees for
    /// `bougie tool`. Local because, like `installs/` and `composer/`,
    /// a tool dir is fully re-creatable from `bougie tool install`.
    pub fn tools(&self) -> PathBuf {
        self.local.join("tools")
    }

    /// `$BOUGIE_LOCAL/tools/<vendor>-<name>/` — single tool's root.
    /// Composer identifiers are slash-separated; the slash is replaced
    /// with `-` for the on-disk dir so the path is one segment deep.
    pub fn tool_dir(&self, package: &str) -> PathBuf {
        self.tools().join(package.replace('/', "-"))
    }

    /// `$BOUGIE_LOCAL/exec-shims/` — bougie-managed helper shims
    /// (currently just `unzip`) that get *prepended* to a tool's `PATH`
    /// at exec time. This lets a tool that shells out to `unzip` — e.g.
    /// the real Composer's `ZipDownloader`, installed via
    /// `bougie tool install composer/composer` and run with `bgx` — find
    /// bougie's `unzip` shim, without seeding it on the user's global
    /// `PATH` (which would shadow the system `unzip` for everything).
    pub fn exec_shims(&self) -> PathBuf {
        self.local.join("exec-shims")
    }

    /// User-facing bin directory where tool launcher symlinks land.
    /// Resolves `BOUGIE_TOOL_BIN_DIR`, then `XDG_BIN_HOME`, then
    /// `~/.local/bin`. On Windows the default is
    /// `%LOCALAPPDATA%/bougie/bin` (computed from `local`) — Phase 1
    /// only emits the symlink on Unix, but the path resolves on both
    /// platforms so callers can render help text consistently.
    pub fn tool_bin_dir(&self) -> PathBuf {
        if let Some(v) = std::env::var_os("BOUGIE_TOOL_BIN_DIR") {
            return PathBuf::from(v);
        }
        #[cfg(unix)]
        {
            if let Some(v) = std::env::var_os("XDG_BIN_HOME") {
                return PathBuf::from(v);
            }
            if let Some(home) = std::env::var_os("HOME") {
                return PathBuf::from(home).join(".local").join("bin");
            }
            // Last-resort fallback; callers typically surface a clearer
            // error before reaching this branch.
            self.local.join("bin")
        }
        #[cfg(windows)]
        {
            self.local.join("bin")
        }
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

    /// Root of the ephemeral `bougie tool run` cache. Each subdir is
    /// a content-addressed install slot keyed by
    /// `(package, constraint, php_version, php_flavor, sorted_with)`
    /// — see `bougie_tool::run::cache_key`. `bougie cache prune`
    /// walks this dir by mtime and drops entries past the TTL.
    pub fn cache_tool_run(&self) -> PathBuf {
        self.cache.join("tool-run")
    }

    /// Specific tool-run cache slot: `cache_tool_run/<hash>/`.
    pub fn cache_tool_run_dir(&self, key: &str) -> PathBuf {
        self.cache_tool_run().join(key)
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

    // ---------- durable per-project state (under `home`) ----------
    //
    // The project-local toolchain dir (`project::dir`) lives under
    // `vendor/` and is disposable — `rm -rf vendor` wipes it and a
    // `bougie sync` regenerates it. Anything that *can't* be
    // regenerated (because it has no other source of truth) lives
    // here instead, keyed by a hash of the project's canonical path so
    // it survives a vendor wipe.

    /// `$BOUGIE_HOME/state/projects/<project-hash>/` — durable,
    /// machine-local state for a single project that must outlive a
    /// `vendor/` wipe. Keyed by [`project_hash`] so it stays stable
    /// across `cd ./foo` vs. an absolute path.
    pub fn project_state_dir(&self, project_root: &Path) -> PathBuf {
        self.state()
            .join("projects")
            .join(project_hash(project_root))
    }

    /// `<project-state>/conf.d-local/` — machine-local extensions added
    /// via `bougie ext add --so <path>`. These are NOT recorded in
    /// `composer.json` and NOT mirrored by `bougie sync`, so they have
    /// no other source of truth: they live under `$BOUGIE_HOME` rather
    /// than in the disposable `vendor/bougie/` tree.
    pub fn project_confd_local(&self, project_root: &Path) -> PathBuf {
        self.project_state_dir(project_root).join("conf.d-local")
    }
}

/// Project-local *disposable* toolchain dir and its subpaths. Lives
/// under `vendor/` (created by `bougie sync`, wiped by `rm -rf vendor`)
/// — analogous to a Python `.venv` or `node_modules/.bin`. Everything
/// here is regenerable from `composer.json` / `composer.lock` + the
/// resolver, so nothing in it is a source of truth. Durable per-project
/// state lives under `$BOUGIE_HOME` (see [`Paths::project_confd_local`]).
pub mod project {
    use std::path::{Path, PathBuf};

    /// `<root>/vendor/bougie` — the project-local toolchain dir.
    pub fn dir(root: &Path) -> PathBuf {
        root.join("vendor").join("bougie")
    }

    /// `true` if `dir` is a bougie project root (carries the marker dir).
    /// Used by the ancestor-walk project-root discovery.
    pub fn is_root(dir: &Path) -> bool {
        self::dir(dir).is_dir()
    }

    /// `<root>/vendor/bougie/bin` — shim symlinks (`php`, `composer`, …).
    pub fn bin_dir(root: &Path) -> PathBuf {
        dir(root).join("bin")
    }

    /// `<root>/vendor/bougie/state` — resolution markers.
    pub fn state_dir(root: &Path) -> PathBuf {
        dir(root).join("state")
    }

    /// `<root>/vendor/bougie/state/bin` — ephemeral recipe bin dir.
    pub fn state_bin_dir(root: &Path) -> PathBuf {
        state_dir(root).join("bin")
    }

    /// `<root>/vendor/bougie/state/resolved` — resolved `<ver>-<flavor>`.
    pub fn resolved(root: &Path) -> PathBuf {
        state_dir(root).join("resolved")
    }

    /// `<root>/vendor/bougie/state/resolved-php-path` — system-PHP path
    /// marker (present only for system-PHP projects).
    pub fn resolved_php_path(root: &Path) -> PathBuf {
        state_dir(root).join("resolved-php-path")
    }

    /// `<root>/vendor/bougie/conf.d` — declared-extension fragments.
    pub fn confd(root: &Path) -> PathBuf {
        dir(root).join("conf.d")
    }

    /// `<root>/vendor/bougie/conf.d-debug` — server lazy-xdebug overlay.
    pub fn confd_debug(root: &Path) -> PathBuf {
        dir(root).join("conf.d-debug")
    }

    /// `<root>/vendor/bougie/.lock` — per-project sync flock.
    pub fn lock(root: &Path) -> PathBuf {
        dir(root).join(".lock")
    }
}

/// Global (non-project) config dir: `${XDG_CONFIG_HOME:-~/.config}/bougie`
/// on Unix, `%APPDATA%\bougie` on Windows.
///
/// Deliberately *not* under `$BOUGIE_HOME`: this is where the dist
/// installer drops its receipt (`bougie-receipt.json`) and where the
/// installer consent snippets (`scripts/install-consent.{sh,ps1}`)
/// write the telemetry mode file — plain shell must be able to compute
/// the path without knowing bougie's `BOUGIE_HOME` rules. Keep the
/// resolution here byte-compatible with those snippets.
pub fn config_dir() -> Result<PathBuf> {
    #[cfg(unix)]
    {
        let xdg = Xdg::new().wrap_err("could not resolve XDG base dirs")?;
        Ok(xdg.config_dir().join("bougie"))
    }
    #[cfg(windows)]
    {
        let win = Windows::new().wrap_err("could not resolve Windows base dirs")?;
        Ok(win.config_dir().join("bougie"))
    }
}

/// The telemetry consent mode file (single line:
/// `<mode> <yyyy-mm-dd> <consent-version>`), next to the install
/// receipt so both are discoverable by installer-side shell.
pub fn telemetry_mode_file() -> Result<PathBuf> {
    config_dir().map(|d| d.join("telemetry"))
}

/// First 12 hex chars of `sha256(canonical_project_path)`. Used as the
/// per-project directory name under `$BOUGIE_HOME/state/projects/` and
/// the server's runtime root. Canonicalization keeps the hash stable
/// across `cd ./foo` vs. `cd $(pwd)/foo` — same project, same hash.
pub fn project_hash(project: &Path) -> String {
    use sha2::{Digest, Sha256};
    use std::fmt::Write as _;
    let canonical = project
        .canonicalize()
        .unwrap_or_else(|_| project.to_path_buf());
    let bytes = canonical
        .to_str()
        .map(str::as_bytes)
        .or_else(|| {
            // Non-UTF-8 path: fall back to the raw OsStr bytes on unix.
            #[cfg(unix)]
            {
                use std::os::unix::ffi::OsStrExt;
                Some(canonical.as_os_str().as_bytes())
            }
            #[cfg(not(unix))]
            {
                None
            }
        })
        .unwrap_or(b"");
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let digest = hasher.finalize();
    let mut out = String::with_capacity(12);
    for b in digest.iter().take(6) {
        write!(out, "{b:02x}").expect("writing to String");
    }
    out
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
        assert_eq!(p.cache_tool_run(), Path::new("/c/tool-run"));
        assert_eq!(
            p.cache_tool_run_dir("abc123"),
            Path::new("/c/tool-run/abc123")
        );
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
    fn tool_paths_compose_under_local() {
        let p = Paths::with_local(
            PathBuf::from("/h"),
            PathBuf::from("/l"),
            PathBuf::from("/c"),
        );
        assert_eq!(p.tools(), Path::new("/l/tools"));
        assert_eq!(
            p.tool_dir("phpstan/phpstan"),
            Path::new("/l/tools/phpstan-phpstan")
        );
    }

    #[test]
    fn project_local_dir_lives_under_vendor() {
        let root = Path::new("/srv/app");
        assert_eq!(project::dir(root), Path::new("/srv/app/vendor/bougie"));
        assert_eq!(project::bin_dir(root), Path::new("/srv/app/vendor/bougie/bin"));
        assert_eq!(
            project::resolved(root),
            Path::new("/srv/app/vendor/bougie/state/resolved")
        );
        assert_eq!(
            project::confd(root),
            Path::new("/srv/app/vendor/bougie/conf.d")
        );
        assert_eq!(
            project::state_bin_dir(root),
            Path::new("/srv/app/vendor/bougie/state/bin")
        );
    }

    #[test]
    fn durable_conf_d_local_lives_under_home_keyed_by_hash() {
        let p = Paths::new(PathBuf::from("/h"), PathBuf::from("/c"));
        let root = Path::new("/srv/app");
        let hash = project_hash(root);
        assert_eq!(
            p.project_confd_local(root),
            PathBuf::from(format!("/h/state/projects/{hash}/conf.d-local"))
        );
        // Distinct from the disposable vendor dir.
        assert!(!p.project_confd_local(root).starts_with(root));
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
