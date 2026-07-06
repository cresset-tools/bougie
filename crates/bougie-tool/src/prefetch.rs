//! Prefetch a tool's native binary into its launcher's cache.
//!
//! Some Composer packages (cresset/wick, cresset/magequery) are thin
//! PHP launchers around a prebuilt Rust binary: the Composer package
//! ships no binary, and the launcher downloads the matching release
//! artifact into a per-version user cache on first run. Installing
//! such a tool therefore isn't really "installed" until that first
//! run has network access.
//!
//! Those packages declare the release layout declaratively under
//! `extra.bougie.native-binary` in their `composer.json` (visible in
//! Packagist metadata and in the installed vendor copy):
//!
//! ```json
//! {
//!   "extra": {
//!     "bougie": {
//!       "native-binary": {
//!         "spec": 1,
//!         "name": "wick",
//!         "tag-prefix": "wick-v",
//!         "base-urls": [
//!           "https://releases.bougie.tools/github/wick/releases/download",
//!           "https://github.com/cresset-tools/wick/releases/download"
//!         ],
//!         "targets": [
//!           "x86_64-unknown-linux-gnu",
//!           "x86_64-unknown-linux-musl",
//!           "aarch64-apple-darwin",
//!           "x86_64-pc-windows-msvc"
//!         ],
//!         "sigstore": { "repository": "cresset-tools/wick" }
//!       }
//!     }
//!   }
//! }
//! ```
//!
//! **Spec 1 is the cargo-dist release layout** — everything else is
//! derived, so there is nothing free-form for a package to abuse:
//! release tag `{tag-prefix}{version}`, archive
//! `{name}-{target}.tar.gz` (`.zip` for windows targets) containing
//! `{name}-{target}/{name}[.exe]`, a `{archive}.sha256` sidecar next
//! to it, and a cache slot at `<user cache>/{name}/{version}/` — the
//! exact path the PHP launcher probes on startup.
//!
//! Safety model. The spec is untrusted input (any Packagist package
//! can declare one), so:
//! - bougie never *executes* anything it prefetches — it verifies the
//!   SHA-256 and places one file in the launcher's cache, which is
//!   byte-for-byte what the launcher itself would have done on first
//!   run;
//! - only the requested tool package is consulted, never transitive
//!   dependencies;
//! - every field is validated against a tight charset (single path /
//!   URL segments only — no separators, no `.`/`..`), download URLs
//!   must be https, and an existing cache file is never overwritten;
//! - when the optional `sigstore` section names a GitHub repository,
//!   the archive's `{archive}.sig` Sigstore bundle sidecar MUST verify
//!   against that repository's GitHub Actions OIDC identity, fail
//!   closed — so a compromised mirror or tampered release asset can't
//!   reach the cache. Note the identity travels in the same untrusted
//!   composer.json, so for arbitrary packages this proves provenance
//!   consistency, not trustworthiness;
//! - prefetch failures are advisory: callers warn and finish the
//!   install, and the launcher's own first-run download remains the
//!   fallback.
//!
//! The actual download runs through the [`NativeFetcher`] callback so
//! this crate stays free of HTTP / archive dependencies, mirroring
//! the other [`InstallContext`](crate::install::InstallContext)
//! callbacks.

use eyre::{bail, Result, WrapErr};
use std::path::{Path, PathBuf};

/// The one spec revision this bougie understands. Unknown revisions
/// skip the prefetch (the tool still works via its first-run
/// download), so packages can evolve the layout without breaking
/// older bougies.
pub const SUPPORTED_SPEC: u64 = 1;

/// Parsed + validated `extra.bougie.native-binary` block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativeBinarySpec {
    /// Binary stem; also the launcher's cache directory name.
    pub name: String,
    /// Release tag = `{tag_prefix}{version}` (e.g. `wick-v` → `wick-v0.2.1`).
    pub tag_prefix: String,
    /// Download bases tried in order (mirror first, GitHub fallback).
    pub base_urls: Vec<String>,
    /// Target triples with published prebuilt binaries.
    pub targets: Vec<String>,
    /// GitHub `owner/name` whose Actions OIDC identity signs each
    /// archive (Sigstore keyless; `{archive}.sig` bundle sidecars).
    /// When set, verification is REQUIRED — a missing or invalid
    /// bundle fails the prefetch, never downgrades to unverified.
    pub sigstore_repository: Option<String>,
}

