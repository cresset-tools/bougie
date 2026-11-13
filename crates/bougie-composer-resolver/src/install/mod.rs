//! Apply `composer.lock` to a project with [`composer_install`], wired to
//! bougie's own seams: dists download through `bougie-fetch` into
//! `$BOUGIE_CACHE/composer-dist`, git sources clone into
//! `$BOUGIE_CACHE/composer-vcs`, `bougie login` credentials join the auth
//! map, progress renders on bougie's package bar, and the installer's typed
//! errors become [`BougieError`]s with bougie's hints and exit codes.

use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

use bougie_errors::BougieError;
use bougie_fetch::{ArchiveKind, DownloadBar, Hash};
use bougie_paths::Paths;
use composer_install::{
    DistOutcome, FetchSpec, Fetcher, GitSourceFetcher, InstallEnv, InstallError, Progress,
    VcsFailure,
};
use composer_patches::PatchPlan;
use eyre::Result;

pub use composer_install::{
    ArchiveKind as DistArchive, DistRequest, InstallOptions, InstallSummary, LinkMode, ScriptHooks,
};

use crate::metadata::AuthCredentials;

/// The autoloader freshness marker bougie has always written, kept so an
/// existing warm `vendor/` stays warm.
const AUTOLOAD_MARKER: &str = ".bougie-autoload-fresh";

/// Apply `composer.lock` to `project_root`.
///
/// # Errors
///
/// As [`composer_install::install_from_lock`], with typed failures mapped to
/// [`BougieError`].
pub fn install_from_lock(
    paths: &Paths,
    project_root: &Path,
    opts: InstallOptions,
    hooks: Option<&dyn ScriptHooks>,
) -> Result<InstallSummary> {
    install_from_lock_with_patches(paths, project_root, opts, hooks, None)
}

/// Like [`install_from_lock`], also applying a resolved [`PatchPlan`].
///
/// # Errors
///
/// As [`composer_install::install_from_lock_with_patches`], with typed
/// failures mapped to [`BougieError`].
pub fn install_from_lock_with_patches(
    paths: &Paths,
    project_root: &Path,
    opts: InstallOptions,
    hooks: Option<&dyn ScriptHooks>,
    patches: Option<&PatchPlan>,
) -> Result<InstallSummary> {
    let fetcher = BougieFetcher {
        client: bougie_fetch::default_client()?,
        bar: DownloadBar::hidden(),
    };
    let progress = PackageBar::new();
    let git = GitSourceFetcher::new(paths.cache_composer_vcs());
    let machine_auth: HashMap<String, composer_install::AuthCredentials> =
        crate::update::read_bougie_auth_json()
            .map_err(|e| eyre::eyre!(e))?
            .into_iter()
            .map(|(host, creds)| (host, to_composer_auth(creds)))
            .collect();
    let cache_root = paths.cache_composer_dist();
    let env = InstallEnv::new(&fetcher, &progress, &cache_root)
        .with_source_fetcher(&git)
        .with_machine_auth(&machine_auth)
        .with_autoload_marker(AUTOLOAD_MARKER);

    let result =
        composer_install::install_from_lock_with_patches(&env, project_root, opts, hooks, patches);
    progress.pb.finish_and_clear();
    result.map_err(to_bougie_error)
}

