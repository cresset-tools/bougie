//! Composer-flavored version: parse, normalize, compare.
//!
//! Closely mirrors `Composer\Semver\VersionParser::normalize` and
//! `Comparator::compare` from `composer/semver` (commit
//! `09af5e85b5f1380e4e098dde28950e2549cba4ed`, the version the
//! Layer 1 conformance fixtures in `tests/data/conformance.json`
//! were generated from). The PHP algorithm is two anchored regexes
//! (classical + date) plus a modifier tail; we keep the same shape
//! to stay auditable against the source.
//!
//! Two flavors share one type:
//!
//! - **Numeric** versions: the body is 1-N digit segments. Segments
//!   are stored as strings so the canonical normalized output
//!   preserves leading zeros (`00.01.03.04` → `00.01.03.04`).
//!   Comparison uses the numeric value of each segment.
//! - **Branch** versions: `dev-feature-foo`. The body isn't numeric
//!   and they live in their own ordering space — bare branches sort
//!   below all numerics; the special "default" branch names
//!   (`master`, `main`, `default`, `trunk`) are normalized to
//!   numeric `9999999.9999999.9999999.9999999-dev` so they sort at
//!   the top.

use crate::stability::Stability;
use regex::Regex;
use std::cmp::Ordering;
use std::sync::OnceLock;

#[derive(Debug, Clone, Eq, PartialEq, Hash)]
pub struct Version {
    pub kind: VersionKind,
    /// Canonical normalized form. Always equal to what Composer's
    /// `VersionParser::normalize` returns for the same input.
    pub normalized: String,
}

#[derive(Debug, Clone, Eq, PartialEq, Hash)]
pub enum VersionKind {
    /// Numeric version: 1+ decimal segments + a stability suffix.
    Numeric {
        /// Segments preserved verbatim for Display fidelity (leading
        /// zeros, etc.).
        segments_raw: Vec<String>,
        suffix: Suffix,
    },
    /// Branch-only version, name verbatim (case-preserved except
    /// for the special "default" branches that always lower-case).
    Branch(String),
}

/// Stability suffix on a numeric version. Composer's enum has more
/// variants than [`Stability`] (which is the top-level keyword for
/// `minimum-stability`) — `patch` is not its own `Stability` but
/// appears in normalized strings as `-patchN`. `Dev` here means a
/// bare `-dev`; `PrereleaseDev` is the `-RC1-dev` hybrid.
#[derive(Debug, Clone, Eq, PartialEq, Hash)]
pub enum Suffix {
    /// No suffix.
    Stable,
    /// `-patchN`. Still stable in Composer's stability model.
    Patch(u16),
    /// `-RC<num>`, `-beta<num>`, `-alpha<num>`. `num` is the
    /// canonical rendered number tail (`""` for bare `-beta`, `"0"`
    /// for `-beta0`/`-beta.0`, `"3.1"` for `-alpha.3.1`, `"2.1-3"`
    /// for `-alpha-2.1-3`). The string is appended verbatim after
    /// the stability keyword.
    Prerelease { stability: Stability, num: String },
    /// Bare `-dev`.
    Dev,
    /// Prerelease followed by `-dev` (`-RC1-dev`, etc.).
    PrereleaseDev { stability: Stability, num: String },
    /// `-patch<num>-dev` hybrid (`1.0.0.pl3-dev` → `1.0.0.0-patch3-dev`).
    PatchDev(u16),
}

impl Suffix {
    pub fn stability(&self) -> Stability {
        match self {
            Self::Stable | Self::Patch(_) => Stability::Stable,
            Self::Prerelease { stability, .. } => *stability,
            Self::Dev | Self::PrereleaseDev { .. } | Self::PatchDev(_) => Stability::Dev,
        }
    }
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub enum ParseError {
    Invalid(String),
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Invalid(s) => write!(f, "invalid version: {s:?}"),
        }
    }
}

impl std::error::Error for ParseError {}

// ---- regex bank --------------------------------------------------------
//
// One-time compiled regexes mirroring Composer's source. The modifier
// regex is composed inline into the classical/date patterns so the
// full input is anchored end-to-end.

