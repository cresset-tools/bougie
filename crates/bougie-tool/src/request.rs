//! Parse `<vendor>/<name>[@<constraint>]` from the CLI.
//!
//! Composer names are `vendor/name`. `vendor` and `name` are non-empty
//! and lowercase per Composer's published rules, but we stay permissive
//! here — Packagist / Composer itself does stricter validation on the
//! HTTP side, so bouncing exotic-but-legal names locally would be a
//! pointless re-implementation. We only enforce: there's exactly one
//! slash, neither side is empty, and the optional `@<constraint>` part
//! is non-empty when present.

use eyre::{Result, bail};

/// A user-supplied tool request, parsed but not yet resolved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolRequest {
    pub vendor: String,
    pub name: String,
    /// Composer constraint string verbatim (`^1.10`, `~2.0`, `dev-main`,
    /// …). `None` means "latest stable matching the tool's PHP".
    pub constraint: Option<String>,
}

impl ToolRequest {
    /// `vendor/name` — the canonical Composer identifier.
    pub fn package(&self) -> String {
        format!("{}/{}", self.vendor, self.name)
    }
}

pub fn parse(input: &str) -> Result<ToolRequest> {
    let (pkg, constraint) = match input.split_once('@') {
        Some((p, c)) => {
            if c.is_empty() {
                bail!("tool request `{input}` has empty constraint after `@`");
            }
            (p, Some(c.to_string()))
        }
        None => (input, None),
    };
    let Some((vendor, name)) = pkg.split_once('/') else {
        bail!(
            "tool request `{input}` is missing the vendor — expected `<vendor>/<name>[@<constraint>]`"
        );
    };
    if vendor.is_empty() || name.is_empty() {
        bail!("tool request `{input}` has empty vendor or name");
    }
    if name.contains('/') {
        bail!("tool request `{input}` contains more than one `/`");
    }
    Ok(ToolRequest {
        vendor: vendor.to_string(),
        name: name.to_string(),
        constraint,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bare_package() {
        let r = parse("phpstan/phpstan").unwrap();
        assert_eq!(r.vendor, "phpstan");
        assert_eq!(r.name, "phpstan");
        assert_eq!(r.constraint, None);
        assert_eq!(r.package(), "phpstan/phpstan");
    }

    #[test]
    fn with_constraint() {
        let r = parse("phpstan/phpstan@^1.10").unwrap();
        assert_eq!(r.constraint.as_deref(), Some("^1.10"));
    }

    #[test]
    fn dev_constraint_is_kept_verbatim() {
        // Composer accepts both `dev-main` and `main-dev`; we don't
        // canonicalize either form, just pass through.
        let r = parse("vendor/pkg@dev-main").unwrap();
        assert_eq!(r.constraint.as_deref(), Some("dev-main"));
    }

    #[test]
    fn missing_slash_errors() {
        let err = parse("phpstan").unwrap_err().to_string();
        assert!(err.contains("missing the vendor"), "{err}");
    }

    #[test]
    fn empty_constraint_errors() {
        let err = parse("phpstan/phpstan@").unwrap_err().to_string();
        assert!(err.contains("empty constraint"), "{err}");
    }

    #[test]
    fn empty_vendor_errors() {
        assert!(parse("/phpstan").is_err());
    }

    #[test]
    fn empty_name_errors() {
        assert!(parse("phpstan/").is_err());
    }

    #[test]
    fn multiple_slashes_error() {
        assert!(parse("a/b/c").is_err());
    }
}
