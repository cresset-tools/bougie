//! `bougie server hosts apply` — rewrite the bougie-managed sentinel
//! block in `/etc/hosts` to match the configured `[[host]]` entries.
//! Spec: SERVER.md §3.3.
//!
//! Source of truth for the host list is `server.toml`; this module
//! has no separate state file. The function that does the actual
//! splice is parameterized on the file path so unit tests can drive
//! it against a tempfile without root.
//!
//! When `[server].manage_etc_hosts = true`, `bougie server add` and
//! `bougie server remove` invoke this automatically via `sudo`. See
//! [`spawn_sudo_apply`].

use bougie_cli::OutputFormat;
use bougie_output::output::{emit, Render};
use eyre::{Result, WrapErr};
use serde::Serialize;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use super::config::{self, ServerConfig};

pub const DEFAULT_ETC_HOSTS: &str = "/etc/hosts";
pub const BLOCK_BEGIN: &str = "# BEGIN bougie";
pub const BLOCK_END: &str = "# END bougie";

/// Path to the hosts file this command targets. `BOUGIE_ETC_HOSTS_PATH`
/// overrides the default `/etc/hosts` — used by integration tests so
/// they don't need root and don't risk poisoning the developer's real
/// hosts file.
pub fn etc_hosts_path() -> PathBuf {
    std::env::var_os("BOUGIE_ETC_HOSTS_PATH")
        .map_or_else(|| PathBuf::from(DEFAULT_ETC_HOSTS), PathBuf::from)
}

/// When the env override is set, the root check is bypassed (tempfiles
/// don't need root). The default path always requires root.
fn requires_root(target: &Path) -> bool {
    target == Path::new(DEFAULT_ETC_HOSTS)
}

#[derive(Debug, Serialize)]
pub struct ApplyResult {
    pub schema_version: u32,
    pub path: PathBuf,
    /// Hostnames now in the sentinel block.
    pub hostnames: Vec<String>,
    /// `true` if `/etc/hosts` was actually modified (no-op when the
    /// block already matched).
    pub changed: bool,
}

impl Render for ApplyResult {
    fn render_text(&self, w: &mut dyn Write) -> io::Result<()> {
        if self.changed {
            writeln!(
                w,
                "synced {} ({} hostnames)",
                self.path.display(),
                self.hostnames.len()
            )?;
        } else {
            writeln!(w, "no change to {}", self.path.display())?;
        }
        Ok(())
    }
}

pub fn apply(
    format: OutputFormat,
        config_override: Option<&Path>,
) -> Result<ExitCode> {
    let target = etc_hosts_path();
    if requires_root(&target) && !is_root() {
        eprintln!(
            "bougie server hosts apply: {} can only be written by root.\n\
             Run: sudo bougie server hosts apply{}",
            target.display(),
            match config_override {
                Some(p) => format!(" --config {}", p.display()),
                None => String::new(),
            }
        );
        return Ok(ExitCode::from(1));
    }
    let cfg_path = config::resolve_path(config_override)?;
    let cfg = config::load(&cfg_path)?;
    let hostnames = hostnames_for_etc_hosts(&cfg);

    let changed = rewrite_sentinel_block(&target, &hostnames)?;
    let result = ApplyResult {
        schema_version: 1,
        path: target,
        hostnames,
        changed,
    };
    emit(format, &result)?;
    Ok(ExitCode::SUCCESS)
}

/// Spawn `sudo <self> server hosts apply --config <abs-path>` and wait
/// for it. Inherits stdio so sudo can prompt for the user's password.
///
/// When `BOUGIE_ETC_HOSTS_PATH` is set (integration tests) sudo is
/// skipped — the tempfile target doesn't need root, and asking for a
/// password during `cargo test` would hang.
///
/// Returns `Ok(true)` if the apply succeeded, `Ok(false)` if the
/// command ran but returned non-zero (e.g. user cancelled at the
/// sudo prompt).
pub fn spawn_sudo_apply(config_path: &Path) -> Result<bool> {
    let self_exe = std::env::current_exe().wrap_err("locating bougie binary")?;
    let use_sudo = std::env::var_os("BOUGIE_ETC_HOSTS_PATH").is_none();
    let mut cmd = if use_sudo {
        eprintln!(
            "bougie: manage_etc_hosts is on — running `sudo {} server hosts apply` to sync /etc/hosts",
            self_exe.display()
        );
        let mut c = std::process::Command::new("sudo");
        c.arg(&self_exe);
        c
    } else {
        eprintln!(
            "bougie: manage_etc_hosts is on (BOUGIE_ETC_HOSTS_PATH set; skipping sudo)"
        );
        std::process::Command::new(&self_exe)
    };
    let status = cmd
        .arg("server")
        .arg("hosts")
        .arg("apply")
        .arg("--config")
        .arg(config_path)
        .stdin(std::process::Stdio::inherit())
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit())
        .status()
        .wrap_err("spawning bougie hosts apply")?;
    Ok(status.success())
}

