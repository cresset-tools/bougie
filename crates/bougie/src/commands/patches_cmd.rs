//! `bougie patches {add,list,import,repatch,relock,doctor}` — the user-facing
//! patch management commands.
//!
//! The lifecycle bridge ([`super::patches`]) owns resolution/materialization;
//! this module is the human surface: inspecting the resolved set, authoring
//! root rules, adopting dependency patches, and forcing re-application. State
//! lives in `composer.json` (`extra.patches`) and `patches.lock.json`.

use std::collections::BTreeMap;
use std::path::Path;
use std::process::ExitCode;

use bougie_cli::{OutputFormat, PatchesCommand, PhpPrefArgs};
use bougie_config::load_project;
use bougie_paths::Paths;
use bougie_patches::lock;
use bougie_patches::model::{DepthSpec, PatchSource};
use eyre::{Result, WrapErr, bail};
use serde_json::{Value, json};

use super::patches::{self, DependencyPatches};

pub fn run(format: OutputFormat, cmd: PatchesCommand) -> Result<ExitCode> {
    match cmd {
        PatchesCommand::List => list(format),
        PatchesCommand::Doctor => doctor(format),
        PatchesCommand::Repatch { packages } => repatch(format, &packages),
        PatchesCommand::Relock => relock(format),
        PatchesCommand::Add {
            source,
            package,
            description,
            depth,
            to_file,
            no_sync,
        } => add(format, &source, package, description, depth, to_file, no_sync),
        PatchesCommand::Import {
            packages,
            all,
            to_file,
        } => import(&packages, all, to_file),
    }
}

// ---------------------------------------------------------------- list

fn list(format: OutputFormat) -> Result<ExitCode> {
    let project_root = std::env::current_dir()?;
    let project = load_project(&project_root)?;
    let value = read_composer_value(&project_root);
    let resolved = patches::resolve_all(&project_root, &project, &value)?;
    let deps = patches::dependency_patches(&project_root);

    if format == OutputFormat::JsonV1 {
        let applied = lock::read(&project_root);
        let out = json!({
            "resolved": resolved.iter().map(patch_to_json).collect::<Vec<_>>(),
            "applied": applied,
            "dependency_declared": deps.iter().map(dep_to_json).collect::<Vec<_>>(),
        });
        println!("{}", serde_json::to_string_pretty(&out)?);
        return Ok(ExitCode::SUCCESS);
    }

    if resolved.is_empty() {
        println!("No root patches declared.");
    } else {
        println!("Resolved patches:");
        for (target, items) in group_by_target(&resolved) {
            println!("  {target}");
            for p in items {
                println!("    - {} ({})", p.description, source_str(&p.source));
            }
        }
    }

    if !deps.is_empty() {
        println!(
            "\nDependency-declared patches (NOT applied — run `bougie patches import` to adopt):"
        );
        for dep in &deps {
            for p in &dep.patches {
                println!(
                    "  {} → {} ({}) from {}@{}",
                    p.target,
                    p.description,
                    source_str(&p.source),
                    dep.dependency,
                    dep.version
                );
            }
        }
    }
    Ok(ExitCode::SUCCESS)
}

// ---------------------------------------------------------------- doctor

