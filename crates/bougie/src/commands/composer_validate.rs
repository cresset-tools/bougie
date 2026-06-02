//! `bougie composer validate` — validate composer.json structure
//! and contents. Matches Composer 2.8.12's `ValidateCommand` checks.

use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use bougie_cli::OutputFormat;
use bougie_composer::lockfile::{self, Lock};
use bougie_output::output::{emit, Render};
use bougie_semver::constraint::Constraint;
use eyre::{Context, Result};
use serde::Serialize;
use serde_json::Value;

#[derive(Debug, Clone, Copy)]
pub struct ValidateOptions {
    pub strict: bool,
    pub no_check_lock: bool,
    pub no_check_publish: bool,
    pub no_check_all: bool,
    pub with_dependencies: bool,
}

#[derive(Debug, Serialize)]
pub struct ValidateResult {
    pub schema_version: u32,
    pub valid: bool,
    pub errors: Vec<String>,
    pub warnings: Vec<String>,
    pub publish_errors: Vec<String>,
}

impl Render for ValidateResult {
    fn render_text(&self, w: &mut dyn Write) -> io::Result<()> {
        if !self.publish_errors.is_empty() {
            writeln!(w, "Publish errors:")?;
            for e in &self.publish_errors {
                writeln!(w, "  - {e}")?;
            }
        }
        if !self.errors.is_empty() {
            writeln!(w, "Errors:")?;
            for e in &self.errors {
                writeln!(w, "  - {e}")?;
            }
        }
        if !self.warnings.is_empty() {
            writeln!(w, "Warnings:")?;
            for w_msg in &self.warnings {
                writeln!(w, "  - {w_msg}")?;
            }
        }
        if self.errors.is_empty() && self.publish_errors.is_empty() && self.warnings.is_empty() {
            writeln!(w, "./composer.json is valid")?;
        } else if self.errors.is_empty() && self.publish_errors.is_empty() {
            writeln!(w, "./composer.json is valid for publishing (warnings only)")?;
        }
        Ok(())
    }
}

pub fn run(
    format: OutputFormat,
    working_dir: Option<PathBuf>,
    opts: ValidateOptions,
) -> Result<ExitCode> {
    let project_root = match working_dir {
        Some(p) => p,
        None => std::env::current_dir().wrap_err("reading current directory")?,
    };
    let composer_json_path = project_root.join("composer.json");
    if !composer_json_path.is_file() {
        let result = ValidateResult {
            schema_version: 1,
            valid: false,
            errors: vec![format!("{} not found", composer_json_path.display())],
            warnings: vec![],
            publish_errors: vec![],
        };
        emit(format, &result)?;
        return Ok(ExitCode::from(3));
    }

    let bytes = std::fs::read(&composer_json_path)
        .wrap_err_with(|| format!("reading {}", composer_json_path.display()))?;

    let mut errors: Vec<String> = Vec::new();
    let mut warnings: Vec<String> = Vec::new();
    let mut publish_errors: Vec<String> = Vec::new();

    detect_duplicate_keys(&bytes, &mut warnings);

    let value: Value = match serde_json::from_slice(&bytes) {
        Ok(v) => v,
        Err(e) => {
            errors.push(format!("composer.json is not valid JSON: {e}"));
            let result = ValidateResult {
                schema_version: 1,
                valid: false,
                errors,
                warnings,
                publish_errors,
            };
            emit(format, &result)?;
            return Ok(ExitCode::from(2));
        }
    };

    let Some(obj) = value.as_object() else {
        errors.push("composer.json must be a JSON object".into());
        let result = ValidateResult {
            schema_version: 1,
            valid: false,
            errors,
            warnings,
            publish_errors,
        };
        emit(format, &result)?;
        return Ok(ExitCode::from(2));
    };

    validate_manifest(obj, &mut errors, &mut warnings, &mut publish_errors, !opts.no_check_all);

    if !opts.no_check_lock {
        validate_lock(&project_root, &bytes, obj, &mut errors);
    }

    if opts.no_check_publish {
        publish_errors.clear();
    }

    let has_errors = !errors.is_empty() || !publish_errors.is_empty();
    let has_warnings = !warnings.is_empty();
    let mut exit_code: u8 = if has_errors {
        2
    } else if opts.strict && has_warnings {
        1
    } else {
        0
    };

    if opts.with_dependencies {
        let lock_path = project_root.join("composer.lock");
        if let Ok(lock) = Lock::read(&lock_path) {
            for pkg in lock.all_packages() {
                let pkg_path = project_root
                    .join("vendor")
                    .join(&pkg.name)
                    .join("composer.json");
                if !pkg_path.is_file() {
                    continue;
                }
                let Ok(pkg_bytes) = std::fs::read(&pkg_path) else {
                    continue;
                };
                let mut dep_errors: Vec<String> = Vec::new();
                let mut dep_warnings: Vec<String> = Vec::new();
                let mut dep_publish: Vec<String> = Vec::new();

                detect_duplicate_keys(&pkg_bytes, &mut dep_warnings);

                if let Ok(pkg_value) = serde_json::from_slice::<Value>(&pkg_bytes) {
                    if let Some(pkg_obj) = pkg_value.as_object() {
                        validate_manifest(
                            pkg_obj,
                            &mut dep_errors,
                            &mut dep_warnings,
                            &mut dep_publish,
                            !opts.no_check_all,
                        );
                    }
                }

                if opts.no_check_publish {
                    dep_publish.clear();
                }

                let dep_has_errors = !dep_errors.is_empty() || !dep_publish.is_empty();
                let dep_has_warnings = !dep_warnings.is_empty();
                let dep_code: u8 = if dep_has_errors {
                    2
                } else if opts.strict && dep_has_warnings {
                    1
                } else {
                    0
                };

                if dep_code > 0 {
                    for e in &dep_publish {
                        publish_errors.push(format!("{}: {e}", pkg.name));
                    }
                    for e in &dep_errors {
                        errors.push(format!("{}: {e}", pkg.name));
                    }
                    for w in &dep_warnings {
                        warnings.push(format!("{}: {w}", pkg.name));
                    }
                }

                exit_code = exit_code.max(dep_code);
            }
        }
    }

    let result = ValidateResult {
        schema_version: 1,
        valid: exit_code == 0,
        errors,
        warnings,
        publish_errors,
    };
    emit(format, &result)?;
    Ok(ExitCode::from(exit_code))
}

