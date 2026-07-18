//! `bougie share` — expose the running project on a public `*.bougie.show` URL.
//!
//! Mirrors the `bougie server` project-verb (locate → sync → ensure the shared
//! dev server hosts this project as a `server` tenant), then opens an outbound
//! tunnel via [`bougie_tunnel`] and registers the relay-assigned
//! `<slug>.bougie.show` as a `[[host]]` on the running server. Because the
//! router matches by exact `Host`, the share exposes *only* that one hostname
//! — the unknown-host 404 containment still holds for everything else on
//! loopback. Ctrl-C tears the share (and its temporary host) down.
//!
//! v1 is foreground + single-connection (no auto-reconnect). The relay speaks
//! all HTTP; the client is a pure byte-splice (see [`bougie_tunnel`]). The
//! Magento `base_url` / `X-Forwarded-*` fixup is a follow-up (`SHARE_PLAN.md` §4.4);
//! today the share inherits the tenant host's docroot + framework rewrites so
//! static assets resolve identically.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use bougie_cli::{OutputFormat, ShareArgs};
use bougie_tunnel::{PasswordMode, ShareHandle, TunnelClient, TunnelConfig};
use eyre::{Result, WrapErr, eyre};
use serde_json::{Value, json};

use crate::commands::server::{
    derive_hostname, locate_project_root, server_listen_port, server_state_version,
};
use crate::commands::tenant::{derive_default_tenant, sanitize_tenant};

/// Relay tunnel-ingress the client dials. The relay isn't deployed yet —
/// override with `BOUGIE_SHARE_RELAY` (`host:port`) + `BOUGIE_SHARE_RELAY_SNI`
/// to point at a local or staging `bougie-relay` (the example harness uses
/// `127.0.0.1:7443` / `tunnel.bougie.test`).
const DEFAULT_RELAY_ADDR: &str = "tunnel.bougie.show:443";
const DEFAULT_RELAY_SNI: &str = "tunnel.bougie.show";

fn relay_addr() -> String {
    std::env::var("BOUGIE_SHARE_RELAY").unwrap_or_else(|_| DEFAULT_RELAY_ADDR.to_owned())
}

fn relay_sni() -> String {
    std::env::var("BOUGIE_SHARE_RELAY_SNI").unwrap_or_else(|_| DEFAULT_RELAY_SNI.to_owned())
}

/// `bougie share [NAME]` — serve the project through the shared dev server,
/// then tunnel it out on a public URL until Ctrl-C.
pub fn run(format: OutputFormat, args: ShareArgs) -> Result<ExitCode> {
    use bougie_config::load_project;
    use bougie_paths::Paths;

    let ShareArgs { name, slug, public, password, no_sync } = args;

    let project_root = locate_project_root()?;
    // Validate it's a real project early (surfaces a bad composer.json before
    // we touch the daemon or the relay).
    load_project(&project_root)?;

    // The dev server routes PHP into this project, so it must be synced before
    // it can serve — the same implicit sync `bougie server` performs.
    if !no_sync {
        crate::commands::sync::run(
            &project_root,
            format,
            false,
            false,
            None,
            None,
            bougie_cli::PhpPrefArgs::default(),
            bougie_composer_resolver::ResolutionStrategy::Highest,
            bougie_composer_resolver::PlatformIgnore::default(),
        )?;
    }

    let paths = Paths::from_env()?;
    let tenant = match &name {
        Some(name) => sanitize_tenant(name),
        None => derive_default_tenant(&project_root, &paths),
    };

    // Ensure the shared dev server hosts this project as a `server` tenant —
    // this provisions the `<tenant>.bougie.run` `[[host]]` and reloads. Same
    // daemon call the `server` project-verb makes; `version` is required on the
    // wire and fixed to the running bougie's own version for the singleton.
    let call_args = json!({
        "project": project_root,
        "services": [{"name": "server", "version": server_state_version(), "tenant": tenant}],
    });
    let _: Value = crate::commands::service::client::call(&paths, "service.up", call_args)?;

    let local_port = server_listen_port(&paths);
    let tenant_host = derive_hostname(&tenant);

    // Inherit the tenant host's docroot + framework rewrites so the shared URL
    // serves byte-identically (including Magento's `/static` rewrites).
    let server_toml = bougie_daemon::daemon::provisioners::bougie_server::server_toml_path(&paths);
    let cfg = bougie_server::server::config::load(&server_toml)?;
    let base = cfg.hosts.iter().find(|h| h.hostname == tenant_host).ok_or_else(|| {
        eyre!("the dev server did not provision a host for {tenant_host} — is the server running?")
    })?;
    let root = base.root.clone();
    let rewrites = base.rewrites.clone();

    // The bearer that authorises this share with the relay (which introspects
    // it against sconce). Anonymous shares send `None`; the relay reports
    // "run `bougie login`" if it requires auth.
    let token = resolve_token();

    let password_mode = if public {
        PasswordMode::None
    } else if password.is_some() {
        PasswordMode::Custom
    } else {
        PasswordMode::Auto
    };

    let tunnel_cfg = TunnelConfig {
        relay_addr: relay_addr(),
        relay_sni: relay_sni(),
        local_port,
        project: tenant.clone(),
        token,
        slug,
        password_mode,
        password,
        resume: None,
    };

    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
    rt.block_on(async move {
        let (handle, serving) = TunnelClient::new(tunnel_cfg).open().await?;

        // Register the relay-assigned `<slug>.bougie.show` as a `[[host]]`
        // mirroring the tenant (same project/root/rewrites), then reload the
        // live server so its router accepts the new Host.
        add_share_host(&server_toml, &handle.host, &project_root, &root, &rewrites, &paths).await?;
        banner(format, &project_root, &handle)?;

        // Drive the tunnel until the relay closes it or the user hits Ctrl-C.
        let out = tokio::select! {
            r = serving.run() => r,
            _ = tokio::signal::ctrl_c() => Ok(()),
        };

        // Always tear the temporary host back down, whatever ended the share.
        remove_share_host(&server_toml, &handle.host, &paths).await;
        out
    })?;

    Ok(ExitCode::SUCCESS)
}