fn doctor(format: OutputFormat) -> Result<ExitCode> {
    let project_root = std::env::current_dir()?;
    let project = load_project(&project_root)?;
    let value = read_composer_value(&project_root);

    let mut problems: Vec<String> = Vec::new();

    // Resolution itself surfaces unresolvable `patches/` files as errors.
    match patches::resolve_all(&project_root, &project, &value) {
        Ok(resolved) => {
            for p in &resolved {
                if let PatchSource::Remote(url) = &p.source {
                    if url.starts_with("http://") {
                        problems.push(format!(
                            "{}: insecure http:// patch URL `{url}` (use https)",
                            p.target
                        ));
                    }
                    if p.sha256.is_none() {
                        problems.push(format!(
                            "{}: remote patch `{}` has no sha256 (trust-on-first-use)",
                            p.target, p.description
                        ));
                    }
                }
            }
        }
        Err(e) => problems.push(format!("resolution error: {e:#}")),
    }

    let deps = patches::dependency_patches(&project_root);
    for dep in &deps {
        problems.push(format!(
            "{} declares {} patch(es) that bougie does not apply (run `bougie patches import {}`)",
            dep.dependency,
            dep.patches.len(),
            dep.dependency
        ));
    }

    if format == OutputFormat::JsonV1 {
        println!("{}", serde_json::to_string_pretty(&json!({ "problems": problems }))?);
        return Ok(ExitCode::SUCCESS);
    }

    if problems.is_empty() {
        println!("patches: no problems found.");
    } else {
        println!("patches doctor found {} issue(s):", problems.len());
        for p in &problems {
            println!("  - {p}");
        }
    }
    Ok(ExitCode::SUCCESS)
}

// ---------------------------------------------------------------- repatch / relock

fn repatch(format: OutputFormat, packages: &[String]) -> Result<ExitCode> {
    let project_root = std::env::current_dir()?;
    let mut fps = lock::read(&project_root);
    if packages.is_empty() {
        fps.clear();
    } else {
        for name in packages {
            fps.remove(name);
        }
    }
    lock::write(&project_root, &fps).wrap_err("rewriting patches.lock.json")?;
    // A fresh sync re-extracts the dropped packages pristine and re-applies.
    resync(format)
}

fn relock(format: OutputFormat) -> Result<ExitCode> {
    let project_root = std::env::current_dir()?;
    // Drop every fingerprint and the cached patch downloads, forcing a full
    // re-download + re-extract + re-apply, then a fresh lock.
    lock::write(&project_root, &BTreeMap::new()).wrap_err("clearing patches.lock.json")?;
    if let Ok(paths) = Paths::from_env() {
        let cache = paths.cache().join("patches");
        let _ = std::fs::remove_dir_all(&cache);
    }
    resync(format)
}

fn resync(format: OutputFormat) -> Result<ExitCode> {
    super::sync::run(
        format,
        false,
        false,
        None,
        Some(true),
        PhpPrefArgs::default(),
        bougie_composer_resolver::ResolutionStrategy::Highest,
    )
}

// ---------------------------------------------------------------- add

#[allow(clippy::too_many_arguments)]
fn add(
    format: OutputFormat,
    source: &str,
    package: Option<String>,
    description: Option<String>,
    depth: Option<usize>,
    to_file: bool,
    no_sync: bool,
) -> Result<ExitCode> {
    let project_root = std::env::current_dir()?;
    let is_url = source.starts_with("http://") || source.starts_with("https://");

    // Materialize the bytes to infer a target and (for URLs) capture sha256.
    let (bytes, origin, sha256) = if is_url {
        let paths = Paths::from_env()?;
        let cache = paths.cache().join("patches");
        let dest = patches::download_for_add(&cache, source)?;
        let bytes = std::fs::read(&dest)?;
        let sha = bougie_patches::content_sha256(&bytes);
        (bytes, source.to_string(), Some(sha))
    } else {
        let abs = project_root.join(source);
        if !abs.starts_with(&project_root) {
            bail!(
                "local patch `{source}` is outside the project — drop it in the `patches/` \
                 directory instead, which is portable"
            );
        }
        let bytes = std::fs::read(&abs)
            .wrap_err_with(|| format!("reading patch file `{}`", abs.display()))?;
        // Record the path relative to the project root (cweagans stores a
        // local path as the entry's url).
        let rel = abs
            .strip_prefix(&project_root)
            .unwrap_or(&abs)
            .to_string_lossy()
            .replace('\\', "/");
        (bytes, rel, None)
    };

    let target = match package {
        Some(p) => p,
        None => infer_target_for(&project_root, &bytes)?,
    };
    let description = description.unwrap_or_else(|| basename(&origin));

    let mut entry = serde_json::Map::new();
    entry.insert("description".into(), Value::String(description.clone()));
    entry.insert("url".into(), Value::String(origin.clone()));
    if let Some(sha) = sha256 {
        entry.insert("sha256".into(), Value::String(sha));
    }
    if let Some(d) = depth {
        entry.insert("depth".into(), json!(d));
    }

    write_patch_entry(&project_root, &target, Value::Object(entry), to_file)?;
    println!("Added patch for {target}: {description}");

    if no_sync {
        Ok(ExitCode::SUCCESS)
    } else {
        resync(format)
    }
}

