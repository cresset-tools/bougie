//! `bougie services exec` + the service-client argv[0] shims.
//!
//! Runs a service's *client* tool (mariadb, mysqldump, redis-cli,
//! rabbitmqctl, …) wired to the current project's tenant. Two entry
//! points share the wiring:
//!
//! - [`run_shim`] — the `vendor/bougie/bin/<tool>` symlinks written by
//!   sync for the project's declared services (see
//!   `shim::Role::ServiceClient`). Because `bougie run`, composer
//!   scripts, and recipes all front-load that dir on PATH, anything
//!   that shells out to `mysqldump` by name gets the version-matched,
//!   tenant-wired client for free.
//! - [`run`] — `bougie services exec <tool> [args…]`, the generic
//!   escape hatch that also reaches uncurated binaries in a declared
//!   service's `bin/`/`sbin/` (mariadb-check, opensearch-plugin, …).
//!
//! Connection info is assembled offline from the tenant ledger +
//! derived password (the same sources `bougie projects list` and the
//! `PhpStorm` data-source writer read) — no daemon round-trip. The
//! service itself still has to be running for the tool to connect,
//! but constructing the invocation never depends on `bougied`.

use bougie_daemon::daemon::catalog::{self, Binding, CatalogEntry};
use bougie_daemon::daemon::provisioners::rabbitmq::rabbitmq_env;
use bougie_daemon::daemon::tenants::{self, Tenant};
use bougie_daemon::daemon::{credentials, store_layout};
use bougie_paths::Paths;
use eyre::{eyre, Result, WrapErr};
use std::ffi::OsString;
use std::io::Write as _;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

/// `bougie services exec [--service NAME] TOOL [ARGS…]`.
#[allow(
    clippy::needless_pass_by_value,
    reason = "owned strings from clap-parsed CLI"
)]
pub fn run(service: Option<String>, tool: String, args: Vec<OsString>) -> Result<ExitCode> {
    // A path separator in the tool name would let `join` escape the
    // service's bin/ — only bare basenames are addressable.
    if tool.contains('/') || tool.is_empty() {
        return Err(eyre!("tool name must be a bare binary name (got `{tool}`)"));
    }
    let project_root = super::config_mut::locate_project_root()?;
    let paths = Paths::from_env()?;
    let project = bougie_config::load_project(&project_root)?;
    let declared: Vec<&str> = project.bougie.services.keys().map(String::as_str).collect();

    let (entry, binary) = resolve_tool(&paths, &declared, service.as_deref(), &tool)?;
    exec_wired(&paths, &project_root, entry, &binary, &args)
}

/// Entry point for the `vendor/bougie/bin/<tool>` shims. `service` and
/// `tool` come from the catalog lookup in `shim::role_from_argv0`, so
/// both are known-good catalog names.
pub fn run_shim(
    service: &str,
    tool: &str,
    project_root: &Path,
    args: &[OsString],
) -> Result<ExitCode> {
    let entry = catalog::find(service)
        .ok_or_else(|| eyre!("BUG: shim references unknown service `{service}`"))?;
    let client = entry
        .clients
        .iter()
        .find(|c| c.name == tool)
        .ok_or_else(|| eyre!("BUG: shim references unknown client `{tool}` of `{service}`"))?;
    let paths = Paths::from_env()?;
    let binary = client_binary(&paths, entry, client.path, tool)?;
    exec_wired(&paths, project_root, entry, &binary, args)
}

// -------------------- resolution --------------------

