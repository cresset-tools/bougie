//! Framework-specific fixups that let a project generate correct absolute URLs
//! when it's served on a public `*.bougie.show` share host (or the local
//! `*.bougie.run` dev host) instead of the host baked into its stored config.
//!
//! Most frameworks (Laravel, Symfony, plain PHP) already build URLs from the
//! request (`Host` + `X-Forwarded-Proto`), so they need nothing. **Magento** is
//! the exception: it builds absolute URLs from the configured `base_url`, not
//! the request — so a share of a Magento store would emit `bougie.run` links on
//! a `bougie.show` page. We fix that by installing a tiny Magento module
//! (`Bougie_Share`) whose plugin re-hosts `Store::getBaseUrl` onto the request,
//! gated to bougie-served hostnames.
//!
//! Why a module and not an `env.php`/DB write (the two approaches we rejected):
//! Magento's `ConfigChangeDetector` hashes the deployment config's
//! `system`/`scopes`/`themes` sections, so writing `base_url` into `env.php`'s
//! `system` block throws "the configuration file has changed" (HTTP 500); and
//! the `{{base_url}}` placeholder is *resolved and frozen* by the config cache
//! at warm time, so it can't be per-request. A plugin runs at request time,
//! after the config cache, touching no stored config at all — so it's
//! per-request, never trips the detector, and lets the config cache stay on.
//!
//! The module is inert on any non-bougie host, so it's safe to leave installed;
//! we deploy it once (idempotent) and never tear it down.

use std::fs;
use std::path::Path;

use eyre::{Result, WrapErr};

/// The bundled `Bougie_Share` module sources, embedded at build time and written
/// into `app/code/Bougie/Share/` on deploy.
const MODULE_FILES: &[(&str, &str)] = &[
    ("registration.php", include_str!("share_assets/magento/registration.php")),
    ("etc/module.xml", include_str!("share_assets/magento/etc/module.xml")),
    ("etc/di.xml", include_str!("share_assets/magento/etc/di.xml")),
    (
        "Plugin/RequestRelativeBaseUrl.php",
        include_str!("share_assets/magento/Plugin/RequestRelativeBaseUrl.php"),
    ),
];

/// What `ensure` did, so the caller can log a one-liner.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FixupOutcome {
    /// The project needs no share fixup (not Magento).
    NotApplicable,
    /// The Magento `Bougie_Share` module was already in place — nothing to do.
    AlreadyInstalled,
    /// The Magento `Bougie_Share` module was (re)deployed; the config cache was
    /// flushed so its plugin takes effect.
    Installed,
}

/// Ensure `project` has whatever share fixup its framework needs. Idempotent.
///
/// Returns the [`FixupOutcome`]; deploying the Magento module writes files under
/// `app/code/Bougie/Share/`, enables it via the (gitignored) `app/etc/env.php`
/// `modules` map, and flushes the config cache once so the plugin is picked up.
pub fn ensure(project: &Path) -> Result<FixupOutcome> {
    if !is_magento(project) {
        return Ok(FixupOutcome::NotApplicable);
    }

    let files_changed = write_module_files(project)?;
    let enabled_now = enable_in_env_php(project)?;

    if files_changed || enabled_now {
        // The plugin list Magento compiled into the config cache predates our
        // module; flush so the next request rebuilds it and applies the plugin.
        // Best-effort: a warm cache we couldn't flush shouldn't sink the share —
        // a cold cache needs no flush, and the user can flush by hand otherwise.
        if let Err(e) = magento::cache_flush(project) {
            tracing::warn!(
                "installed the Bougie_Share module but `bin/magento cache:flush` failed ({e}); \
                 if share URLs look wrong, run `bougie run -- bin/magento cache:flush`"
            );
        }
        Ok(FixupOutcome::Installed)
    } else {
        Ok(FixupOutcome::AlreadyInstalled)
    }
}

