//! Composer-flavored constraint: parse + `matches`.
//!
//! Mirrors `Composer\Semver\VersionParser::parseConstraints` and
//! `Constraint::matches` from `composer/semver` (commit
//! `09af5e85b5f1380e4e098dde28950e2549cba4ed`, the version the
//! conformance fixtures were generated from).
//!
//! Grammar (informally):
//!
//! ```text
//! constraint := alt ("||" alt)*
//! alt        := atom (sep atom)*      -- sep is whitespace or ","
//! atom       := "*" | "x"
//!             | partial "-" partial         -- hyphenated range
//!             | caret_or_tilde version
//!             | op version                  -- >, >=, <, <=, =, ==, !=
//!             | partial_or_wildcard         -- 1, 1.2, 1.2.3, 1.2.*, 2.x.x
//! ```
//!
//! Each atom canonicalizes into an `And` of [`Op { op, version }`]
//! atoms; multi-atom intersections are `And`; `||`-joined groups are
//! `Or`. Matching is then a straight tree walk against the candidate
//! version using [`Version::compare`].

use crate::version::{is_branch_alias as bougie_semver_is_branch_alias, CmpOp, Suffix, Version, VersionKind};
use regex::Regex;
use std::sync::OnceLock;

#[derive(Debug, Clone, Eq, PartialEq)]
pub enum Constraint {
    /// `*` / `x` — matches everything.
    Any,
    /// `op X.Y.Z` — atomic operator clause. `explicit_lower_bound` is
    /// true when this clause is a `>=`/`>` whose version was written
    /// in full (`^1.2.3`, `>=1.2.3`) rather than synthesized from a
    /// partial expansion (`1` → `>=1.0.0`). Composer admits same-numeric
    /// prereleases at the lower bound only in the explicit case.
    Op {
        op: CmpOp,
        version: Version,
        explicit_lower_bound: bool,
    },
    /// Whitespace-or-comma-joined intersection.
    And(Vec<Constraint>),
    /// `||`-joined union.
    Or(Vec<Constraint>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseError {
    Invalid(String),
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Invalid(s) => write!(f, "invalid constraint: {s:?}"),
        }
    }
}

impl std::error::Error for ParseError {}

impl Constraint {
    /// Parse a Composer constraint string.
    ///
    /// # Panics
    ///
    /// Doesn't: the internal `.unwrap()`s reach `parsed.into_iter().next()`
    /// only when `parsed.len() == 1` was just checked.
    pub fn parse(input: &str) -> Result<Self, ParseError> {
        let trimmed = input.trim();
        if trimmed.is_empty() {
            return Err(ParseError::Invalid(input.to_owned()));
        }
        // Split on `||` first.
        let alts: Vec<&str> = re_or_split()
            .split(trimmed)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .collect();
        if alts.is_empty() {
            return Err(ParseError::Invalid(input.to_owned()));
        }
        let parsed: Vec<Constraint> = alts
            .into_iter()
            .map(parse_intersection)
            .collect::<Result<_, _>>()?;
        Ok(if parsed.len() == 1 {
            parsed.into_iter().next().unwrap()
        } else {
            Self::Or(parsed)
        })
    }

    /// Return whether `version` satisfies this constraint.
    ///
    /// Differs from `version.compare(op, target)` in the
    /// prerelease-vs-stable edge case: when `version` is a prerelease
    /// (e.g. `1.2.3-beta`) and `target` is the stable form with the
    /// same numeric body (`1.2.3`), Composer's constraint engine
    /// treats them as `Equal` rather than the normal "prerelease <
    /// stable" ordering. This is what makes `1.2.3-beta` satisfy
    /// `^1.2.3` and *not* satisfy `<1.2.3`.
    pub fn matches(&self, version: &Version) -> bool {
        match self {
            Self::Any => true,
            Self::Op { op, version: target, explicit_lower_bound } => {
                matches_atom(version, *op, target, *explicit_lower_bound)
            }
            Self::And(items) => items.iter().all(|c| c.matches(version)),
            Self::Or(items) => items.iter().any(|c| c.matches(version)),
        }
    }
}

