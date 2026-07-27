//! Port of `Composer\Autoload\AutoloadGenerator::getPlatformCheck`.
//!
//! Emits `vendor/composer/platform_check.php` — the runtime guard
//! `autoload_real.php` `require`s before registering the loader. It
//! aggregates the `php` / `php-64bit` / `ext-*` requirements across the
//! root package and every *prod* locked package (dev packages are
//! excluded, matching Composer's `$devPackageNames` skip) and renders a
//! script that raises a `RuntimeException` when the running PHP doesn't
//! satisfy them.
//!
//! Output is byte-equivalent to Composer 2.8.12. The template below
//! reproduces the upstream heredocs verbatim (the newline immediately
//! before each closing identifier is dropped, which is already baked
//! into the literal `\n` counts here).

use std::collections::BTreeMap;
use std::fmt::Write as _;

use composer_semver::Constraint;

use crate::lock::PlatformCheck;

/// One package's platform-relevant link maps (target → constraint
/// string). Borrowed from the lock / root manifest at call time.
pub(crate) struct PkgLinks<'a> {
    pub require: &'a BTreeMap<String, String>,
    pub replace: &'a BTreeMap<String, String>,
    pub provide: &'a BTreeMap<String, String>,
}

/// Generate the `platform_check.php` body, or `None` when there's
/// nothing to check (Composer returns `null`, and the caller then skips
/// both the file and the `require` line in `autoload_real.php`).
///
/// `check` is the `config.platform-check` mode: [`PlatformCheck::Disabled`]
/// short-circuits to `None`; [`PlatformCheck::PhpOnly`] emits only the
/// PHP-version guard; [`PlatformCheck::Strict`] additionally emits
/// per-extension `extension_loaded()` guards.
pub(crate) fn generate(packages: &[PkgLinks<'_>], check: PlatformCheck) -> Option<String> {
    if check == PlatformCheck::Disabled {
        return None;
    }
    let strict = check == PlatformCheck::Strict;

    // extensionProviders: ext name (post-`ext-`, pre-rename) → the
    // constraints under which it's replaced/provided.
    let mut providers: BTreeMap<String, Vec<Constraint>> = BTreeMap::new();
    for pkg in packages {
        for (target, cstr) in pkg.replace.iter().chain(pkg.provide.iter()) {
            if let Some(ext) = strip_ext(target)
                && let Ok(c) = Constraint::parse(cstr)
            {
                providers.entry(ext).or_default().push(c);
            }
        }
    }

    let mut lowest_php = composer_semver::Bound::zero();
    let mut required_php64 = false;
    // Keyed by the `var_export`ed extension literal (e.g. `'json'`) so
    // BTreeMap iteration reproduces Composer's `ksort($requiredExtensions)`.
    let mut required_ext: BTreeMap<String, String> = BTreeMap::new();

    for pkg in packages {
        'links: for (target, cstr) in pkg.require {
            if (target == "php" || target == "php-64bit")
                && let Ok(c) = Constraint::parse(cstr)
            {
                let lb = c.lower_bound();
                if lb.compare_to(&lowest_php, true) {
                    lowest_php = lb;
                }
            }
            if target == "php-64bit" {
                required_php64 = true;
            }
            if strict && let Some(ext) = strip_ext(target) {
                // Skip if a provider covers this requirement.
                if let Some(provs) = providers.get(&ext)
                    && let Ok(req_c) = Constraint::parse(cstr)
                    && provs.iter().any(|p| p.intersects(&req_c))
                {
                    continue 'links;
                }
                // Composer renames the opcache ext for the human-facing
                // `extension_loaded` argument.
                let ext_name = if ext == "zend-opcache" {
                    "zend opcache".to_owned()
                } else {
                    ext
                };
                let exported = var_export_string(&ext_name);
                // pcntl / readline are CLI-only — guard with the SAPI
                // check so a non-CLI SAPI without them isn't flagged.
                let line = if ext_name == "pcntl" || ext_name == "readline" {
                    format!(
                        "PHP_SAPI !== 'cli' || extension_loaded({exported}) || $missingExtensions[] = {exported};\n"
                    )
                } else {
                    format!("extension_loaded({exported}) || $missingExtensions[] = {exported};\n")
                };
                required_ext.insert(exported, line);
            }
        }
    }

    // ---- PHP version + 64-bit block (PHP_CHECK heredoc) -------------
    let mut required_php = String::new();
    if !lowest_php.is_zero() {
        let operator = if lowest_php.is_inclusive() { ">=" } else { ">" };
        let id = version_id(lowest_php.version());
        let human = human_readable(lowest_php.version());
        let _ = write!(
            required_php,
            "\nif (!(PHP_VERSION_ID {operator} {id})) {{\n    $issues[] = 'Your Composer dependencies require a PHP version \"{operator} {human}\". You are running ' . PHP_VERSION . '.';\n}}\n"
        );
    }
    if required_php64 {
        required_php.push_str(
            "\nif (PHP_INT_SIZE !== 8) {\n    $issues[] = 'Your Composer dependencies require a 64-bit build of PHP.';\n}\n",
        );
    }

    // ---- extension block (EXT_CHECKS heredoc) ----------------------
    let ext_lines: String = required_ext.into_values().collect();
    let required_extensions = if ext_lines.is_empty() {
        String::new()
    } else {
        format!(
            "\n$missingExtensions = array();\n{ext_lines}\nif ($missingExtensions) {{\n    $issues[] = 'Your Composer dependencies require the following PHP extensions to be installed: ' . implode(', ', $missingExtensions) . '.';\n}}\n"
        )
    };

    if required_php.is_empty() && required_extensions.is_empty() {
        return None;
    }

    Some(format!(
        "<?php\n\n// platform_check.php @generated by Composer\n\n$issues = array();\n{required_php}{required_extensions}\nif ($issues) {{\n    if (!headers_sent()) {{\n        header('HTTP/1.1 500 Internal Server Error');\n    }}\n    if (!ini_get('display_errors')) {{\n        if (PHP_SAPI === 'cli' || PHP_SAPI === 'phpdbg') {{\n            fwrite(STDERR, 'Composer detected issues in your platform:' . PHP_EOL.PHP_EOL . implode(PHP_EOL, $issues) . PHP_EOL.PHP_EOL);\n        }} elseif (!headers_sent()) {{\n            echo 'Composer detected issues in your platform:' . PHP_EOL.PHP_EOL . str_replace('You are running '.PHP_VERSION.'.', '', implode(PHP_EOL, $issues)) . PHP_EOL.PHP_EOL;\n        }}\n    }}\n    throw new \\RuntimeException(\n        'Composer detected issues in your platform: ' . implode(' ', $issues)\n    );\n}}\n"
    ))
}

