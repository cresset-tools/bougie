//! `bougie db pull` — fetch the latest production-shaped database snapshot for a
//! repo from the team's sconce registry into the local cache, so `bougie db
//! seed` can load it without a URL.
//!
//! The registry and the bearer both come from `bougie login`: the durable team
//! record (written at login, under `$BOUGIE_HOME/state/projects/<hash>/`) names
//! the registry, and the auth store holds the org-scoped token keyed by the
//! registry host. We GET `<registry>/<org>/<repo>/snapshots/<env>/latest`, which
//! 302s to a short-lived presigned URL (object-store backends) or streams the
//! bytes directly (filesystem backends) — either way reqwest follows to the
//! blob. We stream it to a content-addressed cache file
//! (`<cache>/snapshots/<sha256>.jibsdump`), hashing as we go so a multi-GB dump
//! never sits in memory, and drop a per-project pointer so `bougie db seed`
//! (with no `--from`) knows what to load.

use std::fmt::Write as _;
use std::fs;
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use bougie_cli::{DbPullArgs, OutputFormat};
use bougie_composer_resolver::metadata::auth_origin;
use bougie_composer_resolver::update::read_bougie_bearer;
use bougie_paths::Paths;
use eyre::{Result, WrapErr, eyre};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::super::service::config_mut::locate_project_root;
use crate::commands::team;

/// On-disk shape of `pulled-snapshot.json`. Bumped if the layout changes.
const POINTER_SCHEMA_VERSION: u32 = 1;

/// Exit code for `--if-configured` when nothing is configured to pull — the
/// recipe's signal to fall back to a fresh `setup:install` (distinct from a real
/// failure, which stays non-zero-but-not-3).
const NOT_CONFIGURED: u8 = 3;

/// The per-project record of the last snapshot `bougie db pull` fetched, written
/// under the durable project state dir (survives `rm -rf vendor`). `bougie db
/// seed` reads it when invoked with no `--from`.
#[derive(Serialize, Deserialize)]
pub(crate) struct PulledSnapshot {
    schema_version: u32,
    /// The registry repository the snapshot came from, as `<org>/<repo>`.
    repo: String,
    /// The environment whose `latest` pointer was resolved.
    environment: String,
    /// The data profile whose `latest` was resolved (`full` on pointers written
    /// before profiles existed).
    #[serde(default = "default_profile")]
    profile: String,
    /// Hex sha256 of the dump — also the cache filename stem.
    pub(crate) digest: String,
    /// Absolute path of the cached `.jibsdump`.
    pub(crate) path: String,
}

#[allow(
    clippy::needless_pass_by_value,
    reason = "owned args from clap-parsed CLI"
)]
pub fn run(_format: OutputFormat, args: DbPullArgs) -> Result<ExitCode> {
    let paths = Paths::from_env()?;
    let project_root = locate_project_root()?;

    // `--repo`/`--env`/`--profile` win; otherwise default from the snapshot
    // source the team manifest advertises for this project (cached by `bougie
    // login`/`sync`). No
    // source configured is a hard error normally, but a soft `exit 3` under
    // `--if-configured` so the recipe can fall back to a fresh install.
    let (repo_spec, env, profile) = match resolve_target(&args, &project_root) {
        Ok(target) => target,
        Err(_) if args.if_configured => {
            eprintln!(
                "bougie db pull: no database snapshot source is configured for this project; \
                 skipping (--if-configured)."
            );
            return Ok(ExitCode::from(NOT_CONFIGURED));
        }
        Err(e) => return Err(e),
    };
    let (org, repo) = parse_repo(&repo_spec)?;

    // Registry + auth come from `bougie login` — the team record names the
    // registry, the auth store holds the org-scoped bearer keyed by its host.
    // "Not a team project" is `exit 3` under `--if-configured`; being logged out
    // of one stays a hard error (the user needs to re-login, not silently fall
    // back to a throwaway install).
    let record = match team::read_record(&project_root) {
        Some(record) => record,
        None if args.if_configured => {
            eprintln!(
                "bougie db pull: this project isn't wired to a sconce registry; skipping \
                 (--if-configured)."
            );
            return Ok(ExitCode::from(NOT_CONFIGURED));
        }
        None => {
            return Err(eyre!(
                "this project isn't wired to a sconce registry — run `bougie login <registry>` first"
            ));
        }
    };
    let base = record.registry.trim_end_matches('/').to_string();
    let host = auth_origin(&base);
    let token = read_bougie_bearer(&host)
        .ok_or_else(|| eyre!("not logged in to {host} — run `bougie login {base}`"))?;

    // The default profile stays off the URL: byte-identical to the pre-profile
    // request, so it keeps working against a registry predating profiles.
    let url = if profile == "full" {
        format!("{base}/{org}/{repo}/snapshots/{env}/latest")
    } else {
        format!("{base}/{org}/{repo}/snapshots/{env}/latest?profile={profile}")
    };
    println!("bougie db pull: fetching {org}/{repo} {env}/{profile} snapshot from {base}");

    let snap_dir = paths.cache().join("snapshots");
    fs::create_dir_all(&snap_dir)
        .wrap_err_with(|| format!("creating snapshot cache dir {}", snap_dir.display()))?;

    let (digest, size, path) = download_snapshot(&url, &token, &snap_dir)
        .wrap_err_with(|| format!("pulling the snapshot from {url}"))?;

    write_pointer(&paths, &project_root, &repo_spec, &env, &profile, &digest, &path)?;

    println!(
        "bougie db pull: cached {} ({}, sha256 {digest})",
        path.display(),
        human_size(size),
    );
    println!("run `bougie db seed` to load it into the mariadb tenant");
    Ok(ExitCode::SUCCESS)
}

