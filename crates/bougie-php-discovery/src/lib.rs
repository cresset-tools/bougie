//! Discover and probe **system** PHP interpreters — PHPs already
//! installed on the machine, not managed by bougie.
//!
//! This is the bottom layer of bougie's system-PHP support (uv's
//! system-Python model adapted to PHP). It answers two questions
//! cheaply:
//!
//! - **Discovery** ([`discover`]): which `php` binaries exist on this
//!   host? (`PATH` + a handful of well-known install locations.)
//! - **Probe** ([`probe`]): for a given `php`, what is its version,
//!   thread-safety flavor, and set of loaded extensions?
//!
//! The probe deliberately uses `php --version` (version + flavor) and `php --modules`
//! (loaded extensions) — both terse, stable, and sufficient for
//! selecting a system PHP against a project's `require.php` /
//! `require.ext-*`. Heavier introspection (`php -i`, `php --ini`) is
//! reserved for the specific later features that need it (ABI numbers
//! for opportunistic extension installs; the scan dir for an xdebug
//! overlay) and is not done here.
//!
//! This crate is pure model + parsing + process spawns: it does **no**
//! selection or policy. Choosing between a managed and a system PHP per
//! the user's preference lives one layer up.

pub mod select;

pub use select::{select, PhpPreference, Requirement, Selection};

use bougie_version::request::Flavor;
use bougie_version::version::Version;
use eyre::{eyre, Result, WrapErr};
use std::path::{Path, PathBuf};
use std::process::Command;

/// A system PHP interpreter that has been discovered and probed.
///
/// Only the fields needed to *select and use* a system PHP are
/// populated by [`probe`]. The `php -i`-derived ABI numbers and the
/// `php --ini` scan dir are intentionally absent here — they are
/// fetched lazily by the features that need them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SystemPhp {
    /// Canonical path to the `php` binary.
    pub path: PathBuf,
    /// Reported version (`major.minor.patch`, distro suffixes stripped).
    pub version: Version,
    /// Thread-safety / debug flavor parsed from the `php --version` banner.
    pub flavor: Flavor,
    /// Loaded extension names, lowercased (from `php --modules`). Covers both
    /// the `[PHP Modules]` and `[Zend Modules]` sections.
    pub extensions: Vec<String>,
}

impl SystemPhp {
    /// Whether this PHP already loads an extension, matched
    /// case-insensitively against a Composer `ext-<name>` short name
    /// (pass `<name>`, without the `ext-` prefix).
    ///
    /// Matches both the bare module name (`curl`) and the common
    /// `Zend `-prefixed form PHP prints for Zend extensions
    /// (`Zend OPcache` for `ext-opcache`).
    pub fn has_extension(&self, name: &str) -> bool {
        let want = name.to_ascii_lowercase();
        self.extensions.iter().any(|ext| {
            ext == &want || ext.strip_prefix("zend ").is_some_and(|rest| rest == want)
        })
    }
}

/// Probe a single `php` binary: run `php --version` + `php --modules` and parse them
/// into a [`SystemPhp`].
///
/// Errors if the binary can't be executed or its output can't be
/// parsed (e.g. it isn't actually a PHP CLI).
pub fn probe(php: &Path) -> Result<SystemPhp> {
    // Use the long flags (`--version`/`--modules`) rather than the short
    // `-v`/`-m`: PHP's version flag is the *lowercase* `-v` and uppercase
    // `-V` is rejected outright, so the spelled-out forms remove any
    // chance of that confusion.
    let version_out = run(php, &["--version"])?;
    let modules_out = run(php, &["--modules"])?;

    let (version, flavor) = parse_version_banner(&version_out)
        .wrap_err_with(|| format!("parsing `{} --version` output", php.display()))?;
    let extensions = parse_modules(&modules_out);

    let path = std::fs::canonicalize(php).unwrap_or_else(|_| php.to_path_buf());
    Ok(SystemPhp { path, version, flavor, extensions })
}