/// Resolve `tool` to a service + on-disk binary. Curated clients win;
/// otherwise every declared service's `bin/`/`sbin/` is searched.
fn resolve_tool(
    paths: &Paths,
    declared: &[&str],
    service_hint: Option<&str>,
    tool: &str,
) -> Result<(&'static CatalogEntry, PathBuf)> {
    if let Some(hint) = service_hint {
        let entry = catalog::find(hint).ok_or_else(|| {
            eyre!(
                "unknown service `{hint}`; known: {}",
                catalog::user_facing_names()
            )
        })?;
        if !declared.contains(&hint) {
            return Err(eyre!(
                "service `{hint}` isn't declared in this project \
                 (try `bougie services add {hint}` first)"
            ));
        }
        let binary = if let Some(c) = entry.clients.iter().find(|c| c.name == tool) {
            client_binary(paths, entry, c.path, tool)?
        } else {
            discover_binary(paths, entry, tool)?.ok_or_else(|| {
                eyre!(
                    "no `{tool}` in {hint}'s bin/ or sbin/ \
                     (is the service installed? run `bougie up {hint}`)"
                )
            })?
        };
        return Ok((entry, binary));
    }

    // Curated lookup first — it works even before the tarball is on
    // disk, so the error can say "run `bougie up`" instead of "not
    // found".
    if let Some((entry, client)) = catalog::find_client(tool) {
        if !declared.contains(&entry.name) {
            return Err(eyre!(
                "`{tool}` belongs to service `{name}`, which isn't declared in this \
                 project — run `bougie services add {name}` first",
                name = entry.name
            ));
        }
        return Ok((entry, client_binary(paths, entry, client.path, tool)?));
    }

    // Fallback: scan the declared services' store trees.
    let mut found: Vec<(&'static CatalogEntry, PathBuf)> = Vec::new();
    for name in declared {
        let Some(entry) = catalog::find(name) else {
            continue;
        };
        if let Ok(Some(bin)) = discover_binary(paths, entry, tool) {
            found.push((entry, bin));
        }
    }
    match found.len() {
        1 => Ok(found.pop().expect("len checked")),
        0 => Err(eyre!(
            "no client tool `{tool}` found in this project's declared services ({}); \
             services not yet started haven't been downloaded — try `bougie up` first, \
             or `--service <name>` to say where to look",
            if declared.is_empty() {
                "none declared".to_string()
            } else {
                declared.join(", ")
            }
        )),
        _ => Err(eyre!(
            "`{tool}` exists in multiple services ({}); pass --service to pick one",
            found
                .iter()
                .map(|(e, _)| e.name)
                .collect::<Vec<_>>()
                .join(", ")
        )),
    }
}

/// Locate a curated client inside the service's store tree, with a
/// user-facing error when the tarball isn't fetched yet (the store is
/// populated on first `bougie up`).
fn client_binary(
    paths: &Paths,
    entry: &'static CatalogEntry,
    rel_path: &str,
    tool: &str,
) -> Result<PathBuf> {
    let basedir = store_layout::basedir(paths, entry).map_err(|_| {
        eyre!(
            "{tool}: service `{name}` isn't installed yet — run `bougie up {name}` \
             once to fetch it",
            name = entry.name
        )
    })?;
    let bin = basedir.join(rel_path);
    if !bin.is_file() {
        return Err(eyre!(
            "{tool}: expected at {} but missing — is the {} tarball complete?",
            bin.display(),
            entry.name
        ));
    }
    Ok(bin)
}

/// Look for an uncurated `tool` in the service's `bin/` or `sbin/`.
/// `Ok(None)` when the store tree exists but has no such binary;
/// errors only bubble for a missing store tree (caller decides how to
/// phrase that).
fn discover_binary(
    paths: &Paths,
    entry: &'static CatalogEntry,
    tool: &str,
) -> Result<Option<PathBuf>> {
    let basedir = store_layout::basedir(paths, entry)?;
    for sub in ["bin", "sbin"] {
        let p = basedir.join(sub).join(tool);
        if p.is_file() {
            return Ok(Some(p));
        }
    }
    Ok(None)
}

// -------------------- tenant wiring --------------------

/// Exec `binary` with per-service tenant wiring injected. On success
/// this never returns (`execve`).
fn exec_wired(
    paths: &Paths,
    project_root: &Path,
    entry: &'static CatalogEntry,
    binary: &Path,
    args: &[OsString],
) -> Result<ExitCode> {
    let mut cmd = Command::new(binary);
    match entry.name {
        "mariadb" => {
            cmd.args(mariadb_wiring(paths, project_root, entry, args)?);
        }
        "redis" => {
            cmd.args(redis_wiring(paths, project_root, entry, args)?);
        }
        "rabbitmq" => {
            apply_rabbitmq_env(&mut cmd, paths);
            cmd.args(args);
        }
        _ => {
            cmd.args(args);
        }
    }
    let err = cmd.exec();
    Err(err).wrap_err_with(|| format!("exec {}", binary.display()))
}

/// The project's tenant row for `service`, matched the way
/// `projects purge` matches: by canonical path first, raw path second
/// (the ledger stores whatever spelling the daemon was handed).
fn find_tenant(paths: &Paths, service: &str, project_root: &Path) -> Result<Option<Tenant>> {
    let rows = tenants::load_all_sync(&paths.service_tenants(service))?;
    let canon = project_root
        .canonicalize()
        .unwrap_or_else(|_| project_root.to_path_buf());
    Ok(rows
        .into_iter()
        .find(|t| t.project == canon || t.project == project_root))
}

fn no_tenant_err(service: &str) -> eyre::Report {
    eyre!("no {service} tenant is provisioned for this project — run `bougie up {service}` first")
}

/// The service's Unix socket path, when its catalog binding is one.
fn socket_path(paths: &Paths, entry: &CatalogEntry) -> Option<PathBuf> {
    match entry.binding {
        Binding::UnixSocket { sockname } => Some(paths.service_run(entry.name).join(sockname)),
        Binding::Tcp { .. } | Binding::None => None,
    }
}

// ---------- mariadb ----------

/// Build the argv for a mariadb-family client: a per-tenant
/// `--defaults-extra-file` carrying socket/user/password (and default
/// database for the interactive client) prepended ahead of the user's
/// args. `--defaults-*` options must come first on the mysql command
/// line, so when the caller already manages defaults themselves we
/// inject nothing. Explicit user flags override option-file values,
/// so third-party code passing its own `--host`/`-u` is unharmed.
fn mariadb_wiring(
    paths: &Paths,
    project_root: &Path,
    entry: &'static CatalogEntry,
    args: &[OsString],
) -> Result<Vec<OsString>> {
    if manages_own_defaults(args) {
        return Ok(args.to_vec());
    }
    let tenant =
        find_tenant(paths, entry.name, project_root)?.ok_or_else(|| no_tenant_err(entry.name))?;
    let socket = socket_path(paths, entry)
        .ok_or_else(|| eyre!("BUG: mariadb catalog entry lost its socket binding"))?;
    // Prefer the ledger's recorded password; fall back to deriving it
    // (same function the provisioner used, so they agree).
    let password = match tenant.secrets.get("password") {
        Some(p) => p.clone(),
        None => credentials::derive_password(paths, entry.name, &tenant.project)?,
    };
    let cnf = write_client_cnf(paths, &tenant.tenant, &socket, &password)?;
    let mut out = Vec::with_capacity(args.len() + 1);
    let mut flag = OsString::from("--defaults-extra-file=");
    flag.push(&cnf);
    out.push(flag);
    out.extend(args.iter().cloned());
    Ok(out)
}

/// True when the caller's first arg is a `--defaults-*` /
/// `--no-defaults` option (which mysql clients require to be the
/// *first* argument — injecting ours ahead of it would error).
fn manages_own_defaults(args: &[OsString]) -> bool {
    args.first().is_some_and(|a| {
        let s = a.to_string_lossy();
        s == "--no-defaults" || s.starts_with("--defaults-")
    })
}

/// Write (or refresh) the per-tenant client option file, mode 0600
/// (it carries the tenant password). Regenerated on every invocation
/// so it can never go stale against the ledger, and works for tenants
/// provisioned before this feature existed.
fn write_client_cnf(paths: &Paths, tenant: &str, socket: &Path, password: &str) -> Result<PathBuf> {
    let dir = paths.service_conf("mariadb").join("clients");
    std::fs::create_dir_all(&dir).wrap_err_with(|| format!("creating {}", dir.display()))?;
    let path = dir.join(format!("{tenant}.cnf"));
    let content = client_cnf_content(socket, tenant, password);
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&path)
        .wrap_err_with(|| format!("creating {}", path.display()))?;
    f.write_all(content.as_bytes())
        .wrap_err_with(|| format!("writing {}", path.display()))?;
    // `mode(0o600)` only applies at creation; re-assert on refresh in
    // case an older file was created looser.
    let perms = std::os::unix::fs::PermissionsExt::from_mode(0o600);
    std::fs::set_permissions(&path, perms).wrap_err_with(|| format!("chmod {}", path.display()))?;
    Ok(path)
}

