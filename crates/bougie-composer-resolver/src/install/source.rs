//! Materialize git `source` packages into `vendor/` — the install-side
//! of `RESOLVER_PLAN.md` Phase D. Runs in parallel (like the dist
//! downloader) and mirrors its shape: one request per package, each
//! producing a populated `vendor_dest` that the rest of the install
//! pipeline (patches, deploy, autoload, bins) then treats identically to
//! an extracted dist.

use std::path::Path;

use bougie_paths::Paths;
use eyre::{eyre, Context, Result};
use rayon::prelude::*;

use crate::vcs;

/// One package to install from its git `source`. `urls` are the ordered
/// clone-URL candidates (`LockPackage::source_urls`), tried in turn until
/// one succeeds — a preferred mirror before the origin host.
#[derive(Debug)]
pub struct SourceRequest<'a> {
    pub package_name: &'a str,
    pub urls: &'a [String],
    /// The exact revision to check out — a full git sha in a lockfile.
    pub reference: &'a str,
    pub vendor_dest: &'a Path,
}

/// Clone+checkout every source request into its vendor destination,
/// concurrently. Returns the number of packages materialized (all of
/// them, since any failure aborts). Verifies `git` is available once up
/// front so a missing binary is a single clear error.
pub fn materialize_sources(paths: &Paths, reqs: &[SourceRequest<'_>]) -> Result<usize> {
    if reqs.is_empty() {
        return Ok(0);
    }
    vcs::ensure_git_available()?;

    reqs.par_iter().try_for_each(|r| install_one(paths, r))?;
    Ok(reqs.len())
}

/// Install one package, trying each candidate URL in order. Returns the
/// first success; if every candidate fails, the last error (with the
/// package named) is surfaced.
fn install_one(paths: &Paths, req: &SourceRequest<'_>) -> Result<()> {
    if req.urls.is_empty() {
        return Err(eyre!(
            "package `{}` has a git source but no clone URL",
            req.package_name
        ));
    }
    let mut last_err = None;
    for url in req.urls {
        match vcs::install_source(paths, url, req.reference, req.vendor_dest) {
            Ok(()) => return Ok(()),
            Err(e) => last_err = Some(e),
        }
    }
    Err(last_err.unwrap()).wrap_err_with(|| {
        format!("installing `{}` from git source", req.package_name)
    })
}
