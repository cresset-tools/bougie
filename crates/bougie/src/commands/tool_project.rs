//! Project-context detection for the ephemeral tool lane (`bougie
//! tool run` / `bgx`).
//!
//! Tools like n98-magerun2 or deployer are *project clients*: they
//! boot the surrounding application, so they need the project's PHP
//! version and extension set — yet they can't live in the project's
//! `composer.json` (dependency conflicts are the whole reason
//! `-dist` repacks exist). This module derives both from the project
//! the user is standing in, so
//!
//! ```text
//! bgx --with zip --with pdo_mysql --with intl --php 8.4 n98/magerun2-dist ...
//! ```
//!
//! collapses to `bgx n98/magerun2-dist ...`.
//!
//! Detection is deliberately best-effort and read-only: any missing
//! or malformed input degrades to "no project context" (with a
//! warning for genuinely malformed files) rather than failing the
//! run. Persistent `bougie tool install` never calls this — global
//! tools stay project-blind so they behave the same from any cwd.
//!
//! What flows out, and where it came from:
//!
//! - **Exact interpreter**: `vendor/bougie/state/resolved` of a synced
//!   bougie project (skipped when the project resolved to a *system*
//!   PHP — tools always run managed interpreters).
//! - **PHP constraint**: `bougie.toml [php]version` ∩ `composer.json
//!   require.php`, mirroring sync's `resolve_php_inputs` precedence;
//!   when neither is written, `infer_php::infer_raw` (Magento matrix,
//!   then composer.lock intersection).
//! - **Extensions**: `composer.json require.ext-*` ∪
//!   `infer_php::infer_extensions` (framework recommended set +
//!   lockfile `ext-*`), minus builtins/baseline, minus names the
//!   project opted out of via `[extensions] name = false`.

use bougie_config::load_project;
use bougie_fs::state::{read_project_resolved, read_project_resolved_php_path};
use bougie_tool::resolve::{ProjectContext, ProjectPhp};
use composer_semver::Constraint;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// Detect the surrounding project and derive tool-run context from
/// it. `None` when there's no project in the ancestry or it
/// contributes nothing (no PHP signal, no extensions).
pub fn detect() -> Option<ProjectContext> {
    let cwd = std::env::current_dir().ok()?;
    let root = find_project_root(&cwd)?;
    detect_at(&root)
}

/// Nearest ancestor that looks like a PHP project. Same probe set as
/// `bougie server`'s `locate_project_root`, but returning `None`
/// instead of erroring — for a tool run, "no project" is a normal
/// answer.
fn find_project_root(cwd: &Path) -> Option<PathBuf> {
    cwd.ancestors()
        .find(|anc| {
            anc.join("composer.json").is_file()
                || anc.join("bougie.toml").is_file()
                || bougie_paths::project::is_root(anc)
        })
        .map(Path::to_path_buf)
}

fn detect_at(root: &Path) -> Option<ProjectContext> {
    let config = match load_project(root) {
        Ok(c) => c,
        Err(e) => {
            eprintln!(
                "warning: ignoring project at {} for tool-run context ({e:#})",
                root.display(),
            );
            return None;
        }
    };

    // Exact resolved interpreter of a synced bougie project. A
    // `resolved-php-path` marker means sync picked a *system* PHP —
    // tools only exec managed interpreters, so that signal is
    // constraint-shaped at best, not exact.
    let resolved = if read_project_resolved_php_path(root).is_some() {
        None
    } else {
        read_project_resolved(root).ok()
    };

    let (constraint, constraint_raw, php_source) = project_constraint(root, &config);

    // Extension set: declared ∪ inferred, minus what every tool PHP
    // already loads, minus explicit project opt-outs.
    let mut names: BTreeSet<String> = config
        .composer
        .as_ref()
        .map(|c| c.require_extensions.clone())
        .unwrap_or_default();
    names.extend(super::infer_php::infer_extensions(root).0);
    names.retain(|n| {
        super::tool_callbacks::ext_needs_install(n)
            && config
                .bougie
                .extensions
                .get(n)
                .is_none_or(|pin| !pin.is_disabled())
    });
    let extensions: Vec<String> = names.into_iter().collect();

    if resolved.is_none() && constraint.is_none() && extensions.is_empty() {
        return None;
    }

    let source = php_source.unwrap_or_else(|| root.display().to_string());
    Some(ProjectContext {
        php: ProjectPhp {
            resolved,
            constraint,
            constraint_raw,
            source,
        },
        extensions,
    })
}