fn matches_atom(candidate: &Version, op: CmpOp, target: &Version, explicit_lower: bool) -> bool {
    if same_numeric_prerelease_vs_stable(candidate, target) {
        // Upper-bound ops (`<`, `<=`) always treat the prerelease as
        // "equal to" the stable so it sits at the boundary; the
        // candidate consequently doesn't satisfy a strict `<`.
        // Lower-bound ops (`>=`, `>`) only honor that equality when
        // the constraint was written with a full version (a partial
        // expansion `1` → `>=1.0.0` should reject `1.0.0-beta`).
        // `==`/`!=` follow the same "treat as equal" path because
        // Composer's matchSpecific does too.
        let admit_as_equal = match op {
            CmpOp::Lt | CmpOp::Le | CmpOp::Eq | CmpOp::Ne => true,
            CmpOp::Gt | CmpOp::Ge => explicit_lower,
        };
        if admit_as_equal {
            return matches!(op, CmpOp::Eq | CmpOp::Ge | CmpOp::Le);
        }
    }
    candidate.compare(op, target)
}

/// True iff `a` and `b` are both numeric, have the same numeric
/// segments, and exactly one is a prerelease (Prerelease /
/// `PrereleaseDev` / `PatchDev` / Dev) while the other is Stable.
fn same_numeric_prerelease_vs_stable(a: &Version, b: &Version) -> bool {
    let (
        VersionKind::Numeric { segments_raw: sa, suffix: suf_a },
        VersionKind::Numeric { segments_raw: sb, suffix: suf_b },
    ) = (&a.kind, &b.kind) else { return false };
    if !segments_equal_numerically(sa, sb) {
        return false;
    }
    is_prerelease(suf_a) ^ is_prerelease(suf_b)
}

fn is_prerelease(s: &Suffix) -> bool {
    matches!(
        s,
        Suffix::Prerelease { .. }
            | Suffix::PrereleaseDev { .. }
            | Suffix::PatchDev(_)
            | Suffix::Dev
    )
}

fn segments_equal_numerically(a: &[String], b: &[String]) -> bool {
    let len = a.len().max(b.len());
    for i in 0..len {
        let va: u64 = a.get(i).and_then(|s| s.parse().ok()).unwrap_or(0);
        let vb: u64 = b.get(i).and_then(|s| s.parse().ok()).unwrap_or(0);
        if va != vb {
            return false;
        }
    }
    true
}

// ---- top-level splitters -----------------------------------------------

fn re_or_split() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"\s*\|\|?\s*").unwrap())
}

/// One `||`-alt: a whitespace-or-comma-separated list of atoms,
/// possibly with a hyphenated range form (`A - B`) inside. Returns
/// the intersection as an [`And`] (or the bare atom if there's only
/// one).
fn parse_intersection(s: &str) -> Result<Constraint, ParseError> {
    // Handle hyphenated range `A - B` first — the `-` is whitespace-
    // delimited (`A-B` without spaces would be a single token).
    if let Some(range) = parse_hyphen_range(s)? {
        return Ok(range);
    }
    let tokens = tokenize_intersection(s);
    let parsed: Vec<Constraint> = tokens
        .iter()
        .map(|t| parse_atom(t))
        .collect::<Result<_, _>>()?;
    if parsed.is_empty() {
        return Err(ParseError::Invalid(s.to_owned()));
    }
    Ok(if parsed.len() == 1 {
        parsed.into_iter().next().unwrap()
    } else {
        Constraint::And(parsed)
    })
}

