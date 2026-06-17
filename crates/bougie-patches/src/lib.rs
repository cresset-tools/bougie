//! Native reimplementation of the `cweagans/composer-patches` plugin.
//!
//! bougie never runs the PHP plugin (per the no-plugins invariant); this crate
//! reproduces its effect: resolve the set of `(target package → [patches])`
//! from the **root** project, materialize each patch, and apply it to the
//! freshly-extracted package tree during install. Patches are applied
//! in-process with the [`flickzeug`] fuzzy-matching diff library — never by
//! shelling out to `git apply` / `patch` — so behavior is identical across
//! Linux, macOS, and Windows.
//!
//! The crate is intentionally FS/PHP-agnostic, like `bougie-installers` and
//! `bougie-scripts`: it parses, plans, and applies; the host
//! (`bougie-composer-resolver`'s install orchestrator) decides *when* to call
//! it and wires it into the install lifecycle.
//!
//! # Module map
//!
//! - [`model`] — the internal [`Patch`] model + v1/v2 dialect normalization.
//! - [`diff`] — multi-file diff splitting + `-p` path stripping (over flickzeug).
//! - [`apply`] — apply a patch to a tree with the `-p` fallback loop.
//! - [`resolve`] — build the root patch set from `composer.json` + patches-file.
//! - [`plan`] — the materialized [`PatchPlan`] + re-application fingerprints.
//! - [`pass`] — the per-package apply pass (+ `PATCHES.txt`).
//! - [`lock`] — the `patches.lock.json` applied-state store.
//! - [`target`] — the `patches/` dir header-inference router.
//! - [`report`] — per-application outcome ([`ApplyReport`]).

pub mod apply;
pub mod diff;
pub mod lock;
pub mod model;
pub mod pass;
pub mod plan;
pub mod report;
pub mod resolve;
pub mod target;

pub use apply::{ApplyOptions, apply_patch_text};
pub use model::{DepthSpec, FailureMode, Patch, PatchSource, parse_target_patches};
pub use pass::{PackageApplyResult, apply_package_patches};
pub use plan::{MaterializedPatch, PatchPlan, fingerprint};
pub use report::{ApplyReport, FileAction, FileOutcome};
pub use resolve::{resolve_patches_dir, resolve_root};
pub use target::{InferredTarget, infer_target};

use sha2::{Digest, Sha256};

/// Lowercase hex sha256 of arbitrary bytes.
///
/// Used both to verify a downloaded patch against a declared `sha256` (v2 TOFU)
/// and to build the per-package re-application fingerprint (Phase B): a patch's
/// *content* hash is the thing that flips when a local `patches/foo.patch` is
/// edited, forcing a clean re-extract + re-apply.
pub fn content_sha256(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let digest = hasher.finalize();
    let mut out = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write;
        let _ = write!(out, "{byte:02x}");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha256_known_vector() {
        // sha256("") well-known digest.
        assert_eq!(
            content_sha256(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }
}
