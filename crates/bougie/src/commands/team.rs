//! Team-mode project state: which sconce registry a project is wired to, and
//! self-healing its Composer repo-config overlay after a `vendor/` wipe.
//!
//! When `bougie login` provisions a project's private repositories into the
//! `vendor/bougie/repositories.json` overlay, it also records *which registry*
//! the project logged in against, in a durable per-project file under
//! `$BOUGIE_HOME/state/projects/<hash>/team.json`. Like the tenant cache in
//! [`crate::commands::tenant`], that record lives outside `vendor/`, so it
//! survives `rm -rf vendor`.
//!
//! `bougie sync` reads it back: if the overlay was wiped along with `vendor/`,
//! it re-discovers the project's repositories from the recorded registry
//! (reusing the stored login token) and rewrites the overlay — so private
//! packages resolve again with no manual `bougie login`. Sync is where the
//! overlay is consumed (it feeds resolution), so it's the natural place to
//! restore it; `bougie start` heals for free via its sync prologue. The whole
//! path is best-effort: not a team project, overlay already intact, logged out,
//! or the registry unreachable all degrade to (at most) a one-line note and
//! never block the sync.
//!
//! Discovery is **git-remote-keyed**: when the project has an `origin` remote,
//! bougie fetches the team manifest (`GET /api/v1/manifest?remote=…`) — the
//! authoritative, per-project repo list the registry maps to that remote — and
//! caches it next to the record (`manifest.json`) for offline restores. A
//! project with no remote, or a remote the registry doesn't recognize, falls
//! back to the login org's repositories (M4's `/api/v1/repos`).

use bougie_composer_resolver::metadata::auth_origin;
use bougie_composer_resolver::update::{
    read_bougie_bearer, repositories_overlay_path, write_repositories_overlay,
};
use bougie_paths::Paths;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// The on-disk shape of `team.json`. Bumped if the layout changes.
const RECORD_SCHEMA_VERSION: u32 = 1;

/// The durable per-project record wiring a project to its sconce registry.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TeamRecord {
    /// On-disk schema version (see [`RECORD_SCHEMA_VERSION`]).
    pub schema_version: u32,
    /// The sconce registry base URL the project logged in against (no trailing
    /// slash), e.g. `https://packages.acme.example`. `bougie start`
    /// re-provisions the overlay from `<registry>/api/v1/repos`.
    pub registry: String,
}

/// `$BOUGIE_HOME/state/projects/<hash>/team.json` for `project_root`. `None`
/// when the state root can't be resolved (no `BOUGIE_HOME`/`HOME`), which the
/// best-effort callers treat as "no record".
fn record_path(project_root: &Path) -> Option<PathBuf> {
    let paths = Paths::from_env().ok()?;
    Some(paths.project_state_dir(project_root).join("team.json"))
}

/// Read the durable team record for `project_root`, if any. Best-effort: a
/// missing, unreadable, or malformed file is simply "no record".
#[must_use]
pub fn read_record(project_root: &Path) -> Option<TeamRecord> {
    read_record_at(&record_path(project_root)?)
}