/// Tokenize an intersection: split on whitespace OR `,`, but keep
/// operator + version pairs glued together (`>= 1.2.3` becomes one
/// token `>=1.2.3`).
fn tokenize_intersection(s: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut chars = s.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == ',' || ch.is_whitespace() {
            // If we're mid-operator (e.g. just saw `>=`), absorb the
            // whitespace and continue building the token from the
            // next non-space chars. We detect this by checking if
            // `cur` is a pure-operator prefix.
            if !cur.is_empty() && is_pending_operator(&cur) {
                // Skip whitespace and continue with the version.
                while let Some(&peek) = chars.peek() {
                    if peek.is_whitespace() {
                        chars.next();
                    } else {
                        break;
                    }
                }
                continue;
            }
            if !cur.is_empty() {
                out.push(std::mem::take(&mut cur));
            }
            continue;
        }
        cur.push(ch);
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// True when the current accumulator looks like an operator prefix
/// that's waiting for its version operand. Catches `>=`, `<=`, `>`,
/// `<`, `=`, `==`, `!=`, `<>`.
fn is_pending_operator(s: &str) -> bool {
    matches!(s, ">" | ">=" | "<" | "<=" | "=" | "==" | "!=" | "<>")
}

// ---- hyphenated range --------------------------------------------------

fn re_hyphen() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| {
        // `(A) ... - ... (B)` — the dash must have whitespace on at
        // least one side (so `2.4.0-alpha` is not split). Build
        // metadata after either side is allowed (we strip it on the
        // partial parse).
        Regex::new(r"^(?P<lo>[^\s]+)\s+-\s+(?P<hi>[^\s]+)$").unwrap()
    })
}

fn parse_hyphen_range(s: &str) -> Result<Option<Constraint>, ParseError> {
    let Some(caps) = re_hyphen().captures(s.trim()) else {
        return Ok(None);
    };
    let lo_str = caps.name("lo").unwrap().as_str();
    let hi_str = caps.name("hi").unwrap().as_str();
    let lo = expand_partial_lower(lo_str)?;
    let hi = expand_partial_upper(hi_str)?;
    Ok(Some(Constraint::And(vec![
        Constraint::Op { op: CmpOp::Ge, version: lo, explicit_lower_bound: false },
        Constraint::Op { op: CmpOp::Le, version: hi, explicit_lower_bound: false },
    ])))
}

/// Expand a partial version (`1.2` / `1` / `1.2.3`) for the lower
/// bound of a hyphenated range — missing segments become `0`.
fn expand_partial_lower(s: &str) -> Result<Version, ParseError> {
    // Strip build metadata; doesn't affect comparison.
    let cleaned = s.split_once('+').map_or(s, |(left, _)| left);
    let padded = pad_partial(cleaned);
    Version::parse(&padded).map_err(|e| ParseError::Invalid(e.to_string()))
}

/// Expand a partial version for the upper bound — missing segments
/// become `9999999` so the comparison `<=` reaches every patch in
/// the named minor / major.
fn expand_partial_upper(s: &str) -> Result<Version, ParseError> {
    let cleaned = s.split_once('+').map_or(s, |(left, _)| left);
    // Composer's `1.0 - 2.0` is `>=1.0.0, <2.1` — partials on the
    // upper bound expand to "anything in the named scope". For our
    // representation we keep the inclusive form by parsing with the
    // 9999999 pads.
    let segs = split_partial_segments(cleaned);
    if segs.is_empty() {
        return Err(ParseError::Invalid(s.to_owned()));
    }
    let padded = pad_segments_with(segs, "9999999", 4);
    let candidate = padded.join(".");
    Version::parse(&candidate).map_err(|e| ParseError::Invalid(e.to_string()))
}

// ---- atom parsing ------------------------------------------------------

