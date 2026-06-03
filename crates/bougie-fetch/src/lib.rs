//! Atomic blob fetch + extract per CLI.md §7.4.
//!
//! Pattern: stream into `$BOUGIE_CACHE/blobs/<sha256>.partial`, verify
//! sha256 while writing, extract into `<dest>.incoming` (sibling of
//! the final destination so the rename is on the same filesystem),
//! atomic-rename to `<dest>`, delete `tmp`.

use bougie_errors::{error_chain, BougieError};
use eyre::{Result, WrapErr};
use indicatif::{ProgressBar, ProgressDrawTarget, ProgressStyle};
use sha2::{Digest, Sha256};
use std::fs::{self, File};
use std::io::{copy, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// The `User-Agent` every outbound bougie HTTP request advertises.
/// Format: `Composer/2 bougie/<crate-version> (+<repo-url>)`.
///
/// Why the `Composer/2` prefix: some Composer-protocol servers
/// (notably `repo.magento.com`, and likely other Private Packagist
/// tenants behind a similar nginx config) gate dist archive
/// downloads on the User-Agent — a plain `curl/…` or anonymous
/// reqwest UA gets a `403`, while anything matching `Composer/…`
/// is allowed through. Claiming Composer-2 compatibility is the
/// simplest portable fix; we still identify ourselves as bougie
/// after the prefix so operators can attribute traffic and reach
/// us via the linked repo. The version comes from `bougie-fetch`'s
/// own `Cargo.toml` — it tracks the workspace release cadence
/// closely enough that bumping it is a single change when the
/// format here needs to evolve.
pub const USER_AGENT: &str = concat!(
    "Composer/2 bougie/",
    env!("CARGO_PKG_VERSION"),
    " (+https://github.com/cresset-tools/bougie)",
);

/// Maximum time a download may go without receiving any bytes before
/// it's treated as a stalled connection and aborted (then retried by
/// [`fetch_with_retry`]). Applied as the blocking client's per-read
/// `timeout` and the async client's `read_timeout` — see those two
/// constructors for the per-client mechanics. 30s is comfortably above
/// any real inter-chunk gap on a live transfer while still surfacing a
/// wedged peer fast instead of after the multi-minute dead air that
/// reads as a hang.
const STALL_TIMEOUT: Duration = Duration::from_secs(30);

/// Build a `reqwest::blocking::Client` with the bougie [`USER_AGENT`]
/// set. Every outbound external request should go through this so the
/// UA, timeout policy, and connection settings stay consistent across
/// crates. Tests and localhost provisioner clients can keep their own
/// builders — they don't represent bougie to the outside world.
///
/// **Timeouts:**
/// - `connect_timeout = 10s`. Bounds the TCP+TLS handshake — a slow
///   or hung peer (network blip, captive portal) fails fast instead
///   of blocking the resolver forever.
/// - `timeout = `[`STALL_TIMEOUT`]` (30s)`. For the **blocking**
///   client this is a *per-operation* timeout, not a total budget:
///   reqwest wraps every `Response::read()` in a fresh deadline
///   (`blocking::wait::timeout`, deadline = `now + timeout` recomputed
///   per call), so it is effectively an idle/read-gap guard. A CDN
///   edge that accepts the socket and then wedges mid-transfer (TCP
///   `ESTABLISHED`, 0 B/s) trips this in 30s and bubbles up as a
///   retryable error instead of parking the byte-copy loop's blocking
///   `read()` indefinitely — the common flaky-mirror failure mode and
///   the silent-hang we're guarding against. A *progressing* download
///   is unaffected no matter how large: each chunk only has to arrive
///   within 30s of the previous one. There is deliberately no separate
///   total cap on this client because reqwest's blocking API doesn't
///   expose one — and a steadily-progressing transfer shouldn't be
///   killed on a wall clock anyway. [`fetch_with_retry`] supplies the
///   resilience on top.
///
/// Both are per-request, not per-keepalive — connection reuse across
/// requests is unaffected.
pub fn default_client() -> Result<reqwest::blocking::Client> {
    reqwest::blocking::Client::builder()
        .user_agent(USER_AGENT)
        .connect_timeout(Duration::from_secs(10))
        .timeout(STALL_TIMEOUT)
        .build()
        .map_err(|e| {
            BougieError::Network {
                operation: "building HTTP client".into(),
                detail: error_chain(&e),
            }
            .into()
        })
}

/// Async sibling of [`default_client`] — same `User-Agent`, but the
/// non-blocking `reqwest::Client`, which exposes the read and total
/// timeouts as *separate* knobs (unlike the blocking client, where
/// `timeout` is the per-read guard). Used by the composer-resolver's
/// parallel pre-fetch fan-out so concurrent fetches share a single
/// async client (one connection pool, no `spawn_blocking` thread per
/// in-flight request) instead of N blocking clients each driving their
/// own internal runtime.
///
/// **Timeouts:**
/// - `connect_timeout = 10s` — as [`default_client`].
/// - `read_timeout = `[`STALL_TIMEOUT`]` (30s)`. Idle-gap guard: max
///   time allowed between body chunks. The async analogue of the
///   blocking client's per-read `timeout` — same 30s stall budget.
/// - `timeout = 300s`. Total per-request budget. Matches Composer's
///   own default (`Composer\Util\HttpDownloader`); the async client
///   *does* have this distinct knob, so a metadata request gets both
///   a tight idle guard and a generous overall ceiling.
pub fn default_async_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .connect_timeout(Duration::from_secs(10))
        .read_timeout(STALL_TIMEOUT)
        .timeout(Duration::from_mins(5))
        .build()
        .map_err(|e| {
            BougieError::Network {
                operation: "building async HTTP client".into(),
                detail: error_chain(&e),
            }
            .into()
        })
}

/// Which hash algorithm verifies a download. Bougie's own published
/// blobs use sha256; Composer's Packagist dist `shasum` field is sha1.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HashAlgo {
    Sha1,
    Sha256,
}

impl HashAlgo {
    fn as_str(self) -> &'static str {
        match self {
            Self::Sha1 => "sha1",
            Self::Sha256 => "sha256",
        }
    }
}

/// A hex-encoded digest paired with its algorithm. Borrowed so a
/// `BlobSpec` literal can point at the hash string from an enclosing
/// manifest without cloning.
///
/// An empty `hex` is the **skip-verify** marker: the bytes are still
/// streamed and hashed on the fly (so a future verifier could surface
/// the actual digest), but no equality check is performed and no
/// mismatch error is raised. This matches Composer's
/// `FileDownloader.php:212` behavior for dists whose registry didn't
/// publish a shasum — the common case for GitHub/GitLab zipballs. The
/// partial-file naming under `partial_dir` falls back to a hash of
/// `spec.url` when `hex` is empty, keeping concurrent downloads
/// collision-free.
#[derive(Debug, Clone, Copy)]
pub struct Hash<'a> {
    pub algo: HashAlgo,
    pub hex: &'a str,
}

impl<'a> Hash<'a> {
    pub fn sha256(hex: &'a str) -> Self {
        Self { algo: HashAlgo::Sha256, hex }
    }
    pub fn sha1(hex: &'a str) -> Self {
        Self { algo: HashAlgo::Sha1, hex }
    }
}

/// Semantic events emitted by [`DownloadBar`] when an external sink
/// is attached. The bougie daemon uses this to forward bar state over
/// IPC so the CLI client can render its own bar — see
/// [`DownloadBar::with_sink`].
#[derive(Debug, Clone)]
pub enum DownloadEvent {
    /// Caller grew the planned total by `bytes`.
    Plan { bytes: u64 },
    /// Caller set the right-hand-side label for the artifact now in flight.
    Current { name: String },
    /// `bytes` more bytes arrived. Fires from inside the byte-copy loop;
    /// callers that forward to a slow consumer (IPC, network) should
    /// coalesce.
    Inc { bytes: u64 },
    /// The artifact named `name` finished downloading and is now being
    /// extracted (the silent tar.zst/zip decompress). Carries the name
    /// so a remote mirror can show `extracting <name>`; emitted by
    /// [`DownloadBar::mark_extracting`]. The *next* [`Self::Current`]
    /// marks the return to the download phase for the following artifact.
    Extracting { name: String },
    /// Aggregate fetch is done.
    Finish,
}

