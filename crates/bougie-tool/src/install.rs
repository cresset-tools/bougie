//! `bougie tool install <vendor/name>[@<constraint>]` core logic.
//!
//! Phase 1 scope: single-bin packages only, no `--with`, no `--php`.
//! PHP is the highest installed NTS interpreter; constraint defaults
//! to `*` (latest stable matching the tool's PHP).
//!
//! The install pipeline:
//!
//! 1. Ensure the stable `$BOUGIE_LOCAL/bin/bougie` symlink points at
//!    the current binary — wrappers shebang into this path, not into
//!    the unpredictable `current_exe()`.
//! 2. Acquire `$TOOL_DIR/.lock` so concurrent installs of the same
//!    package can't race on the receipt.
//! 3. Write `$TOOL_DIR/composer.json` requiring just the user's
//!    package, with `allow-plugins: false` (no plugins in Phase 1).
//! 4. Resolve + install via bougie's native composer resolver.
//! 5. Discover bin entries from the installed package's
//!    `vendor/<pkg>/composer.json` (Phase 1 errors on multi-bin).
//! 6. Emit the Unix wrapper at `$TOOL_DIR/bin/<binname>` and the
//!    PATH symlink at `$BOUGIE_TOOL_BIN_DIR/<binname>` — collision
//!    on the latter is a hard error unless `force` is true.
//! 7. Write the receipt.

use crate::receipt::{ToolEntrypoint, ToolReceipt};
use crate::request::ToolRequest;
use crate::{resolve, wrapper};
use bougie_composer_resolver::{InstallOptions, install_from_lock};
use bougie_fs::lock::ExclusiveGuard;
use bougie_paths::Paths;
use eyre::{Result, WrapErr, bail};
use std::path::{Path, PathBuf};
use std::fmt::Write as _;
use std::time::Duration;

/// Composer-the-tool version recorded in the receipt. Not used at
/// install time — Phase 1's install runs through the native resolver,
/// not composer.phar — but Phase 2's `inject` flow uses it to pick the
/// phar version when running composer plugins.
const RECORDED_COMPOSER_VERSION: &str = "2.8.12";

/// Default constraint when the user passes no `@<constraint>`. Composer
/// itself treats `*` as "any version", which combined with bougie's
/// resolver preferring stable releases gives "latest stable matching
/// the tool's PHP" — the intuitive default for tool install.
const DEFAULT_CONSTRAINT: &str = "*";

/// Matches the per-tool composer lock timeout in
/// `bougie-composer/src/mod.rs` — the same "are we waiting on another
/// bougie process or a stuck one?" threshold.
const LOCK_TIMEOUT: Duration = Duration::from_mins(1);

#[derive(Debug, Clone)]
pub struct InstallOutcome {
    pub package: String,
    pub php_version: String,
    pub tool_dir: PathBuf,
    pub installed_bins: Vec<PathBuf>,
}

/// Inject a `resolve_and_write_lock`-shaped callback so the bougie
/// binary can supply its existing implementation (lives in
/// `bougie/src/commands/composer_update.rs`) without `bougie-tool`
/// having to depend on `bougie` itself.
pub type LockResolver = dyn Fn(&Paths, &Path) -> Result<()> + Send + Sync;

pub fn install(
    paths: &Paths,
    request: &ToolRequest,
    force: bool,
    resolve_lock: &LockResolver,
) -> Result<InstallOutcome> {
    ensure_stable_bougie_symlink(paths)
        .wrap_err("setting up stable bougie symlink")?;

    let php = resolve::pick_php(paths)?;
    let constraint = request
        .constraint
        .clone()
        .unwrap_or_else(|| DEFAULT_CONSTRAINT.to_string());
    let package = request.package();
    let tool_dir = paths.tool_dir(&package);

    std::fs::create_dir_all(&tool_dir)
        .wrap_err_with(|| format!("creating {}", tool_dir.display()))?;

    let _guard = ExclusiveGuard::acquire(&tool_dir.join(".lock"), LOCK_TIMEOUT)
        .wrap_err_with(|| {
            format!(
                "acquiring lock on {} (is another `bougie tool` running?)",
                tool_dir.display()
            )
        })?;

    write_composer_json(&tool_dir, &package, &constraint)?;
    resolve_lock(paths, &tool_dir).wrap_err("resolving composer.lock for tool")?;
    install_from_lock(paths, &tool_dir, InstallOptions { no_dev: true })
        .wrap_err("installing tool dependencies")?;

    let bin_entries = read_bin_entries(&tool_dir, &package)?;
    if bin_entries.is_empty() {
        bail!(
            "package `{package}` declares no `bin` entries — there is nothing to install on PATH"
        );
    }
    if bin_entries.len() > 1 {
        bail!(
            "package `{package}` exposes {n} entry points; multi-bin support lands in Phase 2",
            n = bin_entries.len()
        );
    }
    let bin_entry = &bin_entries[0];
    let bin_name = bin_filename(bin_entry);
    if bin_name.is_empty() {
        bail!("could not derive a bin name from `{bin_entry}`");
    }

    let conf_d = tool_dir.join("conf.d");
    std::fs::create_dir_all(&conf_d)
        .wrap_err_with(|| format!("creating {}", conf_d.display()))?;

    let wrapper_path = tool_dir.join("bin").join(&bin_name);
    let wrapper_text = wrapper::render_unix(
        &paths.bin().join("bougie"),
        &bin_name,
        bin_entry,
    );
    wrapper::write_executable(&wrapper_path, &wrapper_text)?;

    let install_path = paths.tool_bin_dir().join(&bin_name);
    place_symlink(&wrapper_path, &install_path, force)?;

    let receipt = ToolReceipt {
        package: package.clone(),
        constraint,
        php_version: php.version.clone(),
        php_flavor: php.flavor.clone(),
        composer_version: RECORDED_COMPOSER_VERSION.into(),
        with: Vec::new(),
        php_resolved_path: php.bin.clone(),
        entrypoints: vec![ToolEntrypoint {
            name: bin_name,
            install_path: install_path.clone(),
            from: package.clone(),
        }],
    };
    crate::receipt::write(&tool_dir.join("receipt.toml"), &receipt)?;

    Ok(InstallOutcome {
        package,
        php_version: php.version,
        tool_dir,
        installed_bins: vec![install_path],
    })
}

