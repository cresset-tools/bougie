//! Per-project `.bougie/conf.d{,-debug}/` fragment generation for user-
//! installed extensions. Bundled extensions are handled separately by
//! `commands::sync::replicate_install_conf_d`, which copies the
//! `00-XX-<name>.ini` shipped with each PHP install. This module
//! covers the `<NN>-<name>.ini` fragments that *enable* extensions
//! bougie installed itself (i.e. via `bougie ext add` or sync's
//! composer.json auto-install).
//!
//! Two parallel directories:
//!
//! - `.bougie/conf.d/` — the project's declared environment. Every
//!   `bougie ext add <name>` lands here, including xdebug. Loaded by
//!   `bougie run`, the server's normal pool, *and* the server's
//!   xdebug pool — when the user says "give me xdebug", they get it
//!   everywhere.
//! - `.bougie/conf.d-debug/` — a server-private overlay. Bougie
//!   server writes here when it lazily activates xdebug on the first
//!   `XDEBUG_SESSION`-cookie request and the user hasn't explicitly
//!   added it. Read only by the server's xdebug pool variant; never
//!   touched by `bougie run`.
//!
//! The numeric prefix `<NN>` is chosen by `install::conf_d_prefix_for`
//! to mirror `php-build-standalone`'s build-time numbering: `35-` for
//! `pdo_*` (after `30-pdo`), `40-` for `mysqli/sqlite3/pgsql`, `20-`
//! otherwise. This keeps PHP's alphabetic conf.d scan loading
//! dependents after dependencies — `20-pdo_mysql.ini` ahead of
//! `30-pdo.ini` would trigger `undefined symbol: pdo_dbh_ce`.

use bougie_index::wire::LoadDirective;
use crate::install::conf_d_prefix_for;
use eyre::{eyre, Result, WrapErr};
use std::io::Write;
use std::path::{Path, PathBuf};

/// `<project>/.bougie/conf.d/` — every extension the user has explicitly
/// added. Loaded by every flow that needs PHP: `bougie run`, the
/// server's normal pool, and the server's xdebug pool.
pub fn project_confd_dir(project_root: &Path) -> PathBuf {
    project_root.join(".bougie").join("conf.d")
}

/// `<project>/.bougie/conf.d-local/` — machine-local extensions added
/// via `bougie ext add --so <path>`. Fragments here are NOT mirrored
/// by `bougie sync` and NOT recorded in `composer.json` — they're for
/// ad-hoc profilers/loaders (Tideways, Blackfire, custom builds) that
/// shouldn't bleed into the project's portable dependency set.
/// Layered into `PHP_INI_SCAN_DIR` for every flow that needs PHP.
pub fn project_confd_local_dir(project_root: &Path) -> PathBuf {
    project_root.join(".bougie").join("conf.d-local")
}

/// `<project>/.bougie/conf.d-debug/` — server-private overlay. Read
/// only by the server's xdebug pool variant. Bougie server writes
/// here from [`write_debug_overlay_fragment`] when it lazily installs
/// xdebug for an `XDEBUG_SESSION`-cookied request and the user hasn't
/// explicitly added it. `bougie ext add` does NOT write here — see
/// [`write_ext_fragment`].
pub fn project_confd_debug_dir(project_root: &Path) -> PathBuf {
    project_root.join(".bougie").join("conf.d-debug")
}

/// Compose a `PHP_INI_SCAN_DIR` value. With `debug_overlay=false` it's
/// just `conf.d/`; with `true` it's `conf.d<SEP>conf.d-debug` so PHP
/// scans both. Shared between `bougie run` and the `php`/`composer`
/// argv0 shim so both paths arrive at the same effective config when
/// `XDEBUG_SESSION` is set or `--xdebug` was passed.
///
/// Separator matches PHP's own scan: `:` on Unix, `;` on Windows.
pub fn php_ini_scan_dir(project_root: &Path, debug_overlay: bool) -> std::ffi::OsString {
    let regular = project_confd_dir(project_root);
    let local = project_confd_local_dir(project_root);
    let mut joined = regular.into_os_string();
    if local.exists() {
        joined.push(SCAN_DIR_SEP);
        joined.push(&local);
    }
    if debug_overlay {
        joined.push(SCAN_DIR_SEP);
        joined.push(project_confd_debug_dir(project_root));
    }
    joined
}

#[cfg(windows)]
const SCAN_DIR_SEP: &str = ";";
#[cfg(not(windows))]
const SCAN_DIR_SEP: &str = ":";

