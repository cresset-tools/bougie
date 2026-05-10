//! Orchestrates a PHP interpreter installation: refresh index, resolve,
//! fetch + extract. Shared by `bougie php install` and `bougie sync`.

use crate::errors::BougieError;
use crate::fetch::{fetch_blob, BlobSpec};
use crate::index::{
    build_verifier,
    fetch::{fetch_manifest, fetch_root, fetch_section},
};
use crate::lock::ExclusiveGuard;
use crate::paths::Paths;
use crate::request::{Flavor, Request};
use crate::resolve::{resolve_php, ResolveOptions, Selected};
use crate::store::install_dir;
use crate::target::Triple;
use crate::version::Version;
use eyre::{eyre, Result};
use std::path::PathBuf;
use std::time::Duration;

pub const DEFAULT_INDEX_URL: &str = "https://index.bougie.tools";
const SECTION_NAME: &str = "interpreter/php";
const LOCK_TIMEOUT: Duration = Duration::from_mins(1);

#[derive(Debug, Clone)]
pub struct InstalledPhp {
    pub version: Version,
    pub flavor: Flavor,
    pub install_path: PathBuf,
    pub already_present: bool,
    pub frozen_warning: bool,
}

pub fn install_php(
    paths: &Paths,
    request: &Request,
    flavor_override: Option<Flavor>,
    opts: ResolveOptions,
) -> Result<InstalledPhp> {
    let target = Triple::detect()?.to_string();
    let host = std::env::var("BOUGIE_INDEX_URL").unwrap_or_else(|_| DEFAULT_INDEX_URL.into());

    let (spec, in_request_flavor) = match request {
        Request::VersionLike { spec, flavor } => (spec.clone(), *flavor),
        Request::FullTag { .. } | Request::Path(_) | Request::Name(_) => {
            return Err(eyre!(
                "this request shape is not supported by `php install`; use a version or constraint"
            ));
        }
    };
    let flavor = pick_flavor(in_request_flavor, flavor_override)?;

    // Lock the global store before mutating anything.
    let _guard = ExclusiveGuard::acquire(&paths.global_lock(), LOCK_TIMEOUT)?;

    let verifier = build_verifier()?;
    let client = reqwest::blocking::Client::builder()
        .build()
        .map_err(|e| BougieError::Network {
            operation: "building HTTP client".into(),
            detail: e.to_string(),
        })?;

    let cache_root = paths.cache_index(&host_to_dirname(&host));
    let fetched = fetch_root(&client, &host, &cache_root, verifier.as_ref())?;
    let target_entry = fetched.root.targets.get(&target).ok_or_else(|| {
        let available: Vec<String> = fetched.root.targets.keys().cloned().collect();
        BougieError::UnknownTarget {
            triple: target.clone(),
            hint: format!(
                "the index at {host} advertises: {}",
                available.join(", ")
            ),
        }
    })?;
    let section_ref =
        target_entry.sections.get(SECTION_NAME).ok_or_else(|| BougieError::Resolution {
            kind: "section".into(),
            detail: format!("the index at {host} has no `{SECTION_NAME}` section under target {target}"),
        })?;
    let section = fetch_section(
        &client,
        &host,
        &cache_root,
        &fetched.root.version,
        &target,
        SECTION_NAME,
        &section_ref.sha256,
    )?;

    let selected: Selected<'_> = resolve_php(&section, &spec, flavor, opts)?;
    let dest = install_dir(paths, selected.version, flavor);
    let already_present = dest.exists();
    if !already_present {
        let manifest = fetch_manifest(
            &client,
            &host,
            &cache_root,
            &selected.artifact.manifest.path,
            &selected.artifact.manifest.sha256,
        )?;
        let blob_spec = BlobSpec {
            url: &manifest.blob.url,
            sha256: &manifest.blob.sha256,
            partial_dir: &paths.cache_blobs(),
            dest: &dest,
        };
        fetch_blob(&client, &blob_spec)?;
    }

    Ok(InstalledPhp {
        version: selected.version,
        flavor,
        install_path: dest,
        already_present,
        frozen_warning: selected.frozen_warning,
    })
}

fn pick_flavor(in_request: Option<Flavor>, flag: Option<Flavor>) -> Result<Flavor> {
    match (in_request, flag) {
        (Some(a), Some(b)) if a != b => Err(BougieError::Resolution {
            kind: "flavor".into(),
            detail: format!(
                "request encodes flavor `{a}` but --flavor=`{b}` was passed; remove one or make them agree"
            ),
        }
        .into()),
        (Some(f), _) | (None, Some(f)) => Ok(f),
        (None, None) => Ok(Flavor::Nts),
    }
}

pub fn host_to_dirname(host: &str) -> String {
    // Strip scheme + sanitize: "https://idx.example.com" → "idx.example.com".
    let h = host.trim_end_matches('/');
    let stripped = h
        .strip_prefix("https://")
        .or_else(|| h.strip_prefix("http://"))
        .unwrap_or(h);
    stripped.replace(['/', ':'], "_")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pick_flavor_resolves_consistent_pair() {
        assert_eq!(
            pick_flavor(Some(Flavor::Zts), Some(Flavor::Zts)).unwrap(),
            Flavor::Zts
        );
        assert_eq!(
            pick_flavor(Some(Flavor::Zts), None).unwrap(),
            Flavor::Zts
        );
        assert_eq!(
            pick_flavor(None, Some(Flavor::ZtsDebug)).unwrap(),
            Flavor::ZtsDebug
        );
        assert_eq!(pick_flavor(None, None).unwrap(), Flavor::Nts);
    }

    #[test]
    fn pick_flavor_rejects_conflict() {
        assert!(pick_flavor(Some(Flavor::Nts), Some(Flavor::Zts)).is_err());
    }

    #[test]
    fn host_to_dirname_strips_scheme() {
        assert_eq!(host_to_dirname("https://idx.example.com"), "idx.example.com");
        assert_eq!(host_to_dirname("http://idx:8080"), "idx_8080");
        assert_eq!(host_to_dirname("idx.example.com/"), "idx.example.com");
    }
}
