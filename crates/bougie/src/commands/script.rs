//! `bougie run --script <file>` — single-file PHP scripts with inline
//! `composer.json` dependencies (uv's PEP 723 / `uv run --script`).
//!
//! The script declares its `php` / `ext-*` / package requires in a
//! `# /// script` … `# ///` comment block (see
//! [`bougie_composer::inline`]). bougie resolves that into a cache-keyed
//! ephemeral environment — a disposable mini-project under
//! `$BOUGIE_CACHE/script-run/<key>/` — then execs PHP on the script with
//! Composer's autoloader prepended via `-d auto_prepend_file`, so the
//! declared classes are available with no `require 'vendor/autoload.php'`
//! boilerplate. A `#!/usr/bin/env -S bougie run --script` shebang makes
//! the file directly executable.
//!
//! The heavy lifting (resolve PHP, install extensions, replicate conf.d,
//! materialize `vendor/`) is the normal project sync pipeline pointed at
//! the cache slot — see [`super::sync::sync_script_slot`].

use bougie_cli::{OutputFormat, PhpPrefArgs};
use bougie_composer::inline::{parse_inline_metadata, replace_block_body};
use bougie_composer_resolver::ResolutionStrategy;
use bougie_fs::lock::ExclusiveGuard;
use bougie_fs::state::{read_project_resolved, read_project_resolved_php_path};
use bougie_installer::conf_d;
use bougie_paths::Paths;
use eyre::{Result, WrapErr, eyre};
use std::fmt::Write as _;
#[cfg(unix)]
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

/// Marker written into a slot once its environment is fully
/// materialized. Distinct from `vendor/autoload.php` so a script with no
/// composer packages (only `php` / `ext-*`) is still recognized as ready
/// on the next run.
const READY_MARKER: &str = ".bougie-script-ready";

/// Matches the per-project composer lock timeout — "are we waiting on
/// another bougie process or a stuck one?".
const LOCK_TIMEOUT: Duration = Duration::from_mins(2);

/// Entry point for `bougie run --script <file> [args...]` (and the
/// shebang it backs). `php_request` is the optional `--php` override;
/// `with` is reserved for ad-hoc deps (not yet wired for scripts).
pub fn run(
    argv: &[String],
    // Reserved for symmetry with `run::run` and a future structured
    // "ran script" result; a script owns its own stdout, so there's
    // nothing for bougie to emit on the happy path today.
    _format: OutputFormat,
    php_request: Option<&str>,
    with: &[String],
    xdebug_flag: bool,
    php_pref: PhpPrefArgs,
) -> Result<ExitCode> {
    let (script_arg, script_args) = argv
        .split_first()
        .ok_or_else(|| eyre!("bougie run --script: missing script path"))?;
    let script_path = std::fs::canonicalize(script_arg)
        .wrap_err_with(|| format!("resolving script path `{script_arg}`"))?;
    let source = std::fs::read_to_string(&script_path)
        .wrap_err_with(|| format!("reading script {}", script_path.display()))?;
    let meta = parse_inline_metadata(&source)
        .wrap_err_with(|| format!("parsing inline metadata in {}", script_path.display()))?
        .ok_or_else(|| {
            eyre!(
                "{} has no `# /// script` inline metadata block; add one or run \
                 it inside a project without `--script`",
                script_path.display()
            )
        })?;

    let paths = Paths::from_env()?;
    // A committed `<script>.lock` sidecar (from `bougie lock --script`)
    // makes the run reproducible: install from it instead of re-resolving.
    let lock_sidecar = sidecar_lock_path(&script_path);
    let lock_bytes = std::fs::read(&lock_sidecar).ok();
    let key = cache_key(&meta.composer_json, php_request, with, lock_bytes.as_deref());
    let slot = paths.cache_script_run_dir(&key);
    let ready = slot.join(READY_MARKER);

    if ready.is_file() {
        // Warm slot: refresh mtime so `bougie cache prune` keeps an
        // actively-used script env, then run straight through.
        touch(&ready);
    } else if let Some(lock) = &lock_bytes {
        // Reproducible path: the inline block (verbatim) must match the
        // lock's content hash, so `--with` is ignored here.
        if !with.is_empty() {
            eprintln!(
                "bougie: ignoring --with because {} pins the dependency set; \
                 edit the block and re-run `bougie lock --script` to change it",
                lock_sidecar.display()
            );
        }
        materialise(&paths, &slot, &meta.composer_json, Some(lock), php_request, php_pref)?;
    } else {
        // Fold any ad-hoc `--with` deps into the inline composer.json
        // before resolving (the inline block always wins on a key clash).
        let composer_json = merge_with(&meta.composer_json, with)
            .wrap_err("merging --with dependencies into the script's composer.json")?;
        materialise(&paths, &slot, &composer_json, None, php_request, php_pref)?;
    }

    exec_script(&paths, &slot, &script_path, script_args, xdebug_flag)
}