/// Outcome of looking for the spec in a package's `composer.json`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SpecLookup {
    /// No `extra.bougie.native-binary` section — not a launcher package.
    Absent,
    /// Section present but its `spec` revision is newer than this
    /// bougie understands; carries the declared revision for messaging.
    UnsupportedSpec(u64),
    Found(NativeBinarySpec),
}

/// Everything the fetcher callback needs: fully-resolved URLs and
/// paths, no further derivation or trust decisions on its side.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrefetchPlan {
    pub name: String,
    pub version: String,
    pub target: String,
    /// `{name}-{target}.tar.gz` / `.zip` — also the sidecar's stem.
    pub archive_name: String,
    pub archive_urls: Vec<String>,
    pub sidecar_urls: Vec<String>,
    pub kind: ArchiveKind,
    /// Where the binary sits inside the extracted archive
    /// (`{name}-{target}/{name}[.exe]`).
    pub staged_rel: PathBuf,
    /// Final cache location — the path the PHP launcher probes:
    /// `<user cache>/{name}/{version}/{name}[.exe]`.
    pub cache_file: PathBuf,
    /// Sigstore verification requirement, when the spec declares one.
    pub signing: Option<PlanSigning>,
}

/// Fail-closed Sigstore requirement on a [`PrefetchPlan`]: the fetcher
/// must download the `{archive}.sig` bundle sidecar and verify the
/// archive bytes against it — expected identity = GitHub Actions OIDC
/// in `repository` — before extracting anything. Missing or invalid
/// bundle ⇒ the prefetch fails; there is no unverified fallback.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanSigning {
    /// GitHub `owner/name` pinned as the signing workflow identity.
    pub repository: String,
    /// URLs of the Sigstore bundle sidecar, same precedence as the
    /// archive URLs.
    pub bundle_urls: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArchiveKind {
    TarGz,
    Zip,
}

/// Decision from [`plan`]: fetch, or skip with a human-readable reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanDecision {
    Fetch(Box<PrefetchPlan>),
    Skip(String),
}

/// Download `plan.archive_urls` (first that succeeds), verify against
/// the `.sha256` sidecar — and, when `plan.signing` is set, verify the
/// archive's Sigstore bundle fail-closed — then extract and place
/// `staged_rel` at `cache_file` atomically. Must NOT mark anything
/// executable beyond the placed binary and must not touch `cache_file`
/// on failure.
pub type NativeFetcher = dyn Fn(&PrefetchPlan) -> Result<()> + Send + Sync;

