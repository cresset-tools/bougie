//! Port of `Composer\Semver\VersionParser::normalize` from
//! composer/semver 3.4.4 (shipped inside composer-2.8.12.phar).
//!
//! `installed.json`'s `version_normalized` and `installed.php`'s
//! `version` field both come from this function. Ground-truth outputs
//! for every documented input shape are captured in
//! `tests/data/version_normalize.tsv` and replayed by
//! `tests/version_normalize.rs`.
//!
//! The PHP algorithm runs a fixed set of string transforms followed by
//! one of two anchored regexes (classical or date-based) plus a
//! dev-branch fallback. The translation below mirrors the exact step
//! order so divergences between the two parsers reduce to "we got a
//! capture group wrong" rather than "we restructured the logic". PHP's
//! possessive quantifiers (`++`, `*+`) become plain greedy ones — the
//! difference is performance-only for these patterns, not semantic.

use std::sync::OnceLock;

use regex::Regex;

/// Composer rejects a malformed version string with
/// `UnexpectedValueException`. We don't reproduce its surrounding
/// `as`-aliasing diagnostic text — only the input that failed.
#[derive(Debug, Clone)]
pub(crate) struct NormalizeError {
    pub input: String,
}

impl std::fmt::Display for NormalizeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Invalid version string \"{}\"", self.input)
    }
}

impl std::error::Error for NormalizeError {}

/// Normalize `s` to the canonical form Composer uses for version
/// comparisons and the `version_normalized` lockfile/installed.json
/// field.
pub(crate) fn normalize(s: &str) -> Result<String, NormalizeError> {
    let orig = s.to_string();
    let v = s.trim();

    // 1. Strip `X as Y` aliasing — keep the source (left of `as`).
    let v = if let Some(c) = as_alias_re().captures(v) {
        c.get(1).expect("group 1").as_str()
    } else {
        v
    };

    // 2. Strip a trailing `@<stability>` flag (case-insensitive).
    let v_owned;
    let v = if let Some(m) = stability_flag_re().find(v) {
        v_owned = v[..m.start()].to_string();
        v_owned.as_str()
    } else {
        v
    };

    // 3. `master` / `trunk` / `default` → `dev-{name}`. Composer
    //    keeps this BC quirk because these used to be valid 1.x
    //    constraints.
    let branch_owned;
    let v = if matches!(v, "master" | "trunk" | "default") {
        branch_owned = format!("dev-{v}");
        branch_owned.as_str()
    } else {
        v
    };

    // 4. `dev-<anything>` short-circuits. The match is
    //    case-insensitive (mirrors PHP's `stripos`) but the emitted
    //    prefix is forced lowercase; the remainder keeps its case.
    if v.len() >= 4 && v.as_bytes()[..4].eq_ignore_ascii_case(b"dev-") {
        return Ok(format!("dev-{}", &v[4..]));
    }

    // 5. Strip `+build` metadata. Composer's regex isn't
    //    modifier-aware — applies even to e.g. `1.0.0-RC1+build`.
    let trimmed_owned;
    let v = if let Some(c) = build_meta_re().captures(v) {
        trimmed_owned = c.get(1).expect("group 1").as_str().to_string();
        trimmed_owned.as_str()
    } else {
        v
    };

    // 6. Try classical, then date-based. Each yields a numeric prefix
    //    plus three modifier captures: stability name, stability
    //    numeric tail, and an optional trailing `-dev` flag.
    if let Some(c) = classical_re().captures(v) {
        let parts = format!(
            "{}{}{}{}",
            c.get(1).expect("major").as_str(),
            c.get(2).map(|m| m.as_str()).filter(|s| !s.is_empty()).unwrap_or(".0"),
            c.get(3).map(|m| m.as_str()).filter(|s| !s.is_empty()).unwrap_or(".0"),
            c.get(4).map(|m| m.as_str()).filter(|s| !s.is_empty()).unwrap_or(".0"),
        );
        return Ok(apply_modifier(parts, &c, 5));
    }

    if let Some(c) = date_re().captures(v) {
        let raw = c.get(1).expect("date group").as_str();
        let dotted: String = raw
            .chars()
            .map(|ch| if ch.is_ascii_digit() { ch } else { '.' })
            .collect();
        return Ok(apply_modifier(dotted, &c, 2));
    }

    // 7. Branch fallback: anything ending in `dev` runs through
    //    normalizeBranch and is returned if the result isn't a
    //    `dev-{name}` passthrough (i.e. it did match the
    //    numeric+x-placeholder branch regex).
    if let Some(c) = dev_trail_re().captures(v) {
        let prefix = c.get(1).expect("group 1").as_str();
        let normalized = normalize_branch(prefix);
        if !normalized.starts_with("dev-") {
            return Ok(normalized);
        }
    }

    Err(NormalizeError { input: orig })
}

