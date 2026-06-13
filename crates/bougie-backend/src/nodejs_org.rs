//! `NodejsOrgBackend` — Node.js interpreter source for all hosts.
//!
//! PHP projects routinely need node/npm for frontend assets (Vite,
//! Laravel Mix, Magento static-content deploy). This backend provisions
//! Node from the official distribution at <https://nodejs.org/dist>,
//! which publishes a machine-readable version index and per-release
//! signed checksums:
//!
//! - `https://nodejs.org/dist/index.json` — every release, newest-first,
//!   each with a `version` (`"v20.11.0"`), an `lts` field (`false` or a
//!   codename string like `"Iron"`), and a `files` token list.
//! - `https://nodejs.org/dist/v<ver>/SHASUMS256.txt` — `<sha256>␠␠<file>`
//!   lines for that release. This is the trust anchor (TLS +
//!   sha256-from-SHASUMS); we don't verify the detached GPG signature.
//!
//! Unlike the PHP backends this is **not** a [`super::Backend`] impl:
//! that trait is PHP-shaped (flavors, extensions, index closures). Node
//! is just resolve-version → one-blob → extract, so this module is
//! standalone and only reuses [`super::BlobRef`] + the shared fetch
//! pipeline.
//!
//! ## Portability
//!
//! Official node binaries are already relocatable and statically bundle
//! V8/OpenSSL/zlib, so there's no node-build-standalone analog the way
//! there is for PHP. The one external dependency on Linux is glibc;
//! official builds (Node 18+) require glibc ≥2.28. We deliberately do
//! **not** consume `nodejs/unofficial-builds` (musl / glibc-217), so
//! musl hosts get a clear up-front error rather than a cryptic
//! exec-time `GLIBC_2.28 not found`.

use super::{BlobRef, build_http_client};
use bougie_errors::{BougieError, error_chain};
use bougie_fetch::ArchiveKind;
use bougie_paths::Paths;
use bougie_platform::target::{Arch, Env, Os, Triple};
use eyre::{Result, WrapErr, eyre};
use serde::Deserialize;
use std::path::{Path, PathBuf};

const INDEX_URL: &str = "https://nodejs.org/dist/index.json";
const DIST_BASE: &str = "https://nodejs.org/dist";
const CACHE_HOST_DIR: &str = "nodejs.org";

/// A user-facing `bougie node install <request>` spec.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NodeRequest {
    /// Highest published version (`latest`, the default).
    Latest,
    /// Highest version flagged as an LTS line (`lts`).
    Lts,
    /// Highest version in a major line (`20`).
    Major(u32),
    /// Highest patch in a minor line (`20.11`).
    MajorMinor(u32, u32),
    /// One exact release (`20.11.0`).
    Exact(NodeVersion),
}

impl std::str::FromStr for NodeRequest {
    type Err = eyre::Report;

    fn from_str(s: &str) -> Result<Self> {
        let s = s.trim();
        match s.to_ascii_lowercase().as_str() {
            "" | "latest" | "*" => return Ok(Self::Latest),
            "lts" => return Ok(Self::Lts),
            _ => {}
        }
        // Tolerate a leading `v` (`v20.11.0`).
        let body = s.strip_prefix(['v', 'V']).unwrap_or(s);
        let parts: Vec<&str> = body.split('.').collect();
        let parse = |p: &str| -> Result<u32> {
            p.parse()
                .wrap_err_with(|| format!("`{p}` in `{s}` is not a version number"))
        };
        match parts.as_slice() {
            [maj] => Ok(Self::Major(parse(maj)?)),
            [maj, min] => Ok(Self::MajorMinor(parse(maj)?, parse(min)?)),
            [maj, min, pat] => Ok(Self::Exact(NodeVersion {
                major: parse(maj)?,
                minor: parse(min)?,
                patch: parse(pat)?,
            })),
            _ => Err(eyre!(
                "`{s}` is not a Node.js version request \
                 (expected `latest`, `lts`, `20`, `20.11`, or `20.11.0`)"
            )),
        }
    }
}

