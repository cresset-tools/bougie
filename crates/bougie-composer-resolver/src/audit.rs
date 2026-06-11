//! Packagist security-advisories client for `bougie composer audit`.
//!
//! Posts the project's installed package names to the advisories API
//! (`POST <base>/api/security-advisories/` with repeated `packages[]`
//! form fields) and returns the raw advisories per package. Matching
//! advisories against locked versions (via `bougie-semver`) is the
//! caller's job — this module is purely the network + parse layer.
//!
//! Reuses the resolver's existing blocking HTTP client
//! ([`crate::metadata::build_client`]); no new transport stack. The base
//! URL defaults to public Packagist and is overridable for mirrors /
//! the air-gapped appliance / tests via `BOUGIE_AUDIT_BASE_URL`.

use bougie_errors::{error_chain, BougieError};
use eyre::{Result, WrapErr};
use serde::Deserialize;
use std::collections::BTreeMap;

const DEFAULT_BASE_URL: &str = "https://packagist.org";

/// Base URL for the advisories API. `BOUGIE_AUDIT_BASE_URL` overrides
/// the default (public Packagist) for private mirrors, the on-prem
/// appliance, or tests pointing at a wiremock server.
pub fn base_url() -> String {
    std::env::var("BOUGIE_AUDIT_BASE_URL").unwrap_or_else(|_| DEFAULT_BASE_URL.into())
}

/// One security advisory as served by the Packagist API. Only the
/// fields bougie surfaces are typed; the rest are ignored.
#[derive(Debug, Clone, Deserialize)]
pub struct Advisory {
    #[serde(rename = "advisoryId", default)]
    pub advisory_id: String,
    #[serde(rename = "packageName", default)]
    pub package_name: String,
    /// Composer constraint describing the vulnerable version range
    /// (e.g. `>=1.8.0,<1.12.0`). Matched against the locked version.
    #[serde(rename = "affectedVersions", default)]
    pub affected_versions: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub cve: Option<String>,
    #[serde(default)]
    pub link: Option<String>,
    #[serde(default)]
    pub severity: Option<String>,
}

#[derive(Debug, Deserialize)]
struct AdvisoriesResponse {
    #[serde(default)]
    advisories: BTreeMap<String, Vec<Advisory>>,
}

/// Fetch advisories for `package_names` from `base_url`. Returns a map
/// from package name to its advisories (only packages with at least one
/// advisory appear). An empty input returns an empty map without a
/// request.
pub fn fetch_advisories(
    client: &reqwest::blocking::Client,
    base_url: &str,
    package_names: &[String],
) -> Result<BTreeMap<String, Vec<Advisory>>> {
    if package_names.is_empty() {
        return Ok(BTreeMap::new());
    }
    let url = format!("{}/api/security-advisories/", base_url.trim_end_matches('/'));
    // `application/x-www-form-urlencoded` body: `packages[]=<name>` per
    // package. Built by hand because the resolver's reqwest is compiled
    // without the form-encoding feature; composer package names only
    // contain `[a-z0-9._/-]`, so `/` is the only char needing escaping.
    let body = package_names
        .iter()
        .map(|n| format!("packages%5B%5D={}", form_encode(n)))
        .collect::<Vec<_>>()
        .join("&");

    let resp = client
        .post(&url)
        .header(
            reqwest::header::CONTENT_TYPE,
            "application/x-www-form-urlencoded",
        )
        .body(body)
        .send()
        .map_err(|e| BougieError::Network {
            operation: format!("POST {url}"),
            detail: error_chain(&e),
        })?;
    if !resp.status().is_success() {
        return Err(BougieError::Network {
            operation: format!("POST {url}"),
            detail: format!("server returned HTTP {}", resp.status()),
        }
        .into());
    }
    let bytes = resp.bytes().map_err(|e| BougieError::Network {
        operation: format!("reading body of {url}"),
        detail: error_chain(&e),
    })?;
    let parsed: AdvisoriesResponse =
        serde_json::from_slice(&bytes).wrap_err("parsing security-advisories response")?;
    // Drop packages whose advisory list came back empty.
    Ok(parsed
        .advisories
        .into_iter()
        .filter(|(_, v)| !v.is_empty())
        .collect())
}

/// Percent-encode a composer package name for a form body. Composer
/// names are `[a-z0-9._/-]`; every char in that set is form-safe except
/// `/`, which becomes `%2F`. Anything unexpected is encoded too, so the
/// function is safe for arbitrary input.
fn form_encode(s: &str) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-') {
            out.push(b as char);
        } else {
            let _ = write!(out, "%{b:02X}");
        }
    }
    out
}