/// mariadb/mysqldump/mysqladmin all read `[client]`; `[mysql]` adds
/// the default database for the interactive client only (mysqldump
/// takes the database positionally). The tenant's database and user
/// are both the tenant name. The socket path is quoted — option-file
/// values may contain spaces.
fn client_cnf_content(socket: &Path, tenant: &str, password: &str) -> String {
    format!(
        "# Written by bougie for the `{tenant}` tenant; regenerated on every\n\
         # client invocation — do not edit.\n\
         [client]\n\
         socket=\"{socket}\"\n\
         user={tenant}\n\
         password={password}\n\
         \n\
         [mysql]\n\
         database={tenant}\n",
        socket = socket.display(),
    )
}

// ---------- redis ----------

/// Prepend `-s <socket> -n <db>` for the project's tenant, unless the
/// caller aims the client elsewhere.
fn redis_wiring(
    paths: &Paths,
    project_root: &Path,
    entry: &'static CatalogEntry,
    args: &[OsString],
) -> Result<Vec<OsString>> {
    let socket = socket_path(paths, entry)
        .ok_or_else(|| eyre!("BUG: redis catalog entry lost its socket binding"))?;
    let tenant =
        find_tenant(paths, entry.name, project_root)?.ok_or_else(|| no_tenant_err(entry.name))?;
    let db = tenant
        .alloc
        .get("db_number")
        .and_then(serde_json::Value::as_u64);
    Ok(redis_args(&socket, db, args))
}