/// The project's PHP constraint, mirroring sync's `resolve_php_inputs`
/// precedence: `bougie.toml [php]version` and `composer.json
/// require.php` are intersected when both exist; `infer_php` fills in
/// when neither is written. Returns `(parsed, raw, source_label)` — `raw`
/// is the single written form handed to the PHP auto-installer when
/// nothing on disk matches, preferring the bougie.toml pin (the most
/// deliberate signal, and always a valid install spec).
fn project_constraint(
    root: &Path,
    config: &bougie_config::ProjectConfig,
) -> (Option<Constraint>, Option<String>, Option<String>) {
    let mut parts: Vec<Constraint> = Vec::new();
    let mut raw: Option<String> = None;
    let mut source: Option<String> = None;

    if let Some(pin) = config.bougie.php.version.as_deref() {
        match Constraint::parse(pin) {
            Ok(c) => {
                parts.push(c);
                raw = Some(pin.to_string());
                source = Some(root.join("bougie.toml").display().to_string());
            }
            Err(e) => eprintln!(
                "warning: ignoring bougie.toml [php]version {pin:?} for tool-run \
                 context ({e})",
            ),
        }
    }
    if let Some(req) = config.composer.as_ref().and_then(|c| c.require_php.as_deref()) {
        match Constraint::parse(req) {
            Ok(c) => {
                parts.push(c);
                if raw.is_none() {
                    raw = Some(req.to_string());
                }
                if source.is_none() {
                    source = Some(root.join("composer.json").display().to_string());
                }
            }
            Err(e) => eprintln!(
                "warning: ignoring composer.json require.php {req:?} for tool-run \
                 context ({e})",
            ),
        }
    }

    if parts.is_empty() {
        if let Some(inferred) = super::infer_php::infer_raw(root) {
            return (Some(inferred.constraint), inferred.raw, Some(inferred.source));
        }
        return (None, None, None);
    }

    let combined = if parts.len() == 1 {
        parts.into_iter().next().expect("len checked")
    } else {
        Constraint::And(parts)
    };
    (Some(combined), raw, source)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(root: &Path, rel: &str, body: &str) {
        let path = root.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, body).unwrap();
    }

    #[test]
    fn no_project_markers_means_none() {
        let td = tempfile::TempDir::new().unwrap();
        let nested = td.path().join("a").join("b");
        std::fs::create_dir_all(&nested).unwrap();
        assert!(find_project_root(&nested).is_none());
    }

    #[test]
    fn walks_up_to_composer_json() {
        let td = tempfile::TempDir::new().unwrap();
        write(td.path(), "composer.json", "{}");
        let nested = td.path().join("src").join("deep");
        std::fs::create_dir_all(&nested).unwrap();
        assert_eq!(
            find_project_root(&nested).unwrap(),
            // TempDir on macOS hands out /var/folders symlinks; compare
            // uncanonicalized paths since find_project_root doesn't
            // canonicalize either.
            td.path()
        );
    }

    #[test]
    fn empty_composer_json_contributes_nothing() {
        let td = tempfile::TempDir::new().unwrap();
        write(td.path(), "composer.json", "{}");
        assert!(detect_at(td.path()).is_none());
    }

    #[test]
    fn require_php_and_extensions_flow_through() {
        let td = tempfile::TempDir::new().unwrap();
        write(
            td.path(),
            "composer.json",
            r#"{"require":{"php":"^8.2","ext-intl":"*","ext-zip":"*","ext-json":"*","ext-mbstring":"*","some/pkg":"^1.0"}}"#,
        );
        let ctx = detect_at(td.path()).unwrap();
        // json is builtin, mbstring is baseline — both filtered.
        assert_eq!(ctx.extensions, vec!["intl".to_string(), "zip".to_string()]);
        assert!(ctx.php.constraint.is_some());
        assert_eq!(ctx.php.constraint_raw.as_deref(), Some("^8.2"));
        assert!(ctx.php.source.ends_with("composer.json"), "{}", ctx.php.source);
        assert!(ctx.php.resolved.is_none());
    }

    #[test]
    fn magento_project_gets_matrix_php_and_recommended_extensions() {
        let td = tempfile::TempDir::new().unwrap();
        write(
            td.path(),
            "composer.json",
            r#"{"require":{"magento/product-community-edition":"2.4.7"}}"#,
        );
        let ctx = detect_at(td.path()).unwrap();
        // The Magento matrix row for 2.4.7 — written form preserved for
        // the installer fallback.
        assert_eq!(ctx.php.constraint_raw.as_deref(), Some("~8.2.0 || ~8.3.0"));
        assert!(ctx.php.source.contains("magento/product-community-edition"));
        // Recommended set flows in, filtered of baseline/builtins.
        assert!(ctx.extensions.iter().any(|e| e == "intl"), "{:?}", ctx.extensions);
        assert!(ctx.extensions.iter().any(|e| e == "pdo_mysql"), "{:?}", ctx.extensions);
        assert!(ctx.extensions.iter().any(|e| e == "zip"), "{:?}", ctx.extensions);
        // mbstring is baseline for every tool PHP → filtered even
        // though Magento's recommended set lists it.
        assert!(!ctx.extensions.iter().any(|e| e == "mbstring"), "{:?}", ctx.extensions);
    }

    #[test]
    fn bougie_toml_pin_wins_raw_and_ands_with_require_php() {
        let td = tempfile::TempDir::new().unwrap();
        write(
            td.path(),
            "composer.json",
            r#"{"require":{"php":"^8.2"}}"#,
        );
        write(td.path(), "bougie.toml", "[php]\nversion = \"8.3\"\n");
        let ctx = detect_at(td.path()).unwrap();
        assert_eq!(ctx.php.constraint_raw.as_deref(), Some("8.3"));
        assert!(ctx.php.source.ends_with("bougie.toml"), "{}", ctx.php.source);
        let c = ctx.php.constraint.unwrap();
        // The AND of both: 8.3.x satisfies, 8.2.x doesn't.
        let v83 = composer_semver::Version::parse("8.3.12").unwrap();
        let v82 = composer_semver::Version::parse("8.2.20").unwrap();
        assert!(c.matches(&v83));
        assert!(!c.matches(&v82));
    }

    #[test]
    fn disabled_extension_pin_is_respected() {
        let td = tempfile::TempDir::new().unwrap();
        write(
            td.path(),
            "composer.json",
            r#"{"require":{"ext-intl":"*","ext-zip":"*"}}"#,
        );
        write(td.path(), "bougie.toml", "[extensions]\nzip = false\n");
        let ctx = detect_at(td.path()).unwrap();
        assert_eq!(ctx.extensions, vec!["intl".to_string()]);
    }

    #[test]
    fn resolved_marker_flows_through_unless_system_php() {
        let td = tempfile::TempDir::new().unwrap();
        write(td.path(), "composer.json", r#"{"require":{"php":"^8.2"}}"#);
        write(td.path(), "vendor/bougie/state/resolved", "8.3.12-nts\n");
        let ctx = detect_at(td.path()).unwrap();
        assert_eq!(
            ctx.php.resolved,
            Some(("8.3.12".to_string(), "nts".to_string()))
        );

        // A system-PHP marker disables the exact lane.
        write(
            td.path(),
            "vendor/bougie/state/resolved-php-path",
            "/usr/bin/php\n",
        );
        let ctx = detect_at(td.path()).unwrap();
        assert!(ctx.php.resolved.is_none());
    }
}
