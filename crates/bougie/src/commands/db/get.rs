//! `bougie db get` — pull a specific production row-graph (anonymized) into the
//! project's existing mariadb tenant, without a reseed: reproduce a prod ticket
//! locally in seconds. Wraps jibs's on-demand `get`.
//!
//! bougie's value-add is resolving the **local** tenant DSN (the same one `db
//! seed` loads into) and the project's `.jibs` config, then handing the rest to
//! jibs: `jibs get --host <source> --local-mysql <tenant> <config> -- <query>`.
//!
//! jibs `get` is SSH-to-source — it runs the anonymizing aggregate server-side
//! on the source host and streams the (anonymized) rows in — so the source
//! connection is supplied by the caller (`--host`/`--remote-mysql`, or the
//! `$BOUGIE_DBGET_*` fallbacks). A login-token hosted gateway that removes the
//! need for any source access on the laptop is future work.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use bougie_cli::{DbGetArgs, OutputFormat};
use bougie_paths::Paths;
use eyre::{Result, WrapErr, eyre};

use super::super::service::config_mut::locate_project_root;
use crate::commands::team::{self, ManifestSource};

const HOST_ENV: &str = "BOUGIE_DBGET_HOST";
const REMOTE_MYSQL_ENV: &str = "BOUGIE_DBGET_REMOTE_MYSQL";
const CONFIG_ENV: &str = "BOUGIE_DBGET_CONFIG";

#[allow(
    clippy::needless_pass_by_value,
    reason = "owned args from clap-parsed CLI"
)]
pub fn run(_format: OutputFormat, args: DbGetArgs) -> Result<ExitCode> {
    let paths = Paths::from_env()?;
    let project_root = locate_project_root()?;

    // Resolve everything cheap/offline first, so a config/host error beats the
    // (potentially slow) jibs fetch. A `--source <name>` selects a team-
    // configured connection from the cached manifest; explicit flags override
    // each of its fields, and the `$BOUGIE_DBGET_*` env vars are the last resort.
    let source = match &args.source {
        Some(name) => Some(resolve_source(team::cached_sources(&project_root), name)?),
        None => None,
    };
    let host = args
        .host
        .clone()
        .or_else(|| source.as_ref().map(|s| s.host.clone()))
        .or_else(|| env_nonempty(HOST_ENV))
        .ok_or_else(|| {
            eyre!(
                "no source host: pass `--host user@host`, `--source <name>` (a team-configured \
                 source), or set {HOST_ENV}. `bougie db get` runs jibs's anonymizing aggregate on \
                 the source host and streams the rows in."
            )
        })?;
    let config = resolve_config(&args, &project_root)?;
    let dsn = super::seed::mariadb_dsn(&paths, &project_root)?;
    let jibs = super::seed::ensure_jibs(&paths)?;

    let remote_mysql = args
        .remote_mysql
        .clone()
        .or_else(|| source.as_ref().and_then(|s| s.remote_mysql.clone()))
        .or_else(|| env_nonempty(REMOTE_MYSQL_ENV));
    let identity = args
        .identity
        .clone()
        .or_else(|| source.as_ref().and_then(|s| s.identity.clone()));
    let port = args.port.or_else(|| source.as_ref().and_then(|s| s.port));
    let argv = build_get_argv(&GetInvocation {
        host: &host,
        remote_mysql: remote_mysql.as_deref(),
        identity: identity.as_deref(),
        port,
        accept_new_host_keys: args.accept_new_host_keys,
        vars: &args.vars,
        local_mysql: &dsn,
        config: &config.to_string_lossy(),
        query: &args.query,
    });

    // jibs streams its own per-aggregate progress; inherit stdio.
    println!(
        "bougie db get: fetching `{}` from {host} into the mariadb tenant",
        args.query.join(" ")
    );
    let status = Command::new(&jibs)
        .args(&argv)
        .status()
        .wrap_err_with(|| format!("failed to execute jibs at {}", jibs.display()))?;

    // Mirror jibs's exit code so scripting sees the real outcome.
    let code = status
        .code()
        .and_then(|c| u8::try_from(c).ok())
        .unwrap_or(1);
    Ok(ExitCode::from(code))
}

