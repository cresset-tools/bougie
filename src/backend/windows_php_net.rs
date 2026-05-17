//! `WindowsPhpNetBackend` — interpreter source for Windows hosts.
//!
//! Talks to <https://windows.php.net/downloads/releases/releases.json>,
//! which lists one entry per supported PHP minor:
//!
//! ```json
//! {
//!   "8.4": {
//!     "version": "8.4.21",
//!     "ts-vs17-x64":  { "zip": { "path": "php-8.4.21-Win32-vs17-x64.zip",
//!                                 "sha256": "..." }, ... },
//!     "nts-vs17-x64": { "zip": { ... }, ... },
//!     "ts-vs17-x86":  { ... },
//!     "nts-vs17-x86": { ... },
//!     "source":       { ... },
//!     "test_pack":    { ... }
//!   },
//!   "8.3": { ...same shape, vs16 keys... }
//! }
//! ```
//!
//! Flavor-key format is `<ts|nts>-<vc>-<arch>`. The VC version is
//! pinned per PHP minor by the upstream build pipeline — `vs17` (VS 2022)
//! for 8.4+, `vs16` (VS 2019) for 8.0–8.3 — so bougie derives it from
//! the minor rather than asking the user. Arch is `x64` for `x86_64`;
//! `arm64` isn't currently published for any version in releases.json
//! despite earlier hints, so aarch64 hosts surface a structured error.
//!
//! The downloaded ZIP is flat (no top-level `php-<ver>/` wrapping —
//! verified at implementation time, against the plan's illustrative
//! example): `php.exe`, `*.dll`, `ext/php_*.dll`, `php.ini-development`
//! all sit at the root. The recipe pins `strip_prefix = ""` and the
//! backend overrides [`super::Backend::fetch_into`] to extract the
//! whole tree into `<install>/bin/` so `php.exe` and its colocated
//! `*.dll`s land where the shim already looks for them via
//! `install/bin/php.exe`.

use super::{build_http_client, BlobRef, PhpRecipe};
use crate::errors::BougieError;
use crate::fetch::{fetch_blob, ArchiveKind, BlobOutcome, DownloadBar};
use crate::paths::Paths;
use crate::request::{Flavor, VersionLike};
use crate::resolve::ResolveOptions;
use crate::target::{Arch, Triple};
use crate::version::Version;
use eyre::{eyre, Result, WrapErr};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

const RELEASES_URL: &str = "https://windows.php.net/downloads/releases/releases.json";
const BLOB_BASE: &str = "https://windows.php.net/downloads/releases";
const CACHE_HOST_DIR: &str = "windows.php.net";

#[derive(Debug)]
pub struct WindowsPhpNetBackend {
    client: reqwest::blocking::Client,
    cache_root: PathBuf,
    arch: Arch,
}

impl WindowsPhpNetBackend {
    pub fn new(paths: &Paths, target: &Triple) -> Result<Self> {
        let client = build_http_client("windows.php.net")?;
        let cache_root = paths.cache_index(CACHE_HOST_DIR);
        Ok(Self {
            client,
            cache_root,
            arch: target.arch,
        })
    }

    /// Translate the host arch into the suffix windows.php.net uses in
    /// its flavor keys. `aarch64` isn't currently published for any
    /// minor in releases.json — surface the gap as a structured error.
    fn arch_suffix(&self) -> Result<&'static str> {
        match self.arch {
            Arch::X86_64 => Ok("x64"),
            Arch::Aarch64 => Err(BougieError::UnknownTarget {
                triple: "aarch64-pc-windows-msvc".into(),
                hint: "windows.php.net does not currently publish ARM64 builds for any PHP minor; \
                       run bougie under x86_64 (Windows-on-ARM has working x64 emulation)"
                    .into(),
            }
            .into()),
        }
    }
}

impl super::Backend for WindowsPhpNetBackend {
    fn client(&self) -> &reqwest::blocking::Client {
        &self.client
    }

    /// windows.php.net ZIPs are flat — every entry lives at the zip
    /// root with no `php-<ver>/` wrapper, so a strip-prefix extract
    /// would scatter `php.exe` and its DLL deps into `<install>`'s top
    /// level. Override `fetch_into` to extract into `<install>/bin/`
    /// instead, matching the layout the bougie shim (`install/bin/php.exe`)
    /// and the Phase 6 `PHP_INI_SCAN_DIR` wiring already expect.
    fn fetch_into(
        &self,
        blob: &BlobRef,
        install_root: &Path,
        partial_dir: &Path,
        bar: &DownloadBar,
    ) -> Result<BlobOutcome> {
        let bin_dir = install_root.join("bin");
        let spec = blob.as_blob_spec(partial_dir, &bin_dir);
        fetch_blob(&self.client, &spec, bar)
    }

