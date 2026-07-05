//! Builtin recipes shipped with the binary, plus per-project-type
//! detection and per-task merge with a local `bougie.toml`.

use super::parser::{parse, Recipe};

/// `(name, TOML body)` of every builtin recipe, embedded at compile
/// time. The name (`"magento"`, etc.) is what `--recipe <name>` selects.
pub const BUILTINS: &[(&str, &str)] = &[
    ("magento", include_str!("../recipes/magento.toml")),
    ("laravel", include_str!("../recipes/laravel.toml")),
    ("generic", include_str!("../recipes/generic.toml")),
];

/// Sniff a `composer.json` for a builtin recipe name. Returns the
/// name (`"magento"`, `"laravel"`, `"generic"`) — never `None`,
/// because `generic` is the universal fallback.
///
/// Detection rules per RECIPES.md §4:
/// - `magento/`, `mage-os/` or `modulargento/` `product-community-edition` /
///   `magento2-base` → `magento` (Mage-OS and the fully-modular modulargento
///   distribution are drop-in Magento forks and use the same recipe / app
///   layout)
/// - `laravel/framework` → `laravel`
/// - otherwise → `generic`
pub fn detect_from_text(composer_json: Option<&str>) -> &'static str {
    let Some(text) = composer_json else { return "generic" };
    let v: serde_json::Value = match serde_json::from_str(text) {
        Ok(v) => v,
        Err(_) => return "generic",
    };
    let require = v.get("require").and_then(|r| r.as_object());
    let has = |pkg: &str| {
        require
            .is_some_and(|r| r.contains_key(pkg))
    };
    let name = v.get("name").and_then(|n| n.as_str()).unwrap_or("");
    if has("magento/product-community-edition")
        || has("magento/magento2-base")
        || has("mage-os/product-community-edition")
        || has("mage-os/magento2-base")
        || has("modulargento/product-community-edition")
        || has("modulargento/magento2-base")
        || name == "magento/magento2ce"
        || name == "magento/magento2"
        || name == "magento/magento2-base"
    {
        return "magento";
    }
    if has("laravel/framework") {
        return "laravel";
    }
    "generic"
}

/// Look up a builtin recipe by name and parse it.
///
/// # Panics
///
/// Panics on a programming error: a builtin recipe TOML literal
/// embedded in this crate fails to parse. Caught at test time, so
/// in production this never fires.
pub fn load_builtin(name: &str) -> Option<Recipe> {
    let (_, text) = BUILTINS.iter().find(|(n, _)| *n == name)?;
    Some(parse(text).expect("builtin recipe must parse"))
}

