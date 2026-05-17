use bougie_errors::BougieError;
use eyre::Result;
use std::process::ExitCode;

/// `bougie self update` ships once a GitHub Releases pipeline exists
/// for bougie. Until then it errors with a clear stub.
pub fn run() -> Result<ExitCode> {
    Err(BougieError::SelfUpdate {
        detail:
            "not yet available in this build (no GitHub Releases pipeline yet); install via your package manager or rebuild from cresset-tools/bougie"
                .into(),
    }
    .into())
}
