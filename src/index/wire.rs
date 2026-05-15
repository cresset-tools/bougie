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
    /// Service / tool tarball (mariadb, redis, opensearch, rabbitmq,
    /// plus runtime-only deps like jdk and erlang). Distinguished
    /// from `Extension` because the manifest carries no PHP `abi`
    /// block and no `extension` field — it's a self-contained
    /// install-shaped tree consumed by `bougied`'s service supervisor.
    Tool,
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
    /// Optional: only interpreter / extension manifests carry the PHP
    /// ABI block. Tool / service manifests (mariadb, redis, jdk, …)
    /// omit it because they're not loaded into a PHP process.
    #[serde(default)]
    pub abi: Option<Abi>,
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
    /// Byte length of the tarball at `url` — used by the CLI to
    /// pre-compute aggregate download progress without paying a
    /// HEAD round-trip per file. `#[serde(default)]` keeps older
    /// manifests (published before this field existed) parsable;
    /// such entries fall through to a sizeless spinner-style bar.
    #[serde(default)]
    pub size: u64,
}

/// Where the extracted `.so` lives inside the extension tarball, and
/// which PHP INI directive to enable it under.
#[derive(Debug, Clone, Deserialize)]
pub struct ExtensionRef {
    pub path: String,
    pub sha256: String,
    /// Whether PHP loads this extension via `extension=` (regular Zend
    /// extension that hooks the runtime through MINIT/RSHUTDOWN) or
    /// `zend_extension=` (Zend extensions that hook the opcode
    /// dispatch, e.g. opcache, xdebug, pcov, datadog-trace).
    ///
    /// Optional + defaulted so manifests written before the field was
    /// introduced still parse: an omitted `load` means `Extension`,
    /// which is correct for the overwhelming majority of `.so` files.
    /// Publishers shipping a zend extension MUST set it explicitly.
    #[serde(default)]
    pub load: LoadDirective,
}

/// The PHP INI directive a `.so` is loaded under. Serialised in
/// kebab-case to match the rest of the wire format (`extension`,
/// `zend-extension`) — the in-INI spelling uses an underscore
/// (`zend_extension`) and is handled at INI-emit time, not here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, serde::Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum LoadDirective {
    #[default]
    Extension,
    ZendExtension,
}

impl LoadDirective {
    /// The INI directive token PHP expects (`extension` or
    /// `zend_extension`). Note the underscore form — unlike the wire
    /// representation, INI files use `snake_case` for this directive.
    pub fn ini_directive(self) -> &'static str {
        match self {
            Self::Extension => "extension",
            Self::ZendExtension => "zend_extension",
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct Closure {
    pub name: String,
    pub version: String,
    pub hash: String,
    pub sha256: String,
    pub url: String,
    /// Byte length of the closure tarball — see [`Blob::size`].
    #[serde(default)]
    pub size: u64,
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
            "extension": {"path":"lib/extensions/20230831/xdebug.so","sha256":"abcd","load":"zend-extension"},
            "closure": [
                {"name":"libffi","version":"3.4.6","hash":"a1b2c3d4","sha256":"00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff","url":"https://b/x"}
            ]
        }"#;
        let m: Manifest = serde_json::from_str(json).unwrap();
        assert_eq!(m.kind, SectionKind::Extension);
        assert_eq!(m.blob.sha256, "aaaa");
        let ext = m.extension.as_ref().unwrap();
        assert_eq!(ext.load, LoadDirective::ZendExtension);
        assert_eq!(ext.load.ini_directive(), "zend_extension");
        assert!(m.sapis.is_none());
        m.closure[0].validate().unwrap();
    }

    #[test]
    fn extension_manifest_load_directive_defaults_to_extension() {
        // Pre-load-directive manifests (no `load` field) must keep
        // parsing — bougie's first ext-bundle releases predate this
        // schema bump and should remain consumable without re-publish.
        let json = r#"{
            "schema": 1,
            "kind": "extension",
            "name": "redis",
            "tag": "redis-6.0.2+php83-x86_64-unknown-linux-gnu-nts",
            "version": "6.0.2",
            "target": "x86_64-unknown-linux-gnu",
            "flavor": "nts",
            "abi": {"php":"8.3","zend_module_api_no":"20230831","zend_extension_api_no":"420230831"},
            "libc": {"family":"gnu","min":"2.17"},
            "blob": {"url":"https://blobs.example.com/blobs/cc/cccc","sha256":"cccc"},
            "extension": {"path":"lib/extensions/20230831/redis.so","sha256":"dddd"}
        }"#;
        let m: Manifest = serde_json::from_str(json).unwrap();
        let ext = m.extension.as_ref().unwrap();
        assert_eq!(ext.load, LoadDirective::Extension);
        assert_eq!(ext.load.ini_directive(), "extension");
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
    fn parse_tool_section() {
        // Service / tool sections (rabbit, redis, mariadb, …) use
        // `kind: "tool"` and omit `php_minor` on every artifact.
        let json = r#"{
            "schema": 1,
            "name": "redis",
            "kind": "tool",
            "target": "x86_64-unknown-linux-gnu",
            "artifacts": [{
                "tag": "redis-8.6.3-x86_64-unknown-linux-gnu-default",
                "version": "8.6.3",
                "flavor": "default",
                "manifest": {
                    "path": "/versions/v1/targets/x86_64-unknown-linux-gnu/manifests/tool/redis/8.6.3/redis-8.6.3-x86_64-unknown-linux-gnu-default.json",
                    "sha256": "deadbeef"
                },
                "yanked": false,
                "frozen": false
            }]
        }"#;
        let s: Section = serde_json::from_str(json).unwrap();
        assert_eq!(s.kind, SectionKind::Tool);
        assert!(s.artifacts[0].php_minor.is_none());
    }

    #[test]
    fn parse_tool_manifest_without_abi() {
        // Tool manifests carry no PHP `abi` block — they aren't loaded
        // into a PHP process. Unknown fields like `binaries`,
        // `build_info`, `bundled_libraries` ride along in the wire
        // format and are ignored.
        let json = r#"{
            "schema": 1,
            "kind": "tool",
            "name": "redis",
            "tag": "redis-8.6.3-x86_64-unknown-linux-gnu-default",
            "version": "8.6.3",
            "target": "x86_64-unknown-linux-gnu",
            "flavor": "default",
            "libc": {"family":"gnu","min":"2.17"},
            "binaries": ["redis-server","redis-cli"],
            "blob": {"url":"https://blobs.example.com/blobs/ff/ffff","sha256":"ffff"},
            "closure": []
        }"#;
        let m: Manifest = serde_json::from_str(json).unwrap();
        assert_eq!(m.kind, SectionKind::Tool);
        assert!(m.abi.is_none());
        assert!(m.extension.is_none());
        assert!(m.sapis.is_none());
        assert_eq!(m.blob.sha256, "ffff");
    }

    #[test]
    fn validate_rejects_short_hash() {
        let c = Closure {
            name: "libffi".into(),
            version: "1".into(),
            hash: "abc".into(),
            sha256: "0".repeat(64),
            url: "https://x".into(),
            size: 0,
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
            size: 0,
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
            size: 0,
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
            size: 0,
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
            size: 0,
        };
        assert!(c.validate().is_err());
    }
}
