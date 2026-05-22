//! Compile a `CatalogEntry` into a `sandbox_run::SandboxPolicy`.
//!
//! The default policy follows SERVICES.md §4: `ProtectSystem::Strict`,
//! `ProtectHome::Yes`, read-write under the service's own data/run/log
//! dirs, read-only on `\$BOUGIE_HOME/store` and the rendered config.
//! Per-entry overrides (`limit_nofile` etc.) come from
//! `CatalogEntry::sandbox_overrides` once those land in Phase 3+.
//!
//! For Phase 3 the policy is identical for every entry except for the
//! sandbox rlimit knobs hardcoded per service below (matching SERVICES
//! .md §4 explicitly). When richer per-entry overrides are needed,
//! extend `CatalogEntry` with a `SandboxOverrides` struct.

use super::catalog::{CatalogEntry, SandboxKind};
use bougie_paths::Paths;
use eyre::{Result, WrapErr};
use sandbox_run::{ProtectHome, ProtectSystem, Sandbox, SandboxPolicy};

/// Build the policy for the given service. Returns `None` when the
/// catalog asks for a policy that this platform can't implement
/// meaningfully (today: `SandboxKind::LightHome` on Linux, because
/// Landlock is allow-list-only and can't compose a permissive
/// baseline with fine-grained denies).
///
/// Creates the per-service data/run/log/conf directories as a side
/// effect so the `read_write_paths` references resolve at spawn time.
pub fn build_policy(entry: &CatalogEntry, paths: &Paths) -> Result<Option<SandboxPolicy>> {
    match entry.sandbox {
        SandboxKind::Strict => build_strict(entry, paths).map(Some),
        SandboxKind::LightHome => build_light_home(entry, paths),
    }
}

fn build_strict(entry: &CatalogEntry, paths: &Paths) -> Result<SandboxPolicy> {
    let data = paths.service_data(entry.name);
    let run = paths.service_run(entry.name);
    let log = paths.service_log(entry.name);
    let conf = paths.service_conf(entry.name);
    for p in [&data, &run, &log, &conf] {
        std::fs::create_dir_all(p)
            .wrap_err_with(|| format!("creating {}", p.display()))?;
    }

    let limit_nofile = nofile_for(entry.name);

    // Baseline writable device nodes. ProtectSystem::Strict makes the
    // entire FS read-only except for explicit RW additions, but POSIX
    // services expect to be able to write to `/dev/null` (shell `>/dev/null`,
    // `redirect-stderr` in launcher scripts, opensearch-env line 92) and
    // read from `/dev/{urandom,random}` (every TLS-using service).
    // Including them as RW is safe — these are char devices with kernel-
    // enforced semantics that don't honour write data, and Landlock
    // gates access at the path layer rather than the byte stream.
    // Per-service `conf` is rendered + owned by bougied; the original
    // read-only mode was defence-in-depth but breaks opensearch (its
    // launcher writes to `config/opensearch.keystore` and similar on
    // first start). Promote it to RW — the boundary against
    // user-input poisoning is still ProtectHome + the store being RO.
    let rw_paths = vec![
        data.clone(),
        run.clone(),
        log.clone(),
        conf.clone(),
        std::path::PathBuf::from("/dev/null"),
        std::path::PathBuf::from("/dev/zero"),
        std::path::PathBuf::from("/dev/full"),
        std::path::PathBuf::from("/dev/random"),
        std::path::PathBuf::from("/dev/urandom"),
    ];

    let mut policy = Sandbox::new()
        .protect_system(ProtectSystem::Strict)
        .protect_home(ProtectHome::Yes)
        .read_write_paths(rw_paths.iter().map(std::path::PathBuf::as_path))
        .read_only_paths([paths.store().as_path()])
        .private_network(false)
        .no_new_privileges(true)
        .limit_nofile(limit_nofile)
        .limit_core(0);

    // Deliberately do NOT cap NPROC. On Linux, RLIMIT_NPROC counts the
    // calling user's *total* live processes, not this service's
    // descendants — setting it lower than the user's current process
    // count causes mariadbd's `timer_create()` (and InnoDB threads, and
    // Erlang's scheduler) to fail with EAGAIN on workstations where the
    // desktop session is already over a few hundred processes. The
    // upstream sandbox-run README warns about this. cgroups `pids.max`
    // is the right knob if we ever need it; setrlimit isn't.
    if let Some(nproc) = nproc_for(entry.name) {
        policy = policy.limit_nproc(nproc);
    }

    policy
        .build()
        .map_err(|e| eyre::eyre!("building sandbox policy for {}: {e}", entry.name))
}

