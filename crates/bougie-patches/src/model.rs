//! The internal patch model and the normalization of cweagans' v1/v2
//! `composer.json` dialects into it.
//!
//! Two on-disk dialects map onto one [`Patch`]:
//!
//! - **v1 compact**: a target maps to an *object* of `description → url`.
//! - **v2 expanded**: a target maps to a *list* of objects
//!   `{description, url, sha256?, depth?, extra?}` (v2 also still accepts the
//!   compact object form).
//!
//! [`parse_target_patches`] accepts either shape so the resolver
//! ([`crate::resolve`], Phase B) never has to branch on dialect.

use std::path::PathBuf;

use eyre::bail;
use serde_json::Value;

/// A single resolved patch rule: one diff to apply to one target package.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Patch {
    /// Target package, `vendor/name` (the key under `extra.patches`). For a
    /// project-root ([`PatchScope::Root`]) patch this is a display-only join of
    /// the touched packages — application is driven off [`Patch::scope`], not
    /// this field.
    pub target: String,
    /// Human description — the compact-form key, or the expanded
    /// `description` field. Used for `PATCHES.txt` and dedup.
    pub description: String,
    /// Where to obtain the patch bytes.
    pub source: PatchSource,
    /// Expected sha256 of the patch *file* (v2 expanded form), if declared.
    /// `None` means trust-on-first-use (compute + record on apply).
    pub sha256: Option<String>,
    /// Strip-depth selection (`-pN`).
    pub depth: DepthSpec,
    /// Opaque `extra` object carried verbatim from the expanded form (e.g.
    /// `imported-from` provenance). `None` for compact entries.
    pub extra: Option<Value>,
    /// Where the patch applies: inside a single package's install dir
    /// (the default, every `extra.patches` entry) or at the project root
    /// (a multi-package top-level `patches/` file).
    pub scope: PatchScope,
}

/// Where a [`Patch`] is applied on disk.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum PatchScope {
    /// Apply inside the target package's install directory. This is every
    /// `extra.patches` entry and every single-package `patches/` file.
    #[default]
    Package,
    /// Apply at the **project root** — a zero-config `patches/` file whose diff
    /// spans multiple packages via project-root-relative paths
    /// (`a/vendor/foo/…`, `b/vendor/bar/…`). `packages` is the sorted set of
    /// packages the diff touches; they are coupled so a change to any one
    /// re-extracts them all pristine before the patch is re-applied as a unit.
    Root { packages: Vec<String> },
}

/// Where a patch's bytes come from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PatchSource {
    /// A local path. Relative paths are resolved against the project root;
    /// cweagans treats a bare path as the entry's `url` value.
    Local(PathBuf),
    /// An `http(s)://` URL.
    Remote(String),
}

impl PatchSource {
    /// Classify a raw `url` value: an `http(s)://` scheme is remote, anything
    /// else is treated as a local path (matching cweagans).
    pub fn from_url(url: &str) -> Self {
        if url.starts_with("http://") || url.starts_with("https://") {
            PatchSource::Remote(url.to_string())
        } else {
            PatchSource::Local(PathBuf::from(url))
        }
    }
}

/// Strip-depth selection for a patch (`-pN`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DepthSpec {
    /// No explicit depth — try the cweagans `-p1 -p0 -p2 -p4` fallback loop
    /// (`-p4` is the Magento case).
    #[default]
    Auto,
    /// An explicit `-pN` (v2 `depth` / v1 `patchLevel` / `--depth`).
    Fixed(usize),
}

impl DepthSpec {
    /// The depths to try, in order, for this spec.
    pub fn candidates(self) -> Vec<usize> {
        match self {
            DepthSpec::Fixed(n) => vec![n],
            // cweagans v1 order; `-p4` covers Magento 2 patches.
            DepthSpec::Auto => vec![1, 0, 2, 4],
        }
    }
}

/// What to do when a patch fails to apply at every candidate depth.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FailureMode {
    /// Skip the failing patch, warn, and continue (cweagans v1 default).
    #[default]
    SkipAndWarn,
    /// Abort the whole install (cweagans v2, or v1's
    /// `composer-exit-on-patch-failure` / `COMPOSER_EXIT_ON_PATCH_FAILURE`).
    Abort,
}

/// Parse the value of one `extra.patches[target]` entry into zero or more
/// [`Patch`]es, accepting **both** dialects:
///
/// - an *object* `{ "Description": "url", … }` (v1 compact), and
/// - an *array* `[ { "description", "url", "sha256"?, "depth"?, "extra"? }, … ]`
///   (v2 expanded; array elements may themselves be the compact one-key object).
///
/// `target` is the `vendor/name` key the value was found under.
pub fn parse_target_patches(target: &str, value: &Value) -> eyre::Result<Vec<Patch>> {
    match value {
        Value::Object(map) => map
            .iter()
            .map(|(description, url)| {
                let url = url.as_str().ok_or_else(|| {
                    eyre::eyre!(
                        "patch entry for `{target}` / `{description}` must map to a URL string"
                    )
                })?;
                Ok(Patch {
                    target: target.to_string(),
                    description: description.clone(),
                    source: PatchSource::from_url(url),
                    sha256: None,
                    depth: DepthSpec::Auto,
                    extra: None,
                    scope: PatchScope::Package,
                })
            })
            .collect(),
        Value::Array(items) => items
            .iter()
            .map(|item| parse_expanded_entry(target, item))
            .collect(),
        _ => bail!(
            "`extra.patches` entry for `{target}` must be an object (compact) \
             or an array (expanded), got {}",
            value_kind(value)
        ),
    }
}

