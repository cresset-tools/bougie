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

    // Layer 1: JSON parse
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

    // Layer 2+3: Structural and semantic validation
    validate_name(obj, &mut errors, &mut publish_errors);
    validate_description(obj, &mut publish_errors);
    validate_type(obj, &mut warnings);
    validate_license(obj, &mut warnings);
    validate_version(obj, &mut warnings);
    validate_require(obj, "require", &mut errors, &mut warnings, !opts.no_check_all);
    validate_require(obj, "require-dev", &mut errors, &mut warnings, !opts.no_check_all);
    validate_require_overlap(obj, &mut warnings);
    validate_autoload(obj, &mut errors, &mut warnings);
    validate_repositories(obj, &mut errors);
    validate_minimum_stability(obj, &mut errors);
    validate_bin(obj, &mut errors);
    validate_extra_branch_alias(obj, &mut warnings);
    validate_suggest(obj, &mut errors);

    // Layer 4: Lock file checks
    if !opts.no_check_lock {
        validate_lock(&project_root, &bytes, obj, &mut errors);
    }

    if opts.no_check_publish {
        publish_errors.clear();
    }

    let has_errors = !errors.is_empty() || !publish_errors.is_empty();
    let has_warnings = !warnings.is_empty();
    let exit_code = if has_errors {
        2
    } else if opts.strict && has_warnings {
        1
    } else {
        0
    };

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

// --- Validation checks ---

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
        let vendor = &name[..slash];
        let pkg = &name[slash + 1..];
        for part in [vendor, pkg] {
            let lower = part.to_lowercase();
            if matches!(
                lower.as_str(),
                "nul" | "con" | "prn" | "aux"
                    | "com1" | "com2" | "com3" | "com4" | "com5"
                    | "com6" | "com7" | "com8" | "com9"
                    | "lpt1" | "lpt2" | "lpt3" | "lpt4" | "lpt5"
                    | "lpt6" | "lpt7" | "lpt8" | "lpt9"
            ) {
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

fn validate_description(
    obj: &serde_json::Map<String, Value>,
    publish_errors: &mut Vec<String>,
) {
    if !obj.contains_key("description") {
        publish_errors.push("description is required for publishing".into());
    } else if let Some(val) = obj.get("description") {
        if !val.is_string() {
            publish_errors.push("description must be a string".into());
        }
    }
}

fn validate_type(
    obj: &serde_json::Map<String, Value>,
    warnings: &mut Vec<String>,
) {
    let Some(val) = obj.get("type") else { return };
    let Some(t) = val.as_str() else {
        warnings.push("type must be a string".into());
        return;
    };
    if t == "composer-installer" {
        warnings.push(
            "type `composer-installer` is deprecated; \
             use `composer-plugin` instead"
                .into(),
        );
    }
}

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
        arr.iter()
            .filter_map(|v| v.as_str())
            .collect()
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

fn validate_version(
    obj: &serde_json::Map<String, Value>,
    warnings: &mut Vec<String>,
) {
    if obj.contains_key("version") {
        warnings.push(
            "`version` field is present; this is generally not \
             recommended and should be omitted (Packagist derives \
             versions from tags)"
                .into(),
        );
    }
}

fn validate_require(
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

    let name = obj
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or("");

    for (dep_name, constraint_val) in reqs {
        if dep_name == name && !name.is_empty() {
            errors.push(format!(
                "{key}: package `{dep_name}` requires itself",
            ));
            continue;
        }
        let Some(raw) = constraint_val.as_str() else {
            errors.push(format!(
                "{key}.{dep_name}: constraint must be a string",
            ));
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

        if check_all && !is_platform_name(dep_name) {
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
                errors.push(format!(
                    "{key}: unknown autoload type `{section}`",
                ));
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
                errors.push(format!(
                    "{key}.psr-4 cannot be used with target-dir",
                ));
            }
        }
        if let Some(psr0) = al.get("psr-0").and_then(Value::as_object) {
            for (ns, _) in psr0 {
                if ns.is_empty() {
                    warnings.push(format!(
                        "{key}.psr-0: empty namespace prefix is a \
                         performance concern",
                    ));
                }
            }
        }
    }
}

fn validate_repositories(
    obj: &serde_json::Map<String, Value>,
    errors: &mut Vec<String>,
) {
    let Some(val) = obj.get("repositories") else { return };
    let repos = if let Some(arr) = val.as_array() {
        arr.iter().collect::<Vec<_>>()
    } else if let Some(map) = val.as_object() {
        map.values().collect::<Vec<_>>()
    } else {
        errors.push("`repositories` must be an array or object".into());
        return;
    };
    for repo in repos {
        let Some(repo_obj) = repo.as_object() else {
            continue;
        };
        if !repo_obj.contains_key("type") {
            errors.push(
                "repository entry is missing `type`".into(),
            );
        }
        let has_url = repo_obj.contains_key("url");
        let has_package = repo_obj.contains_key("package");
        if !has_url && !has_package {
            let repo_type = repo_obj
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            if repo_type != "composer" || !repo_obj.contains_key("url") {
                errors.push(
                    "repository entry is missing `url`".into(),
                );
            }
        }
    }
}

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
            errors.push(format!(
                "suggest.{pkg}: description must be a string",
            ));
        }
    }
}

fn validate_lock(
    project_root: &Path,
    composer_json_bytes: &[u8],
    _obj: &serde_json::Map<String, Value>,
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
}

fn is_valid_package_name(name: &str) -> bool {
    let Some((vendor, pkg)) = name.split_once('/') else {
        return false;
    };
    if vendor.is_empty() || pkg.is_empty() {
        return false;
    }
    let valid_part = |s: &str| -> bool {
        let bytes = s.as_bytes();
        if !bytes[0].is_ascii_lowercase() && !bytes[0].is_ascii_digit() {
            return false;
        }
        let mut prev_sep = false;
        for &b in &bytes[1..] {
            if b.is_ascii_lowercase() || b.is_ascii_digit() {
                prev_sep = false;
            } else if b == b'_' || b == b'.' || b == b'-' {
                if prev_sep && b != b'-' {
                    return false;
                }
                prev_sep = b != b'-' || prev_sep;
            } else {
                return false;
            }
        }
        bytes.last().is_some_and(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
    };
    valid_part(vendor) && valid_part(pkg)
}

fn is_platform_name(name: &str) -> bool {
    name == "php"
        || name.starts_with("ext-")
        || name.starts_with("lib-")
        || name == "composer-plugin-api"
        || name == "composer-runtime-api"
}
