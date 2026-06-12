//! Per-pool conf.d variant generation + php-fpm pool config emission.
//! Spec: SERVER.md §7.3, §7.4.
//!
//! Two artifacts per pool spawn:
//!
//! 1. `<variant>.conf` — the FPM pool config file passed to
//!    `php-fpm -y`. Hand-emitted INI; the schema is small and fixed.
//! 2. `<variant>.confd/` — directory of symlinks to source fragments
//!    under `<project>/.bougie/conf.d{,-debug}/`. The "normal" variant
//!    is built from `conf.d/` only; the "xdebug" variant merges both
//!    `conf.d/` and `conf.d-debug/`. The overlay dir holds fragments
//!    the server itself wrote (via
//!    `commands::server::pool::ensure_debug_extension`) when it
//!    lazily activated xdebug for a request — `bougie ext add xdebug`
//!    instead lands in `conf.d/` and is visible to every variant.

use eyre::{Result, WrapErr};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

use super::paths::create_dir_0700;

/// Build the `<variant>.confd/` directory under the per-project runtime
/// dir. Each entry is a symlink to a `.ini` fragment in one of
/// `source_dirs`. Earlier sources win on filename collision so the
/// caller can order them most-stable-first (the project's `conf.d/`
/// before the debug overlay). Idempotent: any prior `.confd/` is
/// replaced.
pub fn build_variant_confd(
    target_dir: &Path,
    source_dirs: &[&Path],
) -> Result<Vec<PathBuf>> {
    // Drop any stale variant directory so prefix changes between
    // bougie releases don't leave orphans. `remove_dir_all` returns
    // NotFound on a fresh tree; ignore.
    match std::fs::remove_dir_all(target_dir) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => {
            return Err(eyre::eyre!(
                "removing stale {}: {e}",
                target_dir.display()
            ));
        }
    }
    create_dir_0700(target_dir)?;

    let mut linked = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for source in source_dirs {
        let entries = match std::fs::read_dir(source) {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => {
                return Err(eyre::eyre!("reading {}: {e}", source.display()));
            }
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) != Some("ini") {
                continue;
            }
            let Some(fname) = path.file_name().and_then(|s| s.to_str()) else {
                continue;
            };
            if !seen.insert(fname.to_owned()) {
                continue;
            }
            let link = target_dir.join(fname);
            link_fragment(&path, &link).wrap_err_with(|| {
                format!("linking {} -> {}", link.display(), path.display())
            })?;
            linked.push(link);
        }
    }
    Ok(linked)
}

/// Materialize one ini fragment from `source` at `link`. Unix uses a
/// symlink (cheap, points back to the user's `conf.d/`); Windows uses
/// a hard link on NTFS (same inode-equivalent, no Dev Mode required —
/// `symlink_file` would otherwise demand admin or developer mode). The
/// fragments are small files inside the same volume, so a hard link is
/// always feasible.
#[cfg(unix)]
fn link_fragment(source: &Path, link: &Path) -> Result<()> {
    std::os::unix::fs::symlink(source, link)
        .wrap_err_with(|| format!("symlink {} -> {}", link.display(), source.display()))?;
    Ok(())
}

#[cfg(windows)]
fn link_fragment(source: &Path, link: &Path) -> Result<()> {
    std::fs::hard_link(source, link)
        .wrap_err_with(|| format!("hardlink {} -> {}", link.display(), source.display()))?;
    Ok(())
}

/// Pool config emitted to `<variant>.conf`. SERVER.md §7.3 schema.
/// Unix-only: this is a php-fpm pool config, and php-fpm doesn't
/// exist on Windows. The Windows runtime (`php-cgi.exe -b`) is
/// configured purely via CLI args + environment.
#[cfg(unix)]
#[derive(Debug)]
pub struct PoolConf<'a> {
    /// Path to the listen unix socket.
    pub listen_socket: &'a Path,
    /// Path to the per-variant conf.d directory bougie injects into the
    /// worker env, or `None` for a **system** PHP — where bougie does not
    /// inject its (ABI-foreign) extension fragments and the interpreter
    /// keeps its own compiled-in conf.d (Homebrew/distro php.ini).
    pub php_ini_scan_dir: Option<&'a Path>,
}