/// Run `php <args>` and capture stdout as a UTF-8 string.
fn run(php: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new(php)
        .args(args)
        .output()
        .wrap_err_with(|| format!("executing `{} {}`", php.display(), args.join(" ")))?;
    if !output.status.success() {
        return Err(eyre!(
            "`{} {}` exited with {}",
            php.display(),
            args.join(" "),
            output.status
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Parse the first line of `php --version` into `(version, flavor)`.
///
/// The banner's first line looks like:
///
/// ```text
/// PHP 8.3.12 (cli) (built: Sep 26 2024 02:24:53) (NTS)
/// PHP 8.1.2-1ubuntu2.19 (cli) (built: ...) ( NTS )
/// PHP 8.4.0RC1 (cli) (built: ...) ( ZTS DEBUG )
/// ```
///
/// The version token may carry a distro package suffix
/// (`-1ubuntu2.19`) or a pre-release tag (`RC1`); we keep only the
/// leading `major.minor.patch`. The flavor comes from the `NTS`/`ZTS`
/// and `DEBUG` words anywhere on the line.
pub fn parse_version_banner(output: &str) -> Result<(Version, Flavor)> {
    let first = output
        .lines()
        .next()
        .ok_or_else(|| eyre!("empty `php --version` output"))?;

    let rest = first
        .strip_prefix("PHP ")
        .ok_or_else(|| eyre!("`php --version` first line did not start with `PHP `: {first:?}"))?;
    let token = rest
        .split_whitespace()
        .next()
        .ok_or_else(|| eyre!("no version token in `php --version` line: {first:?}"))?;
    let version = parse_version_prefix(token)
        .ok_or_else(|| eyre!("could not parse a version from {token:?}"))?;

    let upper = first.to_ascii_uppercase();
    let zts = upper.contains("ZTS");
    let debug = upper.contains("DEBUG");
    let flavor = match (zts, debug) {
        (false, false) => Flavor::Nts,
        (false, true) => Flavor::NtsDebug,
        (true, false) => Flavor::Zts,
        (true, true) => Flavor::ZtsDebug,
    };

    Ok((version, flavor))
}

/// Take the leading `major.minor.patch` of a possibly-suffixed version
/// token (`8.1.2-1ubuntu2.19` → `8.1.2`, `8.4.0RC1` → `8.4.0`).
fn parse_version_prefix(token: &str) -> Option<Version> {
    let end = token
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(token.len());
    token[..end].parse::<Version>().ok()
}

/// Parse `php --modules` into a sorted, deduped, lowercased list of loaded
/// module names. Section headers (`[PHP Modules]`, `[Zend Modules]`)
/// and blank lines are dropped.
pub fn parse_modules(output: &str) -> Vec<String> {
    let mut mods: Vec<String> = output
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('['))
        .map(str::to_ascii_lowercase)
        .collect();
    mods.sort();
    mods.dedup();
    mods
}

/// Discover candidate `php` binaries on this host: `PATH` entries plus
/// a handful of well-known install locations, deduped by canonical
/// path. Does not probe them — pass each to [`probe`].
///
/// Discovery is best-effort: unreadable directories and broken symlinks
/// are skipped silently.
///
/// Set `BOUGIE_SYSTEM_PHP` to a falsy value (`0`, `false`, `off`, `no`,
/// `never`) to disable system-PHP discovery entirely — an escape hatch
/// for strictly-managed / reproducible setups, and what hermetic tests
/// use so the real host PHP can't leak in.
pub fn discover() -> Vec<PathBuf> {
    if system_php_disabled() {
        return Vec::new();
    }
    let mut found = Vec::new();
    let mut seen = std::collections::HashSet::new();

    let mut consider = |candidate: PathBuf, found: &mut Vec<PathBuf>| {
        let canonical = std::fs::canonicalize(&candidate).unwrap_or(candidate);
        if seen.insert(canonical.clone()) {
            found.push(canonical);
        }
    };

    // PATH entries named `php` or a version-suffixed form (`php8.3`,
    // `php83`).
    if let Some(path) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path) {
            collect_php_in(&dir, |c| consider(c, &mut found));
        }
    }

    // Well-known locations not necessarily on PATH.
    for dir in well_known_dirs() {
        collect_php_in(&dir, |c| consider(c, &mut found));
    }

    found
}

/// Whether `BOUGIE_SYSTEM_PHP` is set to an explicit falsy value,
/// disabling system-PHP discovery.
fn system_php_disabled() -> bool {
    std::env::var("BOUGIE_SYSTEM_PHP").is_ok_and(|v| is_falsy(&v))
}

/// Whether an env value reads as an explicit "off" toggle.
fn is_falsy(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "0" | "false" | "off" | "no" | "never"
    )
}

