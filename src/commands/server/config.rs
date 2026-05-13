//! `server.toml` — schema, load, helper mutations.
//!
//! Spec: SERVER.md §4.2. The shape is a single `[server]` table plus
//! a list of `[[host]]` blocks; each host can carry zero or more
//! `[[host.alias]]` entries.
//!
//! Mutations (`add`/`remove`) go through `toml_edit` so hand-written
//! comments and field order survive helper invocations.

use etcetera::base_strategy::{BaseStrategy, Xdg};
use eyre::{Result, WrapErr};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Default `[server].listen`.
pub const DEFAULT_LISTEN: &str = "127.0.0.1:7080";
/// Default `[server].log_format`.
pub const DEFAULT_LOG_FORMAT: &str = "text";
/// Default `[server].idle_pool_timeout` (human-readable).
pub const DEFAULT_IDLE_POOL_TIMEOUT: &str = "10m";
/// Default `[server].max_concurrent_pools`.
pub const DEFAULT_MAX_CONCURRENT_POOLS: u32 = 16;

/// Extensions that bougie holds back from the "normal" pool variant
/// (loaded only when a request is routed to the "xdebug" variant).
pub fn default_debug_only_extensions() -> Vec<String> {
    vec!["xdebug".into()]
}

/// Whole `server.toml` as parsed for runtime use.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default)]
pub struct ServerConfig {
    pub server: ServerSection,
    #[serde(rename = "host", default, skip_serializing_if = "Vec::is_empty")]
    pub hosts: Vec<HostBlock>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default)]
pub struct ServerSection {
    pub listen: String,
    pub log_format: String,
    pub idle_pool_timeout: String,
    pub max_concurrent_pools: u32,
    pub debug_only_extensions: Vec<String>,
    /// When true, `bougie server add` / `remove` re-sync the bougie
    /// sentinel block in `/etc/hosts` automatically by spawning
    /// `sudo bougie server hosts apply` after the server.toml mutation.
    /// Default `false` — opt-in for users on DNS-rebinding-protected
    /// networks (pi-hole, UniFi/OpenWRT, some corporate DNS) or fully
    /// offline machines.
    pub manage_etc_hosts: bool,
}

impl Default for ServerSection {
    fn default() -> Self {
        Self {
            listen: DEFAULT_LISTEN.into(),
            log_format: DEFAULT_LOG_FORMAT.into(),
            idle_pool_timeout: DEFAULT_IDLE_POOL_TIMEOUT.into(),
            max_concurrent_pools: DEFAULT_MAX_CONCURRENT_POOLS,
            debug_only_extensions: default_debug_only_extensions(),
            manage_etc_hosts: false,
        }
    }
}

