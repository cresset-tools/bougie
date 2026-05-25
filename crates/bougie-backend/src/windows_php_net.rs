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

use super::{build_http_client, BlobRef, ExtRecipe, PhpRecipe};
use bougie_errors::{error_chain, BougieError};
use bougie_fetch::{fetch_blob, ArchiveKind, BlobOutcome, DownloadBar};
use bougie_index::wire::LoadDirective;
use bougie_paths::Paths;
use bougie_version::request::{Flavor, VersionLike};
use bougie_resolver::ResolveOptions;
use bougie_platform::target::{Arch, Triple};
use bougie_version::version::{PartialVersion, Version};
use eyre::{eyre, Result, WrapErr};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

const RELEASES_URL: &str = "https://windows.php.net/downloads/releases/releases.json";
const BLOB_BASE: &str = "https://windows.php.net/downloads/releases";
const PECL_BASE: &str = "https://windows.php.net/downloads/pecl/releases";
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

    fn resolve_extension(
        &self,
        name: &str,
        php_minor: PartialVersion,
        flavor: Flavor,
        _version_pin: Option<&str>,
        _opts: ResolveOptions,
    ) -> Result<ExtRecipe> {
        // windows.php.net PECL surface is version-oracled by the
        // compile-time `WINDOWS_PECL_VERSIONS` table; `version_pin` and
        // `opts` (which steer bougie-index resolution) are intentionally
        // ignored. See WINDOWS_PLAN.md §Phase 4.
        let artifact = self.resolve_pecl(name, php_minor, flavor)?;
        let dll_name = format!("php_{}.dll", artifact.name);
        Ok(ExtRecipe {
            name: artifact.name,
            version: artifact.version,
            php_minor: artifact.php_minor,
            flavor: artifact.flavor,
            blob: BlobRef {
                url: artifact.url,
                sha256: artifact.sha256,
                // windows.php.net doesn't advertise size on the PECL
                // surface (no JSON index; the HTML directory listing
                // carries it but parsing HTML for one number isn't
                // worth it). Bar stays unplanned for this artifact —
                // it ticks bytes received but can't fill, same
                // fallback as a pre-`size` bougie-index publisher.
                size: 0,
                archive: ArchiveKind::Zip,
                // PECL ZIPs are flat — `php_<name>.dll`, `<name>.ini`,
                // LICENSE, contrib/ all at the root.
                strip_prefix: String::new(),
            },
            artifact_rel: std::path::PathBuf::from(dll_name),
            load: artifact.load,
            // PECL DLL deps ride inside the same ZIP, not as separate
            // closure tarballs. The Windows DLL search path bleeds
            // through `needs_store_on_path` instead.
            closure: Vec::new(),
            needs_store_on_path: artifact.needs_store_on_path,
            frozen_warning: false,
        })
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
                // releases.json publishes size as a human string
                // ("33.31MB"); parse to bytes for the progress bar.
                // Unparseable / missing → 0, which falls back to a
                // sizeless bar that ticks bytes received but can't
                // fill — same behaviour as a pre-`size` bougie-index
                // publisher.
                size: zip
                    .size
                    .as_deref()
                    .and_then(parse_human_size)
                    .unwrap_or(0),
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
    /// Human-readable size from releases.json (`"33.31MB"`). `None` if
    /// the field is absent — older minor entries omitted it. Parsed
    /// into bytes by [`parse_human_size`] at recipe-build time.
    size: Option<String>,
}

