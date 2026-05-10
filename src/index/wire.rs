//! Wire-format types for the index protocol (DISTRIBUTION.md).
//!
//! Each level (root → section → manifest → blob) deserializes into a
//! plain `Deserialize` struct.
//!
//! Section rows are *lean*: they carry only what the resolver needs to
//! choose between artifacts (tag, version, flavor, php_minor for
//! extensions, manifest pointer, yanked, frozen). Manifests are *fat*:
//! they carry the full ABI/libc surface, the blob URL + sha256, and the
//! closure of bundled-library store paths. See DISTRIBUTION.md
//! §Section-index and §Manifests-and-blobs.

use eyre::{eyre, Result};
use serde::Deserialize;
use std::collections::BTreeMap;

#[derive(Debug, Clone, Deserialize)]
pub struct Root {
    pub schema: u32,
    /// Per-publish snapshot identifier (DISTRIBUTION.md
    /// §Snapshot-consistency). Clients embed this in section URLs:
    /// `versions/<root.version>/targets/<target>/sections/<name>.json`.
    /// Treat as opaque — the publish pipeline uses an ISO-8601
    /// timestamp but the protocol does not require any structure.
    pub version: String,
    pub generated: String,
    /// Informational: the source revision this publish was built from.
    /// Optional for backwards-compat with older roots; surfaced via
    /// `bougie self version --remote` and audit tooling.
    #[serde(default)]
    pub source: Option<RootSource>,
    pub targets: BTreeMap<String, RootTarget>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RootSource {
    pub git_commit: String,
    pub git_ref: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RootTarget {
    pub sections: BTreeMap<String, SectionRef>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SectionRef {
    pub sha256: String,
    pub size: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Section {
    pub schema: u32,
    pub name: String,
    pub kind: SectionKind,
    pub target: String,
    pub artifacts: Vec<Artifact>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SectionKind {
    Interpreter,
    Extension,
}

/// One row in a section. Lean: only the fields a resolver needs.
#[derive(Debug, Clone, Deserialize)]
pub struct Artifact {
    pub tag: String,
    pub version: String,
    pub flavor: String,
    /// Extension rows only: the PHP minor (`"8.3"`) the extension is
    /// ABI-compatible with. Interpreter rows omit this — `version`
    /// already carries the full PHP version.
    #[serde(default)]
    pub php_minor: Option<String>,
    pub manifest: ManifestRef,
    #[serde(default)]
    pub yanked: bool,
    #[serde(default)]
    pub yanked_reason: Option<String>,
    #[serde(default)]
    pub frozen: bool,
}

/// Pointer to a manifest. `path` is server-absolute and hostname-free
/// (e.g. `/targets/<target>/manifests/...`); the client prepends its
/// configured index host. See DISTRIBUTION.md §Manifests-and-blobs.
#[derive(Debug, Clone, Deserialize)]
pub struct ManifestRef {
    pub path: String,
    pub sha256: String,
}

/// Fat manifest: the complete install spec for one artifact.
#[derive(Debug, Clone, Deserialize)]
pub struct Manifest {
    pub schema: u32,
    pub kind: SectionKind,
    pub name: String,
    pub tag: String,
    pub version: String,
    pub target: String,
    pub flavor: String,
    pub abi: Abi,
    pub libc: Libc,
    pub blob: Blob,
    #[serde(default)]
    pub closure: Vec<Closure>,
    /// Extension manifests only.
    #[serde(default)]
    pub extension: Option<ExtensionRef>,
    /// Interpreter manifests only.
    #[serde(default)]
    pub sapis: Option<Vec<String>>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Abi {
    pub php: String,
    pub zend_module_api_no: String,
    pub zend_extension_api_no: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Libc {
    /// `gnu`, `musl`, or `darwin`.
    pub family: String,
    /// Manylinux-style symbol/macOS floor (`2.17`, `11.0`, …).
    pub min: String,
}

/// The artifact's main tarball.
#[derive(Debug, Clone, Deserialize)]
pub struct Blob {
    pub url: String,
    pub sha256: String,
}

/// Where the extracted `.so` lives inside the extension tarball.
#[derive(Debug, Clone, Deserialize)]
pub struct ExtensionRef {
    pub path: String,
    pub sha256: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Closure {
    pub name: String,
    pub version: String,
    pub hash: String,
    pub sha256: String,
    pub url: String,
}

impl Closure {
    /// Per CLI.md §7.3.
    pub fn validate(&self) -> Result<()> {
        if !is_kebab_lower(&self.name) {
            return Err(eyre!("invalid closure name: {:?}", self.name));
        }
        if self.version.is_empty() {
            return Err(eyre!("closure {} has empty version", self.name));
        }
        if !is_hex_min(&self.hash, 8) {
            return Err(eyre!(
                "closure {} hash is not ≥8 hex chars: {:?}",
                self.name,
                self.hash
            ));
        }
        if !is_hex_exact(&self.sha256, 64) {
            return Err(eyre!(
                "closure {} sha256 must be 64 hex chars: {:?}",
                self.name,
                self.sha256
            ));
        }
        if !(self.url.starts_with("http://") || self.url.starts_with("https://")) {
            return Err(eyre!(
                "closure {} url must be absolute (the index publisher substitutes {{BLOB_BASE}}): {:?}",
                self.name,
                self.url
            ));
        }
        Ok(())
    }
}

fn is_kebab_lower(s: &str) -> bool {
    let mut chars = s.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !(first.is_ascii_lowercase() || first.is_ascii_digit()) {
        return false;
    }
    chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '-'))
}

fn is_hex_min(s: &str, n: usize) -> bool {
    s.len() >= n && s.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
}

fn is_hex_exact(s: &str, n: usize) -> bool {
    s.len() == n && s.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_minimal_root() {
        let json = r#"{
            "schema": 1,
            "version": "20260508T120000Z",
            "generated": "2026-05-08T12:00:00Z",
            "targets": {
                "x86_64-unknown-linux-gnu": {
                    "sections": {
                        "interpreter/php": {"sha256": "11aa", "size": 100}
                    }
                }
            }
        }"#;
        let root: Root = serde_json::from_str(json).unwrap();
        assert_eq!(root.schema, 1);
        assert_eq!(root.version, "20260508T120000Z");
        assert_eq!(root.targets.len(), 1);
    }

    #[test]
    fn parse_extension_section() {
        let json = r#"{
            "schema": 1,
            "name": "xdebug",
            "kind": "extension",
            "target": "x86_64-unknown-linux-gnu",
            "artifacts": [{
                "tag": "xdebug-3.5.1+php83-x86_64-unknown-linux-gnu-nts",
                "version": "3.5.1",
                "flavor": "nts",
                "php_minor": "8.3",
                "manifest": {
                    "path": "/targets/x86_64-unknown-linux-gnu/manifests/ext/xdebug/3.5.1/xdebug-3.5.1+php83-x86_64-unknown-linux-gnu-nts.json",
                    "sha256": "deadbeef"
                },
                "yanked": false,
                "frozen": false
            }]
        }"#;
        let s: Section = serde_json::from_str(json).unwrap();
        assert_eq!(s.kind, SectionKind::Extension);
        assert_eq!(s.artifacts.len(), 1);
        assert_eq!(s.artifacts[0].php_minor.as_deref(), Some("8.3"));
        assert!(s.artifacts[0].manifest.path.starts_with("/targets/"));
        assert!(!s.artifacts[0].yanked);
        assert!(!s.artifacts[0].frozen);
    }

    #[test]
    fn parse_interpreter_section_omits_php_minor() {
        let json = r#"{
            "schema": 1,
            "name": "php",
            "kind": "interpreter",
            "target": "x86_64-unknown-linux-gnu",
            "artifacts": [{
                "tag": "php-8.3.12-x86_64-unknown-linux-gnu-nts",
                "version": "8.3.12",
                "flavor": "nts",
                "manifest": {
                    "path": "/targets/x86_64-unknown-linux-gnu/manifests/php/8.3/php-8.3.12-x86_64-unknown-linux-gnu-nts.json",
                    "sha256": "deadbeef"
                },
                "yanked": false,
                "frozen": false
            }]
        }"#;
        let s: Section = serde_json::from_str(json).unwrap();
        assert!(s.artifacts[0].php_minor.is_none());
    }

    #[test]
    fn parse_extension_manifest() {
        let json = r#"{
            "schema": 1,
            "kind": "extension",
            "name": "xdebug",
            "tag": "xdebug-3.5.1+php83-x86_64-unknown-linux-gnu-nts",
            "version": "3.5.1",
            "target": "x86_64-unknown-linux-gnu",
            "flavor": "nts",
            "abi": {"php":"8.3","zend_module_api_no":"20230831","zend_extension_api_no":"420230831"},
            "libc": {"family":"gnu","min":"2.17"},
            "blob": {"url":"https://blobs.example.com/blobs/aa/aaaa","sha256":"aaaa"},
            "extension": {"path":"lib/extensions/20230831/xdebug.so","sha256":"abcd"},
            "closure": [
                {"name":"libffi","version":"3.4.6","hash":"a1b2c3d4","sha256":"00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff","url":"https://b/x"}
            ]
        }"#;
        let m: Manifest = serde_json::from_str(json).unwrap();
        assert_eq!(m.kind, SectionKind::Extension);
        assert_eq!(m.blob.sha256, "aaaa");
        assert!(m.extension.is_some());
        assert!(m.sapis.is_none());
        m.closure[0].validate().unwrap();
    }

    #[test]
    fn parse_interpreter_manifest() {
        let json = r#"{
            "schema": 1,
            "kind": "interpreter",
            "name": "php",
            "tag": "php-8.3.12-x86_64-unknown-linux-gnu-nts",
            "version": "8.3.12",
            "target": "x86_64-unknown-linux-gnu",
            "flavor": "nts",
            "abi": {"php":"8.3","zend_module_api_no":"20230831","zend_extension_api_no":"420230831"},
            "libc": {"family":"gnu","min":"2.17"},
            "blob": {"url":"https://blobs.example.com/blobs/bb/bbbb","sha256":"bbbb"},
            "closure": [],
            "sapis": ["cli","fpm"]
        }"#;
        let m: Manifest = serde_json::from_str(json).unwrap();
        assert_eq!(m.kind, SectionKind::Interpreter);
        assert_eq!(m.sapis.as_deref(), Some(&["cli".to_string(), "fpm".to_string()][..]));
        assert!(m.extension.is_none());
        assert!(m.closure.is_empty());
    }

    #[test]
    fn validate_rejects_short_hash() {
        let c = Closure {
            name: "libffi".into(),
            version: "1".into(),
            hash: "abc".into(),
            sha256: "0".repeat(64),
            url: "https://x".into(),
        };
        assert!(c.validate().unwrap_err().to_string().contains("hash"));
    }

    #[test]
    fn validate_rejects_wrong_sha_length() {
        let c = Closure {
            name: "libffi".into(),
            version: "1".into(),
            hash: "abcdef01".into(),
            sha256: "0".repeat(63),
            url: "https://x".into(),
        };
        assert!(c.validate().unwrap_err().to_string().contains("64 hex"));
    }

    #[test]
    fn validate_rejects_relative_url() {
        let c = Closure {
            name: "libffi".into(),
            version: "1".into(),
            hash: "abcdef01".into(),
            sha256: "0".repeat(64),
            url: "relative/path".into(),
        };
        assert!(c.validate().unwrap_err().to_string().contains("absolute"));
    }

    #[test]
    fn validate_rejects_unsubstituted_placeholder() {
        let c = Closure {
            name: "libffi".into(),
            version: "1".into(),
            hash: "abcdef01".into(),
            sha256: "0".repeat(64),
            url: "{BLOB_BASE}/00/0000".into(),
        };
        // Placeholders must be substituted at index-generation time;
        // a manifest with a raw {BLOB_BASE} reaching the client is a
        // publisher-side bug.
        assert!(c.validate().unwrap_err().to_string().contains("absolute"));
    }

    #[test]
    fn validate_rejects_uppercase_name() {
        let c = Closure {
            name: "LibFFI".into(),
            version: "1".into(),
            hash: "abcdef01".into(),
            sha256: "0".repeat(64),
            url: "https://x".into(),
        };
        assert!(c.validate().is_err());
    }
}
