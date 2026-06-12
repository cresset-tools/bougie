//! Per-project `.bougie/state/resolved` and the global
//! `$BOUGIE_HOME/state/state.json` (CLI.md §2.1, §3.6.2).

use bougie_paths::Paths;
use bougie_version::request::Flavor;
use bougie_version::version::Version;
use eyre::{eyre, Result, WrapErr};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

/// Atomically write the per-project resolved-version marker to
/// `<project>/.bougie/state/resolved`. Format: `"<version>-<flavor>"`.
pub fn write_project_resolved(project_root: &Path, version: Version, flavor: Flavor) -> Result<PathBuf> {
    let dir = project_root.join(".bougie").join("state");
    fs::create_dir_all(&dir).wrap_err_with(|| format!("creating {}", dir.display()))?;
    let dest = dir.join("resolved");
    let tmp = dir.join("resolved.partial");
    fs::write(&tmp, format!("{version}-{flavor}\n"))
        .wrap_err_with(|| format!("writing {}", tmp.display()))?;
    fs::rename(&tmp, &dest)
        .wrap_err_with(|| format!("rename {} → {}", tmp.display(), dest.display()))?;
    Ok(dest)
}

/// Read the resolved-version marker. Returns `(version, flavor)` strings
/// (caller parses) so the shim can call this without pulling the full
/// version + Request modules into its hot path.
pub fn read_project_resolved(project_root: &Path) -> Result<(String, String)> {
    let path = project_root.join(".bougie").join("state").join("resolved");
    let body = fs::read_to_string(&path)
        .wrap_err_with(|| format!("reading {}", path.display()))?;
    let line = body.trim();
    // Split on the FIRST '-' so "8.3.12-zts-debug" splits correctly.
    let idx = line
        .find('-')
        .ok_or_else(|| eyre!("malformed resolved marker: {line:?}"))?;
    let (v, rest) = line.split_at(idx);
    Ok((v.to_owned(), rest[1..].to_owned()))
}

/// Atomically write `<project>/.bougie/state/resolved-php-path` holding
/// the absolute path to a **system** PHP binary. Written only when sync
/// selects a system PHP; managed projects never have this file, so its
/// presence is the signal "this project uses a system PHP".
pub fn write_project_resolved_php_path(project_root: &Path, php: &Path) -> Result<PathBuf> {
    let dir = project_root.join(".bougie").join("state");
    fs::create_dir_all(&dir).wrap_err_with(|| format!("creating {}", dir.display()))?;
    let dest = dir.join("resolved-php-path");
    let tmp = dir.join("resolved-php-path.partial");
    fs::write(&tmp, format!("{}\n", php.display()))
        .wrap_err_with(|| format!("writing {}", tmp.display()))?;
    fs::rename(&tmp, &dest)
        .wrap_err_with(|| format!("rename {} → {}", tmp.display(), dest.display()))?;
    Ok(dest)
}

/// Read the system-PHP path marker, if present. Returns `None` (not an
/// error) when the file is absent — the common managed-PHP case.
pub fn read_project_resolved_php_path(project_root: &Path) -> Option<PathBuf> {
    let path = project_root.join(".bougie").join("state").join("resolved-php-path");
    let body = fs::read_to_string(&path).ok()?;
    let line = body.trim();
    if line.is_empty() {
        return None;
    }
    Some(PathBuf::from(line))
}

/// Locate the `php-fpm` belonging to a **system** PHP, given the path to
/// its `php` binary (the value stored in `resolved-php-path`). Distros
/// disagree on where fpm lands: Debian/Ubuntu and most package managers
/// drop it next to `php` in `bin/`, while a stock `./configure --prefix`
/// build — and Homebrew — install it in the sibling `sbin/`. Probe both,
/// `bin/` first, and return the first that exists. `None` means this PHP
/// has no fpm SAPI alongside it (CLI-only build).
///
/// Shared by the argv[0] `php-fpm` shim and the dev server so both agree
/// on where a system fpm lives.
pub fn system_fpm_for_php(system_php: &Path) -> Option<PathBuf> {
    #[cfg(windows)]
    let name = "php-fpm.exe";
    #[cfg(not(windows))]
    let name = "php-fpm";

    let bin_dir = system_php.parent()?;
    let sibling = bin_dir.join(name);
    if sibling.exists() {
        return Some(sibling);
    }
    let sbin = bin_dir.parent()?.join("sbin").join(name);
    sbin.exists().then_some(sbin)
}

/// Remove the system-PHP path marker if present (idempotent). Called
/// when a project switches from a system PHP back to a managed one.
pub fn clear_project_resolved_php_path(project_root: &Path) -> Result<()> {
    let path = project_root.join(".bougie").join("state").join("resolved-php-path");
    match fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).wrap_err_with(|| format!("removing {}", path.display())),
    }
}