fn parse_atom(token: &str) -> Result<Constraint, ParseError> {
    let s = token.trim();
    if s.is_empty() {
        return Err(ParseError::Invalid(token.to_owned()));
    }
    // Strip any `#<commit-ref>` suffix Composer accepts on any
    // constraint atom (`"acme/foo": "3.x-dev#abc1234"`). For
    // resolution purposes the ref is purely informational — the
    // installer uses it to pin a specific commit of the branch, but
    // the matcher behaves the same as without it.
    let s = s.split_once('#').map_or(s, |(left, _)| left).trim_end();
    // `*` / `x` / `X`
    if matches!(s, "*" | "x" | "X") {
        return Ok(Constraint::Any);
    }

    // Branch references appear in Composer constraints in two
    // related shapes (cross-referenced against Composer's
    // `Composer\Semver\VersionParser::parseConstraint` "Basic
    // Comparators" fallback + `parseStability`):
    //   1. `Nx-dev` / `N.x-dev` / `N.M.x-dev` — numeric-flavor
    //      branch alias (e.g. `3.x-dev`). `Version::parse`
    //      normalizes this to `N.9999999.9999999.9999999-dev`.
    //   2. `dev-<branch-name>` — bare branch reference (e.g.
    //      `dev-main`, `dev-feature/foo`). `Version::parse` keeps
    //      this as `VersionKind::Branch("<name>")`.
    // Both map to an `==` constraint against the same string
    // re-parsed as a Version — matches Composer's "operator `=`,
    // version normalized" handling. Composer matches `dev-`
    // case-insensitively (`stripos`/`/i` regex) so we do too.
    // Marked explicit-lower-bound so any same-numeric-prerelease
    // comparison falls through to standard ordering.
    let is_dev_branch = s.len() >= 4 && s.as_bytes()[..4].eq_ignore_ascii_case(b"dev-");
    if bougie_semver_is_branch_alias(s) || is_dev_branch {
        let v = Version::parse(s)
            .map_err(|e| ParseError::Invalid(format!("{token}: {e}")))?;
        return Ok(Constraint::Op {
            op: CmpOp::Eq,
            version: v,
            explicit_lower_bound: true,
        });
    }

    // Caret: `^X.Y.Z`
    if let Some(rest) = s.strip_prefix('^') {
        return parse_caret(rest.trim());
    }
    // Tilde: `~X.Y.Z`
    if let Some(rest) = s.strip_prefix('~') {
        return parse_tilde(rest.trim());
    }

    // Operators: `>=`, `<=`, `>`, `<`, `=`, `==`, `!=`, `<>`.
    for prefix in &[">=", "<=", "==", "!=", "<>", ">", "<", "="] {
        if let Some(rest) = s.strip_prefix(*prefix) {
            let op = match *prefix {
                ">=" => CmpOp::Ge,
                "<=" => CmpOp::Le,
                ">" => CmpOp::Gt,
                "<" => CmpOp::Lt,
                "==" | "=" => CmpOp::Eq,
                "!=" | "<>" => CmpOp::Ne,
                _ => unreachable!(),
            };
            let v = Version::parse(rest.trim())
                .map_err(|e| ParseError::Invalid(format!("{token}: {e}")))?;
            // Explicit lower bound iff the user wrote `>=`/`>` with
            // all three numeric segments. Three+ written segments
            // implies an intentional pre-release boundary; fewer
            // implies partial expansion semantics.
            let explicit_lower_bound = matches!(op, CmpOp::Ge | CmpOp::Gt)
                && wrote_full_version(rest.trim());
            return Ok(Constraint::Op { op, version: v, explicit_lower_bound });
        }
    }

    // Wildcard inside a numeric expression: `1.2.*`, `2.x.x`,
    // `1.2.X`.
    if contains_wildcard(s) {
        return parse_wildcard(s);
    }

    // Bare partial or full version: `1.2.3` → ==, `1.2` → range
    // covering all of 1.2, `1` → range covering all of 1.
    parse_partial_or_exact(s)
}

fn contains_wildcard(s: &str) -> bool {
    s.split('.').any(|p| matches!(p, "*" | "x" | "X"))
}