impl MinorEntry {
    fn flavor_zip(&self, key: &str) -> Option<ZipInfo> {
        let entry = self.flavors.get(key)?;
        let zip = entry.get("zip")?;
        let path = zip.get("path")?.as_str()?.to_owned();
        let sha256 = zip.get("sha256")?.as_str()?.to_owned();
        let size = zip.get("size").and_then(|v| v.as_str()).map(str::to_owned);
        Some(ZipInfo { path, sha256, size })
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
    let anchor = match spec {
        VersionLike::Version(pv) => *pv,
        VersionLike::Constraint(c) => constraint_anchor(c).ok_or_else(|| eyre!(
            "constraint `{c:?}` doesn't reduce to a single PHP minor, which is all \
             windows.php.net can resolve against (releases.json exposes only the \
             latest patch per minor). Try specifying a minor (`8.4`) or exact patch \
             (`8.4.21`) instead, or open an issue describing the use case."
        ))?,
    };
    let minor = anchor.minor.ok_or_else(|| eyre!(
        "windows.php.net needs at least <major>.<minor> (e.g. `8.4`); got `{anchor}`. \
         Specify the minor — windows.php.net only ships the latest patch per minor."
    ))?;
    Ok(format!("{}.{minor}", anchor.major))
}

/// Extract the "anchor" major+minor a Composer-style constraint pins
/// to, if there's an unambiguous one. windows.php.net only resolves at
/// minor granularity (releases.json lists one entry per minor), so the
/// only constraints we can satisfy are those that collapse to a single
/// minor we can name:
///
/// - `^8.4` / `~8.4` / `~8.4.0` — all map to the 8.4 entry.
/// - `8.4.x` / `=8.4.21` / a bare exact version — same.
/// - `>=8.4,<8.5` (an `And` intersecting around a single minor) — same.
///
/// Unions and `^8` (whose lower bound `>=8.0.0` says 8.0, fine; the
/// caller picks the lower-bound minor on open-ended forms — same
/// trade-off as the bare `>=8.0` case).
fn constraint_anchor(c: &bougie_semver::Constraint) -> Option<PartialVersion> {
    use bougie_semver::version::CmpOp;
    match c {
        // `Any` (`*` / `x`) doesn't anchor anywhere — caller errors.
        bougie_semver::Constraint::Any => None,
        bougie_semver::Constraint::Op { op, version, .. } => match op {
            // Lower-bound, inclusive upper-bound, and exact equality
            // all pin to the named major.minor.
            CmpOp::Ge | CmpOp::Eq | CmpOp::Le => version_major_minor(version),
            // Strict `<`/`>`/`!=` have no clean anchor on their own —
            // `<9.0.0` includes every 8.x patch (and earlier).
            CmpOp::Lt | CmpOp::Gt | CmpOp::Ne => None,
        },
        // Intersection: the lower bound is the most "centered"
        // candidate. Walk children; the first that yields an anchor
        // wins. For the common Composer expansions:
        //   `^8.4` → `And([>=8.4.0.0, <9.0.0.0])`     → 8.4
        //   `~8.3` → `And([>=8.3.0.0, <9.0.0.0])`     → 8.3
        //   `~8.3.0` → `And([>=8.3.0.0, <8.4.0.0])`   → 8.3
        //   `8.4.*` → `And([>=8.4.0.0, <8.5.0.0])`    → 8.4
        bougie_semver::Constraint::And(items) => items.iter().find_map(constraint_anchor),
        // Unions span multiple minors by construction — no single
        // anchor.
        bougie_semver::Constraint::Or(_) => None,
    }
}

/// Pull `major.minor` (patch elided) from a semver-flavored version.
/// Returns `None` for branch versions and for numeric versions whose
/// first two segments aren't parseable u32s (shouldn't happen for the
/// canonical form, but we don't want to panic on malformed input).
fn version_major_minor(v: &bougie_semver::Version) -> Option<PartialVersion> {
    use bougie_semver::version::VersionKind;
    let VersionKind::Numeric { segments_raw, .. } = &v.kind else {
        return None;
    };
    let major: u32 = segments_raw.first()?.parse().ok()?;
    let minor: u32 = segments_raw.get(1)?.parse().ok()?;
    Some(PartialVersion {
        major,
        minor: Some(minor),
        patch: None,
    })
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

/// Parse the human-readable size string releases.json publishes
/// (`"33.31MB"`, `"512KB"`, `"1.2GB"`) into bytes. Returns `None` on
/// anything it can't recognize — caller treats that the same as a
/// missing field and falls back to a sizeless progress bar.
///
/// Uses base-1024 multipliers (1 MB = 1,048,576 bytes). windows.php.net's
/// `releases.json` is generated by PHP's own release tooling, which
/// reports filesystem-byte-style sizes; a 5% miscount against base-1000
/// wouldn't change the bar's behaviour but base-1024 is what most
/// developer tools mean by "MB" anyway.
fn parse_human_size(s: &str) -> Option<u64> {
    let s = s.trim();
    // Split at the first ASCII alphabetic character so "33.31MB"
    // becomes ("33.31", "MB"). A pure-numeric input ("12345") falls
    // through to bytes via the empty-suffix arm below.
    let split_at = s
        .find(|c: char| c.is_ascii_alphabetic())
        .unwrap_or(s.len());
    let (num_part, unit_part) = s.split_at(split_at);
    let num: f64 = num_part.trim().parse().ok()?;
    if !num.is_finite() || num < 0.0 {
        return None;
    }
    let multiplier: f64 = match unit_part.trim().to_ascii_uppercase().as_str() {
        "" | "B" => 1.0,
        "K" | "KB" | "KIB" => 1024.0,
        "M" | "MB" | "MIB" => 1024.0 * 1024.0,
        "G" | "GB" | "GIB" => 1024.0 * 1024.0 * 1024.0,
        _ => return None,
    };
    let bytes = (num * multiplier).round();
    // Already filtered NaN/negative above; reject anything beyond
    // 2^63 (exactly representable in f64) so the cast below is safe
    // by construction. PHP releases are tens of megabytes — this
    // branch only fires on garbage input.
    if !bytes.is_finite() || bytes < 0.0 || bytes >= 2f64.powi(63) {
        return None;
    }
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "finite, in [0, 2^63) by the checks above"
    )]
    Some(bytes as u64)
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
/// `ETag` dance with the body left as raw JSON since there's no signature
/// to verify — windows.php.net doesn't publish one, and the trust story
/// here is `TLS + sha256-from-releases.json` (see `WINDOWS_PLAN.md` §Non-goals).
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
        detail: error_chain(&e),
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
        detail: error_chain(&e),
    })?;

    fs::write(&body_path, &body)
        .wrap_err_with(|| format!("writing {}", body_path.display()))?;
    if let Some(etag) = new_etag.as_deref() {
        let _ = fs::write(&etag_path, etag);
    }

    serde_json::from_slice(&body).wrap_err("parsing fetched releases.json")
}