fn read_record_at(path: &Path) -> Option<TeamRecord> {
    let bytes = std::fs::read(path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// Persist `registry` as `project_root`'s team record so `bougie start` can
/// self-heal the overlay after a `vendor/` wipe. Best-effort — a write failure
/// only costs a future self-heal, so it never propagates. No-ops when the file
/// already records the same registry.
pub fn write_record(project_root: &Path, registry: &str) {
    let Some(path) = record_path(project_root) else {
        return;
    };
    write_record_at(&path, registry);
}

fn write_record_at(path: &Path, registry: &str) {
    let record = TeamRecord {
        schema_version: RECORD_SCHEMA_VERSION,
        registry: registry.trim_end_matches('/').to_string(),
    };
    if read_record_at(path).as_ref() == Some(&record) {
        return;
    }
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(mut s) = serde_json::to_string_pretty(&record) {
        s.push('\n');
        let _ = std::fs::write(path, s);
    }
}

/// Re-provision `project_root`'s repositories overlay when it's missing — the
/// `rm -rf vendor` self-heal. Called at the top of `bougie sync`, before
/// resolution, so private packages resolve (and so `bougie start` heals via its
/// sync prologue). Entirely best-effort: not a team project, overlay already
/// present, logged out, or registry unreachable each degrade to (at most) a
/// one-line stderr note and never affect the sync's outcome.
pub fn heal_overlay(project_root: &Path) {
    // Not a team project (never logged in here in overlay mode) → nothing to do.
    let Some(record) = read_record(project_root) else {
        return;
    };
    // Overlay intact → keep sync fast and offline; only a wipe (missing file)
    // reaches for the network.
    if repositories_overlay_path(project_root).exists() {
        return;
    }
    match reprovision(project_root, &record.registry) {
        Ok(0) => {}
        Ok(n) => eprintln!(
            "Restored {n} Composer repositor{} from {} (vendor/ was wiped).",
            if n == 1 { "y" } else { "ies" },
            record.registry
        ),
        Err(e) => eprintln!(
            "warning: couldn't restore this project's Composer repositories from {} ({e}). \
             Private packages may fail to install — run `bougie login` to re-authenticate.",
            record.registry
        ),
    }
}

/// Discover the project's repositories at `registry` and (re)write the overlay
/// in `root`; returns how many entries were written. Errors on a missing token
/// or a failed discovery so the caller can surface a note.
fn reprovision(root: &Path, registry: &str) -> eyre::Result<usize> {
    use eyre::eyre;
    let base = registry.trim_end_matches('/');
    let host = auth_origin(base);
    let token = read_bougie_bearer(&host).ok_or_else(|| eyre!("not logged in to {host}"))?;
    let client = http_client()?;
    let urls = discover_repo_urls(&client, base, &token, root)?;
    let (_path, added) =
        write_repositories_overlay(root, &urls).map_err(|e| eyre!("writing overlay: {e}"))?;
    Ok(added)
}

/// A blocking HTTP client configured like `bougie login`'s.
fn http_client() -> eyre::Result<reqwest::blocking::Client> {
    use eyre::WrapErr;
    reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(30))
        .user_agent(format!("bougie/{}", env!("CARGO_PKG_VERSION")))
        .build()
        .wrap_err("building http client")
}

/// The team manifest served at `GET /api/v1/manifest?remote=…`. Forward-
/// compatible: only `repositories` is read today, so a richer manifest (pinned
/// service versions, policy, …) from a newer registry is tolerated. The cache
/// stores the raw bytes, not this struct, so those extra fields survive a
/// round-trip for a future bougie that understands them.
#[derive(Debug, Clone, Deserialize)]
struct Manifest {
    #[serde(default)]
    repositories: Vec<ManifestRepo>,
    // Only read by the (Unix-only) `db pull` command; on Windows it's parsed but
    // unused, so don't let the dead-code lint fail the build there.
    #[serde(default)]
    #[cfg_attr(not(unix), allow(dead_code))]
    snapshot: Option<ManifestSnapshot>,
}

#[derive(Debug, Clone, Deserialize)]
struct ManifestRepo {
    url: String,
}

/// The database snapshot source the team manifest advertises (added by sconce's
/// `remote-snapshot`). Lenient: `env` may be absent (defaults to production).
#[derive(Debug, Clone, Deserialize)]
#[cfg_attr(not(unix), allow(dead_code))]
struct ManifestSnapshot {
    repo: String,
    #[serde(default)]
    env: Option<String>,
}

/// Discover the project's Composer repository URLs. Prefers the git-remote-keyed
/// team manifest (caching it for offline restores); falls back to the login
/// org's `/api/v1/repos` when the project has no `origin` remote or the registry
/// doesn't recognize it. On a network/registry error, a previously-cached
/// manifest is used so a wiped overlay still restores offline.
pub fn discover_repo_urls(
    client: &reqwest::blocking::Client,
    base: &str,
    token: &str,
    project_root: &Path,
) -> eyre::Result<Vec<String>> {
    if let Some(remote) = git_remote(project_root) {
        match fetch_manifest_raw(client, base, token, &remote) {
            Ok(Some(raw)) => {
                write_cached_manifest(project_root, &raw);
                return Ok(manifest_urls(&raw));
            }
            // Remote not registered as a team project → the login org's repos.
            Ok(None) => {}
            Err(e) => {
                // Registry unreachable: fall back to the last cached manifest so
                // a `rm -rf vendor` still heals offline; else surface the error.
                if let Some(raw) = read_cached_manifest(project_root) {
                    return Ok(manifest_urls(&raw));
                }
                return Err(e);
            }
        }
    }
    crate::commands::login::fetch_repo_urls(client, base, token)
}

/// `GET {base}/api/v1/manifest?remote=…` → the raw manifest bytes (`Some`), or
/// `None` when the registry has no team config for this remote (404). Any other
/// non-success status is an error.
fn fetch_manifest_raw(
    client: &reqwest::blocking::Client,
    base: &str,
    token: &str,
    remote: &str,
) -> eyre::Result<Option<Vec<u8>>> {
    use eyre::{WrapErr, eyre};
    let resp = client
        .get(format!("{base}/api/v1/manifest"))
        .query(&[("remote", remote)])
        .bearer_auth(token)
        .send()
        .wrap_err("requesting team manifest")?;
    let status = resp.status();
    if status == reqwest::StatusCode::NOT_FOUND {
        return Ok(None);
    }
    if !status.is_success() {
        return Err(eyre!("registry answered {status} for /api/v1/manifest"));
    }
    let bytes = resp.bytes().wrap_err("reading team manifest")?;
    Ok(Some(bytes.to_vec()))
}

/// The Composer repository URLs in a raw manifest; empty if it can't be parsed
/// (a shape bougie doesn't understand is treated as "no repositories").
fn manifest_urls(raw: &[u8]) -> Vec<String> {
    serde_json::from_slice::<Manifest>(raw)
        .map(|m| m.repositories.into_iter().map(|r| r.url).collect())
        .unwrap_or_default()
}

/// `$BOUGIE_HOME/state/projects/<hash>/manifest.json` — the cached raw manifest,
/// beside the [`record_path`] team record.
fn manifest_cache_path(project_root: &Path) -> Option<PathBuf> {
    let paths = Paths::from_env().ok()?;
    Some(paths.project_state_dir(project_root).join("manifest.json"))
}

fn write_cached_manifest(project_root: &Path, raw: &[u8]) {
    let Some(path) = manifest_cache_path(project_root) else {
        return;
    };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(path, raw);
}

fn read_cached_manifest(project_root: &Path) -> Option<Vec<u8>> {
    std::fs::read(manifest_cache_path(project_root)?).ok()
}

/// The database snapshot source the cached team manifest advertises for this
/// project, as `(repo, env)` where `repo` is `<org>/<repo>` and `env` is the
/// manifest's environment when present. `None` if no manifest is cached (login
/// against a non-team project, or a registry with no snapshot configured) or it
/// carries no snapshot block. Lets `bougie db pull` default its target.
#[cfg_attr(not(unix), allow(dead_code))]
pub(crate) fn cached_snapshot_ref(project_root: &Path) -> Option<(String, Option<String>)> {
    manifest_snapshot(&read_cached_manifest(project_root)?)
}

/// Extract the snapshot source `(repo, env)` from raw manifest bytes. `None`
/// when there's no snapshot block or the bytes don't parse; `env` is `None` when
/// the block omits it.
#[cfg_attr(not(unix), allow(dead_code))]
fn manifest_snapshot(raw: &[u8]) -> Option<(String, Option<String>)> {
    let manifest: Manifest = serde_json::from_slice(raw).ok()?;
    let snap = manifest.snapshot?;
    Some((snap.repo, snap.env))
}

/// The project's `origin` remote URL from `.git/config`, if it's a git checkout
/// with an origin. A dependency-free minimal parse (no `git` binary needed); the
/// value is sent to the registry verbatim and normalized server-side.
fn git_remote(project_root: &Path) -> Option<String> {
    let text = std::fs::read_to_string(project_root.join(".git").join("config")).ok()?;
    parse_git_origin(&text)
}

/// Extract `[remote "origin"] url = …` from a `.git/config` body.
fn parse_git_origin(config: &str) -> Option<String> {
    let mut in_origin = false;
    for line in config.lines() {
        let t = line.trim();
        if t.starts_with('[') {
            in_origin = section_is_remote_origin(t);
            continue;
        }
        if in_origin
            && let Some((key, val)) = t.split_once('=')
            && key.trim().eq_ignore_ascii_case("url")
        {
            let url = val.trim();
            if !url.is_empty() {
                return Some(url.to_string());
            }
        }
    }
    None
}

/// Whether a `.git/config` section header is `[remote "origin"]` (the section
/// name is case-insensitive per git; the subsection `"origin"` is not).
fn section_is_remote_origin(header: &str) -> bool {
    let inner = header.trim_start_matches('[').trim_end_matches(']').trim();
    inner
        .get(..6)
        .is_some_and(|s| s.eq_ignore_ascii_case("remote") && inner[6..].trim() == "\"origin\"")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_round_trips_and_strips_trailing_slash() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("team.json");
        write_record_at(&path, "https://packages.acme.example/");
        let got = read_record_at(&path).expect("record written");
        assert_eq!(got.schema_version, RECORD_SCHEMA_VERSION);
        assert_eq!(got.registry, "https://packages.acme.example");
    }

    #[test]
    fn read_absent_is_none() {
        let dir = tempfile::tempdir().unwrap();
        assert!(read_record_at(&dir.path().join("nope.json")).is_none());
    }

    #[test]
    fn read_malformed_is_none() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("team.json");
        std::fs::write(&path, b"not json").unwrap();
        assert!(read_record_at(&path).is_none());
    }

    #[test]
    fn write_is_idempotent_noop_when_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("team.json");
        write_record_at(&path, "https://reg.example");
        let first = std::fs::read_to_string(&path).unwrap();
        // A same-registry rewrite must not change the bytes on disk.
        write_record_at(&path, "https://reg.example/");
        let second = std::fs::read_to_string(&path).unwrap();
        assert_eq!(first, second);
    }

    #[test]
    fn write_updates_when_registry_changes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("team.json");
        write_record_at(&path, "https://old.example");
        write_record_at(&path, "https://new.example");
        assert_eq!(
            read_record_at(&path).unwrap().registry,
            "https://new.example"
        );
    }

    #[test]
    fn parse_git_origin_reads_the_origin_url() {
        let cfg = "[core]\n\trepositoryformatversion = 0\n\
                   [remote \"origin\"]\n\turl = git@github.com:acme/shop.git\n\
                   \tfetch = +refs/heads/*:refs/remotes/origin/*\n\
                   [remote \"upstream\"]\n\turl = https://github.com/up/shop.git\n\
                   [branch \"main\"]\n\tremote = origin\n";
        assert_eq!(
            parse_git_origin(cfg).as_deref(),
            Some("git@github.com:acme/shop.git")
        );
    }

    #[test]
    fn parse_git_origin_none_without_an_origin() {
        assert!(parse_git_origin("[remote \"upstream\"]\n\turl = https://x/y.git\n").is_none());
        assert!(parse_git_origin("").is_none());
    }

    #[test]
    fn manifest_urls_extracts_repos_and_ignores_unknown_fields() {
        let raw = br#"{"schema_version":1,"org":"acme","remote":"github.com/acme/shop",
            "repositories":[{"org":"acme","repo":"web","url":"https://r/acme/web"},
            {"url":"https://r/acme/api"}],"services":{"php":"8.3"}}"#;
        assert_eq!(
            manifest_urls(raw),
            vec![
                "https://r/acme/web".to_string(),
                "https://r/acme/api".to_string()
            ]
        );
        assert!(manifest_urls(b"not json").is_empty());
        assert!(manifest_urls(br#"{"repositories":[]}"#).is_empty());
    }

    #[test]
    fn manifest_snapshot_reads_repo_and_optional_env() {
        let raw = br#"{"repositories":[],"snapshot":{"repo":"acme/data","env":"staging"}}"#;
        assert_eq!(
            manifest_snapshot(raw),
            Some(("acme/data".to_string(), Some("staging".to_string())))
        );
        // env is optional (defaults are applied downstream).
        assert_eq!(
            manifest_snapshot(br#"{"snapshot":{"repo":"acme/data"}}"#),
            Some(("acme/data".to_string(), None))
        );
        // No snapshot block, and unparseable bytes, both yield None.
        assert_eq!(manifest_snapshot(br#"{"repositories":[]}"#), None);
        assert_eq!(manifest_snapshot(b"not json"), None);
    }
}
