//! Helpers for the on-disk install + store layout (CLI.md §2.1).

use bougie_paths::Paths;
use bougie_version::request::Flavor;
use bougie_version::version::Version;
use std::path::PathBuf;

/// `$BOUGIE_HOME/installs/<version>-<flavor>/`.
pub fn install_dir(paths: &Paths, version: Version, flavor: Flavor) -> PathBuf {
    paths.installs().join(format!("{version}-{flavor}"))
}

/// `$BOUGIE_HOME/store/<name>-<version>-<hash>/`.
pub fn store_dir(paths: &Paths, name: &str, version: &str, hash: &str) -> PathBuf {
    paths.store().join(format!("{name}-{version}-{hash}"))
}

/// List installed `(version, flavor)` pairs by scanning `installs/`.
pub fn list_installed(paths: &Paths) -> std::io::Result<Vec<(String, String)>> {
    let dir = paths.installs();
    let mut out = Vec::new();
    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(out),
        Err(e) => return Err(e),
    };
    for entry in entries.flatten() {
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        // Split on last '-' so "8.3.12-nts-debug" becomes
        // ("8.3.12", "nts-debug"). Iterate possible flavor strings.
        for flavor in ["zts-debug", "nts-debug", "zts", "nts"] {
            if let Some(version) = name.strip_suffix(&format!("-{flavor}")) {
                out.push((version.into(), flavor.into()));
                break;
            }
        }
    }
    out.sort();
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn paths() -> Paths {
        Paths::new(PathBuf::from("/h"), PathBuf::from("/c"))
    }

    #[test]
    fn install_dir_is_versioned() {
        let p = install_dir(&paths(), Version::new(8, 3, 12), Flavor::Nts);
        assert_eq!(p, Path::new("/h/installs/8.3.12-nts"));
    }

    #[test]
    fn install_dir_with_zts_debug() {
        let p = install_dir(&paths(), Version::new(8, 3, 12), Flavor::ZtsDebug);
        assert_eq!(p, Path::new("/h/installs/8.3.12-zts-debug"));
    }

    #[test]
    fn store_dir_concatenates_components() {
        let p = store_dir(&paths(), "libffi", "3.4.6", "abcdef01");
        assert_eq!(p, Path::new("/h/store/libffi-3.4.6-abcdef01"));
    }

    #[test]
    fn list_installed_handles_missing_dir() {
        let dir = tempfile::TempDir::new().unwrap();
        let p = Paths::new(dir.path().to_path_buf(), PathBuf::from("/c"));
        // installs/ doesn't exist yet
        let installed = list_installed(&p).unwrap();
        assert!(installed.is_empty());
    }

    #[test]
    fn list_installed_parses_names() {
        let dir = tempfile::TempDir::new().unwrap();
        let p = Paths::new(dir.path().to_path_buf(), PathBuf::from("/c"));
        std::fs::create_dir_all(p.installs().join("8.3.12-nts")).unwrap();
        std::fs::create_dir_all(p.installs().join("8.4.0-zts")).unwrap();
        std::fs::create_dir_all(p.installs().join("8.3.12-zts-debug")).unwrap();
        std::fs::create_dir_all(p.installs().join("not-a-version")).unwrap();
        let mut installed = list_installed(&p).unwrap();
        installed.sort();
        assert_eq!(
            installed,
            vec![
                ("8.3.12".into(), "nts".into()),
                ("8.3.12".into(), "zts-debug".into()),
                ("8.4.0".into(), "zts".into()),
            ]
        );
    }
}
