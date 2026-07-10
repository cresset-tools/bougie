//! `bougie up [<name>…]` — promoted to a top-level verb from its
//! original home as `bougie service up`. The module path keeps the
//! `service::up` name because the handler still belongs to the
//! services subsystem semantically; only the user-facing CLI surface
//! moved. See CLI.md §3.8.4.

use super::client;
use super::config_mut::locate_project_root;
use super::ide;
use bougie_cli::OutputFormat;
use bougie_config::{load_project, ServicePin};
use bougie_daemon::daemon::store_fetch::ResolvedTool;
use bougie_output::output::{Render, emit};
use bougie_paths::Paths;
use eyre::{eyre, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::io::{self, IsTerminal, Write};
use std::process::ExitCode;

#[derive(Debug, Serialize)]
pub struct ServicesUpResult {
    pub schema_version: u32,
    pub started: Vec<String>,
    pub tenants: BTreeMap<String, String>,
    /// Per-service inventory of resolved tool dependencies. Populated
    /// for services whose auto-fetch path walked a non-empty
    /// `requires_tools[]`; empty (or absent at the JSON layer when
    /// serialized via `skip_serializing_if`) for services that were
    /// already on disk or have no inner-tool deps.
    ///
    /// Per `UNBUNDLE_PLAN.md` Phase 4. Schema bumped to 2 because the
    /// envelope shape grew this field; other CLI command results stay
    /// at `schema_version=1`.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub dependencies: BTreeMap<String, Vec<ResolvedTool>>,
}

#[derive(Debug, Deserialize)]
struct DaemonReply {
    #[serde(default)]
    started: Vec<String>,
    #[serde(default)]
    tenants: BTreeMap<String, String>,
    #[serde(default)]
    dependencies: BTreeMap<String, Vec<ResolvedTool>>,
}

impl Render for ServicesUpResult {
    fn render_text(&self, w: &mut dyn Write) -> io::Result<()> {
        if self.started.is_empty() && self.tenants.is_empty() {
            writeln!(w, "no services to start")?;
            return Ok(());
        }
        for s in &self.started {
            writeln!(w, "started {s}")?;
        }
        for (svc, tenant) in &self.tenants {
            writeln!(w, "tenant for {svc}: {tenant}")?;
        }
        Ok(())
    }
}

pub fn run(format: OutputFormat, names: Vec<String>, detach: bool) -> Result<ExitCode> {
    let project_root = locate_project_root()?;
    let project = load_project(&project_root)?;

    // Figure out which services to bring up. With no argument, every
    // service declared in the project. With names, intersection of the
    // request with the project's declarations.
    let declared: Vec<(String, &ServicePin)> = project
        .bougie
        .services
        .iter()
        .map(|(k, v)| (k.clone(), v))
        .collect();
    // A project runs one relational DB. Reject a hand-edited config that
    // declares both mariadb and mysql before bringing anything up — even a
    // targeted `bougie up mysql` shouldn't proceed from an ambiguous set.
    if let Some((a, b)) =
        bougie_daemon::daemon::catalog::exclusive_conflict(declared.iter().map(|(k, _)| k.as_str()))
    {
        return Err(eyre!(
            "this project declares both `{a}` and `{b}`, but they're mutually exclusive \
             relational databases — keep one in `[services]` (`bougie service remove {b}`)."
        ));
    }
    let selected: Vec<(String, &ServicePin)> = if names.is_empty() {
        declared
    } else {
        let mut out = Vec::new();
        for n in &names {
            if let Some((name, pin)) = declared.iter().find(|(k, _)| k == n) {
                out.push((name.clone(), *pin));
            } else {
                return Err(eyre!(
                    "service `{n}` isn't declared in this project (try `bougie service add {n}` first)"
                ));
            }
        }
        out
    };
    if selected.is_empty() {
        emit(format, &ServicesUpResult {
            schema_version: 2,
            started: vec![],
            tenants: BTreeMap::new(),
            dependencies: BTreeMap::new(),
        })?;
        return Ok(ExitCode::SUCCESS);
    }

    // Default tenant: sanitized project dir basename, made unique +
    // stable against the on-disk ledgers. See `commands::tenant`.
    let paths = Paths::from_env()?;
    let default_tenant = crate::commands::tenant::derive_default_tenant(&project_root, &paths);

    let services_payload: Vec<Value> = selected
        .iter()
        .map(|(name, pin)| -> Result<Value> {
            let tenant = pin
                .tenant().map_or_else(|| default_tenant.clone(), str::to_owned);
            let version = resolve_service_version(name, pin)?;
            Ok(json!({"name": name, "version": version, "tenant": tenant}))
        })
        .collect::<Result<Vec<_>>>()?;
    let args = json!({
        "project": project_root,
        "services": services_payload,
    });

    let reply: DaemonReply = client::call(&paths, "service.up", args)?;
    let result = ServicesUpResult {
        schema_version: 2,
        started: reply.started,
        tenants: reply.tenants,
        dependencies: reply.dependencies,
    };
    emit(format, &result)?;

    // Drop a PhpStorm data source for the project's MariaDB into `.idea/`
    // so the database is pre-wired in the IDE. Pure sugar: never let an
    // IDE-file hiccup fail `up`. Runs before the (blocking) log attach so
    // it happens regardless of follow/detach. Disable with
    // `BOUGIE_IDE_DATASOURCES=0`.
    match ide::write_phpstorm_datasources(&project_root, &paths, &result.tenants) {
        Ok(Some(path)) => tracing::debug!("wrote PhpStorm data source to {}", path.display()),
        Ok(None) => {}
        Err(e) => tracing::warn!("skipped PhpStorm data source: {e:#}"),
    }

    // Magento bakes the DB user/name into `app/etc/env.php` at
    // `setup:install` time. If that baked user no longer matches the
    // tenant we just provisioned — e.g. the tenant drifted under an older
    // bougie, or env.php was copied in from another project — Magento
    // connects as a user the server won't authenticate, and that surfaces
    // as a cryptic `SQLSTATE[HY000] [1698] Access denied` deep inside a
    // later build step. Surface it here instead, up front, with the fix.
    // Best-effort and stderr-only: it never fails `up` and never touches
    // the stdout result envelope (so `--format json-v1` stays clean).
    if let Some(tenant) = result.tenants.get("mariadb")
        && let Some(env_user) = read_magento_db_username(&project_root)
        && env_user != *tenant
    {
        eprintln!(
            "warning: app/etc/env.php connects to MariaDB as user '{env_user}', but this \
             project's provisioned tenant is '{tenant}'.\n         Magento will fail with \
             `SQLSTATE[HY000] [1698] Access denied for user '{env_user}'`. Fix by re-running \
             the installer against the current tenant (remove app/etc/env.php, then `bougie \
             start`) or by updating the db credentials in env.php to '{tenant}'."
        );
    }

    // Repoint any stale bougie socket path baked into app/etc/env.php
    // (Magento's db `host`, redis `server`) at the project's stable
    // connection socket. A shop installed under an older bougie baked the
    // then-current instance socket, which version-keying + the run-dir
    // move relocate; without this the app fails with `[2002] No such file
    // or directory` after the upgrade. Runs after the daemon created the
    // stable symlink (during the up IPC above), so the target exists.
    repoint_env_php_sockets(&project_root, &paths);

    // Attach to the combined ("multilog") stream of the services we
    // brought up, the way `docker compose up` follows its containers.
    // Gated to an interactive text-mode invocation: a non-TTY run (CI,
    // `bougie up | …`) or `--format json-v1` would never want a blocking
    // follow, so those implicitly detach — as does an explicit
    // `--detach`. The follow runs until Ctrl-C, which only detaches the
    // CLI; the daemon keeps the services running. Recipe steps that
    // shell out to `bougie up <svc>` pass `--detach` so the build never
    // blocks here (see recipes/{magento,laravel,generic}.toml).
    let attach = !detach
        && matches!(format, OutputFormat::Text)
        && std::io::stdout().is_terminal();
    if attach {
        let follow: Vec<String> = selected.iter().map(|(n, _)| n.clone()).collect();
        if !follow.is_empty() {
            eprintln!(
                "attached to logs for {} — Ctrl-C to detach (services keep running); `bougie up -d` to skip",
                follow.join(", ")
            );
            let log_args = json!({
                "services": follow,
                "lines": 10,
                "follow": true,
                // `attach` already required a TTY, so colorize the
                // per-service prefixes; the daemon writes the ANSI codes.
                "color": true,
            });
            client::call_streaming(&paths, "service.logs", log_args)?;
        }
    }
    Ok(ExitCode::SUCCESS)
}

/// Resolve a project's `[services]` pin to the concrete version the
/// daemon should run this service at.
///
/// This is the seam where a pin (`mysql = "8.0"`, `redis = "*"`, or a
/// `[services.mysql] version = "…"` table) becomes one exact version. The
/// pin is treated as a Composer constraint and intersected with the
/// service's compiled-in known set ([`CatalogEntry::versions`]); the
/// highest match wins, mirroring `bougie-resolver`'s PHP + extension
/// resolution:
///
/// - `*` / unset → the catalog default.
/// - `8.0` → highest known `8.0.*` (`mysql` → `8.0.46`); `8.4` → `8.4.10`.
/// - `^8.0`, `8` → highest known match across the range.
/// - an exact `X.Y.Z` the catalog doesn't list → passed through verbatim,
///   so the index (authoritative at fetch time) can still serve a patch
///   newer than the one bougie shipped.
/// - anything with no match and not a concrete version → a clear error.
///
/// [`CatalogEntry::versions`]: bougie_daemon::daemon::catalog::CatalogEntry::versions
fn resolve_service_version(name: &str, pin: &ServicePin) -> Result<String> {
    use bougie_daemon::daemon::catalog;
    use composer_semver::constraint::Constraint;
    use composer_semver::version::Version;

    let entry = catalog::find(name);
    let default = entry.map_or("", |e| e.version);
    let known: &[&str] = entry.map_or(&[], |e| e.versions);

    let Some(raw) = pin.version() else {
        return Ok(default.to_owned());
    };
    let p = raw.trim();
    if p.is_empty() || p == "*" {
        return Ok(default.to_owned());
    }

    // Intersect the pin with the known versions and take the highest
    // match. Handles exact hits, partials (`8.0` → 8.0.46), and ranges.
    if let Ok(constraint) = Constraint::parse(p) {
        let best = known
            .iter()
            .filter_map(|v| Version::parse(v).ok().map(|parsed| (*v, parsed)))
            .filter(|(_, parsed)| constraint.matches(parsed))
            .max_by(|(_, a), (_, b)| a.cmp(b))
            .map(|(v, _)| v.to_owned());
        if let Some(best) = best {
            return Ok(best);
        }
    }

    // No known version matched. If the pin names a concrete `X.Y.Z`,
    // trust it and let the daemon's index fetch validate/serve it — this
    // is how a project pins a published patch the compiled catalog omits.
    if is_full_exact(p) {
        return Ok(p.to_owned());
    }

    Err(eyre!(
        "service `{name}`: no known version satisfies pin `{p}` (bougie ships: {}). \
         Pin `*`, one of those versions, or an exact published patch.",
        if known.is_empty() {
            "(none)".to_owned()
        } else {
            known.join(", ")
        }
    ))
}

/// A pin naming one concrete version — at least three dotted numeric
/// components (`8.4.10`, `1.30.2`). Such a pin is trusted verbatim when
/// it doesn't match the compiled catalog set, deferring to the index.
/// `8` and `8.4` are partials (constraints), not exact.
fn is_full_exact(v: &str) -> bool {
    let comps: Vec<&str> = v.split('.').collect();
    comps.len() >= 3
        && comps
            .iter()
            .all(|c| !c.is_empty() && c.bytes().all(|b| b.is_ascii_digit()))
}

/// Best-effort read of the default DB connection's `username` from
/// `app/etc/env.php` (Magento writes it at `setup:install`). Returns None
/// when the file is missing or unreadable; the parse itself lives in
/// [`parse_db_username`] so it can be unit-tested without a fixture file.
fn read_magento_db_username(project_root: &std::path::Path) -> Option<String> {
    let text = std::fs::read_to_string(project_root.join("app/etc/env.php")).ok()?;
    parse_db_username(&text)
}

/// Pull the `'username'` under env.php's top-level `'db'` key. A regex-lite
/// scan over the literal — the same approach `commands::make` uses to read
/// `backend.frontName` — is robust enough for a warning, and anchoring on
/// `'db'` keeps it from picking up an unrelated `'username'` elsewhere in
/// the config (amqp uses `'user'`, not `'username'`). Returns None when the
/// key can't be located.
fn parse_db_username(env_php: &str) -> Option<String> {
    let (_, after) = env_php.split_once("'db'")?;
    let (_, after) = after.split_once("'username'")?;
    let (_, after) = after.split_once("=>")?;
    let after = after.trim_start();
    let quote = after.chars().next()?;
    if quote != '\'' && quote != '"' {
        return None;
    }
    let rest = &after[1..];
    let end = rest.find(quote)?;
    Some(rest[..end].to_string())
}

/// Rewrite every bougie-owned unix-socket path baked into `env.php` to
/// the project's **stable connection socket**, keyed by the socket file
/// name.
///
/// A shop installed under an older bougie baked the then-current instance
/// socket path (Magento's db `host`, redis `server`, …). Version-keying +
/// the run-dir relocation move that socket, so the literal goes stale and
/// the app dies with `[2002] No such file or directory`. We repoint each
/// stale path — any absolute `*.sock` under `state_prefix` (bougie-owned,
/// so a user's external `/var/run/mysqld/mysqld.sock` is never touched) —
/// at the `state/conn/<project-hash>/<sockname>` symlink the daemon keeps
/// pointed at the live instance.
///
/// Returns the rewritten text + the paths replaced, or `None` when
/// nothing changed. Pure over its inputs so it unit-tests without a
/// fixture file.
fn rewrite_env_php_sockets(
    env_php: &str,
    state_prefix: &std::path::Path,
    stable_for: impl Fn(&str) -> std::path::PathBuf,
) -> Option<(String, Vec<String>)> {
    let mut repl: Vec<(String, String)> = Vec::new();
    let bytes = env_php.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        if (c == b'\'' || c == b'"')
            && let Some(rel) = env_php[i + 1..].find(c as char)
        {
            // Socket paths carry no quotes/backslashes, so a naive
            // close-quote scan (no PHP escape handling) is exact here.
            let content = &env_php[i + 1..i + 1 + rel];
            if content.starts_with('/')
                && content.ends_with(".sock")
                && std::path::Path::new(content).starts_with(state_prefix)
                && let Some(sockname) =
                    std::path::Path::new(content).file_name().and_then(|s| s.to_str())
            {
                let target = stable_for(sockname).display().to_string();
                if target != content && !repl.iter().any(|(o, _)| o == content) {
                    repl.push((content.to_string(), target));
                }
            }
            i += 1 + rel + 1;
            continue;
        }
        i += 1;
    }
    if repl.is_empty() {
        return None;
    }
    let mut out = env_php.to_string();
    let mut changed = Vec::new();
    for (old, new) in &repl {
        out = out.replace(old.as_str(), new);
        changed.push(old.clone());
    }
    Some((out, changed))
}

