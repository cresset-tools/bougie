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

use crate::daemon::{
    store_layout,
    tenants::{self, Tenant},
};
use bougie_paths::Paths;
use eyre::{Result, WrapErr, eyre};
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

/// The Erlang node bougie's broker runs as.
///
/// Erlang node names are registered in `epmd`, which is **host-wide**
/// (one daemon on 4369 serving every BEAM on the box) — so the name is
/// a shared namespace, not a per-install one. Under rabbitmq's stock
/// `rabbit@…` name, any other dev tool that also runs a broker
/// (docker-compose with host networking, a distro package, a competing
/// devbox) claims the same name first and every bougie start after that
/// dies with `{duplicate_node_name,"rabbit","localhost"}` — the AMQP
/// port relocation the supervisor already does can't help, because the
/// collision isn't on the port. Naming ourselves `bougie` puts us in
/// our own corner of that namespace so the two brokers coexist.
///
/// The host half stays `localhost`: it's in `/etc/hosts` on every
/// supported platform, resolves to 127.0.0.1, and is dot-free, which
/// keeps Erlang's `shortnames` mode happy (a `bougie@127.0.0.1` name
/// would require `RABBITMQ_USE_LONGNAME=true`).
const NODENAME: &str = "bougie@localhost";

/// The node bougie used before [`NODENAME`]. Ledger rows written back
/// then carry no `node` allocation, so this is what they mean — see
/// [`tenant_node`].
const LEGACY_NODENAME: &str = "rabbit@localhost";

/// Bounds on [`clear_stale_node`]. It runs under the supervisor lock —
/// that placement is what makes it race-free, since it sits after the
/// already-running check, where any node still holding our name is
/// unsupervised by definition — so a wedged broker must not be able to
/// stall `status` and the reaper indefinitely. A start with no collision
/// pays only [`EPMD_QUERY_TIMEOUT`]'s subprocess, which returns in
/// milliseconds.
const EPMD_QUERY_TIMEOUT: Duration = Duration::from_secs(5);
/// epmd's fixed port — see [`node_registered`].
const EPMD_PORT: u16 = 4369;
const NODE_PROBE_TIMEOUT: Duration = Duration::from_secs(10);
const NODE_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(20);
const NODE_RELEASE_TIMEOUT: Duration = Duration::from_secs(5);
/// Loopback connect to a node's distribution port: answers or refuses
/// immediately, so this only has to cover a wedged accept queue.
const DIST_PORT_TIMEOUT: Duration = Duration::from_secs(2);

/// rabbitmq's catalog default version — the `<version>` segment in its
/// state paths. Phase 1a runs a single instance at the default version;
/// once the request layer carries a resolved version this becomes the
/// threaded instance version.
fn svc_version() -> &'static str {
    crate::daemon::catalog::default_version("rabbitmq")
}

