//! `bougie tool-exec <wrapper-path> [args...]` runtime shim.
//!
//! The kernel invokes us via the wrapper's shebang line:
//!
//! ```text
//! #!$BOUGIE_LOCAL/bin/bougie tool-exec
//! ```
//!
//! All we need to do is locate the tool dir (the wrapper's grandparent
//! directory is always `$TOOL_DIR`), load the receipt, set
//! `PHP_INI_SCAN_DIR` to scope extensions per-tool, and `execve` the
//! receipt's pinned PHP with the wrapper file as `argv[1]`. PHP then
//! runs the wrapper, which `require`s the package's real entry point.
//!
//! Split into `prepare()` (testable, no syscalls beyond reading the
//! receipt) and `execve_replace()` (calls into libc, never returns on
//! success). The integration test exercises `prepare()`; the bougie
//! binary calls both in sequence.

use crate::receipt::{self, ToolReceipt};
use bougie_paths::Paths;
use eyre::{Result, WrapErr, bail};
use std::ffi::OsString;
use std::path::{Path, PathBuf};

#[cfg(windows)]
const SCAN_DIR_SEP: &str = ";";
#[cfg(not(windows))]
const SCAN_DIR_SEP: &str = ":";

/// Everything `execve` needs, computed from the wrapper path + the
/// receipt next to it.
#[derive(Debug, Clone)]
pub struct ToolExecPrep {
    /// Absolute path to the pinned PHP binary.
    pub php_path: PathBuf,
    /// argv to hand PHP. argv[0] is the wrapper script path (so PHP
    /// runs it); user args append after.
    pub argv: Vec<OsString>,
    /// Env vars to set on top of the inherited environment.
    pub env: Vec<(String, OsString)>,
    /// The tool package name (for diagnostics).
    pub package: String,
    /// The receipt itself (kept around so the bougie binary can decide
    /// what to do on a failure — e.g. surface a recovery hint).
    pub receipt: ToolReceipt,
}

/// Produce a [`ToolExecPrep`] from the wrapper path the kernel handed
/// us. Pure with respect to the filesystem except for reading the
/// receipt.
pub fn prepare(
    paths: &Paths,
    wrapper: &Path,
    user_args: Vec<OsString>,
) -> Result<ToolExecPrep> {
    // Reject wrapper paths outside `$BOUGIE_LOCAL/tools/` or
    // `$BOUGIE_CACHE/tool-run/`. Without this a misconfigured shebang
    // on a user-controlled file could turn `bougie tool-exec` into a
    // "run this file under our pinned PHP" primitive, which isn't
    // its job. The cache root is allowed too because Phase 3's
    // `bougie tool run` materialises ephemeral tool dirs there.
    let canon_wrapper = canon(wrapper)?;
    if !is_allowed_wrapper_parent(paths, &canon_wrapper) {
        bail!(
            "bougie tool-exec refuses to run {}: not under {} or {}",
            wrapper.display(),
            paths.tools().display(),
            paths.cache_tool_run().display(),
        );
    }

    // Wrapper lives at $TOOL_DIR/bin/<name>; parent().parent() is the
    // tool dir.
    let tool_dir = canon_wrapper
        .parent()
        .and_then(Path::parent)
        .ok_or_else(|| {
            eyre::eyre!(
                "wrapper path {} is too shallow to belong to a tool dir",
                wrapper.display()
            )
        })?
        .to_path_buf();

    let receipt_path = tool_dir.join("receipt.toml");
    if !receipt_path.exists() {
        bail!(
            "bougie: tool dir {} is broken — receipt.toml missing. \
             Run `bougie tool upgrade --reinstall <vendor/name>` to recover.",
            tool_dir.display()
        );
    }
    let receipt = receipt::read(&receipt_path).map_err(|e| {
        eyre::eyre!(
            "bougie: tool dir {} is broken — receipt corrupt: {e}. \
             Run `bougie tool upgrade --reinstall <vendor/name>` to recover.",
            tool_dir.display()
        )
    })?;

    if !receipt.php_resolved_path.exists() {
        bail!(
            "bougie: tool `{pkg}` pinned to PHP {ver} which is no longer installed at {path}. \
             Reinstall the interpreter or run `bougie tool upgrade --reinstall {pkg}`.",
            pkg = receipt.package,
            ver = receipt.php_version,
            path = receipt.php_resolved_path.display(),
        );
    }

    // argv[0] = wrapper file. PHP needs this to find and run the script
    // — the wrapper itself takes care of fixing `$argv[0]` for clean
    // usage messages.
    let mut argv: Vec<OsString> = Vec::with_capacity(1 + user_args.len());
    argv.push(canon_wrapper.into_os_string());
    argv.extend(user_args);

    // PHP_INI_SCAN_DIR must include BOTH the install's bundled
    // conf.d (where baseline extensions like `phar`, `mbstring`,
    // `pdo` live) AND the tool's own conf.d (where `--with intl`
    // landed). Setting `PHP_INI_SCAN_DIR` overrides PHP's
    // compiled-in default, so without including the install's path
    // here, baseline extensions disappear and tools like phpstan
    // hit `Class "Phar" not found`.
    let install_conf_d = receipt
        .php_resolved_path
        .parent()
        .and_then(Path::parent)
        .map(|root| root.join("etc").join("php").join("conf.d"));
    let mut scan = OsString::new();
    if let Some(install_cd) = &install_conf_d {
        scan.push(install_cd);
        scan.push(SCAN_DIR_SEP);
    }
    scan.push(tool_dir.join("conf.d"));

    let env = vec![
        ("PHP_INI_SCAN_DIR".to_string(), scan),
        ("BOUGIE_TOOL".to_string(), receipt.package.clone().into()),
    ];

    Ok(ToolExecPrep {
        php_path: receipt.php_resolved_path.clone(),
        argv,
        env,
        package: receipt.package.clone(),
        receipt,
    })
}