impl ServerSection {
    /// Parse `idle_pool_timeout` into a `Duration`. Accepts the small
    /// suffix set used in deployment config (`s`, `m`, `h`, `d`) plus a
    /// bare integer seconds.
    pub fn idle_pool_timeout_duration(&self) -> Result<Duration> {
        parse_short_duration(&self.idle_pool_timeout).wrap_err_with(|| {
            format!("invalid idle_pool_timeout {:?}", self.idle_pool_timeout)
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct HostBlock {
    pub hostname: String,
    pub project: PathBuf,
    #[serde(default = "default_root")]
    pub root: String,
    #[serde(default = "default_index")]
    pub index: Vec<String>,
    #[serde(default = "default_try_files")]
    pub try_files: Vec<String>,
    #[serde(default, rename = "alias", skip_serializing_if = "Vec::is_empty")]
    pub aliases: Vec<HostAlias>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct HostAlias {
    pub hostname: String,
}

fn default_root() -> String {
    ".".into()
}

fn default_index() -> Vec<String> {
    vec!["index.php".into(), "index.html".into()]
}

fn default_try_files() -> Vec<String> {
    vec!["$uri".into(), "$uri/".into(), "/index.php$is_args$args".into()]
}

/// Resolve the active `server.toml` path. Prefers `--config` (caller
/// passes it in); otherwise `$XDG_CONFIG_HOME/bougie/server.toml`.
pub fn resolve_path(override_path: Option<&Path>) -> Result<PathBuf> {
    if let Some(p) = override_path {
        return Ok(p.to_path_buf());
    }
    let xdg = Xdg::new().wrap_err("could not resolve XDG base dirs")?;
    Ok(xdg.config_dir().join("bougie").join("server.toml"))
}

/// Load the config from `path`. Missing file returns `ServerConfig::default()`.
pub fn load(path: &Path) -> Result<ServerConfig> {
    if !path.exists() {
        return Ok(ServerConfig::default());
    }
    let text = std::fs::read_to_string(path)
        .wrap_err_with(|| format!("reading {}", path.display()))?;
    parse_str(&text).wrap_err_with(|| format!("parsing {}", path.display()))
}

/// Parse a TOML string into `ServerConfig`. Exposed for tests.
pub fn parse_str(text: &str) -> Result<ServerConfig> {
    toml_edit::de::from_str(text).wrap_err("parsing server.toml")
}

/// Append a `[[host]]` block. Returns `false` if a host with the same
/// hostname (or matching alias) is already present.
pub fn add_host(
    path: &Path,
    hostname: &str,
    project: &Path,
    root: Option<&str>,
) -> Result<bool> {
    validate_hostname(hostname)?;
    let project = canonicalize_project(project)?;

    let body = read_or_skeleton(path)?;
    let mut doc: toml_edit::DocumentMut = body
        .parse()
        .wrap_err_with(|| format!("parsing {}", path.display()))?;

    if find_host_index(&doc, hostname).is_some() {
        return Ok(false);
    }

    let mut table = toml_edit::Table::new();
    table["hostname"] = toml_edit::value(hostname);
    table["project"] = toml_edit::value(
        project
            .to_str()
            .ok_or_else(|| eyre::eyre!("project path is not UTF-8: {}", project.display()))?,
    );
    if let Some(r) = root {
        table["root"] = toml_edit::value(r);
    }

    let host = doc
        .entry("host")
        .or_insert(toml_edit::Item::ArrayOfTables(toml_edit::ArrayOfTables::new()))
        .as_array_of_tables_mut()
        .ok_or_else(|| eyre::eyre!("`host` is not an array of tables in {}", path.display()))?;
    host.push(table);

    write_atomically(path, &doc.to_string())?;
    Ok(true)
}

/// Remove the `[[host]]` block with matching `hostname` (top-level or
/// alias). Returns `true` if a block was removed.
pub fn remove_host(path: &Path, hostname: &str) -> Result<bool> {
    if !path.exists() {
        return Ok(false);
    }
    let body = std::fs::read_to_string(path)
        .wrap_err_with(|| format!("reading {}", path.display()))?;
    let mut doc: toml_edit::DocumentMut = body
        .parse()
        .wrap_err_with(|| format!("parsing {}", path.display()))?;
    let Some(idx) = find_host_index(&doc, hostname) else {
        return Ok(false);
    };
    let host = doc
        .get_mut("host")
        .and_then(toml_edit::Item::as_array_of_tables_mut)
        .ok_or_else(|| eyre::eyre!("`host` table disappeared during remove"))?;
    host.remove(idx);
    write_atomically(path, &doc.to_string())?;
    Ok(true)
}

/// Hostname validation. Loose ASCII rules — bougie picks the suffix,
/// so we only need to reject things that won't survive a DNS lookup
/// or HTTP Host header.
fn validate_hostname(hostname: &str) -> Result<()> {
    if hostname.is_empty() {
        return Err(eyre::eyre!("hostname is empty"));
    }
    if hostname.len() > 253 {
        return Err(eyre::eyre!("hostname too long (>253 chars)"));
    }
    for label in hostname.split('.') {
        if label.is_empty() || label.len() > 63 {
            return Err(eyre::eyre!("invalid label in hostname: {hostname:?}"));
        }
        let bytes = label.as_bytes();
        if !bytes.iter().all(|c| c.is_ascii_alphanumeric() || *c == b'-') {
            return Err(eyre::eyre!("invalid character in hostname: {hostname:?}"));
        }
        if bytes.first() == Some(&b'-') || bytes.last() == Some(&b'-') {
            return Err(eyre::eyre!("hostname label may not begin or end with `-`"));
        }
    }
    Ok(())
}

fn canonicalize_project(project: &Path) -> Result<PathBuf> {
    // `canonicalize` would be ideal but the project might be a path
    // the user is planning to create. Reject only the obvious miss:
    // relative paths land in server.toml as-is and the server would
    // resolve them at runtime against an unpredictable cwd.
    if project.is_relative() {
        let cwd = std::env::current_dir().wrap_err("getting current directory")?;
        Ok(cwd.join(project))
    } else {
        Ok(project.to_path_buf())
    }
}

fn find_host_index(doc: &toml_edit::DocumentMut, hostname: &str) -> Option<usize> {
    let host = doc.get("host").and_then(toml_edit::Item::as_array_of_tables)?;
    for (idx, table) in host.iter().enumerate() {
        if table
            .get("hostname")
            .and_then(toml_edit::Item::as_str)
            .is_some_and(|h| h == hostname)
        {
            return Some(idx);
        }
        if let Some(aliases) = table.get("alias").and_then(toml_edit::Item::as_array_of_tables) {
            for alias in aliases {
                if alias
                    .get("hostname")
                    .and_then(toml_edit::Item::as_str)
                    .is_some_and(|h| h == hostname)
                {
                    return Some(idx);
                }
            }
        }
    }
    None
}

fn read_or_skeleton(path: &Path) -> Result<String> {
    if path.exists() {
        std::fs::read_to_string(path).wrap_err_with(|| format!("reading {}", path.display()))
    } else {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .wrap_err_with(|| format!("creating {}", parent.display()))?;
        }
        Ok(skeleton())
    }
}

/// Hand-written skeleton emitted the first time `bougie server add`
/// runs without an existing `server.toml`. Comments mirror the docs in
/// SERVER.md §4.2 so users can see the knobs without crossing to the
/// web. The `[server]` block is left implicit — every field has a
/// default in [`ServerSection`], and the comments live free-floating
/// at the top so `toml_edit` doesn't bind them to a table that may
/// move once the user adds hosts.
pub fn skeleton() -> String {
    // Empty skeleton: every field has a default in [`ServerSection`],
    // and the file fills itself in as users run `bougie server add`.
    // toml_edit binds trailing comments oddly when the first edit
    // appends a `[[host]]` block, so we avoid a doc-header comment
    // here and document the schema in SERVER.md instead.
    String::new()
}

fn write_atomically(path: &Path, contents: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .wrap_err_with(|| format!("creating {}", parent.display()))?;
    }
    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, contents).wrap_err_with(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, path).wrap_err_with(|| format!("renaming {}", path.display()))?;
    Ok(())
}

fn parse_short_duration(s: &str) -> Result<Duration> {
    let s = s.trim();
    if s.is_empty() {
        return Err(eyre::eyre!("empty duration"));
    }
    let (num, unit) = match s.as_bytes().last() {
        Some(c) if c.is_ascii_digit() => (s, "s"),
        Some(_) => s.split_at(s.len() - 1),
        None => unreachable!("checked non-empty above"),
    };
    let n: u64 = num.parse().wrap_err_with(|| format!("not a number: {num:?}"))?;
    let secs = match unit {
        "s" => n,
        "m" => n.checked_mul(60).ok_or_else(|| eyre::eyre!("overflow"))?,
        "h" => n.checked_mul(3600).ok_or_else(|| eyre::eyre!("overflow"))?,
        "d" => n.checked_mul(86_400).ok_or_else(|| eyre::eyre!("overflow"))?,
        other => return Err(eyre::eyre!("unknown duration unit: {other:?}")),
    };
    Ok(Duration::from_secs(secs))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn empty_yields_defaults() {
        let cfg = parse_str("").unwrap();
        assert_eq!(cfg.server.listen, DEFAULT_LISTEN);
        assert_eq!(cfg.server.log_format, DEFAULT_LOG_FORMAT);
        assert_eq!(cfg.server.idle_pool_timeout, DEFAULT_IDLE_POOL_TIMEOUT);
        assert_eq!(cfg.server.max_concurrent_pools, DEFAULT_MAX_CONCURRENT_POOLS);
        assert_eq!(cfg.server.debug_only_extensions, vec!["xdebug".to_string()]);
        assert!(cfg.hosts.is_empty());
    }

    #[test]
    fn full_config_parses() {
        let text = r#"
[server]
listen = "0.0.0.0:7080"
log_format = "json-v1"
idle_pool_timeout = "5m"
max_concurrent_pools = 32
debug_only_extensions = ["xdebug", "ddtrace"]

[[host]]
hostname = "myapp.bougie.run"
project  = "/tmp/myapp"
root     = "public"
index    = ["index.php"]
try_files = ["$uri", "/index.php$is_args$args"]

[[host.alias]]
hostname = "myapp-staging.bougie.run"
"#;
        let cfg = parse_str(text).unwrap();
        assert_eq!(cfg.server.listen, "0.0.0.0:7080");
        assert_eq!(cfg.server.max_concurrent_pools, 32);
        assert_eq!(cfg.server.debug_only_extensions.len(), 2);
        assert_eq!(cfg.hosts.len(), 1);
        let h = &cfg.hosts[0];
        assert_eq!(h.hostname, "myapp.bougie.run");
        assert_eq!(h.project, PathBuf::from("/tmp/myapp"));
        assert_eq!(h.root, "public");
        assert_eq!(h.aliases.len(), 1);
        assert_eq!(h.aliases[0].hostname, "myapp-staging.bougie.run");
    }

    #[test]
    fn host_defaults_apply() {
        let cfg = parse_str(
            r#"
[[host]]
hostname = "x.bougie.run"
project  = "/tmp/x"
"#,
        )
        .unwrap();
        let h = &cfg.hosts[0];
        assert_eq!(h.root, ".");
        assert_eq!(h.index, vec!["index.php", "index.html"]);
        assert_eq!(h.try_files, vec!["$uri", "$uri/", "/index.php$is_args$args"]);
    }

    #[test]
    fn duration_parses() {
        assert_eq!(parse_short_duration("10m").unwrap(), Duration::from_mins(10));
        assert_eq!(parse_short_duration("90s").unwrap(), Duration::from_secs(90));
        assert_eq!(parse_short_duration("2h").unwrap(), Duration::from_hours(2));
        assert_eq!(parse_short_duration("1d").unwrap(), Duration::from_hours(24));
        assert_eq!(parse_short_duration("45").unwrap(), Duration::from_secs(45));
        assert!(parse_short_duration("").is_err());
        assert!(parse_short_duration("3x").is_err());
    }

    #[test]
    fn add_host_creates_file_with_skeleton_then_appends() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("server.toml");
        let added =
            add_host(&path, "myapp.bougie.run", Path::new("/tmp/myapp"), Some("public")).unwrap();
        assert!(added);
        let cfg = load(&path).unwrap();
        assert_eq!(cfg.hosts.len(), 1);
        assert_eq!(cfg.hosts[0].hostname, "myapp.bougie.run");
        assert_eq!(cfg.hosts[0].project, PathBuf::from("/tmp/myapp"));
        assert_eq!(cfg.hosts[0].root, "public");

        // Re-adding is idempotent.
        let again =
            add_host(&path, "myapp.bougie.run", Path::new("/tmp/myapp"), Some("public")).unwrap();
        assert!(!again);
    }

    #[test]
    fn remove_host_drops_block() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("server.toml");
        add_host(&path, "a.bougie.run", Path::new("/tmp/a"), None).unwrap();
        add_host(&path, "b.bougie.run", Path::new("/tmp/b"), None).unwrap();

        assert!(remove_host(&path, "a.bougie.run").unwrap());
        let cfg = load(&path).unwrap();
        assert_eq!(cfg.hosts.len(), 1);
        assert_eq!(cfg.hosts[0].hostname, "b.bougie.run");

        // Removing a missing entry is a no-op (returns false).
        assert!(!remove_host(&path, "ghost.bougie.run").unwrap());
    }