fn validate_manifest(
    obj: &serde_json::Map<String, Value>,
    errors: &mut Vec<String>,
    warnings: &mut Vec<String>,
    publish_errors: &mut Vec<String>,
    check_all: bool,
) {
    validate_name(obj, errors, publish_errors);
    validate_description(obj, errors, publish_errors);
    validate_type(obj, errors, warnings);
    validate_license(obj, warnings);
    validate_version(obj, warnings);
    validate_keywords(obj, errors, warnings);
    validate_homepage(obj, warnings);
    validate_time(obj, errors);
    validate_authors(obj, errors, warnings);
    validate_support(obj, errors, warnings);
    validate_funding(obj, errors, warnings);
    for key in ["require", "require-dev", "conflict", "replace", "provide"] {
        validate_link_section(obj, key, errors, warnings, check_all);
    }
    validate_require_overlap(obj, warnings);
    validate_provide_replace_overlap(obj, warnings);
    validate_conflict_replace_overlap(obj, errors);
    validate_autoload(obj, errors, warnings);
    validate_repositories(obj, errors);
    validate_minimum_stability(obj, errors);
    validate_bin(obj, errors);
    validate_suggest(obj, errors);
    validate_extra_branch_alias(obj, warnings);
    validate_scripts(obj, errors, warnings);
    validate_extra(obj, errors);
    validate_target_dir(obj, errors);
    validate_include_path(obj, errors);
    validate_transport_options(obj, errors);
    validate_config_platform(obj, errors);
}

// --- Name ---

fn validate_name(
    obj: &serde_json::Map<String, Value>,
    errors: &mut Vec<String>,
    publish_errors: &mut Vec<String>,
) {
    let Some(name_val) = obj.get("name") else {
        publish_errors.push("name is required for publishing".into());
        return;
    };
    let Some(name) = name_val.as_str() else {
        errors.push("name must be a string".into());
        return;
    };
    if name.is_empty() {
        errors.push("name must not be empty".into());
        return;
    }

    if name != name.to_lowercase() {
        publish_errors.push(format!(
            "name `{name}` contains uppercase characters; \
             use `{}` instead",
            name.to_lowercase().replace(' ', "-"),
        ));
    }

    if !is_valid_package_name(&name.to_lowercase()) {
        errors.push(format!(
            "name `{name}` is invalid; it must match \
             `vendor-name/package-name` using only lowercase \
             alphanumerics, `-`, `.`, `_`",
        ));
    }

    if let Some(slash) = name.find('/') {
        for part in [&name[..slash], &name[slash + 1..]] {
            if is_windows_reserved(&part.to_lowercase()) {
                errors.push(format!(
                    "name `{name}` contains reserved Windows name `{part}`",
                ));
            }
        }
    }

    if name.ends_with(".json") {
        errors.push(format!("name `{name}` must not end in `.json`"));
    }
}

