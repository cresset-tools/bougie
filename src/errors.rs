//! Domain error types and the §8 exit-code map.

use thiserror::Error;

/// Errors with a defined exit code per CLI.md §8. Wrap an `eyre::Report`
/// around one of these when the surrounding code wants `?`-style chaining;
/// the binary's epilogue uses [`exit_code_for`] to translate back.
#[derive(Debug, Error)]
pub enum BougieError {
    #[error("network failure: {0}")]
    Network(String),

    #[error("index signature failure")]
    IndexSignature,

    #[error("manifest hash mismatch")]
    ManifestHashMismatch,

    #[error("blob hash mismatch")]
    BlobHashMismatch,

    #[error("resolution failure: {0}")]
    Resolution(String),

    #[error("unknown host target: {0}")]
    UnknownTarget(String),

    #[error("yanked artifact selected: {0}")]
    YankedSelected(String),

    #[error("concurrent operation conflict (lock held by pid {pid})")]
    LockHeld { pid: u32 },

    #[error("filesystem error: {0}")]
    Filesystem(String),

    #[error("self-update failed: {0}")]
    SelfUpdate(String),
}

impl BougieError {
    pub fn exit_code(&self) -> u8 {
        match self {
            Self::Network(_) => 10,
            Self::IndexSignature => 11,
            Self::ManifestHashMismatch => 12,
            Self::BlobHashMismatch => 13,
            Self::Resolution(_) => 20,
            Self::UnknownTarget(_) => 21,
            Self::YankedSelected(_) => 22,
            Self::LockHeld { .. } => 40,
            Self::Filesystem(_) => 50,
            Self::SelfUpdate(_) => 60,
        }
    }
}

/// Translate any `eyre::Report` to its mapped exit code, defaulting to 1
/// when the chain doesn't surface a [`BougieError`].
pub fn exit_code_for(err: &eyre::Report) -> u8 {
    err.downcast_ref::<BougieError>()
        .map_or(1, BougieError::exit_code)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_variant_has_distinct_code() {
        let codes = [
            BougieError::Network("x".into()).exit_code(),
            BougieError::IndexSignature.exit_code(),
            BougieError::ManifestHashMismatch.exit_code(),
            BougieError::BlobHashMismatch.exit_code(),
            BougieError::Resolution("x".into()).exit_code(),
            BougieError::UnknownTarget("x".into()).exit_code(),
            BougieError::YankedSelected("x".into()).exit_code(),
            BougieError::LockHeld { pid: 1 }.exit_code(),
            BougieError::Filesystem("x".into()).exit_code(),
            BougieError::SelfUpdate("x".into()).exit_code(),
        ];
        let mut sorted = codes;
        sorted.sort_unstable();
        for w in sorted.windows(2) {
            assert_ne!(w[0], w[1], "duplicate exit code {}", w[0]);
        }
    }

    #[test]
    fn exit_code_for_wrapped_bougie_error() {
        let report = eyre::Report::new(BougieError::BlobHashMismatch);
        assert_eq!(exit_code_for(&report), 13);
    }

    #[test]
    fn exit_code_for_unknown_error_defaults_to_one() {
        let report = eyre::eyre!("something else");
        assert_eq!(exit_code_for(&report), 1);
    }
}