/// `true` if the parent environment signals an active xdebug session.
/// Equivalent to the cookie/query gate the server uses, applied to a
/// child process bougie is about to exec.
pub fn xdebug_session_env_active() -> bool {
    std::env::var_os("XDEBUG_SESSION").is_some_and(|v| !v.is_empty())
}

/// Write — atomically — the `<NN>-<name>.ini` fragment for an
/// explicit `bougie ext add <name>` into `.bougie/conf.d/`. Returns
/// the fragment's absolute path so callers can surface it in
/// `--format json` output. `<NN>` is determined by
/// [`conf_d_prefix_for`] — see module docs.
///
/// `path_extras` carries directories the extension's dependent DLLs
/// live in; they're emitted as `; bougie-path: <dir>` structured
/// comments at the top of the fragment. [`read_path_extras`] picks
/// these up at `bougie run` time and prepends them to PATH so the
/// Windows DLL loader resolves them (used today by imagick — its
/// store dir holds ~170 ImageMagick `CORE_RL_*.dll` and
/// `IM_MOD_RL_*.dll` codec modules). Empty on every Unix extension and
/// on every single-DLL Windows PECL ext.
///
/// Existing fragments are overwritten unconditionally: a re-install at
/// a new version updates the `.so` path in place.
pub fn write_ext_fragment(
    project_root: &Path,
    name: &str,
    so_path: &Path,
    load: LoadDirective,
    path_extras: &[PathBuf],
) -> Result<PathBuf> {
    write_fragment_into(
        project_confd_dir(project_root),
        name,
        so_path,
        load,
        path_extras,
    )
}

/// Write — atomically — the `<NN>-<name>.ini` fragment for the
/// server-private debug overlay (`.bougie/conf.d-debug/`). Used by
/// `commands::server::pool::ensure_debug_extension` when the server
/// lazily activates xdebug on a debug-routed request and the user
/// hasn't explicitly added it. Behaves like [`write_ext_fragment`]
/// otherwise (atomic write, prefix collision cleanup, default INI
/// settings appended).
/// Write — atomically — a `<NN>-<name>.ini` fragment for a local-only
/// extension (`bougie ext add <name> --so <path>`) into
/// `.bougie/conf.d-local/`. Bypasses the sync/composer.json round-trip
/// because the .so came from the user's machine, not the index.
pub fn write_local_ext_fragment(
    project_root: &Path,
    name: &str,
    so_path: &Path,
    load: LoadDirective,
) -> Result<PathBuf> {
    write_fragment_into(project_confd_local_dir(project_root), name, so_path, load, &[])
}

pub fn write_debug_overlay_fragment(
    project_root: &Path,
    name: &str,
    so_path: &Path,
    load: LoadDirective,
) -> Result<PathBuf> {
    // The server's lazy-xdebug activation never has dependent DLLs to
    // surface — xdebug's a single-DLL ext on every platform.
    write_fragment_into(project_confd_debug_dir(project_root), name, so_path, load, &[])
}

fn write_fragment_into(
    dir: PathBuf,
    name: &str,
    so_path: &Path,
    load: LoadDirective,
    path_extras: &[PathBuf],
) -> Result<PathBuf> {
    std::fs::create_dir_all(&dir)
        .wrap_err_with(|| format!("creating {}", dir.display()))?;
    let prefix = conf_d_prefix_for(name);
    let path = dir.join(format!("{prefix}-{name}.ini"));
    // Drop any stale fragment for this ext at a different prefix —
    // happens when bougie's prefix mapping changes between releases
    // (e.g. `20-pdo_mysql.ini` from an older bougie has to go before
    // we write `35-pdo_mysql.ini` or PHP would load pdo_mysql before
    // pdo.so initializes pdo_dbh_ce).
    remove_other_prefix_fragments(&dir, name, prefix)?;
    let mut body = format!(
        "; managed by bougie — do not edit; regenerated by `bougie ext add {name}`\n",
    );
    for extra in path_extras {
        // Format chosen for [`read_path_extras`] to grep: the leading
        // `; bougie-path: ` prefix is the structured marker. Paths
        // are absolute so `bougie run` doesn't need a project-root
        // base to resolve them.
        body.push_str(&format!("; bougie-path: {}\n", extra.display()));
    }
    body.push_str(&format!(
        "{directive}={so}\n",
        directive = load.ini_directive(),
        so = format_ini_path(so_path),
    ));
    body.push_str(default_ini_settings_for(name));
    write_atomic(&path, body.as_bytes())?;
    Ok(path)
}

