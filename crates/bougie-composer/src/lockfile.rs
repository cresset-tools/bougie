//! composer.json / composer.lock IO and editing primitives.
//!
//! Bougie touches both files when adding or removing an extension —
//! without invoking `composer require`, which would re-resolve the full
//! dependency graph and run platform checks against a PHP that hasn't
//! yet loaded the new ext. Doing the edits ourselves means we can:
//!
//! 1. Install the `.so` and enable it in `vendor/bougie/conf.d/` *first*.
//! 2. Add the `require.ext-<name>` line to composer.json directly.
//! 3. Mirror it under `platform.ext-<name>` in composer.lock and
//!    recompute `content-hash`.
//!
//! Step 3 is what this module exists for. `composer install` accepts
//! the result without complaint; no `composer update` involved.
//!
//! See `Composer\Package\Locker::getContentHash` for the algorithm
//! (`src/Composer/Package/Locker.php:89` in composer/composer).

use crate::php_json::{self, Mode};
use eyre::{eyre, Result, WrapErr};
use md5::{Digest, Md5};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Map, Value};
use std::borrow::Cow;
use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};

/// Deserialize a `BTreeMap<String, String>` that accepts PHP's empty-
/// `array` quirk: in PHP, `[]` (array) and `{}` (object) serialize
/// identically when empty, and Composer's older writers sometimes
/// emit `"require": []` instead of `"require": {}` for packages with
/// no dependencies. The default Serde Deserialize would reject the
/// array as "expected a map." This helper accepts either: empty
/// array → empty map; non-empty array → error (genuine type bug).
fn map_or_empty_array<'de, D>(deserializer: D) -> Result<BTreeMap<String, String>, D::Error>
where
    D: Deserializer<'de>,
{
    use serde::de::Error;
    let v = Value::deserialize(deserializer)?;
    match v {
        Value::Object(map) => map
            .into_iter()
            .map(|(k, v)| match v {
                Value::String(s) => Ok((k, s)),
                other => Err(D::Error::custom(format!(
                    "map value for `{k}` must be a string, got {other:?}",
                ))),
            })
            .collect(),
        Value::Array(arr) if arr.is_empty() => Ok(BTreeMap::new()),
        Value::Array(_) => Err(D::Error::custom(
            "expected a map or an empty array (the PHP empty-object quirk); got a non-empty array",
        )),
        other => Err(D::Error::custom(format!(
            "expected a map, got {other:?}",
        ))),
    }
}

/// [`map_or_empty_array`] for maps with arbitrary `Value`s — the PSR
/// namespace maps, whose values are a string or an array of strings.
/// Same PHP empty-object quirk: Composer-written locks carry
/// `"psr-0": []` for a package whose composer.json declared an empty
/// map (real example: Mage-OS module packages).
fn value_map_or_empty_array<'de, D>(deserializer: D) -> Result<BTreeMap<String, Value>, D::Error>
where
    D: Deserializer<'de>,
{
    use serde::de::Error;
    match Value::deserialize(deserializer)? {
        Value::Object(map) => Ok(map.into_iter().collect()),
        Value::Array(arr) if arr.is_empty() => Ok(BTreeMap::new()),
        Value::Array(_) => Err(D::Error::custom(
            "expected a map or an empty array (the PHP empty-object quirk); got a non-empty array",
        )),
        other => Err(D::Error::custom(format!(
            "expected a map, got {other:?}",
        ))),
    }
}

/// Deserialize a `Vec<String>` while mirroring Composer's quirky
/// tolerance for malformed list items in `autoload.classmap` /
/// `autoload.files` / `autoload.exclude-from-classmap` / `bin`.
///
/// Composer's `AutoloadGenerator::parseAutoloadsType` does
/// `(array) $paths` on each list item and then iterates the
/// resulting array's *values*. The practical effect is:
///
/// - A bare string normalizes to a one-element vec. Composer's schema
///   allows several of these fields to be either a string or an array
///   (`license` is the common one — `ArrayLoader` does
///   `(array) $config['license']`), and PHP's `(array)"MIT"` yields
///   `["MIT"]`. Accepting the string here mirrors that.
/// - Strings inside an array pass through verbatim.
/// - Objects (a real-world quirk: `amphp/process` v0.1.3 ships
///   `classmap: [{"Amp\\Process": "Process.php"}]`, almost certainly
///   a `psr-4` declaration that landed in `classmap`) contribute
///   each string-typed value, keys discarded. So that v0.1.3 entry
///   yields a classmap path of `Process.php` — matches what
///   Composer's own autoloader writes.
/// - Other types (numbers, booleans, null, nested arrays) are
///   dropped silently rather than failing the whole entry.
///
/// Bougie's pre-fix strict `Vec<String>` deserialize rejected the
/// whole `LockPackage` over the object item — and, before this, over a
/// scalar `"license": "MIT"` — which broke any resolve that walked
/// across the offending version.
fn string_list_lenient<'de, D>(deserializer: D) -> Result<Vec<String>, D::Error>
where
    D: Deserializer<'de>,
{
    let items = match Value::deserialize(deserializer)? {
        // `(array) "MIT"` in Composer → `["MIT"]`.
        Value::String(s) => return Ok(vec![s]),
        Value::Array(items) => items,
        // null / any other scalar → empty, rather than hard-failing
        // the whole package the way a strict `Vec<String>` would.
        _ => return Ok(Vec::new()),
    };
    let mut out: Vec<String> = Vec::with_capacity(items.len());
    for item in items {
        match item {
            Value::String(s) => out.push(s),
            Value::Object(map) => {
                for (_k, v) in map {
                    if let Value::String(s) = v {
                        out.push(s);
                    }
                }
            }
            _ => {}
        }
    }
    Ok(out)
}

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

/// Composer's canonical `_readme` preamble — the three short strings
/// every Composer-generated `composer.lock` carries verbatim. Exposed
/// as a constructor so callers don't hard-code the wording.
pub fn canonical_readme() -> Vec<String> {
    vec![
        "This file locks the dependencies of your project to a known state".into(),
        "Read more about it at https://getcomposer.org/doc/01-basic-usage.md#installing-dependencies".into(),
        "This file is @generated automatically".into(),
    ]
}

/// Serialize a `Lock` value to bytes in Composer's on-disk format
/// (4-space indent + trailing newline via [`encode_for_disk`]).
///
/// The caller is responsible for setting `lock.content_hash` to the
/// hash of the corresponding `composer.json` — see [`content_hash`].
/// Returning bytes (rather than writing directly) lets the caller
/// hash the output, log it, or write atomically via a different
/// strategy.
pub fn serialize_lock(lock: &Lock) -> Result<Vec<u8>> {
    let value = serde_json::to_value(lock)
        .map_err(|e| eyre!("serializing lockfile: {e}"))?;
    Ok(encode_for_disk(&value))
}