/// Everything needed to build a `jibs get` invocation. Borrowed so [`build_get_argv`]
/// stays a pure, unit-testable function.
struct GetInvocation<'a> {
    host: &'a str,
    remote_mysql: Option<&'a str>,
    identity: Option<&'a str>,
    port: Option<u16>,
    accept_new_host_keys: bool,
    vars: &'a [String],
    local_mysql: &'a str,
    config: &'a str,
    query: &'a [String],
}

/// Build the argv passed to the `jibs` binary (no leading `jibs`). Options come
/// first, then the positional `<config>`, then `-- <query>` verbatim — matching
/// `jibs get [OPTIONS] --host <HOST> <CONFIG> -- <QUERIES>...`.
fn build_get_argv(inv: &GetInvocation) -> Vec<String> {
    let mut argv = vec!["get".to_string(), "--host".to_string(), inv.host.to_string()];
    if let Some(remote) = inv.remote_mysql {
        argv.push("--remote-mysql".to_string());
        argv.push(remote.to_string());
    }
    if let Some(identity) = inv.identity {
        argv.push("--identity".to_string());
        argv.push(identity.to_string());
    }
    if let Some(port) = inv.port {
        argv.push("--port".to_string());
        argv.push(port.to_string());
    }
    if inv.accept_new_host_keys {
        argv.push("--accept-new-host-keys".to_string());
    }
    for var in inv.vars {
        argv.push("--var".to_string());
        argv.push(var.clone());
    }
    argv.push("--local-mysql".to_string());
    argv.push(inv.local_mysql.to_string());
    argv.push(inv.config.to_string());
    argv.push("--".to_string());
    argv.extend(inv.query.iter().cloned());
    argv
}

/// Resolve the `.jibs` config: `--config` / `$BOUGIE_DBGET_CONFIG` if set,
/// otherwise the project's single `*.jibs` file. Errors on none/ambiguous.
fn resolve_config(args: &DbGetArgs, project_root: &Path) -> Result<PathBuf> {
    if let Some(explicit) = args.config.clone().or_else(|| env_nonempty(CONFIG_ENV)) {
        let path = PathBuf::from(explicit);
        if !path.is_file() {
            return Err(eyre!("jibs config not found: {}", path.display()));
        }
        return Ok(path);
    }
    let mut found = discover_jibs_configs(project_root)?;
    match found.len() {
        1 => Ok(found.pop().expect("len checked")),
        0 => Err(eyre!(
            "no `.jibs` config found in {}. Commit the team's config (e.g. `shop.jibs`), or pass \
             `--config <path>` (or set {CONFIG_ENV}).",
            project_root.display()
        )),
        _ => {
            let names: Vec<String> = found
                .iter()
                .map(|p| p.file_name().unwrap_or_default().to_string_lossy().into_owned())
                .collect();
            Err(eyre!(
                "multiple `.jibs` configs in {} ({}); pick one with `--config <path>`",
                project_root.display(),
                names.join(", ")
            ))
        }
    }
}

/// The `*.jibs` files directly under `dir`, sorted for a stable error message.
fn discover_jibs_configs(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut found = Vec::new();
    for entry in std::fs::read_dir(dir).wrap_err_with(|| format!("reading {}", dir.display()))? {
        let path = entry?.path();
        if path.is_file() && path.extension().is_some_and(|ext| ext.eq_ignore_ascii_case("jibs")) {
            found.push(path);
        }
    }
    found.sort();
    Ok(found)
}

fn env_nonempty(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.trim().is_empty())
}

