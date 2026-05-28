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

use crate::classify::{Classified, ExtensionClassifier, classify};
use crate::receipt::{ToolEntrypoint, ToolExtension, ToolReceipt};
use crate::request::ToolRequest;
use crate::resolve::PhpChoice;
use crate::{resolve, wrapper};
use bougie_composer_resolver::{InstallOptions, install_from_lock};
use bougie_fs::lock::ExclusiveGuard;
use bougie_paths::Paths;
use eyre::{Result, WrapErr, bail};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
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
    pub installed_extensions: Vec<String>,
}

/// Inject a `resolve_and_write_lock`-shaped callback so the bougie
/// binary can supply its existing implementation (lives in
/// `bougie/src/commands/composer_update.rs`) without `bougie-tool`
/// having to depend on `bougie` itself.
pub type LockResolver = dyn Fn(&Paths, &Path) -> Result<()> + Send + Sync;

/// Ensure a PHP extension is installed in the shared store for a
/// specific `(version, flavor)`, fronting
/// `bougie_installer::install::install_extension`. Doesn't touch
/// `$TOOL_DIR/conf.d/` — that's bougie-tool's job.
pub type ExtInstaller = dyn Fn(&Paths, &str, &PhpChoice) -> Result<()> + Send + Sync;

/// Bundle of paths + callbacks the install / inject / upgrade flows
/// all need. Saves passing seven near-identical arguments per call.
#[allow(missing_debug_implementations, reason = "fields are non-Debug trait objects")]
pub struct InstallContext<'a> {
    pub paths: &'a Paths,
    pub resolve_lock: &'a LockResolver,
    pub php_installer: &'a resolve::PhpInstaller,
    pub classifier: &'a ExtensionClassifier,
    pub ext_installer: &'a ExtInstaller,
}

pub fn install(
    ctx: &InstallContext<'_>,
    request: &ToolRequest,
    php_spec: Option<&str>,
    with: &[String],
    force: bool,
) -> Result<InstallOutcome> {
    let paths = ctx.paths;
    ensure_stable_bougie_symlink(paths)
        .wrap_err("setting up stable bougie symlink")?;

    let php = resolve::pick_php(paths, php_spec, ctx.php_installer)?;
    let constraint = request
        .constraint
        .clone()
        .unwrap_or_else(|| DEFAULT_CONSTRAINT.to_string());
    let package = request.package();
    let tool_dir = paths.tool_dir(&package);

    // Classify every --with up front so a bad name fails before we
    // touch the tool dir.
    let mut composer_extras: Vec<String> = Vec::new();
    let mut extension_extras: Vec<String> = Vec::new();
    for name in with {
        match classify(name, ctx.classifier)? {
            Classified::ComposerPackage(p) => composer_extras.push(p),
            Classified::Extension(e) => extension_extras.push(e),
        }
    }

    std::fs::create_dir_all(&tool_dir)
        .wrap_err_with(|| format!("creating {}", tool_dir.display()))?;

    let _guard = ExclusiveGuard::acquire(&tool_dir.join(".lock"), LOCK_TIMEOUT)
        .wrap_err_with(|| {
            format!(
                "acquiring lock on {} (is another `bougie tool` running?)",
                tool_dir.display()
            )
        })?;

    write_composer_json(&tool_dir, &package, &constraint, &composer_extras)?;
    (ctx.resolve_lock)(paths, &tool_dir).wrap_err("resolving composer.lock for tool")?;
    install_from_lock(paths, &tool_dir, InstallOptions { no_dev: true })
        .wrap_err("installing tool dependencies")?;

    let conf_d = tool_dir.join("conf.d");
    std::fs::create_dir_all(&conf_d)
        .wrap_err_with(|| format!("creating {}", conf_d.display()))?;

    let (entrypoints, installed_bins) = emit_bins(paths, &tool_dir, &package, force)?;

    // Install + enable extension extras now that the tool dir exists.
    // Errors are propagated — a failed extension install rolls back at
    // the call site if needed; the tool dir + bins stay (the user has
    // a partially-injected tool, which they can recover from with
    // `bougie tool uninstall` + retry).
    let mut tool_extensions: Vec<ToolExtension> = Vec::with_capacity(extension_extras.len());
    for ext in &extension_extras {
        (ctx.ext_installer)(paths, ext, &php)
            .wrap_err_with(|| format!("installing extension `{ext}` for tool"))?;
        let ini_path = write_ext_fragment(&conf_d, ext)?;
        tool_extensions.push(ToolExtension {
            name: ext.clone(),
            ini_path,
        });
    }

    let receipt = ToolReceipt {
        package: package.clone(),
        constraint,
        php_version: php.version.clone(),
        php_flavor: php.flavor.clone(),
        composer_version: RECORDED_COMPOSER_VERSION.into(),
        with: composer_extras,
        php_resolved_path: php.bin.clone(),
        entrypoints,
        extensions: tool_extensions,
    };
    crate::receipt::write(&tool_dir.join("receipt.toml"), &receipt)?;

    Ok(InstallOutcome {
        package,
        php_version: php.version,
        tool_dir,
        installed_bins,
        installed_extensions: extension_extras,
    })
}

/// Write a `20-<name>.ini` fragment under `$TOOL_DIR/conf.d/`. The
/// `20-` prefix matches the project-side `bougie ext add` convention:
/// the install's own bundled extensions live in `00-…`, project /
/// tool overrides in `20-…`.
fn write_ext_fragment(conf_d: &Path, ext: &str) -> Result<PathBuf> {
    let path = conf_d.join(format!("20-{ext}.ini"));
    let body = format!("extension={ext}\n");
    std::fs::write(&path, body)
        .wrap_err_with(|| format!("writing conf.d fragment {}", path.display()))?;
    Ok(path)
}