/// Single path/URL segment: tight charset, never a separator, never
/// `.`/`..`. Everything user-controllable in the spec must pass this
/// before it is spliced into a URL or a filesystem path.
fn valid_segment(s: &str) -> bool {
    !s.is_empty()
        && s != "."
        && s != ".."
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

fn valid_base_url(s: &str) -> bool {
    let Some(rest) = s.strip_prefix("https://") else {
        return false;
    };
    !rest.is_empty() && !s.chars().any(char::is_whitespace) && !s.contains('?') && !s.contains('#')
}

/// Extract + validate the `extra.bougie.native-binary` spec from a
/// parsed `composer.json`. `Err` means the section exists but is
/// malformed (worth a warning); `Absent` / `UnsupportedSpec` are the
/// quiet skip paths.
pub fn read_spec(composer_json: &serde_json::Value) -> Result<SpecLookup> {
    let Some(section) = composer_json
        .get("extra")
        .and_then(|e| e.get("bougie"))
        .and_then(|b| b.get("native-binary"))
    else {
        return Ok(SpecLookup::Absent);
    };

    let Some(spec) = section.get("spec").and_then(serde_json::Value::as_u64) else {
        bail!("`extra.bougie.native-binary.spec` is missing or not an integer");
    };
    if spec != SUPPORTED_SPEC {
        return Ok(SpecLookup::UnsupportedSpec(spec));
    }

    let str_field = |key: &str| -> Result<String> {
        section
            .get(key)
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| {
                eyre::eyre!("`extra.bougie.native-binary.{key}` is missing or not a string")
            })
    };
    let str_list = |key: &str, max: usize| -> Result<Vec<String>> {
        let arr = section
            .get(key)
            .and_then(serde_json::Value::as_array)
            .ok_or_else(|| {
                eyre::eyre!("`extra.bougie.native-binary.{key}` is missing or not an array")
            })?;
        if arr.is_empty() || arr.len() > max {
            bail!("`extra.bougie.native-binary.{key}` must have 1..={max} entries");
        }
        arr.iter()
            .map(|v| {
                v.as_str().map(str::to_owned).ok_or_else(|| {
                    eyre::eyre!("non-string entry in `extra.bougie.native-binary.{key}`")
                })
            })
            .collect()
    };

    let name = str_field("name")?;
    if !valid_segment(&name) {
        bail!("`extra.bougie.native-binary.name` `{name}` has characters outside [A-Za-z0-9._-]");
    }
    // The prefix may be empty (tag == bare version); when set it must
    // be segment-safe since it's spliced into the tag URL segment.
    let tag_prefix = str_field("tag-prefix")?;
    if !tag_prefix.is_empty() && !valid_segment(&tag_prefix) {
        bail!("`extra.bougie.native-binary.tag-prefix` `{tag_prefix}` has characters outside [A-Za-z0-9._-]");
    }
    let base_urls = str_list("base-urls", 8)?;
    for url in &base_urls {
        if !valid_base_url(url) {
            bail!(
                "`extra.bougie.native-binary.base-urls` entry `{url}` must be a plain https:// URL"
            );
        }
    }
    let targets = str_list("targets", 32)?;
    for t in &targets {
        if !valid_segment(t) {
            bail!("`extra.bougie.native-binary.targets` entry `{t}` has characters outside [A-Za-z0-9._-]");
        }
    }

    // Optional signing requirement. Presence of the key with a bad
    // shape is a hard parse error — silently ignoring a malformed
    // `sigstore` section would downgrade to unverified downloads.
    let sigstore_repository = match section.get("sigstore") {
        None => None,
        Some(sig) => {
            let repo = sig
                .get("repository")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| {
                    eyre::eyre!(
                        "`extra.bougie.native-binary.sigstore.repository` is missing or not a string"
                    )
                })?;
            if !valid_github_repository(repo) {
                bail!(
                    "`extra.bougie.native-binary.sigstore.repository` `{repo}` is not a plain GitHub `owner/name`"
                );
            }
            Some(repo.to_owned())
        }
    };

    Ok(SpecLookup::Found(NativeBinarySpec {
        name,
        tag_prefix,
        base_urls,
        targets,
        sigstore_repository,
    }))
}

/// A GitHub `owner/name`: exactly one `/`, both halves segment-safe.
fn valid_github_repository(s: &str) -> bool {
    match s.split_once('/') {
        Some((owner, name)) => valid_segment(owner) && valid_segment(name),
        None => false,
    }
}

/// `v0.2.1` → `0.2.1` (Composer keeps the tag's `v` in the pretty
/// version; the launchers' stamped version and the release cache dirs
/// use the bare number). Only strips a `v` directly followed by a
/// digit so odd version strings pass through unmangled.
fn normalize_version(pretty: &str) -> &str {
    match pretty.strip_prefix('v') {
        Some(rest) if rest.starts_with(|c: char| c.is_ascii_digit()) => rest,
        _ => pretty,
    }
}