fn re_modifier_str() -> &'static str {
    // From Composer: optional stability keyword + optional number(s)
    // + optional `-dev` tail. The `+?` on the keyword group makes
    // the number-only sub-group non-greedy so `-1` after the keyword
    // doesn't get eaten by the number group when no keyword appears.
    //
    // We accept the keyword aliases Composer does:
    //   stable, RC, beta/b, alpha/a, patch/pl/p, dev
    //
    // The capture groups are:
    //   2 = keyword (lower-cased by us at use site)
    //   3 = numeric tail attached to the keyword
    //   4 = trailing `-dev` (when keyword is set)
    // The leading "([._-]?)?" separator is gobbled silently.
    // Keywords intentionally exclude `dev` — `dev` is only matched
    // by the trailing `([.-]?dev)?` group, never as a standalone
    // keyword. (Adding `dev` here would steal `1-2_dev` into group 1.)
    r"(?:[._-]?(?:(stable|RC|beta|b|alpha|a|patch|pl|p)((?:[.-]?\d+)*)?)?)?([.-]?dev)?"
}

fn re_classical() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| {
        let pat = format!(
            r"^(?i)v?(\d{{1,5}})(\.\d+)?(\.\d+)?(\.\d+)?{}$",
            re_modifier_str()
        );
        Regex::new(&pat).unwrap()
    })
}

fn re_date() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| {
        // Year + 1-6 two-digit segments + 0..N trailing 1-3-digit
        // segments. The trailing `*` (not `?`) is what lets
        // `20230131.0.0` and `2010-01-02-10-20-30.0.3` match — the
        // tail can grow indefinitely with short numeric chunks.
        // {0,2} (not `*`) bounds the trailing tail. Matches the
        // upstream regex verbatim — see composer/semver
        // commit 09af5e8 src/VersionParser.php.
        let pat = format!(
            r"^(?i)v?(\d{{4}}(?:[.:\-_]?\d{{2}}){{1,6}}(?:[.:\-_]?\d{{1,3}}){{0,2}}){}$",
            re_modifier_str()
        );
        Regex::new(&pat).unwrap()
    })
}

fn re_branch_alias() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| {
        Regex::new(r"^v?(\d+)(\.(?:\d+|[xX*]))*-dev$").unwrap()
    })
}

/// True iff `s` matches the Composer branch-alias form
/// `Nx-dev` / `N.x-dev` / `N.M.x-dev` (etc.). These appear both as
/// versions (Packagist's dev document lists them) and as constraints
/// in `composer.json` (`"phpmd/phpmd": "3.x-dev"`). The constraint
/// parser routes the form to an `==` against the same string
/// re-parsed as a `Version`.
pub fn is_branch_alias(s: &str) -> bool {
    re_branch_alias().is_match(s.trim())
}

/// Strip a trailing `@stable`/`@dev`/etc. stability flag. Composer
/// accepts these on package require strings (e.g.
/// `name@dev`); `normalize` drops them before further parsing.
fn strip_at_stability(s: &str) -> String {
    static R: OnceLock<Regex> = OnceLock::new();
    let r = R.get_or_init(|| {
        Regex::new(r"@(?i)(stable|RC|beta|alpha|dev)$").unwrap()
    });
    r.replace(s, "").into_owned()
}

fn re_dev_prefix() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"^(?i)dev-(.+)$").unwrap())
}

// ---- parse / normalize --------------------------------------------------