/// rabbitmq pre-start hook. Creates the directories rabbitmq writes
/// to under our RW allowlist. No bootstrap step — rabbitmq creates
/// its own mnesia + log files on first start.
pub async fn pre_start(paths: &Paths) -> Result<()> {
    for p in [
        paths.service_data("rabbitmq", svc_version()),
        paths.service_data("rabbitmq", svc_version()).join("mnesia"),
        paths.service_data("rabbitmq", svc_version()).join("home"),
        paths.service_log("rabbitmq", svc_version()),
        paths.service_run("rabbitmq", svc_version()),
        paths.service_conf("rabbitmq", svc_version()),
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
    let (tenant_name, stale_node) = match plan(&existing, project, tenant_name) {
        Plan::Reuse(t) => return Ok(t.clone()),
        Plan::Create { name, stale_node } => (name, stale_node),
    };
    if let Some(from) = stale_node {
        tracing::info!(
            tenant = tenant_name,
            from,
            to = NODENAME,
            "rabbitmq node changed; re-provisioning tenant on the current node",
        );
        tenants::rewrite(tenants_path, |row| row.project != project).await?;
    }
    if !is_safe_identifier(tenant_name) {
        return Err(eyre!(
            "rabbitmq: tenant name `{tenant_name}` contains characters that aren't \
             safe in a vhost/username (must match `[a-z0-9_]+`); rename via \
             `bougie service add rabbitmq --tenant=...`"
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
    // Stamp the node the vhost/user actually live in, so a later rename
    // is detectable instead of silently handing out dead credentials.
    tenant
        .alloc
        .insert("node".into(), serde_json::json!(NODENAME));
    tenant.secrets.insert("password".into(), password);
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

/// Evict a broker that outlived its daemon and still holds our Erlang
/// node name.
///
/// Node names are registered with `epmd`, a host-wide daemon that
/// outlives any single broker — so a rabbitmq that escaped teardown
/// (`bougied` taken out by a `SIGKILL`, or a BEAM that daemonized out of
/// the process group on a platform with no cgroup backstop) keeps
/// [`NODENAME`] claimed. Every respawn after that dies in prelaunch with
/// `{duplicate_node_name,…}`, through the entire restart-backoff ladder,
/// forever: nothing in that loop removes the squatter, and the port
/// relocation the supervisor does can't help because the collision isn't
/// on a port. The rename that keeps *foreign* brokers out of our way is
/// no help either — here the squatter is our own past self.
///
/// So: ask epmd whether the name is claimed, and if it is, whether the
/// claimant is ours. `rabbitmqctl status` succeeds only against a node
/// that accepts the Erlang cookie in this install's private `HOME`, so a
/// broker belonging to another `BOUGIE_HOME` — or any foreign Erlang app
/// that happened to pick the name — fails that test and is left alone,
/// with the reason logged. We never kill a process we can't prove is
/// ours.
///
/// Best-effort throughout: every failure path leaves the spawn to
/// proceed exactly as it would have, so this can only turn a permanent
/// failure into a recovery, never a working start into a broken one.
pub async fn clear_stale_node(paths: &Paths) {
    let Some(dist_port) = node_dist_port().await else {
        return;
    };
    let Ok(ctl) = ctl_binary(paths) else {
        return;
    };
    // Ours? The distribution port is a cheap gate — a registration whose
    // port refuses connections can't be a live node — and `rabbitmqctl
    // status` is the authority, since it only succeeds against a node
    // that accepts the Erlang cookie in this install's private HOME.
    let ours = dist_port_answers(dist_port).await
        && matches!(
            tokio::time::timeout(NODE_PROBE_TIMEOUT, health(paths)).await,
            Ok(Ok(()))
        );
    if !ours {
        // Two very different things fail that test: a foreign node, and
        // one of ours partway through dying — epmd drops a name only when
        // the node's socket closes, which trails the process exiting, and
        // a killed BEAM can still accept on its port for a moment (seen
        // in the wild: bougied's own `reap_stale_leaves` SIGKILLs a
        // leftover broker at startup and a quarter-second later both the
        // registration and the port are still there). Time tells them
        // apart — a corpse's registration clears, a live squatter's
        // doesn't — so wait before concluding anything. Waiting also
        // keeps us from spawning into a name epmd is about to release,
        // which would burn a crash-and-backoff cycle for nothing.
        if wait_for_deregistration("waiting out a registration with no live node behind it").await {
            return;
        }
        tracing::warn!(
            node = NODENAME,
            "the erlang node name is held by a live node that doesn't answer to this install's \
             cookie; leaving it alone — rabbitmq can't start while it holds the name",
        );
        return;
    }
    tracing::warn!(
        node = NODENAME,
        "a rabbitmq broker outlived its daemon and still holds the node name; shutting it down \
         before respawning",
    );
    match tokio::time::timeout(NODE_SHUTDOWN_TIMEOUT, run_ctl(&ctl, paths, &["shutdown"])).await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => {
            tracing::warn!(node = NODENAME, error = %e, "rabbitmqctl shutdown failed");
            return;
        }
        Err(_) => {
            tracing::warn!(
                node = NODENAME,
                "rabbitmqctl shutdown timed out; spawning anyway",
            );
            return;
        }
    }
    // `shutdown` waits for the OS process to exit, but epmd drops the
    // registration a beat later, when the node's socket closes. Let that
    // land so the spawn we're about to do doesn't race it.
    if wait_for_deregistration("after shutdown").await {
        tracing::info!(node = NODENAME, "stale broker evicted; node name is free");
    }
}

/// Poll until epmd stops listing [`NODENAME`], up to
/// [`NODE_RELEASE_TIMEOUT`]. `true` if the name came free. On timeout the
/// caller spawns anyway: rabbitmq's own prelaunch check is the backstop,
/// and one crash-and-backoff beats refusing to start.
async fn wait_for_deregistration(context: &str) -> bool {
    let deadline = Instant::now() + NODE_RELEASE_TIMEOUT;
    while node_dist_port().await.is_some() {
        if Instant::now() >= deadline {
            tracing::warn!(
                node = NODENAME,
                context,
                "epmd still lists the node; spawning anyway",
            );
            return false;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    true
}

// -------------------- helpers --------------------

/// The Erlang distribution port [`NODENAME`] is registered under with the
/// local epmd, or `None` if the name is free.
///
/// Asked over epmd's own wire protocol rather than by shelling out to
/// the `epmd` binary: that binary sits at a stable path only when the
/// standalone erlang runtime-dep is installed, while a rabbitmq booting
/// from its tarball's *bundled* erlang buries it under an erts-version
/// directory. The request is one byte (`NAMES_REQ`); the reply is epmd's
/// own port followed by the very listing `epmd -names` prints.
///
/// A refused connection means no epmd is running, so nothing can be
/// holding the name — `None`. The port asked is the fixed 4369: what the
/// sidecar the supervisor co-locates listens on
/// (`supervisor::sidecar_for`) and what every Erlang node contacts by
/// default. An operator who moved epmd with `ERL_EPMD_PORT` gets no
/// cleanup — the same as before this existed.
async fn node_dist_port() -> Option<u16> {
    let Ok(Ok(listing)) = tokio::time::timeout(EPMD_QUERY_TIMEOUT, epmd_names()).await else {
        return None;
    };
    epmd_node_port(&listing, node_shortname())
}

/// Is anything actually listening where epmd says the node is? epmd
/// hands out registrations it hasn't yet noticed are dead, so this is
/// what tells a live node from a lingering entry.
async fn dist_port_answers(port: u16) -> bool {
    matches!(
        tokio::time::timeout(
            DIST_PORT_TIMEOUT,
            tokio::net::TcpStream::connect(("127.0.0.1", port)),
        )
        .await,
        Ok(Ok(_))
    )
}

/// epmd's `NAMES` exchange: send `<<1:16, 110>>`, read until epmd closes,
/// drop the leading `<<EpmdPort:32>>`.
async fn epmd_names() -> std::io::Result<String> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    const NAMES_REQ: u8 = 110;

    let mut sock = tokio::net::TcpStream::connect(("127.0.0.1", EPMD_PORT)).await?;
    sock.write_all(&[0, 1, NAMES_REQ]).await?;
    let mut reply = Vec::new();
    sock.read_to_end(&mut reply).await?;
    Ok(String::from_utf8_lossy(reply.get(4..).unwrap_or_default()).into_owned())
}

/// The port an epmd `NAMES` listing gives for `node`, if it lists it at
/// all. Each line reads `name bougie at port 25672`. (The `epmd -names`
/// CLI prints these same lines under an `epmd: up and running …` header
/// of its own.)
fn epmd_node_port(listing: &str, node: &str) -> Option<u16> {
    listing.lines().find_map(|line| {
        let mut fields = line.split_whitespace();
        (fields.next() == Some("name")
            && fields.next() == Some(node)
            && fields.next() == Some("at")
            && fields.next() == Some("port"))
        .then(|| fields.next()?.parse().ok())
        .flatten()
    })
}

/// The node half of [`NODENAME`] — what epmd registers. The host half is
/// implicit there: an epmd only ever speaks for its own host.
fn node_shortname() -> &'static str {
    NODENAME.split_once('@').map_or(NODENAME, |(node, _)| node)
}

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
    let basedir = store_layout::basedir(paths, entry, &entry.version)
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
    // Callers that bound this with a timeout ([`clear_stale_node`]) drop
    // the future on expiry; kill the ctl child with it rather than
    // stranding a BEAM waiting on an unresponsive node.
    cmd.kill_on_drop(true);
    let output = cmd
        .output()
        .await
        .map_err(|e| eyre!("spawning rabbitmqctl: {e}"))?;
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

/// Env-builder for bougied's out-of-band `rabbitmqctl` calls (health
/// probe, `wait_for_ctl_ready`, tenant provision/deprovision). The
/// node-locating knobs (`RABBITMQ_NODENAME` etc.) come from
/// [`rabbitmq_env`], which the supervisor's `rabbitmq-server` spawn also
/// uses — that shared builder is what keeps ctl and the broker pointed
/// at the same node. This function does *not* touch the running broker;
/// it only shapes the environment of the short-lived ctl child.
///
/// We `env_clear()` and rebuild from scratch so a stale
/// `RABBITMQ_NODENAME` in the operator's shell can't point us at the
/// wrong broker.
fn build_ctl_env(cmd: &mut Command, paths: &Paths) {
    cmd.env_clear()
        .env(
            "HOME",
            paths.service_data("rabbitmq", svc_version()).join("home"),
        )
        .env("PATH", ctl_path())
        // Belt-and-suspenders against a dangling cwd: bougied anchors
        // its own cwd to the state root, but pin these out-of-band ctl
        // probes to the rabbitmq data dir regardless. rabbitmqctl is an
        // Erlang/BEAM program that `getcwd()`s at boot and aborts with
        // `invalid_current_directory` if the inherited cwd has been
        // unlinked — exactly the failure mode that anchoring the server
        // via `render_exec_cwd` already guards against. The data dir is
        // created in `pre_start`, owned by us, and stable.
        .current_dir(paths.service_data("rabbitmq", svc_version()))
        // rabbitmqctl reaches the node via RABBITMQ_NODENAME (epmd), not
        // the AMQP port, so this value is immaterial to ctl — but keep it
        // consistent with the running broker's effective port.
        .envs(rabbitmq_env(
            paths,
            crate::daemon::endpoint::effective_primary(paths, "rabbitmq", svc_version(), 5672),
        ));
}

/// PATH for the ctl child: the FHS defaults, plus whatever bougied
/// inherited, appended.
///
/// `rabbitmqctl` is a shell script whose `rabbitmq-env` prelude (and
/// Erlang's own `erl` launcher) shell out to coreutils — `dirname`,
/// `readlink`, etc. On FHS distros those live under `/usr/bin:/bin`, so
/// the old hardcoded value sufficed. On non-FHS hosts (NixOS) `/usr/bin`
/// holds only `env` and `/bin` only `sh`; the launcher dies with
/// `dirname: command not found` and the node never reports healthy.
/// bougied inherits its PATH from the CLI that auto-spawned it — which
/// demonstrably resolves those tools — so appending it recovers the
/// non-FHS case while leaving FHS lookups byte-identical (the FHS dirs
/// are searched first). This must never revert to a bare literal.
fn ctl_path() -> String {
    ctl_path_from(std::env::var("PATH").ok().as_deref())
}

/// Pure core of [`ctl_path`], split out so the `inherited == None` /
/// empty branches are testable without mutating the process environment
/// (`std::env::remove_var` is `unsafe` under edition 2024, which the
/// workspace's `deny(unsafe_code)` forbids here). FHS defaults come
/// first; inherited entries are appended, skipping any already present
/// so the resulting PATH carries no duplicate segments.
fn ctl_path_from(inherited: Option<&str>) -> String {
    let mut segments = vec!["/usr/bin", "/bin"];
    if let Some(inherited) = inherited {
        for segment in inherited.split(':') {
            if !segment.is_empty() && !segments.contains(&segment) {
                segments.push(segment);
            }
        }
    }
    segments.join(":")
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
pub fn rabbitmq_env(paths: &Paths, node_port: u16) -> Vec<(String, String)> {
    let data = paths.service_data("rabbitmq", svc_version());
    let log = paths.service_log("rabbitmq", svc_version());
    let run = paths.service_run("rabbitmq", svc_version());
    let conf = paths.service_conf("rabbitmq", svc_version());
    vec![
        // Pin the node to a stable, bougie-private shortname. The
        // default is `rabbit@$(hostname)`, which both couples the dev
        // broker to the operator's hostname (and breaks in containers
        // where hostname is a hex id) and shares the `rabbit` name
        // with every other broker on the box. See [`NODENAME`].
        ("RABBITMQ_NODENAME".into(), NODENAME.into()),
        ("RABBITMQ_NODE_IP_ADDRESS".into(), "127.0.0.1".into()),
        ("RABBITMQ_NODE_PORT".into(), node_port.to_string()),
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

/// What [`provision`] does for a project, decided from the ledger alone
/// (no broker contact).
#[derive(Debug)]
enum Plan<'a> {
    /// A row already describes this project's tenant on the node we're
    /// about to talk to. Nothing to do.
    Reuse(&'a Tenant),
    /// Create the vhost + user under `name`. `stale_node` names the node
    /// a superseded row was provisioned against — that row must be
    /// dropped from the ledger first.
    Create {
        name: &'a str,
        stale_node: Option<&'a str>,
    },
}

/// Decide [`Plan`] for `project`.
///
/// The interesting case is a row that names a *different* node: rabbitmq
/// keys its metadata store by node name, so that row's vhost and user
/// live in a store the current broker never opened. Reusing it would
/// hand the project credentials for a vhost that doesn't exist and every
/// AMQP login would fail with `ACCESS_REFUSED` — so re-provision instead.
/// That's safe to do unasked because [`crate::daemon::credentials::derive_password`]
/// is deterministic: the re-created user gets the same password an
/// already-installed `env.php` holds. The tenant keeps its *recorded*
/// name rather than the caller's freshly-derived one, for the same
/// reason — it's the vhost name the project is already configured with.
fn plan<'a>(existing: &'a [Tenant], project: &Path, requested: &'a str) -> Plan<'a> {
    match existing.iter().find(|t| t.project == project) {
        Some(t) if tenant_node(t) == NODENAME => Plan::Reuse(t),
        Some(t) => Plan::Create {
            name: &t.tenant,
            stale_node: Some(tenant_node(t)),
        },
        None => Plan::Create {
            name: requested,
            stale_node: None,
        },
    }
}

/// The node a ledger row's vhost + user were created on. Rows written
/// before the node rename carry no `node` allocation; they belong to
/// [`LEGACY_NODENAME`], which is what makes them re-provisionable
/// rather than ambiguous.
fn tenant_node(t: &Tenant) -> &str {
    t.alloc
        .get("node")
        .and_then(serde_json::Value::as_str)
        .unwrap_or(LEGACY_NODENAME)
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
        let env: std::collections::HashMap<_, _> = rabbitmq_env(&paths, 5672).into_iter().collect();
        assert_eq!(
            env.get("RABBITMQ_NODENAME")
                .map(std::string::String::as_str),
            Some("bougie@localhost")
        );
        assert_eq!(
            env.get("RABBITMQ_NODE_IP_ADDRESS")
                .map(std::string::String::as_str),
            Some("127.0.0.1")
        );
        assert_eq!(
            env.get("RABBITMQ_NODE_PORT")
                .map(std::string::String::as_str),
            Some("5672")
        );
        assert!(
            env.get("RABBITMQ_MNESIA_BASE")
                .is_some_and(|p| p.contains("mnesia"))
        );
        assert!(
            env.get("RABBITMQ_LOG_BASE")
                .is_some_and(|p| p.contains("rabbitmq") && p.ends_with("/log"))
        );
    }

    #[test]
    fn nodename_is_bougie_private_and_shortname_safe() {
        // The point of the name: not `rabbit`, so a foreign broker
        // holding the stock name in epmd can't lock us out.
        assert!(NODENAME.starts_with("bougie@"));
        // Erlang `shortnames` mode rejects a dotted host half.
        let (_, host) = NODENAME.split_once('@').expect("node@host");
        assert!(!host.contains('.'), "shortnames mode forbids a dotted host");
    }

    #[test]
    fn tenant_node_reads_the_recorded_node() {
        let mut t = Tenant::new("acme_blog", "/p/acme");
        t.alloc
            .insert("node".into(), serde_json::json!("bougie@localhost"));
        assert_eq!(tenant_node(&t), "bougie@localhost");
    }

    #[test]
    fn tenant_node_treats_an_unstamped_row_as_the_legacy_node() {
        // Pre-rename rows have no `node` key. Reading them as the legacy
        // node is what triggers exactly one re-provision on upgrade —
        // defaulting to NODENAME instead would leave the project holding
        // credentials for a vhost the new node never created.
        let t = Tenant::new("acme_blog", "/p/acme");
        assert_eq!(tenant_node(&t), LEGACY_NODENAME);
        assert_ne!(tenant_node(&t), NODENAME);
    }

    #[test]
    fn epmd_listing_yields_our_nodes_distribution_port() {
        // The port matters as much as the name: it's what a liveness
        // probe connects to, which is how a live squatter is told from a
        // registration epmd hasn't yet dropped.
        let listing = "epmd: up and running on port 4369 with data:\nname bougie at port 25673\n";
        assert_eq!(epmd_node_port(listing, node_shortname()), Some(25673));
        assert_eq!(node_shortname(), "bougie");
    }

    #[test]
    fn epmd_listing_ignores_other_nodes_and_an_empty_registry() {
        // epmd is up but nothing of ours is registered: the header alone.
        assert_eq!(
            epmd_node_port(
                "epmd: up and running on port 4369 with data:\n",
                node_shortname()
            ),
            None
        );
        // Someone else's node — the one case where we must not conclude
        // our name is taken, since that's what leads to an eviction.
        assert_eq!(
            epmd_node_port("name rabbit at port 25672\n", node_shortname()),
            None
        );
        // A node whose name merely starts with ours.
        assert_eq!(
            epmd_node_port("name bougie2 at port 25672\n", "bougie"),
            None
        );
        // No epmd at all — the query failed and there's no listing.
        assert_eq!(epmd_node_port("", node_shortname()), None);
    }

    #[test]
    fn epmd_listing_survives_a_malformed_line() {
        // Never panic on epmd output we didn't anticipate; an
        // unparseable port reads as "not listed" so we leave it alone.
        assert_eq!(
            epmd_node_port("name bougie at port hello\n", "bougie"),
            None
        );
        assert_eq!(epmd_node_port("name bougie at\n", "bougie"), None);
        assert_eq!(
            epmd_node_port("garbage\nname bougie at port 25673\n", "bougie"),
            Some(25673)
        );
    }

    /// A ledger row as the current code writes one.
    fn row_on(node: &str, tenant: &str, project: &str) -> Tenant {
        let mut t = Tenant::new(tenant, project);
        t.alloc.insert("vhost".into(), serde_json::json!(tenant));
        t.alloc.insert("node".into(), serde_json::json!(node));
        t
    }

    #[test]
    fn plan_reuses_a_row_on_the_current_node() {
        let rows = vec![row_on(NODENAME, "acme_blog", "/p/acme")];
        match plan(&rows, Path::new("/p/acme"), "acme_blog") {
            Plan::Reuse(t) => assert_eq!(t.tenant, "acme_blog"),
            other @ Plan::Create { .. } => panic!("expected Reuse, got {other:?}"),
        }
    }

    #[test]
    fn plan_creates_when_the_project_has_no_row() {
        let rows = vec![row_on(NODENAME, "other", "/p/other")];
        match plan(&rows, Path::new("/p/acme"), "acme_blog") {
            Plan::Create { name, stale_node } => {
                assert_eq!(name, "acme_blog");
                assert_eq!(stale_node, None);
            }
            other @ Plan::Reuse(_) => panic!("expected Create, got {other:?}"),
        }
    }

    #[test]
    fn plan_reprovisions_a_row_from_the_legacy_node_under_its_recorded_name() {
        // The upgrade path: a pre-rename row (no `node` key) names a
        // vhost that only exists in the old node's store. It has to be
        // re-created — and under the name the project's env.php already
        // carries, not the caller's derived one.
        let mut legacy = Tenant::new("old_name", "/p/acme");
        legacy
            .alloc
            .insert("vhost".into(), serde_json::json!("old_name"));
        let rows = vec![legacy];
        match plan(&rows, Path::new("/p/acme"), "derived_name") {
            Plan::Create { name, stale_node } => {
                assert_eq!(name, "old_name");
                assert_eq!(stale_node, Some(LEGACY_NODENAME));
            }
            other @ Plan::Reuse(_) => panic!("expected Create, got {other:?}"),
        }
    }

    #[test]
    fn ctl_path_appends_inherited_after_fhs_defaults() {
        assert_eq!(
            ctl_path_from(Some("/nix/store/coreutils/bin:/run/current-system/sw/bin")),
            "/usr/bin:/bin:/nix/store/coreutils/bin:/run/current-system/sw/bin"
        );
    }

    #[test]
    fn ctl_path_falls_back_to_fhs_defaults_when_unset_or_empty() {
        // The `remove_var`-free reason the pure fn exists: exercise the
        // None branch that a live `bougied` with no PATH would hit.
        assert_eq!(ctl_path_from(None), "/usr/bin:/bin");
        assert_eq!(ctl_path_from(Some("")), "/usr/bin:/bin");
    }

    #[test]
    fn ctl_path_dedupes_fhs_and_repeated_segments() {
        // Inherited PATH that already leads with the FHS dirs (and
        // repeats one) must not produce duplicate segments.
        assert_eq!(
            ctl_path_from(Some("/bin:/usr/bin:/opt/x:/opt/x:/usr/local/bin")),
            "/usr/bin:/bin:/opt/x:/usr/local/bin"
        );
    }
}
