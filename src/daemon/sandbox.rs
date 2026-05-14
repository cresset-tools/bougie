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

use super::catalog::CatalogEntry;
use crate::Paths;
use eyre::{Result, WrapErr};
use sandbox_run::{ProtectHome, ProtectSystem, Sandbox, SandboxPolicy};

/// Build the policy for the given service. Creates the per-service
/// data/run/log/conf directories as a side effect so the
/// read_write_paths references resolve at spawn time.
pub fn build_policy(entry: &CatalogEntry, paths: &Paths) -> Result<SandboxPolicy> {
    let data = paths.service_data(entry.name);
    let run = paths.service_run(entry.name);
    let log = paths.service_log(entry.name);
    let conf = paths.service_conf(entry.name);
    for p in [&data, &run, &log, &conf] {
        std::fs::create_dir_all(p)
            .wrap_err_with(|| format!("creating {}", p.display()))?;
    }

    let (limit_nofile, limit_nproc) = rlimits_for(entry.name);

    Sandbox::new()
        .protect_system(ProtectSystem::Strict)
        .protect_home(ProtectHome::Yes)
        .read_write_paths([data.as_path(), run.as_path(), log.as_path()])
        .read_only_paths([paths.store().as_path(), conf.as_path()])
        .private_network(false)
        .no_new_privileges(true)
        .limit_nofile(limit_nofile)
        .limit_nproc(limit_nproc)
        .limit_core(0)
        .build()
        .map_err(|e| eyre::eyre!("building sandbox policy for {}: {e}", entry.name))
}

/// Per-service rlimit overrides (SERVICES.md §4).
fn rlimits_for(name: &str) -> (u64, u64) {
    match name {
        "opensearch" => (65_536, 256),
        "rabbitmq" => (4_096, 8_192),
        "mariadb" => (16_384, 256),
        _ => (4_096, 256),
    }
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
        // Sanity check on the rlimits_for table — the daemon would
        // surface OOM on opensearch indices if this regressed.
        assert_eq!(rlimits_for("opensearch").0, 65_536);
    }

    #[test]
    fn rabbitmq_gets_high_nproc_limit() {
        assert_eq!(rlimits_for("rabbitmq").1, 8_192);
    }

    #[test]
    fn default_service_gets_baseline_rlimits() {
        assert_eq!(rlimits_for("redis"), (4_096, 256));
    }
}