impl Version {
    /// Parse a Composer version string into its normalized form.
    /// Equivalent to PHP's `VersionParser::normalize($s)`.
    pub fn parse(s: &str) -> Result<Self, ParseError> {
        let raw = s.trim();
        if raw.is_empty() {
            return Err(ParseError::Invalid(s.to_owned()));
        }

        // Strip `#<commit-ref>` suffix (any version may carry one).
        let cleaned = match raw.split_once('#') {
            Some((before, _)) => before,
            None => raw,
        };
        let cleaned = cleaned.trim();

        // `1.0 as 2.0` — keep the left side and re-normalize.
        if let Some((left, _right)) = split_aliased(cleaned) {
            return Self::parse(left);
        }

        // `name@stability` suffix — Composer accepts this on the
        // composer.json side as an inline stability flag, but
        // normalize strips it before further processing.
        let cleaned = strip_at_stability(cleaned);

        // Bare default-branch names (`master`, `trunk`, `default`)
        // get a `dev-` prefix and the dev-prefix code path takes
        // over. The wider set (`main`, `latest`, `head`) is NOT
        // promoted by Composer's `normalize`; only the three legacy
        // names. Their max-version-dev treatment lives in
        // `normalize_default_branch` for use by sort, not parse.
        let cleaned = match cleaned.as_str() {
            "master" | "trunk" | "default" => format!("dev-{cleaned}"),
            _ => cleaned,
        };

        // Branch alias forms `1.x-dev` / `1.2.x-dev` → numeric
        // 9999999-padded.
        if let Some(v) = parse_branch_alias(&cleaned) {
            return Ok(v);
        }

        // Strip `+build-metadata` for numeric forms — but only when
        // the tail is non-whitespace (`1.0.0+foo bar` has a space
        // inside the metadata, which Composer rejects), and only
        // when the leading body isn't a `dev-` branch (those keep
        // their `+` verbatim — `dev-feature+issue-1`).
        let numeric_candidate = if cleaned.to_ascii_lowercase().starts_with("dev-") {
            cleaned.clone()
        } else if let Some(idx) = cleaned.find('+') {
            let tail = &cleaned[idx + 1..];
            if tail.is_empty() || tail.chars().any(char::is_whitespace) {
                cleaned.clone()
            } else {
                cleaned[..idx].to_owned()
            }
        } else {
            cleaned.clone()
        };

        // Classical numeric: 1-4 dot-separated segments with optional
        // modifier tail.
        if let Some(caps) = re_classical().captures(&numeric_candidate) {
            let segs = collect_classical_segments(&caps);
            let suffix = parse_modifier(
                caps.get(5).map_or("", |m| m.as_str()),
                caps.get(6).map_or("", |m| m.as_str()),
                caps.get(7).map_or("", |m| m.as_str()),
            )?;
            let normalized = render_numeric(&segs, &suffix);
            return Ok(Version {
                kind: VersionKind::Numeric { segments_raw: segs, suffix },
                normalized,
            });
        }

        // Date-shaped: `YYYYMMDD`, `YYYY.MM.DD`, etc.
        if let Some(caps) = re_date().captures(&numeric_candidate) {
            let body = caps.get(1).unwrap().as_str();
            let segs = split_date_segments(body);
            let suffix = parse_modifier(
                caps.get(2).map_or("", |m| m.as_str()),
                caps.get(3).map_or("", |m| m.as_str()),
                caps.get(4).map_or("", |m| m.as_str()),
            )?;
            let normalized = render_numeric(&segs, &suffix);
            return Ok(Version {
                kind: VersionKind::Numeric { segments_raw: segs, suffix },
                normalized,
            });
        }

        // Explicit `dev-<branchname>` (anything not caught above).
        // Use the original `cleaned` here so the `+metadata` we
        // stripped from the numeric candidate doesn't leak through.
        if let Some(caps) = re_dev_prefix().captures(&cleaned) {
            let body = caps.get(1).unwrap().as_str();
            return Ok(Version {
                kind: VersionKind::Branch(body.to_owned()),
                normalized: format!("dev-{body}"),
            });
        }

        Err(ParseError::Invalid(s.to_owned()))
    }

    /// Composer's `parseNumericAliasPrefix($branch)`. Given a branch
    /// name like `1.x-dev` returns `Some("1.")`; for non-aliases
    /// returns `None`.
    pub fn parse_numeric_alias_prefix(branch: &str) -> Option<String> {
        // Three accepted shapes per Composer:
        //   1.x-dev  → 1.
        //   1.2-dev  → 1.2.
        //   1.2.x-dev → 1.2.
        //   1-dev    → 1.
        //   dev-master → None
        let stripped = strip_v_prefix(branch);
        let dev_body = stripped.strip_suffix("-dev")?;
        // Reject anything with a non-numeric/non-wildcard segment.
        let parts: Vec<&str> = dev_body.split('.').collect();
        let mut prefix_parts: Vec<&str> = Vec::with_capacity(parts.len());
        let mut saw_wildcard = false;
        for p in &parts {
            if matches!(*p, "x" | "X" | "*") {
                saw_wildcard = true;
                break;
            }
            if p.chars().all(|c| c.is_ascii_digit()) && !p.is_empty() {
                prefix_parts.push(p);
            } else {
                return None;
            }
        }
        // `1.2.x-dev`: trailing wildcard → keep numeric prefix and a
        //   trailing dot.
        // `1.2-dev` / `1-dev`: no wildcard → still acceptable, full
        //   numeric becomes the prefix.
        let mut out = prefix_parts.join(".");
        if !out.is_empty() {
            out.push('.');
        } else if !saw_wildcard {
            // `-dev` alone with no numeric body is not a valid alias.
            return None;
        }
        Some(out)
    }