/// Derive the concrete fetch plan for one (spec, version, host) —
/// pure and deterministic so it's exhaustively unit-testable. Skips
/// (rather than errors) on non-release versions and hosts the package
/// publishes no binary for: both are ordinary conditions where the
/// launcher's own first-run behaviour is the answer.
pub fn plan(
    spec: &NativeBinarySpec,
    locked_version: &str,
    host_target: &str,
    cache_base: &Path,
) -> PlanDecision {
    let version = normalize_version(locked_version);
    if !valid_segment(version) || !version.starts_with(|c: char| c.is_ascii_digit()) {
        return PlanDecision::Skip(format!(
            "package version `{locked_version}` is not a release version"
        ));
    }
    if !spec.targets.iter().any(|t| t == host_target) {
        return PlanDecision::Skip(format!("no prebuilt binary for {host_target}"));
    }

    let windows = host_target.contains("-windows-");
    let ext = if windows { ".zip" } else { ".tar.gz" };
    let kind = if windows {
        ArchiveKind::Zip
    } else {
        ArchiveKind::TarGz
    };
    let bin_name = if windows {
        format!("{}.exe", spec.name)
    } else {
        spec.name.clone()
    };

    let tag = format!("{}{version}", spec.tag_prefix);
    let archive_name = format!("{}-{host_target}{ext}", spec.name);
    let file_urls = |file: &str| -> Vec<String> {
        spec.base_urls
            .iter()
            .map(|base| format!("{}/{tag}/{file}", base.trim_end_matches('/')))
            .collect()
    };

    PlanDecision::Fetch(Box::new(PrefetchPlan {
        archive_urls: file_urls(&archive_name),
        sidecar_urls: file_urls(&format!("{archive_name}.sha256")),
        signing: spec.sigstore_repository.as_ref().map(|repo| PlanSigning {
            repository: repo.clone(),
            bundle_urls: file_urls(&format!("{archive_name}.sig")),
        }),
        staged_rel: PathBuf::from(format!("{}-{host_target}", spec.name)).join(&bin_name),
        cache_file: cache_base.join(&spec.name).join(version).join(&bin_name),
        name: spec.name.clone(),
        version: version.to_string(),
        target: host_target.to_string(),
        archive_name,
        kind,
    }))
}

/// The user cache base the PHP launchers resolve in their
/// `cacheDir()`: `%LOCALAPPDATA%` on Windows, `$XDG_CACHE_HOME` else
/// `$HOME/.cache` elsewhere. Deliberately NOT bougie's own cache —
/// prefetching is only useful if the launcher finds the file. `None`
/// (no resolvable base) skips the prefetch; the launcher's own temp
/// fallback isn't worth replicating.
pub fn launcher_cache_base() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        std::env::var_os("LOCALAPPDATA")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
    }
    #[cfg(not(windows))]
    {
        if let Some(xdg) = std::env::var_os("XDG_CACHE_HOME").filter(|v| !v.is_empty()) {
            return Some(PathBuf::from(xdg));
        }
        std::env::var_os("HOME")
            .filter(|v| !v.is_empty())
            .map(|home| PathBuf::from(home).join(".cache"))
    }
}

/// The tool package's exact locked version, read from the tool dir's
/// `composer.lock` (written by the resolve step just before install).
fn locked_version(tool_dir: &Path, package: &str) -> Result<String> {
    let lock_path = tool_dir.join("composer.lock");
    let bytes =
        std::fs::read(&lock_path).wrap_err_with(|| format!("reading {}", lock_path.display()))?;
    let lock: serde_json::Value = serde_json::from_slice(&bytes)
        .wrap_err_with(|| format!("parsing {}", lock_path.display()))?;
    lock.get("packages")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .find(|p| p.get("name").and_then(serde_json::Value::as_str) == Some(package))
        .and_then(|p| p.get("version").and_then(serde_json::Value::as_str))
        .map(str::to_owned)
        .ok_or_else(|| eyre::eyre!("`{package}` has no version in {}", lock_path.display()))
}

