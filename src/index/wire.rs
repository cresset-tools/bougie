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
use std::collections::{BTreeMap, HashSet};

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
    /// Tool manifests only. Inner tools the outer tool needs co-installed
    /// (opensearch → jdk, rabbitmq → erlang). Empty on interpreter /
    /// extension manifests and on self-contained tools. See
    /// `UNBUNDLE_PLAN.md` §"Wire-format additions".
    #[serde(default)]
    pub requires_tools: Vec<RequiresTool>,
    /// Extension manifests only.
    #[serde(default)]
    pub extension: Option<ExtensionRef>,
    /// Interpreter manifests only.
    #[serde(default)]
    pub sapis: Option<Vec<String>>,
}

impl Manifest {
    /// Cross-field validation that can only be checked once the whole
    /// manifest has parsed. Per-entry checks (`closure`, `requires_tools`)
    /// are delegated to those types' own `validate()`.
    pub fn validate(&self) -> Result<()> {
        for c in &self.closure {
            c.validate()?;
        }
        let mut seen_link_into: HashSet<&str> = HashSet::new();
        for r in &self.requires_tools {
            r.validate()?;
            if !r.link_into.is_empty() && !seen_link_into.insert(r.link_into.as_str()) {
                return Err(eyre!(
                    "manifest {} has duplicate requires_tools link_into: {:?}",
                    self.tag,
                    r.link_into
                ));
            }
        }
        Ok(())
    }
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

/// Tool-on-tool dependency entry. Says: the outer tool needs an inner
/// tool (`name`, pinned at `version` / `tag`) installed alongside it,
/// and possibly symlinked at `link_into` inside the outer install root.
///
/// See `UNBUNDLE_PLAN.md` §"Wire-format additions" for the full design.
#[derive(Debug, Clone, Deserialize)]
pub struct RequiresTool {
    /// Catalog short name of the depended-on tool (e.g. `"jdk"`, `"erlang"`).
    pub name: String,
    /// Exact upstream version of the depended-on tool. Pinned, not a range.
    pub version: String,
    /// Full tag of the depended-on artifact
    /// (e.g. `"jdk-21.0.11_10-x86_64-unknown-linux-gnu-default"`).
    /// Identifies one row in the depended-on tool's section.
    pub tag: String,
    /// Manifest URL — absolute, same convention as [`Closure::url`]. The
    /// index publisher substitutes `{INDEX_BASE}` at publish time so the
    /// client never has to reconstruct paths.
    ///
    /// **No `manifest_sha256` companion field.** The inner manifest's
    /// sha256 is verified via the section row for the inner tool
    /// (`tool/<name>.json`), which already carries the authoritative
    /// `manifest.sha256` for every artifact in the publish. Computing
    /// the sha at tarball.nix build time would require a two-pass
    /// substitution in `index.nix` (substitute → hash → re-substitute
    /// cross-refs → re-hash); the section round-trip is the simpler
    /// trade. See `UNBUNDLE_PLAN.md` §"Note on inner-manifest verification".
    pub manifest_url: String,
    /// Path relative to the outer tool's install root where the inner
    /// tool's install root must be symlinked. Example: opensearch sets
    /// `link_into = "jdk"` so its scripts find `${ES_HOME}/jdk/bin/java`.
    /// Empty string means "install the inner tool but don't link it"
    /// (rare; reserved for the case where the outer tool only needs the
    /// inner installed, not visible at a fixed path).
    pub link_into: String,
}

impl RequiresTool {
    /// Per `UNBUNDLE_PLAN.md` §"Wire-format additions". Mirrors
    /// [`Closure::validate`].
    pub fn validate(&self) -> Result<()> {
        if !is_kebab_lower(&self.name) {
            return Err(eyre!("invalid requires_tool name: {:?}", self.name));
        }
        if self.version.is_empty() {
            return Err(eyre!("requires_tool {} has empty version", self.name));
        }
        if self.tag.is_empty() {
            return Err(eyre!("requires_tool {} has empty tag", self.name));
        }
        if !(self.manifest_url.starts_with("http://") || self.manifest_url.starts_with("https://"))
        {
            return Err(eyre!(
                "requires_tool {} manifest_url must be absolute (the index publisher substitutes {{INDEX_BASE}}): {:?}",
                self.name,
                self.manifest_url
            ));
        }
        // Empty link_into is the explicit "install but don't link" case.
        if !self.link_into.is_empty() {
            if self.link_into.starts_with('/') {
                return Err(eyre!(
                    "requires_tool {} link_into must be relative, got {:?}",
                    self.name,
                    self.link_into
                ));
            }
            if self.link_into.split('/').any(|comp| comp == "..") {
                return Err(eyre!(
                    "requires_tool {} link_into must not contain `..` components: {:?}",
                    self.name,
                    self.link_into
                ));
            }
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

    // ---------- requires_tools (Phase 0) ----------

    fn valid_requires_tool() -> RequiresTool {
        RequiresTool {
            name: "jdk".into(),
            version: "21.0.11+10".into(),
            tag: "jdk-21.0.11_10-x86_64-unknown-linux-gnu-default".into(),
            manifest_url: "https://index.example.com/versions/v1/targets/x86_64-unknown-linux-gnu/manifests/tool/jdk/21.0.11+10/jdk-21.0.11_10-x86_64-unknown-linux-gnu-default.json".into(),
            link_into: "jdk".into(),
        }
    }

    #[test]
    fn parse_tool_manifest_with_closure_and_requires_tools() {
        // mariadb-shaped manifest: split-closure (non-empty `closure[]`)
        // and an inner tool dep (non-empty `requires_tools[]`).
        let json = r#"{
            "schema": 1,
            "kind": "tool",
            "name": "rabbitmq",
            "tag": "rabbitmq-4.2.6-x86_64-unknown-linux-gnu-default",
            "version": "4.2.6",
            "target": "x86_64-unknown-linux-gnu",
            "flavor": "default",
            "libc": {"family":"gnu","min":"2.17"},
            "blob": {"url":"https://blobs.example.com/blobs/aa/aaaa","sha256":"aaaa"},
            "closure": [
                {"name":"openssl","version":"3.5.6","hash":"99c0f6e8","sha256":"00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff","url":"https://b/o"}
            ],
            "requires_tools": [
                {
                    "name":"erlang",
                    "version":"27.3.4.11",
                    "tag":"erlang-27.3.4.11-x86_64-unknown-linux-gnu-default",
                    "manifest_url":"https://index.example.com/versions/v1/targets/x86_64-unknown-linux-gnu/manifests/tool/erlang/27.3.4.11/erlang-27.3.4.11-x86_64-unknown-linux-gnu-default.json",
                    "link_into":"erlang"
                }
            ]
        }"#;
        let m: Manifest = serde_json::from_str(json).unwrap();
        assert_eq!(m.kind, SectionKind::Tool);
        assert_eq!(m.closure.len(), 1);
        assert_eq!(m.requires_tools.len(), 1);
        assert_eq!(m.requires_tools[0].name, "erlang");
        assert_eq!(m.requires_tools[0].link_into, "erlang");
        m.validate().unwrap();
    }

    #[test]
    fn parse_tool_manifest_defaults_requires_tools_to_empty() {
        // Pre-split tarballs (no `requires_tools` field) keep parsing.
        // This is the backward-compat hinge that lets the bougie client
        // ship before the index does.
        let json = r#"{
            "schema": 1,
            "kind": "tool",
            "name": "redis",
            "tag": "redis-8.6.3-x86_64-unknown-linux-gnu-default",
            "version": "8.6.3",
            "target": "x86_64-unknown-linux-gnu",
            "flavor": "default",
            "libc": {"family":"gnu","min":"2.17"},
            "blob": {"url":"https://blobs.example.com/blobs/ff/ffff","sha256":"ffff"},
            "closure": []
        }"#;
        let m: Manifest = serde_json::from_str(json).unwrap();
        assert!(m.requires_tools.is_empty());
        m.validate().unwrap();
    }