    /// Composer's `normalizeBranch($branch)` — distinct from
    /// `normalize` in that the input is a branch name (no `dev-`
    /// prefix). Numeric-wildcard branches normalize to the same
    /// 9999999-padded form `parse` would produce; everything else
    /// becomes `dev-<name>`.
    pub fn normalize_branch(branch: &str) -> String {
        let stripped = strip_v_prefix(branch);
        static R: OnceLock<Regex> = OnceLock::new();
        let r = R.get_or_init(|| {
            Regex::new(r"^(\d+)(\.(?:\d+|[xX*]))*$").unwrap()
        });
        if r.is_match(stripped) {
            let mut segs_raw: Vec<String> = Vec::new();
            for part in stripped.split('.') {
                if matches!(part, "x" | "X" | "*") {
                    segs_raw.push("9999999".to_owned());
                } else {
                    segs_raw.push(part.to_owned());
                }
            }
            while segs_raw.len() < 4 {
                segs_raw.push("9999999".to_owned());
            }
            return render_numeric(&segs_raw, &Suffix::Dev);
        }
        format!("dev-{stripped}")
    }

    /// Composer's `parseStability($version)`. Mirrors the upstream
    /// algorithm directly: strip `#commit`, fast-path `dev-` prefix
    /// and `-dev` suffix, then run the modifier regex against the
    /// lower-cased input and read the keyword + dev capture from
    /// the match.
    pub fn parse_stability(version: &str) -> Stability {
        let raw = version.trim();
        let cleaned = match raw.split_once('#') {
            Some((before, _)) => before,
            None => raw,
        };
        let cleaned = cleaned.trim();
        if cleaned.to_ascii_lowercase().starts_with("dev-")
            || cleaned.to_ascii_lowercase().ends_with("-dev")
        {
            return Stability::Dev;
        }
        // Anchored-at-end modifier match (lowercased) — same shape
        // as `VersionParser::parseStability`.
        static R: OnceLock<Regex> = OnceLock::new();
        let r = R.get_or_init(|| {
            let pat = format!(r"(?i){}(?:\+.*)?$", re_modifier_str());
            Regex::new(&pat).unwrap()
        });
        let lower = cleaned.to_ascii_lowercase();
        if let Some(caps) = r.captures(&lower) {
            // Group 3 is the trailing `-dev` capture.
            if caps.get(3).is_some_and(|m| !m.as_str().is_empty()) {
                return Stability::Dev;
            }
            // Group 1 is the stability keyword.
            if let Some(kw) = caps.get(1) {
                let s = kw.as_str();
                if !s.is_empty() {
                    return match s {
                        "beta" | "b" => Stability::Beta,
                        "alpha" | "a" => Stability::Alpha,
                        "rc" => Stability::Rc,
                        _ => Stability::Stable,
                    };
                }
            }
        }
        Stability::Stable
    }
}

impl std::fmt::Display for Version {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.normalized)
    }
}

/// Composer comparison operator used by [`Version::compare`]. The
/// Comparator semantics differ from a total Ord (which we also
/// provide for pubgrub's sake) — for two *different* bare dev
/// branches, *every* operator returns `false` because Composer
/// considers them incomparable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CmpOp {
    Gt,
    Ge,
    Lt,
    Le,
    Eq,
    Ne,
}

