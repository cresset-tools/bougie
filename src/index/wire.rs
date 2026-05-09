//! Wire-format types for the index protocol (DISTRIBUTION.md).
//!
//! Each level (root → section → manifest → blob) deserializes into a
//! plain `Deserialize` struct. Validation per CLI.md §7.3 lives in
//! [`Closure::validate`].

use eyre::{eyre, Result};
use serde::Deserialize;
use std::collections::BTreeMap;

#[derive(Debug, Clone, Deserialize)]
pub struct Root {
    pub schema: u32,
    pub generated: String,
    pub targets: BTreeMap<String, RootTarget>,
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

#[derive(Debug, Clone, Deserialize)]
pub struct Artifact {
    pub tag: String,
    pub version: String,
    pub abi: Abi,
    pub flavor: String,
    pub libc_min: Option<String>,
    pub manifest: ManifestRef,
    #[serde(default)]
    pub yanked: bool,
    #[serde(default)]
    pub yanked_reason: Option<String>,
    #[serde(default)]
    pub frozen: bool,
    pub built: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Abi {
    pub php: String,
    pub zend_module_api_no: String,
    pub ts: bool,
    pub debug: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ManifestRef {
    pub url: String,
    pub sha256: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Manifest {
    pub name: String,
    pub version: String,
    pub abi: Abi,
    #[serde(default)]
    pub closure: Vec<Closure>,
    /// Present for extension manifests.
    #[serde(default)]
    pub extension: Option<ExtensionRef>,
    /// Present for interpreter manifests.
    #[serde(default)]
    pub interpreter: Option<InterpreterRef>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ExtensionRef {
    pub path: String,
    pub sha256: String,
    pub url: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct InterpreterRef {
    pub sha256: String,
    pub url: String,
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
    /// Per §7.3.
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
        if !(self.url.starts_with("http://")
            || self.url.starts_with("https://")
            || self.url.contains("{BLOB_BASE}"))
        {
            return Err(eyre!(
                "closure {} url is not absolute and not a {{BLOB_BASE}} placeholder: {:?}",
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
        assert_eq!(root.targets.len(), 1);
    }

    #[test]
    fn parse_section() {
        let json = r#"{
            "schema": 1,
            "name": "xdebug",
            "kind": "extension",
            "target": "x86_64-unknown-linux-gnu",
            "artifacts": [{
                "tag": "xdebug-3.5.1+php83-x86_64-unknown-linux-gnu-nts",
                "version": "3.5.1",
                "abi": {"php":"8.3","zend_module_api_no":"20230831","ts":false,"debug":false},
                "flavor": "nts",
                "libc_min": "2.17",
                "manifest": {"url": "../../m.json", "sha256": "deadbeef"},
                "yanked": false,
                "built": "2026-05-07T18:42:00Z"
            }]
        }"#;
        let s: Section = serde_json::from_str(json).unwrap();
        assert_eq!(s.kind, SectionKind::Extension);
        assert_eq!(s.artifacts.len(), 1);
        assert!(!s.artifacts[0].yanked);
        assert!(!s.artifacts[0].frozen);
    }

    #[test]
    fn parse_manifest_with_closure() {
        let json = r#"{
            "name":"xdebug","version":"3.5.1",
            "abi":{"php":"8.3","zend_module_api_no":"20230831","ts":false,"debug":false},
            "extension":{"path":"lib/extensions/20230831/xdebug.so","sha256":"abcd","url":"https://b/x"},
            "closure":[
                {"name":"libffi","version":"3.4.6","hash":"a1b2c3d4","sha256":"00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff","url":"{BLOB_BASE}/00/00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff"}
            ]
        }"#;
        let m: Manifest = serde_json::from_str(json).unwrap();
        assert_eq!(m.closure.len(), 1);
        m.closure[0].validate().unwrap();
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
    fn validate_rejects_relative_url_without_placeholder() {
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

    #[test]
    fn validate_accepts_blob_base_placeholder() {
        let c = Closure {
            name: "libffi".into(),
            version: "1".into(),
            hash: "abcdef01".into(),
            sha256: "0".repeat(64),
            url: "{BLOB_BASE}/00/0000".into(),
        };
        c.validate().unwrap();
    }
}
