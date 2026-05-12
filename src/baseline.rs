//! The baseline extension set per CLI.md §3.5.1.1.
//!
//! These extensions are installed and enabled on every interpreter
//! without the user having to ask. The list is compiled into the
//! bougie binary — a bougie release changes the baseline, not an
//! index publication — which keeps `bougie php install` deterministic
//! per bougie version even if the index later advertises new
//! extensions.

use std::collections::BTreeSet;

/// Ordered list of baseline extension names. Order is observable
/// (it's the order conf.d fragments and the JSON `baseline` array
/// appear in) but PHP's alphabetic conf.d scan re-orders fragments
/// at load time, so this is purely cosmetic.
pub const BASELINE_EXTENSIONS: &[&str] = &[
    "mbstring",
    "curl",
    "intl",
    "zip",
    "bcmath",
    "sqlite3",
    "pdo_sqlite",
    "pdo_mysql",
    "mysqli",
];

/// Extensions that are *statically compiled into the PHP binary* and
/// can never be loaded as `.so` files. `composer.json` projects
/// frequently declare `ext-pcre` / `ext-json` / `ext-spl` for
/// platform-validation reasons; bougie must treat those as
/// already-satisfied rather than attempting an index lookup that
/// will fail.
///
/// Derived from PHP's default `get_loaded_extensions()` set on a
/// stock build plus the configure-time-static deps our
/// `php-build-standalone` pipeline links in (`libxml`). Keep
/// lowercase — `composer.json` keys are case-sensitive and the
/// idiomatic spelling is lowercase.
pub const BUILTIN_EXTENSIONS: &[&str] = &[
    "core",
    "date",
    "hash",
    "json",
    "libxml",
    "pcre",
    "random",
    "reflection",
    "spl",
    "standard",
];

/// Per-invocation narrowing applied by `bougie php install`'s
/// `--no-baseline` / `--baseline-only` flags. Project-level opt-out
/// (the `false` sentinel in `[extensions]`) is a separate concern
/// handled at sync time.
#[derive(Debug, Clone)]
pub enum BaselineFilter {
    /// Install the full baseline set. Default.
    All,
    /// Install nothing from the baseline. `--no-baseline`.
    None,
    /// Install only the named subset. `--baseline-only=a,b,c`.
    Only(BTreeSet<String>),
}

impl BaselineFilter {
    /// Whether the given extension name passes the filter.
    pub fn includes(&self, name: &str) -> bool {
        match self {
            Self::All => true,
            Self::None => false,
            Self::Only(set) => set.contains(name),
        }
    }
}

/// `true` if `name` is in [`BASELINE_EXTENSIONS`]. Used by `sync` to
/// decide whether a `mysqli = false` opt-out actually applies (only
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

/// Parse `--baseline-only=a,b,c` into a [`BaselineFilter::Only`].
/// Empty list maps to [`BaselineFilter::None`] for symmetry with
/// `--no-baseline`. Names not in [`BASELINE_EXTENSIONS`] are
/// rejected — the flag is a narrowing filter, not a way to install
/// arbitrary extensions through `php install`.
pub fn parse_baseline_only(spec: &str) -> Result<BaselineFilter, String> {
    let set: BTreeSet<String> = spec
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from)
        .collect();
    for name in &set {
        if !is_baseline(name) {
            return Err(format!(
                "`--baseline-only={spec}` lists `{name}`, which isn't in the baseline set; \
                 use `bougie ext add {name}` instead",
            ));
        }
    }
    if set.is_empty() {
        Ok(BaselineFilter::None)
    } else {
        Ok(BaselineFilter::Only(set))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn baseline_membership() {
        assert!(is_baseline("mbstring"));
        assert!(is_baseline("pdo_mysql"));
        assert!(!is_baseline("xdebug"));
        assert!(!is_baseline("redis"));
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
        assert!(!is_builtin("redis"));
        assert!(!is_builtin("mbstring")); // shared in our build, baseline territory
        assert!(!is_builtin("openssl")); // shared, core territory
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
    fn parse_baseline_only_accepts_subset() {
        let f = parse_baseline_only("mbstring, curl").unwrap();
        assert!(f.includes("mbstring"));
        assert!(f.includes("curl"));
        assert!(!f.includes("intl"));
    }

    #[test]
    fn parse_baseline_only_rejects_non_baseline() {
        let err = parse_baseline_only("mbstring,redis").unwrap_err();
        assert!(err.contains("redis"), "got: {err}");
    }

    #[test]
    fn parse_baseline_only_empty_yields_none() {
        match parse_baseline_only("").unwrap() {
            BaselineFilter::None => {}
            other => panic!("expected None, got {other:?}"),
        }
    }
}