impl Version {
    /// Composer's `Package::getStability()` applied to a parsed
    /// `Version`. Branch versions are always `Dev`; numeric versions
    /// inherit their `Suffix`'s stability (`Stable` for bare,
    /// `Patch(_)`; `Dev` for bare `-dev`; the prerelease keyword
    /// otherwise). This is the granular stability used to filter
    /// candidates against `minimum-stability` and per-package
    /// stability flags.
    pub fn stability(&self) -> Stability {
        match &self.kind {
            VersionKind::Numeric { suffix, .. } => suffix.stability(),
            VersionKind::Branch(_) => Stability::Dev,
        }
    }

    /// Composer's comparator. Returns `false` for every op when the
    /// two operands are bare dev branches with different names; `==`
    /// is `true` only when both are bare branches with the same
    /// name. Numeric-vs-Branch comparisons use the standard Ord
    /// (Branch < Numeric).
    pub fn compare(&self, op: CmpOp, other: &Version) -> bool {
        if let (VersionKind::Branch(a), VersionKind::Branch(b)) = (&self.kind, &other.kind) {
            return match (op, a == b) {
                (CmpOp::Eq, same) => same,
                (CmpOp::Ne, same) => !same,
                (CmpOp::Ge | CmpOp::Le, true) => true,
                _ => false,
            };
        }
        let ord = self.cmp(other);
        match op {
            CmpOp::Gt => ord.is_gt(),
            CmpOp::Ge => ord.is_ge(),
            CmpOp::Lt => ord.is_lt(),
            CmpOp::Le => ord.is_le(),
            CmpOp::Eq => ord.is_eq(),
            CmpOp::Ne => !ord.is_eq(),
        }
    }

    /// Composer's `normalizeDefaultBranch`. Called by sort (NOT by
    /// `normalize`/`parse`) to massage `dev-master`, `dev-default`,
    /// `dev-trunk` into `9999999-dev` so they sort above numeric
    /// versions. Other dev branches pass through unchanged.
    pub fn normalize_default_branch(normalized: &str) -> String {
        match normalized {
            "dev-master" | "dev-default" | "dev-trunk" => "9999999-dev".to_owned(),
            other => other.to_owned(),
        }
    }

    /// Construct a Version with the same numeric body as `self` but
    /// the given suffix. Returns `None` for [`VersionKind::Branch`]
    /// (a branch ref has no numeric body to attach a suffix to).
    ///
    /// Used by the Composer-to-pubgrub `Ranges<Version>` conversion
    /// to synthesize boundary markers: `<X` becomes
    /// `strictly_lower_than(X.with_suffix(Suffix::Dev))` so that
    /// every prerelease of `X` (which sorts ≥ `X-dev`) is excluded.
    pub fn with_suffix(&self, suffix: Suffix) -> Option<Version> {
        match &self.kind {
            VersionKind::Numeric { segments_raw, .. } => {
                let normalized = render_numeric(segments_raw, &suffix);
                Some(Version {
                    kind: VersionKind::Numeric {
                        segments_raw: segments_raw.clone(),
                        suffix,
                    },
                    normalized,
                })
            }
            VersionKind::Branch(_) => None,
        }
    }
}

// ---- ordering -----------------------------------------------------------

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Version {
    fn cmp(&self, other: &Self) -> Ordering {
        match (&self.kind, &other.kind) {
            (
                VersionKind::Numeric { segments_raw: a, suffix: sa },
                VersionKind::Numeric { segments_raw: b, suffix: sb },
            ) => cmp_segments(a, b).then_with(|| suffix_cmp(sa, sb)),
            (VersionKind::Branch(_), VersionKind::Numeric { .. }) => Ordering::Less,
            (VersionKind::Numeric { .. }, VersionKind::Branch(_)) => Ordering::Greater,
            // Two bare-branch versions compare as Equal — Composer
            // doesn't impose a lex order on dev branches (per the
            // `sortProvider` fixture: `dev-foo > dev-bar` is false,
            // and stable sorting preserves input order for the
            // equal-class).
            (VersionKind::Branch(_), VersionKind::Branch(_)) => Ordering::Equal,
        }
    }
}

fn cmp_segments(a: &[String], b: &[String]) -> Ordering {
    let len = a.len().max(b.len());
    for i in 0..len {
        let va: u64 = a.get(i).and_then(|s| s.parse().ok()).unwrap_or(0);
        let vb: u64 = b.get(i).and_then(|s| s.parse().ok()).unwrap_or(0);
        match va.cmp(&vb) {
            Ordering::Equal => {}
            non_eq => return non_eq,
        }
    }
    Ordering::Equal
}

