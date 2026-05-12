//! composer.json / composer.lock IO and editing primitives.
//!
//! Bougie touches both files when adding or removing an extension —
//! without invoking `composer require`, which would re-resolve the full
//! dependency graph and run platform checks against a PHP that hasn't
//! yet loaded the new ext. Doing the edits ourselves means we can:
//!
//! 1. Install the `.so` and enable it in `.bougie/conf.d/` *first*.
//! 2. Add the `require.ext-<name>` line to composer.json directly.
//! 3. Mirror it under `platform.ext-<name>` in composer.lock and
//!    recompute `content-hash`.
//!
//! Step 3 is what this module exists for. `composer install` accepts
//! the result without complaint; no `composer update` involved.
//!
//! See `Composer\Package\Locker::getContentHash` for the algorithm
//! (`src/Composer/Package/Locker.php:89` in composer/composer).

use crate::composer::php_json::{self, Mode};
use eyre::{eyre, Result, WrapErr};
use md5::{Digest, Md5};
use serde_json::{Map, Value};
use std::io::Write;
use std::path::{Path, PathBuf};

/// Keys that participate in Composer's content-hash, in the order
/// PHP's `array_intersect($relevantKeys, array_keys($content))` would
/// produce. Order doesn't actually affect the hash (we `ksort` before
/// encoding) but mirroring composer's source is documentation.
const RELEVANT_KEYS: &[&str] = &[
    "name",
    "version",
    "require",
    "require-dev",
    "conflict",
    "replace",
    "provide",
    "minimum-stability",
    "prefer-stable",
    "repositories",
    "extra",
];

/// Compute Composer's `content-hash` for a composer.json byte stream.
///
/// Algorithm (verbatim from `Locker::getContentHash`):
///
/// 1. JSON-decode the composer.json bytes.
/// 2. Pick the [`RELEVANT_KEYS`] subset plus `config.platform` if
///    present. Nothing else under `config` participates.
/// 3. `ksort` the resulting top-level keys alphabetically.
/// 4. PHP `json_encode(..., 0)` — see [`php_json::Mode::Hash`].
/// 5. MD5 hex.
pub fn content_hash(composer_json_bytes: &[u8]) -> Result<String> {
    let parsed: Value = serde_json::from_slice(composer_json_bytes)
        .map_err(|e| eyre!("composer.json is not valid JSON: {e}"))?;
    let obj = parsed
        .as_object()
        .ok_or_else(|| eyre!("composer.json top level must be a JSON object"))?;

    let mut relevant: Map<String, Value> = Map::new();
    for key in RELEVANT_KEYS {
        if let Some(v) = obj.get(*key) {
            relevant.insert((*key).to_string(), v.clone());
        }
    }
    if let Some(platform) = obj
        .get("config")
        .and_then(Value::as_object)
        .and_then(|c| c.get("platform"))
    {
        let mut config_subset = Map::new();
        config_subset.insert("platform".to_string(), platform.clone());
        relevant.insert("config".to_string(), Value::Object(config_subset));
    }

    sort_top_level(&mut relevant);

    let bytes = php_json::encode(&Value::Object(relevant), Mode::Hash);
    let mut hasher = Md5::new();
    hasher.update(&bytes);
    Ok(hex_lower(&hasher.finalize()))
}

/// In-place ksort of an object's top-level keys (lexicographic on bytes,
/// matching PHP's default `ksort` for string keys). Nested objects keep
/// their own order — Composer's algorithm only sorts the top level.
fn sort_top_level(m: &mut Map<String, Value>) {
    let mut keys: Vec<String> = m.keys().cloned().collect();
    keys.sort();
    // serde_json::Map (with preserve_order) is backed by IndexMap, which
    // doesn't expose sort_keys without a feature; rebuild in order.
    let mut rebuilt: Map<String, Value> = Map::new();
    for k in keys {
        // unwrap: k came from m.keys() above.
        let v = m.shift_remove(&k).unwrap();
        rebuilt.insert(k, v);
    }
    *m = rebuilt;
}

fn hex_lower(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(HEX[((b >> 4) & 0xf) as usize] as char);
        out.push(HEX[(b & 0xf) as usize] as char);
    }
    out
}

/// Read a JSON file from disk and parse as `serde_json::Value`.
/// Preserves object key order (`serde_json::preserve_order` feature)
/// so subsequent re-serialisation mirrors the source layout.
pub fn read_json_file(path: &Path) -> Result<Value> {
    let bytes = std::fs::read(path)
        .wrap_err_with(|| format!("reading {}", path.display()))?;
    serde_json::from_slice(&bytes)
        .map_err(|e| eyre!("parsing {}: {e}", path.display()))
}

