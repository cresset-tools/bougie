//! Version and Composer-constraint types.
//!
//! Bougie's constraint subset (per the plan): `>=`, `<=`, `<`, `>`, `=`,
//! `^`, `~`, `,` (intersection), `||` (union). Tilde / caret semantics
//! follow Composer's specification. Solving (satisfy / intersect) lives
//! in phase 5; this module is parsing only.

use eyre::{eyre, Result};
use std::fmt;

/// A partially-specified version: major, optional minor, optional patch.
/// Matches the `<version>` form in CLI.md §3.5.0.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PartialVersion {
    pub major: u32,
    pub minor: Option<u32>,
    pub patch: Option<u32>,
}

impl PartialVersion {
    pub fn is_exact(&self) -> bool {
        self.minor.is_some() && self.patch.is_some()
    }

    pub fn parse(s: &str) -> Result<Self> {
        if s.is_empty() {
            return Err(eyre!("empty version"));
        }
        let parts: Vec<&str> = s.split('.').collect();
        if parts.len() > 3 {
            return Err(eyre!("version has too many components: {s}"));
        }
        let major = parse_component(parts[0], "major")?;
        let minor = if parts.len() >= 2 {
            Some(parse_component(parts[1], "minor")?)
        } else {
            None
        };
        let patch = if parts.len() == 3 {
            Some(parse_component(parts[2], "patch")?)
        } else {
            None
        };
        Ok(Self { major, minor, patch })
    }
}

fn parse_component(s: &str, label: &str) -> Result<u32> {
    s.parse::<u32>()
        .map_err(|_| eyre!("invalid {label} version component: {s:?}"))
}

impl fmt::Display for PartialVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.major)?;
        if let Some(m) = self.minor {
            write!(f, ".{m}")?;
        }
        if let Some(p) = self.patch {
            write!(f, ".{p}")?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    Gte,
    Lte,
    Gt,
    Lt,
    Eq,
}

impl fmt::Display for Op {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Gte => ">=",
            Self::Lte => "<=",
            Self::Gt => ">",
            Self::Lt => "<",
            Self::Eq => "=",
        })
    }
}

/// One Composer constraint clause: a single comparison, a tilde range,
/// a caret range, or a logical combination.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Constraint {
    Op(Op, PartialVersion),
    Caret(PartialVersion),
    Tilde(PartialVersion),
    /// A bare version like `8.3.12` — the implicit `=`.
    Exact(PartialVersion),
    /// `,`-joined intersection.
    All(Vec<Constraint>),
    /// `||`-joined union.
    Any(Vec<Constraint>),
}

impl Constraint {
    pub fn parse(input: &str) -> Result<Self> {
        let input = input.trim();
        if input.is_empty() {
            return Err(eyre!("empty constraint"));
        }
        let mut anys: Vec<Constraint> = Vec::new();
        for alt in input.split("||").map(str::trim) {
            anys.push(parse_intersection(alt)?);
        }
        match anys.len() {
            0 => Err(eyre!("empty constraint")),
            1 => Ok(anys.swap_remove(0)),
            _ => Ok(Self::Any(anys)),
        }
    }
}

fn parse_intersection(input: &str) -> Result<Constraint> {
    let parts: Vec<&str> = input.split(',').map(str::trim).collect();
    let mut alls = Vec::with_capacity(parts.len());
    for part in parts {
        alls.push(parse_atom(part)?);
    }
    match alls.len() {
        0 => Err(eyre!("empty intersection")),
        1 => Ok(alls.swap_remove(0)),
        _ => Ok(Constraint::All(alls)),
    }
}

fn parse_atom(input: &str) -> Result<Constraint> {
    let s = input.trim();
    if s.is_empty() {
        return Err(eyre!("empty constraint atom"));
    }
    if let Some(rest) = s.strip_prefix(">=") {
        return Ok(Constraint::Op(Op::Gte, PartialVersion::parse(rest.trim())?));
    }
    if let Some(rest) = s.strip_prefix("<=") {
        return Ok(Constraint::Op(Op::Lte, PartialVersion::parse(rest.trim())?));
    }
    if let Some(rest) = s.strip_prefix('>') {
        return Ok(Constraint::Op(Op::Gt, PartialVersion::parse(rest.trim())?));
    }
    if let Some(rest) = s.strip_prefix('<') {
        return Ok(Constraint::Op(Op::Lt, PartialVersion::parse(rest.trim())?));
    }
    if let Some(rest) = s.strip_prefix('=') {
        return Ok(Constraint::Op(Op::Eq, PartialVersion::parse(rest.trim())?));
    }
    if let Some(rest) = s.strip_prefix('^') {
        return Ok(Constraint::Caret(PartialVersion::parse(rest.trim())?));
    }
    if let Some(rest) = s.strip_prefix('~') {
        return Ok(Constraint::Tilde(PartialVersion::parse(rest.trim())?));
    }
    Ok(Constraint::Exact(PartialVersion::parse(s)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pv(major: u32, minor: Option<u32>, patch: Option<u32>) -> PartialVersion {
        PartialVersion { major, minor, patch }
    }

    #[test]
    fn parse_versions() {
        assert_eq!(PartialVersion::parse("8").unwrap(), pv(8, None, None));
        assert_eq!(PartialVersion::parse("8.3").unwrap(), pv(8, Some(3), None));
        assert_eq!(
            PartialVersion::parse("8.3.12").unwrap(),
            pv(8, Some(3), Some(12))
        );
    }

    #[test]
    fn version_display_round_trips() {
        for s in ["8", "8.3", "8.3.12"] {
            assert_eq!(PartialVersion::parse(s).unwrap().to_string(), s);
        }
    }

    #[test]
    fn version_rejects_garbage() {
        assert!(PartialVersion::parse("").is_err());
        assert!(PartialVersion::parse("8.3.12.4").is_err());
        assert!(PartialVersion::parse("8.x").is_err());
        assert!(PartialVersion::parse("v8.3").is_err());
    }

    #[test]
    fn parse_simple_constraint_atoms() {
        assert_eq!(
            Constraint::parse(">=8.3").unwrap(),
            Constraint::Op(Op::Gte, pv(8, Some(3), None))
        );
        assert_eq!(
            Constraint::parse("^8.3").unwrap(),
            Constraint::Caret(pv(8, Some(3), None))
        );
        assert_eq!(
            Constraint::parse("~8.3.0").unwrap(),
            Constraint::Tilde(pv(8, Some(3), Some(0)))
        );
        assert_eq!(
            Constraint::parse("8.3.12").unwrap(),
            Constraint::Exact(pv(8, Some(3), Some(12)))
        );
    }

    #[test]
    fn parse_intersection() {
        let c = Constraint::parse(">=8.3, <8.5").unwrap();
        assert_eq!(
            c,
            Constraint::All(vec![
                Constraint::Op(Op::Gte, pv(8, Some(3), None)),
                Constraint::Op(Op::Lt, pv(8, Some(5), None)),
            ])
        );
    }

    #[test]
    fn parse_union() {
        let c = Constraint::parse("^7.4 || ^8.0").unwrap();
        assert_eq!(
            c,
            Constraint::Any(vec![
                Constraint::Caret(pv(7, Some(4), None)),
                Constraint::Caret(pv(8, Some(0), None)),
            ])
        );
    }

    #[test]
    fn parse_union_of_intersections() {
        let c = Constraint::parse("^7.4 || >=8.3,<8.5").unwrap();
        match c {
            Constraint::Any(inner) => assert_eq!(inner.len(), 2),
            other => panic!("expected Any, got {other:?}"),
        }
    }
}