/// Fold `--with` entries into the script's inline composer.json `require`
/// map. Each entry is classified syntactically (the resolver hasn't run
/// yet, so we can't consult the PHP build): an entry containing `/` is a
/// composer package (`vendor/pkg`, optionally `@`/`=` constraint); any
/// other entry is an extension (`gd`, `gd=2.1` → `ext-gd`). Existing
/// `require` keys from the inline block take precedence — `--with` only
/// *adds*. Returns the original text unchanged when `with` is empty.
fn merge_with(composer_json: &str, with: &[String]) -> Result<String> {
    if with.is_empty() {
        return Ok(composer_json.to_string());
    }
    let mut doc: serde_json::Value = serde_json::from_str(composer_json)
        .wrap_err("re-parsing the inline composer.json")?;
    let obj = doc
        .as_object_mut()
        .ok_or_else(|| eyre!("inline metadata must be a JSON object"))?;
    let require = obj
        .entry("require")
        .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));
    let require = require
        .as_object_mut()
        .ok_or_else(|| eyre!("`require` in the inline block must be an object"))?;
    for spec in with {
        let (key, version) = classify_with(spec);
        require
            .entry(key)
            .or_insert(serde_json::Value::String(version));
    }
    serde_json::to_string_pretty(&doc).wrap_err("re-serializing the merged composer.json")
}

/// Classify a `--with` token into a `(require-key, version)` pair.
/// `vendor/pkg`, `vendor/pkg@^1`, `vendor/pkg=^1` → a package require;
/// `gd`, `gd=2.1` → `ext-gd` (the version value is ignored for an
/// extension require, so `*` is fine).
fn classify_with(spec: &str) -> (String, String) {
    if spec.contains('/') {
        let (name, version) = spec
            .split_once('@')
            .or_else(|| spec.split_once('='))
            .unwrap_or((spec, "*"));
        (name.to_string(), version.to_string())
    } else {
        let name = spec.split_once('=').map_or(spec, |(n, _)| n);
        (format!("ext-{name}"), "*".to_string())
    }
}

/// Build (or rebuild) the ephemeral env at `slot` from the script's
/// inline `composer.json`. Holds an exclusive lock so two concurrent
/// runs of the same script can't race on the slot.
fn materialise(
    paths: &Paths,
    slot: &Path,
    composer_json: &str,
    lock: Option<&[u8]>,
    php_request: Option<&str>,
    php_pref: PhpPrefArgs,
) -> Result<()> {
    std::fs::create_dir_all(slot)
        .wrap_err_with(|| format!("creating script env {}", slot.display()))?;
    let _guard = ExclusiveGuard::acquire(&slot.join(".lock"), LOCK_TIMEOUT).wrap_err_with(
        || format!("acquiring lock on {} (another `bougie run --script`?)", slot.display()),
    )?;
    // Re-check under the lock: a concurrent run may have just finished.
    if slot.join(READY_MARKER).is_file() {
        return Ok(());
    }

    std::fs::write(slot.join("composer.json"), composer_json)
        .wrap_err("writing the script's composer.json")?;
    // Seed a committed sidecar lock so sync installs from it (skipping
    // re-resolution) — the reproducible/offline path.
    if let Some(lock) = lock {
        std::fs::write(slot.join("composer.lock"), lock)
            .wrap_err("writing the script's committed composer.lock")?;
    }

    let request = match php_request {
        Some(s) => Some(
            bougie_version::request::parse_request(s)
                .wrap_err_with(|| format!("parsing --php {s:?}"))?,
        ),
        None => None,
    };

    eprintln!("bougie: preparing script environment…");
    super::sync::sync_script_slot(paths, slot, request.as_ref(), php_pref)
        .wrap_err("preparing the script's dependency environment")?;

    std::fs::write(slot.join(READY_MARKER), b"")
        .wrap_err("writing the script-env ready marker")?;
    Ok(())
}

