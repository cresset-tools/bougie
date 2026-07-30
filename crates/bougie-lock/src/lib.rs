//! The committed `bougie.lock` toolchain lock model.

use eyre::{Result, WrapErr, eyre};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};

pub const FORMAT_VERSION: u32 = 1;
pub const FILE_NAME: &str = "bougie.lock";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolchainLock {
    pub version: u32,
    pub snapshot: String,
    pub php: PhpPin,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extensions: BTreeMap<String, ExtensionPin>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub services: BTreeMap<String, ServicePin>,
    pub targets: BTreeMap<String, TargetArtifacts>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PhpPin {
    pub constraint: String,
    pub version: String,
    pub flavor: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExtensionPin {
    pub constraint: String,
    pub version: String,
    pub origin: ExtensionOrigin,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ExtensionOrigin {
    Declared,
    Inferred,
    Baseline,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServicePin {
    pub constraint: String,
    pub version: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TargetArtifacts {
    pub php: ArtifactDigest,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extensions: BTreeMap<String, ArtifactDigest>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub services: BTreeMap<String, ArtifactDigest>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactDigest {
    pub manifest_sha256: String,
    pub blob_sha256: String,
}

impl ToolchainLock {
    pub fn read(project_root: &Path) -> Result<Option<Self>> {
        let path = project_root.join(FILE_NAME);
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(error).wrap_err_with(|| format!("reading {}", path.display()));
            }
        };
        let lock: Self = toml_edit::de::from_str(&text)
            .wrap_err_with(|| format!("parsing {}", path.display()))?;
        if lock.version != FORMAT_VERSION {
            return Err(eyre!(
                "unsupported bougie.lock format version {}; this bougie supports version {FORMAT_VERSION}",
                lock.version
            ));
        }
        Ok(Some(lock))
    }

    pub fn to_toml(&self) -> Result<String> {
        if self.version != FORMAT_VERSION {
            return Err(eyre!(
                "cannot write bougie.lock format version {}; this bougie supports version {FORMAT_VERSION}",
                self.version
            ));
        }
        toml_edit::ser::to_string_pretty(self).wrap_err("serializing bougie.lock")
    }

    pub fn write(&self, project_root: &Path) -> Result<PathBuf> {
        let path = project_root.join(FILE_NAME);
        let contents = self.to_toml()?;
        let mut temp = tempfile::NamedTempFile::new_in(project_root)
            .wrap_err_with(|| format!("creating temporary lock beside {}", path.display()))?;
        temp.write_all(contents.as_bytes())
            .wrap_err_with(|| format!("writing temporary lock beside {}", path.display()))?;
        temp.as_file()
            .sync_all()
            .wrap_err_with(|| format!("syncing temporary lock beside {}", path.display()))?;
        temp.persist(&path)
            .map_err(|e| e.error)
            .wrap_err_with(|| format!("replacing {}", path.display()))?;
        Ok(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> ToolchainLock {
        ToolchainLock {
            version: FORMAT_VERSION,
            snapshot: "20260730T120000Z".into(),
            php: PhpPin {
                constraint: "^8.3".into(),
                version: "8.4.21".into(),
                flavor: "nts".into(),
            },
            extensions: BTreeMap::from([(
                "redis".into(),
                ExtensionPin {
                    constraint: "*".into(),
                    version: "6.2.0".into(),
                    origin: ExtensionOrigin::Inferred,
                },
            )]),
            services: BTreeMap::from([(
                "mariadb".into(),
                ServicePin {
                    constraint: "11.4".into(),
                    version: "11.4.4".into(),
                },
            )]),
            targets: BTreeMap::from([(
                "x86_64-unknown-linux-gnu".into(),
                TargetArtifacts {
                    php: ArtifactDigest {
                        manifest_sha256: "a".repeat(64),
                        blob_sha256: "b".repeat(64),
                    },
                    extensions: BTreeMap::new(),
                    services: BTreeMap::new(),
                },
            )]),
        }
    }

    #[test]
    fn toml_round_trips() {
        let lock = fixture();
        let text = lock.to_toml().unwrap();
        let parsed: ToolchainLock = toml_edit::de::from_str(&text).unwrap();
        assert_eq!(parsed, lock);
        assert!(text.contains("[extensions.redis]"));
        assert!(text.contains("[targets.x86_64-unknown-linux-gnu.php]"));
    }

    #[test]
    fn writer_replaces_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(FILE_NAME), "old").unwrap();
        let path = fixture().write(dir.path()).unwrap();
        let parsed: ToolchainLock =
            toml_edit::de::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        assert_eq!(parsed, fixture());
    }

    #[test]
    fn reader_handles_missing_and_rejects_unknown_version() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(ToolchainLock::read(dir.path()).unwrap(), None);
        let mut lock = fixture();
        lock.version = FORMAT_VERSION + 1;
        std::fs::write(
            dir.path().join(FILE_NAME),
            toml_edit::ser::to_string(&lock).unwrap(),
        )
        .unwrap();
        let error = ToolchainLock::read(dir.path()).unwrap_err().to_string();
        assert!(error.contains("unsupported bougie.lock format version"));
    }
}