/// A project is Magento if it ships the `bin/magento` CLI entrypoint.
fn is_magento(project: &Path) -> bool {
    project.join("bin/magento").is_file()
}

/// Write the bundled module sources under `app/code/Bougie/Share/`. Returns
/// `true` if any file was created or its contents changed (so the caller knows
/// whether a cache flush is warranted).
fn write_module_files(project: &Path) -> Result<bool> {
    let module_dir = project.join("app/code/Bougie/Share");
    let mut changed = false;
    for (rel, contents) in MODULE_FILES {
        let dest = module_dir.join(rel);
        if fs::read_to_string(&dest).ok().as_deref() == Some(*contents) {
            continue; // already byte-identical
        }
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)
                .wrap_err_with(|| format!("creating {}", parent.display()))?;
        }
        fs::write(&dest, contents).wrap_err_with(|| format!("writing {}", dest.display()))?;
        changed = true;
    }
    Ok(changed)
}

/// Enable `Bougie_Share` by adding it to the `modules` map in `app/etc/env.php`
/// (never `config.php`). `env.php` is the gitignored, environment-specific
/// deployment config; Magento merges its `modules` map over `config.php`, and
/// `modules` is not one of the `ConfigChangeDetector` sections — so this is a
/// zero-committed-footprint enable that never trips the detector. Returns `true`
/// if the file was modified.
fn enable_in_env_php(project: &Path) -> Result<bool> {
    let path = project.join("app/etc/env.php");
    let content = fs::read_to_string(&path)
        .wrap_err_with(|| format!("reading {} to enable Bougie_Share", path.display()))?;

    let Some(updated) = with_module_enabled(&content) else {
        return Ok(false); // already listed — leave the user's value untouched
    };
    fs::write(&path, updated).wrap_err_with(|| format!("writing {}", path.display()))?;
    Ok(true)
}

/// Pure transform: return `env.php` contents with `'Bougie_Share' => 1` added to
/// the `modules` map, or `None` if the module is already listed (so we neither
/// duplicate the key nor override a user's explicit `=> 0`).
///
/// Magento writes `env.php` with `var_export`, so the `modules` map is
/// `'modules' => \n array ( ... )`. We insert our entry as the first element,
/// matching that formatting; if there's no `modules` map we append a fresh one
/// before the closing paren of the returned array.
fn with_module_enabled(content: &str) -> Option<String> {
    if content.contains("'Bougie_Share'") {
        return None;
    }

    // Locate the body of the existing `modules` map, if any. Magento writes
    // `env.php` with `var_export` (2 spaces/level), so a module entry — a
    // depth-2 element — is always four-space indented.
    if let Some(body_start) = modules_body_start(content) {
        let entry = "    'Bougie_Share' => 1,\n";
        let mut out = String::with_capacity(content.len() + entry.len());
        out.push_str(&content[..body_start]);
        out.push_str(entry);
        out.push_str(&content[body_start..]);
        return Some(out);
    }

    // No `modules` map at all: splice a fresh block in before the outer array's
    // closing `)`/`]` at end of file.
    let close = outer_array_close(content)?;
    let block = "  'modules' => \n  array (\n    'Bougie_Share' => 1,\n  ),\n";
    let mut out = String::with_capacity(content.len() + block.len());
    out.push_str(&content[..close]);
    out.push_str(block);
    out.push_str(&content[close..]);
    Some(out)
}

/// Byte offset just after the `array (` / `[` that opens the `modules` map's
/// body (i.e. the start of the first element's line), or `None` if there's no
/// `modules` key.
fn modules_body_start(content: &str) -> Option<usize> {
    let key = content.find("'modules'")?;
    let after = &content[key..];
    // The array opener following the key, whichever syntax the file uses.
    let (open_rel, open_len) = ["array (", "array(", "["]
        .iter()
        .filter_map(|tok| after.find(tok).map(|i| (i, tok.len())))
        .min_by_key(|(i, _)| *i)?;
    let open_end = key + open_rel + open_len;
    // Skip to just past the newline that follows the opener.
    let nl = content[open_end..].find('\n')?;
    Some(open_end + nl + 1)
}