/// Resolve which `(repo, env, profile)` to pull. `--repo` wins (with `--env`/
/// `--profile` or their `production`/`full` defaults); otherwise fall back to
/// the snapshot source the team manifest advertises for this project, letting
/// `--env` and `--profile` still override its fields individually. Errors when
/// neither is available.
fn resolve_target(args: &DbPullArgs, project_root: &Path) -> Result<(String, String, String)> {
    if let Some(repo) = &args.repo {
        let env = args.env.clone().unwrap_or_else(default_env);
        let profile = args.profile.clone().unwrap_or_else(default_profile);
        return Ok((repo.clone(), env, profile));
    }
    let snap = team::cached_snapshot_ref(project_root).ok_or_else(|| {
        eyre!(
            "no --repo given and the team manifest has no snapshot source for this project. \
             Pass `--repo <org/repo>`, or have an admin run `sconce remote-snapshot`. (If you \
             just registered one, run `bougie sync` to refresh the cached manifest.)"
        )
    })?;
    // Explicit --env/--profile override the manifest's fields.
    let env = args.env.clone().or(snap.env).unwrap_or_else(default_env);
    let profile = args
        .profile
        .clone()
        .or(snap.profile)
        .unwrap_or_else(default_profile);
    Ok((snap.repo, env, profile))
}

fn default_env() -> String {
    "production".to_string()
}

fn default_profile() -> String {
    "full".to_string()
}

/// Split `<org>/<repo>`. Both halves must be present and non-empty, and there
/// must be exactly one separator — the value indexes a single registry repo.
fn parse_repo(spec: &str) -> Result<(String, String)> {
    let mut parts = spec.splitn(2, '/');
    match (parts.next(), parts.next()) {
        (Some(org), Some(repo))
            if !org.is_empty() && !repo.is_empty() && !repo.contains('/') =>
        {
            Ok((org.to_string(), repo.to_string()))
        }
        _ => Err(eyre!(
            "`--repo` must be `<org>/<repo>` (e.g. `acme/shop`), got {spec:?}"
        )),
    }
}