fn parse_caret(rest: &str) -> Result<Constraint, ParseError> {
    // `^X.Y.Z[-stab]` → `>=X.Y.Z[-stab], <X+1.0.0` when X≥1
    //                 → `>=0.Y.Z, <0.Y+1.0`            when X=0 and Y>0
    //                 → `>=0.0.Z, <0.0.Z+1`            when X=0 and Y=0
    let segs = split_partial_numeric(rest)?;
    if segs.is_empty() {
        return Err(ParseError::Invalid(rest.to_owned()));
    }
    let major: u32 = segs[0].parse().unwrap_or(0);
    let minor: u32 = segs.get(1).and_then(|s| s.parse().ok()).unwrap_or(0);
    let lower = Version::parse(rest)
        .or_else(|_| Version::parse(&pad_partial(rest)))
        .map_err(|e| ParseError::Invalid(format!("^{rest}: {e}")))?;
    let upper_str = if major > 0 {
        format!("{}.0.0.0", major + 1)
    } else if minor > 0 {
        format!("0.{}.0.0", minor + 1)
    } else {
        // `^0.0.1` → `>=0.0.1, <0.0.2` (only the patch is fluid).
        let patch: u32 = segs.get(2).and_then(|s| s.parse().ok()).unwrap_or(0);
        format!("0.0.{}.0", patch + 1)
    };
    let upper = Version::parse(&upper_str)
        .map_err(|e| ParseError::Invalid(format!("^{rest} upper: {e}")))?;
    // The caret's lower bound is "explicit" when the user wrote a
    // full X.Y.Z — that's when prereleases of X.Y.Z are admissible.
    let explicit_lower = wrote_full_version(rest);
    Ok(Constraint::And(vec![
        Constraint::Op { op: CmpOp::Ge, version: lower, explicit_lower_bound: explicit_lower },
        Constraint::Op { op: CmpOp::Lt, version: upper, explicit_lower_bound: false },
    ]))
}

fn parse_tilde(rest: &str) -> Result<Constraint, ParseError> {
    // `~X.Y.Z[-stab]` → `>=X.Y.Z[-stab], <X.Y+1.0`
    // `~X.Y`         → `>=X.Y.0, <X+1.0.0`     (major-floor)
    // `~X`           → `>=X.0.0, <X+1.0.0`
    let segs = split_partial_numeric(rest)?;
    if segs.is_empty() {
        return Err(ParseError::Invalid(rest.to_owned()));
    }
    let lower = Version::parse(rest)
        .or_else(|_| Version::parse(&pad_partial(rest)))
        .map_err(|e| ParseError::Invalid(format!("~{rest}: {e}")))?;
    let upper_str = match segs.len() {
        1 => {
            let major: u32 = segs[0].parse().unwrap_or(0);
            format!("{}.0.0.0", major + 1)
        }
        2 => {
            let major: u32 = segs[0].parse().unwrap_or(0);
            format!("{}.0.0.0", major + 1)
        }
        _ => {
            let major: u32 = segs[0].parse().unwrap_or(0);
            let minor: u32 = segs[1].parse().unwrap_or(0);
            format!("{}.{}.0.0", major, minor + 1)
        }
    };
    let upper = Version::parse(&upper_str)
        .map_err(|e| ParseError::Invalid(format!("~{rest} upper: {e}")))?;
    let explicit_lower = wrote_full_version(rest);
    Ok(Constraint::And(vec![
        Constraint::Op { op: CmpOp::Ge, version: lower, explicit_lower_bound: explicit_lower },
        Constraint::Op { op: CmpOp::Lt, version: upper, explicit_lower_bound: false },
    ]))
}

fn parse_wildcard(s: &str) -> Result<Constraint, ParseError> {
    // `1.2.*` → `>=1.2.0, <1.3.0`
    // `2.*.*` → `>=2.0.0, <3.0.0` (wildcard at position 1)
    // `2.x.x` → same as above
    let parts: Vec<&str> = s.split('.').collect();
    // The position of the FIRST wildcard determines the floor.
    let first_wild = parts
        .iter()
        .position(|p| matches!(*p, "*" | "x" | "X"))
        .ok_or_else(|| ParseError::Invalid(s.to_owned()))?;
    let numeric_prefix: Vec<u32> = parts[..first_wild]
        .iter()
        .map(|p| p.parse().unwrap_or(0))
        .collect();
    let lower_segs: Vec<String> = numeric_prefix
        .iter()
        .map(u32::to_string)
        .chain(std::iter::repeat_n("0".to_owned(), 4usize.saturating_sub(first_wild)))
        .collect();
    let lower = Version::parse(&lower_segs.join("."))
        .map_err(|e| ParseError::Invalid(format!("{s} lower: {e}")))?;

    let mut upper_segs = numeric_prefix.clone();
    if first_wild == 0 {
        // `*` alone → Any.
        return Ok(Constraint::Any);
    }
    *upper_segs.last_mut().unwrap() += 1;
    let upper_str: String = upper_segs
        .iter()
        .map(u32::to_string)
        .chain(std::iter::repeat_n("0".to_owned(), 4usize.saturating_sub(upper_segs.len())))
        .collect::<Vec<_>>()
        .join(".");
    let upper = Version::parse(&upper_str)
        .map_err(|e| ParseError::Invalid(format!("{s} upper: {e}")))?;
    Ok(Constraint::And(vec![
        Constraint::Op { op: CmpOp::Ge, version: lower, explicit_lower_bound: false },
        Constraint::Op { op: CmpOp::Lt, version: upper, explicit_lower_bound: false },
    ]))
}