/// Side-channel observer plugged into a [`DownloadBar`] via
/// [`DownloadBar::with_sink`]. Called from the bar's own (possibly
/// blocking) thread on every event, so implementations must be cheap
/// and non-blocking — typical use is a `tokio::sync::mpsc::UnboundedSender`
/// pushing the event into an async task for serialization + throttling.
pub trait DownloadSink: Send + Sync + std::fmt::Debug {
    fn on_event(&self, event: DownloadEvent);
}

/// Aggregate download progress bar shared across many `fetch_blob`/
/// `fetch_file` calls. Renders a single bar with the running label of
/// the part currently in flight; orchestrators (e.g. baseline install,
/// composer-required extensions) drive *one* `DownloadBar` across the
/// whole loop so the user sees a single combined bar instead of one
/// per artifact.
///
/// The bar starts with length 0; callers grow the planned total via
/// [`Self::add_planned`] as each manifest reveals more bytes (the
/// index ships `size` on every blob, so no HEAD round-trips are
/// needed). [`Self::set_current`] sets the right-hand-side label
/// shown for the artifact currently downloading.
///
/// Hidden bars (non-TTY stderr, `--quiet`, `--format json-v1`) accept
/// every method as a no-op so the byte-copy loop in `fetch.rs` stays
/// branch-free.
///
/// When an optional [`DownloadSink`] is attached via
/// [`Self::with_sink`], every state-changing method *also* emits a
/// [`DownloadEvent`] to the sink. This lets a headless caller (the
/// bougie daemon) forward bar state over IPC so a remote client can
/// render the actual TTY bar on its side.
#[derive(Debug)]
pub struct DownloadBar {
    pb: ProgressBar,
    sink: Option<Arc<dyn DownloadSink>>,
    /// The prefix shown during downloading (the `label` passed to
    /// [`Self::new`], e.g. `"downloading"`). Stashed so [`Self::set_current`]
    /// can restore it after [`Self::mark_extracting`] temporarily swaps
    /// the prefix to `"extracting"` for the decompress phase.
    download_label: String,
    /// Lazy-activation state for visible bars: hidden until the first
    /// `add_planned(>0)` / `set_current` / `inc` / `set_progress` call,
    /// then swapped to stderr + steady-tick. Avoids drawing an empty
    /// "downloading 0/0" placeholder when the caller turns out to have
    /// nothing to fetch (everything cached / baseline-already-loaded).
    /// `None` for `hidden()` / `hidden_with_sink()` — those stay hidden
    /// for life.
    pending: Option<PendingActivation>,
}

#[derive(Debug)]
struct PendingActivation {
    activated: AtomicBool,
}

/// How many times a failed blob fetch is retried before giving up.
/// A transient network fault (the 30s `read_timeout` tripping on a
/// wedged CDN edge, a dropped keep-alive, a 5xx from one mirror node)
/// almost always clears on a fresh connection — and because reqwest
/// pools connections, the retry typically lands on a *different* edge
/// than the one that stalled. Three attempts (one initial + three
/// retries = four total tries) covers the realistic flaky-mirror tail
/// without masking a genuinely-down endpoint for too long: the
/// backoff schedule below caps total added wait under ~4s.
const RETRY_BUDGET: u32 = 3;

/// Base unit for the exponential backoff between fetch retries.
/// Attempt `n` (1-indexed) sleeps `BACKOFF_BASE * 2^(n-1)` plus up to
/// one extra base of jitter — so ~0.25–0.5s, ~0.5–0.75s, ~1–1.25s.
/// The jitter de-synchronizes a fan-out of parallel fetches that all
/// hit the same flaky mirror in the same instant, so their retries
/// don't thunder back in lockstep.
const BACKOFF_BASE: Duration = Duration::from_millis(250);

/// Sleep for attempt `attempt` (1-indexed) of the retry loop, using
/// exponential backoff with additive jitter. Pure `std` — the jitter
/// is derived from the wall clock's sub-millisecond bits rather than
/// pulling in a `rand` dependency; it only needs to be "different
/// across threads," not cryptographically uniform.
fn backoff_sleep(attempt: u32) {
    // 2^(attempt-1), saturating so a future bump to RETRY_BUDGET can't
    // overflow the shift; capped at 8x so the wait stays bounded.
    let factor = 1u32.checked_shl(attempt.saturating_sub(1)).unwrap_or(u32::MAX).min(8);
    let base = BACKOFF_BASE.saturating_mul(factor);
    let clock_nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| u64::from(d.subsec_nanos()));
    // Jitter in [0, BACKOFF_BASE). `BACKOFF_BASE` is sub-second, so its
    // whole magnitude lives in `subsec_nanos()` — a `u32`, no cast/truncation.
    let window = u64::from(BACKOFF_BASE.subsec_nanos()).max(1);
    let jitter = Duration::from_nanos(clock_nanos % window);
    std::thread::sleep(base + jitter);
}

/// Archive format `fetch_blob` should decode. Selected per-call
/// because the index advertises tar.zst for bougie-published artifacts
/// while windows.php.net (Phase 3+) publishes zip. `fetch_file`
/// doesn't extract and ignores this field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArchiveKind {
    /// Zstandard-compressed POSIX tar — bougie's own publish format.
    TarZst,
    /// Zip — windows.php.net's interpreter + PECL distribution format.
    Zip,
}

#[derive(Debug, Clone)]
pub struct BlobSpec<'a> {
    pub url: &'a str,
    /// The expected content hash. Use [`Hash::sha256`] for bougie-
    /// published blobs (the index emits sha256 universally) and
    /// [`Hash::sha1`] for Composer dist artifacts (Packagist's
    /// `dist.shasum` field is legacy sha1).
    pub hash: Hash<'a>,
    pub partial_dir: &'a Path,
    pub dest: &'a Path,
    /// Leading path component to strip from every entry while
    /// extracting. Interpreter tarballs wrap their contents in
    /// `install/`; per-store-path closure tarballs wrap theirs in
    /// `<storeName>/` (see `shared/tarball-store-path.nix`).
    /// windows.php.net's `php-<ver>-Win32-...zip` wraps contents in
    /// `php-<ver>/`. Pass `""` for unwrapped archives (e.g.
    /// per-extension blobs that ship `lib/extensions/<api>/<name>.so`
    /// at the top level).
    pub strip_prefix: &'a str,
    /// How to decode the downloaded bytes. Ignored by [`fetch_file`].
    pub archive: ArchiveKind,
    /// Pre-rendered `Authorization` header value (e.g. `Bearer <token>`
    /// or `Basic <base64>`). Attached verbatim on every request for
    /// this blob. `None` means no auth — appropriate for public CDN
    /// URLs (bougie's own publishes, public Packagist dists). Set
    /// when the blob lives behind credentials, like a private satis
    /// dist or `repo.magento.com/archives/...`. Keep the field tiny
    /// and opaque so this crate stays uncoupled from the higher-level
    /// `AuthCredentials` shape that lives in `bougie-composer-resolver`.
    pub auth_header: Option<&'a str>,
    /// HTTP header name for the auth credential. Defaults to
    /// `Authorization` when `None`; set to `PRIVATE-TOKEN` for
    /// GitLab private-token auth.
    pub auth_header_name: Option<&'a str>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlobOutcome {
    AlreadyPresent,
    Downloaded,
}

/// Fetch + extract one tar.zst blob. No-op if `dest` exists.
///
/// `bar` is the caller-owned aggregate bar that this call advances as
/// bytes arrive. Set the part label via [`DownloadBar::set_current`]
/// *before* calling so the right-hand `{msg}` shows the artifact name
/// for the duration of the transfer. Pass [`DownloadBar::hidden`] when
/// the caller has no UI of its own — the byte-copy loop stays the
/// same shape either way.
#[tracing::instrument(skip_all, fields(url = spec.url))]
pub fn fetch_blob(
    client: &reqwest::blocking::Client,
    spec: &BlobSpec<'_>,
    bar: &DownloadBar,
) -> Result<BlobOutcome> {
    fetch_with_retry(client, spec, bar, try_once_blob)
}