/// Apply the trailing modifier captures from either the classical or
/// date regex. `base` is the already-formatted numeric prefix; `index`
/// is the offset of the first modifier capture (5 for classical, 2 for
/// date). The three relevant captures at `index`, `index+1`, `index+2`
/// are: stability name, stability numeric tail, `[.-]?dev` flag.
fn apply_modifier(base: String, c: &regex::Captures<'_>, index: usize) -> String {
    let mut version = base;

    let stability = c.get(index).map(|m| m.as_str()).filter(|s| !s.is_empty());
    let stability_num = c
        .get(index + 1)
        .map(|m| m.as_str())
        .filter(|s| !s.is_empty());
    let dev_suffix = c
        .get(index + 2)
        .map(|m| m.as_str())
        .filter(|s| !s.is_empty());

    if let Some(stab) = stability {
        let lower = stab.to_ascii_lowercase();
        if lower == "stable" {
            return version;
        }
        let expanded = expand_stability(&lower);
        let tail = stability_num
            .map(|s| s.trim_start_matches(['.', '-']))
            .unwrap_or("");
        version.push('-');
        version.push_str(expanded);
        version.push_str(tail);
    }

    if dev_suffix.is_some() {
        version.push_str("-dev");
    }

    version
}

/// Port of `VersionParser::expandStability`. Caller must already have
/// lowercased the input.
fn expand_stability(s: &str) -> &str {
    match s {
        "a" => "alpha",
        "b" => "beta",
        "p" | "pl" => "patch",
        "rc" => "RC",
        other => other,
    }
}

/// Port of `VersionParser::normalizeBranch`. Returns either the
/// `1.9999999.9999999.9999999-dev` style synthetic branch version
/// (when the input is a numeric/x branch) or a `dev-{name}` passthrough.
/// The caller distinguishes by checking `starts_with("dev-")`.
fn normalize_branch(name: &str) -> String {
    let name = name.trim();
    if let Some(c) = branch_re().captures(name) {
        let mut version = String::with_capacity(32);
        for i in 1..5 {
            match c.get(i) {
                Some(m) => {
                    let part = m.as_str();
                    // Group 1 is the bare leading number; groups 2–4
                    // start with `.`. Both have `*`/`X` mapped to `x`.
                    for ch in part.chars() {
                        match ch {
                            '*' | 'X' => version.push('x'),
                            _ => version.push(ch),
                        }
                    }
                }
                None => version.push_str(".x"),
            }
        }
        return format!("{}-dev", version.replace('x', "9999999"));
    }
    format!("dev-{name}")
}

// --- Regex cache ---------------------------------------------------------
//
// PHP's possessive quantifiers (`++`, `*+`) are dropped: greedy is
// behaviorally equivalent for these patterns and the regex crate
// doesn't support possessive syntax. The `i` flag (when present)
// becomes an inline `(?i)` prefix. Unicode semantics for `\d` / `\s`
// match PHP here because the inputs are 7-bit ASCII version strings
// (composer/semver enforces that upstream).

fn as_alias_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"^([^,\s]+) +as +([^,\s]+)$").expect("as-alias regex"))
}

fn stability_flag_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"(?i)@(?:stable|RC|beta|alpha|dev)$").expect("stab flag regex"))
}

fn build_meta_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"^([^,\s+]+)\+[^\s]+$").expect("build-meta regex"))
}

fn classical_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| {
        Regex::new(
            r"(?i)^v?(\d{1,5})(\.\d+)?(\.\d+)?(\.\d+)?[._-]?(?:(stable|beta|b|RC|alpha|a|patch|pl|p)((?:[.-]?\d+)*)?)?([.-]?dev)?$",
        )
        .expect("classical version regex")
    })
}

fn date_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| {
        Regex::new(
            r"(?i)^v?(\d{4}(?:[.:-]?\d{2}){1,6}(?:[.:-]?\d{1,3}){0,2})[._-]?(?:(stable|beta|b|RC|alpha|a|patch|pl|p)((?:[.-]?\d+)*)?)?([.-]?dev)?$",
        )
        .expect("date version regex")
    })
}

fn dev_trail_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"(?i)^(.*?)[.-]?dev$").expect("dev-trail regex"))
}

fn branch_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| {
        Regex::new(r"(?i)^v?(\d+)(\.(?:\d+|[xX*]))?(\.(?:\d+|[xX*]))?(\.(?:\d+|[xX*]))?$")
            .expect("branch regex")
    })
}