fn write_composer_json(tool_dir: &Path, package: &str, constraint: &str) -> Result<()> {
    // Minimal composer.json — just the tool's own dep. `allow-plugins:
    // false` is correct for Phase 1: no plugins until `inject` lands.
    let body = format!(
        "{{\n  \"require\": {{\n    {pkg}: {ver}\n  }},\n  \"config\": {{\n    \"allow-plugins\": false\n  }}\n}}\n",
        pkg = json_string(package),
        ver = json_string(constraint),
    );
    let path = tool_dir.join("composer.json");
    std::fs::write(&path, body)
        .wrap_err_with(|| format!("writing {}", path.display()))?;
    Ok(())
}

fn read_bin_entries(tool_dir: &Path, package: &str) -> Result<Vec<String>> {
    let package_json = tool_dir
        .join("vendor")
        .join(package)
        .join("composer.json");
    let bytes = std::fs::read(&package_json).wrap_err_with(|| {
        format!(
            "reading {} (composer install did not place the package)",
            package_json.display()
        )
    })?;
    let value: serde_json::Value = serde_json::from_slice(&bytes)
        .wrap_err_with(|| format!("parsing {}", package_json.display()))?;
    let bin = value.get("bin");
    let mut entries = Vec::new();
    match bin {
        None => {}
        Some(serde_json::Value::String(s)) => {
            entries.push(format!("{package}/{s}"));
        }
        Some(serde_json::Value::Array(arr)) => {
            for v in arr {
                let Some(s) = v.as_str() else {
                    bail!("non-string entry in {}'s `bin` array", package);
                };
                entries.push(format!("{package}/{s}"));
            }
        }
        Some(other) => bail!(
            "unexpected `bin` shape in {}'s composer.json: {:?}",
            package,
            other
        ),
    }
    Ok(entries)
}

