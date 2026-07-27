//! `bougie version` — read or set the project's own version in
//! `composer.json` (uv's `uv version`).
//!
//! Composer projects usually **omit** `version`: Packagist derives it
//! from the VCS tag, and a hardcoded field that drifts from the tag is a
//! well-known footgun. So a missing version is reported as a fact, not an
//! error, and the field is only written when explicitly asked for.
//!
//! Distinct from `bougie self version`, which reports the bougie binary's
//! own version.

use std::io::{self, Write};
use std::path::PathBuf;
use std::process::ExitCode;

use bougie_cli::{OutputFormat, VersionBump};
use bougie_composer::lockfile::{read_json_file, write_json_file};
use bougie_output::output::{emit, Render};
use composer_semver::version::{Version, VersionKind};
use eyre::{eyre, Context, Result};
use serde::Serialize;
use serde_json::{Map, Value};

#[derive(Debug, Serialize)]
pub struct VersionResult {
    pub schema_version: u32,
    /// The package name from `composer.json`, when it declares one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// The version before this command ran; `None` when the field was
    /// absent.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub previous: Option<String>,
    /// The version after this command ran; `None` when reading a project
    /// that declares none.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current: Option<String>,
    /// Whether `composer.json` was (or, under `--dry-run`, would be)
    /// changed.
    pub changed: bool,
    pub dry_run: bool,
    /// Bare-version output was requested; affects text rendering only.
    #[serde(skip)]
    pub short: bool,
}

impl Render for VersionResult {
    fn render_text(&self, w: &mut dyn Write) -> io::Result<()> {
        let Some(current) = self.current.as_deref() else {
            // Reading a project that declares no version. Say so plainly
            // and point at the fix; `--short` stays silent so it can be
            // captured into a shell variable without noise.
            if self.short {
                return Ok(());
            }
            return writeln!(
                w,
                "no version set in composer.json — pass a VERSION or --bump to set one"
            );
        };
        if self.short {
            return writeln!(w, "{current}");
        }

        let label = self.name.as_deref().unwrap_or("project");
        if !self.changed {
            return writeln!(w, "{label} {current}");
        }
        let suffix = if self.dry_run {
            " (dry run, composer.json unchanged)"
        } else {
            ""
        };
        match self.previous.as_deref() {
            Some(previous) => writeln!(w, "{label} {previous} => {current}{suffix}"),
            None => writeln!(w, "{label} {current} (was unset){suffix}"),
        }
    }
}

#[allow(
    clippy::needless_pass_by_value,
    reason = "wired from clap-parsed CLI; ownership crosses the boundary"
)]
pub fn run(
    format: OutputFormat,
    version: Option<String>,
    bump: Option<VersionBump>,
    short: bool,
    dry_run: bool,
    working_dir: Option<PathBuf>,
) -> Result<ExitCode> {
    let project_root = match working_dir {
        Some(p) => p,
        None => std::env::current_dir().wrap_err("reading current directory")?,
    };
    let composer_json_path = project_root.join("composer.json");
    if !composer_json_path.is_file() {
        return Err(eyre!(
            "{} not found — not a Composer project",
            composer_json_path.display()
        ));
    }

    let mut doc = read_json_file(&composer_json_path)
        .wrap_err_with(|| format!("reading {}", composer_json_path.display()))?;
    let (name, previous) = {
        let obj = doc
            .as_object()
            .ok_or_else(|| eyre!("composer.json is not a JSON object"))?;
        (
            obj.get("name").and_then(Value::as_str).map(str::to_owned),
            obj.get("version").and_then(Value::as_str).map(str::to_owned),
        )
    };

    let target = match (version, bump) {
        (Some(explicit), _) => Some(validate(&explicit)?),
        (None, Some(component)) => {
            let current = previous.as_deref().ok_or_else(|| {
                eyre!(
                    "composer.json declares no `version` to bump — \
                     set one first, e.g. `bougie version 0.1.0`"
                )
            })?;
            Some(bump_version(current, component)?)
        }
        (None, None) => None,
    };

    let changed = target
        .as_deref()
        .is_some_and(|t| previous.as_deref() != Some(t));

    if changed && !dry_run {
        // `target` is Some whenever `changed` is true.
        let new = target.as_deref().unwrap_or_default();
        {
            let obj = doc
                .as_object_mut()
                .ok_or_else(|| eyre!("composer.json is not a JSON object"))?;
            set_version(obj, new);
        }
        write_json_file(&composer_json_path, &doc)
            .wrap_err_with(|| format!("writing {}", composer_json_path.display()))?;
    }

    emit(
        format,
        &VersionResult {
            schema_version: 1,
            name,
            previous: previous.clone(),
            current: target.or(previous),
            changed,
            dry_run,
            short,
        },
    )?;
    Ok(ExitCode::SUCCESS)
}