/// Write a `Lock` to disk atomically in Composer's on-disk format.
/// Convenience over [`serialize_lock`] for the common case where the
/// caller just wants the file on disk.
pub fn write_lock(path: &Path, lock: &Lock) -> Result<()> {
    let bytes = serialize_lock(lock)?;
    write_json_bytes(path, &bytes)
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

/// Result of [`add_repositories`]. Parallels [`RequireApplied`].
#[derive(Debug, Clone)]
pub struct RepositoriesApplied {
    pub composer_json_path: PathBuf,
    pub composer_lock_path: Option<PathBuf>,
    pub new_content_hash: String,
    /// Newly-added entries (0 = every URL was already present).
    pub added: usize,
}

/// Add `{"type":"composer","url":…}` entries to the project composer.json's
/// `repositories`, deduped by URL (so re-running is idempotent). Used by
/// `bougie login --composer-json` to provision a team's registry the committed,
/// stock-Composer-visible way (vs. bougie's local overlay).
///
/// Preserves the existing array vs named-object form; an absent `repositories`
/// becomes an array. Because `repositories` is part of the content-hash, an
/// existing composer.lock's `content-hash` is updated too so `install` /
/// `validate` don't flag a stale lock. Mirrors [`apply_require_change`].
pub fn add_repositories(project_root: &Path, urls: &[String]) -> Result<RepositoriesApplied> {
    let composer_json_path = project_root.join("composer.json");
    let composer_lock_path = project_root.join("composer.lock");

    let mut composer_json = read_json_file(&composer_json_path)?;
    let added = repositories_add(&mut composer_json, urls)?;

    let written_bytes = encode_for_disk(&composer_json);
    let new_content_hash = content_hash(&written_bytes)?;
    write_json_bytes(&composer_json_path, &written_bytes)?;

    // `repositories` isn't in the lock's `platform` map (unlike `require`), so
    // only the content-hash needs syncing when a lock exists.
    let lock_updated = if composer_lock_path.exists() {
        let mut lock = read_json_file(&composer_lock_path)?;
        lock_set_content_hash(&mut lock, &new_content_hash)?;
        write_json_file(&composer_lock_path, &lock)?;
        true
    } else {
        false
    };

    Ok(RepositoriesApplied {
        composer_json_path,
        composer_lock_path: lock_updated.then_some(composer_lock_path),
        new_content_hash,
        added,
    })
}

/// Insert composer-type repository entries (by URL) into a composer.json value's
/// `repositories`, deduping by URL. Preserves array vs named-object form; a
/// missing value becomes an array. Returns how many were newly added.
fn repositories_add(composer_json: &mut Value, urls: &[String]) -> Result<usize> {
    let obj = composer_json
        .as_object_mut()
        .ok_or_else(|| eyre!("composer.json top level must be a JSON object"))?;
    let entry = obj
        .entry("repositories")
        .or_insert_with(|| Value::Array(Vec::new()));
    match entry {
        Value::Array(arr) => {
            let mut seen = repo_urls_in(arr.iter());
            let mut added = 0;
            for url in urls {
                if seen.insert(url.clone()) {
                    arr.push(composer_repo_entry(url));
                    added += 1;
                }
            }
            Ok(added)
        }
        Value::Object(named) => {
            let mut seen = repo_urls_in(named.values());
            let mut added = 0;
            for url in urls {
                if seen.insert(url.clone()) {
                    let key = unique_repo_key(url, named);
                    named.insert(key, composer_repo_entry(url));
                    added += 1;
                }
            }
            Ok(added)
        }
        _ => Err(eyre!(
            "composer.json `repositories` must be an array or object"
        )),
    }
}

/// The set of `url`s already declared among some repository entries.
fn repo_urls_in<'a, I: Iterator<Item = &'a Value>>(entries: I) -> std::collections::HashSet<String> {
    entries
        .filter_map(|e| e.get("url").and_then(Value::as_str).map(str::to_owned))
        .collect()
}

fn composer_repo_entry(url: &str) -> Value {
    serde_json::json!({ "type": "composer", "url": url })
}

/// A stable, human-ish key for the named-object `repositories` form, derived
/// from the URL's path (e.g. `https://host/acme/web` → `acme/web`), made unique
/// against existing keys.
fn unique_repo_key(url: &str, existing: &serde_json::Map<String, Value>) -> String {
    let base = url
        .split_once("://")
        .map_or(url, |(_, rest)| rest)
        .split_once('/')
        .map_or("", |(_host, path)| path)
        .trim_matches('/');
    let base = if base.is_empty() { "repo" } else { base };
    if !existing.contains_key(base) {
        return base.to_owned();
    }
    (1..).map(|n| format!("{base}-{n}")).find(|k| !existing.contains_key(k)).unwrap()
}

// -----------------------------------------------------------------
// Typed read API for `composer.lock`.
//
// The edit primitives above operate on `serde_json::Value` to preserve
// byte-for-byte fidelity when round-tripping; the typed API below is
// the read side for callers that need to *consume* a lockfile:
// `bougie composer install` reads the package list, derives a
// `DistRequest` per entry, and hands it to the parallel downloader in
// `bougie-composer-resolver`. Autoload metadata is also exposed for
// the eventual install-time wiring to `bougie-autoloader`.
//
// The schema is intentionally permissive: every field we don't yet act
// on is captured as `Value` or skipped entirely (`serde(default)`),
// because Composer adds new fields over time and we don't want
// parsing to fail when a future Composer release introduces something
// new. Strict validation lives in the resolver, not in the reader.
// -----------------------------------------------------------------

/// Parsed `composer.lock`. Round-trips through `serde_json` but loses
/// the byte-exact representation — for in-place edits use
/// [`read_json_file`] + the `lock_*` helpers above.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Lock {
    /// Composer's standard `_readme` preamble. Three short
    /// human-readable strings the upstream writer always emits.
    /// Skipped on read by `serde(default)` if absent (older locks
    /// may not have it); always emitted on write.
    #[serde(rename = "_readme", default, skip_serializing_if = "Vec::is_empty")]
    pub readme: Vec<String>,
    /// Set on every lockfile produced by Composer 1.10+; absence means
    /// the lockfile predates the algorithm and the install command
    /// should refuse with a clear error.
    #[serde(rename = "content-hash", default)]
    pub content_hash: Option<String>,
    #[serde(default)]
    pub packages: Vec<LockPackage>,
    #[serde(rename = "packages-dev", default)]
    pub packages_dev: Vec<LockPackage>,
    /// Composer's `aliases` array. Empty in practice for projects
    /// without VCS sources; populated by `dev-X as Y` declarations.
    #[serde(default)]
    pub aliases: Vec<Value>,
    /// Top-level `minimum-stability` (string, e.g. `"stable"`,
    /// `"dev"`). Drives the eventual resolver's stability filter; the
    /// installer doesn't act on it but exposes it so the verifier in
    /// Phase B can.
    #[serde(rename = "minimum-stability", default)]
    pub minimum_stability: Option<String>,
    /// Composer's per-package stability flag map (`"acme/foo": 20`
    /// where the integer is Composer's stability constant —
    /// dev=20, alpha=15, beta=10, RC=5, stable=0).
    #[serde(rename = "stability-flags", default)]
    pub stability_flags: BTreeMap<String, i32>,
    #[serde(rename = "prefer-stable", default)]
    pub prefer_stable: bool,
    #[serde(rename = "prefer-lowest", default)]
    pub prefer_lowest: bool,
    /// Platform requirements mirrored by `apply_require_change`. Map
    /// from platform-package name (e.g. `"php"`, `"ext-redis"`) to a
    /// constraint string.
    #[serde(default, deserialize_with = "map_or_empty_array")]
    pub platform: BTreeMap<String, String>,
    #[serde(rename = "platform-dev", default, deserialize_with = "map_or_empty_array")]
    pub platform_dev: BTreeMap<String, String>,
    /// `platform-overrides` from composer.json, copied through.
    #[serde(
        rename = "platform-overrides",
        default,
        deserialize_with = "map_or_empty_array",
        skip_serializing_if = "BTreeMap::is_empty"
    )]
    pub platform_overrides: BTreeMap<String, String>,
    /// Reported by the Composer build that wrote the lockfile; e.g.
    /// `"2.6.0"`. Carried through verbatim by the writer.
    #[serde(rename = "plugin-api-version", default)]
    pub plugin_api_version: Option<String>,
}