/// The login bearer that authorises the share with the relay.
/// `BOUGIE_SHARE_TOKEN` wins (scripting / testing); otherwise, when the user
/// has logged into exactly one registry, reuse that token. Ambiguous (several
/// logins) or absent → `None`.
fn resolve_token() -> Option<String> {
    if let Ok(tok) = std::env::var("BOUGIE_SHARE_TOKEN")
        && !tok.is_empty()
    {
        return Some(tok);
    }
    let creds = bougie_composer_resolver::update::read_bougie_auth_json().ok()?;
    let mut bearers = creds.values().filter_map(|c| match c {
        bougie_composer_resolver::metadata::AuthCredentials::Bearer { token } => Some(token.clone()),
        _ => None,
    });
    let first = bearers.next()?;
    // Only auto-select when it's unambiguous.
    if bearers.next().is_some() { None } else { Some(first) }
}

/// Add `<slug>.bougie.show` to `server.toml` mirroring the tenant, then reload
/// the running server so it starts routing the new Host.
async fn add_share_host(
    server_toml: &Path,
    host: &str,
    project: &Path,
    root: &str,
    rewrites: &[bougie_server::server::config::RewriteRule],
    paths: &bougie_paths::Paths,
) -> Result<()> {
    bougie_server::server::config::add_host_with_rewrites(
        server_toml,
        host,
        project,
        Some(root),
        rewrites,
    )
    .wrap_err_with(|| format!("registering share host {host}"))?;
    bougie_daemon::daemon::provisioners::bougie_server::ping_reload_config(paths)
        .await
        .wrap_err("reloading the dev server after adding the share host")?;
    Ok(())
}

/// Best-effort teardown: drop the temporary `[[host]]` and reload. Failures are
/// logged, never fatal — the share is already over by the time we get here.
async fn remove_share_host(server_toml: &Path, host: &str, paths: &bougie_paths::Paths) {
    if let Err(e) = bougie_server::server::config::remove_host(server_toml, host) {
        tracing::warn!("failed to remove share host {host}: {e}");
    }
    if let Err(e) =
        bougie_daemon::daemon::provisioners::bougie_server::ping_reload_config(paths).await
    {
        tracing::warn!("failed to reload after removing share host {host}: {e}");
    }
}

#[derive(serde::Serialize)]
struct ShareResult {
    schema_version: u32,
    project: PathBuf,
    url: String,
    host: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    view_password: Option<String>,
}

impl bougie_output::output::Render for ShareResult {
    fn render_text(&self, w: &mut dyn std::io::Write) -> std::io::Result<()> {
        writeln!(w, "sharing {} at {}", self.project.display(), self.url)?;
        match &self.view_password {
            Some(pw) => writeln!(w, "  view password: {pw}")?,
            None => writeln!(w, "  \u{26a0} PUBLIC — no view password; anyone with the URL can view")?,
        }
        writeln!(w, "  Ctrl-C to stop the share")
    }
}

fn banner(format: OutputFormat, project: &Path, handle: &ShareHandle) -> Result<()> {
    bougie_output::output::emit(format, &ShareResult {
        schema_version: 1,
        project: project.to_path_buf(),
        url: handle.url.clone(),
        host: handle.host.clone(),
        view_password: handle.view_password.clone(),
    })?;
    Ok(())
}