/// Loose sandbox: keep the user's normal FS access so the service
/// can read project files / spawn helper processes, but deny the
/// sensitive home subdirs (`~/.ssh`, `~/.aws`, `~/.gnupg`,
/// `~/.gitconfig`).
///
/// On macOS this maps to SBPL deny rules and Just Works™. On Linux
/// it's a no-op (returns `None`) because Landlock is allow-list-only
/// — the only way to "block a subpath" is to enumerate every other
/// path as allowed, which we can't do for an arbitrary user home.
/// Code that needs cross-platform deny semantics should switch to a
/// cgroups + LSM-hook approach when we have it.
#[cfg(not(target_os = "macos"))]
fn build_light_home(entry: &CatalogEntry, paths: &Paths) -> Result<Option<SandboxPolicy>> {
    // Still create the per-service dirs so paths the supervisor
    // assumes (data/run/log/conf) are present.
    for p in [
        paths.service_data(entry.name),
        paths.service_run(entry.name),
        paths.service_log(entry.name),
        paths.service_conf(entry.name),
    ] {
        std::fs::create_dir_all(&p)
            .wrap_err_with(|| format!("creating {}", p.display()))?;
    }
    Ok(None)
}

#[cfg(target_os = "macos")]
fn build_light_home(entry: &CatalogEntry, paths: &Paths) -> Result<Option<SandboxPolicy>> {
    for p in [
        paths.service_data(entry.name),
        paths.service_run(entry.name),
        paths.service_log(entry.name),
        paths.service_conf(entry.name),
    ] {
        std::fs::create_dir_all(&p)
            .wrap_err_with(|| format!("creating {}", p.display()))?;
    }
    let home = std::env::var_os("HOME")
        .map(std::path::PathBuf::from)
        .ok_or_else(|| eyre::eyre!("HOME not set; can't compute light-home deny paths"))?;
    let denied: Vec<std::path::PathBuf> = [".ssh", ".aws", ".gnupg", ".gitconfig", ".netrc"]
        .iter()
        .map(|sub| home.join(sub))
        .collect();
    let policy = Sandbox::new()
        .protect_system(ProtectSystem::No)
        .protect_home(ProtectHome::No)
        .inaccessible_paths(denied.iter().map(std::path::PathBuf::as_path))
        .private_network(false)
        .no_new_privileges(true)
        .limit_core(0)
        .build()
        .map_err(|e| eyre::eyre!("building light-home sandbox policy for {}: {e}", entry.name))?;
    Ok(Some(policy))
}

/// Per-service open-file limit. `mariadb` 11.4 computes
/// `open_files_limit = max_connections + table_open_cache*2 + 10` at
/// startup, which lands above 32k with stock settings — give it 65k
/// so we don't spam "Could not increase number of `max_open_files`"
/// into the log. `opensearch`'s many index shards also push past
/// the default cap.
fn nofile_for(name: &str) -> u64 {
    match name {
        // rabbitmq joins this list because Erlang's port driver
        // pre-allocates 65k port slots and chokes on EMFILE if the
        // soft limit is below that ceiling.
        "opensearch" | "mariadb" | "rabbitmq" => 65_536,
        _ => 4_096,
    }
}

/// Per-service NPROC cap. Returns `None` (no cap applied) for every
/// service that uses threads or timers, which is most of them on
/// Linux. Kept as a hook so a future ulimit-aware mode can re-introduce
/// a cap if it ever becomes useful.
fn nproc_for(_name: &str) -> Option<u64> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::catalog;

    #[test]
    fn building_a_policy_creates_service_dirs() {
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = Paths::new(tmp.path().into(), tmp.path().into());
        let entry = catalog::find("redis").unwrap();
        let _policy = build_policy(entry, &paths).expect("policy build");
        assert!(paths.service_data("redis").is_dir());
        assert!(paths.service_run("redis").is_dir());
        assert!(paths.service_log("redis").is_dir());
        assert!(paths.service_conf("redis").is_dir());
    }

    #[test]
    fn opensearch_gets_high_nofile_limit() {
        // Sanity check — opensearch's many index shards each chew a
        // descriptor; regressing this would surface as IOErrors.
        assert_eq!(nofile_for("opensearch"), 65_536);
    }

    #[test]
    fn mariadb_gets_elevated_nofile_limit() {
        assert_eq!(nofile_for("mariadb"), 65_536);
    }

    #[test]
    fn default_service_gets_baseline_nofile() {
        assert_eq!(nofile_for("redis"), 4_096);
    }

    #[test]
    fn nproc_cap_is_disabled_for_every_service() {
        // RLIMIT_NPROC counts the calling user's total processes, not
        // a per-service budget — capping it on a desktop session
        // breaks mariadb/erlang/opensearch threading. Keep this off
        // until we have a cgroup-based pids.max story.
        for name in ["redis", "mariadb", "opensearch", "rabbitmq", "server"] {
            assert!(nproc_for(name).is_none(), "{name} should not cap nproc");
        }
    }
}
