//! `bougie server` CLI glue (`SERVER_CLI_PLAN.md`).
//!
//! The server *engine* lives in the `bougie-server` crate; this module
//! is the binary-side dispatch for the reshaped surface. It maps the
//! `ServerArgs` tree onto engine entry points, resolves the
//! bougied-managed `server.toml` when `--config` is omitted, and
//! delegates `stop` / `logs` to the services layer.
//!
//! The default action (`bougie server`, no subcommand) is the project
//! verb: ensure the shared dev server hosts this project, print its
//! URL, and stream its log. On Unix it goes through bougied (the same
//! path as `bougie up server`); the standalone Windows fallback is
//! Phase 5 of the plan and is still a stub.

use bougie_cli::{OutputFormat, ServeArgs, ServerArgs, ServerCommand};
use eyre::Result;
use std::path::PathBuf;
use std::process::ExitCode;

/// Dispatch `bougie server [SUBCOMMAND]`. With no subcommand the
/// flattened [`ServeArgs`] drive the default project-serve action.
pub fn dispatch(format: OutputFormat, args: ServerArgs) -> Result<ExitCode> {
    match args.command {
        None => serve(format, &args.serve),
        Some(ServerCommand::Run { config, listen, log_format }) => {
            bougie_server::server::run::run(format, &config, listen.as_deref(), log_format.as_deref())
        }
        Some(ServerCommand::Status { config }) => status(format, config),
        Some(ServerCommand::Open { name }) => open(format, name),
        Some(ServerCommand::Stop) => stop(format),
        Some(ServerCommand::Logs { follow, lines }) => logs(format, follow, lines),
        Some(ServerCommand::Tls(cmd)) => tls(format, &cmd),
        Some(ServerCommand::Hosts(cmd)) => hosts(format, cmd),
    }
}

/// Resolve the `server.toml` to act on when `--config` is omitted: the
/// bougied-managed per-service config.
fn resolve_config(config: Option<PathBuf>) -> Result<PathBuf> {
    if let Some(path) = config {
        return Ok(path);
    }
    let paths = bougie_paths::Paths::from_env()?;
    Ok(paths.service_conf("server").join("server.toml"))
}

/// `bougie server status` — host + pool view. Phase 1 reads the
/// configured hosts; the live control-socket enrichment lands in a
/// later phase.
fn status(format: OutputFormat, config: Option<PathBuf>) -> Result<ExitCode> {
    let path = resolve_config(config)?;
    bougie_server::server::helpers::list(format, &path)
}

/// Build a dev URL, dropping the port when it's the scheme default.
fn build_url(tls: bool, hostname: &str, port: u16) -> String {
    let (scheme, default_port) = if tls { ("https", 443) } else { ("http", 80) };
    if port == default_port {
        format!("{scheme}://{hostname}")
    } else {
        format!("{scheme}://{hostname}:{port}")
    }
}

/// Hostname for a tenant. Mirrors the daemon's
/// `provisioners::bougie_server::derive_hostname` (DNS labels can't
/// carry underscores, so the tenant's `_` become `-`).
fn derive_hostname(tenant: &str) -> String {
    format!("{}.bougie.run", tenant.replace('_', "-"))
}

// Tenant derivation is shared with the `services up` path so both agree
// on a project's identity (DB name, vhost, `<tenant>.bougie.run`) — and
// so `server open`/`logs` re-derive the same name `up` provisioned. See
// `crate::commands::tenant`.
use crate::commands::tenant::{derive_default_tenant, sanitize_tenant};

/// Resolve the project's web root (relative to the project dir),
/// mirroring the daemon's `resolve_web_root`: an explicit `[server]
/// root` (bougie.toml / composer `extra.bougie.server.root`) wins,
/// otherwise auto-detect `pub` then `public`. Used by the standalone
/// (Windows) path, where there's no daemon provisioner to do it.
#[cfg_attr(unix, allow(dead_code))] // only the standalone path calls this
fn resolve_web_root(project_root: &std::path::Path, explicit: Option<&str>) -> Result<String> {
    if let Some(explicit) = explicit {
        if explicit.is_empty() {
            return Err(eyre::eyre!(
                "server.root is empty in the project config; remove the field or point it at a \
                 subdirectory like \"public\""
            ));
        }
        return Ok(explicit.to_string());
    }
    for candidate in ["pub", "public"] {
        if project_root.join(candidate).is_dir() {
            return Ok(candidate.to_string());
        }
    }
    Err(eyre::eyre!(
        "could not auto-detect a web root in {}: neither `pub` nor `public` exists. Set \
         `[server] root = \"<dir>\"` in bougie.toml or `extra.bougie.server.root` in composer.json.",
        project_root.display()
    ))
}