/// A concrete `major.minor.patch` Node.js version. Node versions are
/// plain three-segment integers (no pre-release tags on stable
/// releases), so a dedicated tuple is simpler than pulling in the
/// Composer-flavored [`bougie_semver::Version`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct NodeVersion {
    pub major: u32,
    pub minor: u32,
    pub patch: u32,
}

impl std::fmt::Display for NodeVersion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

impl std::str::FromStr for NodeVersion {
    type Err = eyre::Report;

    /// Parse a `dist/index.json` version string (`"v20.11.0"`) or a bare
    /// `20.11.0`.
    fn from_str(s: &str) -> Result<Self> {
        let body = s.trim().strip_prefix(['v', 'V']).unwrap_or(s.trim());
        let mut it = body.split('.');
        let mut next = |what: &str| -> Result<u32> {
            it.next()
                .ok_or_else(|| eyre!("`{s}` is missing its {what} component"))?
                .parse()
                .wrap_err_with(|| format!("`{s}` has a non-numeric {what} component"))
        };
        let v = Self {
            major: next("major")?,
            minor: next("minor")?,
            patch: next("patch")?,
        };
        if it.next().is_some() {
            return Err(eyre!("`{s}` has more than three version components"));
        }
        Ok(v)
    }
}

/// A resolved Node.js release, ready to install: the concrete version
/// plus the single blob to fetch and extract. Mirrors [`super::PhpRecipe`]
/// but without the PHP-only `flavor` / `frozen_warning` fields.
#[derive(Debug, Clone)]
pub struct NodeRecipe {
    pub version: NodeVersion,
    pub blob: BlobRef,
}

#[derive(Debug)]
pub struct NodejsOrgBackend {
    client: reqwest::blocking::Client,
    cache_root: PathBuf,
    target: Triple,
}

impl NodejsOrgBackend {
    pub fn new(paths: &Paths, target: &Triple) -> Result<Self> {
        let client = build_http_client("nodejs.org")?;
        let cache_root = paths.cache_index(CACHE_HOST_DIR);
        Ok(Self {
            client,
            cache_root,
            target: target.clone(),
        })
    }

    /// Borrow the backend's HTTP client so the install command can drive
    /// [`bougie_fetch::fetch_blob`] without rebuilding one.
    pub fn client(&self) -> &reqwest::blocking::Client {
        &self.client
    }

    /// Resolve a request against the live index, then look up the
    /// checksum for the host's platform file. Network I/O happens here
    /// (index.json + SHASUMS256.txt fetch); no filesystem state under
    /// `$BOUGIE_HOME` is mutated.
    pub fn resolve(&self, req: &NodeRequest) -> Result<NodeRecipe> {
        let plat = self.platform_token()?;
        let index = fetch_index(&self.client, &self.cache_root)?;
        let version = select_version(&index, req)?;

        let filename = format!("node-v{version}-{plat}.{}", plat.ext());
        let strip_prefix = format!("node-v{version}-{plat}");
        let url = format!("{DIST_BASE}/v{version}/{filename}");

        let sha256 = fetch_shasum(&self.client, &self.cache_root, version, &filename)?;

        Ok(NodeRecipe {
            version,
            blob: BlobRef {
                url,
                sha256,
                // nodejs.org publishes no per-file size (neither in
                // index.json nor SHASUMS256.txt), so the progress bar
                // ticks bytes received but can't fill — same fallback as
                // a pre-`size` bougie-index publisher.
                size: 0,
                archive: plat.archive(),
                strip_prefix,
            },
        })
    }