/// One package entry from `packages` or `packages-dev`.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct LockPackage {
    /// `vendor/package` exactly as Composer writes it (case-preserved;
    /// Composer canonicalizes case on resolve but the lock keeps the
    /// declared form).
    pub name: String,
    /// Short human description. Composer writes it into the lock; we
    /// surface it in `composer show`'s listing. Absent for some
    /// packages, so `Option`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Selected version. Format depends on the source: a semver string
    /// for stable releases (`"3.5.0"`, `"1.2.3-RC1"`), a `dev-*` ref
    /// for branch installs (`"dev-main"`, `"1.x-dev"`), or rarely an
    /// `as`-alias form (`"dev-main as 1.0.x-dev"`) — we expose the
    /// string verbatim and let the resolver/installer interpret it.
    pub version: String,
    /// Composer's 4-segment normalized form (`"3.5.0.0"`,
    /// `"dev-main"`). Optional because pre-2.x lockfiles omit it.
    #[serde(rename = "version_normalized", default)]
    pub version_normalized: Option<String>,
    /// The dist archive Composer will install from. Some packages ship
    /// only via `source` (git clone) — those are out of scope until
    /// Phase D and the install command surfaces a clear error.
    #[serde(default)]
    pub dist: Option<LockDist>,
    /// VCS source. We capture but don't act on it in Phase A; Phase D
    /// will use it for git-ref installs.
    #[serde(default)]
    pub source: Option<LockSource>,
    /// Package-level `transport-options` — Composer's home for a path
    /// repository's `{"symlink": ..., "relative": ...}` install hints
    /// (it dumps them here, as a sibling of `dist`, not nested inside
    /// it). The install-time symlink-or-copy materializer reads them.
    /// Empty `Value::Null` for every non-path package, suppressed from
    /// output so only path packages carry the key.
    #[serde(rename = "transport-options", default, skip_serializing_if = "Value::is_null")]
    pub transport_options: Value,
    /// Transitive runtime dependencies (package name → constraint).
    /// Composer writes this in a stable order; consumers that care
    /// about ordering iterate in insertion order via `serde_json`'s
    /// `preserve_order` (which `BTreeMap` does *not* do — we use it
    /// here because the resolver only needs set semantics).
    #[serde(default, deserialize_with = "map_or_empty_array")]
    pub require: BTreeMap<String, String>,
    /// Transitive dev dependencies (rarely populated inside the lock —
    /// dev-only constraints land on the root package's
    /// `composer.json`, not on transitive packages).
    #[serde(rename = "require-dev", default, deserialize_with = "map_or_empty_array")]
    pub require_dev: BTreeMap<String, String>,
    /// Package type: `"library"`, `"composer-plugin"`,
    /// `"metapackage"`, etc. Drives plugin-detection in the eventual
    /// fallback path (`composer-plugin` types with resolver-affecting
    /// capabilities force `composer.phar` fallback).
    #[serde(rename = "type", default)]
    pub package_type: Option<String>,
    /// Autoload declarations. Surfaces as the typed
    /// [`LockAutoload`] shape so the installer can hand it to
    /// `bougie-autoloader` without re-parsing.
    #[serde(default)]
    pub autoload: LockAutoload,
    /// `autoload-dev` is intentionally NOT consumed at install time
    /// for transitive packages — Composer itself ignores it for
    /// non-root packages — but we keep the field as `Value` so a
    /// future caller can opt in without reshaping the struct.
    #[serde(rename = "autoload-dev", default)]
    pub autoload_dev: Value,
    /// Packages declared to satisfy this package's `replace`. Each
    /// entry maps `vendor/name → version-constraint`. Phase C feeds
    /// this into the pubgrub replace/provide encoding; Phase A
    /// ignores it.
    #[serde(default, deserialize_with = "map_or_empty_array")]
    pub replace: BTreeMap<String, String>,
    /// Same shape as `replace` for `provide`.
    #[serde(default, deserialize_with = "map_or_empty_array")]
    pub provide: BTreeMap<String, String>,
    /// Inverse: packages this one conflicts with. Phase C uses this;
    /// Phase A ignores it.
    #[serde(default, deserialize_with = "map_or_empty_array")]
    pub conflict: BTreeMap<String, String>,
    /// `bin` listing for the package — each path is relative to the
    /// package root and gets symlinked into `vendor/bin/` at install
    /// time. Captured for the bin-linker that lands alongside the
    /// install command.
    #[serde(default, deserialize_with = "string_list_lenient")]
    pub bin: Vec<String>,
    /// Free-form package metadata Composer copies through verbatim.
    /// `extra.branch-alias` matters for resolution; everything else is
    /// consumed by third-party plugins.
    #[serde(default)]
    pub extra: Value,
    /// ISO-8601 timestamp Composer recorded when this version was
    /// published. Present for Packagist-served packages; absent for
    /// `path` and `vcs` sources. Used by `--prefer-stable` heuristics
    /// in the resolver; the installer ignores it.
    #[serde(default)]
    pub time: Option<String>,
    /// SPDX license identifier(s). Composer writes this as an array of
    /// strings (`["MIT"]`, `["GPL-2.0-or-later", "MIT"]`); a few older
    /// packages use a single string, which `string_list_lenient`
    /// normalizes to a one-element vec. Consumed by `composer licenses`.
    #[serde(default, deserialize_with = "string_list_lenient")]
    pub license: Vec<String>,
    /// Funding URLs declared by the package (`composer fund`). Each
    /// entry is `{ "type": "...", "url": "..." }`; Composer groups the
    /// output by vendor.
    #[serde(default)]
    pub funding: Vec<LockFunding>,
}

/// One `funding` entry — a way to financially support the package's
/// maintainers. Composer's `fund` command groups these by vendor.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct LockFunding {
    /// Funding platform, e.g. `"github"`, `"patreon"`, `"open_collective"`,
    /// `"tidelift"`, or `"custom"`. Optional in the wild; defaults empty.
    #[serde(rename = "type", default)]
    pub kind: String,
    pub url: String,
}