/// Replace the current process with the pinned PHP. Returns `Err` only
/// when `execve` itself fails (e.g. the binary disappeared between
/// receipt-read and exec). On Unix this never returns `Ok`.
///
/// Layout of the resulting argv from PHP's perspective:
///
/// - argv[0] = "php" — PHP CLI parses its own options out of argv
///   before the first non-option, so we leave the program name alone
///   so the wrapper script lands at argv[1] as the *script*. (An
///   earlier version of this function set argv[0] to the wrapper
///   path; PHP then saw `--version` at argv[1] and printed PHP's
///   version banner instead of running the tool.)
/// - argv[1] = wrapper path (the script PHP will run)
/// - argv[2..] = user-supplied tool arguments
///
/// The wrapper itself rewrites `$argv[0]` back to the tool's bin
/// name so usage strings read "Usage: phpstan …" rather than
/// "Usage: php …".
#[cfg(unix)]
pub fn execve_replace(prep: &ToolExecPrep) -> Result<std::convert::Infallible> {
    use std::os::unix::process::CommandExt;
    let mut cmd = std::process::Command::new(&prep.php_path);
    cmd.args(&prep.argv);
    for (k, v) in &prep.env {
        cmd.env(k, v);
    }
    Err(eyre::eyre!(
        "exec {}: {}",
        prep.php_path.display(),
        cmd.exec()
    ))
}

#[cfg(not(unix))]
pub fn execve_replace(_prep: &ToolExecPrep) -> Result<std::convert::Infallible> {
    eyre::bail!("bougie tool-exec is Unix-only in Phase 1")
}

/// Best-effort canonicalization. `Path::canonicalize` requires the
/// path to exist; for callers that pass non-existent paths we want a
/// crisp "not under tools/" rejection rather than an ENOENT
/// pass-through.
fn canon(p: &Path) -> Result<PathBuf> {
    std::fs::canonicalize(p)
        .wrap_err_with(|| format!("resolving {}", p.display()))
}

