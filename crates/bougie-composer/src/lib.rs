//! Composer (the PHP package manager) management.
//!
//! Bougie owns Composer alongside the PHP interpreter: phars live under
//! `$BOUGIE_HOME/composer/<version>/composer.phar`, sourced from
//! getcomposer.org and verified twice (once against the `shasum` field
//! returned by `/versions`, once against the per-version `.sha256sum`
//! file). See CLI.md §3.7 (composer namespace) and §2.1 (paths).

pub mod fetch;
pub mod lockfile;
pub mod php_json;
pub mod request;
pub mod resolve;

use bougie_fs::lock::ExclusiveGuard;
use bougie_paths::Paths;
use eyre::Result;
use std::path::PathBuf;
use std::time::Duration;

pub use fetch::{base_url, ChannelEntry, Channels};
pub use request::{parse_request, ComposerRequest};
pub use resolve::{resolve_request, Resolved};

const LOCK_TIMEOUT: Duration = Duration::from_mins(1);

#[derive(Debug, Clone)]
pub struct Installed {
    pub version: String,
    pub phar_path: PathBuf,
    pub already_present: bool,
}

/// Install (or no-op) a Composer version into
/// `$BOUGIE_HOME/composer/<version>/composer.phar`. Idempotent.
///
/// Path-shaped requests are rejected here — `composer install` only
/// handles index-shaped requests, mirroring `php install`'s rule.
pub fn install_composer(paths: &Paths, request: &ComposerRequest) -> Result<Installed> {
    if matches!(request, ComposerRequest::Path(_)) {
        return Err(eyre::eyre!(
            "this request shape is not supported by `composer install`; \
             use a version, a partial version, or a channel name (stable / preview)"
        ));
    }

    let _guard = ExclusiveGuard::acquire(&paths.global_lock(), LOCK_TIMEOUT)?;

    let client = fetch::build_client()?;
    let channels = fetch::fetch_channels(&client, paths)?;
    let resolved: Resolved = resolve::resolve_request(&channels, request)?;

    let phar = paths.composer_phar(&resolved.version);
    let already_present = phar.exists();
    if !already_present {
        fetch::fetch_phar(&client, paths, &resolved)?;
    }

    Ok(Installed {
        version: resolved.version,
        phar_path: phar,
        already_present,
    })
}

/// Default request used when the user runs `bougie composer install`
/// without an argument: latest stable.
pub fn default_request() -> ComposerRequest {
    ComposerRequest::Channel(request::Channel::Stable)
}
