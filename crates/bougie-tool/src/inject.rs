//! `bougie tool inject <vendor/name> <extras...>` and the matching
//! `uninject` verb.
//!
//! Inject is the post-install variant of `--with`. It mutates an
//! installed tool's `composer.json` (regenerated from the receipt
//! state + the new extras), re-runs the lock resolver and the
//! native installer, and persists the updated receipt. Uninject is
//! the symmetric removal path.
//!
//! Like `install.rs`, the heavy work (composer resolution, extension
//! install) happens via callbacks the bougie binary supplies via
//! [`InstallContext`](crate::install::InstallContext) — `bougie-tool`
//! itself stays free of the installer's transitive dep graph.
//!
//! Phase 2 PR5 scope: composer-package extras only. Extension
//! injection follows the same code path here once the conf.d wiring
//! lands; the classifier callback gates it.

use crate::classify::{Classified, classify};
use crate::install::{InstallContext, write_composer_json_for_receipt};
use crate::receipt::{self, ToolReceipt};
use bougie_composer_resolver::{InstallOptions, install_from_lock};
use bougie_fs::lock::ExclusiveGuard;
use bougie_paths::Paths;
use eyre::{Result, WrapErr, bail};
use std::path::PathBuf;
use std::time::Duration;

const LOCK_TIMEOUT: Duration = Duration::from_mins(1);

#[derive(Debug, Clone)]
pub struct InjectOutcome {
    pub package: String,
    pub tool_dir: PathBuf,
    pub added_composer: Vec<String>,
    pub added_extensions: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct UninjectOutcome {
    pub package: String,
    pub tool_dir: PathBuf,
    pub removed_composer: Vec<String>,
    pub removed_extensions: Vec<String>,
}

pub fn inject(
    ctx: &InstallContext<'_>,
    package: &str,
    extras: &[String],
) -> Result<InjectOutcome> {
    let tool_dir = ctx.paths.tool_dir(package);
    if !tool_dir.exists() {
        bail!("tool `{package}` is not installed");
    }
    let guard = ExclusiveGuard::acquire(&tool_dir.join(".lock"), LOCK_TIMEOUT)
        .wrap_err_with(|| {
            format!("acquiring lock on {} (is another `bougie tool` running?)", tool_dir.display())
        })?;

    let mut receipt = receipt::read(&tool_dir.join("receipt.toml"))?;
    let php = php_from(&receipt);

    let mut added_composer: Vec<String> = Vec::new();
    let mut added_extensions: Vec<String> = Vec::new();
    for name in extras {
        match classify(name, &php, ctx.classifier)? {
            Classified::ComposerPackage(p) => {
                let key = composer_name(&p);
                if receipt.with.iter().any(|w| composer_name(w) == key) {
                    bail!("`{p}` is already injected into `{package}`");
                }
                added_composer.push(p);
            }
            Classified::Extension(e) => {
                if receipt.extensions.iter().any(|x| x.name == e) {
                    bail!("extension `{e}` is already injected into `{package}`");
                }
                added_extensions.push(e);
            }
        }
    }
    if added_composer.is_empty() && added_extensions.is_empty() {
        // Nothing to do — but the user supplied something, so this is
        // worth surfacing as an explicit no-op.
        bail!("no extras to inject (every `--with` is already present)");
    }

    receipt.with.extend(added_composer.iter().cloned());
    if !added_composer.is_empty() {
        regenerate_and_install(ctx.paths, &tool_dir, &receipt)?;
        (ctx.resolve_lock)(ctx.paths, &tool_dir)
            .wrap_err("resolving composer.lock for tool")?;
        install_from_lock(ctx.paths, &tool_dir, InstallOptions { no_dev: true }, None)
            .wrap_err("installing tool dependencies")?;
    }

    // Extension installation: front the bougie binary's callback,
    // which installs the .so into the shared store and writes the
    // matching conf.d fragment under `$TOOL_DIR/conf.d/`. The
    // returned ini path lands in the receipt for uninject /
    // uninstall.
    let conf_d = tool_dir.join("conf.d");
    std::fs::create_dir_all(&conf_d)
        .wrap_err_with(|| format!("creating {}", conf_d.display()))?;
    for ext in &added_extensions {
        let ini_path = (ctx.ext_installer)(ctx.paths, ext, &php, &conf_d)
            .wrap_err_with(|| format!("installing extension `{ext}`"))?;
        receipt.extensions.push(crate::receipt::ToolExtension {
            name: ext.clone(),
            ini_path,
        });
    }

    receipt::write(&tool_dir.join("receipt.toml"), &receipt)?;
    drop(guard);

    Ok(InjectOutcome {
        package: package.to_string(),
        tool_dir,
        added_composer,
        added_extensions,
    })
}

pub fn uninject(
    ctx: &InstallContext<'_>,
    package: &str,
    extras: &[String],
) -> Result<UninjectOutcome> {
    let tool_dir = ctx.paths.tool_dir(package);
    if !tool_dir.exists() {
        bail!("tool `{package}` is not installed");
    }
    let guard = ExclusiveGuard::acquire(&tool_dir.join(".lock"), LOCK_TIMEOUT)
        .wrap_err_with(|| {
            format!("acquiring lock on {} (is another `bougie tool` running?)", tool_dir.display())
        })?;

    let mut receipt = receipt::read(&tool_dir.join("receipt.toml"))?;
    let php = php_from(&receipt);

    let mut removed_composer: Vec<String> = Vec::new();
    let mut removed_extension_inis: Vec<std::path::PathBuf> = Vec::new();
    let mut removed_extensions: Vec<String> = Vec::new();
    for name in extras {
        match classify(name, &php, ctx.classifier)? {
            Classified::ComposerPackage(p) => {
                let key = composer_name(&p);
                let Some(idx) = receipt.with.iter().position(|w| composer_name(w) == key) else {
                    bail!("`{p}` is not currently injected into `{package}`");
                };
                removed_composer.push(receipt.with.remove(idx));
            }
            Classified::Extension(e) => {
                let Some(idx) = receipt.extensions.iter().position(|x| x.name == e) else {
                    bail!("extension `{e}` is not currently injected into `{package}`");
                };
                let entry = receipt.extensions.remove(idx);
                removed_extension_inis.push(entry.ini_path);
                removed_extensions.push(entry.name);
            }
        }
    }
    if removed_composer.is_empty() && removed_extensions.is_empty() {
        bail!("no extras to uninject");
    }

    // Delete conf.d fragments first — the `.so` itself stays in the
    // shared store (content-addressed; other tools or projects may
    // still need it).
    for ini in &removed_extension_inis {
        match std::fs::remove_file(ini) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => bail!("removing {}: {e}", ini.display()),
        }
    }