    fn resolve_php(
        &self,
        spec: &VersionLike,
        flavor: Flavor,
        _opts: ResolveOptions,
    ) -> Result<PhpRecipe> {
        let minor_key = pick_minor_key(spec)?;
        let arch = self.arch_suffix()?;
        let vc = vc_for_minor(&minor_key)?;
        let flavor_tag = flavor_tag(flavor)?;
        let entry_key = format!("{flavor_tag}-{vc}-{arch}");

        let releases = fetch_releases(&self.client, &self.cache_root)?;
        let minor = releases.get(&minor_key).ok_or_else(|| BougieError::Resolution {
            kind: "php interpreter".into(),
            detail: format!(
                "windows.php.net does not publish PHP `{minor_key}`; supported minors right now: {}",
                join_keys(&releases)
            ),
        })?;
        let zip = minor.flavor_zip(&entry_key).ok_or_else(|| BougieError::Resolution {
            kind: "php interpreter".into(),
            detail: format!(
                "windows.php.net's `{minor_key}` entry is missing the `{entry_key}.zip` variant (have: {})",
                minor.flavor_keys().collect::<Vec<_>>().join(", "),
            ),
        })?;

        let version: Version = minor.version.parse().wrap_err_with(|| {
            format!(
                "releases.json `{minor_key}.version = {}` is not a full semver",
                minor.version
            )
        })?;
        // Sanity-check the patch pin if the caller specified one.
        check_patch_pin(spec, version)?;

        Ok(PhpRecipe {
            version,
            flavor,
            blob: BlobRef {
                url: format!("{BLOB_BASE}/{}", zip.path),
                sha256: zip.sha256.clone(),
                // `size` in releases.json is a human string ("33.31MB"),
                // not bytes — drop it. The bar still ticks bytes
                // received but can't fill, same as a pre-`size`
                // bougie-index publisher.
                size: 0,
                archive: ArchiveKind::Zip,
                strip_prefix: String::new(),
            },
            // No upstream "frozen" signal for windows.php.net — the
            // PHP project doesn't expose an equivalent of bougie's
            // yanked/frozen flags. Always false.
            frozen_warning: false,
        })
    }
}

/// Top-level releases.json shape. Strongly-typed keys can't capture
/// the heterogeneous flavor entries, so use a string-keyed map and
/// walk it manually.
type Releases = BTreeMap<String, MinorEntry>;

#[derive(Debug, Deserialize)]
struct MinorEntry {
    version: String,
    /// Sub-entries keyed by flavor (e.g. `nts-vs17-x64`), plus the
    /// non-flavor side entries (`source`, `test_pack`). We treat
    /// everything as a `serde_json::Value` and dig out `zip.{path,sha256}`
    /// at lookup time — it's a small surface and avoids exhaustively
    /// listing every key.
    #[serde(flatten)]
    flavors: BTreeMap<String, serde_json::Value>,
}

#[derive(Debug, Clone)]
struct ZipInfo {
    path: String,
    sha256: String,
}

impl MinorEntry {
    fn flavor_zip(&self, key: &str) -> Option<ZipInfo> {
        let entry = self.flavors.get(key)?;
        let zip = entry.get("zip")?;
        let path = zip.get("path")?.as_str()?.to_owned();
        let sha256 = zip.get("sha256")?.as_str()?.to_owned();
        Some(ZipInfo { path, sha256 })
    }

    /// Flavor-shaped keys (`<ts|nts>-vs<n>-<arch>`), filtered from the
    /// `source` / `test_pack` sidecars. Used to build a helpful error
    /// when the requested combo isn't published.
    fn flavor_keys(&self) -> impl Iterator<Item = &str> {
        self.flavors.keys().filter_map(|k| {
            if k.contains("-vs") {
                Some(k.as_str())
            } else {
                None
            }
        })
    }
}

