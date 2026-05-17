//! Pluggable PHP-distribution backends.
//!
//! A `Backend` resolves a user-facing request (`bougie php install 8.4`)
//! into a [`PhpRecipe`] — a self-contained description of one blob to
//! download and how to extract it. Today there's exactly one
//! implementation, [`bougie_index::BougieIndexBackend`], which talks to
//! `index.bougie.tools` (or a configured mirror) using the signed
//! root → section → manifest protocol. Phase 3 of WINDOWS_PLAN.md adds
//! `WindowsPhpNetBackend`, which fetches `releases.json` from
//! windows.php.net and synthesizes a `PhpRecipe` for that distribution.
//!
//! The trait deliberately keeps the surface narrow: resolution is
//! pure-ish (network I/O, no filesystem mutation). The recipe carries
//! everything `install_php` needs to drive [`crate::fetch::fetch_blob`],
//! so the install code is identical across backends — the only
//! per-backend choice is which `Backend` impl to construct.
//!
//! Phase 4 will extend the trait with `resolve_extension` returning an
//! `ExtRecipe`. Keeping that out of Phase 2 lets the PECL shape be
//! informed by what windows.php.net actually needs (e.g. dependent-DLL
//! handling) rather than locked in by the bougie-index code path.

pub mod bougie_index;

pub use bougie_index::BougieIndexBackend;

use crate::fetch::ArchiveKind;
use crate::request::{Flavor, VersionLike};
use crate::resolve::ResolveOptions;
use crate::version::Version;
use eyre::Result;

/// A source of PHP interpreter artifacts. One concrete impl per
/// distribution channel — bougie's own signed index today,
/// windows.php.net in Phase 3.
pub trait Backend {
    /// Resolve a user-facing request into a [`PhpRecipe`] ready to
    /// hand off to the extract pipeline. Network I/O happens here
    /// (root + section + manifest fetches for `BougieIndexBackend`);
    /// no filesystem state under `$BOUGIE_HOME` is mutated.
    fn resolve_php(
        &self,
        spec: &VersionLike,
        flavor: Flavor,
        opts: ResolveOptions,
    ) -> Result<PhpRecipe>;
}

/// One blob to fetch and extract.
///
/// Carries exactly the information [`crate::fetch::fetch_blob`] needs:
/// URL + sha256 for verification, byte size for the progress bar,
/// archive kind so the extractor knows how to decode, and a strip
/// prefix so the unwrapping directory disappears. The fields map 1:1
/// onto [`crate::fetch::BlobSpec`]; see [`extract`] for the wiring.
#[derive(Debug, Clone)]
pub struct BlobRef {
    pub url: String,
    pub sha256: String,
    pub size: u64,
    pub archive: ArchiveKind,
    /// Leading path component the extractor strips from every entry.
    /// `"install"` for bougie's own tar.zst interpreter tarballs;
    /// `"php-<ver>"` for windows.php.net's interpreter ZIPs;
    /// `""` for flat archives. See [`crate::fetch::BlobSpec::strip_prefix`].
    pub strip_prefix: String,
}

/// A resolved PHP interpreter, ready to install.
///
/// The version comes back from the backend (not the request) because
/// the request may have been a constraint like `^8.4` that the backend
/// pinned to a concrete `8.4.3`. `flavor` round-trips so the caller
/// doesn't have to thread it separately when computing
/// [`crate::store::install_dir`].
///
/// `frozen_warning` propagates from bougie-index frozen artifacts; for
/// non-index backends (windows.php.net) it's always `false`.
#[derive(Debug, Clone)]
pub struct PhpRecipe {
    pub version: Version,
    pub flavor: Flavor,
    pub blob: BlobRef,
    pub frozen_warning: bool,
}

impl BlobRef {
    /// Build a [`crate::fetch::BlobSpec`] borrowing from this recipe.
    /// The caller supplies `partial_dir` (where in-flight downloads
    /// stage) and `dest` (the final install path) — those are
    /// filesystem concerns the backend doesn't know about.
    pub fn as_blob_spec<'a>(
        &'a self,
        partial_dir: &'a std::path::Path,
        dest: &'a std::path::Path,
    ) -> crate::fetch::BlobSpec<'a> {
        crate::fetch::BlobSpec {
            url: &self.url,
            sha256: &self.sha256,
            partial_dir,
            dest,
            strip_prefix: &self.strip_prefix,
            archive: self.archive,
        }
    }
}