/// GET the snapshot, following the registry's 302 to the presigned blob (or its
/// direct byte stream on a filesystem backend), and stream it into a
/// content-addressed cache file, hashing as we write. Returns `(digest, bytes,
/// path)`.
fn download_snapshot(url: &str, token: &str, snap_dir: &Path) -> Result<(String, u64, PathBuf)> {
    // A dedicated client with no *total* timeout: a production dump can take a
    // while, and reqwest's `timeout` caps the whole request, not just the idle
    // gap. Default redirect policy follows the registry's 302 (and drops the
    // bearer on the cross-host hop to the presigned URL, which is self-signed).
    let client = reqwest::blocking::Client::builder()
        .user_agent(concat!("bougie/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(Duration::from_secs(10))
        .build()
        .wrap_err("building the HTTP client")?;

    let mut resp = client
        .get(url)
        .bearer_auth(token)
        .send()
        .wrap_err("connecting to the registry")?;

    let status = resp.status();
    if status == reqwest::StatusCode::NOT_FOUND {
        return Err(eyre!(
            "the registry has no snapshot for this repo/environment/profile (404). Check \
             `--repo`, `--env` and `--profile`, and that a snapshot has been published for it."
        ));
    }
    if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
        return Err(eyre!(
            "the registry rejected the request ({status}) — your login may not grant access to \
             this repo. Re-run `bougie login`."
        ));
    }
    if !status.is_success() {
        return Err(eyre!("the registry answered {status}"));
    }

    // Temp file in the destination dir → the final rename is atomic (same
    // filesystem), so a killed download never leaves a half-written blob under
    // its digest name.
    let mut tmp = tempfile::NamedTempFile::new_in(snap_dir)
        .wrap_err_with(|| format!("creating a temp file in {}", snap_dir.display()))?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1 << 16];
    let mut size: u64 = 0;
    loop {
        let n = resp.read(&mut buf).wrap_err("reading snapshot bytes")?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        tmp.write_all(&buf[..n]).wrap_err("writing the snapshot to cache")?;
        size += n as u64;
    }
    tmp.flush().wrap_err("flushing the snapshot to cache")?;

    let digest = hex_digest(&hasher.finalize());
    let dest = snap_dir.join(format!("{digest}.jibsdump"));
    tmp.persist(&dest)
        .map_err(|e| eyre!("installing the snapshot into {}: {e}", dest.display()))?;
    Ok((digest, size, dest))
}

/// Record the pulled snapshot so `bougie db seed` (no `--from`) can find it.
fn write_pointer(
    paths: &Paths,
    project_root: &Path,
    repo: &str,
    environment: &str,
    profile: &str,
    digest: &str,
    path: &Path,
) -> Result<()> {
    let record = PulledSnapshot {
        schema_version: POINTER_SCHEMA_VERSION,
        repo: repo.to_string(),
        environment: environment.to_string(),
        profile: profile.to_string(),
        digest: digest.to_string(),
        path: path.to_string_lossy().into_owned(),
    };
    let dir = paths.project_state_dir(project_root);
    fs::create_dir_all(&dir)
        .wrap_err_with(|| format!("creating project state dir {}", dir.display()))?;
    let file = dir.join("pulled-snapshot.json");
    let json = serde_json::to_vec_pretty(&record).wrap_err("serializing the snapshot pointer")?;
    fs::write(&file, json).wrap_err_with(|| format!("writing {}", file.display()))?;
    Ok(())
}

/// The snapshot most recently fetched by `bougie db pull` for this project —
/// `None` if nothing has been pulled or the cached file is gone (e.g. the cache
/// was cleared). Carries the digest so `bougie db seed` can record what it
/// loaded. Read by `bougie db seed` when given no `--from`.
pub(crate) fn pulled_snapshot(paths: &Paths, project_root: &Path) -> Option<PulledSnapshot> {
    let file = paths.project_state_dir(project_root).join("pulled-snapshot.json");
    let bytes = fs::read(&file).ok()?;
    let record: PulledSnapshot = serde_json::from_slice(&bytes).ok()?;
    Path::new(&record.path).is_file().then_some(record)
}

/// Lowercase hex of a digest, the repo idiom (`sha2` 0.11 has no `io::Write`).
fn hex_digest(bytes: &[u8]) -> String {
    bytes.iter().fold(String::with_capacity(64), |mut acc, b| {
        let _ = write!(acc, "{b:02x}");
        acc
    })
}