/// Read bin entries for `package` from the just-installed vendor
/// tree, validate names + pre-flight bin-dir collisions, then emit
/// every wrapper + symlink. Returns the receipt's `entrypoints`
/// alongside the absolute bin paths for the install summary.
pub fn emit_bins(
    paths: &Paths,
    tool_dir: &Path,
    package: &str,
    force: bool,
) -> Result<(Vec<ToolEntrypoint>, Vec<PathBuf>)> {
    let bin_entries = read_bin_entries(tool_dir, package)?;
    if bin_entries.is_empty() {
        bail!(
            "package `{package}` declares no `bin` entries — there is nothing to install on PATH"
        );
    }

    let mut bins: Vec<(String, String)> = Vec::with_capacity(bin_entries.len());
    for entry in &bin_entries {
        let name = bin_filename(entry);
        if name.is_empty() {
            bail!("could not derive a bin name from `{entry}`");
        }
        bins.push((name, entry.clone()));
    }

    let tool_bin_dir = paths.tool_bin_dir();
    if !force {
        for (name, _) in &bins {
            let path = tool_bin_dir.join(name);
            if std::fs::symlink_metadata(&path).is_ok() {
                bail!(
                    "executable already exists at {} (use --force to overwrite)",
                    path.display()
                );
            }
        }
    }

    let stable_bougie = paths.bin().join("bougie");
    let mut entrypoints: Vec<ToolEntrypoint> = Vec::with_capacity(bins.len());
    let mut installed_bins: Vec<PathBuf> = Vec::with_capacity(bins.len());
    for (name, vendor_relative) in &bins {
        let wrapper_path = tool_dir.join("bin").join(name);
        let wrapper_text = wrapper::render_unix(&stable_bougie, name, vendor_relative);
        wrapper::write_executable(&wrapper_path, &wrapper_text)?;

        let install_path = tool_bin_dir.join(name);
        place_symlink(&wrapper_path, &install_path, force)?;

        entrypoints.push(ToolEntrypoint {
            name: name.clone(),
            install_path: install_path.clone(),
            from: package.to_string(),
        });
        installed_bins.push(install_path);
    }
    Ok((entrypoints, installed_bins))
}

/// Regenerate `composer.json` from a receipt's current state. Used by
/// `inject` / `uninject` after they mutate `receipt.with`.
pub fn write_composer_json_for_receipt(
    tool_dir: &Path,
    receipt: &ToolReceipt,
) -> Result<()> {
    write_composer_json(
        tool_dir,
        &receipt.package,
        &receipt.constraint,
        &receipt.with,
    )
}

fn write_composer_json(
    tool_dir: &Path,
    package: &str,
    constraint: &str,
    extras: &[String],
) -> Result<()> {
    // `extras` may carry an `@<constraint>` suffix (the user-typed
    // `vendor/name@^1.5`). Split each into (name, constraint or "*").
    // Composer-the-binary doesn't accept `@` in require keys.
    let mut requires: Vec<(String, String)> = vec![(package.to_string(), constraint.to_string())];
    for raw in extras {
        let (name, ver) = match raw.split_once('@') {
            Some((n, v)) if !v.is_empty() => (n.to_string(), v.to_string()),
            _ => (raw.clone(), DEFAULT_CONSTRAINT.to_string()),
        };
        requires.push((name, ver));
    }

    let mut require_block = String::new();
    for (i, (n, v)) in requires.iter().enumerate() {
        if i > 0 {
            require_block.push(',');
            require_block.push('\n');
        }
        require_block.push_str("    ");
        require_block.push_str(&json_string(n));
        require_block.push_str(": ");
        require_block.push_str(&json_string(v));
    }

    // `allow-plugins: false` for every Phase 2 tool. The native
    // resolver doesn't execute plugins, so leaving them disabled
    // matches what actually happens at install time; a future
    // phar-execution path will widen this with a narrow per-plugin
    // map populated from the resolved lockfile.
    let body = format!(
        "{{\n  \"require\": {{\n{require_block}\n  }},\n  \
         \"config\": {{\n    \"allow-plugins\": false\n  }}\n}}\n",
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
        write_composer_json(td.path(), "phpstan/phpstan", "^1.10", &[]).unwrap();
        let text = std::fs::read_to_string(td.path().join("composer.json")).unwrap();
        assert!(text.contains(r#""phpstan/phpstan": "^1.10""#), "{text}");
        assert!(text.contains(r#""allow-plugins": false"#), "{text}");
    }

    #[test]
    fn write_composer_json_includes_extras_with_split_constraints() {
        let td = tempfile::TempDir::new().unwrap();
        write_composer_json(
            td.path(),
            "phpstan/phpstan",
            "^1.10",
            &[
                "phpstan/phpstan-strict-rules@^1.5".to_string(),
                "slevomat/coding-standard".to_string(),
            ],
        )
        .unwrap();
        let text = std::fs::read_to_string(td.path().join("composer.json")).unwrap();
        assert!(text.contains(r#""phpstan/phpstan-strict-rules": "^1.5""#), "{text}");
        // bare extras default to `*`
        assert!(text.contains(r#""slevomat/coding-standard": "*""#), "{text}");
    }

    #[test]
    fn write_ext_fragment_writes_load_directive() {
        let td = tempfile::TempDir::new().unwrap();
        let path = write_ext_fragment(td.path(), "intl").unwrap();
        let body = std::fs::read_to_string(&path).unwrap();
        assert_eq!(body, "extension=intl\n");
        assert!(path.ends_with("20-intl.ini"));
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