/// Exec the resolved PHP on the script: prepend the autoloader, scope
/// extensions via `PHP_INI_SCAN_DIR`, forward the user's args. On Unix
/// this `execve`s and never returns on success.
fn exec_script(
    paths: &Paths,
    slot: &Path,
    script_path: &Path,
    args: &[String],
    xdebug_flag: bool,
) -> Result<ExitCode> {
    let php_bin = resolve_php_bin(paths, slot)?;

    let env_session_set =
        std::env::var_os("XDEBUG_SESSION").is_some_and(|v| !v.is_empty());
    let debug_overlay = xdebug_flag || env_session_set;
    let scan_dir = conf_d::php_ini_scan_dir(paths, slot, debug_overlay);

    let mut cmd = std::process::Command::new(&php_bin);
    // Prepend Composer's autoloader so the script's declared classes load
    // with no `require 'vendor/autoload.php'` line — the "env is active"
    // feel. Skipped when the script declared no composer packages (no
    // autoloader was generated); the Composer autoloader is idempotent,
    // so a script that *also* requires it is unharmed.
    let autoload = slot.join("vendor").join("autoload.php");
    if autoload.is_file() {
        let mut directive = std::ffi::OsString::from("auto_prepend_file=");
        directive.push(autoload.as_os_str());
        cmd.arg("-d").arg(directive);
    }
    cmd.arg(script_path).args(args);
    cmd.env("PHP_INI_SCAN_DIR", &scan_dir)
        .env("BOUGIE_SCRIPT", script_path);
    if xdebug_flag && !env_session_set {
        cmd.env("XDEBUG_SESSION", "1");
    }

    #[cfg(unix)]
    {
        // execve replaces this process; the only return is an error.
        let err = cmd.exec();
        Err(eyre!("exec {}: {err}", php_bin.display()))
    }
    #[cfg(not(unix))]
    {
        let status = cmd
            .status()
            .wrap_err_with(|| format!("spawning {}", php_bin.display()))?;
        let code = u8::try_from(status.code().unwrap_or(1)).unwrap_or(1);
        Ok(ExitCode::from(code))
    }
}

/// Resolve the PHP binary the slot's sync settled on: a system PHP path
/// if one was wired (`--php /path`), else the managed install tree.
fn resolve_php_bin(paths: &Paths, slot: &Path) -> Result<PathBuf> {
    if let Some(system) = read_project_resolved_php_path(slot) {
        return Ok(system);
    }
    let (version, flavor) = read_project_resolved(slot).wrap_err_with(|| {
        format!("reading the resolved PHP marker for script env {}", slot.display())
    })?;
    let bin = paths
        .installs()
        .join(format!("{version}-{flavor}"))
        .join("bin")
        .join(php_exe_name());
    Ok(bin)
}

#[cfg(windows)]
fn php_exe_name() -> &'static str {
    "php.exe"
}
#[cfg(not(windows))]
fn php_exe_name() -> &'static str {
    "php"
}

/// `<script>.lock` sidecar path (uv's `script.py.lock`): the script path
/// with `.lock` appended — `app.php` → `app.php.lock`.
fn sidecar_lock_path(script_path: &Path) -> PathBuf {
    let mut p = script_path.as_os_str().to_owned();
    p.push(".lock");
    PathBuf::from(p)
}

/// `bougie lock --script <file>` — resolve the script's inline
/// dependencies and write an adjacent `<file>.lock` for reproducible,
/// offline `bougie run --script` invocations.
pub fn lock(
    _format: OutputFormat,
    file: &Path,
    dry_run: bool,
    resolution: ResolutionStrategy,
) -> Result<ExitCode> {
    let script_path = std::fs::canonicalize(file)
        .wrap_err_with(|| format!("resolving script path `{}`", file.display()))?;
    let source = std::fs::read_to_string(&script_path)
        .wrap_err_with(|| format!("reading script {}", script_path.display()))?;
    let meta = parse_inline_metadata(&source)?.ok_or_else(|| {
        eyre!("{} has no `# /// script` block to lock", script_path.display())
    })?;
    let sidecar = sidecar_lock_path(&script_path);
    if dry_run {
        eprintln!("bougie: would resolve and write {}", sidecar.display());
        return Ok(ExitCode::SUCCESS);
    }
    let paths = Paths::from_env()?;
    write_sidecar_lock(&paths, &script_path, &meta.composer_json, resolution)?;
    eprintln!("bougie: wrote {}", sidecar.display());
    Ok(ExitCode::SUCCESS)
}

