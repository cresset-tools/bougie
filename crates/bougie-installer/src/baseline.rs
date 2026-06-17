//! The baseline extension set per CLI.md §3.5.1.1.
//!
//! After `REFACTOR_DEBIAN_ALIGNED.md` (php-build-standalone), the bougie
//! baseline starts from Debian's `apt install php8.2-cli` transitive
//! closure — the `.so` extensions that `php8.2-common`,
//! `php8.2-opcache`, and `php8.2-readline` add on top of the bare
//! interpreter. The interpreter tarball ships zero `.so` files;
//! baseline is what makes `bougie php install <ver>` reproduce the
//! "I just installed PHP and it behaves the way I expect" experience.
//!
//! Four extensions to the Debian-strict set live here because we target
//! the *Composer ecosystem* and not just "what the OS calls PHP":
//!
//! - **XML family** (`dom`, `simplexml`, `xml`, `xmlreader`,
//!   `xmlwriter`). Debian splits these into `php-xml`, an explicit
//!   opt-in. Every real-world Composer project's transitive tree
//!   requires at least `ext-xml` — phpunit, phpmd, symfony/console,
//!   monolog, doctrine all gate on it. Including the family in
//!   baseline costs ~120KB of `.so` loaded per invocation and makes
//!   `composer install` work first-shot for the median project.
//!
//! - **`mbstring`**. Debian ships this in a separate `php8.2-mbstring`
//!   package. Baseline because Laravel's `Illuminate\Support\Str`
//!   reaches for `mb_split` / `mb_str_pad` / `mb_strtolower` on every
//!   `studly` / kebab-case helper call (and the migration runner uses
//!   `studly` to derive class names from filenames — so `php artisan
//!   migrate` fatals at boot without it). The broader Composer
//!   ecosystem — symfony/string, monolog, league/csv, phpunit's own
//!   data-provider machinery — gates similarly on `ext-mbstring`.
//!   Costs ~200 KB per invocation. On Windows the DLL rides along in
//!   the windows.php.net ZIP (`bin/ext/php_mbstring.dll`), so the
//!   bundled-DLL baseline path picks it up automatically.
//!
//! - **`mysqlnd`**. The php-build-standalone Debian-faithful build
//!   compiles `--enable-mysqlnd=shared`, so `mysqlnd.so` is a
//!   separate artifact. PHP's `pdo_mysql` and `mysqli` declare
//!   `ZEND_MOD_REQUIRED("mysqlnd")` at MINIT time — `pdo_mysql.so`
//!   refuses to initialize if `mysqlnd` isn't already loaded. Without
//!   it in baseline, a project that pulls in `pdo_mysql` via
//!   composer.json gets `Cannot load module "pdo_mysql" because
//!   required module "mysqlnd" is not loaded` on every PHP
//!   invocation. The numeric prefix on `20-mysqlnd.ini` keeps it
//!   loading before `35-pdo_mysql.ini` and `40-mysqli.ini`.
//!
//! - **`SQLite`** (`pdo_sqlite`, `sqlite3`). Debian ships these in a
//!   separate `php8.2-sqlite3` package. They're baseline because
//!   Laravel's default `DB_CONNECTION=sqlite` and the typical Composer-
//!   project test suite (phpunit fixtures, pest in-memory DBs) expect
//!   sqlite to be loadable without an explicit `bougie ext add`. The
//!   pair lives together — `pdo_sqlite` without `sqlite3` strips the
//!   procedural API that test bootstraps reach for. Costs ~150KB
//!   per invocation. On Windows the DLLs ride along in the
//!   windows.php.net ZIP (`bin/ext/php_pdo_sqlite.dll`,
//!   `bin/ext/php_sqlite3.dll`), so the Windows baseline path picks
//!   them up automatically without a [`WINDOWS_DLL_BASELINE_EXTRAS`]
//!   entry — that list is reserved for [`BUILTIN_EXTENSIONS`] members
//!   like openssl whose Linux build is static.
//!
//! The list is compiled into the bougie binary — a bougie release
//! changes the baseline, not an index publication — which keeps
//! `bougie php install` deterministic per bougie version even if the
//! index later advertises new extensions.