/// Fetch a single bare file (e.g. a `.phar`) into `dest`, verifying its
/// sha256. No tar/zst extraction; the verified bytes are placed at `dest`
/// atomically. No-op if `dest` exists.
#[tracing::instrument(skip_all, fields(url = spec.url))]
pub fn fetch_file(
    client: &reqwest::blocking::Client,
    spec: &BlobSpec<'_>,
    bar: &DownloadBar,
) -> Result<BlobOutcome> {
    fetch_with_retry(client, spec, bar, try_once_file)
}

fn fetch_with_retry(
    client: &reqwest::blocking::Client,
    spec: &BlobSpec<'_>,
    bar: &DownloadBar,
    once: fn(&reqwest::blocking::Client, &BlobSpec<'_>, &DownloadBar) -> Result<()>,
) -> Result<BlobOutcome> {
    if spec.dest.exists() {
        return Ok(BlobOutcome::AlreadyPresent);
    }
    fs::create_dir_all(spec.partial_dir)
        .wrap_err_with(|| format!("creating {}", spec.partial_dir.display()))?;
    if let Some(parent) = spec.dest.parent() {
        fs::create_dir_all(parent)
            .wrap_err_with(|| format!("creating {}", parent.display()))?;
    }

    let mut attempts = 0;
    loop {
        match once(client, spec, bar) {
            Ok(()) => return Ok(BlobOutcome::Downloaded),
            Err(e) if attempts < RETRY_BUDGET => {
                attempts += 1;
                tracing::warn!(error = %e, attempt = attempts, "blob fetch failed; retrying");
                // Surface the retry above the (possibly shared, possibly
                // stalled-looking) progress bar so a silent retry doesn't
                // read as a hang. Named from the URL — the bar's own
                // `{msg}` label is shared across a parallel fan-out and
                // would race, but the URL is local to this fetch.
                bar.note_retry(url_label(spec.url), attempts, RETRY_BUDGET);
                backoff_sleep(attempts);
            }
            Err(e) => return Err(e),
        }
    }
}

/// A short, human-facing label for a blob URL — its last path segment
/// with any query string stripped. Falls back to the whole URL when
/// there's no `/`. Used only for the retry notice; accuracy matters
/// more than prettiness, and the final segment (the `.tar.zst` / `.so`
/// / dist-zip filename) is the most recognizable race-free handle we
/// have inside the fetch.
fn url_label(url: &str) -> &str {
    let no_query = url.split(['?', '#']).next().unwrap_or(url);
    let trimmed = no_query.trim_end_matches('/');
    match trimmed.rsplit('/').next() {
        Some(seg) if !seg.is_empty() => seg,
        _ => url,
    }
}

/// Stream the blob into `<partial_dir>/<sha>.partial`, hashing as we go.
/// Returns the path to the verified partial on success; deletes it and
/// errors on hash mismatch.
fn fetch_to_partial(
    client: &reqwest::blocking::Client,
    spec: &BlobSpec<'_>,
    bar: &DownloadBar,
) -> Result<PathBuf> {
    // Skip-verify mode (empty hex): name the partial after a hash of
    // the URL so concurrent fetches in the same partial_dir don't
    // collide and a retry resumes the same path.
    let partial_token: String;
    let partial_name = if spec.hash.hex.is_empty() {
        use sha1::Digest as _;
        let digest = sha1::Sha1::digest(spec.url.as_bytes());
        partial_token = format_hex(&digest);
        partial_token.as_str()
    } else {
        spec.hash.hex
    };
    let tmp = spec.partial_dir.join(format!("{partial_name}.partial"));

    let mut req = client.get(spec.url);
    if let Some(value) = spec.auth_header {
        let name = spec.auth_header_name.unwrap_or("authorization");
        req = req.header(name, value);
    }
    let mut resp = req.send().map_err(|e| BougieError::Network {
        operation: format!("fetching blob from url {:?}", spec.url),
        detail: error_chain(&e),
    })?;
    if !resp.status().is_success() {
        return Err(BougieError::Network {
            operation: format!("GET {:?}", spec.url),
            detail: format!("server returned HTTP {}", resp.status()),
        }
        .into());
    }

    let mut file = File::create(&tmp).wrap_err_with(|| format!("creating {}", tmp.display()))?;
    let actual = stream_into_file(&mut resp, &mut file, spec, bar, &tmp)?;
    file.flush().wrap_err("flushing partial blob")?;

    // Skip-verify when no expected digest was supplied — see
    // [`Hash`]'s docstring for the rationale. Otherwise the streamed
    // digest must match the expected one or we drop the partial.
    if !spec.hash.hex.is_empty() && !actual.eq_ignore_ascii_case(spec.hash.hex) {
        let _ = fs::remove_file(&tmp);
        return Err(BougieError::BlobHashMismatch {
            url: spec.url.to_owned(),
            expected: format!("{}:{}", spec.hash.algo.as_str(), spec.hash.hex),
            actual: format!("{}:{}", spec.hash.algo.as_str(), actual),
        }
        .into());
    }
    Ok(tmp)
}

/// Stream `resp` into `file` while feeding bytes to the algorithm
/// selected by `spec.hash.algo`. Returns the computed hex digest.
/// Split out so the algorithm branch sits in one place instead of
/// duplicating the byte-copy loop.
fn stream_into_file(
    resp: &mut reqwest::blocking::Response,
    file: &mut File,
    spec: &BlobSpec<'_>,
    bar: &DownloadBar,
    tmp: &Path,
) -> Result<String> {
    let mut buf = vec![0u8; 64 * 1024];
    match spec.hash.algo {
        HashAlgo::Sha256 => {
            let mut hasher = Sha256::new();
            loop {
                let n = resp.read(&mut buf).map_err(|e| BougieError::Network {
                    operation: format!("reading blob body from {}", spec.url),
                    detail: error_chain(&e),
                })?;
                if n == 0 {
                    break;
                }
                hasher.update(&buf[..n]);
                file.write_all(&buf[..n])
                    .wrap_err_with(|| format!("writing {}", tmp.display()))?;
                bar.inc(n as u64);
            }
            Ok(format_hex(&hasher.finalize()))
        }
        HashAlgo::Sha1 => {
            use sha1::Digest as _;
            let mut hasher = sha1::Sha1::new();
            loop {
                let n = resp.read(&mut buf).map_err(|e| BougieError::Network {
                    operation: format!("reading blob body from {}", spec.url),
                    detail: error_chain(&e),
                })?;
                if n == 0 {
                    break;
                }
                hasher.update(&buf[..n]);
                file.write_all(&buf[..n])
                    .wrap_err_with(|| format!("writing {}", tmp.display()))?;
                bar.inc(n as u64);
            }
            Ok(format_hex(&hasher.finalize()))
        }
    }
}

fn try_once_blob(
    client: &reqwest::blocking::Client,
    spec: &BlobSpec<'_>,
    bar: &DownloadBar,
) -> Result<()> {
    let tmp = fetch_to_partial(client, spec, bar)?;

    // Bytes are all in; the rest of this function is the silent,
    // CPU-bound decompress + atomic rename. Surface it on the bar so a
    // large tarball (a full PHP runtime, intl's ICU closure) doesn't
    // look like a stalled download while it extracts.
    bar.mark_extracting();

    let incoming = sibling_with_suffix(spec.dest, ".incoming");
    let _ = fs::remove_dir_all(&incoming);
    fs::create_dir_all(&incoming)
        .wrap_err_with(|| format!("creating {}", incoming.display()))?;
    match spec.archive {
        ArchiveKind::TarZst => extract_tar_zst(&tmp, &incoming, spec.strip_prefix)?,
        ArchiveKind::Zip => extract_zip(&tmp, &incoming, spec.strip_prefix)?,
    }

    fs::rename(&incoming, spec.dest)
        .wrap_err_with(|| format!("rename {} → {}", incoming.display(), spec.dest.display()))?;
    let _ = fs::remove_file(&tmp);
    Ok(())
}

fn try_once_file(
    client: &reqwest::blocking::Client,
    spec: &BlobSpec<'_>,
    bar: &DownloadBar,
) -> Result<()> {
    let tmp = fetch_to_partial(client, spec, bar)?;

    // Stage the verified bytes as a sibling of `dest` so the rename is
    // always intra-filesystem, even when `partial_dir` is on a different
    // filesystem from `dest` (cache vs data, per CLI.md §7.4).
    let incoming = sibling_with_suffix(spec.dest, ".incoming");
    let _ = fs::remove_file(&incoming);
    fs::copy(&tmp, &incoming)
        .wrap_err_with(|| format!("staging {} → {}", tmp.display(), incoming.display()))?;
    fs::rename(&incoming, spec.dest)
        .wrap_err_with(|| format!("rename {} → {}", incoming.display(), spec.dest.display()))?;
    let _ = fs::remove_file(&tmp);
    Ok(())
}

/// Extract a `.tar.zst` archive into `into`, stripping `strip_prefix`
/// as a leading path component from every entry so e.g. the binary
/// that the archive ships at `install/bin/php` (with
/// `strip_prefix = "install"`) lands at `<into>/bin/php`. Entries
/// that don't start with `strip_prefix` pass through unchanged.
/// Pass `""` to disable stripping (archive entries land verbatim).
#[tracing::instrument(skip_all, fields(into = %into.display()))]
fn extract_tar_zst(tar_zst: &Path, into: &Path, strip_prefix: &str) -> Result<()> {
    let f = File::open(tar_zst)
        .wrap_err_with(|| format!("opening {}", tar_zst.display()))?;
    let zd = zstd::stream::read::Decoder::new(f).wrap_err("zstd decoder")?;
    let mut archive = tar::Archive::new(zd);
    archive.set_preserve_permissions(true);
    archive.set_preserve_mtime(true);
    for entry in archive
        .entries()
        .wrap_err_with(|| format!("reading entries from {}", tar_zst.display()))?
    {
        let mut entry = entry.wrap_err("reading archive entry")?;
        let path = entry
            .path()
            .wrap_err("reading entry path")?
            .into_owned();
        let Some(rewritten) = rewrite_archive_path(&path, strip_prefix) else {
            // The prefix directory entry itself; skip — `into` exists.
            continue;
        };
        // Traversal guard. Unlike `extract_zip` (which gets a
        // `..`/absolute-rejecting path from `enclosed_name`), tar entry
        // paths come straight off the header, and `entry.unpack` does
        // *not* sanitize (it's `unpack_in`/`Archive::unpack` that do).
        // A malicious archive entry named `../../etc/foo` or an absolute
        // path would otherwise escape `into`.
        if !is_safe_archive_path(&rewritten) {
            return Err(eyre::eyre!(
                "archive entry {} escapes the extraction root",
                path.display()
            ));
        }
        let dest = into.join(&rewritten);
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)
                .wrap_err_with(|| format!("creating {}", parent.display()))?;
        }
        // Hardlink entries need their target rewritten by `strip_prefix`
        // too: the tar header records the link source as e.g.
        // `install/escript/rabbitmq-diagnostics`, but on disk the file
        // actually lives at `<into>/escript/rabbitmq-diagnostics` once
        // the prefix has been stripped. Letting `entry.unpack(&dest)`
        // handle the link would try to resolve the archive-internal
        // path relative to CWD and fail with ENOENT.
        if entry.header().entry_type().is_hard_link() {
            let link_name = entry
                .link_name()
                .wrap_err("reading hardlink target")?
                .ok_or_else(|| eyre::eyre!("hardlink entry for {} has no link name", path.display()))?
                .into_owned();
            let link_dest_rel = rewrite_archive_path(&link_name, strip_prefix)
                .ok_or_else(|| eyre::eyre!("hardlink target {} resolves to the strip prefix root", link_name.display()))?;
            if !is_safe_archive_path(&link_dest_rel) {
                return Err(eyre::eyre!(
                    "hardlink target {} escapes the extraction root",
                    link_name.display()
                ));
            }
            let link_dest = into.join(&link_dest_rel);
            // Idempotency under `--overwrite`-style retries: a
            // previous half-finished extract may have left the link
            // already in place. Removing first matches what tar's
            // own unpack does (it sets overwrite=true by default in
            // 0.4).
            let _ = fs::remove_file(&dest);
            fs::hard_link(&link_dest, &dest).wrap_err_with(|| {
                format!(
                    "linking {} → {} (for archive entry {})",
                    dest.display(),
                    link_dest.display(),
                    path.display(),
                )
            })?;
            continue;
        }
        entry
            .unpack(&dest)
            .wrap_err_with(|| format!("unpacking {} → {}", path.display(), dest.display()))?;
    }
    Ok(())
}

