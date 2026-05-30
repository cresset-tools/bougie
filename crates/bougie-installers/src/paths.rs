//! `composer/installers` — native install-path routing.
//!
//! `composer/installers` relocates a package based on its `type`, with
//! optional per-project overrides in the root `composer.json`'s
//! `extra.installer-paths`. It does no copying — it only changes where a
//! package's tree is installed (and, downstream, where the autoloader
//! points and what `installed.json`'s `install-path` records).
//!
//! Scope: bougie ships the Magento family of types plus the generic
//! fallback. The built-in [`builtin_location`] table is structured so
//! other frameworks (~85 more upstream) are one-line additions later.
//! Note that `magento2-module` is deliberately *not* here — Magento 2
//! modules stay in `vendor/` and are discovered at runtime via their
//! `registration.php`; `composer/installers` only knows the Magento 1
//! `magento-*` types.
//!
//! Precedence, matching upstream `BaseInstaller`/`Installer`:
//!   1. first matching `extra.installer-paths` entry (file order),
//!   2. else the built-in type → path template,
//!   3. else `vendor/<name>`.

use serde_json::Value;

/// Built-in `type` → path-template map. Ported from
/// `composer/installers` `MagentoInstaller::$locations`. Templates use
/// the `{$name}` / `{$vendor}` / `{$type}` tokens expanded by
/// [`expand_template`].
// Some arms map to coincidentally-identical paths (e.g. drupal-module and
// prestashop-module both `modules/{$name}/`). They're kept as distinct
// arms on purpose: this table mirrors composer/installers' per-framework
// `$locations`, and collapsing unrelated frameworks would obscure that and
// break if either upstream path diverges.
#[allow(clippy::match_same_arms)]
#[must_use]
pub fn builtin_location(package_type: &str) -> Option<&'static str> {
    match package_type {
        // Magento 1 (composer/installers MagentoInstaller).
        "magento-theme" => Some("app/design/frontend/{$name}/"),
        "magento-skin" => Some("skin/frontend/default/{$name}/"),
        "magento-library" => Some("lib/{$name}/"),
        // WordPress (Bedrock / Composer-managed installs).
        "wordpress-plugin" => Some("wp-content/plugins/{$name}/"),
        "wordpress-theme" => Some("wp-content/themes/{$name}/"),
        "wordpress-muplugin" => Some("wp-content/mu-plugins/{$name}/"),
        "wordpress-dropin" => Some("wp-content/{$name}/"),
        // Drupal (drupal/recommended-project depends on composer/installers).
        "drupal-core" => Some("core/"),
        "drupal-module" => Some("modules/{$name}/"),
        "drupal-theme" => Some("themes/{$name}/"),
        "drupal-library" => Some("libraries/{$name}/"),
        "drupal-profile" => Some("profiles/{$name}/"),
        "drupal-database-driver" => Some("drivers/lib/Drupal/Driver/Database/{$name}/"),
        "drupal-drush" => Some("drush/{$name}/"),
        "drupal-custom-theme" => Some("themes/custom/{$name}/"),
        "drupal-custom-module" => Some("modules/custom/{$name}/"),
        "drupal-custom-profile" => Some("profiles/custom/{$name}/"),
        "drupal-multisite" => Some("sites/{$name}/"),
        "drupal-console" => Some("console/{$name}/"),
        "drupal-console-language" => Some("console/language/{$name}/"),
        "drupal-config" => Some("config/sync/"),
        "drupal-recipe" => Some("recipes/{$name}"),
        // Shopware. NB: the `{$name}` token is inflected per-type — see
        // [`inflect_name`]. Templates here are verbatim from upstream.
        "shopware-backend-plugin" => Some("engine/Shopware/Plugins/Local/Backend/{$name}/"),
        "shopware-core-plugin" => Some("engine/Shopware/Plugins/Local/Core/{$name}/"),
        "shopware-frontend-plugin" => Some("engine/Shopware/Plugins/Local/Frontend/{$name}/"),
        "shopware-theme" => Some("templates/{$name}/"),
        "shopware-plugin" => Some("custom/plugins/{$name}/"),
        "shopware-frontend-theme" => Some("themes/Frontend/{$name}/"),
        // PrestaShop.
        "prestashop-module" => Some("modules/{$name}/"),
        "prestashop-theme" => Some("themes/{$name}/"),
        _ => None,
    }
}