    /// The `<os>-<arch>` token nodejs.org uses in its filenames for the
    /// host, e.g. `linux-x64`, `darwin-arm64`, `win-x64`. Rejects musl
    /// (official builds are glibc-only — see module docs).
    fn platform_token(&self) -> Result<PlatformToken> {
        if matches!(self.target.env, Some(Env::Musl)) {
            return Err(BougieError::UnknownTarget {
                triple: self.target.to_string(),
                hint: "official Node.js binaries are built against glibc and do not run on \
                       musl/Alpine. Install Node from your distro's package manager, or run \
                       bougie on a glibc-based image."
                    .into(),
            }
            .into());
        }
        let arch = match self.target.arch {
            Arch::X86_64 => "x64",
            Arch::Aarch64 => "arm64",
        };
        let (os, kind) = match self.target.os {
            Os::Linux => ("linux", PlatformKind::TarGz),
            Os::Darwin => ("darwin", PlatformKind::TarGz),
            Os::Windows => ("win", PlatformKind::Zip),
        };
        Ok(PlatformToken {
            token: format!("{os}-{arch}"),
            kind,
        })
    }
}

/// Resolved `<os>-<arch>` token plus how its artifact is packaged.
#[derive(Debug)]
struct PlatformToken {
    token: String,
    kind: PlatformKind,
}

#[derive(Debug, Clone, Copy)]
enum PlatformKind {
    /// `.tar.gz` — Linux and macOS.
    TarGz,
    /// `.zip` — Windows.
    Zip,
}

impl PlatformToken {
    fn ext(&self) -> &'static str {
        match self.kind {
            PlatformKind::TarGz => "tar.gz",
            PlatformKind::Zip => "zip",
        }
    }
    fn archive(&self) -> ArchiveKind {
        match self.kind {
            PlatformKind::TarGz => ArchiveKind::TarGz,
            PlatformKind::Zip => ArchiveKind::Zip,
        }
    }
}

impl std::fmt::Display for PlatformToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.token)
    }
}

/// One release row in `dist/index.json`. Only the fields bougie needs;
/// the rest (`date`, `npm`, `v8`, `security`, …) are ignored.
#[derive(Debug, Clone, Deserialize)]
struct IndexEntry {
    /// `"v20.11.0"`.
    version: String,
    /// `false` for non-LTS, or the line codename string (`"Iron"`) for
    /// LTS releases. Deserialized untyped because serde can't model a
    /// `bool | string` union directly.
    #[serde(default)]
    lts: serde_json::Value,
}

impl IndexEntry {
    fn parsed(&self) -> Option<NodeVersion> {
        self.version.parse().ok()
    }
    fn is_lts(&self) -> bool {
        self.lts.as_str().is_some()
    }
}

/// Pick the concrete version a request resolves to. The index is
/// published newest-first, but we don't rely on ordering: every arm
/// takes the `max` of the matching candidates so a re-sorted or
/// out-of-order index can't mis-resolve.
fn select_version(index: &[IndexEntry], req: &NodeRequest) -> Result<NodeVersion> {
    let pick = |filter: &dyn Fn(&IndexEntry, NodeVersion) -> bool| -> Option<NodeVersion> {
        index
            .iter()
            .filter_map(|e| e.parsed().map(|v| (e, v)))
            .filter(|(e, v)| filter(e, *v))
            .map(|(_, v)| v)
            .max()
    };
    let chosen = match req {
        NodeRequest::Latest => pick(&|_, _| true),
        NodeRequest::Lts => pick(&|e, _| e.is_lts()),
        NodeRequest::Major(maj) => pick(&|_, v| v.major == *maj),
        NodeRequest::MajorMinor(maj, min) => pick(&|_, v| v.major == *maj && v.minor == *min),
        NodeRequest::Exact(want) => pick(&|_, v| v == *want),
    };
    chosen.ok_or_else(|| {
        BougieError::Resolution {
            kind: "node interpreter".into(),
            detail: format!("nodejs.org has no release matching `{}`", describe(req)),
        }
        .into()
    })
}