fn parse_partial_or_exact(s: &str) -> Result<Constraint, ParseError> {
    // Strip build metadata (`+...`) — Composer accepts it on
    // constraint atoms and ignores it for matching.
    let cleaned = s.split_once('+').map_or(s, |(left, _)| left);
    let segs = split_partial_numeric(cleaned)?;
    if segs.len() >= 3 {
        // Fully qualified → exact match (==). Exact equality with a
        // full version is intentionally "explicit" too — the user
        // pinned the version on the nose.
        let v = Version::parse(cleaned)
            .map_err(|e| ParseError::Invalid(format!("{s}: {e}")))?;
        return Ok(Constraint::Op { op: CmpOp::Eq, version: v, explicit_lower_bound: true });
    }
    // Partial — expand into a wildcard-style range.
    // `1.2` → `>=1.2.0, <1.3.0`
    // `1`   → `>=1.0.0, <2.0.0`
    let major: u32 = segs[0].parse().unwrap_or(0);
    let lower_str = pad_partial(cleaned);
    let lower = Version::parse(&lower_str)
        .map_err(|e| ParseError::Invalid(format!("{s} lower: {e}")))?;
    let upper_str = if segs.len() == 1 {
        format!("{}.0.0.0", major + 1)
    } else {
        let minor: u32 = segs[1].parse().unwrap_or(0);
        format!("{}.{}.0.0", major, minor + 1)
    };
    let upper = Version::parse(&upper_str)
        .map_err(|e| ParseError::Invalid(format!("{s} upper: {e}")))?;
    Ok(Constraint::And(vec![
        Constraint::Op { op: CmpOp::Ge, version: lower, explicit_lower_bound: false },
        Constraint::Op { op: CmpOp::Lt, version: upper, explicit_lower_bound: false },
    ]))
}

// ---- helpers -----------------------------------------------------------

/// True iff `s` looks like a full Composer version (three or more
/// numeric segments before any stability/build tail). Used to decide
/// whether a constraint's lower bound is "explicit" enough to admit
/// same-numeric prereleases.
fn wrote_full_version(s: &str) -> bool {
    let s = s.trim();
    let s = s.strip_prefix(['v', 'V']).unwrap_or(s);
    let body = s.split_once(['-', '+']).map_or(s, |(left, _)| left);
    let segs: Vec<&str> = body.split('.').filter(|p| !p.is_empty()).collect();
    segs.len() >= 3 && segs.iter().all(|p| p.chars().all(|c| c.is_ascii_digit()))
}

fn split_partial_numeric(s: &str) -> Result<Vec<String>, ParseError> {
    let cleaned = s
        .strip_prefix(['v', 'V'])
        .unwrap_or(s)
        .split_once('-')
        .map_or(s.strip_prefix(['v', 'V']).unwrap_or(s), |(left, _)| left);
    let segs: Vec<String> = cleaned
        .split('.')
        .filter(|p| !p.is_empty())
        .map(str::to_owned)
        .collect();
    if segs.is_empty() {
        return Err(ParseError::Invalid(s.to_owned()));
    }
    // Ensure leading segment is numeric (rejects garbage like
    // `>=abc`).
    if !segs[0].chars().all(|c| c.is_ascii_digit()) {
        return Err(ParseError::Invalid(s.to_owned()));
    }
    Ok(segs)
}

