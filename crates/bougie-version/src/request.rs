//! PHP version request parser per CLI.md §3.5.0.
//!
//! Accepts the nine forms enumerated in the spec table and yields a
//! [`Request`]. Resolution against the index / installed set lives in
//! the resolver (phase 5).

use crate::version::{Constraint, PartialVersion};
use eyre::{eyre, Result};
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Flavor {
    Nts,
    NtsDebug,
    Zts,
    ZtsDebug,
}

impl Flavor {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Nts => "nts",
            Self::NtsDebug => "nts-debug",
            Self::Zts => "zts",
            Self::ZtsDebug => "zts-debug",
        }
    }
}

impl std::fmt::Display for Flavor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VersionLike {
    Version(PartialVersion),
    Constraint(Constraint),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Request {
    /// Bare version, constraint, or version+flavor pair (forms 1–4, 6, 7).
    VersionLike {
        spec: VersionLike,
        flavor: Option<Flavor>,
    },
    /// `php-<version>-<target>[-<flavor>]` (form 8).
    FullTag {
        version: PartialVersion,
        target: String,
        flavor: Option<Flavor>,
    },
    /// Absolute path or `~`-prefixed path (form 9 / install-dir).
    Path(PathBuf),
    /// Bare name to resolve on `PATH` (form 10).
    Name(String),
}

pub fn parse_request(input: &str) -> Result<Request> {
    let s = input.trim();
    if s.is_empty() {
        return Err(eyre!("empty request"));
    }

    // 1. Path-shaped (contains '/' or starts with '~').
    if s.contains('/') || s.starts_with('~') {
        return Ok(Request::Path(PathBuf::from(s)));
    }

    // 2. Full tag: php-<version>-<target>[-<flavor>].
    if let Some(rest) = s.strip_prefix("php-") {
        return parse_full_tag(rest);
    }

    // 3. php@<version-or-constraint>[<flavor>].
    if let Some(rest) = s.strip_prefix("php@") {
        return parse_versionlike_with_flavor(rest);
    }

    // 4. php<digit-or-op>... — constraint or compact version.
    if let Some(rest) = s.strip_prefix("php") {
        let first = rest.chars().next();
        match first {
            Some(c) if c.is_ascii_digit() => {
                let normalized = if rest.contains('.') {
                    rest.to_string()
                } else if let Some(flavor_idx) = first_non_digit(rest) {
                    // e.g. "83z" or "83+zts" — split digits from flavor head.
                    let (digits, tail) = rest.split_at(flavor_idx);
                    format!("{}{}", expand_compact_digits(digits), tail)
                } else {
                    expand_compact_digits(rest)
                };
                return parse_versionlike_with_flavor(&normalized);
            }
            Some(c) if is_constraint_lead(c) => {
                return parse_versionlike_with_flavor(rest);
            }
            // "php" alone or "phpunit" etc. — fall through to name.
            _ => return Ok(Request::Name(s.to_string())),
        }
    }

    // 5. Bare version, constraint, or version+flavor.
    if let Some(c) = s.chars().next()
        && (c.is_ascii_digit() || is_constraint_lead(c))
    {
        return parse_versionlike_with_flavor(s);
    }

    // 6. Fallback: name on PATH.
    Ok(Request::Name(s.to_string()))
}

fn parse_versionlike_with_flavor(s: &str) -> Result<Request> {
    let (head, flavor) = strip_flavor_suffix(s);
    let spec = if head.starts_with(is_constraint_lead) || head.contains(',') || head.contains("||") {
        VersionLike::Constraint(Constraint::parse(head)?)
    } else {
        VersionLike::Version(PartialVersion::parse(head)?)
    };
    Ok(Request::VersionLike { spec, flavor })
}

fn parse_full_tag(rest: &str) -> Result<Request> {
    let parts: Vec<&str> = rest.split('-').collect();
    // version + at least 3 target components (arch-vendor-os).
    if parts.len() < 4 {
        return Err(eyre!("full tag is malformed: {rest}"));
    }
    let version = PartialVersion::parse(parts[0])?;

    // Detect trailing flavor: either single-word (nts|zts) or two-word
    // (nts-debug|zts-debug).
    let len = parts.len();
    let (target_parts, flavor) = if len >= 6 && parts[len - 1] == "debug" && (parts[len - 2] == "nts" || parts[len - 2] == "zts") {
        let f = if parts[len - 2] == "nts" { Flavor::NtsDebug } else { Flavor::ZtsDebug };
        (&parts[1..len - 2], Some(f))
    } else if let Some(f) = parse_flavor_word(parts[len - 1]) {
        (&parts[1..len - 1], Some(f))
    } else {
        (&parts[1..], None)
    };

    if target_parts.len() < 3 {
        return Err(eyre!("full tag target has too few components: {rest}"));
    }
    let target = target_parts.join("-");
    Ok(Request::FullTag { version, target, flavor })
}

/// "8" → "8", "83" → "8.3", "84" → "8.4". For PHP, the major is always
/// a single digit; the rest is the minor.
fn expand_compact_digits(s: &str) -> String {
    if s.contains('.') || s.len() < 2 {
        return s.to_string();
    }
    let (a, b) = s.split_at(1);
    format!("{a}.{b}")
}

fn first_non_digit(s: &str) -> Option<usize> {
    s.char_indices().find(|(_, c)| !c.is_ascii_digit()).map(|(i, _)| i)
}

fn is_constraint_lead(c: char) -> bool {
    matches!(c, '>' | '<' | '=' | '^' | '~')
}

fn parse_flavor_word(s: &str) -> Option<Flavor> {
    match s {
        "nts" => Some(Flavor::Nts),
        "zts" => Some(Flavor::Zts),
        _ => None,
    }
}

/// Strip a flavor suffix from `s`. Tries `+`-form first (longer matches
/// first), then short-form (z/d/zd).
fn strip_flavor_suffix(s: &str) -> (&str, Option<Flavor>) {
    for (suffix, flavor) in [
        ("+zts-debug", Flavor::ZtsDebug),
        ("+nts-debug", Flavor::NtsDebug),
        ("+zts", Flavor::Zts),
        ("+nts", Flavor::Nts),
        ("+debug", Flavor::NtsDebug),
    ] {
        if let Some(head) = s.strip_suffix(suffix) {
            return (head, Some(flavor));
        }
    }
    // Short forms — only consume when the preceding character is a digit
    // (otherwise we'd eat the trailing 'd' off "stable-debug" etc.).
    let bytes = s.as_bytes();
    let preceded_by_digit = |idx: usize| idx > 0 && bytes[idx - 1].is_ascii_digit();
    if s.ends_with("zd") && preceded_by_digit(s.len() - 2) {
        return (&s[..s.len() - 2], Some(Flavor::ZtsDebug));
    }
    if s.ends_with('z') && preceded_by_digit(s.len() - 1) {
        return (&s[..s.len() - 1], Some(Flavor::Zts));
    }
    if s.ends_with('d') && preceded_by_digit(s.len() - 1) {
        return (&s[..s.len() - 1], Some(Flavor::NtsDebug));
    }
    (s, None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::version::Op;

    fn pv(major: u32, minor: Option<u32>, patch: Option<u32>) -> PartialVersion {
        PartialVersion { major, minor, patch }
    }

    fn version_request(spec: VersionLike, flavor: Option<Flavor>) -> Request {
        Request::VersionLike { spec, flavor }
    }

    #[test]
    fn bare_versions() {
        assert_eq!(
            parse_request("8").unwrap(),
            version_request(VersionLike::Version(pv(8, None, None)), None)
        );
        assert_eq!(
            parse_request("8.3").unwrap(),
            version_request(VersionLike::Version(pv(8, Some(3), None)), None)
        );
        assert_eq!(
            parse_request("8.3.12").unwrap(),
            version_request(VersionLike::Version(pv(8, Some(3), Some(12))), None)
        );
    }

    #[test]
    fn constraints() {
        assert_eq!(
            parse_request("^8.3").unwrap(),
            version_request(
                VersionLike::Constraint(Constraint::Caret(pv(8, Some(3), None))),
                None,
            )
        );
        let req = parse_request(">=8.3,<8.5").unwrap();
        assert!(matches!(
            req,
            Request::VersionLike { spec: VersionLike::Constraint(Constraint::All(_)), flavor: None }
        ));
    }

    #[test]
    fn short_variant_suffixes() {
        assert_eq!(
            parse_request("8.3z").unwrap(),
            version_request(VersionLike::Version(pv(8, Some(3), None)), Some(Flavor::Zts))
        );
        assert_eq!(
            parse_request("8.3d").unwrap(),
            version_request(VersionLike::Version(pv(8, Some(3), None)), Some(Flavor::NtsDebug))
        );
        assert_eq!(
            parse_request("8.3zd").unwrap(),
            version_request(VersionLike::Version(pv(8, Some(3), None)), Some(Flavor::ZtsDebug))
        );
    }

    #[test]
    fn plus_variants() {
        assert_eq!(
            parse_request("8.3+zts").unwrap(),
            version_request(VersionLike::Version(pv(8, Some(3), None)), Some(Flavor::Zts))
        );
        assert_eq!(
            parse_request("8.3.12+debug").unwrap(),
            version_request(
                VersionLike::Version(pv(8, Some(3), Some(12))),
                Some(Flavor::NtsDebug)
            )
        );
        assert_eq!(
            parse_request("8.3+zts-debug").unwrap(),
            version_request(VersionLike::Version(pv(8, Some(3), None)), Some(Flavor::ZtsDebug))
        );
    }

    #[test]
    fn php_at_form() {
        assert_eq!(
            parse_request("php@8.3").unwrap(),
            version_request(VersionLike::Version(pv(8, Some(3), None)), None)
        );
        assert_eq!(
            parse_request("php@8.3.12").unwrap(),
            version_request(VersionLike::Version(pv(8, Some(3), Some(12))), None)
        );
    }

    #[test]
    fn php_prefix_compact_and_dotted() {
        assert_eq!(
            parse_request("php8.3").unwrap(),
            version_request(VersionLike::Version(pv(8, Some(3), None)), None)
        );
        assert_eq!(
            parse_request("php83").unwrap(),
            version_request(VersionLike::Version(pv(8, Some(3), None)), None)
        );
    }

    #[test]
    fn php_prefix_with_constraint() {
        let req = parse_request("php>=8.3,<8.4").unwrap();
        match req {
            Request::VersionLike { spec: VersionLike::Constraint(Constraint::All(parts)), .. } => {
                assert_eq!(parts.len(), 2);
                assert!(matches!(parts[0], Constraint::Op(Op::Gte, _)));
                assert!(matches!(parts[1], Constraint::Op(Op::Lt, _)));
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn full_tag_with_explicit_flavor() {
        assert_eq!(
            parse_request("php-8.3.12-aarch64-apple-darwin-nts").unwrap(),
            Request::FullTag {
                version: pv(8, Some(3), Some(12)),
                target: "aarch64-apple-darwin".into(),
                flavor: Some(Flavor::Nts),
            }
        );
    }

    #[test]
    fn full_tag_with_two_word_flavor() {
        assert_eq!(
            parse_request("php-8.3.12-x86_64-unknown-linux-gnu-zts-debug").unwrap(),
            Request::FullTag {
                version: pv(8, Some(3), Some(12)),
                target: "x86_64-unknown-linux-gnu".into(),
                flavor: Some(Flavor::ZtsDebug),
            }
        );
    }

    #[test]
    fn full_tag_without_flavor() {
        assert_eq!(
            parse_request("php-8.3.12-x86_64-unknown-linux-gnu").unwrap(),
            Request::FullTag {
                version: pv(8, Some(3), Some(12)),
                target: "x86_64-unknown-linux-gnu".into(),
                flavor: None,
            }
        );
    }

    #[test]
    fn path_shaped() {
        assert_eq!(
            parse_request("/opt/php/bin/php").unwrap(),
            Request::Path(PathBuf::from("/opt/php/bin/php"))
        );
        assert_eq!(
            parse_request("~/.local/share/bougie/installs/8.3.12-nts/bin/php").unwrap(),
            Request::Path(PathBuf::from("~/.local/share/bougie/installs/8.3.12-nts/bin/php"))
        );
    }

    #[test]
    fn name_on_path() {
        assert_eq!(parse_request("php").unwrap(), Request::Name("php".into()));
        // "phpunit" doesn't start with php-digit/op, so it's a name.
        assert_eq!(
            parse_request("phpunit").unwrap(),
            Request::Name("phpunit".into())
        );
    }

    #[test]
    fn rejects_empty() {
        assert!(parse_request("").is_err());
        assert!(parse_request("   ").is_err());
    }

    #[test]
    fn flavor_suffix_only_after_digit() {
        // "php" alone doesn't get its 'p' eaten as a flavor short form.
        assert_eq!(parse_request("php").unwrap(), Request::Name("php".into()));
    }
}