fn describe(req: &NodeRequest) -> String {
    match req {
        NodeRequest::Latest => "latest".into(),
        NodeRequest::Lts => "lts".into(),
        NodeRequest::Major(m) => m.to_string(),
        NodeRequest::MajorMinor(m, n) => format!("{m}.{n}"),
        NodeRequest::Exact(v) => v.to_string(),
    }
}

/// Fetch (or revalidate) `dist/index.json`, caching the body + `ETag`.
/// Same conditional-GET dance as the windows.php.net backend; the trust
/// story is TLS + the per-release `SHASUMS256.txt` checked at fetch time,
/// so there's no signature to verify on the index itself.
fn fetch_index(client: &reqwest::blocking::Client, cache_root: &Path) -> Result<Vec<IndexEntry>> {
    std::fs::create_dir_all(cache_root)
        .wrap_err_with(|| format!("creating {}", cache_root.display()))?;
    let body_path = cache_root.join("index.json");
    let etag_path = cache_root.join("index.json.etag");
    let cached_etag = std::fs::read_to_string(&etag_path).ok();

    let mut req = client.get(INDEX_URL);
    if let Some(etag) = cached_etag.as_deref().filter(|s| !s.is_empty()) {
        req = req.header(reqwest::header::IF_NONE_MATCH, etag.trim());
    }
    let resp = req.send().map_err(|e| BougieError::Network {
        operation: format!("fetching {INDEX_URL}"),
        detail: error_chain(&e),
    })?;

    if resp.status() == reqwest::StatusCode::NOT_MODIFIED {
        let bytes = std::fs::read(&body_path)
            .wrap_err_with(|| format!("reading cached {}", body_path.display()))?;
        return serde_json::from_slice(&bytes).wrap_err("parsing cached index.json");
    }
    if !resp.status().is_success() {
        return Err(BougieError::Network {
            operation: format!("GET {INDEX_URL}"),
            detail: format!("server returned HTTP {}", resp.status()),
        }
        .into());
    }
    let new_etag = resp
        .headers()
        .get(reqwest::header::ETAG)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let body = resp.bytes().map_err(|e| BougieError::Network {
        operation: format!("reading body of {INDEX_URL}"),
        detail: error_chain(&e),
    })?;
    std::fs::write(&body_path, &body)
        .wrap_err_with(|| format!("writing {}", body_path.display()))?;
    if let Some(etag) = new_etag.as_deref() {
        let _ = std::fs::write(&etag_path, etag);
    }
    serde_json::from_slice(&body).wrap_err("parsing fetched index.json")
}

/// Fetch `v<version>/SHASUMS256.txt` and return the sha256 for `filename`.
/// Cached per-version under the index cache root (checksums for a
/// released version are immutable, so no revalidation is needed).
fn fetch_shasum(
    client: &reqwest::blocking::Client,
    cache_root: &Path,
    version: NodeVersion,
    filename: &str,
) -> Result<String> {
    let dir = cache_root.join("shasums");
    std::fs::create_dir_all(&dir).wrap_err_with(|| format!("creating {}", dir.display()))?;
    let cache_path = dir.join(format!("SHASUMS256-{version}.txt"));

    let body = if let Ok(cached) = std::fs::read_to_string(&cache_path) {
        cached
    } else {
        let url = format!("{DIST_BASE}/v{version}/SHASUMS256.txt");
        let resp = client.get(&url).send().map_err(|e| BougieError::Network {
            operation: format!("fetching {url}"),
            detail: error_chain(&e),
        })?;
        if !resp.status().is_success() {
            return Err(BougieError::Network {
                operation: format!("GET {url}"),
                detail: format!("server returned HTTP {}", resp.status()),
            }
            .into());
        }
        let text = resp.text().map_err(|e| BougieError::Network {
            operation: format!("reading body of {url}"),
            detail: error_chain(&e),
        })?;
        let _ = std::fs::write(&cache_path, &text);
        text
    };

    parse_shasum(&body, filename).ok_or_else(|| {
        BougieError::Resolution {
            kind: "node interpreter".into(),
            detail: format!(
                "nodejs.org's SHASUMS256.txt for v{version} has no entry for `{filename}` \
                 (this platform may not be published for that release)"
            ),
        }
        .into()
    })
}