/// Print an actionable fallback when [`spawn_sudo_apply`] returned a
/// non-zero status. server.toml is already committed; the user just
/// needs to re-run apply themselves.
pub fn print_sudo_failure_hint(config_path: &Path) {
    eprintln!(
        "bougie: /etc/hosts was NOT updated (sudo failed or was cancelled).\n\
         The server.toml change is committed. To finish: sudo bougie server hosts apply --config {}",
        config_path.display()
    );
}

/// Returns every hostname (canonical + aliases) bougie should list in
/// `/etc/hosts`. Order matches the `[[host]]` entry order; aliases
/// follow their parent.
pub fn hostnames_for_etc_hosts(cfg: &ServerConfig) -> Vec<String> {
    let mut out = Vec::with_capacity(cfg.hosts.len());
    for host in &cfg.hosts {
        out.push(host.hostname.clone());
        for alias in &host.aliases {
            out.push(alias.hostname.clone());
        }
    }
    out
}

/// Replace the bougie sentinel block in `path` so it lists `hostnames`.
/// Atomic: writes to a sibling tempfile and renames over the target.
/// Preserves the original file's mode + everything outside the block.
///
/// Returns `Ok(true)` if the file was modified (or created), `Ok(false)`
/// when the existing block already matched.
pub fn rewrite_sentinel_block(path: &Path, hostnames: &[String]) -> Result<bool> {
    let existing = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) if e.kind() == io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(eyre::eyre!("reading {}: {e}", path.display())),
    };
    let new_content = splice_sentinel_block(&existing, hostnames);
    if new_content == existing {
        return Ok(false);
    }
    write_preserving_mode(path, &new_content)?;
    Ok(true)
}

/// Pure splice function: take the existing file contents and produce
/// the rewritten contents with the bougie sentinel block replaced.
///
/// - When no block exists, the new block is appended (with a leading
///   blank line if needed for tidiness).
/// - When a malformed half-block exists (BEGIN without END or vice
///   versa), the marker lines are removed and a fresh block is
///   appended. Anything else in the file is preserved.
/// - Empty `hostnames` removes the block entirely.
pub fn splice_sentinel_block(existing: &str, hostnames: &[String]) -> String {
    let lines: Vec<&str> = existing.lines().collect();
    let begin_idx = lines.iter().position(|l| l.trim() == BLOCK_BEGIN);
    let end_idx = lines.iter().position(|l| l.trim() == BLOCK_END);

    let mut out: Vec<String> = match (begin_idx, end_idx) {
        (Some(b), Some(e)) if b < e => {
            // Well-formed block — drop everything between b..=e.
            let mut v: Vec<String> = lines[..b].iter().map(|s| (*s).to_string()).collect();
            v.extend(lines[e + 1..].iter().map(|s| (*s).to_string()));
            v
        }
        _ => {
            // Either no block at all, or malformed. Strip any stray
            // sentinel markers (defensive) and keep the rest.
            lines
                .iter()
                .filter(|l| l.trim() != BLOCK_BEGIN && l.trim() != BLOCK_END)
                .map(|s| (*s).to_string())
                .collect()
        }
    };

    // Tidy trailing blank lines before we decide whether to append.
    while out.last().is_some_and(|l| l.trim().is_empty()) {
        out.pop();
    }

    if !hostnames.is_empty() {
        if !out.is_empty() {
            out.push(String::new());
        }
        out.push(BLOCK_BEGIN.to_string());
        for h in hostnames {
            out.push(format!("127.0.0.1 {h}"));
            out.push(format!("::1 {h}"));
        }
        out.push(BLOCK_END.to_string());
    }

    let mut s = out.join("\n");
    if !s.is_empty() {
        s.push('\n');
    }
    s
}

fn is_root() -> bool {
    rustix::process::geteuid().as_raw() == 0
}

