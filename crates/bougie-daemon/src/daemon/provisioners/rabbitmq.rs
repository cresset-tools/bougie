//! `RabbitMQ` tenancy: per-tenant vhost + user. SERVICES.md §3.5.
//!
//! Per-project tenant gets:
//!   - a vhost named `<tenant>`,
//!   - a user named `<tenant>` with a randomly-generated password,
//!   - full configure/write/read permission on that vhost only.
//!
//! Auth model: rabbitmq is dev-only, loopback-only. The `<tenant>`
//! user is the only credential a project ever needs; the default
//! `guest` user (rabbitmq's stock account) is left untouched because
//! it can't reach 127.0.0.1 from outside the loopback anyway.
//!
//! The bougie-index rabbitmq tarball ships its own bundled erlang at
//! `<basedir>/erlang/`. `sbin/rabbitmq-env` prepends that to PATH
//! before sourcing the rest of its config, so no separate erlang
//! install or symlink wiring is needed at the supervisor layer.

use crate::daemon::{store_layout, tenants::{self, Tenant}};
use bougie_paths::Paths;
use eyre::{eyre, Result, WrapErr};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::process::Command;
use tokio::time::Instant;

/// How long to wait for rabbitmq to come fully online before the
/// first rabbitmqctl call. The supervisor's TCP probe wins as soon
/// as `inet_tcp_listener:5672` binds, but the broker still needs
/// another moment to load mnesia + finish boot before `add_vhost`
/// works.
const RABBITMQCTL_READY_TIMEOUT: Duration = Duration::from_mins(1);

/// rabbitmq pre-start hook. Creates the directories rabbitmq writes
/// to under our RW allowlist. No bootstrap step — rabbitmq creates
/// its own mnesia + log files on first start.
pub async fn pre_start(paths: &Paths) -> Result<()> {
    for p in [
        paths.service_data("rabbitmq"),
        paths.service_data("rabbitmq").join("mnesia"),
        paths.service_data("rabbitmq").join("home"),
        paths.service_log("rabbitmq"),
        paths.service_run("rabbitmq"),
        paths.service_conf("rabbitmq"),
    ] {
        tokio::fs::create_dir_all(&p)
            .await
            .wrap_err_with(|| format!("creating {}", p.display()))?;
    }
    Ok(())
}