// --- Description ---

fn validate_description(
    obj: &serde_json::Map<String, Value>,
    errors: &mut Vec<String>,
    publish_errors: &mut Vec<String>,
) {
    let Some(val) = obj.get("description") else {
        publish_errors.push("description is required for publishing".into());
        return;
    };
    if !val.is_string() {
        errors.push("description must be a string".into());
    }
}

// --- Type ---

fn validate_type(
    obj: &serde_json::Map<String, Value>,
    errors: &mut Vec<String>,
    warnings: &mut Vec<String>,
) {
    let Some(val) = obj.get("type") else { return };
    let Some(t) = val.as_str() else {
        errors.push("type must be a string".into());
        return;
    };
    if t == "composer-installer" {
        warnings.push(
            "type `composer-installer` is deprecated; \
             use `composer-plugin` instead"
                .into(),
        );
    }
    if !t.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
        warnings.push(format!(
            "type `{t}` should only contain lowercase alphanumerics and hyphens",
        ));
    }
}

// --- License ---

fn validate_license(
    obj: &serde_json::Map<String, Value>,
    warnings: &mut Vec<String>,
) {
    let Some(val) = obj.get("license") else {
        warnings.push("no license specified".into());
        return;
    };

    let licenses: Vec<&str> = if let Some(s) = val.as_str() {
        vec![s]
    } else if let Some(arr) = val.as_array() {
        let mut out = Vec::new();
        for v in arr {
            let Some(s) = v.as_str() else {
                warnings.push("license array entries must be strings".into());
                return;
            };
            out.push(s);
        }
        out
    } else {
        warnings.push("license must be a string or array of strings".into());
        return;
    };

    for license in &licenses {
        let trimmed = license.trim();
        if trimmed != *license {
            warnings.push(format!(
                "license `{license}` has leading or trailing whitespace",
            ));
        }
        if trimmed == "proprietary" {
            continue;
        }
        match spdx::Expression::parse(trimmed) {
            Ok(expr) => {
                for req in expr.requirements() {
                    if let Some(id) = req.req.license.id() {
                        if id.is_deprecated() {
                            warnings.push(format!(
                                "license `{}` is deprecated; \
                                 see https://spdx.org/licenses/",
                                id.name,
                            ));
                        }
                    }
                }
            }
            Err(_) => {
                warnings.push(format!(
                    "license `{trimmed}` is not a valid SPDX license identifier; \
                     see https://spdx.org/licenses/",
                ));
            }
        }
    }
}

// --- Version ---

fn validate_version(
    obj: &serde_json::Map<String, Value>,
    warnings: &mut Vec<String>,
) {
    let Some(val) = obj.get("version") else { return };
    if !val.is_string() {
        warnings.push("`version` must be a string".into());
        return;
    }
    warnings.push(
        "`version` field is present; this is generally not \
         recommended and should be omitted (Packagist derives \
         versions from tags)"
            .into(),
    );
}

// --- Keywords ---

fn validate_keywords(
    obj: &serde_json::Map<String, Value>,
    errors: &mut Vec<String>,
    warnings: &mut Vec<String>,
) {
    let Some(val) = obj.get("keywords") else { return };
    let Some(arr) = val.as_array() else {
        errors.push("`keywords` must be an array".into());
        return;
    };
    for entry in arr {
        let Some(kw) = entry.as_str() else {
            errors.push("keyword entries must be strings".into());
            continue;
        };
        if !kw
            .chars()
            .all(|c| c.is_alphanumeric() || c == ' ' || c == '.' || c == '_' || c == '-')
        {
            warnings.push(format!(
                "keyword `{kw}` contains invalid characters; \
                 use only alphanumerics, spaces, `.`, `_`, `-`",
            ));
        }
    }
}

// --- Homepage ---