/// Read the shared server's listen port from the bougied-managed
/// `server.toml`; falls back to the engine default when unreadable.
fn server_listen_port(paths: &bougie_paths::Paths) -> u16 {
    let cfg_path = paths.service_conf("server").join("server.toml");
    bougie_server::server::config::load(&cfg_path)
        .ok()
        .and_then(|cfg| cfg.server.listen.rsplit(':').next().and_then(|p| p.parse().ok()))
        .unwrap_or(7080)
}

/// Best-effort browser launch. Silent no-op target on headless boxes —
/// callers ignore the error and the URL is printed regardless.
fn open_url(url: &str) -> Result<()> {
    let mut cmd;
    #[cfg(target_os = "macos")]
    {
        cmd = std::process::Command::new("open");
        cmd.arg(url);
    }
    #[cfg(target_os = "windows")]
    {
        // `cmd /C start "" <url>` — the empty quoted title is required
        // so `start` treats the URL as the target, not the window title.
        cmd = std::process::Command::new("cmd");
        cmd.args(["/C", "start", ""]).arg(url);
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        cmd = std::process::Command::new("xdg-open");
        cmd.arg(url);
    }
    cmd.spawn()?;
    Ok(())
}

use bougie_output::output::emit;

/// Walk up from cwd for a project root (`bougie.toml` / `composer.json`
/// / `vendor/bougie/`). Cross-platform mirror of
/// `services::config_mut::locate_project_root`, which lives in the
/// Unix-only services module.
fn locate_project_root() -> Result<PathBuf> {
    let cwd = std::env::current_dir()?;
    for anc in cwd.ancestors() {
        if anc.join("bougie.toml").is_file()
            || anc.join("composer.json").is_file()
            || bougie_paths::project::is_root(anc)
        {
            return Ok(anc.to_path_buf());
        }
    }
    Err(eyre::eyre!(
        "no bougie project found (no `composer.json`, `bougie.toml`, or `vendor/bougie/` in {} or any parent)",
        cwd.display()
    ))
}

#[derive(serde::Serialize)]
struct ServeResult {
    schema_version: u32,
    project: PathBuf,
    hostname: String,
    url: String,
    port: u16,
}

impl bougie_output::output::Render for ServeResult {
    fn render_text(&self, w: &mut dyn std::io::Write) -> std::io::Result<()> {
        writeln!(w, "serving {} at {}", self.project.display(), self.url)
    }
}

/// Default action: serve the current project through the shared dev
/// server. Equivalent to `bougie up server` scoped to this project,
/// plus a URL banner, optional browser launch, and a foreground log
/// attach that detaches (leaving the server running) on Ctrl-C.
#[cfg(unix)]
fn serve(format: OutputFormat, args: &ServeArgs) -> Result<ExitCode> {
    use crate::commands::services::client;
    use bougie_config::load_project;
    use bougie_paths::Paths;
    use serde_json::{json, Value};
    use std::io::IsTerminal;

    let project_root = locate_project_root()?;
    // Validate it's a real project (surfaces a bad composer.json early);
    // the tenant now comes from the dir name + ledger, not composer.
    load_project(&project_root)?;

    // The dev server routes PHP into this project, so it must be synced
    // (resolved PHP + conf.d + vendor) before it can serve. Matches the
    // implicit sync `bougie run` / `bougie make` perform.
    if !args.no_sync {
        crate::commands::sync::run(
            format,
            false,
            false,
            None,
            None,
            bougie_cli::PhpPrefArgs::default(),
            bougie_composer_resolver::ResolutionStrategy::Highest,
        )?;
    }

    // Provision this project as a `server` tenant. Unlike `bougie up`,
    // the server service need NOT be declared in the project — this is
    // the zero-config "serve where I'm standing" path, so we send the
    // daemon call directly rather than going through the declared-only
    // selection in `up::run`.
    let paths = Paths::from_env()?;
    let tenant = match &args.name {
        Some(name) => sanitize_tenant(name),
        None => derive_default_tenant(&project_root, &paths),
    };
    let call_args = json!({
        "project": project_root,
        "services": [{"name": "server", "tenant": tenant}],
    });
    let _: Value = client::call(&paths, "service.up", call_args)?;

    let hostname = derive_hostname(&tenant);
    let port = server_listen_port(&paths);
    let url = build_url(args.tls, &hostname, port);
    emit(format, &ServeResult {
        schema_version: 1,
        project: project_root,
        hostname: hostname.clone(),
        url: url.clone(),
        port,
    })?;

    if args.open {
        // Best-effort: a failed launch (headless, no opener) is not
        // fatal — the URL is already printed above.
        let _ = open_url(&url);
    }

    // Attach to the dev server's log, the way `bougie up` follows its
    // services. Gated to interactive text mode; a non-TTY run or
    // `--format json-v1` implicitly detaches, as does `-d`/`--detach`.
    // Ctrl-C only detaches the CLI; the shared server keeps running.
    let attach =
        !args.detach && matches!(format, OutputFormat::Text) && std::io::stdout().is_terminal();
    if attach {
        eprintln!(
            "attached to the dev server log — Ctrl-C to detach (server keeps running); \
             stop with `bougie server stop`"
        );
        // Scope the attach to this project's vhost so a shared server
        // hosting several projects doesn't interleave their requests.
        let log_args = json!({
            "service": "server",
            "lines": 10,
            "follow": true,
            "host": hostname,
        });
        client::call_streaming(&paths, "service.logs", log_args)?;
    }
    Ok(ExitCode::SUCCESS)
}

