//! Lock-verify: read-only pubgrub-based check that a `composer.lock`
//! is a valid solution for its `composer.json`.
//!
//! See `RESOLVER_PLAN.md` Phase B. The provider has one candidate
//! per package (the version named in the lock) and a singleton
//! `Root` package whose dependencies are the root manifest's
//! require / require-dev. Pubgrub's solver either confirms the lock
//! is internally consistent or returns a derivation tree pointing at
//! the offending clause.

mod provider;
mod range;

#[cfg(test)]
mod tests;

pub use provider::{
    is_platform, BuildError, LockVerifyProvider, ProviderError, PubGrubPackage,
};
pub use range::{to_range, ComposerRange};

use std::path::Path;

use bougie_composer::lockfile::{self, Lock};
use eyre::{eyre, Context, Result};
use pubgrub::{resolve, DefaultStringReporter, PubGrubError, Reporter};

/// What the verifier produced. Either a clean pass or a derivation
/// tree (rendered to a string) explaining the inconsistency.
#[derive(Debug)]
pub enum VerifyOutcome {
    Valid,
    Invalid { reason: String },
}

/// Options for [`verify_lock`].
#[derive(Debug, Clone, Copy, Default)]
pub struct VerifyOptions {
    /// Skip dev-only packages and dev-only root requires.
    pub no_dev: bool,
}

/// Read `composer.json` + `composer.lock` from `project_root`,
/// content-hash-verify them, build a [`LockVerifyProvider`], and run
/// pubgrub. The result is `VerifyOutcome::Valid` or
/// `VerifyOutcome::Invalid` with a derivation tree rendered by
/// `DefaultStringReporter`.
///
/// Returns `Err` only for I/O / parse-level failures (missing file,
/// malformed JSON). Lockfile-inconsistency is `Ok(Invalid)`.
#[tracing::instrument(skip_all, fields(project_root = %project_root.display(), no_dev = opts.no_dev))]
pub fn verify_lock(project_root: &Path, opts: VerifyOptions) -> Result<VerifyOutcome> {
    let composer_json_path = project_root.join("composer.json");
    let composer_lock_path = project_root.join("composer.lock");

    if !composer_json_path.is_file() {
        return Err(eyre!(
            "{} not found — not a Composer project",
            composer_json_path.display(),
        ));
    }
    if !composer_lock_path.is_file() {
        return Ok(VerifyOutcome::Invalid {
            reason: format!(
                "{} not found — run `bougie run -- composer update` to generate it",
                composer_lock_path.display(),
            ),
        });
    }

    let composer_json_bytes = std::fs::read(&composer_json_path)
        .wrap_err_with(|| format!("reading {}", composer_json_path.display()))?;
    let composer_json: serde_json::Value = serde_json::from_slice(&composer_json_bytes)
        .map_err(|e| eyre!("parsing composer.json: {e}"))?;
    let lock = Lock::read(&composer_lock_path)?;

    // Content-hash check first — a mismatch is its own kind of
    // invalid, and gets a clearer message than the pubgrub failure
    // would produce.
    if let Some(expected) = &lock.content_hash {
        let actual = lockfile::content_hash(&composer_json_bytes)?;
        if !actual.eq_ignore_ascii_case(expected) {
            return Ok(VerifyOutcome::Invalid {
                reason: format!(
                    "composer.lock is out of sync with composer.json (content-hash {expected} → {actual}). \
                     Run `bougie run -- composer update` to regenerate.",
                ),
            });
        }
    }

    // Validate platform requires (currently `php`) against the
    // project's pinned runtime (#118). Best-effort: an un-synced
    // project (no resolved pin) models nothing and platform requires
    // stay unchecked, as before.
    let platform = crate::platform::PlatformEnv::detect(project_root, &composer_json);
    let provider = LockVerifyProvider::build(&lock, &composer_json, opts.no_dev, &platform)
        .map_err(|e| eyre!(e))?;
    let root_version = provider.root_version();

    match resolve(&provider, PubGrubPackage::Root, root_version) {
        Ok(_solution) => Ok(VerifyOutcome::Valid),
        Err(PubGrubError::NoSolution(tree)) => Ok(VerifyOutcome::Invalid {
            reason: DefaultStringReporter::report(&tree),
        }),
        Err(other) => Err(eyre!("solver error: {other}")),
    }
}