/// Render `path` as the right-hand side of an `extension=` /
/// `zend_extension=` directive. On Windows the result is wrapped in
/// double quotes; PHP's INI parser otherwise treats `~` as the unary
/// bitwise-NOT operator and truncates the value at the tilde — which
/// strikes any path containing a Windows 8.3 short-form segment
/// (`C:\Users\RUNNER~1\…`), as GitHub Actions runners produce by
/// default. Quoting forces the parser into string-literal mode.
/// Unix INI paths don't run into this since they don't carry `~`,
/// and leaving them unquoted keeps the existing test assertions
/// (`extension=/path/foo.so`) matching verbatim.
#[inline]
pub fn format_ini_path(path: &Path) -> String {
    #[cfg(windows)]
    return format!("\"{}\"", path.display());
    #[cfg(not(windows))]
    return path.display().to_string();
}

/// Path-extras marker prefix in conf.d fragments. Exposed as a const
/// so [`read_path_extras`] and [`write_fragment_into`] stay in sync.
const PATH_EXTRA_MARKER: &str = "; bougie-path: ";

/// Walk `<project>/.bougie/conf.d{,-debug}/` looking for the
/// `; bougie-path: <abs path>` markers [`write_ext_fragment`] emits,
/// and return the union of those paths in stable filename order.
///
/// `bougie run` prepends these to `PATH` so the Windows DLL loader
/// finds the dependent DLLs colocated with the extension's `.dll` in
/// the store. Cross-platform — Unix sees no such markers (none of
/// bougie's Unix ext install paths set `path_extras`), so the loop
/// is a cheap no-op there.
pub fn read_path_extras(project_root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut seen: std::collections::BTreeSet<PathBuf> = std::collections::BTreeSet::new();
    for dir in [
        project_confd_dir(project_root),
        project_confd_debug_dir(project_root),
    ] {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        let mut files: Vec<PathBuf> = entries
            .flatten()
            .filter_map(|e| {
                let p = e.path();
                (p.extension().and_then(|s| s.to_str()) == Some("ini")).then_some(p)
            })
            .collect();
        // Sort by filename so the resulting PATH ordering is
        // deterministic across runs (read_dir is unordered on
        // most filesystems).
        files.sort();
        for f in files {
            let Ok(body) = std::fs::read_to_string(&f) else {
                continue;
            };
            for line in body.lines() {
                if let Some(rest) = line.strip_prefix(PATH_EXTRA_MARKER) {
                    let p = PathBuf::from(rest.trim());
                    if seen.insert(p.clone()) {
                        out.push(p);
                    }
                }
            }
        }
    }
    out
}

/// Per-extension INI settings appended to the fragment body. Only
/// xdebug needs them today: xdebug 3 ships with `xdebug.mode=off`,
/// under which the extension loads (phpinfo shows it) but every
/// runtime API — step debugger, breakpoints, `xdebug_break()`,
/// profiler — is a no-op. We pick `debug,develop` to give "step
/// debugger + dev helpers" out of the box, matching the most common
/// IDE/Xdebug-Helper setup. Other extensions return `""`.
fn default_ini_settings_for(name: &str) -> &'static str {
    if name.eq_ignore_ascii_case("xdebug") {
        // xdebug.start_with_request=trigger: only attach when a request
        // carries XDEBUG_SESSION/XDEBUG_TRIGGER — which is already the
        // server-side gate for the xdebug pool variant, so by the time
        // this fragment loads we know we want xdebug active.
        "xdebug.mode=debug,develop\n\
         xdebug.start_with_request=trigger\n"
    } else {
        ""
    }
}

/// Remove any `<NN>-<name>.ini` fragment if it exists. Returns
/// `Ok(true)` when a file was removed, `Ok(false)` if no fragment was
/// present. Scans all numeric prefixes rather than only the canonical
/// one so that `bougie ext remove` works after a prefix mapping change.
/// Scans both `conf.d/` and `conf.d-debug/` so removing an ext that
/// was installed before the split (and so lives in conf.d/) still
/// works.
pub fn remove_ext_fragment(project_root: &Path, name: &str) -> Result<bool> {
    let mut removed = false;
    for dir in [
        project_confd_dir(project_root),
        project_confd_debug_dir(project_root),
        project_confd_local_dir(project_root),
    ] {
        removed |= remove_fragment_in(&dir, name)?;
    }
    Ok(removed)
}