    if !removed_composer.is_empty() {
        regenerate_and_install(ctx.paths, &tool_dir, &receipt)?;
        (ctx.resolve_lock)(ctx.paths, &tool_dir)
            .wrap_err("resolving composer.lock for tool")?;
        install_from_lock(ctx.paths, &tool_dir, InstallOptions { no_dev: true }, None)
            .wrap_err("installing tool dependencies after uninject")?;
    }
    // Extension uninstall (delete conf.d fragment) lands in the
    // extension-wiring follow-up.

    receipt::write(&tool_dir.join("receipt.toml"), &receipt)?;
    drop(guard);

    Ok(UninjectOutcome {
        package: package.to_string(),
        tool_dir,
        removed_composer,
        removed_extensions,
    })
}

/// Regenerate `composer.json` from the receipt — useful when
/// `receipt.with` has been mutated by inject / uninject and we want
/// the on-disk composer state to mirror it before re-resolving.
fn regenerate_and_install(
    _paths: &Paths,
    tool_dir: &std::path::Path,
    receipt: &ToolReceipt,
) -> Result<()> {
    write_composer_json_for_receipt(tool_dir, receipt)
}

/// Strip an `@<constraint>` suffix so two strings naming the same
/// package collide even if one specifies a constraint and the other
/// doesn't.
fn composer_name(s: &str) -> &str {
    s.split_once('@').map_or(s, |(n, _)| n)
}

/// Reconstruct a [`PhpChoice`](crate::resolve::PhpChoice) from a
/// receipt. Used so inject / uninject can pass the right PHP context
/// to the classifier without re-running PHP selection.
fn php_from(receipt: &ToolReceipt) -> crate::resolve::PhpChoice {
    crate::resolve::PhpChoice {
        version: receipt.php_version.clone(),
        flavor: receipt.php_flavor.clone(),
        bin: receipt.php_resolved_path.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn composer_name_strips_constraint() {
        assert_eq!(composer_name("phpstan/phpstan"), "phpstan/phpstan");
        assert_eq!(composer_name("phpstan/phpstan@^1.5"), "phpstan/phpstan");
    }
}