/// Names a `php` binary file in `dir` can take.
fn is_php_name(name: &str) -> bool {
    if name == "php" || name == "php.exe" {
        return true;
    }
    // `php8.3`, `php83`, `php8.3.exe` — `php` followed by digits/dots.
    let stem = name.strip_suffix(".exe").unwrap_or(name);
    stem.strip_prefix("php")
        .is_some_and(|rest| !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit() || b == b'.'))
}

/// Invoke `f` for every `php`-named entry in `dir`.
fn collect_php_in(dir: &Path, mut f: impl FnMut(PathBuf)) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if is_php_name(name) {
            f(entry.path());
        }
    }
}

/// Well-known PHP install dirs to scan in addition to `PATH`, expanded
/// from glob-free literals plus any matching `*`-versioned siblings.
fn well_known_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();

    #[cfg(unix)]
    {
        for base in ["/usr/bin", "/usr/local/bin", "/opt/php/bin"] {
            dirs.push(PathBuf::from(base));
        }
        // Homebrew keg-only php@<ver> and /opt/php*<ver>.
        for parent in ["/opt/homebrew/opt", "/usr/local/opt", "/opt"] {
            push_glob_children(Path::new(parent), "php", "bin", &mut dirs);
        }
    }

    #[cfg(windows)]
    {
        for parent in ["C:\\", "C:\\tools"] {
            push_glob_children(Path::new(parent), "php", "", &mut dirs);
        }
    }

    dirs
}

