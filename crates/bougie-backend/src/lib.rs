//! Pluggable PHP-distribution backends.
//!
//! A `Backend` resolves a user-facing request (`bougie php install 8.4`)
//! into a [`PhpRecipe`] — a self-contained description of one blob to
//! download and how to extract it. Today there's exactly one
//! implementation, [`bougie_index::BougieIndexBackend`], which talks to
//! `index.bougie.tools` (or a configured mirror) using the signed
//! root → section → manifest protocol. Phase 3 of `WINDOWS_PLAN.md` adds
//! `WindowsPhpNetBackend`, which fetches `releases.json` from
//! windows.php.net and synthesizes a `PhpRecipe` for that distribution.
//!
//! The trait deliberately keeps the surface narrow: resolution is
//! pure-ish (network I/O, no filesystem mutation). The recipe carries
//! everything `install_php` needs to drive [`bougie_fetch::fetch_blob`],
//! so the install code is identical across backends — the only
//! per-backend choice is which `Backend` impl to construct.
//!
//! The trait grows symmetrically for extensions: `resolve_extension`
//! returns an [`ExtRecipe`] that carries everything `install_extension`
//! needs to drive the same fetch + place + conf.d dance regardless of
//! backend. Closure peers (bougie-index only) ride on the recipe as a
//! [`ClosureRef`] list — windows.php.net always returns an empty one,
//! and the install code's closure-handling step is a no-op for it.

pub mod bougie_index_backend;
pub mod windows_php_net;

pub use bougie_index_backend::BougieIndexBackend;
pub use windows_php_net::WindowsPhpNetBackend;

use bougie_fetch::{fetch_blob, ArchiveKind, BlobOutcome, DownloadBar};
use bougie_index::wire::LoadDirective;
use bougie_paths::Paths;
use bougie_version::request::{Flavor, VersionLike};
use bougie_resolver::ResolveOptions;
use bougie_platform::target::{Os, Triple};
use bougie_version::version::{PartialVersion, Version};
use eyre::Result;
use std::path::{Path, PathBuf};

/// A source of PHP interpreter artifacts. One concrete impl per
/// distribution channel — bougie's own signed index, windows.php.net.
pub trait Backend {
    /// Resolve a user-facing request into a [`PhpRecipe`] ready to
    /// hand off to the extract pipeline. Network I/O happens here
    /// (root + section + manifest fetches for `BougieIndexBackend`;
    /// `releases.json` fetch for `WindowsPhpNetBackend`); no filesystem
    /// state under `$BOUGIE_HOME` is mutated.
    fn resolve_php(
        &self,
        spec: &VersionLike,
        flavor: Flavor,
        opts: ResolveOptions,
    ) -> Result<PhpRecipe>;

    /// Resolve a user-facing extension request into an [`ExtRecipe`].
    /// Same contract as [`resolve_php`]: network I/O lives here (index
    /// section + manifest fetch for `BougieIndexBackend`; static table
    /// lookup for `WindowsPhpNetBackend`), no filesystem mutation under
    /// `$BOUGIE_HOME`. `version_pin` and `opts` are bougie-index
    /// concepts; the windows.php.net backend ignores them (the
    /// compile-time `WINDOWS_PECL_VERSIONS` table is the version oracle
    /// — see `WINDOWS_PLAN.md` §Phase 4).
    ///
    /// [`resolve_php`]: Self::resolve_php
    fn resolve_extension(
        &self,
        name: &str,
        php_minor: PartialVersion,
        flavor: Flavor,
        version_pin: Option<&str>,
        opts: ResolveOptions,
    ) -> Result<ExtRecipe>;

    /// Borrow the backend's HTTP client. Exposed so [`fetch_into`]'s
    /// default impl (and the test harness) can drive
    /// [`bougie_fetch::fetch_blob`] without re-building a client.
    ///
    /// [`fetch_into`]: Self::fetch_into
    fn client(&self) -> &reqwest::blocking::Client;

    /// Fetch the recipe's blob into `install_root` and extract.
    ///
    /// The default impl extracts directly into `install_root` — right
    /// for backends whose blobs already wrap their contents into a
    /// bougie-shaped tree (`install/bin/...`, stripped via the
    /// recipe's `strip_prefix`). Backends whose blobs ship a different
    /// shape override this to relocate the extracted tree (the
    /// windows.php.net backend extracts into `install_root/bin/` so
    /// `php.exe` and its colocated DLLs land where the rest of bougie
    /// expects to find them).
    fn fetch_into(
        &self,
        blob: &BlobRef,
        install_root: &Path,
        partial_dir: &Path,
        bar: &DownloadBar,
    ) -> Result<BlobOutcome> {
        let spec = blob.as_blob_spec(partial_dir, install_root);
        fetch_blob(self.client(), &spec, bar)
    }
}