/// Parsed, order-preserving view of the root `composer.json`'s
/// `extra.installer-paths`. Each entry is a path template plus the list
/// of matchers (`vendor/name`, `type:<type>`, or `vendor:<vendor>`) that
/// select it. Iteration order is file order — first match wins.
#[derive(Debug, Clone, Default)]
pub struct InstallerPaths {
    entries: Vec<(String, Vec<String>)>,
}

impl InstallerPaths {
    /// Parse from a root `composer.json` value. Missing or malformed
    /// `extra.installer-paths` yields an empty (no-override) instance.
    #[must_use]
    pub fn parse(root_composer_json: &Value) -> Self {
        Self::from_extra(root_composer_json.get("extra").unwrap_or(&Value::Null))
    }

    /// Parse from the root `composer.json`'s `extra` object directly —
    /// for callers that only have the `extra` block on hand (e.g. a
    /// typed manifest model that drops the rest of the document).
    #[must_use]
    pub fn from_extra(extra: &Value) -> Self {
        let mut entries = Vec::new();
        if let Some(obj) = extra.get("installer-paths").and_then(Value::as_object) {
            for (template, names) in obj {
                let matchers = match names {
                    Value::Array(arr) => {
                        arr.iter().filter_map(|v| v.as_str().map(str::to_string)).collect()
                    }
                    // A bare string matcher is tolerated for robustness.
                    Value::String(s) => vec![s.clone()],
                    _ => continue,
                };
                entries.push((template.clone(), matchers));
            }
        }
        Self { entries }
    }

    /// Return the first template whose matcher list selects this package,
    /// in file order, or `None` if no override matches.
    fn matching_template(&self, name: &str, package_type: Option<&str>, vendor: &str) -> Option<&str> {
        let type_matcher = package_type.map(|t| format!("type:{t}"));
        let vendor_matcher = format!("vendor:{vendor}");
        for (template, matchers) in &self.entries {
            let hit = matchers.iter().any(|m| {
                m == name
                    || type_matcher.as_deref() == Some(m.as_str())
                    || *m == vendor_matcher
            });
            if hit {
                return Some(template);
            }
        }
        None
    }
}

/// Compute a package's project-root-relative install path.
///
/// `name` is the full `vendor/package`. Returns `vendor/<name>` when no
/// override and no built-in location applies — the Composer default and
/// the Magento 2 module case.
#[must_use]
pub fn install_path(name: &str, package_type: Option<&str>, installer_paths: &InstallerPaths) -> String {
    let (vendor, pkg) = split_name(name);
    // composer/installers runs `inflectPackageVars` before templating, so
    // the `{$name}` token can differ from the bare package part (Shopware).
    let name_var = inflect_name(package_type, vendor, pkg);

    if let Some(template) = installer_paths.matching_template(name, package_type, vendor) {
        return expand_template(template, &name_var, vendor, package_type);
    }
    if let Some(template) = package_type.and_then(builtin_location) {
        return expand_template(template, &name_var, vendor, package_type);
    }
    format!("vendor/{name}")
}

/// Apply a framework installer's `inflectPackageVars` transform to the
/// `{$name}` token. Identity for every framework bougie supports except
/// Shopware, whose `ShopwareInstaller` rewrites the name:
///   - `shopware-theme` → hyphens to underscores (`my-theme` → `my_theme`);
///   - any other `shopware-*` → `ucfirst(vendor) . ucfirst(camelCase(name))`
///     (vendor `acme`, name `my-plugin` → `AcmeMyPlugin`).
fn inflect_name(package_type: Option<&str>, vendor: &str, pkg: &str) -> String {
    match package_type {
        Some("shopware-theme") => pkg.replace('-', "_"),
        Some(t) if t.starts_with("shopware-") => {
            format!("{}{}", ucfirst(vendor), ucfirst(&camel_case_hyphens(pkg)))
        }
        _ => pkg.to_string(),
    }
}