fn write_preserving_mode(path: &Path, contents: &str) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let parent = path
        .parent()
        .ok_or_else(|| eyre::eyre!("{} has no parent directory", path.display()))?;
    let mode = std::fs::metadata(path)
        .map_or(0o644, |m| m.permissions().mode() & 0o7777);
    // Tempfile lives in the same directory so the final rename is
    // atomic (cross-fs renames aren't).
    let tmp = parent.join(format!(
        "{}.bougie.tmp",
        path.file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("etc-hosts")
    ));
    std::fs::write(&tmp, contents)
        .wrap_err_with(|| format!("writing {}", tmp.display()))?;
    std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(mode))
        .wrap_err_with(|| format!("chmod {} -> {mode:o}", tmp.display()))?;
    std::fs::rename(&tmp, path)
        .wrap_err_with(|| format!("renaming {} -> {}", tmp.display(), path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn splice_into_empty_file_appends_block() {
        let out = splice_sentinel_block("", &["a.bougie.test".into()]);
        assert!(out.contains(BLOCK_BEGIN));
        assert!(out.contains("127.0.0.1 a.bougie.test"));
        assert!(out.contains("::1 a.bougie.test"));
        assert!(out.contains(BLOCK_END));
        assert!(out.ends_with('\n'));
    }

    #[test]
    fn splice_preserves_lines_outside_block() {
        let input = "127.0.0.1 localhost\n::1 localhost\n";
        let out = splice_sentinel_block(input, &["a.bougie.test".into()]);
        assert!(out.starts_with("127.0.0.1 localhost\n"));
        assert!(out.contains("::1 localhost"));
        assert!(out.contains("a.bougie.test"));
    }

    #[test]
    fn splice_replaces_existing_block() {
        let input = format!(
            "127.0.0.1 localhost\n\n{BLOCK_BEGIN}\n127.0.0.1 old.bougie.test\n{BLOCK_END}\n"
        );
        let out = splice_sentinel_block(&input, &["new.bougie.test".into()]);
        assert!(out.contains("127.0.0.1 localhost"));
        assert!(out.contains("new.bougie.test"));
        assert!(!out.contains("old.bougie.test"));
    }

    #[test]
    fn splice_with_empty_hostnames_drops_block() {
        let input = format!(
            "127.0.0.1 localhost\n\n{BLOCK_BEGIN}\n127.0.0.1 a.bougie.test\n{BLOCK_END}\n"
        );
        let out = splice_sentinel_block(&input, &[]);
        assert!(out.contains("127.0.0.1 localhost"));
        assert!(!out.contains(BLOCK_BEGIN));
        assert!(!out.contains("a.bougie.test"));
    }

    #[test]
    fn splice_recovers_from_missing_end_marker() {
        // BEGIN without END is corruption (from a kill -9 mid-write).
        // The recovery rule: strip stray markers, append fresh block.
        let input = format!(
            "127.0.0.1 localhost\n\n{BLOCK_BEGIN}\n127.0.0.1 orphaned.bougie.test\n"
        );
        let out = splice_sentinel_block(&input, &["fresh.bougie.test".into()]);
        // Stray BEGIN is removed; the orphaned line is preserved
        // (we don't know which one was bougie's vs. the user's), but
        // the new sentinel block is appended cleanly.
        assert_eq!(out.matches(BLOCK_BEGIN).count(), 1);
        assert_eq!(out.matches(BLOCK_END).count(), 1);
        assert!(out.contains("fresh.bougie.test"));
    }

    #[test]
    fn rewrite_idempotent_when_already_matching() {
        let td = TempDir::new().unwrap();
        let path = td.path().join("hosts");
        std::fs::write(&path, "127.0.0.1 localhost\n").unwrap();
        let changed = rewrite_sentinel_block(&path, &["a.bougie.test".into()]).unwrap();
        assert!(changed);
        let changed = rewrite_sentinel_block(&path, &["a.bougie.test".into()]).unwrap();
        assert!(!changed, "second apply should be a no-op");
    }

    #[test]
    fn rewrite_preserves_file_mode() {
        use std::os::unix::fs::PermissionsExt;
        let td = TempDir::new().unwrap();
        let path = td.path().join("hosts");
        std::fs::write(&path, "127.0.0.1 localhost\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        rewrite_sentinel_block(&path, &["x.bougie.test".into()]).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o7777;
        assert_eq!(mode, 0o600);
    }

    #[test]
    fn hostnames_for_etc_hosts_includes_aliases() {
        use crate::server::config::{HostAlias, HostBlock, ServerSection};
        let cfg = ServerConfig {
            server: ServerSection::default(),
            hosts: vec![HostBlock {
                hostname: "main.bougie.test".into(),
                project: PathBuf::from("/p"),
                root: ".".into(),
                index: Vec::new(),
                try_files: Vec::new(),
                aliases: vec![HostAlias { hostname: "alias.bougie.test".into() }],
                rewrites: Vec::new(),
            }],
        };
        let hs = hostnames_for_etc_hosts(&cfg);
        assert_eq!(hs, vec!["main.bougie.test", "alias.bougie.test"]);
    }
}