/// Repoint stale bougie socket paths in `<project>/app/etc/env.php` at the
/// project's stable connection socket. Best-effort + stderr-only: a shop
/// that never touched env.php (no Magento) or has no stale paths is a
/// no-op, and an I/O hiccup never fails `up`.
fn repoint_env_php_sockets(project_root: &std::path::Path, paths: &bougie_paths::Paths) {
    let env_php_path = project_root.join("app/etc/env.php");
    let Ok(text) = std::fs::read_to_string(&env_php_path) else {
        return;
    };
    let state_prefix = paths.state();
    let Some((new_text, changed)) = rewrite_env_php_sockets(&text, &state_prefix, |sockname| {
        paths.project_conn_socket(project_root, sockname)
    }) else {
        return;
    };
    match std::fs::write(&env_php_path, new_text) {
        Ok(()) => eprintln!(
            "note: repointed {} stale service socket path(s) in app/etc/env.php at this \
             project's stable socket:\n         {}",
            changed.len(),
            changed.join("\n         "),
        ),
        Err(e) => {
            eprintln!("warning: could not update stale socket paths in app/etc/env.php: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        is_full_exact, parse_db_username, resolve_service_version, rewrite_env_php_sockets,
    };
    use bougie_config::ServicePin;
    use std::path::{Path, PathBuf};

    fn pin(v: &str) -> ServicePin {
        ServicePin::Version(v.to_owned())
    }

    #[test]
    fn is_full_exact_needs_three_numeric_components() {
        assert!(is_full_exact("1.30.2"));
        assert!(is_full_exact("8.4.10"));
        assert!(is_full_exact("11.4.4"));
        for spec in ["8", "8.4", "*", "^8.4", "~1.0", ">=8", "8.x", "8.0.0-rc1", "", "v8.4"] {
            assert!(!is_full_exact(spec), "{spec} should not be full-exact");
        }
    }

    #[test]
    fn multi_version_service_resolves_partials_to_highest_match() {
        // mysql ships 8.4.10 (default) + 8.0.46.
        assert_eq!(resolve_service_version("mysql", &pin("8.0")).unwrap(), "8.0.46");
        assert_eq!(resolve_service_version("mysql", &pin("8.4")).unwrap(), "8.4.10");
        // Bare major and caret both span the two → highest.
        assert_eq!(resolve_service_version("mysql", &pin("8")).unwrap(), "8.4.10");
        assert_eq!(resolve_service_version("mysql", &pin("^8.0")).unwrap(), "8.4.10");
        // Exact published version passes through as itself.
        assert_eq!(resolve_service_version("mysql", &pin("8.0.46")).unwrap(), "8.0.46");
    }

    #[test]
    fn wildcard_and_unset_resolve_to_default() {
        assert_eq!(resolve_service_version("mysql", &pin("*")).unwrap(), "8.4.10");
        // Detail table with no version set → default.
        let detail = ServicePin::Detail(Default::default());
        assert_eq!(resolve_service_version("mysql", &detail).unwrap(), "8.4.10");
    }

    #[test]
    fn single_version_service_resolves_compatible_pins() {
        // redis ships only 8.6.3; a compatible partial resolves to it.
        assert_eq!(resolve_service_version("redis", &pin("8.6")).unwrap(), "8.6.3");
        assert_eq!(resolve_service_version("redis", &pin("*")).unwrap(), "8.6.3");
        // An exact patch the catalog doesn't list is trusted (the index
        // is authoritative) — e.g. a newer mariadb than bougie shipped.
        assert_eq!(
            resolve_service_version("mariadb", &pin("11.4.12")).unwrap(),
            "11.4.12"
        );
    }

    #[test]
    fn unsatisfiable_pin_is_an_error() {
        // mysql doesn't ship a 9.x, and `9.0` isn't a concrete patch to
        // pass through → clear error rather than a silent default.
        assert!(resolve_service_version("mysql", &pin("9.0")).is_err());
        // A partial with no matching known version on a single-version
        // service also errors.
        assert!(resolve_service_version("redis", &pin("7")).is_err());
    }

    const ENV_PHP: &str = r"<?php
return array (
  'backend' => array ( 'frontName' => 'admin_x', ),
  'db' => array (
    'connection' => array (
      'default' => array (
        'host' => '/run/mariadb.sock',
        'dbname' => 'mageos_lite_836b41',
        'username' => 'mageos_lite_836b41',
        'password' => 's3cret',
      ),
    ),
  ),
  'queue' => array (
    'amqp' => array ( 'host' => 'localhost', 'user' => 'guest', ),
  ),
);
";

    #[test]
    fn extracts_db_username() {
        assert_eq!(
            parse_db_username(ENV_PHP).as_deref(),
            Some("mageos_lite_836b41")
        );
    }

    #[test]
    fn handles_short_array_syntax() {
        let s =
            "return [ 'db' => [ 'connection' => [ 'default' => [ 'username' => \"shop\" ] ] ] ];";
        assert_eq!(parse_db_username(s).as_deref(), Some("shop"));
    }

    #[test]
    fn does_not_mistake_amqp_user_for_db_username() {
        // No db username present; the amqp `'user'` must not be picked up.
        let s = "return array ( 'queue' => array ( 'amqp' => array ( 'user' => 'guest' ) ) );";
        assert_eq!(parse_db_username(s), None);
    }

    #[test]
    fn none_when_no_db_block() {
        assert_eq!(parse_db_username("<?php return array ();"), None);
    }

    /// Map every sockname to `/h/state/conn/PROJ/<sockname>`.
    fn stable(sockname: &str) -> PathBuf {
        PathBuf::from(format!("/h/state/conn/PROJ/{sockname}"))
    }

    #[test]
    fn repoints_stale_bougie_db_and_redis_sockets_leaving_external_alone() {
        // A shop installed under an older bougie: db host is the flat
        // pre-version-keying socket, redis is a versioned instance socket,
        // and there's a user's own external socket + a localhost host.
        let env = "return array (\n\
             'db' => array ( 'connection' => array ( 'default' => array (\n\
               'host' => '/h/state/services/mariadb/run/mariadb.sock',\n\
               'username' => 'shop' ) ) ),\n\
             'cache' => array ( 'frontend' => array ( 'default' => array (\n\
               'backend_options' => array ( 'server' => '/h/state/run/abc123def456/redis.sock' ) ) ) ),\n\
             'external' => '/var/run/mysqld/mysqld.sock',\n\
             'queue' => array ( 'amqp' => array ( 'host' => 'localhost' ) ) );";
        let (out, changed) = rewrite_env_php_sockets(env, Path::new("/h/state"), stable).unwrap();
        // Both bougie sockets repointed at the stable per-project socket.
        assert!(out.contains("'host' => '/h/state/conn/PROJ/mariadb.sock'"), "{out}");
        assert!(out.contains("'server' => '/h/state/conn/PROJ/redis.sock'"), "{out}");
        // A user's external socket + a TCP host are left untouched.
        assert!(out.contains("'/var/run/mysqld/mysqld.sock'"), "external socket must survive");
        assert!(out.contains("'host' => 'localhost'"), "TCP host must survive");
        assert_eq!(changed.len(), 2);
    }

    #[test]
    fn socket_rewrite_is_idempotent_and_skips_clean_configs() {
        // Already-stable path → nothing to do.
        let stable_cfg = "'host' => '/h/state/conn/PROJ/mariadb.sock',";
        assert!(rewrite_env_php_sockets(stable_cfg, Path::new("/h/state"), stable).is_none());
        // No bougie-owned sockets at all → nothing to do.
        let external = "'host' => 'localhost', 'server' => '/var/run/redis.sock',";
        assert!(rewrite_env_php_sockets(external, Path::new("/h/state"), stable).is_none());
    }
}
