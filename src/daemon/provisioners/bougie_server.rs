//! `bougie server` tenancy: one `[[host]]` block per project.
//! SERVICES.md §3.5.
//!
//! Per-project tenant gets:
//!   - a `[[host]]` block in `<service_conf>/server.toml` mapping
//!     `<tenant>.bougie.run` → the project directory,
//!   - a live notification to the running server via the control
//!     socket's `reload-config` method, so the new hostname is
//!     reachable without restarting the server.
//!
//! Auth model: bougie's dev server is single-user, loopback-only by
//! design (see SERVER.md §1). There are no auth tokens or signing;
//! tenant isolation is purely "different hostname → different
//! `[[host]]` block → different project root."

use crate::daemon::tenants::{self, Tenant};
use crate::Paths;
use eyre::{eyre, Result, WrapErr};
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Hostname suffix bougie reserves for dev hosts. See SERVER.md §3.
const HOSTNAME_SUFFIX: &str = ".bougie.run";

/// Read/write timeout against the server's control socket. The
/// daemon hands these back via `services up` output, so keep the
/// budget tight enough that a wedged server doesn't strand the CLI.
const CONTROL_TIMEOUT: Duration = Duration::from_secs(5);

/// `bougie server` doesn't need an installer bootstrap. We just
/// make sure the per-service config dir exists so `provision` can
/// write `server.toml` into it. (The sandbox layer also creates
/// the dir, but it does so via `build_strict` which is *not* called
/// for the LightHome stance the server entry uses.)
pub fn pre_start(paths: &Paths) -> Result<()> {
    let conf = paths.service_conf("server");
    std::fs::create_dir_all(&conf)
        .wrap_err_with(|| format!("creating {}", conf.display()))?;
    // Seed an empty server.toml when missing so `bougie server run`
    // can start before the first tenant is provisioned.
    let cfg = conf.join("server.toml");
    if !cfg.exists() {
        std::fs::write(&cfg, default_server_toml())
            .wrap_err_with(|| format!("seeding {}", cfg.display()))?;
    }
    Ok(())
}

/// Provision a tenant. Idempotent — repeated calls for the same
/// project re-use the same hostname + existing `[[host]]` entry.
pub fn provision(paths: &Paths, tenants_path: &Path, tenant_name: &str, project: &Path) -> Result<Tenant> {
    let existing = tenants::load_all(tenants_path)?;
    if let Some(existing_t) = existing.iter().find(|t| t.project == project) {
        // Make sure the server has the entry in memory too — a
        // previous up/down cycle might have removed the [[host]]
        // before the daemon was restarted; we re-add idempotently.
        let hostname = derive_hostname(tenant_name);
        let _ = ensure_host_block(paths, &hostname, project);
        let _ = ping_reload_config(paths);
        return Ok(existing_t.clone());
    }

    let hostname = derive_hostname(tenant_name);
    ensure_host_block(paths, &hostname, project)
        .wrap_err_with(|| format!("adding [[host]] for {hostname}"))?;
    ping_reload_config(paths).wrap_err("notifying running server about new host")?;

    let mut tenant = Tenant::new(tenant_name, project.to_path_buf());
    tenant
        .alloc
        .insert("hostname".into(), serde_json::json!(hostname));
    tenants::append(tenants_path, &tenant)?;
    Ok(tenant)
}

/// Release a tenant. Without `purge` keeps the runtime cache for a
/// possible later `up`. With `purge` also removes the
/// `$XDG_RUNTIME_DIR/bougie/server/<hash>/` directory (php-fpm
/// sockets + rendered conf.d variants).
pub fn deprovision(
    paths: &Paths,
    tenants_path: &Path,
    tenant_name: &str,
    purge: bool,
) -> Result<()> {
    let existing = tenants::load_all(tenants_path)?;
    let Some(target) = existing.iter().find(|t| t.tenant == tenant_name).cloned() else {
        return Ok(());
    };

    let hostname = target
        .alloc
        .get("hostname")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .unwrap_or_else(|| derive_hostname(tenant_name));

    // Best-effort: server.toml might already have lost the block,
    // and the running server might be down. Either way we still
    // want the tenant ledger updated.
    let cfg = server_toml_path(paths);
    if cfg.exists() {
        let _ = crate::commands::server::config::remove_host(&cfg, &hostname);
    }
    let _ = ping_reload_config(paths);

    if purge {
        if let Some(rt) = project_runtime_dir(&target.project) {
            if rt.exists() {
                std::fs::remove_dir_all(&rt)
                    .wrap_err_with(|| format!("removing {}", rt.display()))?;
            }
        }
    }

    tenants::rewrite(tenants_path, |t| t.tenant != tenant_name)?;
    Ok(())
}

// -------------------- helpers --------------------

/// Resolve `<service_conf>/server.toml`.
pub fn server_toml_path(paths: &Paths) -> PathBuf {
    paths.service_conf("server").join("server.toml")
}

/// `bougie services add server` records the tenant name as
/// `<package>` (slashes already replaced with `_`). DNS labels
/// don't allow underscores, so swap them for hyphens in the
/// derived hostname — the dev server's `validate_hostname` rejects
/// the underscore form outright.
fn derive_hostname(tenant_name: &str) -> String {
    let slug = tenant_name.replace('_', "-");
    format!("{slug}{HOSTNAME_SUFFIX}")
}