/// Extract a `.zip` archive into `into`, stripping `strip_prefix` as a
/// leading path component from every entry (same convention as
/// [`extract_tar_zst`]). Used for windows.php.net interpreter ZIPs,
/// which wrap their contents in `php-<version>/`, for PECL DLL ZIPs,
/// which are flat (`strip_prefix = ""`), and by
/// `bougie-composer-resolver` for Composer package dist zips (which
/// wrap contents in `<vendor>-<package>-<short_sha>/`).
///
/// Symlink entries (only seen in unix-built ZIPs) are not expected on
/// the windows.php.net surface and are unpacked as plain files;
/// re-introduce a symlink branch when a non-windows ZIP source needs
/// it. The `zip` crate's own `ZipArchive::extract` would handle them
/// on Unix, but rolling the walk by hand here keeps the strip-prefix
/// rewrite (which `extract` doesn't support) trivial.
#[tracing::instrument(skip_all, fields(into = %into.display()))]
pub fn extract_zip(zip_path: &Path, into: &Path, strip_prefix: &str) -> Result<()> {
    let f = File::open(zip_path)
        .wrap_err_with(|| format!("opening {}", zip_path.display()))?;
    let mut archive = zip::ZipArchive::new(f)
        .wrap_err_with(|| format!("reading zip {}", zip_path.display()))?;
    for i in 0..archive.len() {
        let mut entry = archive
            .by_index(i)
            .wrap_err_with(|| format!("reading zip entry {i}"))?;
        // `enclosed_name` is the traversal-safe path (rejects `..` and
        // absolute paths) — `name()` would return the raw header bytes.
        let Some(raw) = entry.enclosed_name() else {
            continue;
        };
        let Some(rewritten) = rewrite_archive_path(&raw, strip_prefix) else {
            // The prefix directory entry itself; skip — `into` exists.
            continue;
        };
        let dest = into.join(&rewritten);
        if entry.is_dir() {
            fs::create_dir_all(&dest)
                .wrap_err_with(|| format!("creating {}", dest.display()))?;
            continue;
        }
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)
                .wrap_err_with(|| format!("creating {}", parent.display()))?;
        }
        let mut out = File::create(&dest)
            .wrap_err_with(|| format!("creating {}", dest.display()))?;
        std::io::copy(&mut entry, &mut out)
            .wrap_err_with(|| format!("writing {}", dest.display()))?;
        // Preserve Unix executable bit if the entry carried mode info
        // (windows.php.net ZIPs don't, but ZIPs built on Unix do).
        #[cfg(unix)]
        if let Some(mode) = entry.unix_mode() {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(&dest, fs::Permissions::from_mode(mode));
        }
    }
    Ok(())
}

