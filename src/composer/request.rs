//! Parser for the Composer-version request grammar (CLI.md §3.7).
//!
//! Accepted forms:
//!   - `latest` / `stable`               → `Channel::Stable`
//!   - `preview`                         → `Channel::Preview`
//!   - exact `2.8.5`                     → Exact
//!   - partial `2`, `2.8`                → Partial
//!   - absolute path (starts with `/` or `~`) → Path
//!
//! Unknown forms error. There is no constraint solver — each version is
//! a literal lookup against the channel snapshot from getcomposer.org.

use eyre::{eyre, Result};
use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Channel {
    Stable,
    Preview,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ComposerRequest {
    Channel(Channel),
    Exact(String),
    /// `<major>` or `<major>.<minor>` — highest matching across stable+preview.
    Partial(String),
    Path(PathBuf),
}

pub fn parse_request(s: &str) -> Result<ComposerRequest> {
    let trimmed = s.trim();
    if trimmed.is_empty() {
        return Err(eyre!("composer version request cannot be empty"));
    }
    if trimmed.starts_with('/') || trimmed.starts_with('~') {
        return Ok(ComposerRequest::Path(PathBuf::from(trimmed)));
    }
    match trimmed {
        "latest" | "stable" => return Ok(ComposerRequest::Channel(Channel::Stable)),
        "preview" => return Ok(ComposerRequest::Channel(Channel::Preview)),
        _ => {}
    }
    if let Some(kind) = classify_version(trimmed) {
        return Ok(kind);
    }
    Err(eyre!(
        "unrecognized composer version request: {trimmed:?} \
         (expected stable | preview | latest | <major> | <major>.<minor> | <major>.<minor>.<patch> | /abs/path)"
    ))
}

fn classify_version(s: &str) -> Option<ComposerRequest> {
    let parts: Vec<&str> = s.split('.').collect();
    if parts.is_empty() || parts.len() > 3 {
        return None;
    }
    if !parts.iter().all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit())) {
        return None;
    }
    Some(match parts.len() {
        3 => ComposerRequest::Exact(s.to_owned()),
        _ => ComposerRequest::Partial(s.to_owned()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_channel_aliases() {
        assert_eq!(
            parse_request("stable").unwrap(),
            ComposerRequest::Channel(Channel::Stable)
        );
        assert_eq!(
            parse_request("latest").unwrap(),
            ComposerRequest::Channel(Channel::Stable)
        );
        assert_eq!(
            parse_request("preview").unwrap(),
            ComposerRequest::Channel(Channel::Preview)
        );
    }

    #[test]
    fn parses_exact_version() {
        assert_eq!(
            parse_request("2.8.5").unwrap(),
            ComposerRequest::Exact("2.8.5".into())
        );
    }

    #[test]
    fn parses_partial_versions() {
        assert_eq!(
            parse_request("2").unwrap(),
            ComposerRequest::Partial("2".into())
        );
        assert_eq!(
            parse_request("2.8").unwrap(),
            ComposerRequest::Partial("2.8".into())
        );
    }

    #[test]
    fn parses_paths() {
        assert_eq!(
            parse_request("/opt/composer.phar").unwrap(),
            ComposerRequest::Path("/opt/composer.phar".into())
        );
        assert_eq!(
            parse_request("~/bin/composer.phar").unwrap(),
            ComposerRequest::Path("~/bin/composer.phar".into())
        );
    }

    #[test]
    fn whitespace_is_trimmed() {
        assert_eq!(
            parse_request("  2.8.5\n").unwrap(),
            ComposerRequest::Exact("2.8.5".into())
        );
    }

    #[test]
    fn rejects_garbage() {
        assert!(parse_request("").is_err());
        assert!(parse_request("dev-master").is_err());
        assert!(parse_request("2.8.5-rc1").is_err());
        assert!(parse_request("2.").is_err());
        assert!(parse_request("2.8.5.6").is_err());
    }
}