fn validate_homepage(
    obj: &serde_json::Map<String, Value>,
    warnings: &mut Vec<String>,
) {
    let Some(val) = obj.get("homepage") else { return };
    let Some(url) = val.as_str() else {
        warnings.push("homepage must be a string".into());
        return;
    };
    if !is_http_url(url) {
        warnings.push(format!(
            "homepage `{url}` should be an http:// or https:// URL",
        ));
    }
}

// --- Time ---

fn validate_time(
    obj: &serde_json::Map<String, Value>,
    errors: &mut Vec<String>,
) {
    let Some(val) = obj.get("time") else { return };
    let Some(t) = val.as_str() else {
        errors.push("time must be a string".into());
        return;
    };
    if !looks_like_datetime(t) {
        errors.push(format!(
            "time `{t}` is not a valid datetime",
        ));
    }
}

// --- Authors ---

fn validate_authors(
    obj: &serde_json::Map<String, Value>,
    errors: &mut Vec<String>,
    warnings: &mut Vec<String>,
) {
    let Some(val) = obj.get("authors") else { return };
    let Some(arr) = val.as_array() else {
        errors.push("`authors` must be an array".into());
        return;
    };
    for (i, entry) in arr.iter().enumerate() {
        let Some(author) = entry.as_object() else {
            errors.push(format!("authors[{i}] must be an object"));
            continue;
        };
        for field in ["name", "email", "homepage", "role"] {
            if let Some(v) = author.get(field) {
                if !v.is_string() {
                    errors.push(format!("authors[{i}].{field} must be a string"));
                }
            }
        }
        if let Some(hp) = author.get("homepage").and_then(Value::as_str) {
            if !is_http_url(hp) {
                warnings.push(format!(
                    "authors[{i}].homepage `{hp}` should be an http:// or https:// URL",
                ));
            }
        }
        if let Some(email) = author.get("email").and_then(Value::as_str) {
            if !looks_like_email(email) {
                warnings.push(format!(
                    "authors[{i}].email `{email}` does not look like a valid email",
                ));
            }
        }
    }
}

// --- Support ---

fn validate_support(
    obj: &serde_json::Map<String, Value>,
    errors: &mut Vec<String>,
    warnings: &mut Vec<String>,
) {
    let Some(val) = obj.get("support") else { return };
    let Some(support) = val.as_object() else {
        errors.push("`support` must be an object".into());
        return;
    };
    let url_fields = [
        "issues", "forum", "wiki", "source", "docs", "chat", "security",
    ];
    for (key, v) in support {
        if !v.is_string() {
            errors.push(format!("support.{key} must be a string"));
            continue;
        }
        let s = v.as_str().unwrap_or("");
        if key == "email" && !looks_like_email(s) {
            warnings.push(format!(
                "support.email `{s}` does not look like a valid email",
            ));
        } else if key == "irc" && !s.starts_with("irc://") && !s.starts_with("ircs://") {
            warnings.push(format!(
                "support.irc `{s}` should be an irc:// or ircs:// URL",
            ));
        } else if url_fields.contains(&key.as_str()) && !is_http_url(s) {
            warnings.push(format!(
                "support.{key} `{s}` should be an http:// or https:// URL",
            ));
        }
    }
}

// --- Funding ---

fn validate_funding(
    obj: &serde_json::Map<String, Value>,
    errors: &mut Vec<String>,
    warnings: &mut Vec<String>,
) {
    let Some(val) = obj.get("funding") else { return };
    let Some(arr) = val.as_array() else {
        errors.push("`funding` must be an array".into());
        return;
    };
    for (i, entry) in arr.iter().enumerate() {
        let Some(fund) = entry.as_object() else {
            errors.push(format!("funding[{i}] must be an object"));
            continue;
        };
        for field in ["type", "url"] {
            if let Some(v) = fund.get(field) {
                if !v.is_string() {
                    errors.push(format!("funding[{i}].{field} must be a string"));
                }
            }
        }
        if let Some(url) = fund.get("url").and_then(Value::as_str) {
            if !is_http_url(url) {
                warnings.push(format!(
                    "funding[{i}].url `{url}` should be an http:// or https:// URL",
                ));
            }
        }
    }
}

// --- Link sections (require, require-dev, conflict, replace, provide) ---