/// True when `canon_wrapper` lives under one of the two roots
/// `tool-exec` accepts: persistent tools or the ephemeral run cache.
/// Missing roots (neither has ever been created) are treated as
/// "not under" — the wrapper can't be inside a dir that doesn't
/// exist.
fn is_allowed_wrapper_parent(paths: &Paths, canon_wrapper: &Path) -> bool {
    [paths.tools(), paths.cache_tool_run()]
        .into_iter()
        .any(|root| canon(&root).is_ok_and(|c| canon_wrapper.starts_with(c)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::receipt::{ToolEntrypoint, ToolReceipt};

    fn make_tool_dir(root: &Path, package: &str) -> PathBuf {
        let tool_dir = root.join("tools").join(package.replace('/', "-"));
        std::fs::create_dir_all(tool_dir.join("bin")).unwrap();
        std::fs::create_dir_all(tool_dir.join("conf.d")).unwrap();
        tool_dir
    }

    fn write_receipt(tool_dir: &Path, php_bin: &Path) -> ToolReceipt {
        let r = ToolReceipt {
            package: "phpstan/phpstan".into(),
            constraint: "^1.10".into(),
            php_version: "8.3.12".into(),
            php_flavor: "nts".into(),
            composer_version: "2.8.12".into(),
            with: vec![],
            php_resolved_path: php_bin.to_path_buf(),
            entrypoints: vec![ToolEntrypoint {
                name: "phpstan".into(),
                install_path: tool_dir.join("bin").join("phpstan"),
                from: "phpstan/phpstan".into(),
            }],
            extensions: vec![],
        };
        crate::receipt::write(&tool_dir.join("receipt.toml"), &r).unwrap();
        r
    }

    fn paths_for(td: &Path) -> Paths {
        Paths::new(td.to_path_buf(), td.join("cache"))
    }

    #[test]
    fn prepare_rejects_wrapper_outside_tools_dir() {
        let td = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(td.path().join("tools")).unwrap();
        let stray = td.path().join("not-tools").join("phpstan");
        std::fs::create_dir_all(stray.parent().unwrap()).unwrap();
        std::fs::write(&stray, "stray").unwrap();
        let paths = paths_for(td.path());
        let err = prepare(&paths, &stray, Vec::new()).unwrap_err().to_string();
        assert!(err.contains("not under"), "{err}");
    }

    #[test]
    fn prepare_reports_missing_receipt() {
        let td = tempfile::TempDir::new().unwrap();
        let tool_dir = make_tool_dir(td.path(), "phpstan/phpstan");
        let wrapper = tool_dir.join("bin").join("phpstan");
        std::fs::write(&wrapper, "<?php\n").unwrap();
        // no receipt.toml
        let paths = paths_for(td.path());
        let err = prepare(&paths, &wrapper, Vec::new()).unwrap_err().to_string();
        assert!(err.contains("receipt.toml missing"), "{err}");
    }

    #[test]
    fn prepare_reports_missing_php() {
        let td = tempfile::TempDir::new().unwrap();
        let tool_dir = make_tool_dir(td.path(), "phpstan/phpstan");
        let wrapper = tool_dir.join("bin").join("phpstan");
        std::fs::write(&wrapper, "<?php\n").unwrap();
        write_receipt(&tool_dir, Path::new("/does/not/exist/php"));
        let paths = paths_for(td.path());
        let err = prepare(&paths, &wrapper, Vec::new()).unwrap_err().to_string();
        assert!(err.contains("no longer installed"), "{err}");
    }

    #[test]
    fn prepare_builds_expected_argv_and_env() {
        let td = tempfile::TempDir::new().unwrap();
        let tool_dir = make_tool_dir(td.path(), "phpstan/phpstan");
        let wrapper = tool_dir.join("bin").join("phpstan");
        std::fs::write(&wrapper, "<?php\n").unwrap();
        // Use the test runner itself as a stand-in for "an existing
        // file" — we just need `php_resolved_path.exists()` to be true.
        let fake_php = std::env::current_exe().unwrap();
        write_receipt(&tool_dir, &fake_php);
        let paths = paths_for(td.path());
        let prep = prepare(&paths, &wrapper, vec![OsString::from("--version")]).unwrap();

        assert_eq!(prep.php_path, fake_php);
        assert_eq!(prep.argv.len(), 2);
        assert_eq!(prep.argv[1], OsString::from("--version"));
        // The wrapper passed in to argv[0] is canonicalized; just make
        // sure it points at the same file.
        assert_eq!(
            std::fs::canonicalize(&wrapper).unwrap(),
            PathBuf::from(&prep.argv[0])
        );

        // PHP_INI_SCAN_DIR layers the install's bundled conf.d (so
        // baseline extensions like phar/mbstring stay loaded) plus
        // the tool's own conf.d (where `--with intl` lands).
        let scan = prep
            .env
            .iter()
            .find_map(|(k, v)| (k == "PHP_INI_SCAN_DIR").then_some(v.clone()))
            .unwrap();
        let scan_str = scan.to_string_lossy();
        let tool_cd = tool_dir.join("conf.d");
        assert!(
            scan_str.contains(&*tool_cd.to_string_lossy()),
            "tool conf.d should be in scan dir; got {scan_str}"
        );
        assert!(
            scan_str.contains(':') || cfg!(windows),
            "expected layered scan dir (install + tool), got {scan_str}"
        );
        let tool_env = prep
            .env
            .iter()
            .find_map(|(k, v)| (k == "BOUGIE_TOOL").then_some(v.clone()))
            .unwrap();
        assert_eq!(tool_env, OsString::from("phpstan/phpstan"));
    }
}
