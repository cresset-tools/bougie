//! `bougie db seed --from <path-or-url>` — load a jibs `.jibsdump` snapshot into
//! the project's mariadb tenant, giving a local database shaped like production.
//!
//! bougie doesn't bundle jibs. Like `bougie format` does for `wick`, this
//! downloads a *pinned* `jibs` binary on first use (same mirror→GitHub +
//! SHA-256 fetch as `bougie self update`), caches it, resolves the project's
//! mariadb tenant **offline** (the tenant ledger + derived password — exactly
//! what `bougie service credentials` reads), and execs
//! `jibs load <from> --local-mysql <tenant-dsn>`.
//!
//! `MariaDB` tenants are unix-socket only, so the DSN is
//! `mysql://<user>:<pw>@localhost/<db>?socket=<sock>` (the host is a placeholder
//! `jibs`'s `mysql::Opts::from_url` ignores once `socket=` is set).

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use bougie_cli::{DbSeedArgs, OutputFormat};
use bougie_daemon::daemon::credentials::derive_password;
use bougie_daemon::daemon::tenants;
use bougie_paths::Paths;
use bougie_platform::target::Triple;
use eyre::{Result, WrapErr, eyre};
use serde::{Deserialize, Serialize};

use super::super::native_fetch;
use super::super::service::config_mut::locate_project_root;

/// Default pinned jibs version. Must be ≥ 0.5.0 — the release that added
/// `jibs load <url>` — and must have published prebuilt binaries. Bump in
/// lockstep with the jibs release bougie should ship against.
const DEFAULT_JIBS_VERSION: &str = "0.5.0";
const JIBS_VERSION_ENV: &str = "BOUGIE_JIBS_VERSION";
/// Escape hatch: point at a prebuilt `jibs` binary instead of fetching one
/// (dev builds, air-gapped machines, or a version whose binaries aren't
/// published yet).
const JIBS_PATH_ENV: &str = "BOUGIE_JIBS_PATH";

const MIRROR_BASE: &str = "https://releases.bougie.tools/github/jibs/releases/download";
const GITHUB_BASE: &str = "https://github.com/cresset-tools/jibs/releases/download";
const TAG_PREFIX: &str = "jibs-v";

const MARIADB: &str = "mariadb";

#[allow(
    clippy::needless_pass_by_value,
    reason = "owned args from clap-parsed CLI"
)]
pub fn run(_format: OutputFormat, args: DbSeedArgs) -> Result<ExitCode> {
    let paths = Paths::from_env()?;
    let project_root = locate_project_root()?;

    // One-shot gate: once a project is seeded, `db seed` is a no-op so it's safe
    // to run on every `bougie start`. Checked *before* resolving a source, so a
    // seeded project needs neither `--from` nor a cached pull. `--force` (and
    // `db refresh`) bypass it to reload the newest data — the deliberate clobber.
    let marker_path = seed_marker_path(&paths, &project_root);
    if !args.force {
        if let Some(marker) = read_seed_marker(&marker_path) {
            println!(
                "bougie db seed: database already seeded (from {}). Use `bougie db seed \
                 --force` or `bougie db refresh` to reload.",
                marker.describe()
            );
            return Ok(ExitCode::SUCCESS);
        }
    }

    // `--from` wins; otherwise load whatever `bougie db pull` last fetched for
    // this project (carrying its digest so the marker can record it).
    let (from, digest) = match args.from {
        Some(from) => (from, None),
        None => {
            let snap = super::pull::pulled_snapshot(&paths, &project_root).ok_or_else(|| {
                eyre!(
                    "nothing to seed: pass `--from <path-or-url>`, or run `bougie db pull \
                     --repo <org/repo>` first"
                )
            })?;
            (snap.path, Some(snap.digest))
        }
    };

    let dsn = mariadb_dsn(&paths, &project_root)?;
    let jibs = ensure_jibs(&paths)?;

    // jibs load <from> --local-mysql <dsn> [--clean]. Inherit stdio so jibs's
    // per-table progress and any dropped-row warnings pass straight through.
    let mut cmd = Command::new(&jibs);
    cmd.arg("load")
        .arg(&from)
        .arg("--local-mysql")
        .arg(&dsn);
    if args.clean {
        cmd.arg("--clean");
    }

    println!("bougie db seed: loading {from} into the mariadb tenant");
    let status = cmd
        .status()
        .wrap_err_with(|| format!("failed to execute jibs at {}", jibs.display()))?;

    // Record the seed only on success, so a failed/partial load stays re-runnable
    // (no marker → next `db seed` retries). A marker-write hiccup only forfeits
    // the one-shot skip; it never fails the load.
    if status.success() {
        let marker = SeedMarker {
            schema_version: SEED_MARKER_SCHEMA_VERSION,
            source: from,
            digest,
            seeded_at_unix: now_unix(),
        };
        if let Err(e) = write_seed_marker(&marker_path, &marker) {
            eprintln!("bougie db seed: warning: couldn't record the seed marker: {e}");
        }
    }

    // Mirror jibs's exit code so scripting/CI sees the load's real outcome.
    let code = status
        .code()
        .and_then(|c| u8::try_from(c).ok())
        .unwrap_or(1);
    Ok(ExitCode::from(code))
}