fn validate_link_section(
    obj: &serde_json::Map<String, Value>,
    key: &str,
    errors: &mut Vec<String>,
    warnings: &mut Vec<String>,
    check_all: bool,
) {
    let Some(val) = obj.get(key) else { return };
    let Some(reqs) = val.as_object() else {
        errors.push(format!("`{key}` must be a JSON object"));
        return;
    };

    let name = obj.get("name").and_then(Value::as_str).unwrap_or("");

    for (dep_name, constraint_val) in reqs {
        if dep_name == name && !name.is_empty() {
            errors.push(format!("{key}: package `{dep_name}` requires itself"));
            continue;
        }
        if dep_name.contains(|c: char| {
            !c.is_ascii_alphanumeric() && c != '/' && c != '_' && c != '.' && c != '-'
        }) {
            errors.push(format!(
                "{key}.{dep_name}: package name contains invalid characters",
            ));
        }
        let Some(raw) = constraint_val.as_str() else {
            errors.push(format!("{key}.{dep_name}: constraint must be a string"));
            continue;
        };
        if raw.contains('#') {
            warnings.push(format!(
                "{key}.{dep_name}: constraint `{raw}` contains a \
                 commit hash reference; this is fragile",
            ));
        }
        let cleaned = raw.split('@').next().unwrap_or(raw);
        if Constraint::parse(cleaned).is_err() {
            errors.push(format!(
                "{key}.{dep_name}: constraint `{raw}` is not a valid \
                 version constraint",
            ));
            continue;
        }

        if check_all
            && (key == "require" || key == "require-dev")
            && !is_platform_name(dep_name)
        {
            if raw == "*" || raw == ">=0" || raw.starts_with(">=0.") {
                warnings.push(format!(
                    "{key}.{dep_name}: constraint `{raw}` is unbound; \
                     consider adding an upper bound",
                ));
            }
        }
    }
}

fn validate_require_overlap(
    obj: &serde_json::Map<String, Value>,
    warnings: &mut Vec<String>,
) {
    let req = obj.get("require").and_then(Value::as_object);
    let req_dev = obj.get("require-dev").and_then(Value::as_object);
    let (Some(req), Some(req_dev)) = (req, req_dev) else { return };
    for key in req.keys() {
        if req_dev.contains_key(key) {
            warnings.push(format!(
                "`{key}` appears in both require and require-dev",
            ));
        }
    }
}

fn validate_provide_replace_overlap(
    obj: &serde_json::Map<String, Value>,
    warnings: &mut Vec<String>,
) {
    for section in ["provide", "replace"] {
        let Some(map) = obj.get(section).and_then(Value::as_object) else {
            continue;
        };
        for link_key in ["require", "require-dev"] {
            let Some(reqs) = obj.get(link_key).and_then(Value::as_object) else {
                continue;
            };
            for key in map.keys() {
                if reqs.contains_key(key) {
                    warnings.push(format!(
                        "`{key}` appears in both {section} and {link_key}",
                    ));
                }
            }
        }
    }
}

fn validate_conflict_replace_overlap(
    obj: &serde_json::Map<String, Value>,
    errors: &mut Vec<String>,
) {
    let conflict = obj.get("conflict").and_then(Value::as_object);
    let replace = obj.get("replace").and_then(Value::as_object);
    let (Some(conflict), Some(replace)) = (conflict, replace) else { return };
    for key in conflict.keys() {
        if replace.contains_key(key) {
            errors.push(format!(
                "`{key}` appears in both conflict and replace",
            ));
        }
    }
}

// --- Autoload ---

fn validate_autoload(
    obj: &serde_json::Map<String, Value>,
    errors: &mut Vec<String>,
    warnings: &mut Vec<String>,
) {
    for key in ["autoload", "autoload-dev"] {
        let Some(val) = obj.get(key) else { continue };
        let Some(al) = val.as_object() else {
            errors.push(format!("`{key}` must be a JSON object"));
            continue;
        };
        for (section, _) in al {
            if !matches!(
                section.as_str(),
                "psr-0" | "psr-4" | "classmap" | "files" | "exclude-from-classmap"
            ) {
                errors.push(format!("{key}: unknown autoload type `{section}`"));
            }
        }
        if let Some(psr4) = al.get("psr-4").and_then(Value::as_object) {
            for (ns, _) in psr4 {
                if !ns.is_empty() && !ns.ends_with('\\') {
                    errors.push(format!(
                        "{key}.psr-4: namespace `{ns}` must end with `\\\\`",
                    ));
                }
            }
            if obj.contains_key("target-dir") {
                errors.push(format!("{key}.psr-4 cannot be used with target-dir"));
            }
        }
        if let Some(psr0) = al.get("psr-0").and_then(Value::as_object) {
            for (ns, _) in psr0 {
                if ns.is_empty() {
                    warnings.push(format!(
                        "{key}.psr-0: empty namespace prefix is a performance concern",
                    ));
                }
            }
        }
    }
}