/// `dist` block — what bougie's parallel downloader actually consumes.
/// The combination of `kind` + `shasum` + `url` is the minimum
/// information needed to materialize the package; `reference` carries
/// the upstream commit hash (used for the wrapping-directory name in
/// Packagist zipballs, and for verification debugging).
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct LockDist {
    /// `"zip"` for Packagist API zipballs, `"tar"` for tarballs (rare,
    /// not yet supported by the downloader — `RESOLVER_PLAN.md` Phase
    /// A explicitly defers), `"path"` for local-path repositories
    /// (the autoloader fixtures use this; no fetching).
    #[serde(rename = "type")]
    pub kind: String,
    pub url: String,
    /// sha1 hex of the dist archive, lower-case. Optional because
    /// `path` dists don't have one.
    #[serde(default)]
    pub shasum: Option<String>,
    /// Upstream VCS reference (full sha for git). Used to derive the
    /// wrapping-directory name inside Packagist zipballs.
    #[serde(default)]
    pub reference: Option<String>,
    /// Alternate download locations, carried through from the
    /// repository's root `packages.json` `mirrors` key (Private
    /// Packagist, satis with dist mirroring). Each entry is a URL
    /// *template* with `%package%` / `%version%` / `%reference%` /
    /// `%type%` placeholders — substitution happens at download time
    /// via [`LockPackage::dist_urls`], exactly like Composer's
    /// `ComposerMirror::processUrl`. Composer dumps these into the
    /// lock (`ArrayDumper`), which is what lets `composer install`
    /// reach a private repo's mirrored dists without credentials for
    /// the origin VCS host; bougie does the same. Empty for packages
    /// from repos that declare no mirrors (all of public Packagist),
    /// and suppressed from output so those lockfiles stay byte-stable.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mirrors: Vec<DistMirror>,
    /// Legacy field: some lockfiles carry `transport-options` nested
    /// inside `dist`. Composer itself writes them at the *package*
    /// level (see [`LockPackage::transport_options`]); this is kept
    /// only so a dist that happens to carry them still round-trips.
    /// Suppressed from output when empty so normal dists stay clean.
    #[serde(rename = "transport-options", default, skip_serializing_if = "Value::is_null")]
    pub transport_options: Value,
}

/// One mirror entry on a [`LockDist`] (or [`LockSource`]): a URL
/// template plus Composer's `preferred` flag. A preferred mirror is
/// tried *before* the dist's own `url`; a non-preferred one after it.
/// Shape matches what Composer's `ArrayDumper` writes into the lock:
/// `{"url": "...", "preferred": true}`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct DistMirror {
    pub url: String,
    #[serde(default)]
    pub preferred: bool,
}

/// `source` block — VCS coordinates. Phase D will use this when we
/// add git-clone-as-source-install; Phase A only surfaces it so error
/// messages can name the source URL when a dist is missing.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct LockSource {
    #[serde(rename = "type")]
    pub kind: String,
    pub url: String,
    pub reference: String,
    /// Source-mirror entries (same `{url, preferred}` shape as dist
    /// mirrors). Bougie never installs from source, but a
    /// Composer-written lock may carry them — kept so a read → write
    /// round-trip doesn't silently drop the key.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mirrors: Vec<DistMirror>,
}

/// `autoload` block — passed through to `bougie-autoloader` at install
/// time. The shapes mirror Composer's schema:
///
/// - `psr-4` and `psr-0`: namespace → directory(ies). A single string
///   or an array of strings.
/// - `classmap`: list of directories or files to scan.
/// - `files`: list of files to `require_once` from `vendor/autoload.php`.
/// - `exclude-from-classmap`: glob patterns the autoloader skips when
///   building the classmap.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct LockAutoload {
    /// `psr-4` maps a namespace prefix (with trailing `\\`) to one or
    /// more directories. Each value is either a single string or an
    /// array of strings; both arrive here as a generic `Value` so we
    /// don't lose information when round-tripping through the typed
    /// shape. `bougie-autoloader` already handles both forms. (Empty
    /// maps arrive as `[]` — the PHP empty-object quirk; round-tripping
    /// normalizes them to `{}`, which Composer also accepts.)
    #[serde(rename = "psr-4", default, deserialize_with = "value_map_or_empty_array")]
    pub psr_4: BTreeMap<String, Value>,
    #[serde(rename = "psr-0", default, deserialize_with = "value_map_or_empty_array")]
    pub psr_0: BTreeMap<String, Value>,
    #[serde(default, deserialize_with = "string_list_lenient")]
    pub classmap: Vec<String>,
    #[serde(default, deserialize_with = "string_list_lenient")]
    pub files: Vec<String>,
    #[serde(rename = "exclude-from-classmap", default, deserialize_with = "string_list_lenient")]
    pub exclude_from_classmap: Vec<String>,
}

impl Lock {
    /// Read and parse a `composer.lock` file from disk.
    pub fn read(path: &Path) -> Result<Self> {
        let bytes = std::fs::read(path)
            .wrap_err_with(|| format!("reading {}", path.display()))?;
        Self::from_bytes(&bytes)
            .wrap_err_with(|| format!("parsing {}", path.display()))
    }

    /// Parse from raw bytes. Useful for tests and for callers that
    /// already have the lock in memory (e.g. after staging an edit).
    pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
        serde_json::from_slice(bytes)
            .map_err(|e| eyre!("composer.lock parse: {e}"))
    }

    /// Iterate over `packages` then `packages-dev`. Used by the
    /// installer when `--no-dev` is off; with `--no-dev` the caller
    /// iterates `self.packages` directly.
    pub fn all_packages(&self) -> impl Iterator<Item = &LockPackage> {
        self.packages.iter().chain(self.packages_dev.iter())
    }
}

impl LockPackage {
    /// Convenience: `true` when the package's only install source is a
    /// `path` dist (no shasum, no remote URL). The Phase A downloader
    /// skips these — `path` dists are materialized by Composer through
    /// a symlink-or-copy mechanism that lives outside the
    /// dist-archive flow.
    pub fn is_path_dist(&self) -> bool {
        self.dist.as_ref().is_some_and(|d| d.kind == "path")
    }

    /// Is this a Composer plugin? Used to gate the fallback to
    /// `composer.phar` for installs that involve plugin packages
    /// (which can register install-time hooks we don't run).
    pub fn is_composer_plugin(&self) -> bool {
        matches!(self.package_type.as_deref(), Some("composer-plugin" | "composer-installer"))
    }

    /// Is this a metapackage? Metapackages have no code and no `dist`
    /// block — they exist purely to group a set of `require` entries
    /// under one name. The install flow skips them (nothing to fetch
    /// or extract) and the autoloader emits `install_path => NULL`
    /// for them in `InstalledVersions.php` (mirroring Composer).
    pub fn is_metapackage(&self) -> bool {
        self.package_type.as_deref() == Some("metapackage")
    }

    /// Download URL candidates for this package's dist, in the order
    /// the downloader should try them. Composer's `Package::getUrls`:
    /// the dist's own `url` comes first, then each mirror's
    /// substituted URL — except `preferred` mirrors, which are moved
    /// to the *front* of the list. Duplicates are dropped. Empty when
    /// the package has no dist.
    ///
    /// This ordering is what lets a Private-Packagist project install
    /// without credentials for the origin VCS host: the preferred
    /// mirror on the (authenticated) Packagist host is tried before
    /// the raw `gitlab.example.com` archive URL.
    ///
    /// `%version%` substitution wants Composer's *normalized* version
    /// (`1.2.0.0`, not `1.2.0`) — bougie-written locks always carry
    /// `version_normalized`, Composer-written locks strip it, so we
    /// re-normalize the pretty version as the fallback (exactly what
    /// Composer's `ArrayLoader` does on lock read).
    pub fn dist_urls(&self) -> Vec<String> {
        let Some(dist) = &self.dist else {
            return Vec::new();
        };
        let name = self.name.to_ascii_lowercase();
        let version: Cow<'_, str> = match &self.version_normalized {
            Some(v) => Cow::Borrowed(v.as_str()),
            None => match composer_semver::version::Version::parse(&self.version) {
                Ok(v) => Cow::Owned(v.normalized),
                Err(_) => Cow::Borrowed(self.version.as_str()),
            },
        };
        let process = |template: &str| {
            process_mirror_url(
                template,
                &name,
                &version,
                &self.version,
                dist.reference.as_deref(),
                &dist.kind,
            )
        };
        // Composer only runs the dist's own URL through placeholder
        // substitution when it actually contains one — mirrors always.
        let first = if dist.url.contains('%') { process(&dist.url) } else { dist.url.clone() };
        let mut urls = vec![first];
        for mirror in &dist.mirrors {
            let url = process(&mirror.url);
            if urls.contains(&url) {
                continue;
            }
            if mirror.preferred {
                urls.insert(0, url);
            } else {
                urls.push(url);
            }
        }
        urls
    }
}