/// `bougie add --script <file> <pkgs>` — add packages to the script's
/// inline `# /// script` block in place (uv's `uv add --script`), then
/// refresh its adjacent `<file>.lock`. A `vendor/pkg@constraint` token
/// keeps its constraint; a bare `vendor/pkg` is added at `*` and pinned
/// exactly by the refreshed lock.
pub fn add(
    _format: OutputFormat,
    file: &Path,
    packages: &[String],
    dry_run: bool,
    resolution: ResolutionStrategy,
) -> Result<ExitCode> {
    let script_path = std::fs::canonicalize(file)
        .wrap_err_with(|| format!("resolving script path `{}`", file.display()))?;
    let source = std::fs::read_to_string(&script_path)
        .wrap_err_with(|| format!("reading script {}", script_path.display()))?;
    let meta = parse_inline_metadata(&source)?.ok_or_else(|| {
        eyre!(
            "{} has no `# /// script` block; add one before `bougie add --script`",
            script_path.display()
        )
    })?;

    // Bare packages (no `@constraint`) get the same `>=X.Y` lower-bound
    // default as `bougie add`, resolved against the script's own block
    // (its repositories / stability) in a temp dir. `@constraint` tokens
    // keep their constraint verbatim.
    let bare: Vec<String> = packages
        .iter()
        .filter(|s| !s.contains('@'))
        .cloned()
        .collect();
    let defaults = if bare.is_empty() {
        std::collections::HashMap::new()
    } else {
        let tmp = tempfile::tempdir()
            .wrap_err("creating a temp dir to resolve default constraints")?;
        std::fs::write(tmp.path().join("composer.json"), &meta.composer_json)
            .wrap_err("writing composer.json for constraint resolution")?;
        super::composer_require::default_constraints_for(
            &Paths::from_env()?,
            tmp.path(),
            &bare,
            super::composer_require::DefaultConstraint::LowerBound,
        )?
    };

    let mut doc: serde_json::Value = serde_json::from_str(&meta.composer_json)
        .wrap_err("re-parsing the inline composer.json")?;
    let require = doc
        .as_object_mut()
        .ok_or_else(|| eyre!("inline metadata must be a JSON object"))?
        .entry("require")
        .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()))
        .as_object_mut()
        .ok_or_else(|| eyre!("`require` in the inline block must be an object"))?;
    for spec in packages {
        let (name, version) = match spec.split_once('@') {
            Some((n, v)) => (n.to_string(), v.to_string()),
            None => (
                spec.clone(),
                defaults
                    .get(spec.as_str())
                    .cloned()
                    .unwrap_or_else(|| "*".to_string()),
            ),
        };
        require.insert(name, serde_json::Value::String(version));
    }
    let new_body = serde_json::to_string_pretty(&doc)
        .wrap_err("serializing the updated metadata block")?;
    let new_source = replace_block_body(&source, &new_body)
        .wrap_err("splicing the updated metadata block back into the script")?;

    if dry_run {
        eprintln!("bougie: would rewrite {} as:", script_path.display());
        eprint!("{new_source}");
        return Ok(ExitCode::SUCCESS);
    }
    std::fs::write(&script_path, &new_source)
        .wrap_err_with(|| format!("writing {}", script_path.display()))?;
    eprintln!(
        "bougie: updated the `# /// script` block in {}",
        script_path.display()
    );

    // Refresh the adjacent lock so the new deps are pinned + reproducible.
    let paths = Paths::from_env()?;
    let sidecar = write_sidecar_lock(&paths, &script_path, &new_body, resolution)?;
    eprintln!("bougie: wrote {}", sidecar.display());
    Ok(ExitCode::SUCCESS)
}