    #[test]
    fn requires_tool_validate_accepts_well_formed() {
        valid_requires_tool().validate().unwrap();
    }

    #[test]
    fn requires_tool_validate_accepts_empty_link_into() {
        // Explicit "install but don't link at a fixed path" mode.
        let mut r = valid_requires_tool();
        r.link_into = String::new();
        r.validate().unwrap();
    }

    #[test]
    fn requires_tool_validate_rejects_empty_name() {
        let mut r = valid_requires_tool();
        r.name = String::new();
        assert!(r.validate().unwrap_err().to_string().contains("name"));
    }

    #[test]
    fn requires_tool_validate_rejects_empty_version() {
        let mut r = valid_requires_tool();
        r.version = String::new();
        assert!(r.validate().unwrap_err().to_string().contains("version"));
    }

    #[test]
    fn requires_tool_validate_rejects_empty_tag() {
        let mut r = valid_requires_tool();
        r.tag = String::new();
        assert!(r.validate().unwrap_err().to_string().contains("tag"));
    }

    #[test]
    fn requires_tool_validate_rejects_relative_manifest_url() {
        let mut r = valid_requires_tool();
        r.manifest_url = "/relative/path.json".into();
        assert!(r
            .validate()
            .unwrap_err()
            .to_string()
            .contains("absolute"));
    }