fn suffix_cmp(a: &Suffix, b: &Suffix) -> Ordering {
    suffix_rank(a).cmp(&suffix_rank(b))
}

/// (tier, n). Higher tier = newer. `n` is the leading-int from the
/// stability tail (e.g. `"2.1-3"` → 2).
fn suffix_rank(s: &Suffix) -> (u8, u16) {
    let leading_int = |num: &str| -> u16 {
        let mut s = String::new();
        for ch in num.chars() {
            if ch.is_ascii_digit() {
                s.push(ch);
            } else {
                break;
            }
        }
        s.parse().unwrap_or(0)
    };
    match s {
        Suffix::Dev => (0, 0),
        Suffix::PrereleaseDev { stability: Stability::Alpha, num } => (1, leading_int(num)),
        Suffix::Prerelease { stability: Stability::Alpha, num } => (2, leading_int(num)),
        Suffix::PrereleaseDev { stability: Stability::Beta, num } => (3, leading_int(num)),
        Suffix::Prerelease { stability: Stability::Beta, num } => (4, leading_int(num)),
        Suffix::PrereleaseDev { stability: Stability::Rc, num } => (5, leading_int(num)),
        Suffix::Prerelease { stability: Stability::Rc, num } => (6, leading_int(num)),
        Suffix::PatchDev(n) => (0, *n),
        Suffix::Stable => (7, 0),
        Suffix::Patch(n) => (7, *n),
        Suffix::Prerelease { stability: Stability::Dev | Stability::Stable, num } => (0, leading_int(num)),
        Suffix::PrereleaseDev { stability: Stability::Dev | Stability::Stable, num } => (0, leading_int(num)),
    }
}

// ---- helpers ------------------------------------------------------------

fn strip_v_prefix(s: &str) -> &str {
    s.strip_prefix(['v', 'V']).unwrap_or(s)
}

/// Split a `... as ...` alias form into (left, right). Returns `None`
/// when the input has no alias.
fn split_aliased(s: &str) -> Option<(&str, &str)> {
    static R: OnceLock<Regex> = OnceLock::new();
    let r = R.get_or_init(|| {
        Regex::new(r"^([^,\s]+)\s+as\s+([^,\s]+)$").unwrap()
    });
    let caps = r.captures(s)?;
    Some((caps.get(1).unwrap().as_str(), caps.get(2).unwrap().as_str()))
}

/// Collect classical-regex capture segments into a 4-segment Vec<String>,
/// padding with `"0"` for missing positions.
fn collect_classical_segments(caps: &regex::Captures) -> Vec<String> {
    let s1 = caps.get(1).unwrap().as_str().to_owned();
    let s2 = caps
        .get(2).map_or_else(|| "0".to_owned(), |m| m.as_str().trim_start_matches('.').to_owned());
    let s3 = caps
        .get(3).map_or_else(|| "0".to_owned(), |m| m.as_str().trim_start_matches('.').to_owned());
    let s4 = caps
        .get(4).map_or_else(|| "0".to_owned(), |m| m.as_str().trim_start_matches('.').to_owned());
    vec![s1, s2, s3, s4]
}