use std::collections::BTreeSet;

use bougie_version::version::PartialVersion;

/// Ordered list of baseline extension names — Debian's `php8.2-cli`
/// transitive closure. Order is observable (it's the order conf.d
/// fragments and the JSON `baseline` array appear in) but PHP's
/// alphabetic conf.d scan re-orders fragments at load time, so this
/// is purely cosmetic.
///
/// Platform notes (applied at install time via [`skip_for_platform`]):
/// - `gettext`: Linux only. Apple's libc lacks a real libintl
///   implementation; the php-build-standalone Darwin build emits no
///   gettext.so, so the index has no entry to fetch.
///
/// Per-PHP-minor notes (applied at install time via [`skip_for_php_minor`]):
/// - `opcache`: ships as a .so on 8.1–8.4 only. PHP 8.5+ builds
///   opcache statically into bin/php — `extension_loaded("Zend
///   OPcache")` returns true already, so an index lookup would fail
///   and surface as an alarming `baseline_failed` entry. Silently
///   skipped on 8.5+; the conf.d fragment is unnecessary because the
///   extension is loaded by the interpreter itself.
pub const BASELINE_EXTENSIONS: &[&str] = &[
    // php8.2-common transitive closure
    "calendar",
    "ctype",
    "exif",
    "ffi",
    "fileinfo",
    "ftp",
    "gettext",
    "iconv",
    "pdo",
    "phar",
    "posix",
    "shmop",
    "sockets",
    "sysvmsg",
    "sysvsem",
    "sysvshm",
    "tokenizer",
    // php8.2-opcache (8.1–8.4 only; static on 8.5+)
    "opcache",
    // php8.2-readline
    "readline",
    // XML family — Debian splits into `php-xml`, we baseline because
    // the Composer median project needs it. See module docs.
    "dom",
    "simplexml",
    "xml",
    "xmlreader",
    "xmlwriter",
    // mbstring — Laravel's Str helpers and most Composer libs gate
    // on mb_* functions. See module docs.
    "mbstring",
    // mysqlnd — required for pdo_mysql / mysqli to initialize. See
    // module docs.
    "mysqlnd",
    // sqlite — Laravel's default DB driver and the typical phpunit
    // fixture backend. See module docs.
    "pdo_sqlite",
    "sqlite3",
    // curl, so often needed we should just have it
    "curl",
];

/// `true` when the named baseline extension is not available on the
/// current target OS and should be skipped silently by
/// [`crate::install::install_baseline_into`]. The set is intentionally
/// hardcoded — these are upstream-library facts (Apple's libc), not
/// runtime probes.
pub fn skip_for_platform(name: &str) -> bool {
    matches!(name, "gettext") && !cfg!(target_os = "linux")
}

/// `true` when the named baseline extension is statically built into
/// the interpreter for the given PHP minor and should be skipped
/// silently by [`crate::install::install_baseline_into`]. Mirrors
/// [`skip_for_platform`] but for upstream-PHP-build facts that vary
/// per minor:
///
/// - `opcache` on PHP 8.5+: upstream switched opcache from a shared
///   `.so` to static-into-`bin/php`. The php-build-standalone index
///   emits no `opcache` artifact for 8.5+, so a fetch attempt always
///   fails. `extension_loaded("Zend OPcache")` is already true at
///   that point — skipping silently produces the same user-visible
///   outcome without the alarming `baseline_failed` line.
///
/// Anything off this list passes (returns `false`).
pub fn skip_for_php_minor(name: &str, php_minor: PartialVersion) -> bool {
    name == "opcache" && is_php_8_5_or_newer(php_minor)
}