    #[test]
    fn remove_finds_alias() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("server.toml");
        // Construct manually: helper doesn't expose alias creation in
        // phase 0.
        std::fs::write(
            &path,
            r#"
[[host]]
hostname = "main.bougie.run"
project = "/tmp/main"

[[host.alias]]
hostname = "alias.bougie.run"
"#,
        )
        .unwrap();
        assert!(remove_host(&path, "alias.bougie.run").unwrap());
        let cfg = load(&path).unwrap();
        assert!(cfg.hosts.is_empty());
    }

    #[test]
    fn add_preserves_top_level_comments() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("server.toml");
        std::fs::write(
            &path,
            r#"# my custom comment
[server]
listen = "0.0.0.0:7080"  # bound everywhere
"#,
        )
        .unwrap();
        add_host(&path, "a.bougie.run", Path::new("/tmp/a"), None).unwrap();
        let body = std::fs::read_to_string(&path).unwrap();
        assert!(body.contains("my custom comment"));
        assert!(body.contains("bound everywhere"));
        assert!(body.contains("a.bougie.run"));
    }

    #[test]
    fn validate_hostname_rules() {
        assert!(validate_hostname("a.bougie.run").is_ok());
        assert!(validate_hostname("my-app.bougie.run").is_ok());
        assert!(validate_hostname("").is_err());
        assert!(validate_hostname("-bad.bougie.run").is_err());
        assert!(validate_hostname("bad-.bougie.run").is_err());
        assert!(validate_hostname("under_score.bougie.run").is_err());
        assert!(validate_hostname(&"a".repeat(64)).is_err());
    }
}
