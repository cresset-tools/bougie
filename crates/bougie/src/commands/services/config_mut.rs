//! Shared helpers for the offline `bougie services {add,remove}`
//! mutations on `composer.json` / `bougie.toml`.

use bougie_composer::lockfile::{read_json_file, write_json_file};
use eyre::{eyre, Result, WrapErr};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

/// Which config file the mutation should target.
#[derive(Debug, Clone)]
pub enum ConfigTarget {
    /// `<project>/composer.json`'s `extra.bougie.services`.
    Composer(PathBuf),
    /// `<project>/bougie.toml`'s `[services]` table.
    Toml(PathBuf),
}

/// Walk up from cwd looking for the project root. Order of search:
/// `bougie.toml`, `composer.json`, `.bougie/`. The first match wins.
pub fn locate_project_root() -> Result<PathBuf> {
    let cwd = std::env::current_dir().wrap_err("reading cwd")?;
    for anc in cwd.ancestors() {
        if anc.join("bougie.toml").is_file()
            || anc.join("composer.json").is_file()
            || anc.join(".bougie").is_dir()
        {
            return Ok(anc.to_path_buf());
        }
    }
    Err(eyre!(
        "no bougie project found (no `composer.json`, `bougie.toml`, or `.bougie/` in {} or any parent)",
        cwd.display()
    ))
}

/// Pick which file to mutate. If `bougie.toml` exists in the project,
/// that's where the user opted to keep config (see
/// [feedback-dual-config-source]); otherwise edit composer.json. If
/// neither exists, we create composer.json with an empty skeleton.
pub fn choose_config_target(project_root: &Path) -> Result<ConfigTarget> {
    let toml = project_root.join("bougie.toml");
    if toml.is_file() {
        return Ok(ConfigTarget::Toml(toml));
    }
    Ok(ConfigTarget::Composer(project_root.join("composer.json")))
}

/// Add a service pin. Returns `true` if a new entry was created;
/// `false` if the entry was already present with the same pin (idempotent).
pub fn add_service(target: &ConfigTarget, name: &str, version: &str) -> Result<bool> {
    match target {
        ConfigTarget::Composer(path) => add_to_composer_json(path, name, version),
        ConfigTarget::Toml(path) => add_to_bougie_toml(path, name, version),
    }
}

/// Remove a service pin. Returns `true` if an entry was actually removed.
pub fn remove_service(target: &ConfigTarget, name: &str) -> Result<bool> {
    match target {
        ConfigTarget::Composer(path) => remove_from_composer_json(path, name),
        ConfigTarget::Toml(path) => remove_from_bougie_toml(path, name),
    }
}

// -------------------- composer.json --------------------

fn add_to_composer_json(path: &Path, name: &str, version: &str) -> Result<bool> {
    let mut v = read_or_init_composer_json(path)?;
    let services = ensure_extra_bougie_services(&mut v);
    let map = services
        .as_object_mut()
        .ok_or_else(|| eyre!("extra.bougie.services in {} is not an object", path.display()))?;
    let new_value = Value::String(version.into());
    let was_new = match map.get(name) {
        Some(existing) if existing == &new_value => return Ok(false),
        _ => !map.contains_key(name),
    };
    map.insert(name.to_string(), new_value);
    write_json_file(path, &v)?;
    Ok(was_new)
}

fn remove_from_composer_json(path: &Path, name: &str) -> Result<bool> {
    if !path.exists() {
        return Ok(false);
    }
    let mut v = read_json_file(path)?;
    let services_present = v
        .get("extra")
        .and_then(|e| e.get("bougie"))
        .and_then(|b| b.get("services"))
        .and_then(Value::as_object)
        .is_some_and(|m| m.contains_key(name));
    if !services_present {
        return Ok(false);
    }
    if let Some(map) = v
        .get_mut("extra")
        .and_then(|e| e.get_mut("bougie"))
        .and_then(|b| b.get_mut("services"))
        .and_then(Value::as_object_mut)
    {
        map.remove(name);
    }
    write_json_file(path, &v)?;
    Ok(true)
}

fn read_or_init_composer_json(path: &Path) -> Result<Value> {
    if path.exists() {
        read_json_file(path)
    } else {
        Ok(json!({}))
    }
}

/// Drill into `extra.bougie.services`, creating empty objects at every
/// level that's missing. Returns a `&mut` to the services object.
fn ensure_extra_bougie_services(v: &mut Value) -> &mut Value {
    if !v.is_object() {
        *v = json!({});
    }
    let root = v.as_object_mut().expect("just made it an object");
    let extra = root
        .entry("extra")
        .or_insert_with(|| json!({}));
    if !extra.is_object() {
        *extra = json!({});
    }
    let extra = extra.as_object_mut().expect("just made it an object");
    let bougie = extra.entry("bougie").or_insert_with(|| json!({}));
    if !bougie.is_object() {
        *bougie = json!({});
    }
    let bougie = bougie.as_object_mut().expect("just made it an object");
    let services = bougie.entry("services").or_insert_with(|| json!({}));
    if !services.is_object() {
        *services = json!({});
    }
    services
}

// -------------------- bougie.toml --------------------

fn add_to_bougie_toml(path: &Path, name: &str, version: &str) -> Result<bool> {
    let text = std::fs::read_to_string(path)
        .wrap_err_with(|| format!("reading {}", path.display()))?;
    let mut doc: toml_edit::DocumentMut = text
        .parse()
        .wrap_err_with(|| format!("parsing {} as TOML", path.display()))?;

    // Ensure `[services]` table exists.
    if !doc.contains_table("services") {
        doc["services"] = toml_edit::Item::Table(toml_edit::Table::new());
    }
    let services = doc["services"]
        .as_table_mut()
        .ok_or_else(|| eyre!("`services` in {} is not a table", path.display()))?;

    // Idempotent: if the name already has the same bare-string pin, no
    // write. (Detail-form entries are always overwritten — the table
    // form's structural complexity makes "is it identical" not worth
    // the careful compare for a UX detail.)
    let already_same = services
        .get(name)
        .and_then(toml_edit::Item::as_str) == Some(version);
    if already_same {
        return Ok(false);
    }

    let was_new = !services.contains_key(name);
    services[name] = toml_edit::value(version);
    let bytes = doc.to_string();
    atomic_write(path, bytes.as_bytes())?;
    Ok(was_new)
}

fn remove_from_bougie_toml(path: &Path, name: &str) -> Result<bool> {
    if !path.exists() {
        return Ok(false);
    }
    let text = std::fs::read_to_string(path)
        .wrap_err_with(|| format!("reading {}", path.display()))?;
    let mut doc: toml_edit::DocumentMut = text
        .parse()
        .wrap_err_with(|| format!("parsing {} as TOML", path.display()))?;
    let Some(services) = doc.get_mut("services").and_then(toml_edit::Item::as_table_mut) else {
        return Ok(false);
    };
    if services.remove(name).is_none() {
        return Ok(false);
    }
    atomic_write(path, doc.to_string().as_bytes())?;
    Ok(true)
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write;
    let parent = path
        .parent()
        .ok_or_else(|| eyre!("path {} has no parent", path.display()))?;
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