// ---------- PECL ----------
//
// windows.php.net mirrors a subset of PECL under
//   <PECL_BASE>/<name>/<version>/php_<name>-<version>-<php_minor>-<ts|nts>-<vc>-<arch>.zip
// Each ZIP is flat: `php_<name>.dll` + a sample `<name>.ini`, LICENSE,
// docs. Some extensions (imagick) ship extra DLL dependencies in the
// same ZIP — Phase 5 handles their placement; Phase 4b only ensures
// the main DLL lands in the store and a conf.d fragment points at it.
//
// `.sha256` sidecars are NOT consistently published on this surface
// (a survey across xdebug/redis/msgpack/apcu/mongodb found every
// sidecar URL returns 404). The trust anchor is therefore
// [`WINDOWS_PECL_VERSIONS`] below — a compile-time table of
// known-good (name, version, sha256) triplets, refreshed per bougie
// release. Verifying the downloaded ZIP against the embedded sha256
// is strictly stronger than trusting an upstream-provided sidecar
// (and dodges the "sidecar missing" question entirely).

/// One row in the compile-time table of known-good PECL artifacts on
/// windows.php.net. Refresh per bougie release. The flavor + arch are
/// hardcoded to `("nts", "x64")` today — TS / x86 entries get added
/// when there's user demand.
#[derive(Debug)]
struct WindowsPeclVersion {
    name: &'static str,
    version: &'static str,
    /// `<major>.<minor>` PHP version this artifact targets.
    php_minor: &'static str,
    flavor: &'static str,
    arch: &'static str,
    /// sha256 of the ZIP at the deterministic URL. Independent of any
    /// `.sha256` sidecar the upstream may or may not publish.
    sha256: &'static str,
}

/// `true` when this extension's store dir must be on the PATH at run
/// time so the Windows DLL loader can find its dependent DLLs. The
/// imagick distribution is the canonical case — the ZIP bundles
/// `CORE_RL_*.dll` (`MagickWand`, `MagickCore`, …) and `IM_MOD_RL_*.dll`
/// (codec modules) alongside `php_imagick.dll`. Pointing PATH at the
/// store dir is enough — `ImageMagick`'s runtime conventions find both
/// the link-time deps and the codec modules in that same directory,
/// so no IM-specific env vars (`MAGICK_CODER_MODULE_PATH`,
/// `MAGICK_CONFIGURE_PATH`) are needed. Verified empirically against
/// imagick 3.8.1 / `ImageMagick` 7.1.1-46.
fn pecl_needs_store_on_path(name: &str) -> bool {
    matches!(name, "imagick")
}