/// Write a JSON value to disk in the same format Composer's
/// `JsonFile::encode` produces — 4-space indent, raw `/`, raw UTF-8
/// except U+2028 / U+2029, plus a trailing newline — and atomically
/// via tempfile-then-rename so a concurrent `composer install` never
/// sees a half-written file.
pub fn write_json_file(path: &Path, value: &Value) -> Result<()> {
    write_json_bytes(path, &encode_for_disk(value))
}

/// Composer's on-disk JSON encoding: `Mode::Pretty` + trailing newline.
/// Exposed for callers that need the byte stream itself — e.g. computing
/// `content_hash` from the exact bytes about to be written.
pub fn encode_for_disk(value: &Value) -> Vec<u8> {
    let mut bytes = php_json::encode(value, Mode::Pretty);
    bytes.push(b'\n');
    bytes
}

/// Atomic write: tempfile in the destination directory, `fsync`,
/// rename onto the target. Same-filesystem rename guarantees atomicity
/// on POSIX; concurrent readers see either the old file or the new,
/// never a torn read.
fn write_json_bytes(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path.parent().ok_or_else(|| {
        eyre!("path {} has no parent directory", path.display())
    })?;
    let dir = if parent.as_os_str().is_empty() {
        Path::new(".")
    } else {
        std::fs::create_dir_all(parent)
            .wrap_err_with(|| format!("creating {}", parent.display()))?;
        parent
    };
    let mut tf = tempfile::NamedTempFile::new_in(dir)
        .wrap_err_with(|| format!("creating tempfile in {}", dir.display()))?;
    tf.as_file_mut()
        .write_all(bytes)
        .wrap_err_with(|| format!("writing {}", tf.path().display()))?;
    tf.as_file_mut()
        .sync_all()
        .wrap_err_with(|| format!("fsyncing {}", tf.path().display()))?;
    tf.persist(path)
        .map_err(|e| eyre!("renaming temp to {}: {e}", path.display()))?;
    Ok(())
}

/// `composer require ext-<name>` semantics, but as a pure JSON edit.
/// Appends to the existing `require` (or `require-dev` if `dev`) map,
/// or creates the map if absent. Re-inserting an existing key updates
/// its constraint in place, preserving position — same as composer.
pub fn require_add(
    composer_json: &mut Value,
    key: &str,
    constraint: &str,
    dev: bool,
) -> Result<()> {
    let obj = composer_json
        .as_object_mut()
        .ok_or_else(|| eyre!("composer.json top level must be a JSON object"))?;
    let map_key = if dev { "require-dev" } else { "require" };
    let entry = obj
        .entry(map_key.to_string())
        .or_insert_with(|| Value::Object(Map::new()));
    let map = entry
        .as_object_mut()
        .ok_or_else(|| eyre!("composer.json `{map_key}` exists but is not an object"))?;
    map.insert(key.to_string(), Value::String(constraint.to_string()));
    Ok(())
}