/// `bougie server open [NAME]` — open a project's dev URL in a browser.
fn open(format: OutputFormat, name: Option<String>) -> Result<ExitCode> {
    use bougie_paths::Paths;

    let paths = Paths::from_env()?;
    let tenant = if let Some(name) = name {
        sanitize_tenant(&name)
    } else {
        let project_root = locate_project_root()?;
        derive_default_tenant(&project_root, &paths)
    };
    let hostname = derive_hostname(&tenant);
    let url = build_url(false, &hostname, server_listen_port(&paths));
    emit(format, &OpenResult { schema_version: 1, url: url.clone() })?;
    open_url(&url)?;
    Ok(ExitCode::SUCCESS)
}

#[derive(serde::Serialize)]
struct OpenResult {
    schema_version: u32,
    url: String,
}

impl bougie_output::output::Render for OpenResult {
    fn render_text(&self, w: &mut dyn std::io::Write) -> std::io::Result<()> {
        writeln!(w, "opening {}", self.url)
    }
}

#[cfg(unix)]
fn stop(format: OutputFormat) -> Result<ExitCode> {
    // Stopping the shared server == taking the `server` service down.
    super::services::down::run(format, vec!["server".to_string()], false)
}

/// The current project's dev-server vhost, or `None` when not in a
/// project (so `bougie server logs` outside a project shows everything).
#[cfg(unix)]
fn current_project_host() -> Option<String> {
    let project_root = locate_project_root().ok()?;
    let paths = bougie_paths::Paths::from_env().ok()?;
    let tenant = derive_default_tenant(&project_root, &paths);
    Some(derive_hostname(&tenant))
}

#[cfg(unix)]
fn logs(_format: OutputFormat, follow: bool, lines: usize) -> Result<ExitCode> {
    use crate::commands::services::client;
    use bougie_paths::Paths;
    use serde_json::json;

    let paths = Paths::from_env()?;
    let mut args = json!({"service": "server", "lines": lines, "follow": follow});
    // In a project, scope to this project's vhost; outside one, stream
    // the whole dev-server log.
    if let Some(host) = current_project_host() {
        args["host"] = json!(host);
    }
    client::call_streaming(&paths, "service.logs", args)?;
    Ok(ExitCode::SUCCESS)
}

#[cfg(unix)]
fn hosts(format: OutputFormat, cmd: bougie_cli::ServerHostsCommand) -> Result<ExitCode> {
    match cmd {
        bougie_cli::ServerHostsCommand::Apply { config } => {
            let path = resolve_config(config)?;
            bougie_server::server::hosts::apply(format, &path)
        }
    }
}

#[cfg(unix)]
fn tls(format: OutputFormat, cmd: &bougie_cli::ServerTlsCommand) -> Result<ExitCode> {
    match cmd {
        bougie_cli::ServerTlsCommand::Install => bougie_server::server::tls::install(format),
        bougie_cli::ServerTlsCommand::Uninstall => bougie_server::server::tls::uninstall(format),
    }
}

/// Windows standalone `serve` (no bougied): see [`serve_standalone`].
#[cfg(not(unix))]
fn serve(format: OutputFormat, args: &ServeArgs) -> Result<ExitCode> {
    serve_standalone(format, args)
}