#[cfg(unix)]
impl PoolConf<'_> {
    pub fn render(&self) -> String {
        // The worker-env scan-dir line is omitted for system PHP so the
        // interpreter loads its own extensions rather than bougie's store.
        let scan_dir_line = match self.php_ini_scan_dir {
            Some(dir) => format!("env[PHP_INI_SCAN_DIR] = {}\n", dir.display()),
            None => String::new(),
        };
        format!(
            "; managed by bougie server — regenerated on every pool spawn\n\
             [global]\n\
             daemonize = no\n\
             error_log = /dev/stderr\n\
             \n\
             [www]\n\
             listen = {socket}\n\
             listen.mode = 0600\n\
             pm = ondemand\n\
             pm.max_children = 16\n\
             pm.process_idle_timeout = 60s\n\
             catch_workers_output = yes\n\
             clear_env = no\n\
             ; Web requests default to 1G — php.ini's 128M default OOMs\n\
             ; Magento pages. `php_value` (not `php_admin_value`) so a\n\
             ; project's own .user.ini / ini_set keeps the final say\n\
             ; (e.g. Magento's pub/.user.ini). CLI php is set separately\n\
             ; (memory_limit=-1) in the argv[0] shim.\n\
             php_value[memory_limit] = 1G\n\
             {scan_dir_line}",
            socket = self.listen_socket.display(),
        )
    }
}