fn pick_minor_key(spec: &VersionLike) -> Result<String> {
    match spec {
        VersionLike::Version(pv) => {
            let minor = pv.minor.ok_or_else(|| eyre!(
                "windows.php.net needs at least <major>.<minor> (e.g. `8.4`); got `{pv}`. \
                 Specify the minor — windows.php.net only ships the latest patch per minor."
            ))?;
            Ok(format!("{}.{minor}", pv.major))
        }
        VersionLike::Constraint(_) => Err(eyre!(
            "version constraints aren't supported by the windows.php.net backend yet — \
             specify a minor (`8.4`) or exact patch (`8.4.21`). \
             Constraint resolution would need bougie to discover every published patch, \
             which releases.json doesn't expose (only the latest per minor)."
        )),
    }
}

fn vc_for_minor(minor_key: &str) -> Result<&'static str> {
    let (maj, min) = parse_minor(minor_key)?;
    match (maj, min) {
        // PHP 8.4+ is built against VS 2022 (vs17).
        (8, 4..) | (9.., _) => Ok("vs17"),
        // PHP 8.0–8.3 ship against VS 2019 (vs16).
        (8, 0..=3) => Ok("vs16"),
        // PHP 7.4 also ships against vs16 but bougie's interpreter
        // support starts at 8.0 anyway (CLI.md §3.5).
        _ => Err(eyre!(
            "PHP {minor_key} predates bougie's supported minors (8.0+); \
             windows.php.net may still publish it, but bougie won't"
        )),
    }
}

fn parse_minor(s: &str) -> Result<(u32, u32)> {
    let (maj, min) = s
        .split_once('.')
        .ok_or_else(|| eyre!("`{s}` is not a <major>.<minor>"))?;
    Ok((maj.parse()?, min.parse()?))
}

fn flavor_tag(flavor: Flavor) -> Result<&'static str> {
    match flavor {
        Flavor::Nts => Ok("nts"),
        Flavor::Zts => Ok("ts"),
        Flavor::NtsDebug | Flavor::ZtsDebug => Err(eyre!(
            "windows.php.net ships debug symbols as a separate `debug_pack` ZIP layered \
             on the runtime ZIP, not as a standalone `--enable-debug` build. Bougie's \
             `*-debug` flavors don't map cleanly onto that — use `nts` or `zts`, \
             or open an issue if you need debug-pack overlay support."
        )),
    }
}

fn check_patch_pin(spec: &VersionLike, available: Version) -> Result<()> {
    let VersionLike::Version(pv) = spec else {
        return Ok(());
    };
    let Some(req_patch) = pv.patch else {
        return Ok(());
    };
    if req_patch != available.patch {
        return Err(eyre!(
            "windows.php.net only ships the latest patch per minor (currently {available} for {}.{}), \
             so the pin `{pv}` isn't satisfiable. Drop the patch component or upgrade to {available}.",
            pv.major,
            pv.minor.unwrap_or(0),
        ));
    }
    Ok(())
}

fn join_keys(r: &Releases) -> String {
    r.keys().cloned().collect::<Vec<_>>().join(", ")
}