// --- Repositories ---

fn validate_repositories(
    obj: &serde_json::Map<String, Value>,
    errors: &mut Vec<String>,
) {
    let Some(val) = obj.get("repositories") else { return };
    let repos: Vec<&Value> = if let Some(arr) = val.as_array() {
        arr.iter().collect()
    } else if let Some(map) = val.as_object() {
        map.values().collect()
    } else {
        errors.push("`repositories` must be an array or object".into());
        return;
    };
    for repo in repos {
        let Some(repo_obj) = repo.as_object() else {
            continue;
        };
        if !repo_obj.contains_key("type") {
            errors.push("repository entry is missing `type`".into());
        }
        if !repo_obj.contains_key("url") && !repo_obj.contains_key("package") {
            errors.push("repository entry is missing `url`".into());
        }
    }
}

// --- minimum-stability ---

fn validate_minimum_stability(
    obj: &serde_json::Map<String, Value>,
    errors: &mut Vec<String>,
) {
    let Some(val) = obj.get("minimum-stability") else { return };
    let Some(s) = val.as_str() else {
        errors.push("minimum-stability must be a string".into());
        return;
    };
    if !matches!(
        s.to_lowercase().as_str(),
        "dev" | "alpha" | "beta" | "rc" | "stable"
    ) {
        errors.push(format!(
            "minimum-stability `{s}` is invalid; must be one of: \
             dev, alpha, beta, RC, stable",
        ));
    }
}

// --- bin ---

fn validate_bin(
    obj: &serde_json::Map<String, Value>,
    errors: &mut Vec<String>,
) {
    let Some(val) = obj.get("bin") else { return };
    if val.is_string() {
        return;
    }
    let Some(arr) = val.as_array() else {
        errors.push("`bin` must be a string or array of strings".into());
        return;
    };
    for entry in arr {
        if !entry.is_string() {
            errors.push("`bin` entries must be strings".into());
            return;
        }
    }
}

// --- suggest ---

fn validate_suggest(
    obj: &serde_json::Map<String, Value>,
    errors: &mut Vec<String>,
) {
    let Some(val) = obj.get("suggest") else { return };
    let Some(suggest) = val.as_object() else {
        errors.push("`suggest` must be a JSON object".into());
        return;
    };
    for (pkg, desc) in suggest {
        if !desc.is_string() {
            errors.push(format!("suggest.{pkg}: description must be a string"));
        }
    }
}

// --- extra.branch-alias ---

fn validate_extra_branch_alias(
    obj: &serde_json::Map<String, Value>,
    warnings: &mut Vec<String>,
) {
    let aliases = obj
        .get("extra")
        .and_then(Value::as_object)
        .and_then(|e| e.get("branch-alias"))
        .and_then(Value::as_object);
    let Some(aliases) = aliases else { return };
    for (branch, target) in aliases {
        let Some(target_str) = target.as_str() else {
            warnings.push(format!(
                "extra.branch-alias.{branch}: alias must be a string",
            ));
            continue;
        };
        if !target_str.ends_with("-dev") && !target_str.starts_with("dev-") {
            warnings.push(format!(
                "extra.branch-alias.{branch}: alias `{target_str}` \
                 should end with `-dev`",
            ));
        }
    }
}

// --- scripts ---

fn validate_scripts(
    obj: &serde_json::Map<String, Value>,
    errors: &mut Vec<String>,
    warnings: &mut Vec<String>,
) {
    let Some(val) = obj.get("scripts") else { return };
    if !val.is_object() {
        errors.push("`scripts` must be an object".into());
        return;
    }
    if let Some(descs) = obj.get("scripts-descriptions").and_then(Value::as_object) {
        let scripts = val.as_object().unwrap();
        for key in descs.keys() {
            if !scripts.contains_key(key) {
                warnings.push(format!(
                    "scripts-descriptions.{key} references non-existent script",
                ));
            }
        }
    }
    if let Some(aliases) = obj.get("scripts-aliases").and_then(Value::as_object) {
        let scripts = val.as_object().unwrap();
        for key in aliases.keys() {
            if !scripts.contains_key(key) {
                warnings.push(format!(
                    "scripts-aliases.{key} references non-existent script",
                ));
            }
        }
    }
}

// --- extra ---