/// Pure argv builder for redis-cli (unit-tested). Endpoint flags in
/// the user's args (`-s`/`-h`/`-u`/`--cluster`) mean they're driving
/// the connection themselves — inject nothing, since redis-cli merges
/// repeated connection flags rather than letting the last one win
/// cleanly. An explicit `-n` alone still gets the tenant socket.
fn redis_args(socket: &Path, db: Option<u64>, args: &[OsString]) -> Vec<OsString> {
    let has = |flag: &str| args.iter().any(|a| a == flag);
    let mut out = Vec::with_capacity(args.len() + 4);
    if !(has("-s") || has("-h") || has("-u") || has("--cluster")) {
        out.push("-s".into());
        out.push(socket.as_os_str().to_owned());
        if let Some(db) = db
            && !has("-n")
        {
            out.push("-n".into());
            out.push(db.to_string().into());
        }
    }
    out.extend(args.iter().cloned());
    out
}

// ---------- rabbitmq ----------

/// Point the ctl-family scripts at bougie's private node: strip any
/// inherited `RABBITMQ_*` (a stale shell var must not aim us at a
/// foreign broker), then set the same env the supervisor spawns the
/// server with, plus the `HOME` holding the Erlang cookie. Unlike the
/// daemon's own `env_clear()` ctl calls, the rest of the user's env
/// (TERM, LANG, PATH…) is kept — this is an interactive tool.
fn apply_rabbitmq_env(cmd: &mut Command, paths: &Paths) {
    for (k, _) in std::env::vars_os() {
        if k.to_string_lossy().starts_with("RABBITMQ_") {
            cmd.env_remove(&k);
        }
    }
    cmd.env("HOME", paths.service_data("rabbitmq").join("home"));
    cmd.envs(rabbitmq_env(paths));
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn os(args: &[&str]) -> Vec<OsString> {
        args.iter().map(OsString::from).collect()
    }

    // ---------- mariadb ----------

    #[test]
    fn cnf_content_carries_socket_user_password_and_database() {
        let text = client_cnf_content(Path::new("/run/mariadb.sock"), "acme_blog", "deadbeef");
        assert!(
            text.contains("[client]\nsocket=\"/run/mariadb.sock\"\n"),
            "{text}"
        );
        assert!(text.contains("user=acme_blog\n"), "{text}");
        assert!(text.contains("password=deadbeef\n"), "{text}");
        // Default database only for the interactive client.
        assert!(text.contains("[mysql]\ndatabase=acme_blog\n"), "{text}");
    }

    #[test]
    fn own_defaults_flag_suppresses_injection() {
        assert!(manages_own_defaults(&os(&[
            "--no-defaults",
            "-e",
            "select 1"
        ])));
        assert!(manages_own_defaults(&os(&["--defaults-file=/x.cnf"])));
        assert!(manages_own_defaults(&os(&["--defaults-extra-file=/x.cnf"])));
        // Only position 0 counts — mysql requires defaults options first,
        // so a later occurrence is the client's own error to report.
        assert!(!manages_own_defaults(&os(&["-e", "select 1"])));
        assert!(!manages_own_defaults(&os(&[])));
        assert!(!manages_own_defaults(&os(&["somedb", "--no-defaults"])));
    }

    #[test]
    fn client_cnf_is_written_0600_and_refreshed() {
        use std::os::unix::fs::PermissionsExt;
        let td = TempDir::new().unwrap();
        let paths = Paths::new(td.path().into(), td.path().join("cache"));
        let sock = Path::new("/run/mariadb.sock");

        let p = write_client_cnf(&paths, "acme", sock, "pw1").unwrap();
        let mode = std::fs::metadata(&p).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        assert!(
            std::fs::read_to_string(&p)
                .unwrap()
                .contains("password=pw1")
        );

        // Refresh replaces content in place.
        let p2 = write_client_cnf(&paths, "acme", sock, "pw2").unwrap();
        assert_eq!(p, p2);
        assert!(
            std::fs::read_to_string(&p)
                .unwrap()
                .contains("password=pw2")
        );
    }

    // ---------- redis ----------

    #[test]
    fn redis_injects_socket_and_db() {
        let got = redis_args(Path::new("/run/redis.sock"), Some(3), &os(&["KEYS", "*"]));
        assert_eq!(got, os(&["-s", "/run/redis.sock", "-n", "3", "KEYS", "*"]));
    }

    #[test]
    fn redis_keeps_user_db_choice() {
        let got = redis_args(Path::new("/run/redis.sock"), Some(3), &os(&["-n", "9"]));
        assert_eq!(got, os(&["-s", "/run/redis.sock", "-n", "9"]));
    }

    #[test]
    fn redis_endpoint_flags_suppress_all_injection() {
        for flags in [
            &["-h", "other.host"][..],
            &["-s", "/tmp/x.sock"],
            &["-u", "redis://x"],
            &["--cluster", "info"],
        ] {
            let got = redis_args(Path::new("/run/redis.sock"), Some(3), &os(flags));
            assert_eq!(got, os(flags), "flags {flags:?} should suppress injection");
        }
    }

    #[test]
    fn redis_without_allocation_still_gets_socket() {
        let got = redis_args(Path::new("/run/redis.sock"), None, &os(&["PING"]));
        assert_eq!(got, os(&["-s", "/run/redis.sock", "PING"]));
    }

    // ---------- tenant lookup ----------

    #[test]
    fn find_tenant_matches_project_by_path() {
        let td = TempDir::new().unwrap();
        let paths = Paths::new(td.path().into(), td.path().join("cache"));
        let proj = td.path().join("proj");
        std::fs::create_dir_all(&proj).unwrap();

        let ledger = paths.service_tenants("redis");
        std::fs::create_dir_all(ledger.parent().unwrap()).unwrap();
        let row = format!(
            "{}\n",
            serde_json::json!({
                "schema_version": 1,
                "tenant": "proj",
                "project": proj.canonicalize().unwrap(),
                "created_at": "2026-01-01T00:00:00Z",
                "alloc": {"db_number": 5},
            })
        );
        std::fs::write(&ledger, row).unwrap();

        let t = find_tenant(&paths, "redis", &proj).unwrap().unwrap();
        assert_eq!(t.tenant, "proj");
        assert_eq!(t.alloc["db_number"], 5);
        // A different project resolves to no tenant.
        let other = td.path().join("other");
        std::fs::create_dir_all(&other).unwrap();
        assert!(find_tenant(&paths, "redis", &other).unwrap().is_none());
    }

    // ---------- discovery ----------

    #[test]
    fn discover_binary_searches_bin_then_sbin() {
        let td = TempDir::new().unwrap();
        let paths = Paths::new(td.path().into(), td.path().join("cache"));
        let entry = catalog::find("rabbitmq").unwrap();
        let base = paths.store().join(entry.tarball);
        std::fs::create_dir_all(base.join("sbin")).unwrap();
        std::fs::write(base.join("sbin/rabbitmq-queues"), "#!/bin/sh\n").unwrap();

        let found = discover_binary(&paths, entry, "rabbitmq-queues")
            .unwrap()
            .unwrap();
        assert!(found.ends_with("sbin/rabbitmq-queues"));
        assert!(discover_binary(&paths, entry, "nope").unwrap().is_none());
    }

    #[test]
    fn resolve_rejects_undeclared_curated_service() {
        let td = TempDir::new().unwrap();
        let paths = Paths::new(td.path().into(), td.path().join("cache"));
        let err = resolve_tool(&paths, &["redis"], None, "mysqldump").unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("services add mariadb"), "{msg}");
    }

    #[test]
    fn resolve_unknown_tool_lists_declared_services() {
        let td = TempDir::new().unwrap();
        let paths = Paths::new(td.path().into(), td.path().join("cache"));
        let err = resolve_tool(&paths, &["redis", "mariadb"], None, "wat").unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("redis, mariadb"), "{msg}");
    }
}