/// If `config.sort-packages` is `true`, reorder `require` and
/// `require-dev` exactly like `composer require` would: a prefix-based
/// grouping matching `Composer\Json\JsonManipulator::sortPackages`.
///
/// The groups, in ascending order:
///
/// 1. `php` family (`php`, `php-64bit`, `php-ipv6`, `php-zts`, `php-debug`)
/// 2. `hhvm`
/// 3. `ext-*`
/// 4. `lib-*`
/// 5. Other platform-style names (no `/`, not in groups 1-4)
/// 6. Regular `vendor/package`
///
/// Within each group, names compare lexicographically. Composer uses
/// PHP's `strnatcmp` for the inner comparison; we use `str::cmp`,
/// which only diverges when names contain numeric runs whose digit
/// counts differ (`pkg-2` vs `pkg-10`). Real composer.json files
/// rarely have such names, and the divergence is purely cosmetic —
/// the content-hash is computed from the post-sort bytes either way.
pub fn sort_packages_if_configured(composer_json: &mut Value) -> Result<()> {
    let Some(obj) = composer_json.as_object_mut() else {
        return Err(eyre!("composer.json top level must be a JSON object"));
    };
    let enabled = obj
        .get("config")
        .and_then(Value::as_object)
        .and_then(|c| c.get("sort-packages"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if !enabled {
        return Ok(());
    }
    for map_key in ["require", "require-dev"] {
        if let Some(entry) = obj.get_mut(map_key)
            && let Some(map) = entry.as_object_mut()
        {
            sort_require_map(map);
        }
    }
    Ok(())
}

fn sort_require_map(m: &mut Map<String, Value>) {
    let mut keys: Vec<String> = m.keys().cloned().collect();
    keys.sort_by_key(|k| sort_key(k));
    let mut rebuilt: Map<String, Value> = Map::new();
    for k in keys {
        let v = m.shift_remove(&k).expect("key came from m.keys()");
        rebuilt.insert(k, v);
    }
    *m = rebuilt;
}

/// Compute composer's prefix-then-name sort key. Matches the
/// `preg_replace` chain in `JsonManipulator::sortPackages`.
fn sort_key(name: &str) -> String {
    if name.starts_with("php") && !name.contains('/') {
        return format!("0-{name}");
    }
    if name == "hhvm" {
        return format!("1-{name}");
    }
    if name.starts_with("ext-") {
        return format!("2-{name}");
    }
    if name.starts_with("lib-") {
        return format!("3-{name}");
    }
    if !name.contains('/') && !name.chars().next().is_some_and(|c| c.is_ascii_digit()) {
        // Other platform-style names (no slash, non-digit start) get
        // bucket 4 — mirrors composer's `/^\D/` fallback inside the
        // platform-matched branch.
        return format!("4-{name}");
    }
    format!("5-{name}")
}

/// Inverse of [`require_add`]. Returns `Ok(true)` if the key was
/// removed, `Ok(false)` if it wasn't present.
pub fn require_remove(composer_json: &mut Value, key: &str, dev: bool) -> Result<bool> {
    let obj = composer_json
        .as_object_mut()
        .ok_or_else(|| eyre!("composer.json top level must be a JSON object"))?;
    let map_key = if dev { "require-dev" } else { "require" };
    let Some(entry) = obj.get_mut(map_key) else {
        return Ok(false);
    };
    let Some(map) = entry.as_object_mut() else {
        return Err(eyre!("composer.json `{map_key}` exists but is not an object"));
    };
    Ok(map.shift_remove(key).is_some())
}

/// Mirror a `require[-dev]` entry in `composer.lock`'s top-level
/// `platform` / `platform-dev` map. Composer writes this when running
/// `composer require`; replicating it keeps the lockfile in the shape
/// `composer install` expects.
pub fn lock_set_platform(
    lock: &mut Value,
    key: &str,
    constraint: &str,
    dev: bool,
) -> Result<()> {
    let obj = lock
        .as_object_mut()
        .ok_or_else(|| eyre!("composer.lock top level must be a JSON object"))?;
    let map_key = if dev { "platform-dev" } else { "platform" };
    let entry = obj
        .entry(map_key.to_string())
        .or_insert_with(|| Value::Object(Map::new()));
    // Composer writes an empty platform as `[]` (PHP empty-array form).
    // If we encounter that shape, replace it with an object before
    // inserting so the type is consistent post-edit.
    if entry.is_array() {
        *entry = Value::Object(Map::new());
    }
    let map = entry
        .as_object_mut()
        .ok_or_else(|| eyre!("composer.lock `{map_key}` exists but is not an object"))?;
    map.insert(key.to_string(), Value::String(constraint.to_string()));
    Ok(())
}

/// Inverse of [`lock_set_platform`].
pub fn lock_unset_platform(lock: &mut Value, key: &str, dev: bool) -> Result<bool> {
    let obj = lock
        .as_object_mut()
        .ok_or_else(|| eyre!("composer.lock top level must be a JSON object"))?;
    let map_key = if dev { "platform-dev" } else { "platform" };
    let Some(entry) = obj.get_mut(map_key) else {
        return Ok(false);
    };
    let Some(map) = entry.as_object_mut() else {
        // `[]` form: nothing to remove.
        return Ok(false);
    };
    Ok(map.shift_remove(key).is_some())
}

/// Update the top-level `content-hash` field. Creates it if absent —
/// older composer.lock files (pre-1.0) didn't have one, but every
/// current lockfile does, so absence is exceptional.
pub fn lock_set_content_hash(lock: &mut Value, hash: &str) -> Result<()> {
    let obj = lock
        .as_object_mut()
        .ok_or_else(|| eyre!("composer.lock top level must be a JSON object"))?;
    obj.insert("content-hash".to_string(), Value::String(hash.to_string()));
    Ok(())
}

/// What [`apply_require_change`] should do.
#[derive(Debug, Clone)]
pub enum RequireChange {
    /// `composer require <key>:<constraint>` (or `--dev`).
    Add {
        key: String,
        constraint: String,
        dev: bool,
    },
    /// `composer remove <key>` (or `--dev`).
    Remove { key: String, dev: bool },
}

/// Result of [`apply_require_change`]. The new `content-hash` is
/// returned so the caller can surface it in `--format json` output
/// without re-reading the lockfile.
#[derive(Debug, Clone)]
pub struct RequireApplied {
    pub composer_json_path: PathBuf,
    pub composer_lock_path: Option<PathBuf>,
    pub new_content_hash: String,
    pub change_applied: bool,
}

/// Drive the end-to-end edit: load composer.json, apply the change,
/// recompute the hash from the post-edit bytes, write composer.json
/// back, and — if composer.lock exists — mirror the require to its
/// `platform` map and splice in the new content-hash.
///
/// Idempotent: `Add` of an already-present key updates the constraint
/// (composer's behaviour); `Remove` of an absent key is a no-op with
/// `change_applied = false`.
pub fn apply_require_change(
    project_root: &Path,
    change: &RequireChange,
) -> Result<RequireApplied> {
    let composer_json_path = project_root.join("composer.json");
    let composer_lock_path = project_root.join("composer.lock");

    let mut composer_json = read_json_file(&composer_json_path)?;
    let change_applied = match change {
        RequireChange::Add { key, constraint, dev } => {
            require_add(&mut composer_json, key, constraint, *dev)?;
            true
        }
        RequireChange::Remove { key, dev } => {
            require_remove(&mut composer_json, key, *dev)?
        }
    };
    // Honor `config.sort-packages`: applied after the edit so the new
    // entry lands in the same position composer would have placed it.
    // Idempotent when the flag is off.
    sort_packages_if_configured(&mut composer_json)?;

    // Re-encode and recompute the hash from the *post-edit* bytes —
    // this is what composer would itself hash if it re-read the file
    // we're about to write.
    let written_bytes = encode_for_disk(&composer_json);
    let new_content_hash = content_hash(&written_bytes)?;
    write_json_bytes(&composer_json_path, &written_bytes)?;

    let lock_updated = if composer_lock_path.exists() {
        let mut lock = read_json_file(&composer_lock_path)?;
        match change {
            RequireChange::Add { key, constraint, dev } => {
                lock_set_platform(&mut lock, key, constraint, *dev)?;
            }
            RequireChange::Remove { key, dev } => {
                lock_unset_platform(&mut lock, key, *dev)?;
            }
        }
        lock_set_content_hash(&mut lock, &new_content_hash)?;
        write_json_file(&composer_lock_path, &lock)?;
        true
    } else {
        false
    };

    Ok(RequireApplied {
        composer_json_path,
        composer_lock_path: lock_updated.then_some(composer_lock_path),
        new_content_hash,
        change_applied,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Fixture composer.json + its content-hash, both generated by
    /// running Composer's actual `Locker::getContentHash` algorithm
    /// against PHP 8.5.6. If this test ever drifts, the algorithm has
    /// changed upstream — re-run the oracle generator (see commit
    /// message for the one-liner).
    const FIXTURE_COMPOSER_JSON: &str = r#"{
    "name": "acme/widget-tool",
    "description": "An example application for testing.",
    "type": "project",
    "license": "MIT",
    "require": {
        "php": "^8.3",
        "monolog/monolog": "^3.5",
        "ext-redis": "*"
    },
    "require-dev": {
        "phpunit/phpunit": "^10.5"
    },
    "minimum-stability": "stable",
    "prefer-stable": true,
    "config": {
        "sort-packages": true,
        "platform": {
            "php": "8.3.12"
        }
    },
    "extra": {
        "branch-alias": {
            "dev-main": "1.0.x-dev"
        }
    },
    "authors": [
        {"name": "Alice", "email": "alice@example.com"}
    ]
}
"#;
    const FIXTURE_EXPECTED_HASH: &str = "9b37bf1b84c6c80e4dae34a4a6a8c18d";
    const FIXTURE_EXPECTED_ENCODED: &str = concat!(
        r#"{"config":{"platform":{"php":"8.3.12"}},"#,
        r#""extra":{"branch-alias":{"dev-main":"1.0.x-dev"}},"#,
        r#""minimum-stability":"stable","#,
        r#""name":"acme\/widget-tool","#,
        r#""prefer-stable":true,"#,
        r#""require":{"php":"^8.3","monolog\/monolog":"^3.5","ext-redis":"*"},"#,
        r#""require-dev":{"phpunit\/phpunit":"^10.5"}}"#,
    );

    #[test]
    fn fixture_hash_matches_real_php() {
        let actual = content_hash(FIXTURE_COMPOSER_JSON.as_bytes()).unwrap();
        assert_eq!(actual, FIXTURE_EXPECTED_HASH);
    }

    #[test]
    fn fixture_encoded_bytes_match_real_php() {
        // The hash is downstream of the encode; if this asserts succeeds
        // and the hash differs, the bug is in MD5 / hex (vanishingly
        // unlikely). If THIS fails, the encoder is wrong — surface
        // exactly which bytes diverged.
        let parsed: Value = serde_json::from_str(FIXTURE_COMPOSER_JSON).unwrap();
        let obj = parsed.as_object().unwrap();
        let mut relevant: Map<String, Value> = Map::new();
        for key in RELEVANT_KEYS {
            if let Some(v) = obj.get(*key) {
                relevant.insert((*key).to_string(), v.clone());
            }
        }
        if let Some(platform) = obj
            .get("config")
            .and_then(Value::as_object)
            .and_then(|c| c.get("platform"))
        {
            let mut config_subset = Map::new();
            config_subset.insert("platform".to_string(), platform.clone());
            relevant.insert("config".to_string(), Value::Object(config_subset));
        }
        sort_top_level(&mut relevant);
        let bytes = php_json::encode(&Value::Object(relevant), Mode::Hash);
        assert_eq!(String::from_utf8(bytes).unwrap(), FIXTURE_EXPECTED_ENCODED);
    }

    /// PHP-generated oracle for a composer.json containing non-ASCII
    /// BMP characters (`café/résumé`) — exercises the `\uXXXX`
    /// escape path under `Mode::Hash`.
    #[test]
    fn unicode_bmp_fixture_hash_matches_real_php() {
        let composer_json = serde_json::json!({
            "name": "café/résumé",
            "description": "Test 💩 with U+1F4A9",
            "require": {"php": "^8.3"},
        });
        let bytes = serde_json::to_vec(&composer_json).unwrap();
        // PHP-generated reference (composer.json above → flags=0 hash bytes)
        let expected = "4744162acf486d68ae8e72ecca67f4ab";
        assert_eq!(content_hash(&bytes).unwrap(), expected);
    }

    #[test]
    fn missing_relevant_keys_simply_omitted() {
        // A composer.json with none of the relevant keys hashes a `{}`.
        let bytes = br#"{"authors": [], "description": "x"}"#;
        let h = content_hash(bytes).unwrap();
        // md5("{}") confirms we don't accidentally pull in non-relevant
        // fields (`authors`, `description` etc. are not in RELEVANT_KEYS).
        assert_eq!(h, "99914b932bd37a50b983c5e7c90ae93b");
    }

    #[test]
    fn config_keys_other_than_platform_are_ignored() {
        // Only config.platform participates. config.sort-packages etc
        // must not affect the hash, otherwise editing local user prefs
        // would invalidate the lockfile.
        let base = br#"{"name":"a/b"}"#;
        let with_config = br#"{"name":"a/b","config":{"sort-packages":true,"optimize-autoloader":false}}"#;
        assert_eq!(
            content_hash(base).unwrap(),
            content_hash(with_config).unwrap()
        );
    }

    #[test]
    fn config_platform_participates() {
        let without = br#"{"name":"a/b"}"#;
        let with = br#"{"name":"a/b","config":{"platform":{"php":"8.3"}}}"#;
        assert_ne!(
            content_hash(without).unwrap(),
            content_hash(with).unwrap()
        );
    }

    #[test]
    fn rejects_non_object_top_level() {
        let err = content_hash(b"[]").unwrap_err();
        assert!(err.to_string().contains("must be a JSON object"));
    }

    #[test]
    fn rejects_invalid_json() {
        let err = content_hash(b"{not json").unwrap_err();
        assert!(err.to_string().contains("not valid JSON"));
    }

    #[test]
    fn hex_lower_is_lowercase() {
        assert_eq!(hex_lower(&[0xab, 0xcd]), "abcd");
        assert_eq!(hex_lower(&[0x00, 0xff]), "00ff");
    }

    // ---- IO & editing -------------------------------------------------------

    use tempfile::TempDir;

    /// Composer-emitted composer.json (4-space indent, trailing newline,
    /// raw slashes — `JsonFile::encode` default).
    const FIXTURE_DISK_COMPOSER_JSON: &str = "\
{
    \"name\": \"acme/widget-tool\",
    \"require\": {
        \"php\": \"^8.3\",
        \"monolog/monolog\": \"^3.5\"
    },
    \"require-dev\": {
        \"phpunit/phpunit\": \"^10.5\"
    }
}
";
    const FIXTURE_STARTING_HASH: &str = "be62286b165a989453dc015b7cf2d1f3";
    const FIXTURE_POST_ADD_HASH: &str = "d353d0970b82c8e447c124f0129142d5";

    /// Skeletal composer.lock with the starting content-hash baked in.
    /// Real composer.lock files have many more keys (packages, aliases,
    /// stability-flags, etc.) — the editor must touch only `content-hash`
    /// and `platform[-dev]` and leave everything else byte-identical
    /// modulo pretty-print normalisation.
    const FIXTURE_DISK_COMPOSER_LOCK: &str = "\
{
    \"_readme\": [
        \"This file locks the dependencies of your project to a known state\"
    ],
    \"content-hash\": \"be62286b165a989453dc015b7cf2d1f3\",
    \"packages\": [],
    \"packages-dev\": [],
    \"aliases\": [],
    \"minimum-stability\": \"stable\",
    \"stability-flags\": {},
    \"prefer-stable\": false,
    \"prefer-lowest\": false,
    \"platform\": {
        \"php\": \"^8.3\"
    },
    \"platform-dev\": [],
    \"plugin-api-version\": \"2.6.0\"
}
";

    #[test]
    fn round_trip_composer_json_via_encode_for_disk() {
        // Re-encoding what PHP wrote must produce the exact same bytes.
        // If this test ever fails, the pretty-print encoder has drifted
        // from JsonFile::encode's output.
        let value: Value = serde_json::from_str(FIXTURE_DISK_COMPOSER_JSON).unwrap();
        let bytes = encode_for_disk(&value);
        assert_eq!(
            std::str::from_utf8(&bytes).unwrap(),
            FIXTURE_DISK_COMPOSER_JSON
        );
    }

    #[test]
    fn starting_hash_matches_disk_bytes() {
        // The hash is computed from the on-disk composer.json (which
        // has `/` raw + indented), but the hash algorithm itself
        // produces the flags=0 byte stream. So content_hash(disk bytes)
        // should equal the PHP-generated starting hash.
        let h = content_hash(FIXTURE_DISK_COMPOSER_JSON.as_bytes()).unwrap();
        assert_eq!(h, FIXTURE_STARTING_HASH);
    }

    #[test]
    fn require_add_appends_to_existing_require() {
        let mut v: Value = serde_json::from_str(FIXTURE_DISK_COMPOSER_JSON).unwrap();
        require_add(&mut v, "ext-redis", "*", false).unwrap();
        let req = v.get("require").unwrap().as_object().unwrap();
        assert_eq!(req.get("ext-redis").unwrap(), &Value::String("*".into()));
        // Existing entries stay in source order, new entry at the end.
        let keys: Vec<&str> = req.keys().map(String::as_str).collect();
        assert_eq!(keys, ["php", "monolog/monolog", "ext-redis"]);
    }

    #[test]
    fn require_add_creates_require_if_absent() {
        let mut v: Value = serde_json::from_str(r#"{"name":"a/b"}"#).unwrap();
        require_add(&mut v, "ext-redis", "*", false).unwrap();
        assert_eq!(
            v.get("require").unwrap().get("ext-redis").unwrap(),
            &Value::String("*".into())
        );
    }

    #[test]
    fn require_add_updates_existing_key_in_place() {
        // composer require ext-redis:^6 on a project that already has
        // ext-redis:* updates the constraint without moving the key.
        let mut v: Value = serde_json::from_str(
            r#"{"require":{"php":"^8.3","ext-redis":"*","monolog/monolog":"^3.5"}}"#,
        )
        .unwrap();
        require_add(&mut v, "ext-redis", "^6", false).unwrap();
        let req = v.get("require").unwrap().as_object().unwrap();
        let keys: Vec<&str> = req.keys().map(String::as_str).collect();
        assert_eq!(keys, ["php", "ext-redis", "monolog/monolog"]);
        assert_eq!(req.get("ext-redis").unwrap(), &Value::String("^6".into()));
    }

    #[test]
    fn require_add_with_dev_uses_require_dev() {
        let mut v: Value = serde_json::from_str(FIXTURE_DISK_COMPOSER_JSON).unwrap();
        require_add(&mut v, "ext-xdebug", "*", true).unwrap();
        assert!(v.get("require-dev").unwrap().get("ext-xdebug").is_some());
        assert!(v.get("require").unwrap().get("ext-xdebug").is_none());
    }

    #[test]
    fn require_remove_drops_key_and_reports_state() {
        let mut v: Value = serde_json::from_str(FIXTURE_DISK_COMPOSER_JSON).unwrap();
        assert!(require_remove(&mut v, "monolog/monolog", false).unwrap());
        assert!(v.get("require").unwrap().get("monolog/monolog").is_none());
        // Idempotent: removing again is a no-op returning false.
        assert!(!require_remove(&mut v, "monolog/monolog", false).unwrap());
    }

    #[test]
    fn lock_set_platform_handles_array_form_empty() {
        // Composer writes empty platform-dev as `[]` (PHP array form).
        let mut lock: Value = serde_json::from_str(FIXTURE_DISK_COMPOSER_LOCK).unwrap();
        assert!(lock.get("platform-dev").unwrap().is_array());
        lock_set_platform(&mut lock, "ext-xdebug", "*", true).unwrap();
        let pd = lock.get("platform-dev").unwrap();
        assert!(pd.is_object());
        assert_eq!(pd.get("ext-xdebug").unwrap(), &Value::String("*".into()));
    }

    #[test]
    fn lock_set_content_hash_replaces_existing() {
        let mut lock: Value = serde_json::from_str(FIXTURE_DISK_COMPOSER_LOCK).unwrap();
        lock_set_content_hash(&mut lock, "deadbeef").unwrap();
        assert_eq!(
            lock.get("content-hash").unwrap(),
            &Value::String("deadbeef".into())
        );
    }

    #[test]
    fn apply_require_change_updates_both_files_and_hash() {
        // The end-to-end story: a project with composer.json + lockfile
        // matching `FIXTURE_STARTING_HASH`; bougie adds ext-redis;
        // composer.json gains the require, composer.lock's `platform`
        // gains the mirror and `content-hash` updates to a value that
        // matches our content_hash of the new composer.json.
        let td = TempDir::new().unwrap();
        let proj = td.path();
        std::fs::write(proj.join("composer.json"), FIXTURE_DISK_COMPOSER_JSON).unwrap();
        std::fs::write(proj.join("composer.lock"), FIXTURE_DISK_COMPOSER_LOCK).unwrap();

        let applied = apply_require_change(
            proj,
            &RequireChange::Add {
                key: "ext-redis".into(),
                constraint: "*".into(),
                dev: false,
            },
        )
        .unwrap();

        assert!(applied.change_applied);
        assert!(applied.composer_lock_path.is_some());
        assert_eq!(applied.new_content_hash, FIXTURE_POST_ADD_HASH);

        // composer.json has the require entry.
        let cj: Value =
            serde_json::from_slice(&std::fs::read(proj.join("composer.json")).unwrap()).unwrap();
        assert_eq!(
            cj.get("require").unwrap().get("ext-redis").unwrap(),
            &Value::String("*".into())
        );

        // composer.lock has the platform mirror and the new hash.
        let lock: Value =
            serde_json::from_slice(&std::fs::read(proj.join("composer.lock")).unwrap()).unwrap();
        assert_eq!(
            lock.get("content-hash").unwrap(),
            &Value::String(FIXTURE_POST_ADD_HASH.into())
        );
        assert_eq!(
            lock.get("platform").unwrap().get("ext-redis").unwrap(),
            &Value::String("*".into())
        );
    }

    #[test]
    fn apply_require_change_self_consistent() {
        // The new content-hash returned by apply_require_change MUST
        // equal content_hash(the composer.json we just wrote) — that
        // self-consistency is what makes `composer install` accept it.
        let td = TempDir::new().unwrap();
        let proj = td.path();
        std::fs::write(proj.join("composer.json"), FIXTURE_DISK_COMPOSER_JSON).unwrap();
        std::fs::write(proj.join("composer.lock"), FIXTURE_DISK_COMPOSER_LOCK).unwrap();
        let applied = apply_require_change(
            proj,
            &RequireChange::Add {
                key: "ext-mongodb".into(),
                constraint: "^1.18".into(),
                dev: false,
            },
        )
        .unwrap();
        let written_json = std::fs::read(proj.join("composer.json")).unwrap();
        let recomputed = content_hash(&written_json).unwrap();
        assert_eq!(recomputed, applied.new_content_hash);
    }

    #[test]
    fn apply_require_change_without_lockfile_skips_it() {
        let td = TempDir::new().unwrap();
        let proj = td.path();
        std::fs::write(proj.join("composer.json"), FIXTURE_DISK_COMPOSER_JSON).unwrap();
        // No composer.lock — first sync hasn't happened yet.
        let applied = apply_require_change(
            proj,
            &RequireChange::Add {
                key: "ext-redis".into(),
                constraint: "*".into(),
                dev: false,
            },
        )
        .unwrap();
        assert!(applied.composer_lock_path.is_none());
        assert!(!proj.join("composer.lock").exists());
        // composer.json was still updated.
        assert!(std::fs::read_to_string(proj.join("composer.json"))
            .unwrap()
            .contains("ext-redis"));
    }

    // ---- sort-packages ------------------------------------------------------

    #[test]
    fn sort_packages_disabled_is_noop() {
        // Without config.sort-packages, the require map keeps its
        // source order — even if it's currently unsorted.
        let mut v: Value = serde_json::from_str(
            r#"{"require":{"monolog/monolog":"^3.5","php":"^8.3","ext-redis":"*"}}"#,
        )
        .unwrap();
        sort_packages_if_configured(&mut v).unwrap();
        let keys: Vec<&str> = v
            .get("require")
            .unwrap()
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(keys, ["monolog/monolog", "php", "ext-redis"]);
    }

    #[test]
    fn sort_packages_matches_composer_oracle() {
        // PHP-generated oracle from `JsonManipulator::sortPackages`
        // (see commit message for the one-liner):
        //   php < php-64bit < hhvm < ext-mongodb < ext-redis
        //                  < lib-curl < monolog/monolog < symfony/console
        let mut v: Value = serde_json::from_str(
            r#"{
                "config": {"sort-packages": true},
                "require": {
                    "monolog/monolog": "^3.5",
                    "lib-curl": "*",
                    "ext-redis": "*",
                    "php": "^8.3",
                    "symfony/console": "^7.0",
                    "ext-mongodb": "^1.18",
                    "hhvm": "*",
                    "php-64bit": "*"
                }
            }"#,
        )
        .unwrap();
        sort_packages_if_configured(&mut v).unwrap();
        let keys: Vec<&str> = v
            .get("require")
            .unwrap()
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(
            keys,
            [
                "php",
                "php-64bit",
                "hhvm",
                "ext-mongodb",
                "ext-redis",
                "lib-curl",
                "monolog/monolog",
                "symfony/console",
            ]
        );
    }

    #[test]
    fn sort_packages_handles_require_dev_too() {
        let mut v: Value = serde_json::from_str(
            r#"{
                "config": {"sort-packages": true},
                "require": {"php": "^8.3"},
                "require-dev": {
                    "phpunit/phpunit": "^10.5",
                    "ext-xdebug": "*"
                }
            }"#,
        )
        .unwrap();
        sort_packages_if_configured(&mut v).unwrap();
        let dev_keys: Vec<&str> = v
            .get("require-dev")
            .unwrap()
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(dev_keys, ["ext-xdebug", "phpunit/phpunit"]);
    }

    #[test]
    fn apply_require_change_with_sort_packages_places_new_entry_correctly() {
        // The bug `bougie ext add redis` would hit without sort-packages
        // support: the new entry lands at the end of require instead of
        // between php and monolog/monolog. This test pins the fix.
        let td = TempDir::new().unwrap();
        let proj = td.path();
        std::fs::write(
            proj.join("composer.json"),
            r#"{
    "name": "acme/x",
    "config": {"sort-packages": true},
    "require": {
        "php": "^8.3",
        "monolog/monolog": "^3.5"
    }
}
"#,
        )
        .unwrap();
        apply_require_change(
            proj,
            &RequireChange::Add {
                key: "ext-redis".into(),
                constraint: "*".into(),
                dev: false,
            },
        )
        .unwrap();
        let cj: Value =
            serde_json::from_slice(&std::fs::read(proj.join("composer.json")).unwrap()).unwrap();
        let keys: Vec<&str> = cj
            .get("require")
            .unwrap()
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(keys, ["php", "ext-redis", "monolog/monolog"]);
    }

    #[test]
    fn sort_key_buckets_match_composer() {
        // Direct unit test of the sort_key fn against composer's
        // bucketing — guards against accidental drift in the prefix
        // ordering even when no end-to-end test covers a given group.
        assert!(sort_key("php") < sort_key("hhvm"));
        assert!(sort_key("php-zts") < sort_key("hhvm"));
        assert!(sort_key("hhvm") < sort_key("ext-redis"));
        assert!(sort_key("ext-zzz") < sort_key("lib-aaa"));
        assert!(sort_key("lib-curl") < sort_key("composer-runtime-api"));
        assert!(sort_key("composer-runtime-api") < sort_key("acme/widget"));
    }

    #[test]
    fn apply_require_change_remove_absent_key_is_noop() {
        let td = TempDir::new().unwrap();
        let proj = td.path();
        std::fs::write(proj.join("composer.json"), FIXTURE_DISK_COMPOSER_JSON).unwrap();
        let applied = apply_require_change(
            proj,
            &RequireChange::Remove { key: "ext-redis".into(), dev: false },
        )
        .unwrap();
        assert!(!applied.change_applied);
        // composer.json still parses cleanly.
        let cj: Value =
            serde_json::from_slice(&std::fs::read(proj.join("composer.json")).unwrap()).unwrap();
        assert!(cj.get("require").unwrap().get("ext-redis").is_none());
    }
}