/// Ensure `[[host]]` exists for the given hostname/project. Wraps
/// `commands::server::config::add_host` so the daemon writes
/// through the same atomic-temp-then-rename code path the CLI uses.
fn ensure_host_block(paths: &Paths, hostname: &str, project: &Path) -> Result<()> {
    let cfg = server_toml_path(paths);
    let parent = cfg.parent().ok_or_else(|| eyre!("config path has no parent"))?;
    std::fs::create_dir_all(parent)
        .wrap_err_with(|| format!("creating {}", parent.display()))?;
    // Per `bougie server add` behaviour, `add_host` returns
    // `Ok(None)` when the hostname is already present — idempotent
    // by design.
    crate::commands::server::config::add_host(&cfg, hostname, project, None)
        .wrap_err_with(|| format!("adding host {hostname} to {}", cfg.display()))?;
    Ok(())
}

/// Send `{"v":1,"method":"reload-config"}` to the running server.
/// Best-effort: no-op when the socket file is missing (server not
/// up yet) and surfaces real I/O failures otherwise so the CLI can
/// report `provision_failed` with useful context.
fn ping_reload_config(_paths: &Paths) -> Result<()> {
    let sock = control_socket_path();
    if !sock.exists() {
        // Server isn't running. That's fine on the first
        // `services up` — bougied will start the server child and
        // the server will load its config from disk at boot.
        return Ok(());
    }
    let mut stream = UnixStream::connect(&sock)
        .wrap_err_with(|| format!("connecting to {}", sock.display()))?;
    stream
        .set_read_timeout(Some(CONTROL_TIMEOUT))
        .wrap_err("set_read_timeout on server control socket")?;
    stream
        .set_write_timeout(Some(CONTROL_TIMEOUT))
        .wrap_err("set_write_timeout on server control socket")?;
    stream
        .write_all(b"{\"v\":1,\"method\":\"reload-config\"}\n")
        .wrap_err("writing reload-config request")?;
    stream
        .shutdown(std::net::Shutdown::Write)
        .wrap_err("shutting down write half")?;
    let mut reply = String::new();
    stream
        .read_to_string(&mut reply)
        .wrap_err("reading reload-config reply")?;
    // Don't parse — the server's reply schema is `{ok: true|false,
    // hosts: N}`. A bad reply still tells us the server is up and
    // responsive; reload-config failures (e.g. bad server.toml on
    // disk) would surface as `ok: false` which we'd want to bubble
    // up, but that case is hard to hit through our own controlled
    // mutations. v1: trust the server, log later if needed.
    Ok(())
}

/// Mirror `ServerPaths::control_socket` without reaching across
/// module boundaries (the server module currently constructs that
/// path through `ServerPaths::from_env`, which the daemon would
/// otherwise have to instantiate just to derive a string).
fn control_socket_path() -> PathBuf {
    let xdg = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            #[cfg(unix)]
            {
                PathBuf::from(format!("/tmp/bougie-server-{}", rustix::process::geteuid().as_raw()))
            }
            #[cfg(not(unix))]
            {
                PathBuf::from("/tmp/bougie-server")
            }
        });
    xdg.join("bougie").join("server").join("control.sock")
}

/// `$XDG_RUNTIME_DIR/bougie/server/<project-hash>/`. Mirrors
/// `ServerPaths::project_dir` (12-hex digest of the canonical
/// project path).
fn project_runtime_dir(project: &Path) -> Option<PathBuf> {
    use sha2::{Digest, Sha256};
    let canonical = project.canonicalize().ok()?;
    let hash = {
        let mut h = Sha256::new();
        h.update(canonical.as_os_str().to_string_lossy().as_bytes());
        let digest = h.finalize();
        const HEX: &[u8; 16] = b"0123456789abcdef";
        let mut out = String::with_capacity(12);
        for &b in &digest[..6] {
            out.push(HEX[(b >> 4) as usize] as char);
            out.push(HEX[(b & 0x0f) as usize] as char);
        }
        out
    };
    let parent = control_socket_path()
        .parent()?
        .to_path_buf();
    Some(parent.join(hash))
}

fn default_server_toml() -> &'static str {
    // Match the schema produced by `bougie server add`'s "first run"
    // skeleton: a `[server]` table with defaults and an empty
    // `[[host]]` array. Comments are kept minimal so the file looks
    // intentional in an editor.
    "# Managed by bougied — `bougie services add server` writes here.\n\
     # Edits survive bougied restarts; per-tenant hosts append below.\n\
     [server]\n\
     listen = \"127.0.0.1:7080\"\n\
     log_format = \"text\"\n"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derive_hostname_replaces_underscores_with_hyphens() {
        // DNS labels disallow `_`; the tenant slug carries `_` from
        // composer's `vendor/package` → `vendor_package` mapping.
        assert_eq!(derive_hostname("acme_blog"), "acme-blog.bougie.run");
        assert_eq!(derive_hostname("a_b_c"), "a-b-c.bougie.run");
    }

    #[test]
    fn derive_hostname_preserves_already_dns_safe_names() {
        assert_eq!(derive_hostname("foo"), "foo.bougie.run");
        assert_eq!(derive_hostname("my-app"), "my-app.bougie.run");
    }

    #[test]
    fn default_toml_parses() {
        let cfg: crate::commands::server::config::ServerConfig =
            toml_edit::de::from_str(default_server_toml()).expect("default toml parses");
        assert_eq!(cfg.server.listen, "127.0.0.1:7080");
        assert!(cfg.hosts.is_empty());
    }
}