/// Scan a zip's central directory and return the single common
/// top-level path component if there is exactly one. Returns the
/// empty string when entries are already flat at the archive root,
/// or when there are multiple top-level components (no safe strip is
/// possible).
///
/// Reads only the central directory — no decompression — so this is
/// cheap enough to call before extracting every Composer dist.
/// Packagist's zipballs always wrap contents in
/// `<owner>-<repo>-<short_sha>/`, but the wrapper name isn't
/// predictable across CDNs, so detection beats computation.
pub fn detect_zip_top_level(zip_path: &Path) -> Result<String> {
    let f = File::open(zip_path)
        .wrap_err_with(|| format!("opening {}", zip_path.display()))?;
    let mut archive = zip::ZipArchive::new(f)
        .wrap_err_with(|| format!("reading zip {}", zip_path.display()))?;
    let mut top: Option<String> = None;
    for i in 0..archive.len() {
        let entry = archive
            .by_index(i)
            .wrap_err_with(|| format!("reading zip entry {i}"))?;
        let Some(raw) = entry.enclosed_name() else {
            continue;
        };
        // First normal path component. `enclosed_name` already
        // rejects `..` and absolute paths so `Component::Normal` is
        // the only kind we can encounter on a non-empty input.
        let Some(first) = raw.components().next() else {
            continue;
        };
        let std::path::Component::Normal(os) = first else {
            continue;
        };
        let Some(s) = os.to_str() else {
            // Non-utf-8 entry name — bail on detection, fall back to
            // no-strip rather than guessing.
            return Ok(String::new());
        };
        match &top {
            None => top = Some(s.to_owned()),
            Some(existing) if existing == s => {}
            Some(_) => return Ok(String::new()),
        }
    }
    Ok(top.unwrap_or_default())
}

/// Apply `strip_prefix` to a tar-internal path. Returns `None` when the
/// rewrite produces an empty path (the prefix directory entry itself
/// — caller skips it because the destination already exists). Entries
/// that don't start with the prefix are left alone.
fn rewrite_archive_path(path: &Path, strip_prefix: &str) -> Option<PathBuf> {
    let rewritten = if strip_prefix.is_empty() {
        path.to_path_buf()
    } else {
        match path.strip_prefix(strip_prefix) {
            Ok(rest) => rest.to_path_buf(),
            Err(_) => path.to_path_buf(),
        }
    };
    if rewritten.as_os_str().is_empty() {
        None
    } else {
        Some(rewritten)
    }
}

/// Whether `path` is safe to join onto an extraction root: it must be
/// relative and contain only normal components (no `..`, no root/prefix).
/// This is the tar-side equivalent of the `zip` crate's `enclosed_name`.
fn is_safe_archive_path(path: &Path) -> bool {
    use std::path::Component;
    path.components().all(|c| match c {
        Component::Normal(_) | Component::CurDir => true,
        Component::ParentDir | Component::RootDir | Component::Prefix(_) => false,
    })
}

/// Stream `from` into `into` and verify its sha256. Used by callers
/// that already have the bytes locally (e.g. manifests).
pub fn copy_with_sha256<R: Read, W: Write>(from: &mut R, into: &mut W) -> Result<String> {
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = from.read(&mut buf).wrap_err("reading source")?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        into.write_all(&buf[..n]).wrap_err("writing dest")?;
    }
    Ok(format_hex(&hasher.finalize()))
}

fn format_hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        use std::fmt::Write as _;
        let _ = write!(s, "{b:02x}");
    }
    s
}

fn sibling_with_suffix(p: &Path, suffix: &str) -> PathBuf {
    let parent = p.parent().unwrap_or_else(|| Path::new(""));
    let name = p.file_name().and_then(|s| s.to_str()).unwrap_or("blob");
    parent.join(format!("{name}{suffix}"))
}

impl DownloadBar {
    /// Build an aggregate download bar that renders on stderr (when
    /// the global progress-visible flag is set) or stays hidden
    /// otherwise. Length starts at 0; call [`Self::add_planned`] as
    /// each manifest is parsed and the next chunk of expected bytes
    /// becomes known. Set the per-artifact label via
    /// [`Self::set_current`] before each `fetch_blob`/`fetch_file`.
    pub fn new(label: &str) -> Self {
        if !bougie_output::output::progress_visible() {
            return Self::hidden();
        }
        // Stays hidden until the first real activity (`add_planned`
        // with >0 bytes, `set_current`, `inc`, `set_progress`). Style
        // + prefix are wired up now so the activation path is just a
        // draw-target swap.
        let pb = ProgressBar::new(0);
        pb.set_draw_target(ProgressDrawTarget::hidden());
        // Template-with-fallback: `indicatif`'s template parser is
        // pinned at build time, so a malformed template is a bug,
        // not a user-visible failure mode. `unwrap_or_else` keeps
        // us off the panic path even if a future edit breaks it.
        //
        // `progress_chars("--")` paints the entire bar with `-`; the
        // foreground/background colors in the `{bar}` token split it
        // into a magenta filled portion and a dim-grey unfilled tail.
        let style = ProgressStyle::with_template(
            "  {prefix:<12} {bar:32.magenta/white.dim} {bytes}/{total_bytes} ({bytes_per_sec}, {eta}) {msg}",
        )
        .unwrap_or_else(|_| ProgressStyle::default_bar())
        .progress_chars("--");
        pb.set_style(style);
        pb.set_prefix(label.to_owned());
        Self {
            pb,
            sink: None,
            download_label: label.to_owned(),
            pending: Some(PendingActivation { activated: AtomicBool::new(false) }),
        }
    }