/// Parse one element of an expanded-form array. Accepts either the full
/// `{description, url, …}` object or the compact single-key `{desc: url}`
/// object (v2 allows mixing).
fn parse_expanded_entry(target: &str, item: &Value) -> eyre::Result<Patch> {
    let obj = item
        .as_object()
        .ok_or_else(|| eyre::eyre!("patch entry for `{target}` must be an object"))?;

    // Compact single-key object inside an array: `{ "Description": "url" }`
    // — only when it carries neither `url` nor `description`.
    if !obj.contains_key("url") && !obj.contains_key("description") && obj.len() == 1 {
        let (description, url) = obj.iter().next().expect("len == 1");
        let url = url
            .as_str()
            .ok_or_else(|| eyre::eyre!("patch `{description}` for `{target}` must map to a URL"))?;
        return Ok(Patch {
            target: target.to_string(),
            description: description.clone(),
            source: PatchSource::from_url(url),
            sha256: None,
            depth: DepthSpec::Auto,
            extra: None,
            scope: PatchScope::Package,
        });
    }

    let url = obj
        .get("url")
        .and_then(Value::as_str)
        .ok_or_else(|| eyre::eyre!("expanded patch entry for `{target}` is missing `url`"))?;
    let description = obj
        .get("description")
        .and_then(Value::as_str)
        .unwrap_or(url)
        .to_string();
    let sha256 = obj
        .get("sha256")
        .and_then(Value::as_str)
        .map(str::to_string);
    let depth = match obj.get("depth") {
        Some(v) => {
            let n = v.as_u64().ok_or_else(|| {
                eyre::eyre!("`depth` for `{target}` patch must be a non-negative integer")
            })?;
            DepthSpec::Fixed(usize::try_from(n).unwrap_or(usize::MAX))
        }
        None => DepthSpec::Auto,
    };
    let extra = obj.get("extra").cloned();

    Ok(Patch {
        target: target.to_string(),
        description,
        source: PatchSource::from_url(url),
        sha256,
        depth,
        extra,
        scope: PatchScope::Package,
    })
}

fn value_kind(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "an array",
        Value::Object(_) => "an object",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn compact_object_form() {
        let v = json!({ "Fix the thing": "patches/fix.patch" });
        let patches = parse_target_patches("vendor/pkg", &v).unwrap();
        assert_eq!(patches.len(), 1);
        let p = &patches[0];
        assert_eq!(p.target, "vendor/pkg");
        assert_eq!(p.description, "Fix the thing");
        assert_eq!(p.source, PatchSource::Local("patches/fix.patch".into()));
        assert_eq!(p.depth, DepthSpec::Auto);
        assert!(p.sha256.is_none());
    }

    #[test]
    fn expanded_array_form_with_metadata() {
        let v = json!([
            { "description": "Remote fix", "url": "https://example.com/a.patch", "sha256": "abc", "depth": 2 }
        ]);
        let patches = parse_target_patches("vendor/pkg", &v).unwrap();
        assert_eq!(patches.len(), 1);
        let p = &patches[0];
        assert_eq!(p.description, "Remote fix");
        assert_eq!(p.source, PatchSource::Remote("https://example.com/a.patch".into()));
        assert_eq!(p.sha256.as_deref(), Some("abc"));
        assert_eq!(p.depth, DepthSpec::Fixed(2));
    }

    #[test]
    fn compact_entry_inside_array() {
        let v = json!([{ "Just a desc": "patches/x.patch" }]);
        let patches = parse_target_patches("vendor/pkg", &v).unwrap();
        assert_eq!(patches.len(), 1);
        assert_eq!(patches[0].description, "Just a desc");
        assert_eq!(patches[0].source, PatchSource::Local("patches/x.patch".into()));
    }

    #[test]
    fn url_scheme_classification() {
        assert_eq!(
            PatchSource::from_url("https://x/y.patch"),
            PatchSource::Remote("https://x/y.patch".into())
        );
        assert_eq!(
            PatchSource::from_url("patches/y.patch"),
            PatchSource::Local("patches/y.patch".into())
        );
    }

    #[test]
    fn depth_candidates() {
        assert_eq!(DepthSpec::Auto.candidates(), vec![1, 0, 2, 4]);
        assert_eq!(DepthSpec::Fixed(3).candidates(), vec![3]);
    }

    #[test]
    fn missing_url_in_expanded_is_error() {
        let v = json!([{ "description": "no url here" }]);
        assert!(parse_target_patches("vendor/pkg", &v).is_err());
    }
}