fn validate_extra(
    obj: &serde_json::Map<String, Value>,
    errors: &mut Vec<String>,
) {
    let Some(val) = obj.get("extra") else { return };
    if !val.is_object() {
        errors.push("`extra` must be an object".into());
    }
}

// --- target-dir ---

fn validate_target_dir(
    obj: &serde_json::Map<String, Value>,
    errors: &mut Vec<String>,
) {
    let Some(val) = obj.get("target-dir") else { return };
    if !val.is_string() {
        errors.push("`target-dir` must be a string".into());
    }
}

// --- include-path ---

fn validate_include_path(
    obj: &serde_json::Map<String, Value>,
    errors: &mut Vec<String>,
) {
    let Some(val) = obj.get("include-path") else { return };
    let Some(arr) = val.as_array() else {
        errors.push("`include-path` must be an array".into());
        return;
    };
    for entry in arr {
        if !entry.is_string() {
            errors.push("`include-path` entries must be strings".into());
            return;
        }
    }
}

// --- transport-options ---

fn validate_transport_options(
    obj: &serde_json::Map<String, Value>,
    errors: &mut Vec<String>,
) {
    let Some(val) = obj.get("transport-options") else { return };
    if !val.is_object() {
        errors.push("`transport-options` must be an object".into());
    }
}

// --- config.platform ---

fn validate_config_platform(
    obj: &serde_json::Map<String, Value>,
    errors: &mut Vec<String>,
) {
    let platform = obj
        .get("config")
        .and_then(Value::as_object)
        .and_then(|c| c.get("platform"))
        .and_then(Value::as_object);
    let Some(platform) = platform else { return };
    for (key, val) in platform {
        if val.is_boolean() && !val.as_bool().unwrap_or(true) {
            continue;
        }
        let Some(ver_str) = val.as_str() else {
            errors.push(format!(
                "config.platform.{key} must be a version string or false",
            ));
            continue;
        };
        if bougie_semver::version::Version::parse(ver_str).is_err() {
            errors.push(format!(
                "config.platform.{key}: `{ver_str}` is not a valid version",
            ));
        }
    }
}

// --- Lock file ---

fn validate_lock(
    project_root: &Path,
    composer_json_bytes: &[u8],
    obj: &serde_json::Map<String, Value>,
    errors: &mut Vec<String>,
) {
    let lock_path = project_root.join("composer.lock");
    if !lock_path.exists() {
        errors.push(
            "composer.lock is not present; run `bougie composer update` \
             to generate it"
                .into(),
        );
        return;
    }
    let lock = match Lock::read(&lock_path) {
        Ok(l) => l,
        Err(e) => {
            errors.push(format!("composer.lock is not valid: {e}"));
            return;
        }
    };

    if let Some(expected) = &lock.content_hash {
        match lockfile::content_hash(composer_json_bytes) {
            Ok(actual) => {
                if !actual.eq_ignore_ascii_case(expected) {
                    errors.push(format!(
                        "composer.lock is not up to date with composer.json \
                         (content-hash {expected} → {actual}); \
                         run `bougie composer update` to regenerate",
                    ));
                }
            }
            Err(e) => {
                errors.push(format!("could not compute content-hash: {e}"));
            }
        }
    }

    validate_lock_requirements(obj, &lock, errors);
}

fn validate_lock_requirements(
    obj: &serde_json::Map<String, Value>,
    lock: &Lock,
    errors: &mut Vec<String>,
) {
    for (section, lock_packages) in [
        ("require", &lock.packages),
        ("require-dev", &lock.packages_dev),
    ] {
        let Some(reqs) = obj.get(section).and_then(Value::as_object) else {
            continue;
        };
        for (dep_name, constraint_val) in reqs {
            if is_platform_name(dep_name) {
                continue;
            }
            let Some(raw) = constraint_val.as_str() else {
                continue;
            };
            let cleaned = raw.split('@').next().unwrap_or(raw);
            let Ok(constraint) = Constraint::parse(cleaned) else {
                continue;
            };
            let locked_pkg = lock_packages
                .iter()
                .find(|p| p.name == *dep_name);
            let Some(pkg) = locked_pkg else {
                errors.push(format!(
                    "{section}.{dep_name}: required but not present in composer.lock",
                ));
                continue;
            };
            if let Ok(ver) = bougie_semver::version::Version::parse(&pkg.version) {
                if !constraint.matches(&ver) {
                    errors.push(format!(
                        "{section}.{dep_name}: locked version {} does not \
                         satisfy constraint `{raw}`",
                        pkg.version,
                    ));
                }
            }
        }
    }
}