fn is_php_8_5_or_newer(v: PartialVersion) -> bool {
    match v.minor {
        Some(minor) => v.major > 8 || (v.major == 8 && minor >= 5),
        None => v.major >= 9,
    }
}

/// Extensions that the Linux/Debian-aligned build statically links into
/// `php.so` (so they appear in [`BUILTIN_EXTENSIONS`] and get no conf.d
/// fragment from [`crate::install::install_baseline_into`]) but that
/// windows.php.net ships as ride-along DLLs under `<install>/bin/ext/`.
/// Without conf.d activation, `php_openssl.dll` etc. sit on disk
/// unloaded and the first composer install errors with
/// "The openssl extension is required for SSL/TLS protection".
///
/// Iterated by `install_baseline_from_bundled_windows` on top of
/// [`BASELINE_EXTENSIONS`]. `BaselineFilter` applies to both lists
/// — `--without openssl` opts out symmetrically.
///
/// Strictly Linux-static-but-Windows-DLL: members of
/// [`BUILTIN_EXTENSIONS`] whose `php_<name>.dll` actually ships in the
/// Windows ZIP. Verified empirically against `8.3.31-nts-vs16-x64.zip`'s
/// `ext/` listing.
pub const WINDOWS_DLL_BASELINE_EXTRAS: &[&str] = &["openssl", "sodium"];

/// Extensions to pre-download into the content-addressed store as part
/// of a default `bougie php install` / `bougie sync`, but NOT enable
/// via any conf.d fragment. The motivating case is xdebug: every PHP
/// dev wants it cached so the first `?XDEBUG_TRIGGER` request doesn't
/// stall on a 5 MB download, but loading it on every CLI invocation
/// would tank `bougie run` speed (and any normal HTTP request).
///
/// Activation is the caller's job:
/// - `bougie server` writes a fragment under `vendor/bougie/conf.d-debug/`
///   the first time the xdebug pool variant is hit — see
///   `commands/server/pool.rs::ensure_debug_extension`.
/// - `bougie ext add xdebug` makes the activation explicit and
///   permanent (also lands in `conf.d-debug/`).
///
/// Skipped when `--bare` is set on `bougie php install` (the same flag
/// that strips the baseline set down to nothing).
pub const PREINSTALLED_EXTENSIONS: &[&str] = &["xdebug"];

/// Extensions that are *statically compiled into the PHP binary* and
/// can never be loaded as `.so` files. `composer.json` projects
/// frequently declare `ext-pcre` / `ext-json` / `ext-spl` for
/// platform-validation reasons; bougie must treat those as
/// already-satisfied rather than attempting an index lookup that
/// will fail.
///
/// Derived from PHP's default `get_loaded_extensions()` set on a
/// stock build plus the configure-time-static deps our
/// `php-build-standalone` pipeline links in. After
/// `REFACTOR_DEBIAN_ALIGNED.md` (Phase A), this matches Debian's
/// `php8.2-cli` static set: openssl, sodium, session, filter, pcntl
/// joined the always-built core (Core, date, hash, json, libxml,
/// pcre, random, Reflection, SPL, standard, zlib).
///
/// Keep lowercase — `composer.json` keys are case-sensitive and the
/// idiomatic spelling is lowercase.
pub const BUILTIN_EXTENSIONS: &[&str] = &[
    "core",
    "date",
    "filter",
    "hash",
    "json",
    "libxml",
    "openssl",
    "pcntl",
    "pcre",
    "random",
    "reflection",
    "session",
    "sodium",
    "spl",
    "standard",
    "zlib",
];

/// Per-invocation narrowing applied by `bougie php install`'s `--bare`
/// / `--without` flags. Project-level opt-out (the `false` sentinel
/// in `[extensions]`) is a separate concern handled at sync time.
#[derive(Debug, Clone)]
pub enum BaselineFilter {
    /// Install the full baseline set. Default.
    All,
    /// Install nothing from the baseline. `--bare`.
    None,
    /// Install the baseline set MINUS the named subset. `--without
    /// <name>` (repeatable).
    Without(BTreeSet<String>),
}