/// Accept any version string Composer's own parser accepts, and store it
/// **verbatim**. Composer records what the author wrote (`1.4.0`), not the
/// normalized form (`1.4.0.0`), so normalizing here would rewrite manifests
/// gratuitously.
fn validate(raw: &str) -> Result<String> {
    Version::parse(raw).map_err(|e| eyre!("`{raw}` is not a valid Composer version: {e}"))?;
    Ok(raw.to_owned())
}

/// Increment one semver component, zeroing everything to its right.
///
/// Three deliberate behaviors:
/// - A version shorter than three segments is padded (`1.2` → `1.2.0`)
///   before bumping, so `--bump patch` yields `1.2.1` rather than `1.3`.
/// - A pre-release suffix is **dropped**: `1.2.3-beta1 --bump patch` is
///   `1.2.4`, matching cargo and uv. Bumping produces a stable version.
/// - A `v` prefix is preserved, since Composer accepts either spelling and
///   the author's choice shouldn't be rewritten underneath them.
///
/// Segments come from the **raw** string rather than the parsed version:
/// Composer normalizes every version to four segments (`1.2.3` →
/// `1.2.3.0`), so reusing the parsed form would grow a three-segment
/// version into a four-segment one on every bump.
fn bump_version(current: &str, bump: VersionBump) -> Result<String> {
    let parsed = Version::parse(current).map_err(|e| {
        eyre!("composer.json's version `{current}` is not a valid Composer version: {e}")
    })?;
    if matches!(parsed.kind, VersionKind::Branch(_)) {
        return Err(eyre!(
            "version `{current}` is a branch version — `--bump` needs a numeric version like `1.2.3`"
        ));
    }

    let trimmed = current.trim();
    let (prefix, rest) = trimmed
        .strip_prefix(['v', 'V'])
        .map_or(("", trimmed), |rest| (&trimmed[..1], rest));
    // Cut at the first character that can't be part of a dotted numeric
    // core: `-beta1`, `+build`, and friends are suffixes we drop.
    let core_len = rest
        .find(|c: char| !c.is_ascii_digit() && c != '.')
        .unwrap_or(rest.len());
    let core = rest[..core_len].trim_end_matches('.');

    let mut segments: Vec<u64> = core
        .split('.')
        .map(|s| {
            s.parse::<u64>()
                .map_err(|e| eyre!("version `{current}` has a non-numeric segment `{s}`: {e}"))
        })
        .collect::<Result<_>>()?;
    if segments.len() < 3 {
        segments.resize(3, 0);
    }

    let index = match bump {
        VersionBump::Major => 0,
        VersionBump::Minor => 1,
        VersionBump::Patch => 2,
    };
    segments[index] += 1;
    for segment in segments.iter_mut().skip(index + 1) {
        *segment = 0;
    }

    let bumped = segments
        .iter()
        .map(u64::to_string)
        .collect::<Vec<_>>()
        .join(".");
    Ok(format!("{prefix}{bumped}"))
}