/// Hand-curated table of `(name, php_minor, flavor, arch) → (version, sha256)`.
/// Add a row when shipping support for a new extension/version combo;
/// the version is the latest stable at the time of the bougie release.
///
/// Today's coverage is xdebug 3.5.1 for the PHP minors windows.php.net
/// actively publishes (8.0–8.5), NTS x64. Other extensions
/// (redis, igbinary, msgpack, apcu, pcov, mongodb, imagick) and other
/// flavors land as the user-demand picture clarifies.
const WINDOWS_PECL_VERSIONS: &[WindowsPeclVersion] = &[
    WindowsPeclVersion {
        name: "xdebug",
        version: "3.5.1",
        php_minor: "8.0",
        flavor: "nts",
        arch: "x64",
        sha256: "6105bc3ffe76c79f3a38f27a2b7d605594a68bfec42984e9ab12c17d64bac067",
    },
    WindowsPeclVersion {
        name: "xdebug",
        version: "3.5.1",
        php_minor: "8.1",
        flavor: "nts",
        arch: "x64",
        sha256: "cf5bcf99b0f64339f14c28e120d5e52c7a608ce317d1ba4b9c06b3d755eb70fc",
    },
    WindowsPeclVersion {
        name: "xdebug",
        version: "3.5.1",
        php_minor: "8.2",
        flavor: "nts",
        arch: "x64",
        sha256: "b3c1bb3c709e1f62d5e8a8b62094995663eec428f4d10136db9d96d3f3dd63b0",
    },
    WindowsPeclVersion {
        name: "xdebug",
        version: "3.5.1",
        php_minor: "8.3",
        flavor: "nts",
        arch: "x64",
        sha256: "be2e8553d51d3b048c79022cce8002e133573ad1fa33cbeaa4e823e9013faf01",
    },
    WindowsPeclVersion {
        name: "xdebug",
        version: "3.5.1",
        php_minor: "8.4",
        flavor: "nts",
        arch: "x64",
        sha256: "967cceb6aebbc5592f6aeb61e67ce2e1bef26e985a5b07efe3a622de090a70a9",
    },
    WindowsPeclVersion {
        name: "xdebug",
        version: "3.5.1",
        php_minor: "8.5",
        flavor: "nts",
        arch: "x64",
        sha256: "1f5a5ec509971c35bf738ff21ccf1e5652a223f2101ea9e3c66e79b647e06e2a",
    },
    // imagick 3.8.1 — NTS x64 across PHP 8.0–8.5. The ZIP bundles
    // ~50 MB of ImageMagick CORE_RL_*.dll + IM_MOD_RL_*.dll codec
    // modules alongside php_imagick.dll; the store dir gets added
    // to PATH at run-time so the Windows DLL loader and ImageMagick's
    // codec loader both find them (see [`pecl_needs_store_on_path`]).
    WindowsPeclVersion {
        name: "imagick",
        version: "3.8.1",
        php_minor: "8.0",
        flavor: "nts",
        arch: "x64",
        sha256: "6d57c741e338eed606bd239e44d6ec144e54b8ff65ccf99dbb09d4b1b76b9de3",
    },
    WindowsPeclVersion {
        name: "imagick",
        version: "3.8.1",
        php_minor: "8.1",
        flavor: "nts",
        arch: "x64",
        sha256: "7297bd599f58b26ed209f9ffa373f29a2bdf6a88cacd46573ed673fb90071dba",
    },
    WindowsPeclVersion {
        name: "imagick",
        version: "3.8.1",
        php_minor: "8.2",
        flavor: "nts",
        arch: "x64",
        sha256: "c15582bfbe19abad8a7894965e82f51d6e5c167d1fa3c0876e5dc64573a4daa9",
    },
    WindowsPeclVersion {
        name: "imagick",
        version: "3.8.1",
        php_minor: "8.3",
        flavor: "nts",
        arch: "x64",
        sha256: "6954c4cd93fb2844616baab9c04a0dd83f3fde19289563f8b693f613b4cc825c",
    },
    WindowsPeclVersion {
        name: "imagick",
        version: "3.8.1",
        php_minor: "8.4",
        flavor: "nts",
        arch: "x64",
        sha256: "98bd9e5d7355aa8fbc348774613c0ee9844447a3ba5f2565a7aa08486aead541",
    },
    WindowsPeclVersion {
        name: "imagick",
        version: "3.8.1",
        php_minor: "8.5",
        flavor: "nts",
        arch: "x64",
        sha256: "67ab8675e59cbbbefd3462c91be662670592f5d02d862f5ee480d9e4707b1fc0",
    },
];