/// Fetch (or revalidate) releases.json. Mirrors `index::fetch::fetch_root`'s
/// ETag dance with the body left as raw JSON since there's no signature
/// to verify — windows.php.net doesn't publish one, and the trust story
/// here is `TLS + sha256-from-releases.json` (see WINDOWS_PLAN.md §Non-goals).
fn fetch_releases(client: &reqwest::blocking::Client, cache_root: &Path) -> Result<Releases> {
    fs::create_dir_all(cache_root)
        .wrap_err_with(|| format!("creating {}", cache_root.display()))?;
    let body_path = cache_root.join("releases.json");
    let etag_path = cache_root.join("releases.json.etag");
    let cached_etag = fs::read_to_string(&etag_path).ok();

    let mut req = client.get(RELEASES_URL);
    if let Some(etag) = cached_etag.as_deref().filter(|s| !s.is_empty()) {
        req = req.header(reqwest::header::IF_NONE_MATCH, etag.trim());
    }
    let resp = req.send().map_err(|e| BougieError::Network {
        operation: format!("fetching {RELEASES_URL}"),
        detail: e.to_string(),
    })?;

    if resp.status() == reqwest::StatusCode::NOT_MODIFIED {
        let bytes = fs::read(&body_path)
            .wrap_err_with(|| format!("reading cached {}", body_path.display()))?;
        return serde_json::from_slice(&bytes).wrap_err("parsing cached releases.json");
    }

    if !resp.status().is_success() {
        return Err(BougieError::Network {
            operation: format!("GET {RELEASES_URL}"),
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
        operation: format!("reading body of {RELEASES_URL}"),
        detail: e.to_string(),
    })?;

    fs::write(&body_path, &body)
        .wrap_err_with(|| format!("writing {}", body_path.display()))?;
    if let Some(etag) = new_etag.as_deref() {
        let _ = fs::write(&etag_path, etag);
    }

    serde_json::from_slice(&body).wrap_err("parsing fetched releases.json")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::version::PartialVersion;

    #[test]
    fn vc_pinning_follows_php_minor() {
        assert_eq!(vc_for_minor("8.4").unwrap(), "vs17");
        assert_eq!(vc_for_minor("8.5").unwrap(), "vs17");
        assert_eq!(vc_for_minor("9.0").unwrap(), "vs17");
        assert_eq!(vc_for_minor("8.0").unwrap(), "vs16");
        assert_eq!(vc_for_minor("8.3").unwrap(), "vs16");
        assert!(vc_for_minor("7.4").is_err()); // pre-bougie-support
    }

    #[test]
    fn flavor_tag_rejects_debug_combos() {
        assert_eq!(flavor_tag(Flavor::Nts).unwrap(), "nts");
        assert_eq!(flavor_tag(Flavor::Zts).unwrap(), "ts");
        assert!(flavor_tag(Flavor::NtsDebug).is_err());
        assert!(flavor_tag(Flavor::ZtsDebug).is_err());
    }

    #[test]
    fn pick_minor_key_requires_minor_component() {
        let just_major =
            VersionLike::Version(PartialVersion { major: 8, minor: None, patch: None });
        assert!(pick_minor_key(&just_major).is_err());
        let with_minor =
            VersionLike::Version(PartialVersion { major: 8, minor: Some(4), patch: None });
        assert_eq!(pick_minor_key(&with_minor).unwrap(), "8.4");
        let with_patch =
            VersionLike::Version(PartialVersion { major: 8, minor: Some(4), patch: Some(21) });
        assert_eq!(pick_minor_key(&with_patch).unwrap(), "8.4");
    }

    #[test]
    fn check_patch_pin_accepts_match_and_rejects_drift() {
        let pin_match = VersionLike::Version(PartialVersion {
            major: 8,
            minor: Some(4),
            patch: Some(21),
        });
        let pin_drift = VersionLike::Version(PartialVersion {
            major: 8,
            minor: Some(4),
            patch: Some(20),
        });
        let no_patch =
            VersionLike::Version(PartialVersion { major: 8, minor: Some(4), patch: None });
        let available = Version::new(8, 4, 21);
        assert!(check_patch_pin(&pin_match, available).is_ok());
        assert!(check_patch_pin(&pin_drift, available).is_err());
        assert!(check_patch_pin(&no_patch, available).is_ok());
    }

    /// Snapshot of the real releases.json shape (trimmed to one minor)
    /// — guards against an upstream schema change that'd silently turn
    /// resolve_php into a 404.
    #[test]
    fn parses_minor_entry_and_extracts_zip() {
        let json = r#"{
            "8.4": {
                "version": "8.4.21",
                "ts-vs17-x64": {
                    "mtime": "2026-05-06T09:37:26+00:00",
                    "zip": {
                        "path": "php-8.4.21-Win32-vs17-x64.zip",
                        "size": "33.31MB",
                        "sha256": "9e2f6e455d3f42993f09deed23ad0178b3787090c924793e50414b6a92de186a"
                    },
                    "debug_pack": {"path":"x","sha256":"y"}
                },
                "nts-vs17-x64": {
                    "zip": {
                        "path": "php-8.4.21-nts-Win32-vs17-x64.zip",
                        "sha256": "2cb57d0d3a17b1248c6a53b600719d4b051e1c374373404d5031409c0725031d"
                    }
                },
                "source": {"path":"src.zip","sha256":"abc"}
            }
        }"#;
        let r: Releases = serde_json::from_str(json).unwrap();
        let minor = r.get("8.4").expect("8.4 entry");
        assert_eq!(minor.version, "8.4.21");
        let nts = minor.flavor_zip("nts-vs17-x64").unwrap();
        assert_eq!(nts.path, "php-8.4.21-nts-Win32-vs17-x64.zip");
        assert!(nts.sha256.starts_with("2cb57d0d"));
        assert!(minor.flavor_zip("nts-vs17-arm64").is_none());
        // `source` is not a flavor key — flavor_keys filters it out.
        let keys: Vec<_> = minor.flavor_keys().collect();
        assert!(keys.contains(&"ts-vs17-x64"));
        assert!(keys.contains(&"nts-vs17-x64"));
        assert!(!keys.contains(&"source"));
    }
}