/// Push `<parent>/<entry>/<suffix>` for every child of `parent` whose
/// name starts with `prefix` (e.g. `php@8.3`, `php8.4`). `suffix` is
/// the bin subdir (`"bin"`) or `""` (Windows, binaries at the root).
fn push_glob_children(parent: &Path, prefix: &str, suffix: &str, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(parent) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if name.starts_with(prefix) {
            let mut dir = entry.path();
            if !suffix.is_empty() {
                dir.push(suffix);
            }
            out.push(dir);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_nts_banner() {
        let out = "PHP 8.3.12 (cli) (built: Sep 26 2024 02:24:53) (NTS)\n\
                   Copyright (c) The PHP Group\n";
        let (v, f) = parse_version_banner(out).unwrap();
        assert_eq!(v, Version::new(8, 3, 12));
        assert_eq!(f, Flavor::Nts);
    }

    #[test]
    fn parse_zts_banner_spaced() {
        let out = "PHP 8.2.10 (cli) (built: ...) ( ZTS )\n";
        let (v, f) = parse_version_banner(out).unwrap();
        assert_eq!(v, Version::new(8, 2, 10));
        assert_eq!(f, Flavor::Zts);
    }

    #[test]
    fn parse_nts_debug_banner() {
        let out = "PHP 8.4.1 (cli) (built: ...) ( NTS DEBUG )\n";
        let (_, f) = parse_version_banner(out).unwrap();
        assert_eq!(f, Flavor::NtsDebug);
    }

    #[test]
    fn parse_zts_debug_banner() {
        let out = "PHP 8.4.1 (cli) (built: ...) ( ZTS DEBUG )\n";
        let (_, f) = parse_version_banner(out).unwrap();
        assert_eq!(f, Flavor::ZtsDebug);
    }

    #[test]
    fn parse_fedora_banner_with_compiler_suffix() {
        // Fedora prints extra tokens after the flavor: `(NTS gcc x86_64)`.
        let out = "PHP 8.3.27 (cli) (built: Oct 21 2025 14:53:41) (NTS gcc x86_64)\n";
        let (v, f) = parse_version_banner(out).unwrap();
        assert_eq!(v, Version::new(8, 3, 27));
        assert_eq!(f, Flavor::Nts);
    }

    #[test]
    fn strips_debian_package_suffix() {
        let out = "PHP 8.1.2-1ubuntu2.19 (cli) (built: ...) (NTS)\n";
        let (v, _) = parse_version_banner(out).unwrap();
        assert_eq!(v, Version::new(8, 1, 2));
    }

    #[test]
    fn strips_prerelease_suffix() {
        let out = "PHP 8.4.0RC1 (cli) (built: ...) ( ZTS )\n";
        let (v, f) = parse_version_banner(out).unwrap();
        assert_eq!(v, Version::new(8, 4, 0));
        assert_eq!(f, Flavor::Zts);
    }

    #[test]
    fn rejects_non_php_banner() {
        assert!(parse_version_banner("Python 3.11.0\n").is_err());
        assert!(parse_version_banner("").is_err());
    }

    #[test]
    fn parses_modules() {
        let out = "[PHP Modules]\n\
                   Core\n\
                   ctype\n\
                   curl\n\
                   \n\
                   [Zend Modules]\n\
                   Zend OPcache\n\
                   Xdebug\n";
        let mods = parse_modules(out);
        assert_eq!(
            mods,
            vec![
                "core".to_string(),
                "ctype".to_string(),
                "curl".to_string(),
                "xdebug".to_string(),
                "zend opcache".to_string(),
            ]
        );
    }

    #[test]
    fn has_extension_matches_plain_and_zend() {
        let php = SystemPhp {
            path: PathBuf::from("/usr/bin/php"),
            version: Version::new(8, 3, 12),
            flavor: Flavor::Nts,
            extensions: parse_modules(
                "[PHP Modules]\ncurl\nintl\n[Zend Modules]\nZend OPcache\n",
            ),
        };
        assert!(php.has_extension("curl"));
        assert!(php.has_extension("CURL")); // case-insensitive request
        assert!(php.has_extension("intl"));
        assert!(php.has_extension("opcache")); // matched via `Zend OPcache`
        assert!(!php.has_extension("redis"));
    }

    #[test]
    fn falsy_env_values_disable_discovery() {
        for v in ["0", "false", "FALSE", "off", "no", "Never", " 0 "] {
            assert!(is_falsy(v), "{v:?} should disable");
        }
        for v in ["1", "true", "yes", "on", ""] {
            assert!(!is_falsy(v), "{v:?} should not disable");
        }
    }

    #[test]
    fn is_php_name_accepts_versioned_forms() {
        for name in ["php", "php8.3", "php83", "php8.4"] {
            assert!(is_php_name(name), "{name} should be a php binary name");
        }
        for name in ["phpunit", "phpize", "php-config", "phpdbg", "phar"] {
            assert!(!is_php_name(name), "{name} should not match");
        }
    }

    #[cfg(windows)]
    #[test]
    fn is_php_name_accepts_exe() {
        assert!(is_php_name("php.exe"));
        assert!(is_php_name("php8.3.exe"));
        assert!(!is_php_name("phpunit.exe"));
    }

    /// Write an executable `php` shell stub that answers `--version` and `--modules`
    /// like a real CLI, and probe it end-to-end through `Command`.
    #[cfg(unix)]
    #[test]
    fn probe_runs_a_real_stub() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let php = dir.path().join("php");
        std::fs::write(
            &php,
            "#!/bin/sh\n\
             case \"$1\" in\n\
               --version) echo 'PHP 8.3.12 (cli) (built: x) (NTS)';;\n\
               --modules) printf '[PHP Modules]\\ncurl\\nintl\\n[Zend Modules]\\nZend OPcache\\n';;\n\
             esac\n",
        )
        .unwrap();
        std::fs::set_permissions(&php, std::fs::Permissions::from_mode(0o755)).unwrap();

        let probed = probe(&php).unwrap();
        assert_eq!(probed.version, Version::new(8, 3, 12));
        assert_eq!(probed.flavor, Flavor::Nts);
        assert!(probed.has_extension("curl"));
        assert!(probed.has_extension("opcache"));
        assert!(!probed.has_extension("redis"));
    }
}
