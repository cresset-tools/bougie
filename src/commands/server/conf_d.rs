//! Per-pool conf.d variant generation + php-fpm pool config emission.
//! Spec: SERVER.md §7.3, §7.4.
//!
//! Two artifacts per pool spawn:
//!
//! 1. `<variant>.conf` — the FPM pool config file passed to
//!    `php-fpm -y`. Hand-emitted INI; the schema is small and fixed.
//! 2. `<variant>.confd/` — directory of symlinks to source fragments
//!    under `<project>/.bougie/conf.d/`. The "normal" variant excludes
//!    fragments whose extension name appears in
//!    `[server].debug_only_extensions` (default `["xdebug"]`); the
//!    "xdebug" variant (phase 3) carries every fragment.

use eyre::{Result, WrapErr};
use std::path::{Path, PathBuf};

use super::paths::create_dir_0700;

/// Build the `<variant>.confd/` directory under the per-project runtime
/// dir. Each entry is a symlink to a `.ini` fragment in the project's
/// `.bougie/conf.d/`. `exclude_extensions` is consulted by ext name —
/// for the "normal" variant pass `["xdebug"]`; for the "xdebug" variant
/// pass `&[]`. Idempotent: any prior `.confd/` is replaced.
pub fn build_variant_confd(
    target_dir: &Path,
    source_confd: &Path,
    exclude_extensions: &[String],
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
    let entries = match std::fs::read_dir(source_confd) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(linked),
        Err(e) => {
            return Err(eyre::eyre!(
                "reading {}: {e}",
                source_confd.display()
            ));
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
        if let Some(name) = extension_name_from_fragment(fname)
            && exclude_extensions.iter().any(|x| x.eq_ignore_ascii_case(name))
        {
            continue;
        }
        let link = target_dir.join(fname);
        std::os::unix::fs::symlink(&path, &link)
            .wrap_err_with(|| format!("symlinking {} -> {}", link.display(), path.display()))?;
        linked.push(link);
    }
    Ok(linked)
}

/// Parse the extension name out of a conf.d fragment basename. The
/// project's fragments follow the `<NN>-<name>.ini` convention bougie
/// itself writes via `src/conf_d.rs::write_ext_fragment`. Returns `None`
/// for files that don't match — those are passed through unfiltered.
fn extension_name_from_fragment(filename: &str) -> Option<&str> {
    // `35-pdo_mysql.ini` → `pdo_mysql`.
    let stem = filename.strip_suffix(".ini")?;
    let dash = stem.find('-')?;
    let after = &stem[dash + 1..];
    if after.is_empty() {
        return None;
    }
    let leading: &str = &stem[..dash];
    if !leading.bytes().all(|c| c.is_ascii_digit()) {
        return None;
    }
    Some(after)
}

/// Pool config emitted to `<variant>.conf`. SERVER.md §7.3 schema.
#[derive(Debug)]
pub struct PoolConf<'a> {
    /// Path to the listen unix socket.
    pub listen_socket: &'a Path,
    /// Path to the per-variant conf.d directory.
    pub php_ini_scan_dir: &'a Path,
}

impl PoolConf<'_> {
    pub fn render(&self) -> String {
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
             env[PHP_INI_SCAN_DIR] = {scan_dir}\n",
            socket = self.listen_socket.display(),
            scan_dir = self.php_ini_scan_dir.display(),
        )
    }
}

/// Atomically write the pool conf at `path`. Tempfile + rename so a
/// `kill -9` mid-write can't leave the pool with a half-written conf
/// that php-fpm would reject on next spawn.
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
    fn variant_links_every_ini_when_nothing_excluded() {
        let td = TempDir::new().unwrap();
        let source = td.path().join("conf.d");
        let target = td.path().join("normal.confd");
        write_fragment(&source, "20-redis.ini", "extension=redis.so\n");
        write_fragment(&source, "35-pdo_mysql.ini", "extension=pdo_mysql.so\n");

        let linked = build_variant_confd(&target, &source, &[]).unwrap();
        assert_eq!(linked.len(), 2);
        assert!(target.join("20-redis.ini").exists());
        assert!(target.join("35-pdo_mysql.ini").exists());
    }

    #[test]
    fn variant_excludes_xdebug_by_name() {
        let td = TempDir::new().unwrap();
        let source = td.path().join("conf.d");
        let target = td.path().join("normal.confd");
        write_fragment(&source, "20-redis.ini", "extension=redis.so\n");
        write_fragment(&source, "30-xdebug.ini", "zend_extension=xdebug.so\n");

        let linked =
            build_variant_confd(&target, &source, &["xdebug".to_string()]).unwrap();
        let names: Vec<String> = linked
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["20-redis.ini".to_string()]);
        assert!(!target.join("30-xdebug.ini").exists());
    }

    #[test]
    fn variant_ignores_non_ini_files() {
        let td = TempDir::new().unwrap();
        let source = td.path().join("conf.d");
        let target = td.path().join("normal.confd");
        write_fragment(&source, "README.md", "# notes\n");
        write_fragment(&source, "20-redis.ini", "extension=redis.so\n");

        let linked = build_variant_confd(&target, &source, &[]).unwrap();
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

        let _ = build_variant_confd(&target, &source, &[]).unwrap();
        assert!(target.join("20-redis.ini").exists());
        assert!(!target.join("stale.ini").exists());
    }

    #[test]
    fn variant_returns_empty_when_source_missing() {
        let td = TempDir::new().unwrap();
        let source = td.path().join("does-not-exist");
        let target = td.path().join("normal.confd");
        let linked = build_variant_confd(&target, &source, &[]).unwrap();
        assert!(linked.is_empty());
        assert!(target.exists());
    }

    #[test]
    fn extension_name_parses_canonical_form() {
        assert_eq!(extension_name_from_fragment("20-redis.ini"), Some("redis"));
        assert_eq!(extension_name_from_fragment("35-pdo_mysql.ini"), Some("pdo_mysql"));
        assert_eq!(extension_name_from_fragment("40-mysqli.ini"), Some("mysqli"));
    }

    #[test]
    fn extension_name_rejects_malformed() {
        assert!(extension_name_from_fragment("noprefix.ini").is_none());
        assert!(extension_name_from_fragment("abc-redis.ini").is_none());
        assert!(extension_name_from_fragment("20-.ini").is_none());
        assert!(extension_name_from_fragment("20-redis.conf").is_none());
    }

    #[test]
    fn pool_conf_render_matches_schema() {
        let conf = PoolConf {
            listen_socket: Path::new("/run/x/normal.sock"),
            php_ini_scan_dir: Path::new("/run/x/normal.confd"),
        };
        let rendered = conf.render();
        assert!(rendered.contains("listen = /run/x/normal.sock"));
        assert!(rendered.contains("listen.mode = 0600"));
        assert!(rendered.contains("pm = ondemand"));
        assert!(rendered.contains("env[PHP_INI_SCAN_DIR] = /run/x/normal.confd"));
        assert!(rendered.contains("daemonize = no"));
        assert!(rendered.contains("clear_env = no"));
    }

    #[test]
    fn write_pool_conf_round_trip() {
        let td = TempDir::new().unwrap();
        let path = td.path().join("normal.conf");
        let conf = PoolConf {
            listen_socket: Path::new("/run/normal.sock"),
            php_ini_scan_dir: Path::new("/run/normal.confd"),
        };
        write_pool_conf(&path, &conf).unwrap();
        let body = std::fs::read_to_string(&path).unwrap();
        assert!(body.contains("pm = ondemand"));
    }
}