/// Byte offset of the outer (returned) array's closing `)` or `]` — the last one
/// in the file, before any trailing `;`/whitespace.
fn outer_array_close(content: &str) -> Option<usize> {
    content.rfind([')', ']'])
}

mod magento {
    use std::path::Path;
    use std::process::Command;

    use eyre::{Result, WrapErr, eyre};

    use crate::commands::env;

    /// Run `bin/magento cache:flush` in the project environment, using the same
    /// resolved project PHP + env that `bougie run` would give a child (so the
    /// tenant's DB/cache service vars are present). Output is captured — only
    /// surfaced on failure — so it never clutters the share banner.
    pub fn cache_flush(project: &Path) -> Result<()> {
        let php = env::resolve_php_bin(project)
            .ok_or_else(|| eyre!("could not resolve the project PHP interpreter"))?;
        let out = Command::new(&php)
            .arg(project.join("bin/magento"))
            .arg("cache:flush")
            .current_dir(project)
            .envs(env::project_script_env(project, true))
            .output()
            .wrap_err("spawning bin/magento cache:flush")?;
        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr);
            return Err(eyre!("bin/magento cache:flush {}: {}", out.status, stderr.trim()));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // A trimmed but faithful Magento `env.php` (var_export formatting).
    const ENV_WITH_MODULES: &str = "<?php\nreturn array (\n  'backend' => \n  array (\n    'frontName' => 'admin',\n  ),\n  'modules' => \n  array (\n    'Magento_TwoFactorAuth' => 0,\n  ),\n);\n";

    const ENV_NO_MODULES: &str = "<?php\nreturn array (\n  'backend' => \n  array (\n    'frontName' => 'admin',\n  ),\n  'install' => \n  array (\n    'date' => 'x',\n  ),\n);\n";

    #[test]
    fn inserts_into_existing_modules_map() {
        let out = with_module_enabled(ENV_WITH_MODULES).expect("should modify");
        assert!(out.contains("'Bougie_Share' => 1,"));
        // Inserted as the first element of the modules map, four-space indented.
        assert!(out.contains("array (\n    'Bougie_Share' => 1,\n    'Magento_TwoFactorAuth' => 0,"));
        // Still valid: only the modules map grew.
        assert!(out.contains("'Magento_TwoFactorAuth' => 0,"));
    }

    #[test]
    fn adds_a_modules_map_when_absent() {
        let out = with_module_enabled(ENV_NO_MODULES).expect("should modify");
        assert!(out.contains("'modules' => \n  array (\n    'Bougie_Share' => 1,\n  ),"));
        // Spliced before the outer array close, so the file still ends `);`.
        assert!(out.trim_end().ends_with(");"));
    }

    #[test]
    fn idempotent_when_already_listed() {
        let already = ENV_WITH_MODULES.replace(
            "'Magento_TwoFactorAuth' => 0,",
            "'Magento_TwoFactorAuth' => 0,\n    'Bougie_Share' => 1,",
        );
        assert_eq!(with_module_enabled(&already), None);
    }

    #[test]
    fn respects_an_explicit_disable() {
        let disabled =
            ENV_WITH_MODULES.replace("'Magento_TwoFactorAuth' => 0,", "'Bougie_Share' => 0,");
        // Present (even as 0) → we don't touch it.
        assert_eq!(with_module_enabled(&disabled), None);
    }

    #[test]
    fn handles_short_array_syntax() {
        let short = "<?php\nreturn [\n  'modules' => [\n    'Magento_TwoFactorAuth' => 0,\n  ],\n];\n";
        let out = with_module_enabled(short).expect("should modify");
        assert!(out.contains("'Bougie_Share' => 1,"));
        assert!(out.contains("'Magento_TwoFactorAuth' => 0,"));
    }
}