/// On-disk shape of the seed marker. Bumped if the layout changes.
const SEED_MARKER_SCHEMA_VERSION: u32 = 1;

/// The durable record that a project's database has been seeded — written under
/// the project state dir (survives `rm -rf vendor`), so `db seed` is a one-shot.
#[derive(Serialize, Deserialize)]
struct SeedMarker {
    schema_version: u32,
    /// What was loaded — the pulled snapshot's cache path, or the `--from` value.
    source: String,
    /// Hex sha256 of the loaded snapshot, when known (a pulled snapshot). Lets a
    /// later staleness check compare it against the registry's `latest`.
    digest: Option<String>,
    /// When the seed ran (unix seconds).
    seeded_at_unix: u64,
}

impl SeedMarker {
    /// A short human description for the "already seeded (from …)" message.
    fn describe(&self) -> String {
        match &self.digest {
            Some(d) => format!("snapshot {}", &d[..d.len().min(16)]),
            None => self.source.clone(),
        }
    }
}

fn seed_marker_path(paths: &Paths, project_root: &Path) -> PathBuf {
    paths.project_state_dir(project_root).join("seeded.json")
}

fn read_seed_marker(path: &Path) -> Option<SeedMarker> {
    serde_json::from_slice(&fs::read(path).ok()?).ok()
}

fn write_seed_marker(path: &Path, marker: &SeedMarker) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .wrap_err_with(|| format!("creating project state dir {}", parent.display()))?;
    }
    let json = serde_json::to_vec_pretty(marker).wrap_err("serializing the seed marker")?;
    fs::write(path, json).wrap_err_with(|| format!("writing {}", path.display()))
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Build a `mysql://` DSN for the project's mariadb tenant, sourced offline from
/// the tenant ledger + derived password (the same values `bougie service
/// credentials` prints). `MariaDB` is socket-only; `user` and the database name
/// are both the tenant name.
pub(crate) fn mariadb_dsn(paths: &Paths, project_root: &Path) -> Result<String> {
    // Multi-instance: resolve which mariadb version this project runs by
    // scanning the on-disk ledgers, then read that instance's tenant ledger.
    let version = tenants::project_instance_version(paths, MARIADB, project_root).ok_or_else(|| {
        eyre!("no mariadb tenant is provisioned for this project — run `bougie up mariadb` first")
    })?;
    let ledger = paths.service_tenants(MARIADB, &version);
    let rows = tenants::load_all_sync(&ledger).wrap_err("reading the mariadb tenant ledger")?;
    let canon = project_root
        .canonicalize()
        .unwrap_or_else(|_| project_root.to_path_buf());
    let tenant = rows
        .into_iter()
        .find(|t| t.project == canon || t.project == project_root)
        .ok_or_else(|| {
            eyre!(
                "no mariadb tenant is provisioned for this project — run `bougie up mariadb` first"
            )
        })?;

    let user = tenant.tenant.clone();
    let database = tenant.tenant.clone();
    let password = match tenant.secrets.get("password") {
        Some(p) => p.clone(),
        None => derive_password(paths, MARIADB, &tenant.project)
            .wrap_err("deriving the mariadb tenant password")?,
    };
    // Connect via the project's STABLE connection socket (a symlink the
    // daemon keeps pointed at the live instance) — the same path `bougie
    // service credentials` and `bougie run` hand out, stable across DB
    // version bumps.
    let socket = paths.project_conn_socket(project_root, "mariadb.sock");

    // `localhost` is a placeholder the mysql driver ignores once `socket=` is
    // set. Percent-encode every interpolated value: the socket path is absolute
    // and BOUGIE_HOME may contain characters that aren't URL-safe.
    Ok(format!(
        "mysql://{user}:{pw}@localhost/{db}?socket={sock}",
        user = urlencode(&user),
        pw = urlencode(&password),
        db = urlencode(&database),
        sock = urlencode(&socket.to_string_lossy()),
    ))
}

/// Percent-encode everything outside the RFC 3986 unreserved set
/// (`A-Z a-z 0-9 - _ . ~`), so a value drops safely into a URL.
fn urlencode(s: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push('%');
            out.push(HEX[(b >> 4) as usize] as char);
            out.push(HEX[(b & 0x0f) as usize] as char);
        }
    }
    out
}