// ---------------------------------------------------------------- import

fn import(packages: &[String], all: bool, to_file: bool) -> Result<ExitCode> {
    let project_root = std::env::current_dir()?;
    let deps = patches::dependency_patches(&project_root);
    if deps.is_empty() {
        println!("No dependency declares patches; nothing to import.");
        return Ok(ExitCode::SUCCESS);
    }

    let today = today_iso();
    let mut imported = 0usize;
    for dep in &deps {
        if !all && !packages.contains(&dep.dependency) {
            continue;
        }
        for p in &dep.patches {
            let mut entry = serde_json::Map::new();
            entry.insert("description".into(), Value::String(p.description.clone()));
            entry.insert("url".into(), Value::String(source_str(&p.source)));
            if let Some(sha) = &p.sha256 {
                entry.insert("sha256".into(), Value::String(sha.clone()));
            }
            if let DepthSpec::Fixed(d) = p.depth {
                entry.insert("depth".into(), json!(d));
            }
            // Provenance: where this patch came from, so a reader can audit it.
            entry.insert(
                "extra".into(),
                json!({
                    "imported-from": {
                        "package": dep.dependency,
                        "version": dep.version,
                        "imported-at": today,
                        "by": "bougie patches import",
                    }
                }),
            );
            write_patch_entry(&project_root, &p.target, Value::Object(entry), to_file)?;
            imported += 1;
        }
    }

    if imported == 0 {
        println!("No matching dependency patches to import.");
    } else {
        println!(
            "Imported {imported} patch(es) into the root. Run `bougie sync` to apply them."
        );
    }
    Ok(ExitCode::SUCCESS)
}

// ---------------------------------------------------------------- helpers

fn infer_target_for(project_root: &Path, patch_bytes: &[u8]) -> Result<String> {
    let text = std::str::from_utf8(patch_bytes)
        .wrap_err("patch is not valid UTF-8; pass --package")?;
    let files = bougie_patches::diff::split(text)?;
    let header_paths: Vec<&str> = files
        .iter()
        .filter_map(bougie_patches::diff::FileDiff::routed_path)
        .collect();
    let value = read_composer_value(project_root);
    let install_paths = patches::lock_install_paths(project_root, &value);
    let inferred = bougie_patches::infer_target(&header_paths, &install_paths)
        .wrap_err("could not infer target package; pass --package")?;
    Ok(inferred.package)
}

/// Read `composer.json` as a JSON value, or `Null` if absent/unparseable.
fn read_composer_value(project_root: &Path) -> Value {
    std::fs::read(project_root.join("composer.json"))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or(Value::Null)
}

