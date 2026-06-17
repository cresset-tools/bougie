//! `patches.lock.json` — the applied-state store that makes re-application
//! correct.
//!
//! This file is **always written** (not gated behind any opt-in): it records,
//! per package, the fingerprint of the patch set currently applied on disk.
//! The next install compares each package's desired fingerprint against this
//! store and forces a pristine re-extract on any mismatch.
//!
//! The on-disk shape is deliberately small and forward-compatible; the
//! optional v2-style *human* serialization (Phase D, `write_lock`) is layered
//! on top of the same file without disturbing the fingerprint map.

use std::collections::BTreeMap;
use std::path::Path;

use eyre::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// File name, at the project root, alongside `composer.lock`.
pub const LOCK_FILE_NAME: &str = "patches.lock.json";

/// The deserialized `patches.lock.json`. Only the fingerprint map is
/// load-bearing; unknown keys are ignored so a v2-shaped lock round-trips.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PatchLock {
    /// Schema version of bougie's fingerprint block.
    #[serde(rename = "_version", default = "default_version")]
    pub version: u32,
    /// Tool that wrote it.
    #[serde(rename = "_generator", default)]
    pub generator: String,
    /// Package name → applied patch-set fingerprint.
    #[serde(default)]
    pub fingerprints: BTreeMap<String, String>,
    /// Optional v2-shaped human view of the applied patches (`write_lock`),
    /// `target → [ { description, url, sha256?, depth? } ]`. Omitted unless
    /// opted in; bougie never reads it back (the fingerprint map is the
    /// source of truth), it exists for cross-tool inspection.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub patches: Option<Value>,
}

fn default_version() -> u32 {
    1
}

impl PatchLock {
    /// Build a lock from a fingerprint map.
    pub fn from_fingerprints(fingerprints: BTreeMap<String, String>) -> Self {
        Self {
            version: default_version(),
            generator: "bougie".to_string(),
            fingerprints,
            patches: None,
        }
    }
}

/// Read the fingerprint map from `<project_root>/patches.lock.json`. A missing
/// or unparseable file yields an empty map (treated as "nothing applied yet"),
/// so a corrupted lock self-heals by re-extracting + re-patching everything.
pub fn read(project_root: &Path) -> BTreeMap<String, String> {
    let path = project_root.join(LOCK_FILE_NAME);
    let Ok(bytes) = std::fs::read(&path) else {
        return BTreeMap::new();
    };
    match serde_json::from_slice::<PatchLock>(&bytes) {
        Ok(lock) => lock.fingerprints,
        Err(_) => BTreeMap::new(),
    }
}

/// Write the fingerprint map to `<project_root>/patches.lock.json`. Packages
/// with no recorded fingerprint (failed/partial applies) are simply absent.
pub fn write(project_root: &Path, fingerprints: &BTreeMap<String, String>) -> Result<()> {
    write_with_human(project_root, fingerprints, None)
}

/// Like [`write`], but additionally embeds the optional v2-shaped human view
/// of the applied patches (`write_lock` opt-in). `human` is a
/// `target → [entries]` JSON object; pass `None` to omit it.
pub fn write_with_human(
    project_root: &Path,
    fingerprints: &BTreeMap<String, String>,
    human: Option<Value>,
) -> Result<()> {
    let path = project_root.join(LOCK_FILE_NAME);
    let mut lock = PatchLock::from_fingerprints(fingerprints.clone());
    lock.patches = human;
    let mut json = serde_json::to_string_pretty(&lock).wrap_err("serializing patches.lock.json")?;
    json.push('\n');
    std::fs::write(&path, json).wrap_err_with(|| format!("writing {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn round_trip() {
        let dir = tempdir().unwrap();
        let mut fps = BTreeMap::new();
        fps.insert("vendor/a".to_string(), "abc".to_string());
        fps.insert("vendor/b".to_string(), "def".to_string());
        write(dir.path(), &fps).unwrap();
        let back = read(dir.path());
        assert_eq!(back, fps);
    }

    #[test]
    fn missing_file_is_empty() {
        let dir = tempdir().unwrap();
        assert!(read(dir.path()).is_empty());
    }

    #[test]
    fn corrupted_file_is_empty() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join(LOCK_FILE_NAME), "{ not json").unwrap();
        assert!(read(dir.path()).is_empty());
    }

    #[test]
    fn human_view_is_embedded_but_not_read_back() {
        let dir = tempdir().unwrap();
        let mut fps = BTreeMap::new();
        fps.insert("vendor/a".to_string(), "abc".to_string());
        let human = serde_json::json!({ "vendor/a": [ { "description": "d", "url": "u" } ] });
        write_with_human(dir.path(), &fps, Some(human)).unwrap();

        let raw = std::fs::read_to_string(dir.path().join(LOCK_FILE_NAME)).unwrap();
        assert!(raw.contains("\"patches\""), "human view embedded: {raw}");
        // read() still returns only the fingerprint map (source of truth).
        assert_eq!(read(dir.path()).get("vendor/a").map(String::as_str), Some("abc"));
    }

    #[test]
    fn write_omits_human_view_by_default() {
        let dir = tempdir().unwrap();
        let mut fps = BTreeMap::new();
        fps.insert("vendor/a".to_string(), "abc".to_string());
        write(dir.path(), &fps).unwrap();
        let raw = std::fs::read_to_string(dir.path().join(LOCK_FILE_NAME)).unwrap();
        assert!(!raw.contains("\"patches\""), "no human view unless opted in: {raw}");
    }

    #[test]
    fn unknown_keys_round_trip_fingerprints() {
        let dir = tempdir().unwrap();
        std::fs::write(
            dir.path().join(LOCK_FILE_NAME),
            r#"{"_version":1,"patches":{"vendor/x":[]},"fingerprints":{"vendor/x":"99"}}"#,
        )
        .unwrap();
        let back = read(dir.path());
        assert_eq!(back.get("vendor/x").map(String::as_str), Some("99"));
    }
}
