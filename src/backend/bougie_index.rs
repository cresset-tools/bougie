//! `BougieIndexBackend` — the legacy code path, now behind the
//! [`super::Backend`] trait.
//!
//! Talks to a bougie-format index (`index.bougie.tools` by default,
//! `$BOUGIE_INDEX_URL` to override) using the signed root → section →
//! manifest protocol from `DISTRIBUTION.md`. Sigstore-verifies the
//! root, then walks one level down at a time, ending with a `Manifest`
//! that gets translated into a [`super::PhpRecipe`].
//!
//! Construction is cheap: a `reqwest::blocking::Client`, a per-host
//! cache root path, and a target-triple string. Re-use the same
//! instance for back-to-back resolves to avoid re-fetching the root.

use super::{build_http_client, BlobRef, PhpRecipe};
use crate::errors::BougieError;
use crate::fetch::ArchiveKind;
use crate::index::{
    build_verifier,
    fetch::{fetch_manifest, fetch_root, fetch_section},
};
use crate::install::host_to_dirname;
use crate::paths::Paths;
use crate::request::{Flavor, VersionLike};
use crate::resolve::{resolve_php, ResolveOptions, Selected};
use eyre::Result;
use std::path::PathBuf;

const SECTION_NAME: &str = "interpreter/php";

#[derive(Debug)]
pub struct BougieIndexBackend {
    client: reqwest::blocking::Client,
    host: String,
    target: String,
    cache_root: PathBuf,
}

impl BougieIndexBackend {
    /// Build a backend pointing at `host`, caching index responses
    /// under `$BOUGIE_CACHE/index/<host>/`. The triple is captured at
    /// construction so each `resolve_*` call doesn't re-derive it.
    pub fn new(paths: &Paths, host: &str, target: &str) -> Result<Self> {
        let client = build_http_client("bougie index")?;
        let cache_root = paths.cache_index(&host_to_dirname(host));
        Ok(Self {
            client,
            host: host.to_owned(),
            target: target.to_owned(),
            cache_root,
        })
    }
}

impl super::Backend for BougieIndexBackend {
    fn client(&self) -> &reqwest::blocking::Client {
        &self.client
    }

    fn resolve_php(
        &self,
        spec: &VersionLike,
        flavor: Flavor,
        opts: ResolveOptions,
    ) -> Result<PhpRecipe> {
        let fetched = fetch_root(&self.client, &self.host, &self.cache_root, build_verifier)?;
        let target_entry = fetched.root.targets.get(&self.target).ok_or_else(|| {
            let available: Vec<String> = fetched.root.targets.keys().cloned().collect();
            BougieError::UnknownTarget {
                triple: self.target.clone(),
                hint: format!(
                    "the index at {} advertises: {}",
                    self.host,
                    available.join(", ")
                ),
            }
        })?;
        let section_ref =
            target_entry
                .sections
                .get(SECTION_NAME)
                .ok_or_else(|| BougieError::Resolution {
                    kind: "section".into(),
                    detail: format!(
                        "the index at {} has no `{SECTION_NAME}` section under target {}",
                        self.host, self.target,
                    ),
                })?;
        let section = fetch_section(
            &self.client,
            &self.host,
            &self.cache_root,
            &fetched.root.version,
            &self.target,
            SECTION_NAME,
            &section_ref.sha256,
        )?;

        let selected: Selected<'_> = resolve_php(&section, spec, flavor, opts)?;
        let manifest = fetch_manifest(
            &self.client,
            &self.host,
            &self.cache_root,
            &selected.artifact.manifest.path,
            &selected.artifact.manifest.sha256,
        )?;

        Ok(PhpRecipe {
            version: selected.version,
            flavor,
            blob: BlobRef {
                url: manifest.blob.url.clone(),
                sha256: manifest.blob.sha256.clone(),
                size: manifest.blob.size,
                archive: ArchiveKind::TarZst,
                // Interpreter tarballs wrap their contents in `install/`.
                strip_prefix: "install".to_owned(),
            },
            frozen_warning: selected.frozen_warning,
        })
    }
}