/// Pick the right backend for the host target.
///
/// `windows.*` triples go through [`WindowsPhpNetBackend`] regardless
/// of `host` (`$BOUGIE_INDEX_URL` is a bougie-index concept). Everything
/// else uses [`BougieIndexBackend`] pointed at `host`. Returning a
/// boxed trait object lets `install_php` stay branch-free.
pub fn select(target: &Triple, host: &str, paths: &Paths) -> Result<Box<dyn Backend>> {
    if target.os == Os::Windows {
        Ok(Box::new(WindowsPhpNetBackend::new(paths, target)?))
    } else {
        Ok(Box::new(BougieIndexBackend::new(
            paths,
            host,
            &target.to_string(),
        )?))
    }
}

pub(crate) fn build_http_client(_label: &'static str) -> Result<reqwest::blocking::Client> {
    bougie_fetch::default_client()
}

/// One blob to fetch and extract.
///
/// Carries exactly the information [`bougie_fetch::fetch_blob`] needs:
/// URL + sha256 for verification, byte size for the progress bar,
/// archive kind so the extractor knows how to decode, and a strip
/// prefix so the unwrapping directory disappears. The fields map 1:1
/// onto [`bougie_fetch::BlobSpec`]; see [`extract`] for the wiring.
#[derive(Debug, Clone)]
pub struct BlobRef {
    pub url: String,
    pub sha256: String,
    pub size: u64,
    pub archive: ArchiveKind,
    /// Leading path component the extractor strips from every entry.
    /// `"install"` for bougie's own tar.zst interpreter tarballs;
    /// `"php-<ver>"` for windows.php.net's interpreter ZIPs;
    /// `""` for flat archives. See [`bougie_fetch::BlobSpec::strip_prefix`].
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
    /// Build a [`bougie_fetch::BlobSpec`] borrowing from this recipe.
    /// The caller supplies `partial_dir` (where in-flight downloads
    /// stage) and `dest` (the final install path) — those are
    /// filesystem concerns the backend doesn't know about.
    pub fn as_blob_spec<'a>(
        &'a self,
        partial_dir: &'a std::path::Path,
        dest: &'a std::path::Path,
    ) -> bougie_fetch::BlobSpec<'a> {
        bougie_fetch::BlobSpec {
            url: &self.url,
            hash: bougie_fetch::Hash::sha256(&self.sha256),
            partial_dir,
            dest,
            strip_prefix: &self.strip_prefix,
            archive: self.archive,
            auth_header: None,
            auth_header_name: None,
        }
    }
}

/// A resolved PHP extension, ready to fetch + place into the
/// content-addressed store.
///
/// `name`/`version`/`php_minor`/`flavor` round-trip from the request so
/// `install_extension` doesn't have to thread them separately when
/// computing the store directory. `blob` is the single archive to
/// fetch; on bougie-index that's the per-extension `.so` tarball, on
/// windows.php.net the flat PECL `.zip`. `artifact_rel` is the path of
/// the loadable file relative to the extracted store dir (`lib/extensions/<api>/<name>.so`
/// for bougie-index manifests, `php_<name>.dll` for PECL zips); the
/// conf.d emitter joins it onto the store dir to get the absolute path
/// it writes into the `extension=` / `zend_extension=` directive.
///
/// `closure` carries the bundled-library closure for bougie-index
/// artifacts — see [`ClosureRef`]. windows.php.net always returns an
/// empty vec; dependent DLLs ride inside the same PECL zip and are
/// handled via `needs_store_on_path` (see [`super::WindowsPhpNetBackend`]).
///
/// `frozen_warning` propagates the bougie-index frozen flag; always
/// false for non-index backends.
#[derive(Debug, Clone)]
pub struct ExtRecipe {
    pub name: String,
    pub version: Version,
    pub php_minor: PartialVersion,
    pub flavor: Flavor,
    pub blob: BlobRef,
    pub artifact_rel: PathBuf,
    pub load: LoadDirective,
    pub closure: Vec<ClosureRef>,
    pub needs_store_on_path: bool,
    pub frozen_warning: bool,
}

/// One bundled-library entry an extension `.so` depends on at runtime.
///
/// Mirrors [`bougie_index::wire::Closure`] field-for-field, but lives
/// here so the [`Backend`] trait can stay agnostic of the index wire
/// schema (the windows.php.net backend depends on none of it). The
/// install code uses these to materialize the install-shaped
/// `store/<name>-<version>-<hash>/` peer layout the `.so`'s RPATH was
/// compiled against — see [`crate::install::install_closure_peers`].
#[derive(Debug, Clone)]
pub struct ClosureRef {
    pub name: String,
    pub version: String,
    pub hash: String,
    pub sha256: String,
    pub url: String,
    pub size: u64,
}

impl From<&bougie_index::wire::Closure> for ClosureRef {
    fn from(c: &bougie_index::wire::Closure) -> Self {
        Self {
            name: c.name.clone(),
            version: c.version.clone(),
            hash: c.hash.clone(),
            sha256: c.sha256.clone(),
            url: c.url.clone(),
            size: c.size,
        }
    }
}