/// Atomically write the pool conf at `path`. Tempfile + rename so a
/// `kill -9` mid-write can't leave the pool with a half-written conf
/// that php-fpm would reject on next spawn. Unix-only (see [`PoolConf`]).
#[cfg(unix)]
pub fn write_pool_conf(path: &Path, conf: &PoolConf<'_>) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| eyre::eyre!("pool conf path has no parent: {}", path.display()))?;
    create_dir_0700(parent)?;
    let tmp = path.with_extension("conf.tmp");
    std::fs::write(&tmp, conf.render())
        .wrap_err_with(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, path)
        .wrap_err_with(|| format!("renaming {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn write_fragment(dir: &Path, name: &str, body: &str) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(dir.join(name), body).unwrap();
    }

    #[test]
    fn variant_links_every_ini_from_single_source() {
        let td = TempDir::new().unwrap();
        let source = td.path().join("conf.d");
        let target = td.path().join("normal.confd");
        write_fragment(&source, "20-redis.ini", "extension=redis.so\n");
        write_fragment(&source, "35-pdo_mysql.ini", "extension=pdo_mysql.so\n");

        let linked = build_variant_confd(&target, &[&source]).unwrap();
        assert_eq!(linked.len(), 2);
        assert!(target.join("20-redis.ini").exists());
        assert!(target.join("35-pdo_mysql.ini").exists());
    }

    #[test]
    fn variant_excludes_debug_dir_when_not_listed() {
        let td = TempDir::new().unwrap();
        let normal = td.path().join("conf.d");
        let debug = td.path().join("conf.d-debug");
        let target = td.path().join("normal.confd");
        write_fragment(&normal, "20-redis.ini", "extension=redis.so\n");
        write_fragment(&debug, "30-xdebug.ini", "zend_extension=xdebug.so\n");

        // Normal variant only sees `conf.d/`.
        let linked = build_variant_confd(&target, &[&normal]).unwrap();
        let names: Vec<String> = linked
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["20-redis.ini".to_string()]);
        assert!(!target.join("30-xdebug.ini").exists());
    }

    #[test]
    fn variant_merges_debug_dir_when_listed() {
        let td = TempDir::new().unwrap();
        let normal = td.path().join("conf.d");
        let debug = td.path().join("conf.d-debug");
        let target = td.path().join("xdebug.confd");
        write_fragment(&normal, "20-redis.ini", "extension=redis.so\n");
        write_fragment(&debug, "30-xdebug.ini", "zend_extension=xdebug.so\n");

        let linked = build_variant_confd(&target, &[&normal, &debug]).unwrap();
        let mut names: Vec<String> = linked
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(names, vec!["20-redis.ini".to_string(), "30-xdebug.ini".to_string()]);
    }

    /// Unix-only: the assertion uses `read_link` which is meaningful
    /// for the symlink path. On Windows the fragments are hard-linked
    /// (no Dev Mode requirement); `read_link` returns Err there. The
    /// "earlier source wins" invariant is the same on both platforms —
    /// this just exercises it via the symlink target.
    #[cfg(unix)]
    #[test]
    fn variant_earlier_source_wins_on_collision() {
        let td = TempDir::new().unwrap();
        let primary = td.path().join("conf.d");
        let overlay = td.path().join("conf.d-debug");
        let target = td.path().join("xdebug.confd");
        write_fragment(&primary, "30-xdebug.ini", "from-primary\n");
        write_fragment(&overlay, "30-xdebug.ini", "from-overlay\n");

        let _ = build_variant_confd(&target, &[&primary, &overlay]).unwrap();
        // Link should resolve through primary.
        let resolved = std::fs::read_link(target.join("30-xdebug.ini")).unwrap();
        assert!(resolved.starts_with(&primary));
    }

    /// Cross-platform equivalent: read the file content rather than
    /// the link target, since hard-linked files don't expose origin.
    #[test]
    fn variant_earlier_source_wins_by_content() {
        let td = TempDir::new().unwrap();
        let primary = td.path().join("conf.d");
        let overlay = td.path().join("conf.d-debug");
        let target = td.path().join("xdebug.confd");
        write_fragment(&primary, "30-xdebug.ini", "from-primary\n");
        write_fragment(&overlay, "30-xdebug.ini", "from-overlay\n");

        let _ = build_variant_confd(&target, &[&primary, &overlay]).unwrap();
        let body = std::fs::read_to_string(target.join("30-xdebug.ini")).unwrap();
        assert_eq!(body, "from-primary\n");
    }

    #[test]
    fn variant_ignores_non_ini_files() {
        let td = TempDir::new().unwrap();
        let source = td.path().join("conf.d");
        let target = td.path().join("normal.confd");
        write_fragment(&source, "README.md", "# notes\n");
        write_fragment(&source, "20-redis.ini", "extension=redis.so\n");

        let linked = build_variant_confd(&target, &[&source]).unwrap();
        assert_eq!(linked.len(), 1);
        assert!(linked[0].ends_with("20-redis.ini"));
    }

    #[test]
    fn variant_replaces_stale_directory() {
        let td = TempDir::new().unwrap();
        let source = td.path().join("conf.d");
        let target = td.path().join("normal.confd");
        write_fragment(&target, "stale.ini", "leftover\n");
        write_fragment(&source, "20-redis.ini", "extension=redis.so\n");

        let _ = build_variant_confd(&target, &[&source]).unwrap();
        assert!(target.join("20-redis.ini").exists());
        assert!(!target.join("stale.ini").exists());
    }

    #[test]
    fn variant_returns_empty_when_sources_missing() {
        let td = TempDir::new().unwrap();
        let source = td.path().join("does-not-exist");
        let target = td.path().join("normal.confd");
        let linked = build_variant_confd(&target, &[&source]).unwrap();
        assert!(linked.is_empty());
        assert!(target.exists());
    }

    #[cfg(unix)]
    #[test]
    fn pool_conf_render_matches_schema() {
        let conf = PoolConf {
            listen_socket: Path::new("/run/x/normal.sock"),
            php_ini_scan_dir: Some(Path::new("/run/x/normal.confd")),
        };
        let rendered = conf.render();
        assert!(rendered.contains("listen = /run/x/normal.sock"));
        assert!(rendered.contains("listen.mode = 0600"));
        assert!(rendered.contains("pm = ondemand"));
        assert!(rendered.contains("env[PHP_INI_SCAN_DIR] = /run/x/normal.confd"));
        assert!(rendered.contains("daemonize = no"));
        assert!(rendered.contains("clear_env = no"));
        // Web requests get a workable default memory limit (128M OOMs
        // Magento), overridable by the app — `php_value`, not
        // `php_admin_value`.
        assert!(rendered.contains("php_value[memory_limit] = 1G"));
    }

    #[cfg(unix)]
    #[test]
    fn write_pool_conf_round_trip() {
        let td = TempDir::new().unwrap();
        let path = td.path().join("normal.conf");
        let conf = PoolConf {
            listen_socket: Path::new("/run/normal.sock"),
            php_ini_scan_dir: Some(Path::new("/run/normal.confd")),
        };
        write_pool_conf(&path, &conf).unwrap();
        let body = std::fs::read_to_string(&path).unwrap();
        assert!(body.contains("pm = ondemand"));
    }

    #[cfg(unix)]
    #[test]
    fn pool_conf_omits_scan_dir_for_system_php() {
        let conf = PoolConf {
            listen_socket: Path::new("/run/x/normal.sock"),
            php_ini_scan_dir: None,
        };
        let rendered = conf.render();
        assert!(rendered.contains("listen = /run/x/normal.sock"));
        // System PHP keeps its own conf.d — bougie injects no scan dir.
        assert!(!rendered.contains("PHP_INI_SCAN_DIR"));
        assert!(rendered.contains("php_value[memory_limit] = 1G"));
    }
}