/// Testable core of the prefetch flow: consult the *installed*
/// package's `composer.json` (the same bytes Packagist serves as
/// metadata), derive the plan, and hand it to the fetcher unless the
/// cache is already warm. Returns the cache path when the binary is
/// in place afterwards, `None` on any of the (noted) skip paths.
pub fn prefetch_with(
    tool_dir: &Path,
    package: &str,
    host_target: &str,
    cache_base: &Path,
    fetcher: &NativeFetcher,
) -> Result<Option<PathBuf>> {
    let manifest = tool_dir.join("vendor").join(package).join("composer.json");
    let Ok(bytes) = std::fs::read(&manifest) else {
        // emit_bins reports a missing vendor manifest properly;
        // prefetch just declines to guess.
        return Ok(None);
    };
    let doc: serde_json::Value = serde_json::from_slice(&bytes)
        .wrap_err_with(|| format!("parsing {}", manifest.display()))?;

    let spec = match read_spec(&doc)? {
        SpecLookup::Absent => return Ok(None),
        SpecLookup::UnsupportedSpec(v) => {
            eprintln!(
                "note: `{package}` declares a native binary (spec {v}) this bougie doesn't \
                 support; it will be downloaded on first run"
            );
            return Ok(None);
        }
        SpecLookup::Found(spec) => spec,
    };

    let version = locked_version(tool_dir, package)?;
    match plan(&spec, &version, host_target, cache_base) {
        PlanDecision::Skip(reason) => {
            eprintln!("note: not prefetching `{package}`'s native binary: {reason}");
            Ok(None)
        }
        PlanDecision::Fetch(p) => {
            if p.cache_file.is_file() {
                // Already cached (a previous install, `bougie tool
                // run`, or a plain composer install that ran the
                // tool). Never re-download, never overwrite.
                return Ok(Some(p.cache_file.clone()));
            }
            fetcher(&p)
                .wrap_err_with(|| format!("prefetching {} {} ({})", p.name, p.version, p.target))?;
            Ok(Some(p.cache_file.clone()))
        }
    }
}