/// Set `version`, preserving the manifest's key order.
///
/// `serde_json`'s `insert` appends a *new* key at the end of the object,
/// which would bury `version` below `require` in a typical manifest.
/// Composer's schema lists it near the top, so when the key is absent the
/// map is rebuilt with `version` spliced in after `description` (or
/// `name`). An existing key is replaced in place and keeps its position.
fn set_version(obj: &mut Map<String, Value>, version: &str) {
    if let Some(slot) = obj.get_mut("version") {
        *slot = Value::String(version.to_owned());
        return;
    }
    let Some(anchor) = ["description", "name"]
        .into_iter()
        .find(|key| obj.contains_key(*key))
    else {
        obj.insert("version".to_owned(), Value::String(version.to_owned()));
        return;
    };

    let mut rebuilt = Map::with_capacity(obj.len() + 1);
    for (key, value) in std::mem::take(obj) {
        let is_anchor = key == anchor;
        rebuilt.insert(key, value);
        if is_anchor {
            rebuilt.insert("version".to_owned(), Value::String(version.to_owned()));
        }
    }
    *obj = rebuilt;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bump(current: &str, component: VersionBump) -> String {
        bump_version(current, component).expect("bump should succeed")
    }

    #[test]
    fn bumps_each_component_and_zeroes_the_right() {
        assert_eq!(bump("1.2.3", VersionBump::Major), "2.0.0");
        assert_eq!(bump("1.2.3", VersionBump::Minor), "1.3.0");
        assert_eq!(bump("1.2.3", VersionBump::Patch), "1.2.4");
    }

    #[test]
    fn pads_short_versions_to_three_segments() {
        assert_eq!(bump("1.2", VersionBump::Patch), "1.2.1");
        assert_eq!(bump("1", VersionBump::Minor), "1.1.0");
        assert_eq!(bump("1", VersionBump::Major), "2.0.0");
    }

    #[test]
    fn keeps_extra_segments_composer_allows() {
        // Composer accepts four-segment versions; bumping must not
        // silently truncate one away.
        assert_eq!(bump("1.2.3.4", VersionBump::Minor), "1.3.0.0");
    }

    #[test]
    fn drops_a_prerelease_suffix() {
        assert_eq!(bump("1.2.3-beta1", VersionBump::Patch), "1.2.4");
        assert_eq!(bump("2.0.0-RC1", VersionBump::Minor), "2.1.0");
    }

    #[test]
    fn preserves_a_v_prefix() {
        assert_eq!(bump("v1.2.3", VersionBump::Minor), "v1.3.0");
        assert_eq!(bump("v2.0.0-RC1", VersionBump::Patch), "v2.0.1");
    }

    #[test]
    fn rejects_a_branch_version() {
        let err = bump_version("dev-main", VersionBump::Patch)
            .expect_err("branch versions cannot be bumped");
        assert!(
            err.to_string().contains("branch version"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn validate_accepts_composer_versions_verbatim() {
        assert_eq!(validate("1.4.0").unwrap(), "1.4.0");
        assert_eq!(validate("v2.0.0").unwrap(), "v2.0.0");
        assert_eq!(validate("1.0.0-beta.2").unwrap(), "1.0.0-beta.2");
        assert!(validate("not a version").is_err());
    }

    #[test]
    fn replaces_an_existing_version_in_place() {
        let mut obj: Map<String, Value> =
            serde_json::from_str(r#"{"name":"acme/app","version":"1.0.0","type":"library"}"#)
                .unwrap();
        set_version(&mut obj, "1.1.0");
        assert_eq!(
            obj.keys().map(String::as_str).collect::<Vec<_>>(),
            ["name", "version", "type"]
        );
        assert_eq!(obj["version"], Value::String("1.1.0".into()));
    }

    #[test]
    fn splices_a_new_version_after_description() {
        let mut obj: Map<String, Value> = serde_json::from_str(
            r#"{"name":"acme/app","description":"An app","require":{"php":"^8.3"}}"#,
        )
        .unwrap();
        set_version(&mut obj, "0.1.0");
        assert_eq!(
            obj.keys().map(String::as_str).collect::<Vec<_>>(),
            ["name", "description", "version", "require"]
        );
    }

    #[test]
    fn splices_after_name_when_there_is_no_description() {
        let mut obj: Map<String, Value> =
            serde_json::from_str(r#"{"name":"acme/app","require":{"php":"^8.3"}}"#).unwrap();
        set_version(&mut obj, "0.1.0");
        assert_eq!(
            obj.keys().map(String::as_str).collect::<Vec<_>>(),
            ["name", "version", "require"]
        );
    }

    #[test]
    fn appends_when_there_is_no_anchor_key() {
        let mut obj: Map<String, Value> =
            serde_json::from_str(r#"{"require":{"php":"^8.3"}}"#).unwrap();
        set_version(&mut obj, "0.1.0");
        assert_eq!(
            obj.keys().map(String::as_str).collect::<Vec<_>>(),
            ["require", "version"]
        );
    }

    #[test]
    fn reads_sets_bumps_and_honors_dry_run_on_disk() {
        let dir = tempfile::tempdir().expect("tempdir");
        let manifest = dir.path().join("composer.json");
        std::fs::write(
            &manifest,
            "{\n    \"name\": \"acme/app\",\n    \"description\": \"An app\"\n}\n",
        )
        .expect("write manifest");

        let read = |path: &std::path::Path| -> Option<String> {
            let doc: Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
            doc.get("version")
                .and_then(Value::as_str)
                .map(str::to_owned)
        };

        // Reading a manifest with no version writes nothing.
        run(
            OutputFormat::Text,
            None,
            None,
            false,
            false,
            Some(dir.path().to_path_buf()),
        )
        .expect("read should succeed");
        assert_eq!(read(&manifest), None);

        // Setting an explicit version writes it.
        run(
            OutputFormat::Text,
            Some("1.2.3".into()),
            None,
            false,
            false,
            Some(dir.path().to_path_buf()),
        )
        .expect("set should succeed");
        assert_eq!(read(&manifest).as_deref(), Some("1.2.3"));

        // `--dry-run` reports but does not write.
        run(
            OutputFormat::Text,
            None,
            Some(VersionBump::Minor),
            false,
            true,
            Some(dir.path().to_path_buf()),
        )
        .expect("dry-run should succeed");
        assert_eq!(read(&manifest).as_deref(), Some("1.2.3"));

        // A real bump does write.
        run(
            OutputFormat::Text,
            None,
            Some(VersionBump::Minor),
            false,
            false,
            Some(dir.path().to_path_buf()),
        )
        .expect("bump should succeed");
        assert_eq!(read(&manifest).as_deref(), Some("1.3.0"));

        // The spliced key landed after `description`, not at the end.
        let doc: Value = serde_json::from_slice(&std::fs::read(&manifest).unwrap()).unwrap();
        let keys: Vec<&str> = doc.as_object().unwrap().keys().map(String::as_str).collect();
        assert_eq!(keys, ["name", "description", "version"]);
    }

    #[test]
    fn bumping_without_a_version_is_an_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("composer.json"), r#"{"name":"acme/app"}"#)
            .expect("write manifest");
        let err = run(
            OutputFormat::Text,
            None,
            Some(VersionBump::Patch),
            false,
            false,
            Some(dir.path().to_path_buf()),
        )
        .expect_err("bumping an unset version should fail");
        assert!(
            err.to_string().contains("declares no `version`"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn missing_manifest_is_an_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let err = run(
            OutputFormat::Text,
            None,
            None,
            false,
            false,
            Some(dir.path().to_path_buf()),
        )
        .expect_err("a directory with no composer.json should fail");
        assert!(
            err.to_string().contains("not a Composer project"),
            "unexpected error: {err}"
        );
    }
}