impl BaselineFilter {
    /// Whether the given extension name passes the filter.
    pub fn includes(&self, name: &str) -> bool {
        match self {
            Self::All => true,
            Self::None => false,
            Self::Without(set) => !set.contains(name),
        }
    }
}

/// `true` if `name` is in [`BASELINE_EXTENSIONS`]. Used by `sync` to
/// decide whether an `ext-foo = false` opt-out actually applies (only
/// baseline extensions are opt-outable; core is non-negotiable).
pub fn is_baseline(name: &str) -> bool {
    BASELINE_EXTENSIONS.contains(&name)
}

/// `true` if `name` names an extension that's statically compiled
/// into every PHP binary (see [`BUILTIN_EXTENSIONS`]). Sync uses this
/// to short-circuit `composer.json`'s platform-validation entries
/// like `ext-pcre` before attempting an index lookup that has no
/// chance of succeeding.
pub fn is_builtin(name: &str) -> bool {
    BUILTIN_EXTENSIONS.contains(&name)
}

/// Validate `--without <name>` arguments against the baseline set and
/// return the filter. Names not in [`BASELINE_EXTENSIONS`] are
/// rejected — `--without` is a baseline-narrowing flag, not a
/// general-purpose exclusion list. An empty `names` slice maps to
/// [`BaselineFilter::All`] (no narrowing).
pub fn parse_without(names: &[String]) -> Result<BaselineFilter, String> {
    let set: BTreeSet<String> = names
        .iter()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    for name in &set {
        if !is_baseline(name) {
            return Err(format!(
                "`--without {name}` names an extension that isn't in the baseline set; \
                 use `bougie ext remove {name}` after install instead"
            ));
        }
    }
    if set.is_empty() {
        Ok(BaselineFilter::All)
    } else {
        Ok(BaselineFilter::Without(set))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn baseline_covers_debian_closure_plus_composer_essentials() {
        // Debian's `php8.2-cli` transitive closure (from
        // REFACTOR_DEBIAN_ALIGNED.md §"Default-install set").
        assert!(is_baseline("calendar"));
        assert!(is_baseline("ctype"));
        assert!(is_baseline("opcache"));
        assert!(is_baseline("readline"));
        assert!(is_baseline("ffi"));
        // Composer-essential additions (see module docs):
        // XML family — needed by phpunit/symfony/monolog/everything.
        assert!(is_baseline("dom"));
        assert!(is_baseline("simplexml"));
        assert!(is_baseline("xml"));
        assert!(is_baseline("xmlreader"));
        assert!(is_baseline("xmlwriter"));
        // mysqlnd — pdo_mysql / mysqli refuse to init without it
        // loaded first (ZEND_MOD_REQUIRED).
        assert!(is_baseline("mysqlnd"));
        // SQLite — Laravel default DB driver / phpunit fixture backend.
        assert!(is_baseline("pdo_sqlite"));
        assert!(is_baseline("sqlite3"));
        // mbstring — universal Composer-ecosystem dep (Laravel Str
        // helpers, symfony/string, monolog).
        assert!(is_baseline("mbstring"));
        // Composer-domain exts that still ship as per-ext tarballs
        // (loud on every PHP install if they were baselined; users add
        // them explicitly via `bougie ext add` or implicitly via
        // composer.json's `require.ext-*`).
        assert!(!is_baseline("intl"));
        assert!(!is_baseline("zip"));
        // PECL exts that are never baseline.
        assert!(!is_baseline("xdebug"));
        assert!(!is_baseline("redis"));
    }

    #[test]
    fn preinstalled_and_baseline_are_disjoint() {
        // A name in both would be enabled by baseline and so the
        // preinstall (no-conf.d) semantic would be meaningless.
        for &name in PREINSTALLED_EXTENSIONS {
            assert!(
                !is_baseline(name),
                "{name} is in both BASELINE_EXTENSIONS and PREINSTALLED_EXTENSIONS"
            );
        }
    }

    #[test]
    fn xdebug_is_preinstalled_by_default() {
        assert!(PREINSTALLED_EXTENSIONS.contains(&"xdebug"));
    }

    #[test]
    fn builtin_membership() {
        // Composer projects commonly list `ext-pcre` / `ext-spl` as
        // platform-validation entries — those must short-circuit the
        // index lookup in sync's auto-install path.
        assert!(is_builtin("pcre"));
        assert!(is_builtin("spl"));
        assert!(is_builtin("json"));
        assert!(is_builtin("standard"));
        // Post-Phase-A static additions.
        assert!(is_builtin("openssl"));
        assert!(is_builtin("sodium"));
        assert!(is_builtin("session"));
        assert!(is_builtin("filter"));
        assert!(is_builtin("pcntl"));
        // Things that are loaded but via per-ext.
        assert!(!is_builtin("mbstring"));
        assert!(!is_builtin("opcache")); // 8.1–8.4 per-ext; on 8.5 static, but never user-visible as `ext-opcache`
    }

    #[test]
    fn filter_all_accepts_everything_in_list() {
        let f = BaselineFilter::All;
        for name in BASELINE_EXTENSIONS {
            assert!(f.includes(name));
        }
    }

    #[test]
    fn filter_none_rejects_everything() {
        let f = BaselineFilter::None;
        for name in BASELINE_EXTENSIONS {
            assert!(!f.includes(name));
        }
    }

    #[test]
    fn parse_without_excludes_named() {
        let f = parse_without(&["opcache".into(), "readline".into()]).unwrap();
        assert!(f.includes("calendar"));
        assert!(!f.includes("opcache"));
        assert!(!f.includes("readline"));
    }

    #[test]
    fn parse_without_rejects_non_baseline() {
        let err = parse_without(&["redis".into()]).unwrap_err();
        assert!(err.contains("redis"), "got: {err}");
    }

    #[test]
    fn parse_without_empty_yields_all() {
        match parse_without(&[]).unwrap() {
            BaselineFilter::All => {}
            other => panic!("expected All, got {other:?}"),
        }
    }

    #[test]
    fn gettext_skipped_off_linux() {
        if cfg!(target_os = "linux") {
            assert!(!skip_for_platform("gettext"));
        } else {
            assert!(skip_for_platform("gettext"));
        }
        // Non-gettext names always pass.
        assert!(!skip_for_platform("ffi"));
        assert!(!skip_for_platform("opcache"));
    }

    fn pv(major: u32, minor: u32) -> PartialVersion {
        PartialVersion {
            major,
            minor: Some(minor),
            patch: None,
        }
    }

    #[test]
    fn opcache_skipped_on_php_8_5_plus() {
        // Upstream moved opcache to static-into-bin/php on 8.5.
        assert!(skip_for_php_minor("opcache", pv(8, 5)));
        assert!(skip_for_php_minor("opcache", pv(8, 6)));
        assert!(skip_for_php_minor("opcache", pv(9, 0)));
        // Still a .so on 8.1–8.4 — must NOT be skipped.
        assert!(!skip_for_php_minor("opcache", pv(8, 1)));
        assert!(!skip_for_php_minor("opcache", pv(8, 2)));
        assert!(!skip_for_php_minor("opcache", pv(8, 3)));
        assert!(!skip_for_php_minor("opcache", pv(8, 4)));
        // Other baseline names are never skipped per-minor.
        assert!(!skip_for_php_minor("readline", pv(8, 5)));
        assert!(!skip_for_php_minor("gettext", pv(8, 5)));
        assert!(!skip_for_php_minor("mysqlnd", pv(8, 5)));
    }
}