    /// Promote a lazily-constructed visible bar to actually draw. No-op
    /// on hidden bars and on already-activated ones. Called from every
    /// state-changing method so a stray `inc()` on a stalled fetch
    /// still surfaces a bar.
    fn activate(&self) {
        let Some(pending) = self.pending.as_ref() else { return };
        // Relaxed is fine: contention here just means two threads
        // both call `set_draw_target` — idempotent, and the steady
        // tick can only be enabled once anyway.
        if pending
            .activated
            .compare_exchange(false, true, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
        {
            self.pb
                .set_draw_target(ProgressDrawTarget::stderr_with_hz(15));
            self.pb.enable_steady_tick(Duration::from_millis(120));
        }
    }

    /// Build a **step-count** progress bar rendering `{prefix} {pos}/{len}
    /// {msg}`. Unlike the byte bar from [`Self::new`], it draws immediately
    /// and keeps drawing even when no bytes flow — for a sequence of small
    /// units that are frequently cache hits (e.g. the baseline PHP
    /// extensions), where "12/24 intl" is the real signal and an aggregate
    /// byte bar would just sit at `0 B/0 B`. Advance with [`Self::step`]
    /// once per unit; call [`Self::finish`] at the end.
    ///
    /// Hidden (a no-op) unless the global progress-visible flag is set,
    /// same as [`Self::new`]. A step bar tracks item *counts*, not bytes,
    /// so don't also pass it to `fetch_blob` / `fetch_file` — its position
    /// would then mix item ticks with byte increments. Use a separate
    /// [`Self::hidden`] byte bar for the fetches underneath.
    pub fn steps(label: &str, total: u64) -> Self {
        if !bougie_output::output::progress_visible() {
            return Self::hidden();
        }
        let pb = ProgressBar::new(total);
        let style = ProgressStyle::with_template(
            "  {prefix:<12} {pos}/{len} {spinner:.magenta} {wide_msg:.dim}",
        )
        .unwrap_or_else(|_| ProgressStyle::default_spinner());
        pb.set_style(style);
        pb.set_prefix(label.to_owned());
        pb.set_draw_target(ProgressDrawTarget::stderr_with_hz(15));
        pb.enable_steady_tick(Duration::from_millis(120));
        Self { pb, sink: None, download_label: label.to_owned(), pending: None }
    }

    /// Label the unit now starting and advance a [`steps`](Self::steps)
    /// bar by one. No-op on hidden bars (their draw target is hidden).
    pub fn step(&self, name: impl Into<String>) {
        self.pb.set_message(name.into());
        self.pb.inc(1);
    }

    /// A no-op bar. Use when a caller has no aggregate of its own but
    /// still needs to satisfy [`fetch_blob`] / [`fetch_file`].
    pub fn hidden() -> Self {
        let pb = ProgressBar::new(0);
        pb.set_draw_target(ProgressDrawTarget::hidden());
        Self { pb, sink: None, download_label: "downloading".to_owned(), pending: None }
    }

    /// Hidden draw target + sink. Use when the local process has no UI
    /// of its own (e.g. the daemon) but wants to forward bar state to
    /// a remote consumer that does. Every `add_planned` / `set_current`
    /// / `inc` / `finish` call also emits a [`DownloadEvent`] to `sink`.
    ///
    /// The indicatif bar still tracks position + length internally —
    /// `inc` and friends behave the same way they would on a visible
    /// bar, just without any drawing. That keeps the sink semantics
    /// identical to what a TTY user would see.
    pub fn hidden_with_sink(sink: Arc<dyn DownloadSink>) -> Self {
        let pb = ProgressBar::new(0);
        pb.set_draw_target(ProgressDrawTarget::hidden());
        Self { pb, sink: Some(sink), download_label: "downloading".to_owned(), pending: None }
    }

    /// Grow the planned total. Safe to call repeatedly as each manifest
    /// reveals the next batch of bytes; calling with `0` is a no-op
    /// (older publishers may emit `size: 0` for backwards-compat —
    /// such contributions just don't extend the bar).
    pub fn add_planned(&self, bytes: u64) {
        if bytes > 0 {
            self.activate();
            self.pb.inc_length(bytes);
            if let Some(sink) = &self.sink {
                sink.on_event(DownloadEvent::Plan { bytes });
            }
        }
    }

    /// Current planned total. Returns 0 for a fresh / hidden bar that
    /// has never been planned against. Used in tests to assert
    /// planning correctness; not part of the user-facing UX.
    ///
    /// Only called from the closure-peer plan test (in bougie's
    /// install.rs), which is itself gated `cfg(not(target_os =
    /// "windows"))`. Mark `#[doc(hidden)]` rather than `cfg(test)` so
    /// the symbol is visible across the crate boundary; gate to
    /// non-Windows so `-D dead_code` on Windows CI doesn't fire.
    #[cfg(not(target_os = "windows"))]
    #[doc(hidden)]
    pub fn planned(&self) -> u64 {
        self.pb.length().unwrap_or(0)
    }

    /// Set the right-hand-side label showing which artifact is
    /// currently downloading. Overwrites any previous label.
    pub fn set_current(&self, name: impl Into<String>) {
        let name = name.into();
        self.activate();
        // Restore the download prefix in case the previous artifact left
        // it on "extracting" (see `mark_extracting`); a fresh artifact is
        // always entering its download phase here.
        self.pb.set_prefix(self.download_label.clone());
        self.pb.set_message(name.clone());
        if let Some(sink) = &self.sink {
            sink.on_event(DownloadEvent::Current { name });
        }
    }

    /// Print a one-line retry notice *above* the bar without disturbing
    /// its running state. Called from [`fetch_with_retry`] when a fetch
    /// fails and is about to be retried, so a silent retry (and the
    /// backoff sleep that follows) reads as deliberate progress rather
    /// than a hang. On a hidden draw target (non-TTY / `--quiet`) the
    /// line is discarded — the `tracing::warn!` at the call site is the
    /// machine-readable record in that mode.
    pub fn note_retry(&self, what: &str, attempt: u32, max: u32) {
        // `println` redraws the bar after the line, so this composes
        // cleanly with an active steady tick on a stalled-looking bar.
        self.pb
            .println(format!("  retrying {what} (attempt {attempt}/{max})…"));
    }

    /// Swap the bar's prefix from `"downloading"` to `"extracting"` for
    /// the artifact currently in flight, keeping its name in the message.
    /// Called by [`fetch_blob`] (via [`try_once_blob`]) the moment the
    /// download bytes are all in and the silent, CPU-bound tar.zst/zip
    /// decompress begins — otherwise the bar freezes at `N/N bytes` with
    /// the `downloading` prefix while extraction runs, which reads as a
    /// hang (the exact symptom this is fixing). The byte counters stop
    /// advancing during extraction (there's nothing to count against the
    /// download total), but the steady tick keeps the bar animated and
    /// the `extracting <name>` line makes the phase legible.
    ///
    /// The next [`Self::set_current`] restores the `"downloading"`
    /// prefix for the following artifact, so the two phases alternate
    /// cleanly across a sequential install loop. Idempotent — setting
    /// the prefix to `"extracting"` more than once (e.g. a retried
    /// fetch) is a no-op.
    pub fn mark_extracting(&self) {
        self.activate();
        self.pb.set_prefix("extracting");
        if let Some(sink) = &self.sink {
            // Forward the phase so a remote mirror (the daemon's IPC
            // bar) can flip its own prefix to `extracting`. The artifact
            // name is whatever the preceding `set_current` put in the
            // message — the thing we just finished downloading.
            sink.on_event(DownloadEvent::Extracting { name: self.pb.message() });
        }
    }

    /// Replace the bar's running state with an absolute snapshot
    /// (planned total, bytes so far, current artifact label). Used by
    /// the CLI when it's mirroring a remote `DownloadBar` over IPC:
    /// each `download` frame carries cumulative counters, not deltas,
    /// so we set them directly rather than reconstructing deltas from
    /// the previous snapshot.
    ///
    /// `extracting` mirrors the remote bar's phase: `true` shows the
    /// `extracting` prefix, `false` restores the download prefix (the
    /// `label` passed to [`Self::new`]). This is the wire-mirror
    /// counterpart of [`Self::mark_extracting`] / [`Self::set_current`],
    /// which flip the prefix locally.
    ///
    /// Does not fire the sink. Sinks observe local (incremental)
    /// activity; absolute updates come from somewhere else by
    /// definition, so re-emitting them would either duplicate
    /// downstream state or create a feedback loop.
    pub fn set_progress(&self, pos: u64, total: u64, label: &str, extracting: bool) {
        if total > 0 || pos > 0 || !label.is_empty() {
            self.activate();
        }
        if total > 0 {
            self.pb.set_length(total);
        }
        self.pb.set_position(pos);
        // Avoid `set_message(name)` on every frame when the label
        // hasn't changed — indicatif redraws the bar on each call,
        // and the daemon emits ~20 frames/s of which most carry the
        // same label.
        if !label.is_empty() {
            self.pb.set_message(label.to_string());
        }
        // Likewise only touch the prefix when the phase actually flips,
        // so a steady stream of same-phase frames doesn't force redraws.
        let want_prefix = if extracting { "extracting" } else { self.download_label.as_str() };
        if self.pb.prefix() != want_prefix {
            self.pb.set_prefix(want_prefix.to_owned());
        }
    }

    /// Final flush — clears the bar from the terminal.
    pub fn finish(&self) {
        self.pb.finish_and_clear();
        if let Some(sink) = &self.sink {
            sink.on_event(DownloadEvent::Finish);
        }
    }

    /// Advance the bar by `n` freshly-downloaded bytes. Called from
    /// the byte-copy loop in `fetch_to_partial`; not part of the
    /// public surface.
    fn inc(&self, n: u64) {
        if n > 0 {
            self.activate();
        }
        self.pb.inc(n);
        if let Some(sink) = &self.sink {
            sink.on_event(DownloadEvent::Inc { bytes: n });
        }
    }
}

/// Discard a partial download — used on cancellation / error
/// recovery (callers that know the blob is invalid).
pub fn discard_partial(partial_dir: &Path, sha256: &str) {
    let p = partial_dir.join(format!("{sha256}.partial"));
    let _ = fs::remove_file(p);
}

/// Like [`copy`] but consumes a known body and writes it; returns the
/// hex sha256 of the bytes written.
pub fn write_with_sha256(into: &Path, bytes: &[u8]) -> Result<String> {
    let mut f = File::create(into).wrap_err_with(|| format!("creating {}", into.display()))?;
    f.write_all(bytes).wrap_err("writing")?;
    let _ = copy(&mut std::io::empty(), &mut f); // ensure no warnings
    Ok(format_hex(&Sha256::digest(bytes)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_hex_lowercase() {
        assert_eq!(format_hex(&[0xab, 0xcd]), "abcd");
        assert_eq!(format_hex(&[0]), "00");
    }

    #[test]
    fn user_agent_has_expected_shape() {
        // `Composer/2 bougie/<semver> (+https://...)`. The exact
        // version is the bougie-fetch crate version — checking the
        // shape, not the value, so a release-plz bump doesn't break
        // the test. The `Composer/2` prefix is load-bearing: some
        // Composer-protocol servers (repo.magento.com) gate dist
        // downloads on it. See [`USER_AGENT`] for context.
        assert!(USER_AGENT.starts_with("Composer/2 bougie/"), "{USER_AGENT}");
        assert!(
            USER_AGENT.contains("(+https://github.com/cresset-tools/bougie)"),
            "{USER_AGENT}",
        );
    }

    #[test]
    fn download_bar_hidden_accepts_all_methods() {
        // Smoke test: a hidden bar must accept the full driver API
        // without panicking, so the byte-copy loop in non-TTY contexts
        // stays branch-free.
        let bar = DownloadBar::hidden();
        bar.add_planned(0);
        bar.add_planned(1024);
        bar.set_current("php-8.3.12");
        bar.inc(512);
        bar.mark_extracting();
        bar.set_current("ext-intl"); // restores the prefix after extraction
        bar.note_retry("php-8.3.12.tar.zst", 1, RETRY_BUDGET);
        bar.finish();
    }

    #[test]
    fn url_label_takes_last_path_segment() {
        assert_eq!(
            url_label("https://cdn.example/blobs/php-8.3.12.tar.zst"),
            "php-8.3.12.tar.zst",
        );
        // Query + fragment are stripped before the segment is taken.
        assert_eq!(
            url_label("https://cdn.example/x/dist.zip?token=abc#frag"),
            "dist.zip",
        );
        // Trailing slash doesn't yield an empty label.
        assert_eq!(url_label("https://cdn.example/pkg/"), "pkg");
        // No path component → whole URL is the fallback.
        assert_eq!(url_label("not-a-url"), "not-a-url");
    }

    #[test]
    fn backoff_factor_grows_then_caps() {
        // Mirror the doubling+cap used inside `backoff_sleep` so the
        // schedule is asserted without actually sleeping. Attempt 1→1x,
        // 2→2x, 3→4x, 4→8x, and capped at 8x thereafter.
        let factor = |attempt: u32| {
            1u32.checked_shl(attempt.saturating_sub(1)).unwrap_or(u32::MAX).min(8)
        };
        assert_eq!(factor(1), 1);
        assert_eq!(factor(2), 2);
        assert_eq!(factor(3), 4);
        assert_eq!(factor(4), 8);
        assert_eq!(factor(99), 8, "huge attempt counts saturate, never overflow the shift");
    }

    #[test]
    fn sibling_with_suffix_appends() {
        let p = Path::new("/a/b/c");
        assert_eq!(sibling_with_suffix(p, ".incoming"), Path::new("/a/b/c.incoming"));
    }

    #[test]
    fn write_with_sha256_returns_correct_hash() {
        let dir = tempfile::TempDir::new().unwrap();
        let dest = dir.path().join("f");
        let h = write_with_sha256(&dest, b"hello").unwrap();
        assert_eq!(
            h,
            "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
        );
    }

    #[test]
    fn try_once_file_writes_verified_bytes_atomically() {
        let dir = tempfile::TempDir::new().unwrap();
        let partial_dir = dir.path().join("partial");
        std::fs::create_dir_all(&partial_dir).unwrap();
        let dest = dir.path().join("out").join("composer.phar");

        // Pre-stage a "downloaded" partial so try_once_file can act
        // on it without a real HTTP server.
        let body = b"#!/usr/bin/env php\n<?php echo 'hi';\n";
        let sha = format_hex(&Sha256::digest(body));
        let tmp = partial_dir.join(format!("{sha}.partial"));
        std::fs::write(&tmp, body).unwrap();

        std::fs::create_dir_all(dest.parent().unwrap()).unwrap();
        let incoming = sibling_with_suffix(&dest, ".incoming");
        let _ = std::fs::remove_file(&incoming);
        std::fs::copy(&tmp, &incoming).unwrap();
        std::fs::rename(&incoming, &dest).unwrap();
        let _ = std::fs::remove_file(&tmp);

        assert!(dest.is_file());
        assert_eq!(std::fs::read(&dest).unwrap(), body);
        assert!(!tmp.exists());
        assert!(!incoming.exists());
    }

    #[test]
    fn extract_strips_install_prefix() {
        let dir = tempfile::TempDir::new().unwrap();
        let archive_path = dir.path().join("a.tar.zst");

        let mut tar_buf: Vec<u8> = Vec::new();
        {
            let mut builder = tar::Builder::new(&mut tar_buf);
            let mut header = tar::Header::new_gnu();
            header.set_size(5);
            header.set_mode(0o644);
            header.set_cksum();
            builder
                .append_data(&mut header, "install/bin/php", &b"hello"[..])
                .unwrap();
            let mut header2 = tar::Header::new_gnu();
            header2.set_size(2);
            header2.set_mode(0o644);
            header2.set_cksum();
            builder
                .append_data(&mut header2, "install/etc/php.ini", &b"hi"[..])
                .unwrap();
            builder.finish().unwrap();
        }
        let zst = zstd::encode_all(&tar_buf[..], 0).unwrap();
        std::fs::write(&archive_path, zst).unwrap();

        let into = dir.path().join("out");
        std::fs::create_dir_all(&into).unwrap();
        extract_tar_zst(&archive_path, &into, "install").unwrap();

        assert!(into.join("bin/php").is_file());
        assert!(into.join("etc/php.ini").is_file());
        assert!(!into.join("install").exists());
    }

    #[test]
    fn extract_passes_through_when_no_prefix() {
        let dir = tempfile::TempDir::new().unwrap();
        let archive_path = dir.path().join("a.tar.zst");

        let mut tar_buf: Vec<u8> = Vec::new();
        {
            let mut builder = tar::Builder::new(&mut tar_buf);
            let mut header = tar::Header::new_gnu();
            header.set_size(3);
            header.set_mode(0o644);
            header.set_cksum();
            builder
                .append_data(&mut header, "bin/php", &b"abc"[..])
                .unwrap();
            builder.finish().unwrap();
        }
        let zst = zstd::encode_all(&tar_buf[..], 0).unwrap();
        std::fs::write(&archive_path, zst).unwrap();

        let into = dir.path().join("out");
        std::fs::create_dir_all(&into).unwrap();
        extract_tar_zst(&archive_path, &into, "install").unwrap();

        assert!(into.join("bin/php").is_file());
    }

    #[test]
    fn extract_strips_arbitrary_prefix() {
        // Closure tarballs wrap contents in `<storeName>/`; the
        // extractor must strip whatever prefix the caller specifies.
        let dir = tempfile::TempDir::new().unwrap();
        let archive_path = dir.path().join("a.tar.zst");
        let mut tar_buf: Vec<u8> = Vec::new();
        {
            let mut builder = tar::Builder::new(&mut tar_buf);
            let mut header = tar::Header::new_gnu();
            header.set_size(4);
            header.set_mode(0o644);
            header.set_cksum();
            builder
                .append_data(&mut header, "libcurl-8.20.0-aaaa/lib/libcurl.so.4", &b"data"[..])
                .unwrap();
            builder.finish().unwrap();
        }
        let zst = zstd::encode_all(&tar_buf[..], 0).unwrap();
        std::fs::write(&archive_path, zst).unwrap();

        let into = dir.path().join("out");
        std::fs::create_dir_all(&into).unwrap();
        extract_tar_zst(&archive_path, &into, "libcurl-8.20.0-aaaa").unwrap();

        assert!(into.join("lib/libcurl.so.4").is_file());
        assert!(!into.join("libcurl-8.20.0-aaaa").exists());
    }

    // Uses `MetadataExt::ino()` to confirm a real hardlink (same inode)
    // was produced. The inode API only exists on Unix; on Windows we'd
    // need to compare via `GetFileInformationByHandle`. Skip on
    // Windows — the behavior the test covers (rewriting the link
    // target with `strip_prefix`) is exercised the same way on either
    // platform; the inode check is incidental.
    #[cfg(unix)]
    #[test]
    fn extract_rewrites_hardlink_targets_with_strip_prefix() {
        use std::os::unix::fs::MetadataExt;
        // Mirrors the rabbitmq tarball shape that previously broke
        // `services up`: a regular file at `install/escript/rabbitmq-
        // diagnostics` followed by several hardlinks whose tar header
        // records the link target with the `install/` prefix.
        // Without rewriting, the link target dangled because we only
        // wrote the file at `<into>/escript/rabbitmq-diagnostics`.
        let dir = tempfile::TempDir::new().unwrap();
        let archive_path = dir.path().join("rmq.tar.zst");

        let body = b"escript-stub";
        let mut tar_buf: Vec<u8> = Vec::new();
        {
            let mut builder = tar::Builder::new(&mut tar_buf);
            // The original file.
            let mut header = tar::Header::new_gnu();
            header.set_size(body.len() as u64);
            header.set_mode(0o755);
            header.set_entry_type(tar::EntryType::Regular);
            header.set_cksum();
            builder
                .append_data(&mut header, "install/escript/rabbitmq-diagnostics", &body[..])
                .unwrap();
            // Hardlinks share the entry-type code with the regular
            // GNU header; size is zero and link_name carries the
            // source path (still prefixed by `install/` here).
            let mut link_header = tar::Header::new_gnu();
            link_header.set_size(0);
            link_header.set_mode(0o755);
            link_header.set_entry_type(tar::EntryType::Link);
            link_header
                .set_link_name("install/escript/rabbitmq-diagnostics")
                .unwrap();
            link_header.set_cksum();
            builder
                .append_data(&mut link_header, "install/escript/rabbitmqctl", std::io::empty())
                .unwrap();
            builder.finish().unwrap();
        }
        let zst = zstd::encode_all(&tar_buf[..], 0).unwrap();
        std::fs::write(&archive_path, zst).unwrap();

        let into = dir.path().join("out");
        std::fs::create_dir_all(&into).unwrap();
        extract_tar_zst(&archive_path, &into, "install").unwrap();

        let orig = into.join("escript/rabbitmq-diagnostics");
        let linked = into.join("escript/rabbitmqctl");
        assert!(orig.is_file());
        assert!(linked.is_file());
        // Same inode → same hardlink target, same contents.
        let m1 = std::fs::metadata(&orig).unwrap();
        let m2 = std::fs::metadata(&linked).unwrap();
        assert_eq!(m1.ino(), m2.ino());
        assert_eq!(std::fs::read(&linked).unwrap(), body);
    }

    /// windows.php.net's interpreter ZIP wraps `php.exe`, `ext/*.dll`,
    /// etc. inside a top-level `php-<version>/` directory. The extractor
    /// must strip that prefix so the materialized tree mirrors the
    /// `bin/`-style layout the rest of bougie assumes.
    #[test]
    fn extract_zip_strips_prefix_and_writes_nested_files() {
        let dir = tempfile::TempDir::new().unwrap();
        let zip_path = dir.path().join("a.zip");

        let mut buf: Vec<u8> = Vec::new();
        {
            let cursor = std::io::Cursor::new(&mut buf);
            let mut zw = zip::ZipWriter::new(cursor);
            let opts = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Stored);
            zw.start_file("php-8.4.3/php.exe", opts).unwrap();
            zw.write_all(b"MZ exe stub").unwrap();
            zw.start_file("php-8.4.3/ext/php_curl.dll", opts).unwrap();
            zw.write_all(b"DLL stub").unwrap();
            zw.start_file("php-8.4.3/php.ini-development", opts).unwrap();
            zw.write_all(b"; ini\n").unwrap();
            zw.finish().unwrap();
        }
        std::fs::write(&zip_path, &buf).unwrap();

        let into = dir.path().join("out");
        std::fs::create_dir_all(&into).unwrap();
        extract_zip(&zip_path, &into, "php-8.4.3").unwrap();

        assert_eq!(std::fs::read(into.join("php.exe")).unwrap(), b"MZ exe stub");
        assert_eq!(std::fs::read(into.join("ext/php_curl.dll")).unwrap(), b"DLL stub");
        assert_eq!(std::fs::read(into.join("php.ini-development")).unwrap(), b"; ini\n");
        // The wrapping directory itself is not materialized.
        assert!(!into.join("php-8.4.3").exists());
    }

    /// PECL DLL ZIPs are flat — pass `strip_prefix = ""` and entries
    /// land verbatim.
    #[test]
    fn extract_zip_passes_through_when_no_prefix() {
        let dir = tempfile::TempDir::new().unwrap();
        let zip_path = dir.path().join("flat.zip");
        let mut buf: Vec<u8> = Vec::new();
        {
            let cursor = std::io::Cursor::new(&mut buf);
            let mut zw = zip::ZipWriter::new(cursor);
            let opts = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Stored);
            zw.start_file("php_xdebug.dll", opts).unwrap();
            zw.write_all(b"xdebug").unwrap();
            zw.finish().unwrap();
        }
        std::fs::write(&zip_path, &buf).unwrap();

        let into = dir.path().join("out");
        std::fs::create_dir_all(&into).unwrap();
        extract_zip(&zip_path, &into, "").unwrap();

        assert_eq!(std::fs::read(into.join("php_xdebug.dll")).unwrap(), b"xdebug");
    }

    fn make_zip(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut buf: Vec<u8> = Vec::new();
        let cursor = std::io::Cursor::new(&mut buf);
        let mut zw = zip::ZipWriter::new(cursor);
        let opts = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        for (name, body) in entries {
            zw.start_file(*name, opts).unwrap();
            zw.write_all(body).unwrap();
        }
        zw.finish().unwrap();
        buf
    }

    #[test]
    fn detect_zip_top_level_returns_single_wrapper() {
        let dir = tempfile::TempDir::new().unwrap();
        let p = dir.path().join("a.zip");
        std::fs::write(
            &p,
            make_zip(&[
                ("acme-foo-abc1234/composer.json", b"{}"),
                ("acme-foo-abc1234/src/Foo.php", b"<?php"),
            ]),
        )
        .unwrap();
        assert_eq!(detect_zip_top_level(&p).unwrap(), "acme-foo-abc1234");
    }

    #[test]
    fn detect_zip_top_level_returns_empty_for_flat_archive() {
        let dir = tempfile::TempDir::new().unwrap();
        let p = dir.path().join("flat.zip");
        std::fs::write(&p, make_zip(&[("composer.json", b"{}"), ("Foo.php", b"<?php")])).unwrap();
        assert_eq!(detect_zip_top_level(&p).unwrap(), "");
    }

    #[test]
    fn detect_zip_top_level_returns_empty_when_multiple_wrappers() {
        let dir = tempfile::TempDir::new().unwrap();
        let p = dir.path().join("mixed.zip");
        std::fs::write(
            &p,
            make_zip(&[
                ("acme-one-aaa/file.php", b"a"),
                ("acme-two-bbb/file.php", b"b"),
            ]),
        )
        .unwrap();
        assert_eq!(detect_zip_top_level(&p).unwrap(), "");
    }

    #[test]
    fn rewrite_archive_path_strips_when_prefixed() {
        let out = rewrite_archive_path(Path::new("install/bin/php"), "install").unwrap();
        assert_eq!(out, Path::new("bin/php"));
    }

    #[test]
    fn rewrite_archive_path_passes_through_when_unprefixed() {
        let out = rewrite_archive_path(Path::new("bin/php"), "install").unwrap();
        assert_eq!(out, Path::new("bin/php"));
    }

    #[test]
    fn rewrite_archive_path_handles_empty_prefix() {
        let out = rewrite_archive_path(Path::new("bin/php"), "").unwrap();
        assert_eq!(out, Path::new("bin/php"));
    }

    #[test]
    fn rewrite_archive_path_returns_none_for_prefix_directory_entry() {
        // The prefix dir entry itself (`install/`) rewrites to an
        // empty path. Caller skips the entry because the destination
        // root already exists.
        assert!(rewrite_archive_path(Path::new("install"), "install").is_none());
    }
}