/// Port of composer/installers' `preg_replace_callback('/(-[a-z])/',
/// fn => strtoupper)`: drop each hyphen that precedes a lowercase ASCII
/// letter and uppercase that letter. A hyphen before anything else is
/// left intact.
fn camel_case_hyphens(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '-' && chars.peek().is_some_and(char::is_ascii_lowercase) {
            // `peek` already confirmed a lowercase letter follows.
            let next = chars.next().unwrap();
            out.push(next.to_ascii_uppercase());
        } else {
            out.push(c);
        }
    }
    out
}

/// PHP `ucfirst`: uppercase the first ASCII char, leave the rest.
fn ucfirst(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(first) => first.to_ascii_uppercase().to_string() + chars.as_str(),
        None => String::new(),
    }
}

/// Split `vendor/package` into its parts. A name without a slash (rare,
/// invalid Composer name) yields an empty vendor.
fn split_name(name: &str) -> (&str, &str) {
    match name.split_once('/') {
        Some((vendor, pkg)) => (vendor, pkg),
        None => ("", name),
    }
}

/// Composer's `Filesystem::findShortestPath('vendor/composer',
/// <install_path>, directories: true)` — the value stored as
/// `install-path` in `installed.json` / `installed.php`. Both paths are
/// project-root-relative; the result is relative to the
/// `vendor/composer` repository directory.
///
/// For a normal `vendor/<name>` install this yields `../<name>`
/// (identical to the previous hardcoded form, so output is unchanged for
/// non-relocated packages). A package relocated outside `vendor/` (e.g.
/// `app/design/frontend/theme`) yields `../../app/design/frontend/theme`.
#[must_use]
pub fn install_path_relative_to_repo(install_path: &str) -> String {
    const REPO: [&str; 2] = ["vendor", "composer"];
    let to: Vec<&str> = install_path.split('/').filter(|s| !s.is_empty()).collect();

    // Drop the common leading segments.
    let mut common = 0;
    while common < REPO.len() && common < to.len() && REPO[common] == to[common] {
        common += 1;
    }

    let ups = REPO.len() - common;
    let remainder = to[common..].join("/");

    // When the target is a descendant of `vendor/composer` (ups == 0 —
    // i.e. every `composer/*` package, which installs to
    // `vendor/composer/<pkg>`), Composer's `findShortestPath` returns
    // `./<pkg>`, not a bare `<pkg>`. For ancestors it emits `../`-prefixed
    // paths with no `./`.
    if ups == 0 {
        return format!("./{remainder}");
    }
    let mut out = String::new();
    for _ in 0..ups {
        out.push_str("../");
    }
    out.push_str(&remainder);
    out
}

/// Expand `{$name}` / `{$vendor}` / `{$type}` in a path template and
/// strip the trailing slash so the result matches Composer's normalized
/// install path (what it records in `installed.json`).
fn expand_template(template: &str, name_var: &str, vendor: &str, package_type: Option<&str>) -> String {
    let expanded = template
        .replace("{$name}", name_var)
        .replace("{$vendor}", vendor)
        .replace("{$type}", package_type.unwrap_or(""));
    expanded.trim_end_matches('/').to_string()
}

/// Every framework prefix `composer/installers` recognizes (the `name`
/// part of a `<name>-<suffix>` package type), verbatim from its
/// `Installer::$supportedTypes`. Used by [`unsupported_framework`] to
/// distinguish "a relocatable type bougie doesn't handle yet" from an
/// ordinary `library` package that genuinely belongs in `vendor/`.
const INSTALLER_FRAMEWORKS: &[&str] = &[
    "agl", "akaunting", "annotatecms", "asgard", "attogram", "bitrix", "bonefish", "botble",
    "cakephp", "ccframework", "chef", "civicrm", "cockpit", "codeigniter", "concrete5",
    "concretecms", "croogo", "decibel", "dframe", "dokuwiki", "dolibarr", "drupal", "ee2", "ee3",
    "elgg", "eliasis", "ezplatform", "fork", "fuel", "fuelphp", "grav", "hurad", "imagecms",
    "itop", "kanboard", "known", "kodicms", "kohana", "laravel", "lavalite", "lithium", "lms",
    "magento", "majima", "mako", "mantisbt", "matomo", "mautic", "maya", "mediawiki", "miaoxing",
    "microweber", "modulework", "modx", "modxevo", "moodle", "october", "ontowiki", "osclass",
    "oxid", "phifty", "phpbb", "piwik", "plentymarkets", "porto", "ppi", "prestashop",
    "processwire", "puppet", "pxcms", "quicksilver", "radphp", "redaxo", "redaxo5", "reindex",
    "roundcube", "shopware", "silverstripe", "sitedirect", "smf", "starbug", "sydes", "sylius",
    "tao", "tastyigniter", "thelia", "tusk", "userfrosting", "vanilla", "whmcs", "winter",
    "wolfcms", "wordpress", "yawik", "zend", "zikula",
];