fn split_partial_segments(s: &str) -> Vec<String> {
    let stripped = s.strip_prefix(['v', 'V']).unwrap_or(s);
    let cleaned = stripped.split_once('-').map_or(stripped, |(left, _)| left);
    cleaned
        .split('.')
        .filter(|p| !p.is_empty())
        .map(str::to_owned)
        .collect()
}

/// Pad a partial numeric to 4 segments with `"0"` and reattach any
/// stability suffix.
fn pad_partial(s: &str) -> String {
    let stripped = s.strip_prefix(['v', 'V']).unwrap_or(s);
    let (body, tail) = match stripped.split_once('-') {
        Some((b, t)) => (b, format!("-{t}")),
        None => (stripped, String::new()),
    };
    let segs: Vec<&str> = body.split('.').filter(|p| !p.is_empty()).collect();
    let mut padded: Vec<String> = segs.iter().map(|p| (*p).to_owned()).collect();
    while padded.len() < 4 {
        padded.push("0".to_owned());
    }
    format!("{}{}", padded.join("."), tail)
}

fn pad_segments_with(mut segs: Vec<String>, pad: &str, target: usize) -> Vec<String> {
    while segs.len() < target {
        segs.push(pad.to_owned());
    }
    segs
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Composer accepts `Nx-dev` / `N.x-dev` / `N.M.x-dev` as
    /// constraint strings meaning "exactly the named dev branch."
    /// They parse to an `==` constraint against the same string
    /// re-parsed as a Version (which already canonicalizes
    /// `3.x-dev` into `3.9999999.9999999.9999999-dev`).
    #[test]
    fn nx_dev_constraint_matches_same_branch_version() {
        let c = Constraint::parse("3.x-dev").unwrap();
        let v = Version::parse("3.x-dev").unwrap();
        assert!(c.matches(&v), "got constraint {c:?}");
    }

    #[test]
    fn nx_dev_constraint_rejects_other_major_branch() {
        let c = Constraint::parse("3.x-dev").unwrap();
        let other = Version::parse("2.x-dev").unwrap();
        assert!(!c.matches(&other), "got constraint {c:?}");
    }

    #[test]
    fn nx_dev_constraint_rejects_stable_release() {
        // `3.x-dev` is the exact dev branch; a stable `3.0.0` does
        // not satisfy it.
        let c = Constraint::parse("3.x-dev").unwrap();
        let stable = Version::parse("3.0.0").unwrap();
        assert!(!c.matches(&stable), "got constraint {c:?}");
    }

    #[test]
    fn n_dot_x_dev_handles_two_segment_form() {
        let c = Constraint::parse("1.x-dev").unwrap();
        let v = Version::parse("1.x-dev").unwrap();
        assert!(c.matches(&v));
    }

    #[test]
    fn n_dot_m_dot_x_dev_handles_three_segment_form() {
        // `1.0.x-dev` parses to `1.0.9999999.9999999-dev`.
        let c = Constraint::parse("1.0.x-dev").unwrap();
        let v = Version::parse("1.0.x-dev").unwrap();
        assert!(c.matches(&v));
    }

    #[test]
    fn dev_branch_constraint_matches_named_branch() {
        // `"dep": "dev-main"` is the bare branch form — pinned to
        // the named branch. Parses to `==` against
        // `Version::Branch("main")`.
        let c = Constraint::parse("dev-main").unwrap();
        let v = Version::parse("dev-main").unwrap();
        assert!(c.matches(&v), "constraint {c:?} should match {v:?}");
    }

    #[test]
    fn dev_branch_constraint_rejects_other_branches() {
        let c = Constraint::parse("dev-main").unwrap();
        let other = Version::parse("dev-feature-x").unwrap();
        assert!(!c.matches(&other));
    }

    #[test]
    fn dev_branch_constraint_handles_slashed_branch_name() {
        // Composer accepts branch names with slashes
        // (e.g. `dev-fix/some-bug`). Parses through Version::parse,
        // which preserves the branch body verbatim.
        let c = Constraint::parse("dev-fix/some-bug").unwrap();
        let v = Version::parse("dev-fix/some-bug").unwrap();
        assert!(c.matches(&v));
    }
}