/// Strip a case-insensitive `ext-` prefix, returning the (case-preserved)
/// remainder. Mirrors Composer's `{^ext-(.+)$}iD`.
fn strip_ext(target: &str) -> Option<String> {
    if target.len() > 4 && target.as_bytes()[..4].eq_ignore_ascii_case(b"ext-") {
        Some(target[4..].to_owned())
    } else {
        None
    }
}

/// PHP `var_export($s, true)` for a string: single-quoted with `\` and
/// `'` backslash-escaped. Extension names never contain either, but we
/// escape anyway to stay faithful.
fn var_export_string(s: &str) -> String {
    format!("'{}'", s.replace('\\', "\\\\").replace('\'', "\\'"))
}

/// Composer's `formatToPhpVersionId(Bound)`: split the normalized
/// version on `.`/`-`, `intval` each chunk, then
/// `major*10000 + minor*100 + patch`. Non-numeric chunks (a stability
/// tail like `dev`) intval to 0, exactly as in PHP.
fn version_id(version: &str) -> u32 {
    let chunks: Vec<u32> = version
        .replace('-', ".")
        .split('.')
        .map(|c| c.parse::<u32>().unwrap_or(0))
        .collect();
    let at = |i: usize| chunks.get(i).copied().unwrap_or(0);
    at(0) * 10000 + at(1) * 100 + at(2)
}

/// Composer's `formatToHumanReadable(Bound)`: the first three
/// dot/dash-separated chunks of the normalized version, re-joined with
/// `.`.
fn human_readable(version: &str) -> String {
    version
        .replace('-', ".")
        .split('.')
        .take(3)
        .collect::<Vec<_>>()
        .join(".")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    fn links<'a>(require: &'a BTreeMap<String, String>) -> PkgLinks<'a> {
        // Leak two empty maps with 'static lifetime for the borrow.
        static EMPTY: std::sync::OnceLock<BTreeMap<String, String>> = std::sync::OnceLock::new();
        let empty = EMPTY.get_or_init(BTreeMap::new);
        PkgLinks {
            require,
            replace: empty,
            provide: empty,
        }
    }

    #[test]
    fn disabled_emits_nothing() {
        let req = map(&[("php", ">=8.1")]);
        assert!(generate(&[links(&req)], PlatformCheck::Disabled).is_none());
    }

    #[test]
    fn php_only_skips_extension_guards() {
        let req = map(&[("php", ">=8.1"), ("ext-json", "*")]);
        let out = generate(&[links(&req)], PlatformCheck::PhpOnly).unwrap();
        assert!(out.contains("PHP_VERSION_ID >= 80100"));
        assert!(!out.contains("extension_loaded"));
    }

    #[test]
    fn strict_emits_ksorted_extension_guards_with_special_cases() {
        let req = map(&[
            ("php", ">=8.1"),
            ("ext-mbstring", "*"),
            ("ext-json", "*"),
            ("ext-pcntl", "*"),
            ("ext-zend-opcache", "*"),
        ]);
        let out = generate(&[links(&req)], PlatformCheck::Strict).unwrap();
        // ksort: json before mbstring before pcntl before zend opcache.
        let json = out.find("'json'").unwrap();
        let mb = out.find("'mbstring'").unwrap();
        let pcntl = out.find("'pcntl'").unwrap();
        let opcache = out.find("'zend opcache'").unwrap();
        assert!(json < mb && mb < pcntl && pcntl < opcache);
        // pcntl is CLI-gated; json is not.
        assert!(out.contains("PHP_SAPI !== 'cli' || extension_loaded('pcntl')"));
        assert!(out.contains("extension_loaded('json') || $missingExtensions[] = 'json';"));
    }

    #[test]
    fn no_requirements_returns_none() {
        let req = map(&[("acme/lib", "*")]);
        assert!(generate(&[links(&req)], PlatformCheck::Strict).is_none());
    }

    #[test]
    fn version_id_and_human_readable_match_composer() {
        assert_eq!(version_id("8.1.0.0"), 80100);
        assert_eq!(version_id("8.0.0.0-dev"), 80000);
        assert_eq!(version_id("7.4.0.0"), 70400);
        assert_eq!(human_readable("8.1.0.0"), "8.1.0");
        assert_eq!(human_readable("8.0.0.0-dev"), "8.0.0");
    }
}