// --- Helpers ---

fn detect_duplicate_keys(bytes: &[u8], warnings: &mut Vec<String>) {
    use std::collections::HashMap;

    let Ok(text) = std::str::from_utf8(bytes) else { return };
    let mut depth: Vec<HashMap<String, u32>> = Vec::new();
    let mut in_string = false;
    let mut escape = false;
    let mut current_key = String::new();
    let mut collecting_key = false;
    let mut expect_key = false;
    let mut after_colon = false;

    let chars: Vec<char> = text.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if in_string {
            if escape {
                if collecting_key {
                    current_key.push(c);
                }
                escape = false;
            } else if c == '\\' {
                escape = true;
                if collecting_key {
                    current_key.push(c);
                }
            } else if c == '"' {
                in_string = false;
                if collecting_key {
                    collecting_key = false;
                    if let Some(level) = depth.last_mut() {
                        let count = level.entry(current_key.clone()).or_insert(0);
                        *count += 1;
                        if *count == 2 {
                            warnings.push(format!(
                                "key `{current_key}` is a duplicate in composer.json",
                            ));
                        }
                    }
                }
            } else if collecting_key {
                current_key.push(c);
            }
        } else {
            match c {
                '"' => {
                    in_string = true;
                    if expect_key && !after_colon {
                        collecting_key = true;
                        current_key.clear();
                    } else {
                        collecting_key = false;
                    }
                    after_colon = false;
                }
                '{' => {
                    depth.push(HashMap::new());
                    expect_key = true;
                    after_colon = false;
                }
                '}' => {
                    depth.pop();
                    expect_key = false;
                    after_colon = false;
                }
                ':' => {
                    after_colon = true;
                    expect_key = false;
                }
                ',' => {
                    expect_key = !depth.is_empty();
                    after_colon = false;
                }
                '[' | ']' => {
                    after_colon = false;
                }
                _ => {}
            }
        }
        i += 1;
    }
}

pub(crate) fn is_valid_package_name(name: &str) -> bool {
    let Some((vendor, pkg)) = name.split_once('/') else {
        return false;
    };
    if vendor.is_empty() || pkg.is_empty() {
        return false;
    }
    fn valid_part(s: &str) -> bool {
        let bytes = s.as_bytes();
        if !bytes[0].is_ascii_lowercase() && !bytes[0].is_ascii_digit() {
            return false;
        }
        let last = *bytes.last().unwrap();
        if !last.is_ascii_lowercase() && !last.is_ascii_digit() {
            return false;
        }
        for &b in &bytes[1..] {
            if b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'.' || b == b'-'
            {
                continue;
            }
            return false;
        }
        true
    }
    valid_part(vendor) && valid_part(pkg)
}

fn is_windows_reserved(name: &str) -> bool {
    matches!(
        name,
        "nul" | "con" | "prn" | "aux"
            | "com1" | "com2" | "com3" | "com4" | "com5"
            | "com6" | "com7" | "com8" | "com9"
            | "lpt1" | "lpt2" | "lpt3" | "lpt4" | "lpt5"
            | "lpt6" | "lpt7" | "lpt8" | "lpt9"
    )
}

fn is_platform_name(name: &str) -> bool {
    name == "php"
        || name.starts_with("ext-")
        || name.starts_with("lib-")
        || name == "composer-plugin-api"
        || name == "composer-runtime-api"
}

fn is_http_url(s: &str) -> bool {
    s.starts_with("http://") || s.starts_with("https://")
}

fn looks_like_email(s: &str) -> bool {
    let parts: Vec<&str> = s.split('@').collect();
    parts.len() == 2 && !parts[0].is_empty() && parts[1].contains('.')
}

fn looks_like_datetime(s: &str) -> bool {
    // Accept ISO 8601 / RFC 3339 patterns: YYYY-MM-DD or YYYY-MM-DDTHH:MM:SS
    if s.len() < 10 {
        return false;
    }
    let date_part = &s[..10];
    let bytes = date_part.as_bytes();
    bytes[4] == b'-' && bytes[7] == b'-'
        && bytes[..4].iter().all(u8::is_ascii_digit)
        && bytes[5..7].iter().all(u8::is_ascii_digit)
        && bytes[8..10].iter().all(u8::is_ascii_digit)
}