/// Append an expanded patch entry under `extra.patches[target]` in either
/// `composer.json` or the external patches file, normalizing the target's
/// value to an array first so metadata (sha256/depth/extra) can be carried.
fn write_patch_entry(
    project_root: &Path,
    target: &str,
    entry: Value,
    to_file: bool,
) -> Result<()> {
    let path = if to_file {
        patches_file_path(project_root)
    } else {
        project_root.join("composer.json")
    };
    let body = std::fs::read_to_string(&path).unwrap_or_else(|_| "{}".to_string());
    let mut doc: Value = serde_json::from_str(&body)
        .wrap_err_with(|| format!("parsing {}", path.display()))?;

    // Navigate to the patches map: composer.json → extra.patches; patches
    // file → top-level patches.
    let patches_map = if to_file {
        ensure_object(&mut doc)?.entry("patches").or_insert_with(|| json!({}))
    } else {
        let extra = ensure_object(&mut doc)?
            .entry("extra")
            .or_insert_with(|| json!({}));
        ensure_object(extra)?
            .entry("patches")
            .or_insert_with(|| json!({}))
    };

    let map = ensure_object(patches_map)?;
    let slot = map.entry(target.to_string()).or_insert_with(|| json!([]));
    // Normalize a compact object form to an array so we can append.
    if let Value::Object(obj) = slot {
        let arr: Vec<Value> = obj
            .iter()
            .map(|(desc, url)| json!({ "description": desc, "url": url }))
            .collect();
        *slot = Value::Array(arr);
    }
    match slot {
        Value::Array(arr) => arr.push(entry),
        other => *other = Value::Array(vec![entry]),
    }

    let mut s = serde_json::to_string_pretty(&doc).wrap_err("encoding JSON")?;
    s.push('\n');
    std::fs::write(&path, s).wrap_err_with(|| format!("writing {}", path.display()))?;
    Ok(())
}

/// The external patches file path, defaulting to `patches.json`.
fn patches_file_path(project_root: &Path) -> std::path::PathBuf {
    let value = read_composer_value(project_root);
    let name = value
        .pointer("/extra/composer-patches/patches-file")
        .and_then(Value::as_str)
        .or_else(|| value.pointer("/extra/patches-file").and_then(Value::as_str))
        .unwrap_or("patches.json")
        .to_string();
    project_root.join(name)
}

fn ensure_object(v: &mut Value) -> Result<&mut serde_json::Map<String, Value>> {
    if !v.is_object() {
        *v = json!({});
    }
    v.as_object_mut()
        .ok_or_else(|| eyre::eyre!("expected a JSON object"))
}

fn group_by_target(patches: &[bougie_patches::Patch]) -> BTreeMap<&str, Vec<&bougie_patches::Patch>> {
    let mut by_target: BTreeMap<&str, Vec<&bougie_patches::Patch>> = BTreeMap::new();
    for p in patches {
        by_target.entry(&p.target).or_default().push(p);
    }
    by_target
}

fn source_str(source: &PatchSource) -> String {
    match source {
        PatchSource::Local(p) => p.to_string_lossy().replace('\\', "/"),
        PatchSource::Remote(url) => url.clone(),
    }
}

fn basename(s: &str) -> String {
    s.rsplit(['/', '\\']).next().unwrap_or(s).to_string()
}

fn patch_to_json(p: &bougie_patches::Patch) -> Value {
    json!({
        "target": p.target,
        "description": p.description,
        "source": source_str(&p.source),
        "sha256": p.sha256,
        "depth": match p.depth { DepthSpec::Fixed(n) => json!(n), DepthSpec::Auto => Value::Null },
    })
}

fn dep_to_json(d: &DependencyPatches) -> Value {
    json!({
        "dependency": d.dependency,
        "version": d.version,
        "patches": d.patches.iter().map(patch_to_json).collect::<Vec<_>>(),
    })
}

/// Today's date as `YYYY-MM-DD` (UTC), without pulling in a date crate, via
/// Howard Hinnant's days→civil algorithm.
fn today_iso() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let days = i64::try_from(secs / 86_400).unwrap_or(0);
    let (y, m, d) = civil_from_days(days);
    format!("{y:04}-{m:02}-{d:02}")
}

fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = u32::try_from(doy - (153 * mp + 2) / 5 + 1).unwrap_or(1);
    let m = u32::try_from(if mp < 10 { mp + 3 } else { mp - 9 }).unwrap_or(1);
    (if m <= 2 { y + 1 } else { y }, m, d)
}