/// Production entry point: detect the host triple and the launcher
/// cache base from the environment, then run [`prefetch_with`].
pub fn prefetch_into_launcher_cache(
    tool_dir: &Path,
    package: &str,
    fetcher: &NativeFetcher,
) -> Result<Option<PathBuf>> {
    let host_target = match bougie_platform::target::Triple::detect() {
        Ok(t) => t.to_string(),
        Err(e) => {
            eprintln!("note: not prefetching `{package}`'s native binary: {e:#}");
            return Ok(None);
        }
    };
    let Some(cache_base) = launcher_cache_base() else {
        eprintln!(
            "note: not prefetching `{package}`'s native binary: no user cache dir \
             (HOME/XDG_CACHE_HOME unset)"
        );
        return Ok(None);
    };
    prefetch_with(tool_dir, package, &host_target, &cache_base, fetcher)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn wick_json(extra: &str) -> serde_json::Value {
        serde_json::from_str(&format!(
            r#"{{"name":"cresset/wick","require":{{"php":">=8.1"}},"bin":["bin/wick"]{extra}}}"#
        ))
        .unwrap()
    }

    const WICK_EXTRA: &str = r#","extra":{"bougie":{"native-binary":{
        "spec":1,
        "name":"wick",
        "tag-prefix":"wick-v",
        "base-urls":[
            "https://releases.bougie.tools/github/wick/releases/download",
            "https://github.com/cresset-tools/wick/releases/download"
        ],
        "targets":[
            "x86_64-unknown-linux-gnu",
            "x86_64-unknown-linux-musl",
            "aarch64-apple-darwin",
            "x86_64-pc-windows-msvc"
        ],
        "sigstore":{"repository":"cresset-tools/wick"}}}}"#;

    fn wick_spec() -> NativeBinarySpec {
        match read_spec(&wick_json(WICK_EXTRA)).unwrap() {
            SpecLookup::Found(s) => s,
            other => panic!("expected Found, got {other:?}"),
        }
    }

    #[test]
    fn read_spec_absent_without_extra() {
        assert_eq!(read_spec(&wick_json("")).unwrap(), SpecLookup::Absent);
    }

    #[test]
    fn read_spec_parses_wick_block() {
        let spec = wick_spec();
        assert_eq!(spec.name, "wick");
        assert_eq!(spec.tag_prefix, "wick-v");
        assert_eq!(spec.base_urls.len(), 2);
        assert_eq!(spec.targets.len(), 4);
        assert_eq!(
            spec.sigstore_repository.as_deref(),
            Some("cresset-tools/wick")
        );
    }

    #[test]
    fn read_spec_allows_omitting_sigstore() {
        let json = wick_json(
            r#","extra":{"bougie":{"native-binary":{"spec":1,"name":"wick",
              "tag-prefix":"v","base-urls":["https://example.com/dl"],
              "targets":["x86_64-unknown-linux-gnu"]}}}"#,
        );
        let SpecLookup::Found(spec) = read_spec(&json).unwrap() else {
            panic!("expected Found");
        };
        assert_eq!(spec.sigstore_repository, None);
    }

    #[test]
    fn read_spec_rejects_malformed_sigstore_repository() {
        for bad in [
            r#"{"repository":"not-a-repo"}"#,
            r#"{"repository":"a/b/c"}"#,
            r#"{"repository":"../x/y"}"#,
            r#"{}"#,
        ] {
            let json = wick_json(&format!(
                r#","extra":{{"bougie":{{"native-binary":{{"spec":1,"name":"wick",
                  "tag-prefix":"v","base-urls":["https://example.com/dl"],
                  "targets":["x86_64-unknown-linux-gnu"],
                  "sigstore":{bad}}}}}}}"#
            ));
            let err = read_spec(&json).unwrap_err();
            assert!(err.to_string().contains("sigstore"), "{bad}: {err}");
        }
    }

    #[test]
    fn read_spec_skips_newer_spec_revision() {
        let json = wick_json(r#","extra":{"bougie":{"native-binary":{"spec":2,"name":"wick"}}}"#);
        assert_eq!(read_spec(&json).unwrap(), SpecLookup::UnsupportedSpec(2));
    }

    #[test]
    fn read_spec_rejects_traversal_name() {
        let json = wick_json(
            r#","extra":{"bougie":{"native-binary":{"spec":1,"name":"../evil",
              "tag-prefix":"v","base-urls":["https://example.com/dl"],
              "targets":["x86_64-unknown-linux-gnu"]}}}"#,
        );
        let err = read_spec(&json).unwrap_err();
        assert!(err.to_string().contains("name"), "{err}");
    }

    #[test]
    fn read_spec_rejects_non_https_url() {
        let json = wick_json(
            r#","extra":{"bougie":{"native-binary":{"spec":1,"name":"wick",
              "tag-prefix":"v","base-urls":["http://example.com/dl"],
              "targets":["x86_64-unknown-linux-gnu"]}}}"#,
        );
        let err = read_spec(&json).unwrap_err();
        assert!(err.to_string().contains("https"), "{err}");
    }

    #[test]
    fn read_spec_rejects_missing_targets() {
        let json = wick_json(
            r#","extra":{"bougie":{"native-binary":{"spec":1,"name":"wick",
              "tag-prefix":"v","base-urls":["https://example.com/dl"]}}}"#,
        );
        assert!(read_spec(&json).is_err());
    }

    #[test]
    fn plan_derives_dist_layout_for_linux_gnu() {
        let decision = plan(
            &wick_spec(),
            "v0.2.1",
            "x86_64-unknown-linux-gnu",
            Path::new("/home/u/.cache"),
        );
        let PlanDecision::Fetch(p) = decision else {
            panic!("expected Fetch, got {decision:?}");
        };
        assert_eq!(p.version, "0.2.1");
        assert_eq!(p.kind, ArchiveKind::TarGz);
        assert_eq!(p.archive_name, "wick-x86_64-unknown-linux-gnu.tar.gz");
        assert_eq!(
            p.archive_urls[0],
            "https://releases.bougie.tools/github/wick/releases/download/wick-v0.2.1/wick-x86_64-unknown-linux-gnu.tar.gz"
        );
        assert_eq!(
            p.sidecar_urls[1],
            "https://github.com/cresset-tools/wick/releases/download/wick-v0.2.1/wick-x86_64-unknown-linux-gnu.tar.gz.sha256"
        );
        let signing = p.signing.as_ref().expect("spec declares sigstore");
        assert_eq!(signing.repository, "cresset-tools/wick");
        assert_eq!(
            signing.bundle_urls[0],
            "https://releases.bougie.tools/github/wick/releases/download/wick-v0.2.1/wick-x86_64-unknown-linux-gnu.tar.gz.sig"
        );
        assert_eq!(
            p.staged_rel,
            PathBuf::from("wick-x86_64-unknown-linux-gnu/wick")
        );
        assert_eq!(
            p.cache_file,
            PathBuf::from("/home/u/.cache/wick/0.2.1/wick")
        );
    }

    #[test]
    fn plan_uses_zip_and_exe_for_windows_target() {
        let decision = plan(
            &wick_spec(),
            "0.2.1",
            "x86_64-pc-windows-msvc",
            Path::new("/base"),
        );
        let PlanDecision::Fetch(p) = decision else {
            panic!("expected Fetch, got {decision:?}");
        };
        assert_eq!(p.kind, ArchiveKind::Zip);
        assert_eq!(p.archive_name, "wick-x86_64-pc-windows-msvc.zip");
        assert_eq!(
            p.staged_rel,
            PathBuf::from("wick-x86_64-pc-windows-msvc/wick.exe")
        );
        assert_eq!(p.cache_file, PathBuf::from("/base/wick/0.2.1/wick.exe"));
    }

    #[test]
    fn plan_skips_unsupported_target() {
        let decision = plan(
            &wick_spec(),
            "0.2.1",
            "aarch64-unknown-linux-gnu",
            Path::new("/base"),
        );
        assert!(matches!(decision, PlanDecision::Skip(_)), "{decision:?}");
    }

    #[test]
    fn plan_skips_dev_version() {
        let decision = plan(
            &wick_spec(),
            "dev-main",
            "x86_64-unknown-linux-gnu",
            Path::new("/b"),
        );
        assert!(matches!(decision, PlanDecision::Skip(_)), "{decision:?}");
    }

    fn write_tool_dir(td: &Path, extra: &str, version: &str) {
        let pkg = td.join("vendor").join("cresset").join("wick");
        std::fs::create_dir_all(&pkg).unwrap();
        std::fs::write(
            pkg.join("composer.json"),
            serde_json::to_vec(&wick_json(extra)).unwrap(),
        )
        .unwrap();
        std::fs::write(
            td.join("composer.lock"),
            format!(r#"{{"packages":[{{"name":"cresset/wick","version":"{version}"}}]}}"#),
        )
        .unwrap();
    }

    #[test]
    fn prefetch_with_calls_fetcher_and_reports_cache_path() {
        let td = tempfile::TempDir::new().unwrap();
        write_tool_dir(td.path(), WICK_EXTRA, "v0.2.1");
        let cache = td.path().join("cache");

        static CALLS: AtomicUsize = AtomicUsize::new(0);
        let fetcher = |plan: &PrefetchPlan| -> Result<()> {
            CALLS.fetch_add(1, Ordering::SeqCst);
            std::fs::create_dir_all(plan.cache_file.parent().unwrap()).unwrap();
            std::fs::write(&plan.cache_file, b"elf").unwrap();
            Ok(())
        };

        let got = prefetch_with(
            td.path(),
            "cresset/wick",
            "x86_64-unknown-linux-gnu",
            &cache,
            &fetcher,
        )
        .unwrap();
        assert_eq!(got, Some(cache.join("wick").join("0.2.1").join("wick")));
        assert_eq!(CALLS.load(Ordering::SeqCst), 1);

        // Second run: cache hit, fetcher not called again.
        let again = prefetch_with(
            td.path(),
            "cresset/wick",
            "x86_64-unknown-linux-gnu",
            &cache,
            &fetcher,
        )
        .unwrap();
        assert_eq!(again, got);
        assert_eq!(CALLS.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn prefetch_with_is_noop_without_spec() {
        let td = tempfile::TempDir::new().unwrap();
        write_tool_dir(td.path(), "", "v0.2.1");
        let fetcher = |_: &PrefetchPlan| -> Result<()> {
            panic!("fetcher must not run for spec-less packages")
        };
        let got = prefetch_with(
            td.path(),
            "cresset/wick",
            "x86_64-unknown-linux-gnu",
            td.path(),
            &fetcher,
        )
        .unwrap();
        assert_eq!(got, None);
    }
}