/// `bougie init --script <file>` — scaffold a self-contained script stub
/// (uv's `uv init --script`): a `bougie run --script` shebang, a
/// `# /// script` block pinning the current PHP floor, and a hello-world
/// body. Refuses to overwrite an existing file; makes it executable on
/// Unix so `./<file>` runs immediately.
pub fn init(_format: OutputFormat, file: &Path) -> Result<ExitCode> {
    if file.exists() {
        return Err(eyre!("{} already exists; refusing to overwrite", file.display()));
    }
    let php_floor = default_php_floor(Paths::from_env().ok().as_ref());
    let name = file.file_name().and_then(|n| n.to_str()).unwrap_or("script");
    let stub = SCRIPT_STUB
        .replace("__PHP__", &php_floor)
        .replace("__NAME__", name);
    std::fs::write(file, &stub).wrap_err_with(|| format!("writing {}", file.display()))?;
    #[cfg(unix)]
    make_executable(file);
    eprintln!("bougie: created script {} (php {php_floor})", file.display());
    Ok(ExitCode::SUCCESS)
}

/// The `bougie init --script` scaffold. `__PHP__` / `__NAME__` are
/// substituted at write time. The `# /// script` block is a composer.json
/// object; the body prints a greeting.
const SCRIPT_STUB: &str = r#"#!/usr/bin/env -S bougie run --script
<?php
# /// script
# {
#   "require": {
#     "php": "__PHP__"
#   }
# }
# ///

echo "Hello from __NAME__!\n";
"#;

/// The `>=X.Y` PHP floor to pin in a scaffolded script: the highest
/// installed managed NTS interpreter's `major.minor` (uv floors at the
/// available interpreter), or `>=8.3` when none is installed yet.
fn default_php_floor(paths: Option<&Paths>) -> String {
    let highest = paths.and_then(|p| {
        bougie_fs::store::list_installed(p).ok().and_then(|list| {
            list.into_iter()
                .filter(|(_, flavor)| flavor == "nts")
                .filter_map(|(v, _)| {
                    let mut parts = v.split('.');
                    let major: u64 = parts.next()?.parse().ok()?;
                    let minor: u64 = parts.next()?.parse().ok()?;
                    Some((major, minor))
                })
                .max()
        })
    });
    match highest {
        Some((major, minor)) => format!(">={major}.{minor}"),
        None => ">=8.3".to_string(),
    }
}

#[cfg(unix)]
fn make_executable(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    if let Ok(meta) = std::fs::metadata(path) {
        let mut perms = meta.permissions();
        perms.set_mode(perms.mode() | 0o111);
        let _ = std::fs::set_permissions(path, perms);
    }
}

/// Resolve `composer_json` in a throwaway temp dir and copy the resulting
/// `composer.lock` to `<script>.lock`. Returns the sidecar path.
fn write_sidecar_lock(
    paths: &Paths,
    script_path: &Path,
    composer_json: &str,
    resolution: ResolutionStrategy,
) -> Result<PathBuf> {
    let tmp = tempfile::tempdir().wrap_err("creating a temp dir to resolve the script lock")?;
    std::fs::write(tmp.path().join("composer.json"), composer_json)
        .wrap_err("writing composer.json for lock resolution")?;
    let (lock_path, _outcome) =
        super::composer_update::resolve_and_write_lock(paths, tmp.path(), resolution)
            .wrap_err("resolving the script's dependencies")?;
    let sidecar = sidecar_lock_path(script_path);
    std::fs::copy(&lock_path, &sidecar)
        .wrap_err_with(|| format!("writing {}", sidecar.display()))?;
    Ok(sidecar)
}

/// Stable cache key for a script env. Keyed on the inline `composer.json`
/// (the dep set), the `--php` request, the sorted `--with` list, and the
/// committed sidecar lock (if any) — the inputs that determine the
/// resolved environment.
fn cache_key(
    composer_json: &str,
    php_request: Option<&str>,
    with: &[String],
    lock: Option<&[u8]>,
) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(composer_json.as_bytes());
    hasher.update(b"\0");
    hasher.update(php_request.unwrap_or("").as_bytes());
    hasher.update(b"\0");
    let mut sorted = with.to_vec();
    sorted.sort();
    for w in &sorted {
        hasher.update(w.as_bytes());
        hasher.update(b"\0");
    }
    if let Some(lock) = lock {
        hasher.update(b"lock\0");
        hasher.update(lock);
    }
    let digest = hasher.finalize();
    // 16 hex chars (~64 bits) — plenty to keep slots distinct, matching
    // the tool-run cache key width.
    let mut key = String::with_capacity(16);
    for byte in &digest[..8] {
        write!(key, "{byte:02x}").expect("writing to a String is infallible");
    }
    key
}