/// `$BOUGIE_HOME/state/state.json` shape.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct GlobalState {
    pub schema_version: u32,
    pub host_target: Option<String>,
    pub projects: Vec<PathBuf>,
}

impl GlobalState {
    pub fn load(paths: &Paths) -> Result<Self> {
        let path = paths.state_json();
        if !path.exists() {
            return Ok(Self { schema_version: 1, ..Default::default() });
        }
        let bytes = fs::read(&path).wrap_err_with(|| format!("reading {}", path.display()))?;
        let mut s: Self = serde_json::from_slice(&bytes)
            .wrap_err_with(|| format!("parsing {}", path.display()))?;
        if s.schema_version == 0 {
            s.schema_version = 1;
        }
        Ok(s)
    }

    pub fn save(&self, paths: &Paths) -> Result<()> {
        let path = paths.state_json();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).wrap_err_with(|| format!("creating {}", parent.display()))?;
        }
        let tmp = path.with_extension("json.partial");
        let bytes = serde_json::to_vec_pretty(self).wrap_err("encoding state.json")?;
        fs::write(&tmp, &bytes).wrap_err_with(|| format!("writing {}", tmp.display()))?;
        fs::rename(&tmp, &path)
            .wrap_err_with(|| format!("rename {} → {}", tmp.display(), path.display()))?;
        Ok(())
    }

    /// Add a project root if not already present.
    pub fn touch_project(&mut self, root: &Path) {
        let canon = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
        if !self.projects.iter().any(|p| p == &canon) {
            self.projects.push(canon);
            self.projects.sort();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn round_trip_resolved() {
        let proj = TempDir::new().unwrap();
        let p =
            write_project_resolved(proj.path(), Version::new(8, 3, 12), Flavor::Nts).unwrap();
        assert!(p.exists());
        let (v, f) = read_project_resolved(proj.path()).unwrap();
        assert_eq!(v, "8.3.12");
        assert_eq!(f, "nts");
    }

    #[test]
    fn resolved_handles_multipart_flavor() {
        let proj = TempDir::new().unwrap();
        write_project_resolved(proj.path(), Version::new(8, 3, 12), Flavor::ZtsDebug).unwrap();
        let (v, f) = read_project_resolved(proj.path()).unwrap();
        assert_eq!(v, "8.3.12");
        assert_eq!(f, "zts-debug");
    }

    #[test]
    fn resolved_php_path_round_trip() {
        let proj = TempDir::new().unwrap();
        // Absent → None, clear is a no-op.
        assert!(read_project_resolved_php_path(proj.path()).is_none());
        clear_project_resolved_php_path(proj.path()).unwrap();

        let bin = Path::new("/usr/bin/php");
        write_project_resolved_php_path(proj.path(), bin).unwrap();
        assert_eq!(
            read_project_resolved_php_path(proj.path()).as_deref(),
            Some(bin)
        );

        clear_project_resolved_php_path(proj.path()).unwrap();
        assert!(read_project_resolved_php_path(proj.path()).is_none());
    }

    #[cfg(not(windows))]
    #[test]
    fn system_fpm_prefers_bin_then_sbin() {
        let td = TempDir::new().unwrap();
        let bin = td.path().join("bin");
        let sbin = td.path().join("sbin");
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::create_dir_all(&sbin).unwrap();
        let php = bin.join("php");
        std::fs::write(&php, "").unwrap();

        // No fpm anywhere → None.
        assert!(system_fpm_for_php(&php).is_none());

        // Homebrew / stock `--prefix` layout: fpm in the sibling sbin/.
        let sbin_fpm = sbin.join("php-fpm");
        std::fs::write(&sbin_fpm, "").unwrap();
        assert_eq!(system_fpm_for_php(&php).as_deref(), Some(sbin_fpm.as_path()));

        // bin/php-fpm (Debian-style) wins when both exist.
        let bin_fpm = bin.join("php-fpm");
        std::fs::write(&bin_fpm, "").unwrap();
        assert_eq!(system_fpm_for_php(&php).as_deref(), Some(bin_fpm.as_path()));
    }

    #[test]
    fn global_state_round_trip() {
        let dir = TempDir::new().unwrap();
        let paths = Paths::new(dir.path().to_path_buf(), dir.path().to_path_buf());
        let mut s = GlobalState::load(&paths).unwrap();
        assert_eq!(s.schema_version, 1);
        s.host_target = Some("x86_64-unknown-linux-gnu".into());
        s.touch_project(Path::new("/tmp"));
        s.touch_project(Path::new("/tmp")); // dedupe
        s.save(&paths).unwrap();

        let loaded = GlobalState::load(&paths).unwrap();
        assert_eq!(loaded.host_target.as_deref(), Some("x86_64-unknown-linux-gnu"));
        assert_eq!(loaded.projects.len(), 1);
    }
}