/// A resolved PECL artifact ready to fetch + extract. Internal to
/// this module — the trait's [`super::ExtRecipe`] is the public shape
/// the install code sees.
#[derive(Debug, Clone)]
struct WindowsPeclArtifact {
    name: String,
    version: Version,
    php_minor: PartialVersion,
    flavor: Flavor,
    url: String,
    sha256: String,
    /// `extension=` vs `zend_extension=`. Hardcoded per extension —
    /// the windows.php.net PECL surface doesn't carry this metadata.
    load: LoadDirective,
    /// `true` when the extension's store dir needs to be on PATH at
    /// run time so the Windows DLL loader can find its dependent DLLs.
    /// imagick is the canonical case (see [`pecl_needs_store_on_path`]).
    needs_store_on_path: bool,
}

/// PHP INI load directive for a PECL extension on Windows. The
/// classification is invariant across versions, so it lives outside
/// [`WINDOWS_PECL_VERSIONS`].
fn pecl_load_directive(name: &str) -> LoadDirective {
    match name {
        "xdebug" => LoadDirective::ZendExtension,
        _ => LoadDirective::Extension,
    }
}

impl WindowsPhpNetBackend {
    /// Resolve a PECL extension for the requested `(name, php_minor, flavor)`
    /// to a fetchable artifact. Looks up [`WINDOWS_PECL_VERSIONS`] for
    /// the known-good (version, sha256) triplet and builds the
    /// deterministic URL.
    ///
    /// Returns a structured error if bougie has no entry for the
    /// requested combo. The user-visible fix is either to ship a
    /// bougie release that adds the row, or to vendor the DLL by hand
    /// — there's no automatic discovery on this surface.
    ///
    /// Internal helper for the trait's [`super::Backend::resolve_extension`]
    /// impl; the install code reaches the backend through the trait, not
    /// this inherent method.
    fn resolve_pecl(
        &self,
        name: &str,
        php_minor: PartialVersion,
        flavor: Flavor,
    ) -> Result<WindowsPeclArtifact> {
        let arch = self.arch_suffix()?;
        let flavor_tag = flavor_tag(flavor)?;
        let minor = php_minor.minor.ok_or_else(|| {
            eyre!(
                "PECL resolution needs <major>.<minor>; got `{}`",
                php_minor
            )
        })?;
        let php_minor_str = format!("{}.{minor}", php_minor.major);
        let vc = vc_for_minor(&php_minor_str)?;

        let entry = WINDOWS_PECL_VERSIONS
            .iter()
            .find(|e| {
                e.name == name
                    && e.php_minor == php_minor_str
                    && e.flavor == flavor_tag
                    && e.arch == arch
            })
            .ok_or_else(|| BougieError::Resolution {
                kind: "extension".into(),
                detail: format!(
                    "no compile-time WINDOWS_PECL_VERSIONS entry for ext-{name} on \
                     PHP {php_minor_str} ({flavor_tag}-{arch}). windows.php.net \
                     may still publish it — open an issue so bougie's next release \
                     can bake in the (version, sha256) pair."
                ),
            })?;

        let filename = format!(
            "php_{name}-{}-{php_minor_str}-{flavor_tag}-{vc}-{arch}.zip",
            entry.version
        );
        Ok(WindowsPeclArtifact {
            name: name.to_owned(),
            version: entry.version.parse().wrap_err_with(|| {
                format!(
                    "WINDOWS_PECL_VERSIONS entry for {name} has non-semver `version = {}`",
                    entry.version
                )
            })?,
            php_minor,
            flavor,
            url: format!("{PECL_BASE}/{name}/{}/{filename}", entry.version),
            sha256: entry.sha256.to_owned(),
            load: pecl_load_directive(name),
            needs_store_on_path: pecl_needs_store_on_path(name),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bougie_version::version::PartialVersion;

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

    /// `bougie init` writes `"php": "^8.4"` to composer.json; sync
    /// passes that constraint through to the backend. Resolving it to
    /// a windows.php.net minor key has to work for the everyday "open
    /// a new project on Windows" flow to function.
    #[test]
    fn pick_minor_key_accepts_caret_tilde_and_pinned_op_constraints() {
        use bougie_semver::Constraint;
        let caret = VersionLike::Constraint(Constraint::parse("^8.4").unwrap());
        assert_eq!(pick_minor_key(&caret).unwrap(), "8.4");
        let tilde = VersionLike::Constraint(Constraint::parse("~8.3.0").unwrap());
        assert_eq!(pick_minor_key(&tilde).unwrap(), "8.3");
        let exact = VersionLike::Constraint(Constraint::parse("8.5.0").unwrap());
        assert_eq!(pick_minor_key(&exact).unwrap(), "8.5");
        let pinned_op = VersionLike::Constraint(Constraint::parse("=8.2.15").unwrap());
        assert_eq!(pick_minor_key(&pinned_op).unwrap(), "8.2");
    }

    #[test]
    fn pick_minor_key_anchors_on_lower_bound_for_open_constraints() {
        use bougie_semver::Constraint;
        // `^8` expands to `>=8.0.0.0, <9.0.0.0`. The lower bound's
        // 8.0 is taken as the anchor — backend picks 8.0's latest
        // patch even though `^8` would happily accept 8.4 too. Same
        // trade-off as the bare `>=8.0` case.
        let caret_major = VersionLike::Constraint(Constraint::parse("^8").unwrap());
        assert_eq!(pick_minor_key(&caret_major).unwrap(), "8.0");
        // Bare `>=8.0` likewise anchors at 8.0.
        let open = VersionLike::Constraint(Constraint::parse(">=8.0").unwrap());
        assert_eq!(pick_minor_key(&open).unwrap(), "8.0");
    }

    #[test]
    fn pick_minor_key_rejects_wildcard_and_unions() {
        use bougie_semver::Constraint;
        // `*` has no anchor.
        let star = VersionLike::Constraint(Constraint::parse("*").unwrap());
        assert!(pick_minor_key(&star).is_err());
        // Unions span multiple minors.
        let union = VersionLike::Constraint(Constraint::parse("^7.4 || ^8.0").unwrap());
        assert!(pick_minor_key(&union).is_err());
    }

    /// The original bug from issue #106: `8.3.*` should resolve to
    /// the 8.3 minor key on Windows just like everywhere else.
    #[test]
    fn pick_minor_key_accepts_wildcard_patch() {
        use bougie_semver::Constraint;
        let c = VersionLike::Constraint(Constraint::parse("8.3.*").unwrap());
        assert_eq!(pick_minor_key(&c).unwrap(), "8.3");
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
    /// `resolve_php` into a 404.
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
        // `size` is optional on the wire — nts-vs17-x64 in this
        // snapshot omits it, ts-vs17-x64 carries it.
        assert!(nts.size.is_none());
        let ts = minor.flavor_zip("ts-vs17-x64").unwrap();
        assert_eq!(ts.size.as_deref(), Some("33.31MB"));
        assert!(minor.flavor_zip("nts-vs17-arm64").is_none());
        // `source` is not a flavor key — flavor_keys filters it out.
        let keys: Vec<_> = minor.flavor_keys().collect();
        assert!(keys.contains(&"ts-vs17-x64"));
        assert!(keys.contains(&"nts-vs17-x64"));
        assert!(!keys.contains(&"source"));
    }

    #[test]
    fn parse_human_size_handles_real_releases_json_shapes() {
        // The shape the bar actually has to deal with: "33.31MB" for
        // every runtime ZIP in the wild today.
        assert_eq!(parse_human_size("33.31MB"), Some(34_928_067));
        // Variants we haven't seen but might land if PHP's release
        // tooling ever shifts. None of them should fall back to 0.
        assert_eq!(parse_human_size("512KB"), Some(524_288));
        assert_eq!(parse_human_size("1.2GB"), Some(1_288_490_189));
        assert_eq!(parse_human_size("100"), Some(100));
        assert_eq!(parse_human_size("100B"), Some(100));
        // Leading/trailing whitespace and case-insensitive units.
        assert_eq!(parse_human_size("  33.31 mb "), Some(34_928_067));
        assert_eq!(parse_human_size("1MiB"), Some(1_048_576));
    }

    #[test]
    fn parse_human_size_rejects_unrecognised_inputs() {
        // Garbage / unsupported units degrade to None so the bar
        // silently falls back to its sizeless mode rather than the
        // install failing on a numeric blip.
        assert!(parse_human_size("").is_none());
        assert!(parse_human_size("MB").is_none());
        assert!(parse_human_size("abc").is_none());
        assert!(parse_human_size("-5MB").is_none());
        assert!(parse_human_size("NaN").is_none());
        // PB and TB aren't on the windows.php.net surface — refuse
        // rather than silently underestimate.
        assert!(parse_human_size("2TB").is_none());
    }

    // ---------- PECL ----------

    #[test]
    fn pecl_load_directive_classifies_known_extensions() {
        assert_eq!(pecl_load_directive("xdebug"), LoadDirective::ZendExtension);
        // Unknown / regular extensions default to plain `extension=`.
        assert_eq!(pecl_load_directive("redis"), LoadDirective::Extension);
        assert_eq!(pecl_load_directive("apcu"), LoadDirective::Extension);
        assert_eq!(pecl_load_directive("mongodb"), LoadDirective::Extension);
        assert_eq!(pecl_load_directive("imagick"), LoadDirective::Extension);
    }

    #[test]
    fn pecl_needs_store_on_path_only_imagick() {
        // imagick bundles ~170 CORE_RL_*.dll + IM_MOD_*.dll codec
        // modules in its ZIP; pointing PATH at the store dir lets the
        // Windows DLL loader and ImageMagick's codec loader find them.
        assert!(pecl_needs_store_on_path("imagick"));
        // Every other PECL extension we ship is single-DLL — no PATH
        // augmentation needed.
        assert!(!pecl_needs_store_on_path("xdebug"));
        assert!(!pecl_needs_store_on_path("redis"));
        assert!(!pecl_needs_store_on_path("apcu"));
    }

    /// Smoke-check the compile-time PECL table: every entry parses,
    /// uses the expected flavor/arch surface, and its sha256 looks like
    /// a real 64-hex digest. Guards against a typo-during-refresh.
    #[test]
    fn windows_pecl_versions_table_is_well_formed() {
        for e in WINDOWS_PECL_VERSIONS {
            // Flavor + arch are constrained: NTS x64 only today.
            // Loosen these asserts when the table grows.
            assert_eq!(e.flavor, "nts", "{}: TS not yet supported in table", e.name);
            assert_eq!(e.arch, "x64", "{}: x86 not yet supported in table", e.name);
            // sha256 should be 64 lowercase hex chars.
            assert_eq!(e.sha256.len(), 64, "{}: bad sha256 length", e.name);
            assert!(
                e.sha256.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()),
                "{}: sha256 must be lowercase hex",
                e.name
            );
            // Version must parse as a full semver (PartialVersion would
            // accept partials; Version requires patch).
            assert!(
                e.version.parse::<Version>().is_ok(),
                "{}: version `{}` must be full semver",
                e.name,
                e.version,
            );
            // PHP minor must parse as `<major>.<minor>`.
            let (maj, min) = parse_minor(e.php_minor).unwrap_or_else(|err| {
                panic!("{}: php_minor `{}` malformed: {err}", e.name, e.php_minor)
            });
            assert!(maj >= 8, "{}: php_minor major {maj} pre-bougie-support", e.name);
            let _ = min;
        }
    }

    /// xdebug 8.4 NTS x64 must round-trip from `(name, php_minor, flavor)`
    /// to a deterministic URL whose filename matches windows.php.net's
    /// real PECL naming (`php_xdebug-3.5.1-8.4-nts-vs17-x64.zip`).
    #[test]
    fn resolve_pecl_builds_xdebug_url_for_php_84_nts_x64() {
        let td = tempfile::TempDir::new().unwrap();
        let paths = bougie_paths::Paths::new(td.path().into(), td.path().join("cache"));
        let target = bougie_platform::target::Triple {
            arch: Arch::X86_64,
            vendor: bougie_platform::target::Vendor::Pc,
            os: bougie_platform::target::Os::Windows,
            env: Some(bougie_platform::target::Env::Msvc),
        };
        let backend = WindowsPhpNetBackend::new(&paths, &target).unwrap();
        let art = backend
            .resolve_pecl(
                "xdebug",
                PartialVersion { major: 8, minor: Some(4), patch: None },
                Flavor::Nts,
            )
            .unwrap();
        assert_eq!(art.name, "xdebug");
        assert_eq!(art.version, Version::new(3, 5, 1));
        assert_eq!(art.url,
            "https://windows.php.net/downloads/pecl/releases/xdebug/3.5.1/php_xdebug-3.5.1-8.4-nts-vs17-x64.zip");
        assert_eq!(art.load, LoadDirective::ZendExtension);
        assert!(!art.needs_store_on_path);
        assert_eq!(
            art.sha256,
            "967cceb6aebbc5592f6aeb61e67ce2e1bef26e985a5b07efe3a622de090a70a9"
        );
    }

    #[test]
    fn resolve_pecl_imagick_signals_needs_store_on_path() {
        let td = tempfile::TempDir::new().unwrap();
        let paths = bougie_paths::Paths::new(td.path().into(), td.path().join("cache"));
        let target = bougie_platform::target::Triple {
            arch: Arch::X86_64,
            vendor: bougie_platform::target::Vendor::Pc,
            os: bougie_platform::target::Os::Windows,
            env: Some(bougie_platform::target::Env::Msvc),
        };
        let backend = WindowsPhpNetBackend::new(&paths, &target).unwrap();
        let art = backend
            .resolve_pecl(
                "imagick",
                PartialVersion { major: 8, minor: Some(4), patch: None },
                Flavor::Nts,
            )
            .unwrap();
        assert_eq!(art.name, "imagick");
        assert_eq!(art.version, Version::new(3, 8, 1));
        assert_eq!(art.load, LoadDirective::Extension);
        assert!(art.needs_store_on_path, "imagick must set needs_store_on_path");
    }

    /// Trait surface: `Backend::resolve_extension` must wrap the
    /// inner `resolve_pecl` result into an [`ExtRecipe`] with an empty
    /// closure (windows.php.net PECL deps ride inside the ZIP) and the
    /// correct `artifact_rel` (`php_<name>.dll` at the zip root).
    #[test]
    fn resolve_extension_for_imagick_produces_recipe_with_empty_closure_and_path_extras() {
        use super::super::Backend as _;
        let td = tempfile::TempDir::new().unwrap();
        let paths = bougie_paths::Paths::new(td.path().into(), td.path().join("cache"));
        let target = bougie_platform::target::Triple {
            arch: Arch::X86_64,
            vendor: bougie_platform::target::Vendor::Pc,
            os: bougie_platform::target::Os::Windows,
            env: Some(bougie_platform::target::Env::Msvc),
        };
        let backend = WindowsPhpNetBackend::new(&paths, &target).unwrap();
        let recipe = backend
            .resolve_extension(
                "imagick",
                PartialVersion { major: 8, minor: Some(4), patch: None },
                Flavor::Nts,
                None,
                bougie_resolver::ResolveOptions::default(),
            )
            .unwrap();
        assert_eq!(recipe.name, "imagick");
        assert_eq!(recipe.version, Version::new(3, 8, 1));
        assert_eq!(recipe.artifact_rel, std::path::PathBuf::from("php_imagick.dll"));
        assert_eq!(recipe.blob.archive, ArchiveKind::Zip);
        assert_eq!(recipe.blob.strip_prefix, "");
        assert!(recipe.closure.is_empty(), "windows.php.net PECL has no closure entries");
        assert!(recipe.needs_store_on_path, "imagick must signal needs_store_on_path");
        assert!(!recipe.frozen_warning);
    }

    /// `version_pin` and `opts` are bougie-index concepts; the
    /// windows.php.net backend must ignore them rather than error.
    /// Otherwise the unified install path would have to switch on
    /// backend type to filter what it passes through, defeating the
    /// trait.
    #[test]
    fn resolve_extension_ignores_version_pin_and_opts() {
        use super::super::Backend as _;
        let td = tempfile::TempDir::new().unwrap();
        let paths = bougie_paths::Paths::new(td.path().into(), td.path().join("cache"));
        let target = bougie_platform::target::Triple {
            arch: Arch::X86_64,
            vendor: bougie_platform::target::Vendor::Pc,
            os: bougie_platform::target::Os::Windows,
            env: Some(bougie_platform::target::Env::Msvc),
        };
        let backend = WindowsPhpNetBackend::new(&paths, &target).unwrap();
        // Pass a version_pin that doesn't match the table row's
        // version — backend should still resolve to the table row
        // (windows.php.net is one-version-per-row).
        let recipe = backend
            .resolve_extension(
                "xdebug",
                PartialVersion { major: 8, minor: Some(4), patch: None },
                Flavor::Nts,
                Some("99.99.99"),
                bougie_resolver::ResolveOptions::default(),
            )
            .unwrap();
        assert_eq!(recipe.version, Version::new(3, 5, 1));
    }

    #[test]
    fn resolve_pecl_errors_when_table_has_no_entry() {
        let td = tempfile::TempDir::new().unwrap();
        let paths = bougie_paths::Paths::new(td.path().into(), td.path().join("cache"));
        let target = bougie_platform::target::Triple {
            arch: Arch::X86_64,
            vendor: bougie_platform::target::Vendor::Pc,
            os: bougie_platform::target::Os::Windows,
            env: Some(bougie_platform::target::Env::Msvc),
        };
        let backend = WindowsPhpNetBackend::new(&paths, &target).unwrap();
        // redis isn't in the bundled table yet — surface a structured error.
        let err = backend
            .resolve_pecl(
                "redis",
                PartialVersion { major: 8, minor: Some(4), patch: None },
                Flavor::Nts,
            )
            .unwrap_err();
        assert!(err.to_string().contains("WINDOWS_PECL_VERSIONS"), "got: {err}");
    }
}
