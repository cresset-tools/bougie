//! `bougie db status` — the project's database state at a glance, and the
//! staleness half of the snapshot-awareness guards: compare the local seed
//! marker's digest against the registry's latest (via the cheap
//! `snapshots/{env}/latest/info` metadata route — no dump download) and say
//! whether the local data is current or behind. **Informational only**: it
//! never reseeds on its own — the seed-once rule stays with the dev, who acts
//! with `bougie db refresh` when they want fresh data.

use std::process::ExitCode;
use std::time::Duration;

use bougie_cli::{DbStatusArgs, OutputFormat};
use bougie_composer_resolver::metadata::auth_origin;
use bougie_composer_resolver::update::read_bougie_bearer;
use bougie_paths::Paths;
use eyre::{Result, WrapErr};
use serde::Deserialize;

use super::super::service::config_mut::locate_project_root;
use crate::commands::team;

#[allow(
    clippy::needless_pass_by_value,
    reason = "owned args from clap-parsed CLI"
)]
pub fn run(_format: OutputFormat, args: DbStatusArgs) -> Result<ExitCode> {
    let paths = Paths::from_env()?;
    let project_root = locate_project_root()?;
    println!("bougie db status: {}", project_root.display());

    // The team's configured snapshot source (from the cached manifest), with
    // the same defaults `db pull` applies.
    let snap = team::cached_snapshot_ref(&project_root);
    match &snap {
        Some(s) => println!(
            "  source: {} {}/{} (team manifest)",
            s.repo,
            s.env.as_deref().unwrap_or("production"),
            s.profile.as_deref().unwrap_or("full"),
        ),
        None => println!("  source: none configured (no team snapshot source for this project)"),
    }

    // What `db pull` last cached.
    let pulled = super::pull::pulled_snapshot(&paths, &project_root);
    match &pulled {
        Some(p) => println!(
            "  pulled: {} {}/{} (sha256 {})",
            p.repo,
            p.environment,
            p.profile,
            &p.digest[..p.digest.len().min(16)],
        ),
        None => println!("  pulled: nothing (run `bougie db pull`)"),
    }

    // The seed marker — when the local database last got its data.
    let marker = super::seed::read_seed_marker(&super::seed::seed_marker_path(
        &paths,
        &project_root,
    ));
    match &marker {
        Some(m) => println!(
            "  seeded: {} (from {})",
            super::seed::human_age(super::seed::now_unix().saturating_sub(m.seeded_at_unix)),
            m.describe(),
        ),
        None => println!("  seeded: never (run `bougie db seed`)"),
    }

    if args.offline {
        return Ok(ExitCode::SUCCESS);
    }

    // Staleness: ask the registry for its latest snapshot's metadata and
    // compare digests. Best-effort — an unreachable registry degrades to a
    // note, never a failure (status must stay safe to run anywhere).
    match latest_info(&project_root, snap.as_ref()) {
        Ok(Some(info)) => {
            println!(
                "  registry: latest published {} (sha256 {}, {})",
                super::seed::human_age(
                    super::seed::now_unix().saturating_sub(info.created_at.max(0).unsigned_abs())
                ),
                &info.digest[..info.digest.len().min(16)],
                human_size_i64(info.size_bytes),
            );
            match marker.as_ref().and_then(|m| m.digest.as_deref()) {
                None if marker.is_none() => {
                    println!("  → not seeded yet — `bougie db seed` loads it");
                }
                None => println!(
                    "  → seeded from a local file, so no digest to compare against the registry"
                ),
                Some(d) if d == info.digest => {
                    println!("  → up to date: the local seed matches the registry's latest");
                }
                Some(_) => println!(
                    "  → behind: the registry has a newer snapshot — `bougie db refresh` \
                     reloads it (replaces local data)"
                ),
            }
        }
        Ok(None) => {}
        Err(e) => println!("  registry: unavailable ({e:#}); local state only"),
    }

    Ok(ExitCode::SUCCESS)
}

/// The metadata the registry serves at `snapshots/{env}/latest/info`.
#[derive(Debug, Deserialize)]
struct SnapshotInfo {
    digest: String,
    size_bytes: i64,
    /// Unix seconds of when the snapshot was published.
    created_at: i64,
}

/// Fetch the registry's latest-snapshot metadata for the manifest-configured
/// source. `Ok(None)` when there is nothing to check (not a team project, no
/// snapshot source, or logged out) — each with a printed note, since status is
/// a report, not a gate.
fn latest_info(
    project_root: &std::path::Path,
    snap: Option<&team::SnapshotRef>,
) -> Result<Option<SnapshotInfo>> {
    let Some(snap) = snap else {
        return Ok(None);
    };
    let Some(record) = team::read_record(project_root) else {
        println!("  registry: not wired (run `bougie login <registry>`); local state only");
        return Ok(None);
    };
    let base = record.registry.trim_end_matches('/').to_string();
    let host = auth_origin(&base);
    let Some(token) = read_bougie_bearer(&host) else {
        println!("  registry: logged out of {host} (run `bougie login {base}`); local state only");
        return Ok(None);
    };

    let (org, repo) = super::pull::parse_repo(&snap.repo)?;
    let env = snap.env.as_deref().unwrap_or("production");
    let profile = snap.profile.as_deref().unwrap_or("full");
    let mut url = format!("{base}/{org}/{repo}/snapshots/{env}/latest/info");
    if profile != "full" {
        url = format!("{url}?profile={profile}");
    }

    let client = reqwest::blocking::Client::builder()
        .user_agent(concat!("bougie/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(15))
        .build()
        .wrap_err("building the HTTP client")?;
    let resp = client
        .get(&url)
        .bearer_auth(&token)
        .send()
        .wrap_err("contacting the registry")?;
    if resp.status() == reqwest::StatusCode::NOT_FOUND {
        println!("  registry: no snapshot published for {org}/{repo} {env}/{profile}");
        return Ok(None);
    }
    if !resp.status().is_success() {
        return Err(eyre::eyre!("the registry answered {}", resp.status()));
    }
    let info: SnapshotInfo = resp.json().wrap_err("parsing the snapshot info")?;
    Ok(Some(info))
}

/// [`super::pull`]'s human size, for an `i64` wire value (negatives clamp to 0).
fn human_size_i64(bytes: i64) -> String {
    super::pull::human_size(bytes.max(0).unsigned_abs())
}