/// Download (if not cached) and extract `dists` into their destinations
/// with bougie's fetcher and dist cache, outside a full install.
///
/// # Errors
///
/// A download, verification or extraction failure.
pub fn fetch_and_extract_dists(paths: &Paths, dists: &[DistRequest<'_>]) -> Result<()> {
    let fetcher = BougieFetcher {
        client: bougie_fetch::default_client()?,
        bar: DownloadBar::hidden(),
    };
    composer_install::fetch_and_extract_dists(
        &fetcher,
        &paths.cache_composer_dist(),
        dists,
        &composer_install::NoProgress,
        LinkMode::Extract,
    )
    .map(|_| ())
}

/// Downloads dists with `bougie-fetch`, so they share bougie's HTTP client
/// (User-Agent, timeouts) and its retry policy.
struct BougieFetcher {
    client: reqwest::blocking::Client,
    bar: DownloadBar,
}

impl Fetcher for BougieFetcher {
    fn fetch(&self, spec: &FetchSpec<'_>) -> Result<()> {
        let blob = bougie_fetch::BlobSpec {
            url: spec.url,
            hash: Hash::sha1(spec.sha1),
            partial_dir: spec.partial_dir,
            dest: spec.dest,
            strip_prefix: "",
            // Ignored by `fetch_file`, which only downloads.
            archive: ArchiveKind::Zip,
            auth_header: spec.auth_header,
            auth_header_name: spec.auth_header_name,
        };
        // The byte bar is hidden here, so its own retry notice goes nowhere:
        // report the retry to the package bar instead.
        bougie_fetch::fetch_file_with(&self.client, &blob, &self.bar, &|attempt, error| {
            spec.progress.on_retry(spec.package_name, attempt, error);
        })
        .map(|_| ())
    }
}

/// bougie's "<done>/<total> packages" bar. Lockfiles carry no dist sizes, so
/// it counts packages: once through the downloads, then again through the
/// extraction.
struct PackageBar {
    pb: indicatif::ProgressBar,
    extracting: AtomicBool,
}

impl PackageBar {
    fn new() -> Self {
        let pb = if bougie_output::output::progress_visible() {
            let pb = indicatif::ProgressBar::new(0);
            pb.set_draw_target(indicatif::ProgressDrawTarget::stderr_with_hz(15));
            // No `{per_sec}`/`{eta}`: packages vary wildly in size, so a
            // per-package rate misleads and the eta jitters.
            let style = indicatif::ProgressStyle::with_template(
                "  {prefix:<12} {bar:32.magenta/white.dim} {pos}/{len} packages {msg}",
            )
            .unwrap_or_else(|_| indicatif::ProgressStyle::default_bar())
            .progress_chars("--");
            pb.set_style(style);
            pb.set_prefix("downloading");
            pb.enable_steady_tick(std::time::Duration::from_millis(120));
            pb
        } else {
            indicatif::ProgressBar::hidden()
        };
        Self {
            pb,
            extracting: AtomicBool::new(false),
        }
    }
}

impl Progress for PackageBar {
    fn on_start(&self, dists: usize) {
        self.pb.set_length(dists as u64);
    }

    fn on_download(&self, package: &str, _outcome: DistOutcome) {
        self.pb.set_message(package.to_owned());
        self.pb.inc(1);
    }

    fn on_extract(&self, package: &str) {
        // The first extraction resets the bar to count the second phase.
        if !self.extracting.swap(true, Ordering::AcqRel) {
            self.pb.set_prefix("extracting");
            self.pb.set_position(0);
        }
        self.pb.set_message(package.to_owned());
        self.pb.inc(1);
    }

    fn on_retry(&self, package: &str, attempt: u32, _error: &eyre::Report) {
        self.pb
            .println(format!("  retrying {package} (attempt {attempt})…"));
    }
}

fn to_composer_auth(creds: AuthCredentials) -> composer_install::AuthCredentials {
    use composer_install::AuthCredentials as C;
    match creds {
        AuthCredentials::Basic { username, password } => C::Basic { username, password },
        AuthCredentials::Bearer { token } => C::Bearer { token },
        AuthCredentials::GitHubToken { token } => C::GitHubToken { token },
        AuthCredentials::GitLabToken { token } => C::GitLabToken { token },
    }
}

/// Map the installer's typed failures to bougie's errors, keeping any
/// context the installer wrapped around them.
fn to_bougie_error(report: eyre::Report) -> eyre::Report {
    let Some(err) = report.downcast_ref::<InstallError>() else {
        return report;
    };
    let mapped: eyre::Report = match err {
        InstallError::MissingLock { path } => eyre::eyre!(
            "{} not found — run `bougie run -- composer update` first to generate it",
            path.display()
        ),
        InstallError::InvalidManifest { detail, .. } => BougieError::Config {
            path: "composer.json".into(),
            detail: detail.clone(),
        }
        .into(),
        InstallError::Vcs {
            failure,
            operation,
            url,
            detail,
        } => BougieError::Vcs {
            operation: operation.clone(),
            url: url.clone(),
            detail: detail.clone(),
            hint: vcs_hint(*failure).to_owned(),
        }
        .into(),
        _ => return report,
    };
    // Re-attach the outer context ("installing `acme/lib` from git source")
    // when the installer added one.
    let outer = report.to_string();
    if outer == err.to_string() {
        mapped
    } else {
        mapped.wrap_err(outer)
    }
}

fn vcs_hint(failure: VcsFailure) -> &'static str {
    match failure {
        VcsFailure::GitMissing => {
            "install git and make sure it's on PATH — bougie shells out to your system git \
             for VCS/source packages, or install this project's git dependencies with \
             upstream Composer"
        }
        VcsFailure::Auth => {
            "the repository is private or the credentials are wrong. bougie uses your system \
             git — configure a git credential helper or ssh key, or run `bougie login`"
        }
        VcsFailure::MissingRef => {
            "the requested commit/ref is not on the remote — the lock may point at an unpushed \
             or force-pushed commit; re-run `bougie update` to re-resolve"
        }
        VcsFailure::Unreachable => {
            "the repository could not be reached — check the URL and your network/access"
        }
        _ => "see git's output above",
    }
}