/// Provision a tenant. Idempotent — re-running for the same project
/// re-uses the existing vhost/user (rabbitmqctl returns non-zero on
/// "already exists", which we treat as success).
pub async fn provision(
    paths: &Paths,
    tenants_path: &Path,
    tenant_name: &str,
    project: &Path,
) -> Result<Tenant> {
    let existing = tenants::load_all(tenants_path).await?;
    if let Some(existing_t) = existing.iter().find(|t| t.project == project) {
        return Ok(existing_t.clone());
    }
    if !is_safe_identifier(tenant_name) {
        return Err(eyre!(
            "rabbitmq: tenant name `{tenant_name}` contains characters that aren't \
             safe in a vhost/username (must match `[a-z0-9_]+`); rename via \
             `bougie services add rabbitmq --tenant=...`"
        ));
    }

    let ctl = ctl_binary(paths)?;
    wait_for_ctl_ready(&ctl, paths, RABBITMQCTL_READY_TIMEOUT)
        .await
        .wrap_err("rabbitmq node never became rabbitmqctl-ready")?;

    // Derived (not random) so re-provisioning yields the same password
    // and a previously-installed env.php keeps connecting. The
    // change_password-on-duplicate path below re-asserts it on the live
    // broker, healing any drift from an earlier random-password install.
    let password = crate::daemon::credentials::derive_password(paths, "rabbitmq", project)?;

    // `add_vhost` is idempotent in v4 (`--ignore-duplicate`); the
    // user creation isn't. Treat "already exists" as success so a
    // re-run after a partial failure (vhost created, user-add
    // crashed mid-call) converges.
    match run_ctl(&ctl, paths, &["add_vhost", tenant_name]).await {
        Ok(()) => {}
        Err(e) if e.to_string().contains("already") || e.to_string().contains("exists") => {}
        Err(e) => {
            return Err(e.wrap_err(format!("rabbitmqctl add_vhost {tenant_name}")));
        }
    }
    // `add_user` errors on duplicate. The user can already exist for
    // two reasons: (1) recovery from a partial-failure run that got
    // past add_vhost but crashed inside add_user, (2) a prior
    // `bougie down` (without `--purge`) wiped the bougie tenant
    // ledger but left rabbitmq's mnesia store intact, and we've
    // since generated a fresh password.
    //
    // For (1) the existing password matches the one in `password`
    // (we'd have written the same ledger row). For (2) the broker
    // still has the *old* password while the ledger row we're about
    // to write carries the new one — and any AMQP client picking up
    // `BOUGIE_SERVICE_RABBITMQ_PASSWORD` would get ACCESS_REFUSED on
    // login (cresset-tools/bougie#31). Always re-assert the password
    // via `change_password` after a duplicate so the broker and the
    // ledger never disagree. Idempotent on the (1) path.
    match run_ctl(&ctl, paths, &["add_user", tenant_name, &password]).await {
        Ok(()) => {}
        Err(e) if e.to_string().contains("already") || e.to_string().contains("exists") => {
            run_ctl(&ctl, paths, &["change_password", tenant_name, &password])
                .await
                .wrap_err_with(|| format!("rabbitmqctl change_password {tenant_name}"))?;
        }
        Err(e) => {
            return Err(e.wrap_err(format!("rabbitmqctl add_user {tenant_name}")));
        }
    }
    run_ctl(
        &ctl,
        paths,
        &[
            "set_permissions",
            "-p",
            tenant_name,
            tenant_name,
            ".*",
            ".*",
            ".*",
        ],
    )
    .await
    .wrap_err_with(|| format!("rabbitmqctl set_permissions for {tenant_name}"))?;

    let mut tenant = Tenant::new(tenant_name, project.to_path_buf());
    tenant
        .alloc
        .insert("vhost".into(), serde_json::json!(tenant_name));
    tenant
        .alloc
        .insert("username".into(), serde_json::json!(tenant_name));
    tenant
        .secrets
        .insert("password".into(), password);
    tenants::append(tenants_path, &tenant).await?;
    Ok(tenant)
}

/// Release a tenant. With `purge`, also drops the vhost and user on
/// the live broker; without it, the tenants ledger entry goes away
/// but the broker keeps the state for a later `up` (matches mariadb
/// + opensearch).
pub async fn deprovision(
    paths: &Paths,
    tenants_path: &Path,
    tenant_name: &str,
    purge: bool,
) -> Result<()> {
    let existing = tenants::load_all(tenants_path).await?;
    if !existing.iter().any(|t| t.tenant == tenant_name) {
        return Ok(());
    }
    if purge {
        if !is_safe_identifier(tenant_name) {
            return Err(eyre!(
                "rabbitmq: refusing to purge tenant with unsafe identifier `{tenant_name}`"
            ));
        }
        // Best-effort: the broker may already be down. Either way the
        // ledger entry is dropped below.
        if let Ok(ctl) = ctl_binary(paths) {
            let _ = run_ctl(&ctl, paths, &["delete_vhost", tenant_name]).await;
            let _ = run_ctl(&ctl, paths, &["delete_user", tenant_name]).await;
        }
    }
    tenants::rewrite(tenants_path, |t| t.tenant != tenant_name).await?;
    Ok(())
}

// -------------------- helpers --------------------