/// A rough human-readable byte count for the "cached …" line.
fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KiB", "MiB", "GiB"];
    let mut val = bytes as f64;
    let mut unit = 0;
    while val >= 1024.0 && unit < UNITS.len() - 1 {
        val /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{val:.1} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_target_uses_explicit_repo_over_manifest() {
        // repo is Some → the manifest is never consulted, so project_root is
        // irrelevant. Default env is production and default profile is full;
        // --env/--profile override them.
        let root = Path::new("/nonexistent");
        let a = DbPullArgs {
            repo: Some("acme/shop".to_string()),
            env: None,
            profile: None,
            if_configured: false,
        };
        assert_eq!(
            resolve_target(&a, root).unwrap(),
            (
                "acme/shop".to_string(),
                "production".to_string(),
                "full".to_string()
            )
        );
        let b = DbPullArgs {
            repo: Some("acme/shop".to_string()),
            env: Some("staging".to_string()),
            profile: Some("small".to_string()),
            if_configured: false,
        };
        assert_eq!(
            resolve_target(&b, root).unwrap(),
            (
                "acme/shop".to_string(),
                "staging".to_string(),
                "small".to_string()
            )
        );
    }

    #[test]
    fn parse_repo_splits_org_and_repo() {
        assert_eq!(
            parse_repo("acme/shop").unwrap(),
            ("acme".to_string(), "shop".to_string())
        );
    }

    #[test]
    fn parse_repo_rejects_malformed_specs() {
        for bad in ["acme", "acme/", "/shop", "acme/shop/extra", ""] {
            assert!(parse_repo(bad).is_err(), "{bad:?} should be rejected");
        }
    }

    #[test]
    fn hex_digest_is_lowercase_and_padded() {
        assert_eq!(hex_digest(&[0x00, 0x0f, 0xa0, 0xff]), "000fa0ff");
    }

    #[test]
    fn human_size_scales_units() {
        assert_eq!(human_size(512), "512 B");
        assert_eq!(human_size(1024), "1.0 KiB");
        assert_eq!(human_size(1024 * 1024 * 3 / 2), "1.5 MiB");
    }

    #[test]
    fn pulled_snapshot_round_trips_and_checks_existence() {
        let tmp = tempfile::TempDir::new().unwrap();
        // Home + cache under the tempdir so project_state_dir resolves there.
        let paths = Paths::new(tmp.path().to_path_buf(), tmp.path().join("cache"));
        let project = tmp.path().join("proj");
        fs::create_dir_all(&project).unwrap();

        // No pointer yet → None.
        assert!(pulled_snapshot(&paths, &project).is_none());

        // A pointer to a real cached file → Some(record) carrying path + digest.
        let cached = tmp.path().join("snap.jibsdump");
        fs::write(&cached, b"dump").unwrap();
        write_pointer(
            &paths,
            &project,
            "acme/shop",
            "production",
            "small",
            "deadbeef",
            &cached,
        )
        .unwrap();
        let got = pulled_snapshot(&paths, &project).expect("record");
        assert_eq!(got.path, cached.to_string_lossy());
        assert_eq!(got.digest, "deadbeef");
        assert_eq!(got.profile, "small");

        // A pointer whose file has been removed → None (cache was cleared).
        fs::remove_file(&cached).unwrap();
        assert!(pulled_snapshot(&paths, &project).is_none());
    }

    #[test]
    fn pulled_snapshot_pointer_predating_profiles_defaults_to_full() {
        // A pointer written before profiles existed has no `profile` key — it
        // must still parse (as `full`), not invalidate the pulled snapshot.
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = Paths::new(tmp.path().to_path_buf(), tmp.path().join("cache"));
        let project = tmp.path().join("proj");
        fs::create_dir_all(&project).unwrap();
        let cached = tmp.path().join("snap.jibsdump");
        fs::write(&cached, b"dump").unwrap();

        let dir = paths.project_state_dir(&project);
        fs::create_dir_all(&dir).unwrap();
        let old = serde_json::json!({
            "schema_version": 1,
            "repo": "acme/shop",
            "environment": "production",
            "digest": "deadbeef",
            "path": cached.to_string_lossy(),
        });
        fs::write(dir.join("pulled-snapshot.json"), old.to_string()).unwrap();

        let got = pulled_snapshot(&paths, &project).expect("old pointer still parses");
        assert_eq!(got.profile, "full");
        assert_eq!(got.digest, "deadbeef");
    }
}
