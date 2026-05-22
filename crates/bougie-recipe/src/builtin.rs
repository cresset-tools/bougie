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
/// - `magento/product-community-edition` or `magento/magento2-base`
///   → `magento`
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

/// Look up a builtin recipe by name and parse it. Panics on a
/// programming error (a malformed builtin would mean the binary
/// shipped broken).
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