/// Parse a `SHASUMS256.txt` body for `filename`'s sha256. Lines are
/// `<64-hex>␠␠<path>`; node lists bare filenames, but tolerate a leading
/// `./` just in case.
fn parse_shasum(body: &str, filename: &str) -> Option<String> {
    for line in body.lines() {
        let mut parts = line.split_whitespace();
        let sha = parts.next()?;
        let name = parts.next()?;
        let name = name.strip_prefix("./").unwrap_or(name);
        if name == filename && sha.len() == 64 && sha.chars().all(|c| c.is_ascii_hexdigit()) {
            return Some(sha.to_ascii_lowercase());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use bougie_platform::target::{Arch, Os, Triple, Vendor};

    fn linux_x64() -> Triple {
        Triple {
            arch: Arch::X86_64,
            vendor: Vendor::Unknown,
            os: Os::Linux,
            env: Some(Env::Gnu),
        }
    }

    #[test]
    fn parses_node_requests() {
        use NodeRequest::*;
        assert_eq!("latest".parse::<NodeRequest>().unwrap(), Latest);
        assert_eq!("".parse::<NodeRequest>().unwrap(), Latest);
        assert_eq!("LTS".parse::<NodeRequest>().unwrap(), Lts);
        assert_eq!("20".parse::<NodeRequest>().unwrap(), Major(20));
        assert_eq!("20.11".parse::<NodeRequest>().unwrap(), MajorMinor(20, 11));
        assert_eq!(
            "v20.11.0".parse::<NodeRequest>().unwrap(),
            Exact(NodeVersion {
                major: 20,
                minor: 11,
                patch: 0
            })
        );
        assert!("20.11.0.1".parse::<NodeRequest>().is_err());
        assert!("twenty".parse::<NodeRequest>().is_err());
    }

    fn idx(version: &str, lts: serde_json::Value) -> IndexEntry {
        IndexEntry {
            version: version.into(),
            lts,
        }
    }

    fn sample_index() -> Vec<IndexEntry> {
        use serde_json::json;
        vec![
            idx("v22.3.0", json!(false)),
            idx("v22.2.0", json!(false)),
            idx("v20.14.0", json!("Iron")),
            idx("v20.13.1", json!("Iron")),
            idx("v18.20.3", json!("Hydrogen")),
        ]
    }

    #[test]
    fn select_version_resolves_each_request_kind() {
        let i = sample_index();
        let v = |s: &str| s.parse::<NodeVersion>().unwrap();
        assert_eq!(
            select_version(&i, &NodeRequest::Latest).unwrap(),
            v("22.3.0")
        );
        // lts → highest version whose `lts` is a codename, not the
        // overall newest (which is non-LTS).
        assert_eq!(select_version(&i, &NodeRequest::Lts).unwrap(), v("20.14.0"));
        assert_eq!(
            select_version(&i, &NodeRequest::Major(20)).unwrap(),
            v("20.14.0")
        );
        assert_eq!(
            select_version(&i, &NodeRequest::MajorMinor(22, 2)).unwrap(),
            v("22.2.0")
        );
        assert_eq!(
            select_version(&i, &NodeRequest::Exact(v("18.20.3"))).unwrap(),
            v("18.20.3")
        );
    }

    #[test]
    fn select_version_errors_on_no_match() {
        let i = sample_index();
        assert!(select_version(&i, &NodeRequest::Major(19)).is_err());
    }

    #[test]
    fn select_version_takes_max_regardless_of_index_order() {
        // Out-of-order index: max must still win.
        use serde_json::json;
        let i = vec![
            idx("v20.1.0", json!("Iron")),
            idx("v20.14.0", json!("Iron")),
            idx("v20.9.0", json!("Iron")),
        ];
        assert_eq!(
            select_version(&i, &NodeRequest::Major(20)).unwrap(),
            "20.14.0".parse::<NodeVersion>().unwrap()
        );
    }

    #[test]
    fn platform_token_maps_each_os_and_arch() {
        let td = tempfile::TempDir::new().unwrap();
        let paths = Paths::new(td.path().into(), td.path().join("cache"));

        let mk = |arch, os, env| {
            let t = Triple {
                arch,
                vendor: Vendor::Unknown,
                os,
                env,
            };
            NodejsOrgBackend::new(&paths, &t).unwrap().platform_token()
        };
        let lx = mk(Arch::X86_64, Os::Linux, Some(Env::Gnu)).unwrap();
        assert_eq!(lx.token, "linux-x64");
        assert_eq!(lx.ext(), "tar.gz");
        assert!(matches!(lx.archive(), ArchiveKind::TarGz));

        let mac = mk(Arch::Aarch64, Os::Darwin, None).unwrap();
        assert_eq!(mac.token, "darwin-arm64");
        assert_eq!(mac.ext(), "tar.gz");

        let win = mk(Arch::X86_64, Os::Windows, Some(Env::Msvc)).unwrap();
        assert_eq!(win.token, "win-x64");
        assert_eq!(win.ext(), "zip");
        assert!(matches!(win.archive(), ArchiveKind::Zip));
    }

    #[test]
    fn platform_token_rejects_musl() {
        let td = tempfile::TempDir::new().unwrap();
        let paths = Paths::new(td.path().into(), td.path().join("cache"));
        let t = Triple {
            arch: Arch::X86_64,
            vendor: Vendor::Unknown,
            os: Os::Linux,
            env: Some(Env::Musl),
        };
        let err = NodejsOrgBackend::new(&paths, &t)
            .unwrap()
            .platform_token()
            .unwrap_err();
        assert!(err.to_string().contains("musl"), "got: {err}");
    }

    #[test]
    fn parse_shasum_finds_the_right_file() {
        let body = "\
aaaa1111aaaa1111aaaa1111aaaa1111aaaa1111aaaa1111aaaa1111aaaa1111  node-v20.11.0-linux-arm64.tar.gz
bbbb2222bbbb2222bbbb2222bbbb2222bbbb2222bbbb2222bbbb2222bbbb2222  node-v20.11.0-linux-x64.tar.gz
cccc3333cccc3333cccc3333cccc3333cccc3333cccc3333cccc3333cccc3333  node-v20.11.0-win-x64.zip
";
        assert_eq!(
            parse_shasum(body, "node-v20.11.0-linux-x64.tar.gz").as_deref(),
            Some("bbbb2222bbbb2222bbbb2222bbbb2222bbbb2222bbbb2222bbbb2222bbbb2222")
        );
        assert!(parse_shasum(body, "node-v20.11.0-darwin-x64.tar.gz").is_none());
    }

    #[test]
    fn node_version_round_trips() {
        let v: NodeVersion = "v20.11.0".parse().unwrap();
        assert_eq!(v.to_string(), "20.11.0");
        assert!("20.11".parse::<NodeVersion>().is_err());
        assert!("20.11.0.0".parse::<NodeVersion>().is_err());
    }

    /// The backend constructs without network access and exposes a
    /// glibc-friendly token on a stock Linux triple.
    #[test]
    fn backend_constructs_on_linux() {
        let td = tempfile::TempDir::new().unwrap();
        let paths = Paths::new(td.path().into(), td.path().join("cache"));
        let backend = NodejsOrgBackend::new(&paths, &linux_x64()).unwrap();
        assert_eq!(backend.platform_token().unwrap().token, "linux-x64");
    }
}