/// Split a date-body string on any non-digit separator into segments
/// (preserves leading zeros).
fn split_date_segments(body: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut cur = String::new();
    for ch in body.chars() {
        if ch.is_ascii_digit() {
            cur.push(ch);
        } else if !cur.is_empty() {
            out.push(std::mem::take(&mut cur));
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

fn render_numeric(segments: &[String], suffix: &Suffix) -> String {
    let mut out = segments.join(".");
    match suffix {
        Suffix::Stable => {}
        Suffix::Patch(n) => {
            out.push_str("-patch");
            out.push_str(&n.to_string());
        }
        Suffix::Prerelease { stability, num } => {
            out.push('-');
            out.push_str(stability.as_str());
            out.push_str(num);
        }
        Suffix::Dev => out.push_str("-dev"),
        Suffix::PrereleaseDev { stability, num } => {
            out.push('-');
            out.push_str(stability.as_str());
            out.push_str(num);
            out.push_str("-dev");
        }
        Suffix::PatchDev(n) => {
            out.push_str("-patch");
            out.push_str(&n.to_string());
            out.push_str("-dev");
        }
    }
    out
}

/// Combine the three modifier-regex captures into a [`Suffix`].
/// `explicit` flag tracks whether a number was actually written in
/// the source (so `-beta.0` keeps the `0`, while `-beta` drops it).
fn parse_modifier(
    keyword: &str,
    number_tail: &str,
    dev_tail: &str,
) -> Result<Suffix, ParseError> {
    let kw_present = !keyword.is_empty();
    let dev_present = !dev_tail.is_empty();
    if !kw_present && !dev_present {
        return Ok(Suffix::Stable);
    }
    if !kw_present && dev_present {
        return Ok(Suffix::Dev);
    }
    let (stab, is_patch) = match keyword.to_ascii_lowercase().as_str() {
        "stable" => return if dev_present { Ok(Suffix::Dev) } else { Ok(Suffix::Stable) },
        "rc" => (Some(Stability::Rc), false),
        "beta" | "b" => (Some(Stability::Beta), false),
        "alpha" | "a" => (Some(Stability::Alpha), false),
        "patch" | "pl" | "p" => (None, true),
        _ => return Err(ParseError::Invalid(keyword.to_owned())),
    };

    let num = canonical_number_tail(number_tail);

    if is_patch {
        let n: u16 = num.parse().unwrap_or(0);
        return Ok(if dev_present {
            Suffix::PatchDev(n)
        } else {
            Suffix::Patch(n)
        });
    }
    let stab = stab.unwrap();
    Ok(if dev_present {
        Suffix::PrereleaseDev { stability: stab, num }
    } else {
        Suffix::Prerelease { stability: stab, num }
    })
}

/// Convert a raw number tail (`""`, `"5"`, `".0"`, `"-2.1-3"`,
/// `".3.1"`) into the canonical Composer rendering — strip a single
/// leading separator (`.`, `-`, `_`) before the first digit, leave
/// internal separators alone. Empty input returns the empty string.
fn canonical_number_tail(t: &str) -> String {
    let mut out = String::new();
    let mut leading_stripped = false;
    let mut chars = t.chars().peekable();
    while let Some(&ch) = chars.peek() {
        if !leading_stripped && matches!(ch, '.' | '-' | '_') {
            chars.next();
            leading_stripped = true;
            continue;
        }
        if ch.is_ascii_digit() {
            out.push(ch);
            leading_stripped = true; // any leading separator opportunity over
            chars.next();
        } else {
            // After the first digit, allow internal `.` / `-` /
            // digits. Composer canonicalizes `_` separators inside
            // the tail to … well, the fixture doesn't include such
            // cases inside the modifier so we mirror the few that
            // appear by passing them through.
            if !out.is_empty() && matches!(ch, '.' | '-') {
                out.push(ch);
                chars.next();
            } else {
                break;
            }
        }
    }
    out
}

fn parse_branch_alias(s: &str) -> Option<Version> {
    let m = re_branch_alias().captures(s)?;
    let whole = m.get(0).unwrap().as_str();
    let body = strip_v_prefix(whole).strip_suffix("-dev").unwrap();
    let mut segs_raw: Vec<String> = Vec::new();
    let mut had_wildcard = false;
    for part in body.split('.') {
        if matches!(part, "x" | "X" | "*") {
            had_wildcard = true;
            segs_raw.push("9999999".to_owned());
        } else if part.chars().all(|c| c.is_ascii_digit()) && !part.is_empty() {
            segs_raw.push(part.to_owned());
        } else {
            return None;
        }
    }
    if !had_wildcard {
        // No wildcard → not a branch alias; let the numeric parser
        // handle `1.0-dev` etc.
        return None;
    }
    while segs_raw.len() < 4 {
        segs_raw.push("9999999".to_owned());
    }
    let normalized = render_numeric(&segs_raw, &Suffix::Dev);
    Some(Version {
        kind: VersionKind::Numeric { segments_raw: segs_raw, suffix: Suffix::Dev },
        normalized,
    })
}