/// Pick the named source out of the manifest's `sources` map. Errors — listing
/// the names that *are* available — when the source is unknown, so a typo is a
/// clean message rather than a puzzling missing-host error downstream.
fn resolve_source(
    mut sources: BTreeMap<String, ManifestSource>,
    name: &str,
) -> Result<ManifestSource> {
    if let Some(src) = sources.remove(name) {
        return Ok(src);
    }
    if sources.is_empty() {
        Err(eyre!(
            "no team database sources are configured for this project (looked for `{name}`). A \
             maintainer adds them with `sconce remote-source <remote> {name} --host …`; run \
             `bougie sync` to refresh the manifest. Or pass `--host user@host` directly."
        ))
    } else {
        let names: Vec<&str> = sources.keys().map(String::as_str).collect();
        Err(eyre!(
            "no team database source named `{name}` (available: {}). Pick one with `--source \
             <name>`, or pass `--host user@host` directly.",
            names.join(", ")
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_get_argv_minimal() {
        let argv = build_get_argv(&GetInvocation {
            host: "deploy@prod",
            remote_mysql: None,
            identity: None,
            port: None,
            accept_new_host_keys: false,
            vars: &[],
            local_mysql: "mysql://u:p@localhost/db?socket=/s.sock",
            config: "shop.jibs",
            query: &["order".to_string(), "--id".to_string(), "12345".to_string()],
        });
        assert_eq!(
            argv,
            vec![
                "get",
                "--host",
                "deploy@prod",
                "--local-mysql",
                "mysql://u:p@localhost/db?socket=/s.sock",
                "shop.jibs",
                "--",
                "order",
                "--id",
                "12345",
            ]
        );
    }

    #[test]
    fn build_get_argv_all_options_before_config_and_query() {
        let argv = build_get_argv(&GetInvocation {
            host: "deploy@prod",
            remote_mysql: Some("mysql://root@10.0.0.1:3306"),
            identity: Some("/home/u/.ssh/id"),
            port: Some(2222),
            accept_new_host_keys: true,
            vars: &["limit=100".to_string(), "since=2024".to_string()],
            local_mysql: "mysql://local",
            config: "cfg/shop.jibs",
            query: &["customer".to_string(), "--email".to_string(), "x@y.z".to_string()],
        });
        // Everything is an option until the positional config, then `-- query`.
        let cfg = argv.iter().position(|a| a == "cfg/shop.jibs").unwrap();
        let sep = argv.iter().position(|a| a == "--").unwrap();
        assert!(cfg < sep, "config must precede the `--` separator");
        assert_eq!(&argv[sep + 1..], &["customer", "--email", "x@y.z"]);
        for flag in ["--remote-mysql", "--identity", "--port", "--accept-new-host-keys", "--var"] {
            let at = argv.iter().position(|a| a == flag).unwrap();
            assert!(at < cfg, "{flag} must come before the config positional");
        }
        // Repeated --var is emitted once per assignment.
        assert_eq!(argv.iter().filter(|a| *a == "--var").count(), 2);
    }

    fn src(host: &str, port: Option<u16>) -> ManifestSource {
        ManifestSource {
            host: host.to_string(),
            remote_mysql: None,
            identity: None,
            port,
        }
    }

    #[test]
    fn resolve_source_selects_named_and_errors_on_unknown() {
        let mut sources = BTreeMap::new();
        sources.insert("production".to_string(), src("deploy@prod", Some(2201)));
        sources.insert("staging".to_string(), src("deploy@staging", None));

        // A known name resolves to its connection.
        let picked = resolve_source(sources.clone(), "staging").unwrap();
        assert_eq!(picked.host, "deploy@staging");
        let picked = resolve_source(sources.clone(), "production").unwrap();
        assert_eq!(picked.port, Some(2201));

        // An unknown name lists what's available, and names the typo.
        let err = resolve_source(sources, "prod").unwrap_err().to_string();
        assert!(err.contains("production"), "{err}");
        assert!(err.contains("staging"), "{err}");
        assert!(err.contains("`prod`"), "{err}");

        // No sources at all → the "configure them" message, not a name list.
        let err = resolve_source(BTreeMap::new(), "production").unwrap_err().to_string();
        assert!(err.contains("no team database sources"), "{err}");
    }

    #[test]
    fn discover_jibs_configs_finds_and_orders() {
        let tmp = tempfile::TempDir::new().unwrap();
        assert!(discover_jibs_configs(tmp.path()).unwrap().is_empty());
        std::fs::write(tmp.path().join("b.jibs"), "{}").unwrap();
        std::fs::write(tmp.path().join("a.jibs"), "{}").unwrap();
        std::fs::write(tmp.path().join("composer.json"), "{}").unwrap();
        let found = discover_jibs_configs(tmp.path()).unwrap();
        let names: Vec<_> = found
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["a.jibs", "b.jibs"]);
    }
}
