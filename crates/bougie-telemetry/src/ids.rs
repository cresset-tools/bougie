//! The anonymous install id.
//!
//! A random `UUIDv4`, minted only once consent exists (`telemetry on`,
//! the consent prompts, or an explicit `BOUGIE_TELEMETRY=on`) and
//! stored next to the mode file. `bougie telemetry reset` rotates it.
//! Nothing about it derives from the machine — no MAC, no hostname,
//! no fingerprinting.

use std::fs;
use std::path::{Path, PathBuf};

/// Placeholder recorded on events spooled before an id exists (e.g. in
/// `local` mode, which deliberately mints nothing persistent).
pub const UNSET: &str = "unset";

pub fn install_id_path(config_dir: &Path) -> PathBuf {
    config_dir.join("install-id")
}

/// Read the stored install id, if present and plausible.
pub fn read(config_dir: &Path) -> Option<String> {
    let raw = fs::read_to_string(install_id_path(config_dir)).ok()?;
    let id = raw.trim();
    // 36 chars of hex + hyphens; reject anything mangled rather than
    // uploading garbage.
    let plausible = id.len() == 36
        && id.chars().all(|c| c.is_ascii_hexdigit() || c == '-');
    plausible.then(|| id.to_owned())
}

/// Mint (or overwrite with) a fresh install id. Returns `None` on any
/// I/O failure — callers degrade to [`UNSET`].
pub fn mint(config_dir: &Path) -> Option<String> {
    let id = uuid::Uuid::new_v4().to_string();
    fs::create_dir_all(config_dir).ok()?;
    fs::write(install_id_path(config_dir), format!("{id}\n")).ok()?;
    Some(id)
}

/// Existing id, or mint one. Used when mode is `on`.
pub fn read_or_mint(config_dir: &Path) -> Option<String> {
    read(config_dir).or_else(|| mint(config_dir))
}

/// Delete the stored id (part of `telemetry reset`).
pub fn remove(config_dir: &Path) {
    let _ = fs::remove_file(install_id_path(config_dir));
}

/// A fresh per-invocation id (never stored).
pub fn invocation_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn mint_read_rotate() {
        let tmp = TempDir::new().unwrap();
        assert_eq!(read(tmp.path()), None);
        let a = read_or_mint(tmp.path()).unwrap();
        assert_eq!(read(tmp.path()).as_deref(), Some(a.as_str()));
        let b = mint(tmp.path()).unwrap();
        assert_ne!(a, b, "mint overwrites with a fresh id");
        remove(tmp.path());
        assert_eq!(read(tmp.path()), None);
    }

    #[test]
    fn mangled_id_rejected() {
        let tmp = TempDir::new().unwrap();
        fs::write(install_id_path(tmp.path()), "not-a-uuid\n").unwrap();
        assert_eq!(read(tmp.path()), None);
    }
}