    #[test]
    fn requires_tool_validate_rejects_link_into_dotdot() {
        let mut r = valid_requires_tool();
        r.link_into = "foo/../bar".into();
        assert!(r.validate().unwrap_err().to_string().contains(".."));
    }

    #[test]
    fn requires_tool_validate_rejects_link_into_absolute() {
        let mut r = valid_requires_tool();
        r.link_into = "/opt/jdk".into();
        assert!(r
            .validate()
            .unwrap_err()
            .to_string()
            .contains("relative"));
    }

    #[test]
    fn manifest_validate_rejects_duplicate_link_into() {
        // Two requires_tools entries pointing at the same `link_into`
        // would clobber each other on install — reject up front.
        let json = r#"{
            "schema": 1,
            "kind": "tool",
            "name": "weird",
            "tag": "weird-1.0.0-x86_64-unknown-linux-gnu-default",
            "version": "1.0.0",
            "target": "x86_64-unknown-linux-gnu",
            "flavor": "default",
            "libc": {"family":"gnu","min":"2.17"},
            "blob": {"url":"https://blobs.example.com/blobs/aa/aaaa","sha256":"aaaa"},
            "requires_tools": [
                {
                    "name":"jdk","version":"21.0.11+10",
                    "tag":"jdk-21.0.11_10-x86_64-unknown-linux-gnu-default",
                    "manifest_url":"https://x/j.json",
                    "link_into":"runtime"
                },
                {
                    "name":"erlang","version":"27.3.4.11",
                    "tag":"erlang-27.3.4.11-x86_64-unknown-linux-gnu-default",
                    "manifest_url":"https://x/e.json",
                    "link_into":"runtime"
                }
            ]
        }"#;
        let m: Manifest = serde_json::from_str(json).unwrap();
        let err = m.validate().unwrap_err().to_string();
        assert!(err.contains("duplicate"), "got: {err}");
    }

    #[test]
    fn manifest_validate_allows_multiple_empty_link_into() {
        // Empty link_into means "no symlink" — multiple such entries are
        // fine (they don't conflict with each other on the filesystem).
        let json = r#"{
            "schema": 1,
            "kind": "tool",
            "name": "weird",
            "tag": "weird-1.0.0-x86_64-unknown-linux-gnu-default",
            "version": "1.0.0",
            "target": "x86_64-unknown-linux-gnu",
            "flavor": "default",
            "libc": {"family":"gnu","min":"2.17"},
            "blob": {"url":"https://blobs.example.com/blobs/aa/aaaa","sha256":"aaaa"},
            "requires_tools": [
                {
                    "name":"jdk","version":"21.0.11+10",
                    "tag":"jdk-21.0.11_10-x86_64-unknown-linux-gnu-default",
                    "manifest_url":"https://x/j.json",
                    "link_into":""
                },
                {
                    "name":"erlang","version":"27.3.4.11",
                    "tag":"erlang-27.3.4.11-x86_64-unknown-linux-gnu-default",
                    "manifest_url":"https://x/e.json",
                    "link_into":""
                }
            ]
        }"#;
        let m: Manifest = serde_json::from_str(json).unwrap();
        m.validate().unwrap();
    }
}