fn bin_filename(vendor_relative: &str) -> String {
    Path::new(vendor_relative)
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// Make sure `$BOUGIE_LOCAL/bin/bougie` exists and points at the
/// current bougie binary. Tool wrappers shebang into this path, so
/// the indirection is what lets self-update relocate the binary
/// without breaking installed tools.
///
/// Phase 1 expedient: long-term this belongs in `bougie self install`
/// once that's not a stub. Idempotent — does nothing once the symlink
/// already resolves correctly.
fn ensure_stable_bougie_symlink(paths: &Paths) -> Result<()> {
    let stable = paths.bin().join("bougie");
    let target = std::env::current_exe().wrap_err("locating current bougie executable")?;
    if let Some(parent) = stable.parent() {
        std::fs::create_dir_all(parent)
            .wrap_err_with(|| format!("creating {}", parent.display()))?;
    }
    #[cfg(unix)]
    {
        if let Ok(existing) = std::fs::read_link(&stable) {
            if existing == target {
                return Ok(());
            }
            std::fs::remove_file(&stable)
                .wrap_err_with(|| format!("removing stale symlink {}", stable.display()))?;
        } else if stable.exists() {
            // Concrete file at that path — refuse to clobber it.
            bail!(
                "{} exists but isn't a symlink; remove it before re-running",
                stable.display()
            );
        }
        std::os::unix::fs::symlink(&target, &stable)
            .wrap_err_with(|| format!("symlink {} → {}", stable.display(), target.display()))?;
    }
    #[cfg(not(unix))]
    {
        // Windows lands in Phase 4; this path is unreachable from the
        // Phase 1 CLI but the compile must succeed everywhere.
        let _ = target;
        let _ = stable;
    }
    Ok(())
}

#[cfg(unix)]
fn place_symlink(target: &Path, link: &Path, force: bool) -> Result<()> {
    if let Some(parent) = link.parent() {
        std::fs::create_dir_all(parent)
            .wrap_err_with(|| format!("creating {}", parent.display()))?;
    }
    // `try_exists` follows symlinks; we want to know about *any*
    // entry, including broken links. `symlink_metadata` is the right
    // probe.
    match std::fs::symlink_metadata(link) {
        Ok(_) => {
            if !force {
                bail!(
                    "executable already exists at {} (use --force to overwrite)",
                    link.display()
                );
            }
            std::fs::remove_file(link)
                .wrap_err_with(|| format!("removing existing {}", link.display()))?;
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => {
            return Err(eyre::eyre!(
                "checking {}: {e}",
                link.display()
            ));
        }
    }
    std::os::unix::fs::symlink(target, link)
        .wrap_err_with(|| format!("symlink {} → {}", link.display(), target.display()))?;
    Ok(())
}

#[cfg(not(unix))]
fn place_symlink(_target: &Path, _link: &Path, _force: bool) -> Result<()> {
    eyre::bail!("`bougie tool install` is Unix-only in Phase 1")
}

/// Minimal JSON string escaping. We control the inputs (a parsed
/// `<vendor>/<name>` and a constraint that came from clap), but better
/// to escape than to assume Composer constraint strings can never
/// contain a `"`.
fn json_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_string_escapes_quote_and_backslash() {
        assert_eq!(json_string(r#"a"b\c"#), r#""a\"b\\c""#);
    }

    #[test]
    fn bin_filename_strips_directories() {
        assert_eq!(
            bin_filename("phpstan/phpstan/bin/phpstan"),
            "phpstan"
        );
        assert_eq!(bin_filename("vendor/pkg/bin/tool.php"), "tool.php");
        assert_eq!(bin_filename(""), "");
    }

    #[test]
    fn write_composer_json_renders_expected_shape() {
        let td = tempfile::TempDir::new().unwrap();
        write_composer_json(td.path(), "phpstan/phpstan", "^1.10").unwrap();
        let text = std::fs::read_to_string(td.path().join("composer.json")).unwrap();
        assert!(text.contains(r#""phpstan/phpstan": "^1.10""#), "{text}");
        assert!(text.contains(r#""allow-plugins": false"#), "{text}");
    }

    #[test]
    fn read_bin_entries_handles_string_form() {
        let td = tempfile::TempDir::new().unwrap();
        let pkg_dir = td.path().join("vendor").join("phpstan").join("phpstan");
        std::fs::create_dir_all(&pkg_dir).unwrap();
        std::fs::write(
            pkg_dir.join("composer.json"),
            r#"{"name":"phpstan/phpstan","bin":"bin/phpstan"}"#,
        )
        .unwrap();
        let entries = read_bin_entries(td.path(), "phpstan/phpstan").unwrap();
        assert_eq!(entries, vec!["phpstan/phpstan/bin/phpstan".to_string()]);
    }

    #[test]
    fn read_bin_entries_handles_array_form() {
        let td = tempfile::TempDir::new().unwrap();
        let pkg_dir = td.path().join("vendor").join("vimeo").join("psalm");
        std::fs::create_dir_all(&pkg_dir).unwrap();
        std::fs::write(
            pkg_dir.join("composer.json"),
            r#"{"name":"vimeo/psalm","bin":["psalm","psalter"]}"#,
        )
        .unwrap();
        let entries = read_bin_entries(td.path(), "vimeo/psalm").unwrap();
        assert_eq!(
            entries,
            vec!["vimeo/psalm/psalm".to_string(), "vimeo/psalm/psalter".to_string()]
        );
    }

    #[test]
    fn read_bin_entries_returns_empty_when_field_absent() {
        let td = tempfile::TempDir::new().unwrap();
        let pkg_dir = td.path().join("vendor").join("v").join("p");
        std::fs::create_dir_all(&pkg_dir).unwrap();
        std::fs::write(pkg_dir.join("composer.json"), r#"{"name":"v/p"}"#).unwrap();
        let entries = read_bin_entries(td.path(), "v/p").unwrap();
        assert!(entries.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn place_symlink_collides_without_force() {
        let td = tempfile::TempDir::new().unwrap();
        let target = td.path().join("real");
        std::fs::write(&target, "real").unwrap();
        let link = td.path().join("link");
        std::fs::write(&link, "preexisting").unwrap();
        let err = place_symlink(&target, &link, false).unwrap_err();
        assert!(err.to_string().contains("already exists"), "{err}");
    }

    #[cfg(unix)]
    #[test]
    fn place_symlink_force_overwrites() {
        let td = tempfile::TempDir::new().unwrap();
        let target = td.path().join("real");
        std::fs::write(&target, "real").unwrap();
        let link = td.path().join("link");
        std::fs::write(&link, "preexisting").unwrap();
        place_symlink(&target, &link, true).unwrap();
        let resolved = std::fs::read_link(&link).unwrap();
        assert_eq!(resolved, target);
    }
}