/// Health probe: a single `rabbitmqctl status --quiet`, healthy on exit
/// 0. The supervisor's old TCP probe was satisfied the moment the inet
/// listener bound, but the AMQP layer keeps rejecting work until mnesia +
/// boot modules finish loading; `ctl status` is the canonical "is the
/// node actually up" check (it's what [`wait_for_ctl_ready`] polls).
pub(crate) async fn health(paths: &Paths) -> Result<()> {
    let ctl = ctl_binary(paths)?;
    let mut cmd = Command::new(&ctl);
    cmd.args(["status", "--quiet"]);
    build_ctl_env(&mut cmd, paths);
    // The continuous probe bounds this with a timeout; kill rabbitmqctl
    // if that timeout drops the future so a wedged node can't strand it.
    cmd.kill_on_drop(true);
    let out = cmd
        .output()
        .await
        .map_err(|e| eyre!("spawning rabbitmqctl status: {e}"))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(eyre!(
            "rabbitmqctl status returned non-zero (exit {}): {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        ))
    }
}

/// Locate `sbin/rabbitmqctl` inside the rabbitmq tarball.
fn ctl_binary(paths: &Paths) -> Result<PathBuf> {
    let entry = crate::daemon::catalog::find("rabbitmq")
        .ok_or_else(|| eyre!("BUG: rabbitmq missing from catalog"))?;
    let basedir = store_layout::basedir(paths, entry)
        .wrap_err("resolving rabbitmq basedir")?;
    let ctl = basedir.join("sbin/rabbitmqctl");
    if !ctl.is_file() {
        return Err(eyre!("rabbitmqctl missing at {}", ctl.display()));
    }
    Ok(ctl)
}

/// Spawn rabbitmqctl with the same env knobs the supervisor uses so
/// the script discovers our private node + mnesia paths. We
/// `env_clear()` and rebuild from scratch — that way a stale
/// `RABBITMQ_NODENAME` in the operator's shell can't point us at
/// the wrong broker.
async fn run_ctl(ctl: &Path, paths: &Paths, args: &[&str]) -> Result<()> {
    let mut cmd = Command::new(ctl);
    cmd.args(args);
    build_ctl_env(&mut cmd, paths);
    let output = cmd.output().await.map_err(|e| eyre!("spawning rabbitmqctl: {e}"))?;
    if !output.status.success() {
        return Err(eyre!(
            "rabbitmqctl {} failed (exit {}): {}",
            args.join(" "),
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(())
}

/// Shared env-builder for both rabbitmqctl and the supervisor's
/// rabbitmq-server spawn. Centralised so a path drift between the
/// two would surface as "node not running" against ourselves rather
/// than a silent split-brain.
fn build_ctl_env(cmd: &mut Command, paths: &Paths) {
    cmd.env_clear()
        .env("HOME", paths.service_data("rabbitmq").join("home"))
        .env("PATH", "/usr/bin:/bin")
        // Belt-and-suspenders against a dangling cwd: bougied anchors
        // its own cwd to the state root, but pin these out-of-band ctl
        // probes to the rabbitmq data dir regardless. rabbitmqctl is an
        // Erlang/BEAM program that `getcwd()`s at boot and aborts with
        // `invalid_current_directory` if the inherited cwd has been
        // unlinked — exactly the failure mode that anchoring the server
        // via `render_exec_cwd` already guards against. The data dir is
        // created in `pre_start`, owned by us, and stable.
        .current_dir(paths.service_data("rabbitmq"))
        .envs(rabbitmq_env(paths));
}

/// Block until `rabbitmqctl status` returns 0. The TCP probe was
/// satisfied the moment the inet listener bound, but mnesia + boot
/// modules need another second or two to load before ctl calls
/// stop returning "node not running."
async fn wait_for_ctl_ready(ctl: &Path, paths: &Paths, timeout: Duration) -> Result<()> {
    let deadline = Instant::now() + timeout;
    loop {
        let mut cmd = Command::new(ctl);
        cmd.args(["status", "--quiet"]);
        build_ctl_env(&mut cmd, paths);
        let last_err = match cmd.output().await {
            Ok(o) if o.status.success() => return Ok(()),
            Ok(o) => String::from_utf8_lossy(&o.stderr).trim().to_string(),
            Err(e) => e.to_string(),
        };
        if Instant::now() >= deadline {
            return Err(eyre!(
                "rabbitmqctl never reported running within {timeout:?}; last error: {last_err}"
            ));
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

/// Env vars shared between the supervisor's spawn of rabbitmq-server
/// and bougie's own out-of-band rabbitmqctl calls. Kept in one place
/// so an off-by-one between the two would surface as "node not
/// running" against ourselves rather than a silent split-brain.
pub fn rabbitmq_env(paths: &Paths) -> Vec<(String, String)> {
    let data = paths.service_data("rabbitmq");
    let log = paths.service_log("rabbitmq");
    let run = paths.service_run("rabbitmq");
    let conf = paths.service_conf("rabbitmq");
    vec![
        // Pin the node to a stable shortname. Default is
        // `rabbit@$(hostname)`, which couples the dev broker to the
        // operator's hostname (and breaks in containers where
        // hostname is a hex id). `localhost` is in /etc/hosts on
        // every supported platform and resolves to 127.0.0.1; it's
        // also dot-free, which keeps Erlang's `shortnames` mode
        // happy (a `rabbit@127.0.0.1` name would require
        // `RABBITMQ_USE_LONGNAME=true`).
        ("RABBITMQ_NODENAME".into(), "rabbit@localhost".into()),
        ("RABBITMQ_NODE_IP_ADDRESS".into(), "127.0.0.1".into()),
        ("RABBITMQ_NODE_PORT".into(), "5672".into()),
        // RabbitMQ's `rabbitmq-defaults` script reads $RABBITMQ_BASE
        // for everything that doesn't have a more specific knob.
        ("RABBITMQ_BASE".into(), data.display().to_string()),
        (
            "RABBITMQ_MNESIA_BASE".into(),
            data.join("mnesia").display().to_string(),
        ),
        ("RABBITMQ_LOG_BASE".into(), log.display().to_string()),
        (
            "RABBITMQ_PID_FILE".into(),
            run.join("rabbitmq.pid").display().to_string(),
        ),
        (
            "RABBITMQ_CONF_ENV_FILE".into(),
            conf.join("rabbitmq-env.conf").display().to_string(),
        ),
        (
            "RABBITMQ_ENABLED_PLUGINS_FILE".into(),
            conf.join("enabled_plugins").display().to_string(),
        ),
        // Run beam without an erlang cookie of its own; this is
        // single-node so no inter-node auth is needed. Erlang
        // insists on a `.erlang.cookie` file in HOME, so we point
        // HOME at our RW data dir.
    ]
}

/// Match `[a-z0-9_]+`. Vhost/username characters are looser at the
/// rabbitmq layer but tightening here defends against tenant names
/// derived from user-controlled `composer.json` content.
fn is_safe_identifier(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 128
        && s.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn safe_identifier_accepts_typical_tenants() {
        assert!(is_safe_identifier("acme_blog"));
        assert!(is_safe_identifier("blog_2026"));
        assert!(is_safe_identifier("a"));
    }

    #[test]
    fn safe_identifier_rejects_uppercase_and_metacharacters() {
        assert!(!is_safe_identifier(""));
        assert!(!is_safe_identifier("AcmeBlog"));
        assert!(!is_safe_identifier("foo bar"));
        assert!(!is_safe_identifier("foo/bar"));
        assert!(!is_safe_identifier(&"x".repeat(129)));
    }

    #[test]
    fn rabbitmq_env_pins_loopback_and_state_paths() {
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = Paths::new(tmp.path().into(), tmp.path().into());
        let env: std::collections::HashMap<_, _> =
            rabbitmq_env(&paths).into_iter().collect();
        assert_eq!(env.get("RABBITMQ_NODENAME").map(std::string::String::as_str), Some("rabbit@localhost"));
        assert_eq!(env.get("RABBITMQ_NODE_IP_ADDRESS").map(std::string::String::as_str), Some("127.0.0.1"));
        assert_eq!(env.get("RABBITMQ_NODE_PORT").map(std::string::String::as_str), Some("5672"));
        assert!(env
            .get("RABBITMQ_MNESIA_BASE")
            .is_some_and(|p| p.contains("mnesia")));
        assert!(env
            .get("RABBITMQ_LOG_BASE")
            .is_some_and(|p| p.contains("rabbitmq") && p.ends_with("/log")));
    }
}