/// Substitute Composer's mirror-URL placeholders — a port of
/// `ComposerMirror::processUrl`:
///
/// - `%package%` → the (lowercase) package name
/// - `%version%` → the normalized version, md5-hashed when it contains
///   a `/` (branch names like `dev-feature/x` would break the URL path)
/// - `%reference%` → the dist reference, kept verbatim only when it is
///   lowercase hex (a git sha); anything else is md5-hashed
/// - `%type%` → the dist type (`zip`, `tar`)
/// - `%prettyVersion%` → the version exactly as the lock spells it
pub fn process_mirror_url(
    template: &str,
    package_name: &str,
    version: &str,
    pretty_version: &str,
    reference: Option<&str>,
    dist_type: &str,
) -> String {
    let reference = reference.unwrap_or("");
    let is_hex = reference
        .chars()
        .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c));
    let reference: Cow<'_, str> = if is_hex || reference == "%reference%" {
        Cow::Borrowed(reference)
    } else {
        Cow::Owned(md5_hex(reference))
    };
    let version: Cow<'_, str> = if version.contains('/') {
        Cow::Owned(md5_hex(version))
    } else {
        Cow::Borrowed(version)
    };
    template
        .replace("%package%", package_name)
        .replace("%version%", &version)
        .replace("%reference%", &reference)
        .replace("%type%", dist_type)
        .replace("%prettyVersion%", pretty_version)
}