/// Best-effort mtime refresh for a slot we just hit, so `bougie cache
/// prune` doesn't GC an actively-used script env. Failure isn't fatal.
fn touch(marker: &Path) {
    let Ok(file) = std::fs::OpenOptions::new().write(true).open(marker) else {
        return;
    };
    let times = std::fs::FileTimes::new().set_modified(std::time::SystemTime::now());
    let _ = file.set_times(times);
}

#[cfg(test)]
mod tests {
    use super::{cache_key, classify_with, default_php_floor, merge_with, SCRIPT_STUB};

    #[test]
    fn scaffolded_stub_roundtrips_through_the_parser() {
        let stub = SCRIPT_STUB
            .replace("__PHP__", ">=8.3")
            .replace("__NAME__", "demo.php");
        let meta = bougie_composer::inline::parse_inline_metadata(&stub)
            .unwrap()
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(&meta.composer_json).unwrap();
        assert_eq!(v["require"]["php"], ">=8.3");
        assert!(stub.starts_with("#!/usr/bin/env -S bougie run --script\n"));
        assert!(stub.contains(r#"echo "Hello from demo.php!\n";"#));
    }

    #[test]
    fn default_php_floor_falls_back_without_installs() {
        assert_eq!(default_php_floor(None), ">=8.3");
    }

    #[test]
    fn classify_with_splits_packages_and_extensions() {
        assert_eq!(classify_with("gd"), ("ext-gd".into(), "*".into()));
        assert_eq!(classify_with("gd=2.1"), ("ext-gd".into(), "*".into()));
        assert_eq!(
            classify_with("monolog/monolog"),
            ("monolog/monolog".into(), "*".into())
        );
        assert_eq!(
            classify_with("monolog/monolog@^3.0"),
            ("monolog/monolog".into(), "^3.0".into())
        );
        assert_eq!(
            classify_with("monolog/monolog=^3.0"),
            ("monolog/monolog".into(), "^3.0".into())
        );
    }

    #[test]
    fn merge_with_adds_without_overriding_inline_block() {
        let base = r#"{"require":{"php":">=8.2","psr/log":"^3.0"}}"#;
        let merged = merge_with(base, &["gd".into(), "psr/log".into(), "guzzlehttp/guzzle@^7".into()])
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(&merged).unwrap();
        let req = v["require"].as_object().unwrap();
        // inline block's psr/log constraint is preserved (not clobbered by --with)
        assert_eq!(req["psr/log"], "^3.0");
        assert_eq!(req["ext-gd"], "*");
        assert_eq!(req["guzzlehttp/guzzle"], "^7");
        assert_eq!(req["php"], ">=8.2");
    }

    #[test]
    fn merge_with_empty_is_identity() {
        let base = r#"{"require":{"php":">=8.2"}}"#;
        assert_eq!(merge_with(base, &[]).unwrap(), base);
    }

    #[test]
    fn merge_with_creates_require_when_absent() {
        let merged = merge_with("{}", &["gd".into()]).unwrap();
        let v: serde_json::Value = serde_json::from_str(&merged).unwrap();
        assert_eq!(v["require"]["ext-gd"], "*");
    }

    #[test]
    fn cache_key_is_deterministic_and_input_sensitive() {
        let base = cache_key(r#"{"require":{"php":">=8.2"}}"#, None, &[], None);
        let same = cache_key(r#"{"require":{"php":">=8.2"}}"#, None, &[], None);
        assert_eq!(base, same);
        let other_php_req = cache_key(r#"{"require":{"php":">=8.3"}}"#, None, &[], None);
        assert_ne!(base, other_php_req);
        let pinned_php = cache_key(r#"{"require":{"php":">=8.2"}}"#, Some("8.4"), &[], None);
        assert_ne!(base, pinned_php);
        // A committed sidecar lock changes the key (different env).
        let with_lock = cache_key(r#"{"require":{"php":">=8.2"}}"#, None, &[], Some(b"lockbytes"));
        assert_ne!(base, with_lock);
        assert_eq!(base.len(), 16);
    }
}
