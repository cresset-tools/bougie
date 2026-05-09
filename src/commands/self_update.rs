use crate::errors::BougieError;
use eyre::Result;
use std::process::ExitCode;

/// `bougie self update` ships once a GitHub Releases pipeline exists
/// for bougie. Until then it errors with a clear stub.
pub fn run() -> Result<ExitCode> {
    Err(BougieError::SelfUpdate(
        "self-update is not yet available in this build; install via your package manager"
            .into(),
    )
    .into())
}