/// If `package_type` is a `composer/installers` framework type that
/// bougie has no built-in location for, return the framework prefix —
/// the caller should warn that the package will land in `vendor/`
/// instead of its framework path. Returns `None` for handled types
/// (covered by [`builtin_location`]), non-installers types, and the
/// default `library`/`project`/etc.
///
/// Note: a matching `extra.installer-paths` override still relocates the
/// package, so callers should suppress the warning when
/// [`install_path`] didn't fall back to `vendor/<name>`.
#[must_use]
pub fn unsupported_framework(package_type: Option<&str>) -> Option<&'static str> {
    let ty = package_type?;
    if builtin_location(ty).is_some() {
        return None;
    }
    let prefix = ty.split('-').next()?;
    INSTALLER_FRAMEWORKS.iter().copied().find(|&f| f == prefix)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn empty() -> InstallerPaths {
        InstallerPaths::default()
    }

    #[test]
    fn unknown_type_defaults_to_vendor() {
        assert_eq!(install_path("acme/foo", Some("library"), &empty()), "vendor/acme/foo");
        assert_eq!(install_path("acme/foo", None, &empty()), "vendor/acme/foo");
    }

    #[test]
    fn magento2_module_stays_in_vendor() {
        // magento2-module is not in composer/installers' table.
        assert_eq!(
            install_path("magento/module-catalog", Some("magento2-module"), &empty()),
            "vendor/magento/module-catalog"
        );
    }

    #[test]
    fn builtin_magento_locations() {
        assert_eq!(
            install_path("acme/theme", Some("magento-theme"), &empty()),
            "app/design/frontend/theme"
        );
        assert_eq!(
            install_path("acme/skin", Some("magento-skin"), &empty()),
            "skin/frontend/default/skin"
        );
        assert_eq!(install_path("acme/lib", Some("magento-library"), &empty()), "lib/lib");
    }

    #[test]
    fn installer_paths_override_by_type() {
        let root = json!({
            "extra": { "installer-paths": { "custom/{$name}/": ["type:magento-theme"] } }
        });
        let ip = InstallerPaths::parse(&root);
        assert_eq!(install_path("acme/theme", Some("magento-theme"), &ip), "custom/theme");
    }

    #[test]
    fn installer_paths_override_by_exact_name_and_vendor() {
        let root = json!({
            "extra": { "installer-paths": {
                "byname/{$name}/": ["acme/special"],
                "byvendor/{$vendor}/{$name}/": ["vendor:acme"]
            } }
        });
        let ip = InstallerPaths::parse(&root);
        // Exact name entry comes first in file order → wins for acme/special.
        assert_eq!(install_path("acme/special", Some("library"), &ip), "byname/special");
        // A different acme package falls through to the vendor matcher.
        assert_eq!(install_path("acme/other", Some("library"), &ip), "byvendor/acme/other");
    }

    #[test]
    fn first_match_in_file_order_wins() {
        let root = json!({
            "extra": { "installer-paths": {
                "first/{$name}/": ["type:magento-theme"],
                "second/{$name}/": ["type:magento-theme"]
            } }
        });
        let ip = InstallerPaths::parse(&root);
        assert_eq!(install_path("acme/theme", Some("magento-theme"), &ip), "first/theme");
    }

    #[test]
    fn override_beats_builtin() {
        let root = json!({
            "extra": { "installer-paths": { "app/design/custom/{$name}/": ["type:magento-theme"] } }
        });
        let ip = InstallerPaths::parse(&root);
        assert_eq!(
            install_path("acme/theme", Some("magento-theme"), &ip),
            "app/design/custom/theme"
        );
    }

    #[test]
    fn malformed_extra_is_empty() {
        assert!(InstallerPaths::parse(&json!({})).entries.is_empty());
        assert!(InstallerPaths::parse(&json!({"extra": {"installer-paths": 5}})).entries.is_empty());
    }

    #[test]
    fn builtin_wordpress_drupal_prestashop_locations() {
        assert_eq!(install_path("acme/wp", Some("wordpress-plugin"), &empty()), "wp-content/plugins/wp");
        assert_eq!(install_path("acme/wp", Some("wordpress-muplugin"), &empty()), "wp-content/mu-plugins/wp");
        assert_eq!(install_path("acme/views", Some("drupal-module"), &empty()), "modules/views");
        assert_eq!(install_path("acme/core", Some("drupal-core"), &empty()), "core");
        assert_eq!(install_path("acme/sync", Some("drupal-config"), &empty()), "config/sync");
        // drupal-recipe has no trailing slash upstream.
        assert_eq!(install_path("acme/r", Some("drupal-recipe"), &empty()), "recipes/r");
        assert_eq!(install_path("acme/mod", Some("prestashop-module"), &empty()), "modules/mod");
    }

    #[test]
    fn shopware_name_inflection() {
        // Plugin: ucfirst(vendor) . ucfirst(camelCase(name)).
        assert_eq!(
            install_path("acme/my-plugin", Some("shopware-plugin"), &empty()),
            "custom/plugins/AcmeMyPlugin"
        );
        assert_eq!(
            install_path("acme/my-cool-plugin", Some("shopware-frontend-plugin"), &empty()),
            "engine/Shopware/Plugins/Local/Frontend/AcmeMyCoolPlugin"
        );
        // Theme: hyphens to underscores, no camel/ucfirst.
        assert_eq!(
            install_path("acme/my-theme", Some("shopware-theme"), &empty()),
            "templates/my_theme"
        );
    }

    #[test]
    fn unsupported_framework_detection() {
        // Handled families → None (we relocate them).
        assert_eq!(unsupported_framework(Some("wordpress-plugin")), None);
        assert_eq!(unsupported_framework(Some("magento-theme")), None);
        assert_eq!(unsupported_framework(Some("shopware-plugin")), None);
        // composer/installers frameworks we don't relocate → Some(prefix).
        assert_eq!(unsupported_framework(Some("cakephp-plugin")), Some("cakephp"));
        assert_eq!(unsupported_framework(Some("typo3-cms-extension")), None); // typo3 not in installers
        assert_eq!(unsupported_framework(Some("drupal-unknownsuffix")), Some("drupal"));
        // Ordinary packages → None.
        assert_eq!(unsupported_framework(Some("library")), None);
        assert_eq!(unsupported_framework(Some("composer-plugin")), None);
        assert_eq!(unsupported_framework(None), None);
    }

    #[test]
    fn from_extra_matches_parse() {
        let extra = json!({"installer-paths": {"p/{$name}/": ["type:magento-theme"]}});
        let ip = InstallerPaths::from_extra(&extra);
        assert_eq!(install_path("a/theme", Some("magento-theme"), &ip), "p/theme");
    }

    #[test]
    fn repo_relative_install_path() {
        // Normal vendor package — identical to the old hardcoded form.
        assert_eq!(install_path_relative_to_repo("vendor/acme/foo"), "../acme/foo");
        // Relocated outside vendor.
        assert_eq!(
            install_path_relative_to_repo("app/design/frontend/theme"),
            "../../app/design/frontend/theme"
        );
        // Relocated inside vendor but not at vendor/<name>.
        assert_eq!(install_path_relative_to_repo("vendor/foo/theme"), "../foo/theme");
        // A `composer/*` package is a direct child of the vendor/composer
        // repo dir — Composer emits `./<pkg>`, matching findShortestPath.
        assert_eq!(install_path_relative_to_repo("vendor/composer/installers"), "./installers");
        assert_eq!(install_path_relative_to_repo("vendor/composer/ca-bundle"), "./ca-bundle");
    }
}