fn remove_fragment_in(dir: &Path, name: &str) -> Result<bool> {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(eyre!("reading {}: {e}", dir.display())),
    };
    let target_suffix = format!("-{name}.ini");
    let mut removed = false;
    for entry in entries.flatten() {
        let fname = entry.file_name();
        let Some(fname_str) = fname.to_str() else { continue };
        if fname_str.ends_with(&target_suffix) && has_numeric_prefix(fname_str) {
            match std::fs::remove_file(entry.path()) {
                Ok(()) => removed = true,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(eyre!("removing {}: {e}", entry.path().display())),
            }
        }
    }
    Ok(removed)
}

/// `true` if a baseline-replicated `00-*-<name>.ini` fragment is
/// present in the project's regular conf.d. Sync writes those when
/// it mirrors the install's `etc/php/conf.d/` (see
/// `commands::sync::replicate_install_conf_d`), so this is the direct
/// observable signal that the install already loads `<name>` without
/// any user-written fragment. `bougie ext add` uses it to skip the
/// would-be-duplicate `20-<name>.ini` write that produced PHP's
/// "Module already loaded" warning.
pub fn installed_fragment_present(project_root: &Path, name: &str) -> bool {
    let dir = project_confd_dir(project_root);
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return false;
    };
    let suffix = format!("-{name}.ini");
    for entry in entries.flatten() {
        if let Some(fname) = entry.file_name().to_str()
            && fname.starts_with("00-")
            && fname.ends_with(&suffix)
        {
            return true;
        }
    }
    false
}

/// Remove any user-written `<NN>-<name>.ini` fragment from the
/// regular conf.d/ where `<NN>` is not the baseline-replication `00`
/// prefix. Used by `bougie ext add` to clean up duplicates left
/// behind by an older bougie that wrote `20-<name>.ini` alongside the
/// install's bundled `00-20-<name>.ini` — see GitHub issue #28.
/// Returns `Ok(true)` when at least one file was removed.
pub fn remove_user_ext_fragment(project_root: &Path, name: &str) -> Result<bool> {
    let dir = project_confd_dir(project_root);
    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(eyre!("reading {}: {e}", dir.display())),
    };
    let target_suffix = format!("-{name}.ini");
    let mut removed = false;
    for entry in entries.flatten() {
        let fname = entry.file_name();
        let Some(fname_str) = fname.to_str() else { continue };
        if fname_str.starts_with("00-") {
            continue;
        }
        if fname_str.ends_with(&target_suffix) && has_numeric_prefix(fname_str) {
            match std::fs::remove_file(entry.path()) {
                Ok(()) => removed = true,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(eyre!("removing {}: {e}", entry.path().display())),
            }
        }
    }
    Ok(removed)
}

/// `true` if `<NN>-<name>.ini` already exists in either the regular
/// or the debug-overlay conf.d. Used by the server's lazy xdebug
/// activator to skip when the user already installed xdebug
/// explicitly (in which case the fragment is in `conf.d/` and the
/// xdebug pool's merged scan dir picks it up).
pub fn fragment_present_anywhere(project_root: &Path, name: &str) -> bool {
    for dir in [
        project_confd_dir(project_root),
        project_confd_debug_dir(project_root),
        project_confd_local_dir(project_root),
    ] {
        if fragment_present_in(&dir, name) {
            return true;
        }
    }
    false
}

fn fragment_present_in(dir: &Path, name: &str) -> bool {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return false;
    };
    let suffix = format!("-{name}.ini");
    for entry in entries.flatten() {
        if let Some(fname) = entry.file_name().to_str()
            && fname.ends_with(&suffix)
            && has_numeric_prefix(fname)
        {
            return true;
        }
    }
    false
}

/// Delete `<other_prefix>-<name>.ini` fragments where `other_prefix`
/// is any numeric prefix that doesn't match `keep_prefix`. Used by
/// [`write_ext_fragment`] to keep exactly one fragment per ext when
/// the canonical prefix changes between bougie releases.
fn remove_other_prefix_fragments(dir: &Path, name: &str, keep_prefix: u32) -> Result<()> {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(eyre!("reading {}: {e}", dir.display())),
    };
    let target_suffix = format!("-{name}.ini");
    let keep_full = format!("{keep_prefix}-{name}.ini");
    for entry in entries.flatten() {
        let fname = entry.file_name();
        let Some(fname_str) = fname.to_str() else { continue };
        if fname_str == keep_full {
            continue;
        }
        if fname_str.ends_with(&target_suffix) && has_numeric_prefix(fname_str) {
            let _ = std::fs::remove_file(entry.path());
        }
    }
    Ok(())
}