/// Per-task merge per RECIPES.md §4: a task defined locally fully
/// replaces the builtin's version; builtin-only tasks are unchanged;
/// new local tasks are added.
pub fn merge_with_builtin(builtin: Recipe, local: Recipe) -> Recipe {
    let mut out = builtin;
    for (name, def) in local.tasks {
        out.tasks.insert(name, def);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_builtins_parse() {
        for (name, text) in BUILTINS {
            parse(text).unwrap_or_else(|e| panic!("builtin {name} failed to parse: {e}"));
        }
    }

    #[test]
    fn magento_detection() {
        let j = r#"{"require":{"magento/product-community-edition":"2.4.7"}}"#;
        assert_eq!(detect_from_text(Some(j)), "magento");
    }

    #[test]
    fn mageos_detection() {
        // Mage-OS is a drop-in Magento fork (mage-os/* vendor) and must
        // map to the same recipe — otherwise `bougie start` falls back
        // to `generic` and never brings up services / runs setup:install.
        let j = r#"{"require":{"mage-os/product-community-edition":"3.0.0"}}"#;
        assert_eq!(detect_from_text(Some(j)), "magento");
        let b = r#"{"require":{"mage-os/magento2-base":"3.0.0"}}"#;
        assert_eq!(detect_from_text(Some(b)), "magento");
    }

    #[test]
    fn modulargento_detection() {
        // The fully-modular modulargento distribution (modulargento/* vendor,
        // named modulargento/project-community-edition, requiring
        // modulargento/product-community-edition) must map to the magento
        // recipe too — otherwise `bougie start` runs `generic` and skips
        // services / setup:install.
        let j = r#"{"name":"modulargento/project-community-edition","require":{"modulargento/product-community-edition":"3.0.0"}}"#;
        assert_eq!(detect_from_text(Some(j)), "magento");
        let b = r#"{"require":{"modulargento/magento2-base":"3.0.0"}}"#;
        assert_eq!(detect_from_text(Some(b)), "magento");
    }

    #[test]
    fn magento_upstream_monorepo_detection() {
        // The magento/magento2 repo's own composer.json doesn't
        // require the metapackages; it *is* the source. Detect by
        // package name as well.
        let j = r#"{"name":"magento/magento2ce","require":{"php":"~8.3.0"}}"#;
        assert_eq!(detect_from_text(Some(j)), "magento");
    }

    #[test]
    fn laravel_detection() {
        let j = r#"{"require":{"laravel/framework":"^11.0"}}"#;
        assert_eq!(detect_from_text(Some(j)), "laravel");
    }

    #[test]
    fn falls_back_to_generic() {
        assert_eq!(detect_from_text(None), "generic");
        assert_eq!(detect_from_text(Some("{}")), "generic");
    }

    /// Execute a builtin task's `run` script the way the runner does
    /// (`/bin/sh -e -c`, cwd = project root) with a fake `bougie` on
    /// PATH that records every invocation's argv, plus the tenant env
    /// the daemon would inject. Returns the recorded argv log.
    fn run_script(script: &str, root: &std::path::Path, rabbitmq_env: bool) -> String {
        use std::os::unix::fs::PermissionsExt;
        let bindir = root.join(".stub-bin");
        std::fs::create_dir_all(&bindir).unwrap();
        let log = root.join("argv.log");
        let _ = std::fs::remove_file(&log);
        let fake = bindir.join("bougie");
        // `cat` drains stdin so the install task's heredoc-fed
        // `bougie run -- php <<'PHP'` step can't block or SIGPIPE.
        std::fs::write(
            &fake,
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"$BOUGIE_STUB_LOG\"\ncat > /dev/null\n",
        )
        .unwrap();
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        let path = format!(
            "{}:{}",
            bindir.display(),
            std::env::var("PATH").unwrap_or_default()
        );
        let mut cmd = std::process::Command::new("/bin/sh");
        cmd.arg("-e")
            .arg("-c")
            .arg(script)
            .current_dir(root)
            .env("PATH", path)
            .env("BOUGIE_STUB_LOG", &log);
        if rabbitmq_env {
            cmd.env("BOUGIE_SERVICE_RABBITMQ_HOST", "127.0.0.1")
                .env("BOUGIE_SERVICE_RABBITMQ_PORT", "5672")
                .env("BOUGIE_SERVICE_RABBITMQ_USER", "u")
                .env("BOUGIE_SERVICE_RABBITMQ_PASSWORD", "p")
                .env("BOUGIE_SERVICE_RABBITMQ_VHOST", "/");
        }
        let status = cmd.status().unwrap();
        assert!(status.success(), "script exited {status:?}");
        std::fs::read_to_string(&log).unwrap_or_default()
    }

    #[test]
    fn magento_amqp_wiring_follows_module_presence() {
        // Additive / modular Mage-OS builds (modulargento) can omit the
        // Amqp module, and `setup:install` only accepts `--amqp-*` for
        // module components on disk — so the recipe must key both the
        // rabbitmq service and the flags off the module directory.
        let magento = load_builtin("magento").unwrap();
        let install = magento.tasks["install"].run.clone().unwrap();
        let services = magento.tasks["services"].run.clone().unwrap();

        // Package layout with the module → flags wired, rabbitmq up'd.
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("vendor/mage-os/module-amqp")).unwrap();
        let log = run_script(&install, dir.path(), true);
        assert!(log.contains("--amqp-host=127.0.0.1"), "log:\n{log}");
        assert!(log.contains("--amqp-virtualhost=/"), "log:\n{log}");
        let log = run_script(&services, dir.path(), true);
        assert!(log.contains("service add rabbitmq"), "log:\n{log}");
        assert!(log.contains("service up --detach rabbitmq"), "log:\n{log}");

        // Module absent (the framework-amqp *library* alone doesn't
        // register the CLI options) → no flags, no broker.
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("vendor/mage-os/framework-amqp")).unwrap();
        let log = run_script(&install, dir.path(), true);
        assert!(!log.contains("--amqp"), "log:\n{log}");
        assert!(log.contains("setup:install"), "install must still run:\n{log}");
        let log = run_script(&services, dir.path(), true);
        assert!(!log.contains("rabbitmq"), "log:\n{log}");
        assert!(log.contains("service add mariadb"), "log:\n{log}");

        // magento/magento2 monorepo layout (modules live in-tree).
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("app/code/Magento/Amqp")).unwrap();
        let log = run_script(&install, dir.path(), true);
        assert!(log.contains("--amqp-host=127.0.0.1"), "log:\n{log}");

        // Module present but no rabbitmq tenant env (service skipped or
        // daemon down) → flags withheld rather than passed empty.
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("vendor/magento/module-amqp")).unwrap();
        let log = run_script(&install, dir.path(), false);
        assert!(!log.contains("--amqp"), "log:\n{log}");
    }

    #[test]
    fn local_overrides_builtin_task() {
        let builtin = parse(
            r#"
[task.vendor]
run = "composer install"

[task.start]
deps = ["vendor"]
run = "echo orig"
"#,
        )
        .unwrap();
        let local = parse(
            r#"
[task.vendor]
run = "composer install --no-dev"
"#,
        )
        .unwrap();
        let merged = merge_with_builtin(builtin, local);
        assert_eq!(merged.tasks["vendor"].run.as_deref(), Some("composer install --no-dev"));
        assert_eq!(merged.tasks["start"].run.as_deref(), Some("echo orig"));
    }
}