/// Path to a ready-to-run `jibs` binary: the `BOUGIE_JIBS_PATH` override when
/// set, otherwise a cached pinned download (fetched + extracted on a cache
/// miss), mirroring `bougie format`'s wick handling.
pub(crate) fn ensure_jibs(paths: &Paths) -> Result<PathBuf> {
    if let Some(p) = std::env::var_os(JIBS_PATH_ENV) {
        let p = PathBuf::from(p);
        if !p.is_file() {
            return Err(eyre!(
                "{JIBS_PATH_ENV} is set but {} is not a file",
                p.display()
            ));
        }
        return Ok(p);
    }

    let version = std::env::var(JIBS_VERSION_ENV)
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_JIBS_VERSION.to_string());

    let bin = paths.cache().join("jibs").join(&version).join("jibs");
    if bin.is_file() {
        return Ok(bin);
    }

    let target = Triple::detect()?.to_string();
    let archive = format!("jibs-{target}.tar.gz");
    let tag = format!("{TAG_PREFIX}{version}");
    let client = bougie_fetch::default_client()?;

    let tmp = tempfile::TempDir::new().wrap_err("creating temp dir for jibs download")?;
    let archive_path = tmp.path().join(&archive);
    let sha_path = tmp.path().join(format!("{archive}.sha256"));
    let extract_root = tmp.path().join("extracted");
    fs::create_dir_all(&extract_root).wrap_err("preparing extract dir")?;

    native_fetch::download(
        &client,
        &urls(&tag, &format!("{archive}.sha256")),
        &sha_path,
    )
    .wrap_err("downloading jibs sha256 sidecar")?;
    let expected = native_fetch::parse_sidecar(&sha_path, &archive)?;

    println!("bougie db seed: fetching jibs {version} ({target})");
    native_fetch::download(&client, &urls(&tag, &archive), &archive_path).wrap_err_with(|| {
        format!(
            "downloading jibs {version}. If this 404s, that version may not have published \
             binaries yet — check https://github.com/cresset-tools/jibs/releases, pin another \
             with {JIBS_VERSION_ENV}, or point {JIBS_PATH_ENV} at a local jibs binary."
        )
    })?;
    native_fetch::verify_sha256(&archive_path, &expected)?;
    native_fetch::extract(&archive_path, &extract_root, false)?;

    // dist packs archives as `jibs-<target>/jibs`.
    let staged = extract_root.join(format!("jibs-{target}")).join("jibs");
    if !staged.is_file() {
        return Err(eyre!(
            "extracted jibs archive missing expected binary at {}",
            staged.display()
        ));
    }
    native_fetch::install_file_atomic(&staged, &bin)?;

    Ok(bin)
}

/// Mirror first (low latency, no GitHub anonymous rate limits), GitHub release
/// as fallback — same precedence as `bougie self update` and `bougie format`.
fn urls(tag: &str, file: &str) -> Vec<String> {
    vec![
        format!("{MIRROR_BASE}/{tag}/{file}"),
        format!("{GITHUB_BASE}/{tag}/{file}"),
    ]
}

#[cfg(test)]
mod tests {
    use super::{
        SEED_MARKER_SCHEMA_VERSION, SeedMarker, read_seed_marker, urlencode, write_seed_marker,
    };

    #[test]
    fn seed_marker_round_trips() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("state").join("seeded.json");

        // Absent marker → None (the "not yet seeded" case).
        assert!(read_seed_marker(&path).is_none());

        let marker = SeedMarker {
            schema_version: SEED_MARKER_SCHEMA_VERSION,
            source: "/cache/snapshots/deadbeefcafe1234.jibsdump".to_string(),
            digest: Some("deadbeefcafe1234abcd".to_string()),
            seeded_at_unix: 1_700_000_000,
        };
        write_seed_marker(&path, &marker).unwrap(); // also creates the parent dir
        let back = read_seed_marker(&path).expect("round-trips");
        assert_eq!(back.source, marker.source);
        assert_eq!(back.digest.as_deref(), Some("deadbeefcafe1234abcd"));
        assert_eq!(back.seeded_at_unix, 1_700_000_000);
    }

    #[test]
    fn seed_marker_describe_prefers_short_digest_else_source() {
        let with_digest = SeedMarker {
            schema_version: SEED_MARKER_SCHEMA_VERSION,
            source: "/some/very/long/path.jibsdump".to_string(),
            digest: Some("0123456789abcdef0000".to_string()),
            seeded_at_unix: 0,
        };
        // First 16 hex chars of the digest.
        assert_eq!(with_digest.describe(), "snapshot 0123456789abcdef");

        let no_digest = SeedMarker {
            schema_version: SEED_MARKER_SCHEMA_VERSION,
            source: "https://example.test/prod.jibsdump".to_string(),
            digest: None,
            seeded_at_unix: 0,
        };
        assert_eq!(no_digest.describe(), "https://example.test/prod.jibsdump");
    }

    #[test]
    fn urlencode_leaves_unreserved_untouched() {
        assert_eq!(urlencode("acme_db-1.0~x"), "acme_db-1.0~x");
        assert_eq!(urlencode("deadbeef0123"), "deadbeef0123");
    }

    #[test]
    fn urlencode_escapes_socket_path_and_specials() {
        assert_eq!(
            urlencode("/home/u/.local/share/bougie/run/mariadb.sock"),
            "%2Fhome%2Fu%2F.local%2Fshare%2Fbougie%2Frun%2Fmariadb.sock"
        );
        // Spaces and other unsafe bytes are encoded.
        assert_eq!(urlencode("a b@c"), "a%20b%40c");
    }
}