fn has_numeric_prefix(filename: &str) -> bool {
    let Some(dash) = filename.find('-') else { return false };
    let prefix = &filename[..dash];
    !prefix.is_empty() && prefix.chars().all(|c| c.is_ascii_digit())
}

/// tempfile + rename in the same directory. Same atomicity guarantees
/// as `composer::lockfile::write_json_bytes` (POSIX same-fs rename).
fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let dir = path.parent().ok_or_else(|| {
        eyre!("path {} has no parent directory", path.display())
    })?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn writes_regular_extension_fragment() {
        let td = TempDir::new().unwrap();
        let so = td.path().join("store/redis-6/redis.so");
        let path = write_ext_fragment(
            td.path(),
            "redis",
            &so,
            LoadDirective::Extension,
            &[],
        )
        .unwrap();
        let body = std::fs::read_to_string(&path).unwrap();
        assert!(path.ends_with(".bougie/conf.d/20-redis.ini"));
        assert!(body.starts_with("; managed by bougie"));
        assert!(body.contains(&format!("extension={}", format_ini_path(&so))));
        assert!(!body.contains("zend_extension"));
    }

    #[test]
    fn writes_zend_extension_fragment() {
        let td = TempDir::new().unwrap();
        let so = td.path().join("store/xdebug-3/xdebug.so");
        let path = write_ext_fragment(
            td.path(),
            "xdebug",
            &so,
            LoadDirective::ZendExtension,
            &[],
        )
        .unwrap();
        let body = std::fs::read_to_string(&path).unwrap();
        assert!(body.contains(&format!("zend_extension={}", format_ini_path(&so))));
    }

    #[test]
    fn overwrites_existing_fragment() {
        // A re-install at a new path must replace the old fragment so
        // PHP loads the right `.so` after `bougie sync`.
        let td = TempDir::new().unwrap();
        let old_so = td.path().join("store/redis-5/redis.so");
        let new_so = td.path().join("store/redis-6/redis.so");
        write_ext_fragment(td.path(), "redis", &old_so, LoadDirective::Extension, &[]).unwrap();
        let path =
            write_ext_fragment(td.path(), "redis", &new_so, LoadDirective::Extension, &[]).unwrap();
        let body = std::fs::read_to_string(&path).unwrap();
        assert!(body.contains(&format!("extension={}", format_ini_path(&new_so))));
        assert!(!body.contains(&format!("extension={}", format_ini_path(&old_so))));
    }

    #[test]
    fn remove_reports_state() {
        let td = TempDir::new().unwrap();
        let so = td.path().join("store/redis-6/redis.so");
        write_ext_fragment(td.path(), "redis", &so, LoadDirective::Extension, &[]).unwrap();
        assert!(remove_ext_fragment(td.path(), "redis").unwrap());
        assert!(!remove_ext_fragment(td.path(), "redis").unwrap());
    }

    #[test]
    fn remove_of_absent_is_noop() {
        let td = TempDir::new().unwrap();
        assert!(!remove_ext_fragment(td.path(), "ghost").unwrap());
    }

    #[test]
    fn pdo_drivers_use_35_prefix() {
        // pdo_* must load after `30-pdo.ini` or PHP errors with
        // "undefined symbol: pdo_dbh_ce". The 35- prefix mirrors
        // php-build-standalone's build-php.sh numbering.
        let td = TempDir::new().unwrap();
        let so = td.path().join("store/ext/pdo_mysql.so");
        let path = write_ext_fragment(
            td.path(),
            "pdo_mysql",
            &so,
            LoadDirective::Extension,
            &[],
        )
        .unwrap();
        assert!(path.ends_with(".bougie/conf.d/35-pdo_mysql.ini"));
    }

    #[test]
    fn db_drivers_use_40_prefix() {
        // mysqli / sqlite3 / pgsql load alongside `35-pdo_*` but after
        // them; matches build-php.sh convention.
        let td = TempDir::new().unwrap();
        for name in ["mysqli", "sqlite3", "pgsql"] {
            let so = td.path().join(format!("store/ext/{name}.so"));
            let path = write_ext_fragment(
                td.path(),
                name,
                &so,
                LoadDirective::Extension,
                &[],
            )
            .unwrap();
            assert!(
                path.ends_with(format!(".bougie/conf.d/40-{name}.ini")),
                "{}: expected 40- prefix, got {}",
                name,
                path.display()
            );
        }
    }

    #[test]
    fn rewrite_at_new_prefix_drops_old_fragment() {
        // Simulate the upgrade case: a previous bougie wrote
        // `20-pdo_mysql.ini`; after the prefix mapping change we
        // should end up with just `35-pdo_mysql.ini`.
        let td = TempDir::new().unwrap();
        let dir = td.path().join(".bougie/conf.d");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("20-pdo_mysql.ini"), "extension=old\n").unwrap();
        let so = td.path().join("store/ext/pdo_mysql.so");
        write_ext_fragment(td.path(), "pdo_mysql", &so, LoadDirective::Extension, &[])
            .unwrap();
        assert!(dir.join("35-pdo_mysql.ini").exists());
        assert!(!dir.join("20-pdo_mysql.ini").exists());
    }

    #[test]
    fn remove_finds_fragment_at_any_numeric_prefix() {
        let td = TempDir::new().unwrap();
        let dir = td.path().join(".bougie/conf.d");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("35-pdo_mysql.ini"), "extension=x\n").unwrap();
        assert!(remove_ext_fragment(td.path(), "pdo_mysql").unwrap());
        assert!(!dir.join("35-pdo_mysql.ini").exists());
    }

    #[test]
    fn explicit_xdebug_add_lands_in_conf_d() {
        // The user's explicit `bougie ext add xdebug` writes to the
        // regular conf.d/ — this is the "I want xdebug everywhere"
        // signal. The debug-overlay dir is reserved for the server's
        // implicit lazy activation.
        let td = TempDir::new().unwrap();
        let so = td.path().join("store/xdebug-3/xdebug.so");
        let path = write_ext_fragment(
            td.path(),
            "xdebug",
            &so,
            LoadDirective::ZendExtension,
            &[],
        )
        .unwrap();
        assert!(
            path.starts_with(td.path().join(".bougie/conf.d")),
            "expected conf.d/, got {}",
            path.display()
        );
        assert!(!td.path().join(".bougie/conf.d-debug").exists());
    }

    #[test]
    fn xdebug_fragment_includes_mode_and_trigger() {
        // With xdebug 3's default `xdebug.mode=off`, the extension
        // loads but every runtime API is inert. Make sure the
        // fragment carries the mode flags that flip it on.
        let td = TempDir::new().unwrap();
        let so = td.path().join("store/xdebug-3/xdebug.so");
        let path = write_ext_fragment(
            td.path(),
            "xdebug",
            &so,
            LoadDirective::ZendExtension,
            &[],
        )
        .unwrap();
        let body = std::fs::read_to_string(&path).unwrap();
        assert!(body.contains("xdebug.mode=debug,develop"), "got: {body}");
        assert!(body.contains("xdebug.start_with_request=trigger"), "got: {body}");
    }

    #[test]
    fn regular_ext_does_not_get_xdebug_settings() {
        let td = TempDir::new().unwrap();
        let so = td.path().join("store/redis-6/redis.so");
        let path =
            write_ext_fragment(td.path(), "redis", &so, LoadDirective::Extension, &[]).unwrap();
        let body = std::fs::read_to_string(&path).unwrap();
        assert!(!body.contains("xdebug.mode"));
        assert!(!body.contains("start_with_request"));
    }

    #[test]
    fn debug_overlay_writer_lands_in_conf_d_debug() {
        // The server's lazy-activation path explicitly opts into the
        // debug overlay via `write_debug_overlay_fragment`.
        let td = TempDir::new().unwrap();
        let so = td.path().join("store/xdebug-3/xdebug.so");
        let path = write_debug_overlay_fragment(
            td.path(),
            "xdebug",
            &so,
            LoadDirective::ZendExtension,
        )
        .unwrap();
        assert!(
            path.starts_with(td.path().join(".bougie/conf.d-debug")),
            "expected conf.d-debug/, got {}",
            path.display()
        );
        assert!(!td.path().join(".bougie/conf.d").exists());
    }

    #[test]
    fn fragment_present_anywhere_finds_explicit_add() {
        let td = TempDir::new().unwrap();
        let so = td.path().join("store/xdebug-3/xdebug.so");
        write_ext_fragment(td.path(), "xdebug", &so, LoadDirective::ZendExtension, &[]).unwrap();
        assert!(fragment_present_anywhere(td.path(), "xdebug"));
    }

    #[test]
    fn fragment_present_anywhere_finds_overlay() {
        let td = TempDir::new().unwrap();
        let so = td.path().join("store/xdebug-3/xdebug.so");
        write_debug_overlay_fragment(td.path(), "xdebug", &so, LoadDirective::ZendExtension)
            .unwrap();
        assert!(fragment_present_anywhere(td.path(), "xdebug"));
    }

    #[test]
    fn fragment_present_anywhere_is_false_when_absent() {
        let td = TempDir::new().unwrap();
        assert!(!fragment_present_anywhere(td.path(), "xdebug"));
    }

    #[test]
    fn write_ext_fragment_emits_path_extra_markers() {
        // Imagick-style: when the caller supplies path_extras, the
        // fragment carries one `; bougie-path: <dir>` line per path,
        // ahead of the extension= directive so a line-by-line reader
        // sees them before parsing the actual config.
        let td = TempDir::new().unwrap();
        let so = td.path().join("store/ext-imagick-xyz/php_imagick.dll");
        let extras = vec![td.path().join("store/ext-imagick-xyz")];
        let path = write_ext_fragment(
            td.path(),
            "imagick",
            &so,
            LoadDirective::Extension,
            &extras,
        )
        .unwrap();
        let body = std::fs::read_to_string(&path).unwrap();
        assert!(
            body.contains(&format!("; bougie-path: {}", extras[0].display())),
            "missing bougie-path marker: {body}"
        );
        // Marker comes before the extension directive — so a stream
        // parser sees the marker first.
        let marker_pos = body.find("; bougie-path:").unwrap();
        let ext_pos = body.find("extension=").unwrap();
        assert!(marker_pos < ext_pos, "marker after directive: {body}");
    }

    #[test]
    fn read_path_extras_collects_markers_from_all_fragments() {
        let td = TempDir::new().unwrap();
        let dir = project_confd_dir(td.path());
        std::fs::create_dir_all(&dir).unwrap();
        // Two fragments with one marker each + one fragment without.
        std::fs::write(
            dir.join("20-imagick.ini"),
            "; managed by bougie\n\
             ; bougie-path: C:\\store\\ext-imagick-aaa\n\
             extension=C:\\store\\ext-imagick-aaa\\php_imagick.dll\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("20-someext.ini"),
            "; managed by bougie\n\
             ; bougie-path: D:\\store\\ext-some-bbb\n\
             extension=D:\\store\\ext-some-bbb\\php_some.dll\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("20-xdebug.ini"),
            "; managed by bougie\nzend_extension=C:\\store\\ext-xdebug-ccc\\php_xdebug.dll\n",
        )
        .unwrap();
        let extras = read_path_extras(td.path());
        // Filename-sorted: imagick, someext (xdebug has no marker).
        assert_eq!(
            extras,
            vec![
                PathBuf::from("C:\\store\\ext-imagick-aaa"),
                PathBuf::from("D:\\store\\ext-some-bbb"),
            ]
        );
    }

    #[test]
    fn read_path_extras_dedups_repeated_markers() {
        // Two fragments asking for the same dir — `bougie run`
        // shouldn't end up with PATH containing duplicate entries.
        let td = TempDir::new().unwrap();
        let dir = project_confd_dir(td.path());
        std::fs::create_dir_all(&dir).unwrap();
        let marker = "; bougie-path: /shared/dir\n";
        std::fs::write(dir.join("20-a.ini"), format!("; mgr\n{marker}extension=a.so\n")).unwrap();
        std::fs::write(dir.join("20-b.ini"), format!("; mgr\n{marker}extension=b.so\n")).unwrap();
        let extras = read_path_extras(td.path());
        assert_eq!(extras, vec![PathBuf::from("/shared/dir")]);
    }

    #[test]
    fn read_path_extras_is_empty_when_no_conf_d_dir_exists() {
        // Brand-new project (no `.bougie/` yet) must return empty
        // rather than erroring — `bougie run` is on the critical
        // path and should never trip on a missing-config-dir error.
        let td = TempDir::new().unwrap();
        assert!(read_path_extras(td.path()).is_empty());
    }

    #[test]
    fn php_ini_scan_dir_default_is_conf_d_only() {
        let root = Path::new("/p");
        let s = php_ini_scan_dir(root, false);
        let expected = project_confd_dir(root);
        assert_eq!(Path::new(&s), expected);
    }

    #[test]
    fn php_ini_scan_dir_overlay_joins_both_with_platform_separator() {
        let root = Path::new("/p");
        let s = php_ini_scan_dir(root, true);
        let expected = {
            let mut j = project_confd_dir(root).into_os_string();
            j.push(SCAN_DIR_SEP);
            j.push(project_confd_debug_dir(root));
            j
        };
        assert_eq!(s, expected);
        // Sanity-check the separator the PHP runtime actually expects.
        #[cfg(windows)]
        assert!(s.to_string_lossy().contains(';'));
        #[cfg(not(windows))]
        assert!(s.to_string_lossy().contains(':'));
    }

    #[test]
    fn installed_fragment_present_detects_baseline_replicated_file() {
        // Sync mirrors `<install>/etc/php/conf.d/20-intl.ini` to
        // `.bougie/conf.d/00-20-intl.ini`. `bougie ext add intl`
        // must see that and skip the duplicate write — see issue #28.
        let td = TempDir::new().unwrap();
        let dir = td.path().join(".bougie/conf.d");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("00-20-intl.ini"), "extension=intl\n").unwrap();
        assert!(installed_fragment_present(td.path(), "intl"));
        assert!(!installed_fragment_present(td.path(), "redis"));
    }

    #[test]
    fn installed_fragment_present_ignores_user_fragments() {
        // A user-written `20-redis.ini` is not a "bundled" fragment —
        // it's the user-add path, which must still install on a fresh
        // re-add. Only `00-`-prefixed files count.
        let td = TempDir::new().unwrap();
        let dir = td.path().join(".bougie/conf.d");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("20-redis.ini"), "extension=/path/redis.so\n").unwrap();
        assert!(!installed_fragment_present(td.path(), "redis"));
    }

    #[test]
    fn installed_fragment_present_handles_missing_dir() {
        let td = TempDir::new().unwrap();
        assert!(!installed_fragment_present(td.path(), "intl"));
    }

    #[test]
    fn remove_user_ext_fragment_drops_duplicate_but_keeps_baseline() {
        // Pre-fix bougie wrote `20-intl.ini` alongside the baseline-
        // replicated `00-20-intl.ini`. After the fix, `ext add intl`
        // must remove the user-written duplicate while leaving the
        // baseline mirror in place — that's the file PHP actually
        // loads from.
        let td = TempDir::new().unwrap();
        let dir = td.path().join(".bougie/conf.d");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("00-20-intl.ini"), "extension=intl\n").unwrap();
        std::fs::write(dir.join("20-intl.ini"), "extension=/dup/intl.so\n").unwrap();

        let removed = remove_user_ext_fragment(td.path(), "intl").unwrap();
        assert!(removed);
        assert!(dir.join("00-20-intl.ini").exists());
        assert!(!dir.join("20-intl.ini").exists());
    }

    #[test]
    fn remove_user_ext_fragment_noop_when_only_baseline_present() {
        let td = TempDir::new().unwrap();
        let dir = td.path().join(".bougie/conf.d");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("00-20-intl.ini"), "extension=intl\n").unwrap();

        let removed = remove_user_ext_fragment(td.path(), "intl").unwrap();
        assert!(!removed);
        assert!(dir.join("00-20-intl.ini").exists());
    }

    #[test]
    fn remove_user_ext_fragment_handles_missing_dir() {
        let td = TempDir::new().unwrap();
        assert!(!remove_user_ext_fragment(td.path(), "intl").unwrap());
    }

    #[test]
    fn remove_finds_fragment_in_either_dir() {
        // remove_ext_fragment still scans both directories so cleanup
        // works regardless of which writer placed the fragment.
        let td = TempDir::new().unwrap();
        let debug_dir = td.path().join(".bougie/conf.d-debug");
        std::fs::create_dir_all(&debug_dir).unwrap();
        std::fs::write(debug_dir.join("30-xdebug.ini"), "x\n").unwrap();
        assert!(remove_ext_fragment(td.path(), "xdebug").unwrap());

        let regular_dir = td.path().join(".bougie/conf.d");
        std::fs::create_dir_all(&regular_dir).unwrap();
        std::fs::write(regular_dir.join("30-xdebug.ini"), "x\n").unwrap();
        assert!(remove_ext_fragment(td.path(), "xdebug").unwrap());
    }
}