/// Standalone (no-daemon) serve: synthesize a one-host `server.toml`
/// for the current project and run the server in the foreground.
/// Multi-tenant by `Host:` header on one port, exactly like the Unix
/// daemon-managed server — re-running for another project adds its
/// `[[host]]` to the same file. Ctrl-C stops the process (there's no
/// daemon to detach into).
///
/// Defined cross-platform so it type-checks on every host even though
/// only the non-Unix `serve` calls it (the Unix path goes through the
/// daemon instead).
#[cfg_attr(unix, allow(dead_code))]
fn serve_standalone(format: OutputFormat, args: &ServeArgs) -> Result<ExitCode> {
    use bougie_config::load_project;
    use bougie_paths::Paths;

    let project_root = locate_project_root()?;
    let project = load_project(&project_root)?;

    if !args.no_sync {
        crate::commands::sync::run(
            format,
            false,
            false,
            None,
            None,
            bougie_cli::PhpPrefArgs::default(),
            bougie_composer_resolver::ResolutionStrategy::Highest,
        )?;
    }

    let paths = Paths::from_env()?;
    let tenant = match &args.name {
        Some(name) => sanitize_tenant(name),
        None => derive_default_tenant(&project_root, &paths),
    };
    let hostname = derive_hostname(&tenant);
    let root = resolve_web_root(&project_root, project.bougie.server.root.as_deref())?;

    let cfg_path = paths.service_conf("server").join("server.toml");
    if let Some(parent) = cfg_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // Idempotent + seeds the file from a skeleton when missing.
    bougie_server::server::config::add_host_with_rewrites(
        &cfg_path,
        &hostname,
        &project_root,
        Some(&root),
        &[],
    )?;

    let port = server_listen_port(&paths);
    let url = build_url(args.tls, &hostname, port);
    emit(format, &ServeResult {
        schema_version: 1,
        project: project_root,
        hostname,
        url: url.clone(),
        port,
    })?;
    if args.open {
        let _ = open_url(&url);
    }

    // Foreground: blocks until Ctrl-C. `-d`/`--detach` has no effect here —
    // there's no background daemon to detach into.
    bougie_server::server::run::run(format, &cfg_path, None, None)
}

#[cfg(not(unix))]
fn stop(_format: OutputFormat) -> Result<ExitCode> {
    windows_unsupported("bougie server stop")
}

#[cfg(not(unix))]
fn logs(_format: OutputFormat, _follow: bool, _lines: usize) -> Result<ExitCode> {
    windows_unsupported("bougie server logs")
}

#[cfg(not(unix))]
fn hosts(_format: OutputFormat, _cmd: bougie_cli::ServerHostsCommand) -> Result<ExitCode> {
    windows_unsupported("bougie server hosts")
}

#[cfg(not(unix))]
fn tls(_format: OutputFormat, _cmd: &bougie_cli::ServerTlsCommand) -> Result<ExitCode> {
    windows_unsupported("bougie server tls")
}

#[cfg(not(unix))]
fn windows_unsupported(feature: &str) -> Result<ExitCode> {
    Err(eyre::eyre!(
        "{feature} is not supported on Windows yet — see SERVER.md."
    ))
}

#[cfg(test)]
mod tests {
    use super::{build_url, derive_hostname, resolve_web_root, sanitize_tenant};

    #[test]
    fn hostname_swaps_underscores_for_hyphens() {
        assert_eq!(derive_hostname("acme_blog"), "acme-blog.bougie.run");
        assert_eq!(derive_hostname("shop"), "shop.bougie.run");
    }

    #[test]
    fn url_drops_default_port() {
        assert_eq!(build_url(false, "a.bougie.run", 80), "http://a.bougie.run");
        assert_eq!(build_url(false, "a.bougie.run", 7080), "http://a.bougie.run:7080");
        assert_eq!(build_url(true, "a.bougie.run", 443), "https://a.bougie.run");
        assert_eq!(build_url(true, "a.bougie.run", 7443), "https://a.bougie.run:7443");
    }

    #[test]
    fn sanitize_lowercases_and_replaces_non_alnum() {
        assert_eq!(sanitize_tenant("Acme/Blog"), "acme_blog");
        assert_eq!(sanitize_tenant("my-shop.local"), "my_shop_local");
    }

    #[test]
    fn web_root_prefers_explicit_then_errors_on_empty() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(resolve_web_root(dir.path(), Some("web")).unwrap(), "web");
        assert!(resolve_web_root(dir.path(), Some("")).is_err());
    }

    #[test]
    fn web_root_autodetects_pub_before_public_else_errors() {
        let dir = tempfile::tempdir().unwrap();
        // Nothing present yet → error.
        assert!(resolve_web_root(dir.path(), None).is_err());
        // `public` only.
        std::fs::create_dir(dir.path().join("public")).unwrap();
        assert_eq!(resolve_web_root(dir.path(), None).unwrap(), "public");
        // `pub` takes precedence once it exists.
        std::fs::create_dir(dir.path().join("pub")).unwrap();
        assert_eq!(resolve_web_root(dir.path(), None).unwrap(), "pub");
    }

    // Tenant derivation (basename, reuse, collision) is tested in
    // `crate::commands::tenant`.
}
