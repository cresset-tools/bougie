//! Resolve a parsed `ComposerRequest` to a concrete (version, shasum,
//! download path) tuple, given the channels JSON snapshot fetched from
//! getcomposer.org.

use super::fetch::{ChannelEntry, Channels};
use super::request::{Channel, ComposerRequest};
use bougie_errors::BougieError;
use eyre::Result;

#[derive(Debug, Clone)]
pub struct Resolved {
    pub version: String,
    /// Server-relative path, e.g. `/download/2.9.7/composer.phar`.
    pub path: String,
}

pub fn resolve_request(channels: &Channels, request: &ComposerRequest) -> Result<Resolved> {
    let entry = match request {
        ComposerRequest::Channel(Channel::Stable) => channels
            .stable
            .first()
            .ok_or_else(|| resolution_error("stable", "channel is empty"))?,
        ComposerRequest::Channel(Channel::Preview) => channels
            .preview
            .first()
            .or_else(|| channels.stable.first())
            .ok_or_else(|| resolution_error("preview", "no preview or stable releases listed"))?,
        ComposerRequest::Channel(Channel::Lts) => channels
            .lts
            .first()
            .ok_or_else(|| resolution_error("lts", "channel is empty"))?,
        ComposerRequest::Exact(v) => find_exact(channels, v)
            .ok_or_else(|| resolution_error("exact", &format!("no version {v} in stable+preview")))?,
        ComposerRequest::Partial(prefix) => find_highest_with_prefix(channels, prefix)
            .ok_or_else(|| {
                resolution_error("partial", &format!("no version matching prefix {prefix}.*"))
            })?,
        ComposerRequest::Path(p) => {
            return Err(BougieError::Resolution {
                kind: "composer".into(),
                detail: format!(
                    "path-shaped requests ({}) are not resolvable against the index — \
                     pass to a command that accepts paths (find / pin)",
                    p.display()
                ),
            }
            .into())
        }
    };
    Ok(Resolved {
        version: entry.version.clone(),
        path: entry.path.clone(),
    })
}

fn find_exact<'a>(channels: &'a Channels, v: &str) -> Option<&'a ChannelEntry> {
    channels
        .stable
        .iter()
        .chain(channels.preview.iter())
        .chain(channels.lts.iter())
        .find(|e| e.version == v)
}

fn find_highest_with_prefix<'a>(channels: &'a Channels, prefix: &str) -> Option<&'a ChannelEntry> {
    // Channels arrive newest-first per getcomposer.org convention; the
    // first match within the union is therefore the highest.
    channels
        .stable
        .iter()
        .chain(channels.preview.iter())
        .chain(channels.lts.iter())
        .find(|e| version_has_prefix(&e.version, prefix))
}

/// `2.8` matches `2.8.x`; `2` matches `2.y.z`. Avoids matching `2.80.0`
/// when the user wrote `2.8` by checking that the next char is `.`.
fn version_has_prefix(version: &str, prefix: &str) -> bool {
    let Some(rest) = version.strip_prefix(prefix) else {
        return false;
    };
    rest.is_empty() || rest.starts_with('.')
}

fn resolution_error(kind: &str, detail: &str) -> BougieError {
    BougieError::Resolution {
        kind: format!("composer/{kind}"),
        detail: detail.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(v: &str) -> ChannelEntry {
        ChannelEntry {
            version: v.into(),
            path: format!("/download/{v}/composer.phar"),
        }
    }

    fn fixture() -> Channels {
        Channels {
            stable: vec![entry("2.8.5"), entry("2.8.4"), entry("2.7.9"), entry("2.6.6")],
            preview: vec![entry("2.9.0-RC1")],
            lts: vec![entry("2.2.28")],
        }
    }

    #[test]
    fn lts_picks_first_lts() {
        let r = resolve_request(&fixture(), &ComposerRequest::Channel(Channel::Lts)).unwrap();
        assert_eq!(r.version, "2.2.28");
    }

    #[test]
    fn lts_errors_when_channel_empty() {
        let ch = Channels {
            stable: fixture().stable,
            ..Channels::default()
        };
        let err = resolve_request(&ch, &ComposerRequest::Channel(Channel::Lts)).unwrap_err();
        assert!(err.to_string().contains("composer/lts"));
    }

    #[test]
    fn stable_picks_first_stable() {
        let r = resolve_request(&fixture(), &ComposerRequest::Channel(Channel::Stable)).unwrap();
        assert_eq!(r.version, "2.8.5");
    }

    #[test]
    fn preview_picks_first_preview_falling_back_to_stable() {
        let r = resolve_request(&fixture(), &ComposerRequest::Channel(Channel::Preview)).unwrap();
        assert_eq!(r.version, "2.9.0-RC1");

        let only_stable = Channels {
            stable: fixture().stable,
            ..Channels::default()
        };
        let r = resolve_request(&only_stable, &ComposerRequest::Channel(Channel::Preview)).unwrap();
        assert_eq!(r.version, "2.8.5");
    }

    #[test]
    fn exact_match_works() {
        let r = resolve_request(&fixture(), &ComposerRequest::Exact("2.7.9".into())).unwrap();
        assert_eq!(r.version, "2.7.9");
    }

    #[test]
    fn exact_misses() {
        assert!(
            resolve_request(&fixture(), &ComposerRequest::Exact("2.7.0".into())).is_err()
        );
    }

    #[test]
    fn partial_picks_highest() {
        let r = resolve_request(&fixture(), &ComposerRequest::Partial("2.8".into())).unwrap();
        assert_eq!(r.version, "2.8.5");
        let r = resolve_request(&fixture(), &ComposerRequest::Partial("2".into())).unwrap();
        assert_eq!(r.version, "2.8.5");
    }

    #[test]
    fn partial_does_not_match_decimal_continuation() {
        let mut ch = fixture();
        ch.stable.insert(0, entry("2.80.0")); // shouldn't match "2.8"
        let r = resolve_request(&ch, &ComposerRequest::Partial("2.8".into())).unwrap();
        assert_eq!(r.version, "2.8.5");
    }

    #[test]
    fn path_request_errors_in_resolve() {
        let err = resolve_request(
            &fixture(),
            &ComposerRequest::Path("/opt/composer.phar".into()),
        )
        .unwrap_err();
        assert!(err.to_string().contains("path-shaped"));
    }

    #[test]
    fn empty_stable_channel_errors_with_useful_message() {
        let ch = Channels::default();
        let err = resolve_request(&ch, &ComposerRequest::Channel(Channel::Stable)).unwrap_err();
        assert!(err.to_string().contains("composer/stable"));
    }
}