fn md5_hex(s: &str) -> String {
    let mut hasher = Md5::new();
    hasher.update(s.as_bytes());
    hex_lower(&hasher.finalize())
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
    fn license_accepts_string_array_and_null() {
        // Composer's schema allows `license` to be a bare string; a
        // strict `Vec<String>` deserialize used to fail the whole
        // package with "invalid type: string, expected a sequence".
        let string_form: LockPackage = serde_json::from_str(
            r#"{"name": "acme/lib", "version": "1.0.0", "license": "MIT"}"#,
        )
        .expect("string license must parse");
        assert_eq!(string_form.license, vec!["MIT".to_string()]);

        let array_form: LockPackage = serde_json::from_str(
            r#"{"name": "acme/lib", "version": "1.0.0", "license": ["GPL-2.0-or-later", "MIT"]}"#,
        )
        .expect("array license must parse");
        assert_eq!(
            array_form.license,
            vec!["GPL-2.0-or-later".to_string(), "MIT".to_string()]
        );

        // null and absent both normalize to empty rather than failing.
        let null_form: LockPackage = serde_json::from_str(
            r#"{"name": "acme/lib", "version": "1.0.0", "license": null}"#,
        )
        .expect("null license must parse");
        assert!(null_form.license.is_empty());

        let absent: LockPackage =
            serde_json::from_str(r#"{"name": "acme/lib", "version": "1.0.0"}"#).unwrap();
        assert!(absent.license.is_empty());
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
    fn lock_autoload_psr_maps_handle_array_form_empty() {
        // PHP's json_encode writes an empty assoc array as `[]`, so
        // Composer-written locks carry `"psr-0": []` for packages whose
        // composer.json declared an empty map (seen in the wild on
        // Mage-OS module packages). A non-empty array stays an error.
        let lock = Lock::from_bytes(
            br#"{
                "content-hash": "x",
                "packages": [{
                    "name": "acme/mod",
                    "version": "1.0.0",
                    "autoload": {
                        "psr-4": {"Acme\\Mod\\": ""},
                        "psr-0": [],
                        "classmap": [],
                        "files": ["registration.php"]
                    }
                }],
                "packages-dev": []
            }"#,
        )
        .unwrap();
        let autoload = &lock.packages[0].autoload;
        assert!(autoload.psr_0.is_empty());
        assert_eq!(autoload.psr_4.len(), 1);
        assert_eq!(autoload.files, vec!["registration.php"]);

        let err = Lock::from_bytes(
            br#"{
                "content-hash": "x",
                "packages": [{
                    "name": "acme/mod",
                    "version": "1.0.0",
                    "autoload": {"psr-0": ["oops"]}
                }],
                "packages-dev": []
            }"#,
        )
        .unwrap_err();
        assert!(err.to_string().contains("non-empty array"), "{err}");
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
    fn add_repositories_writes_array_dedups_and_syncs_lock_hash() {
        let td = TempDir::new().unwrap();
        let proj = td.path();
        std::fs::write(
            proj.join("composer.json"),
            "{\n    \"name\": \"acme/app\",\n    \"require\": {}\n}\n",
        )
        .unwrap();
        std::fs::write(
            proj.join("composer.lock"),
            "{\n    \"content-hash\": \"stale\",\n    \"packages\": [],\n    \"packages-dev\": []\n}\n",
        )
        .unwrap();

        let urls = vec![
            "https://pkgs.example/acme/web".to_string(),
            "https://pkgs.example/acme/api".to_string(),
        ];
        let applied = add_repositories(proj, &urls).unwrap();
        assert_eq!(applied.added, 2);
        assert!(applied.composer_lock_path.is_some());

        // composer.json gains an array `repositories` with both composer URLs,
        // in order.
        let cj: Value =
            serde_json::from_slice(&std::fs::read(proj.join("composer.json")).unwrap()).unwrap();
        let repos = cj.get("repositories").unwrap().as_array().unwrap();
        let got: Vec<&str> = repos.iter().map(|r| r["url"].as_str().unwrap()).collect();
        assert_eq!(
            got,
            ["https://pkgs.example/acme/web", "https://pkgs.example/acme/api"]
        );
        assert!(repos.iter().all(|r| r["type"] == "composer"));

        // The lock's content-hash is updated and self-consistent with the
        // composer.json we just wrote — what `install`/`validate` demand.
        let written = std::fs::read(proj.join("composer.json")).unwrap();
        let expect_hash = content_hash(&written).unwrap();
        assert_eq!(applied.new_content_hash, expect_hash);
        let lock: Value =
            serde_json::from_slice(&std::fs::read(proj.join("composer.lock")).unwrap()).unwrap();
        assert_eq!(lock["content-hash"], Value::String(expect_hash));

        // Idempotent: re-adding an existing URL plus a new one adds only the new.
        let urls2 = vec![
            "https://pkgs.example/acme/web".to_string(),
            "https://pkgs.example/acme/theme".to_string(),
        ];
        let applied2 = add_repositories(proj, &urls2).unwrap();
        assert_eq!(applied2.added, 1, "only the new URL is added");
        let cj2: Value =
            serde_json::from_slice(&std::fs::read(proj.join("composer.json")).unwrap()).unwrap();
        assert_eq!(cj2["repositories"].as_array().unwrap().len(), 3);
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

    // -----------------------------------------------------------------
    // Tests for the typed read API (Lock, LockPackage, ...).
    // -----------------------------------------------------------------

    /// Realistic Packagist-shape lock entry. Modeled on a real
    /// `monolog/monolog` 3.5.0 lock record (URL + sha1 are placeholders
    /// — the test never makes a network call). Covers: top-level
    /// metadata, dist + source, transitive require, psr-4 autoload,
    /// time, replace/provide/conflict shapes.
    const FIXTURE_PACKAGIST_LOCK: &str = r#"{
    "_readme": ["…"],
    "content-hash": "abc123def456abc123def456abc123de",
    "packages": [
        {
            "name": "monolog/monolog",
            "version": "3.5.0",
            "version_normalized": "3.5.0.0",
            "source": {
                "type": "git",
                "url": "https://github.com/Seldaek/monolog.git",
                "reference": "c915e2634718dbc8a4a15c61b0e62e7a44e14448"
            },
            "dist": {
                "type": "zip",
                "url": "https://api.github.com/repos/Seldaek/monolog/zipball/c915e2634718dbc8a4a15c61b0e62e7a44e14448",
                "reference": "c915e2634718dbc8a4a15c61b0e62e7a44e14448",
                "shasum": "0000000000000000000000000000000000000000"
            },
            "require": {
                "php": ">=8.1",
                "psr/log": "^2.0 || ^3.0"
            },
            "provide": {
                "psr/log-implementation": "3.0.0"
            },
            "require-dev": {
                "phpunit/phpunit": "^10.5.17"
            },
            "type": "library",
            "extra": {"branch-alias": {"dev-main": "3.x-dev"}},
            "autoload": {
                "psr-4": {"Monolog\\": "src/Monolog/"}
            },
            "time": "2023-12-05T16:23:35+00:00"
        },
        {
            "name": "acme/plugin",
            "version": "1.0.0",
            "dist": {
                "type": "zip",
                "url": "https://example.com/acme-plugin-1.0.0.zip",
                "shasum": "1111111111111111111111111111111111111111",
                "reference": "abcdef1234567890abcdef1234567890abcdef12"
            },
            "type": "composer-plugin",
            "require": {"composer-plugin-api": "^2.0"},
            "autoload": {
                "psr-0": {"Acme\\Plugin\\": "lib/"},
                "classmap": ["compat/"],
                "files": ["bootstrap.php"],
                "exclude-from-classmap": ["compat/legacy/"]
            },
            "bin": ["bin/acme-plugin"]
        }
    ],
    "packages-dev": [
        {
            "name": "phpunit/phpunit",
            "version": "10.5.0",
            "dist": {
                "type": "zip",
                "url": "https://api.github.com/repos/sebastianbergmann/phpunit/zipball/aaaa",
                "shasum": "2222222222222222222222222222222222222222"
            },
            "type": "library",
            "autoload": {"classmap": ["src/"]}
        }
    ],
    "aliases": [],
    "minimum-stability": "stable",
    "stability-flags": {},
    "prefer-stable": true,
    "prefer-lowest": false,
    "platform": {"php": "^8.3", "ext-redis": "*"},
    "platform-dev": {},
    "plugin-api-version": "2.6.0"
}"#;

    #[test]
    fn lock_parses_packagist_shape() {
        let lock = Lock::from_bytes(FIXTURE_PACKAGIST_LOCK.as_bytes()).unwrap();
        assert_eq!(lock.content_hash.as_deref(), Some("abc123def456abc123def456abc123de"));
        assert_eq!(lock.packages.len(), 2);
        assert_eq!(lock.packages_dev.len(), 1);
        assert_eq!(lock.minimum_stability.as_deref(), Some("stable"));
        assert!(lock.prefer_stable);
        assert!(!lock.prefer_lowest);
        assert_eq!(lock.plugin_api_version.as_deref(), Some("2.6.0"));
        assert_eq!(lock.platform.get("php").map(String::as_str), Some("^8.3"));
        assert_eq!(lock.platform.get("ext-redis").map(String::as_str), Some("*"));
    }

    #[test]
    fn lock_package_exposes_dist_and_source() {
        let lock = Lock::from_bytes(FIXTURE_PACKAGIST_LOCK.as_bytes()).unwrap();
        let monolog = &lock.packages[0];
        assert_eq!(monolog.name, "monolog/monolog");
        assert_eq!(monolog.version, "3.5.0");
        assert_eq!(monolog.version_normalized.as_deref(), Some("3.5.0.0"));

        let dist = monolog.dist.as_ref().expect("dist present");
        assert_eq!(dist.kind, "zip");
        assert!(dist.url.contains("Seldaek/monolog/zipball/"));
        assert_eq!(dist.shasum.as_deref(), Some("0000000000000000000000000000000000000000"));
        assert_eq!(
            dist.reference.as_deref(),
            Some("c915e2634718dbc8a4a15c61b0e62e7a44e14448")
        );

        let src = monolog.source.as_ref().expect("source present");
        assert_eq!(src.kind, "git");
        assert!(src.url.contains("Seldaek/monolog.git"));

        assert!(!monolog.is_path_dist());
        assert!(!monolog.is_composer_plugin());
    }

    #[test]
    fn lock_package_exposes_autoload_variants() {
        let lock = Lock::from_bytes(FIXTURE_PACKAGIST_LOCK.as_bytes()).unwrap();
        let monolog = &lock.packages[0];
        let plugin = &lock.packages[1];

        // psr-4: single dir as string.
        let psr4 = monolog.autoload.psr_4.get("Monolog\\").expect("psr-4 entry");
        assert_eq!(psr4.as_str(), Some("src/Monolog/"));

        // psr-0 + classmap + files + exclude-from-classmap, all on
        // the same package (the plugin entry exercises every shape).
        assert!(plugin.autoload.psr_0.contains_key("Acme\\Plugin\\"));
        assert_eq!(plugin.autoload.classmap, vec!["compat/"]);
        assert_eq!(plugin.autoload.files, vec!["bootstrap.php"]);
        assert_eq!(plugin.autoload.exclude_from_classmap, vec!["compat/legacy/"]);
        assert_eq!(plugin.bin, vec!["bin/acme-plugin"]);
    }

    #[test]
    fn lock_package_detects_composer_plugin_type() {
        let lock = Lock::from_bytes(FIXTURE_PACKAGIST_LOCK.as_bytes()).unwrap();
        let plugin = &lock.packages[1];
        assert_eq!(plugin.package_type.as_deref(), Some("composer-plugin"));
        assert!(plugin.is_composer_plugin());
    }

    #[test]
    fn lock_package_captures_replace_and_provide() {
        let lock = Lock::from_bytes(FIXTURE_PACKAGIST_LOCK.as_bytes()).unwrap();
        let monolog = &lock.packages[0];
        assert_eq!(
            monolog.provide.get("psr/log-implementation").map(String::as_str),
            Some("3.0.0")
        );
        assert!(monolog.replace.is_empty());
        assert!(monolog.conflict.is_empty());
    }

    #[test]
    fn lock_all_packages_iterates_runtime_then_dev() {
        let lock = Lock::from_bytes(FIXTURE_PACKAGIST_LOCK.as_bytes()).unwrap();
        let names: Vec<&str> = lock.all_packages().map(|p| p.name.as_str()).collect();
        assert_eq!(names, vec!["monolog/monolog", "acme/plugin", "phpunit/phpunit"]);
    }

    #[test]
    fn lock_tolerates_minimal_lockfile() {
        // Older / sparser lockfiles may omit nearly everything except
        // packages + content-hash. The reader must not error on the
        // absence of optional fields.
        let minimal = br#"{
            "content-hash": "deadbeefdeadbeefdeadbeefdeadbeef",
            "packages": [
                {"name": "acme/lean", "version": "0.1.0"}
            ]
        }"#;
        let lock = Lock::from_bytes(minimal).unwrap();
        assert_eq!(lock.packages.len(), 1);
        let p = &lock.packages[0];
        assert!(p.dist.is_none());
        assert!(p.source.is_none());
        assert!(p.require.is_empty());
        assert!(p.autoload.psr_4.is_empty());
        assert_eq!(p.package_type, None);
        // Defaults on top-level booleans.
        assert!(!lock.prefer_stable);
        assert!(!lock.prefer_lowest);
        assert!(lock.platform.is_empty());
    }

    #[test]
    fn lock_path_dist_round_trips_from_autoloader_fixture() {
        // The bougie-autoloader fixtures use `dist.type: "path"`
        // entries (local-path repositories). Our reader must parse
        // them too — exercising the same files the autoloader works
        // against keeps the two crates honest about a shared schema.
        let bytes = include_bytes!(
            "../../bougie-autoloader/tests/fixtures/psr4-single/input/composer.lock"
        );
        let lock = Lock::from_bytes(bytes).unwrap();
        assert_eq!(lock.packages.len(), 1);
        let p = &lock.packages[0];
        assert_eq!(p.name, "acme/lib");
        assert!(p.is_path_dist());
        let dist = p.dist.as_ref().unwrap();
        assert_eq!(dist.kind, "path");
        assert!(dist.shasum.is_none());
        // psr-4 entry round-trips with the string form Composer
        // writes for a single directory.
        assert_eq!(
            p.autoload.psr_4.get("Acme\\Lib\\").and_then(|v| v.as_str()),
            Some("src/")
        );
    }

    #[test]
    fn lock_read_reads_from_disk() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("composer.lock");
        std::fs::write(&path, FIXTURE_PACKAGIST_LOCK).unwrap();
        let lock = Lock::read(&path).unwrap();
        assert_eq!(lock.packages.len(), 2);
    }

    #[test]
    fn lock_rejects_invalid_json_with_filename_context() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("composer.lock");
        std::fs::write(&path, b"not json").unwrap();
        let err = Lock::read(&path).expect_err("must error");
        let msg = format!("{err:#}");
        assert!(msg.contains("composer.lock"), "error must name the file: {msg}");
    }

    #[test]
    fn write_lock_round_trips_through_read() {
        // Build a small Lock, serialize it, read it back, assert
        // structural equivalence. Catches obvious renames /
        // missing-fields issues in the new Serialize derives.
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("composer.lock");
        let original = Lock {
            readme: canonical_readme(),
            content_hash: Some("0123456789abcdef0123456789abcdef".into()),
            packages: vec![LockPackage {
                name: "acme/foo".into(),
                description: None,
                version: "1.2.3".into(),
                version_normalized: Some("1.2.3.0".into()),
                dist: Some(LockDist {
                    kind: "zip".into(),
                    url: "https://example.test/acme-foo.zip".into(),
                    shasum: Some(
                        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into(),
                    ),
                    reference: Some("abc1234".into()),
                    mirrors: Vec::new(),
                    transport_options: serde_json::Value::default(),
                }),
                source: None,
                transport_options: Value::Null,
                require: BTreeMap::from([("php".into(), ">=8.1".into())]),
                require_dev: BTreeMap::new(),
                package_type: Some("library".into()),
                autoload: LockAutoload::default(),
                autoload_dev: Value::Null,
                replace: BTreeMap::new(),
                provide: BTreeMap::new(),
                conflict: BTreeMap::new(),
                bin: vec![],
                extra: Value::Null,
                time: Some("2024-01-01T00:00:00+00:00".into()),
                license: vec![],
                funding: vec![],
            }],
            packages_dev: vec![],
            aliases: vec![],
            minimum_stability: Some("stable".into()),
            stability_flags: BTreeMap::new(),
            prefer_stable: false,
            prefer_lowest: false,
            platform: BTreeMap::new(),
            platform_dev: BTreeMap::new(),
            platform_overrides: BTreeMap::new(),
            plugin_api_version: Some("2.6.0".into()),
        };
        // Sanity: stability_flags should serialize as `{}` even when
        // empty (Composer expects the key present).
        write_lock(&path, &original).unwrap();
        let round_tripped = Lock::read(&path).unwrap();

        // Spot-check fields that round-trip through the rename
        // attributes — those are the easy ones to typo.
        assert_eq!(
            round_tripped.content_hash,
            Some("0123456789abcdef0123456789abcdef".into()),
        );
        assert_eq!(round_tripped.readme.len(), 3);
        assert_eq!(round_tripped.minimum_stability.as_deref(), Some("stable"));
        assert_eq!(round_tripped.plugin_api_version.as_deref(), Some("2.6.0"));
        assert_eq!(round_tripped.packages.len(), 1);
        let p = &round_tripped.packages[0];
        assert_eq!(p.name, "acme/foo");
        assert_eq!(p.version, "1.2.3");
        assert_eq!(p.version_normalized.as_deref(), Some("1.2.3.0"));
        let dist = p.dist.as_ref().unwrap();
        assert_eq!(dist.kind, "zip");
        assert_eq!(dist.url, "https://example.test/acme-foo.zip");
        assert_eq!(p.require.get("php").unwrap(), ">=8.1");
        // And the file itself ends with a newline like Composer
        // expects.
        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(bytes.last(), Some(&b'\n'));

        // Cosmetic check: the readme is the canonical strings, not
        // something we accidentally substituted.
        assert!(original.readme[0].contains("locks the dependencies"));
    }

    #[test]
    fn write_lock_omits_readme_when_empty() {
        // Round-tripping a Lock with an empty readme should not
        // emit an empty `_readme: []` — Composer's older locks
        // (and synthetic minimal locks) don't carry the key at all.
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("composer.lock");
        let lock = Lock {
            readme: vec![],
            content_hash: Some("0123".into()),
            packages: vec![],
            packages_dev: vec![],
            aliases: vec![],
            minimum_stability: None,
            stability_flags: BTreeMap::new(),
            prefer_stable: false,
            prefer_lowest: false,
            platform: BTreeMap::new(),
            platform_dev: BTreeMap::new(),
            platform_overrides: BTreeMap::new(),
            plugin_api_version: None,
        };
        write_lock(&path, &lock).unwrap();
        let body = std::fs::read_to_string(&path).unwrap();
        assert!(!body.contains("_readme"), "{body}");
    }

    /// PHP serializes an empty associative array as `[]`, not `{}`,
    /// so older Composer-produced composer.lock files (and
    /// hand-written ones via PHP code) may carry `"platform-dev": []`
    /// or `"require": []` instead of the JSON-object form. Our
    /// reader must accept both.
    #[test]
    fn reads_lock_with_php_empty_array_for_empty_maps() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("composer.lock");
        // Top-level `platform-dev` and a package-level `require` /
        // `replace` / `provide` / `conflict` / `require-dev` all
        // exercised as `[]`.
        let body = r#"{
            "content-hash": "0123456789abcdef0123456789abcdef",
            "packages": [
                {
                    "name": "acme/foo",
                    "version": "1.2.3",
                    "version_normalized": "1.2.3.0",
                    "dist": {"type":"zip","url":"https://e/a","shasum":"aa"},
                    "require": [],
                    "require-dev": [],
                    "replace": [],
                    "provide": [],
                    "conflict": []
                }
            ],
            "packages-dev": [],
            "minimum-stability": "stable",
            "stability-flags": {},
            "prefer-stable": false,
            "prefer-lowest": false,
            "platform": [],
            "platform-dev": []
        }"#;
        std::fs::write(&path, body).unwrap();
        let lock = Lock::read(&path).expect("lock with [] empty-maps must parse");
        assert_eq!(lock.packages.len(), 1);
        assert!(lock.platform.is_empty());
        assert!(lock.platform_dev.is_empty());
        assert!(lock.packages[0].require.is_empty());
        assert!(lock.packages[0].replace.is_empty());
    }

    #[test]
    fn autoload_lists_mirror_composers_array_cast() {
        // Reproduces the wire shape Packagist returns for
        // amphp/process v0.1.3:
        // `autoload.classmap = [{"Amp\\Process": "Process.php"}]`.
        // Composer's `(array) $paths` cast extracts the object's
        // values into the classmap, so the autoloader ends up
        // scanning `Process.php`. Our lenient deserializer must
        // produce the same flattened list — that way an autoload
        // built from the resulting LockPackage matches what
        // Composer would write.
        let entry: LockPackage = serde_json::from_str(
            r#"{
                "name": "amphp/process",
                "version": "v0.1.3",
                "version_normalized": "0.1.3.0",
                "type": "library",
                "autoload": {
                    "classmap": [
                        "lib/",
                        {"Amp\\Process": "Process.php", "Amp\\Other": "Other.php"},
                        "extras/"
                    ],
                    "files": ["bootstrap.php", {"key": "init.php"}, 42],
                    "exclude-from-classmap": ["legacy/", 42, null]
                },
                "bin": ["bin/run", {"label": "bin/other"}]
            }"#,
        )
        .expect("entry with non-string list items must deserialize");
        assert_eq!(
            entry.autoload.classmap,
            vec!["lib/", "Process.php", "Other.php", "extras/"]
        );
        assert_eq!(entry.autoload.files, vec!["bootstrap.php", "init.php"]);
        assert_eq!(entry.autoload.exclude_from_classmap, vec!["legacy/"]);
        assert_eq!(entry.bin, vec!["bin/run", "bin/other"]);
    }

    /// A LockPackage shaped like a Private-Packagist entry: the dist
    /// URL points at the customer's origin VCS host, the mirror
    /// template at the Packagist host.
    fn mirrored_package(preferred: bool) -> LockPackage {
        serde_json::from_value(serde_json::json!({
            "name": "hyva-themes/commerce-module-cms",
            "version": "1.2.0",
            "version_normalized": "1.2.0.0",
            "dist": {
                "type": "zip",
                "url": "https://gitlab.example.io/api/v4/projects/x%2Fy/repository/archive.zip?sha=54423c75",
                "reference": "54423c75ea9ee3601042882dc089fac99933cdbd",
                "shasum": "",
                "mirrors": [
                    {
                        "url": "https://repo.example.com/dists/%package%/%version%/r%reference%.%type%",
                        "preferred": preferred
                    }
                ]
            }
        }))
        .unwrap()
    }

    #[test]
    fn dist_urls_puts_preferred_mirror_first() {
        // Composer's `Package::getUrls`: preferred mirrors are
        // unshifted ahead of the dist's own URL, so the downloader
        // hits the (authenticated) mirror host before the origin VCS
        // host — that ordering is what makes Private Packagist
        // installs work without VCS credentials.
        let urls = mirrored_package(true).dist_urls();
        assert_eq!(
            urls,
            vec![
                "https://repo.example.com/dists/hyva-themes/commerce-module-cms/1.2.0.0/r54423c75ea9ee3601042882dc089fac99933cdbd.zip",
                "https://gitlab.example.io/api/v4/projects/x%2Fy/repository/archive.zip?sha=54423c75",
            ],
        );
    }

    #[test]
    fn dist_urls_appends_non_preferred_mirror() {
        let urls = mirrored_package(false).dist_urls();
        assert_eq!(
            urls[0],
            "https://gitlab.example.io/api/v4/projects/x%2Fy/repository/archive.zip?sha=54423c75",
        );
        assert!(urls[1].starts_with("https://repo.example.com/dists/"));
    }

    #[test]
    fn dist_urls_renormalizes_version_when_lock_lacks_version_normalized() {
        // Composer's own Locker strips `version_normalized` from the
        // lock, so a Composer-written lock exercises the fallback:
        // re-normalize the pretty version. `%version%` must come out
        // as the 4-segment form (`1.2.0.0`), not `1.2.0` — Private
        // Packagist's dist paths 403 on the pretty form.
        let mut pkg = mirrored_package(true);
        pkg.version_normalized = None;
        let urls = pkg.dist_urls();
        assert!(
            urls[0].contains("/1.2.0.0/"),
            "expected normalized version in mirror URL, got {}",
            urls[0],
        );
    }

    #[test]
    fn dist_urls_without_mirrors_is_the_dist_url_verbatim() {
        let mut pkg = mirrored_package(true);
        pkg.dist.as_mut().unwrap().mirrors.clear();
        // The origin URL's `%2F` escape contains a literal `%` but no
        // placeholder token — it must pass through untouched.
        assert_eq!(
            pkg.dist_urls(),
            vec!["https://gitlab.example.io/api/v4/projects/x%2Fy/repository/archive.zip?sha=54423c75"],
        );
    }

    #[test]
    fn process_mirror_url_hashes_non_hex_reference_and_slashed_version() {
        // ComposerMirror::processUrl: a reference that isn't lowercase
        // hex (e.g. a path-repo config hash label) and a version
        // containing `/` (branch `dev-feature/x`) are md5'd so they
        // can't break the URL path.
        let url = process_mirror_url(
            "https://m.test/%package%/%version%/%reference%.%type%",
            "acme/foo",
            "dev-feature/x",
            "dev-feature/x",
            Some("not-hex!"),
            "zip",
        );
        assert_eq!(
            url,
            format!(
                "https://m.test/acme/foo/{}/{}.zip",
                md5_hex("dev-feature/x"),
                md5_hex("not-hex!"),
            ),
        );
        // ...while a real git sha and a plain version pass through.
        let url = process_mirror_url(
            "https://m.test/%package%/%version%/r%reference%.%type%",
            "acme/foo",
            "1.0.0.0",
            "1.0.0",
            Some("abc123"),
            "zip",
        );
        assert_eq!(url, "https://m.test/acme/foo/1.0.0.0/rabc123.zip");
    }

    #[test]
    fn dist_mirrors_round_trip_through_lock_serialization() {
        // `mirrors` must survive a write → read cycle (that's how the
        // install command learns them without re-probing the repo)
        // and must NOT appear on dists that have none, keeping
        // mirror-less lockfiles byte-identical to before.
        let pkg = mirrored_package(true);
        let json = serde_json::to_value(&pkg).unwrap();
        assert_eq!(
            json["dist"]["mirrors"][0]["url"],
            "https://repo.example.com/dists/%package%/%version%/r%reference%.%type%",
        );
        assert_eq!(json["dist"]["mirrors"][0]["preferred"], true);
        let back: LockPackage = serde_json::from_value(json).unwrap();
        assert_eq!(back.dist.unwrap().mirrors, pkg.dist.unwrap().mirrors);

        let mut plain = mirrored_package(true);
        plain.dist.as_mut().unwrap().mirrors.clear();
        let json = serde_json::to_value(&plain).unwrap();
        assert!(
            json["dist"].get("mirrors").is_none(),
            "empty mirrors must be suppressed from lock output",
        );
    }
}
