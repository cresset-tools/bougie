//! Per-instance resolved TCP endpoint — `endpoint.json`.
//!
//! Records the *actual* ports an instance bound, which differ from the
//! catalog default when that port was already taken (by the developer's
//! own service, or by a sibling instance — two search engines both want
//! 9200). It is the runtime source of truth the catalog `const` can't
//! be: read by the supervisor (sticky reuse), the health probe, the
//! exec-arg/provisioner rendering, and — crucially — the *offline*
//! consumers (`bougie run` env, `service credentials`, diagnose) that
//! have no daemon to ask.
//!
//! Lives at `state/services/<name>/<version>/endpoint.json`, alongside
//! the instance's tenant ledger. Socket-only services (mariadb, redis)
//! have no endpoint file — their coexistence comes from the
//! version-keyed socket path, not a port.

use bougie_paths::Paths;
use eyre::{Result, WrapErr};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;

/// Load the recorded endpoint for an instance (`(name, version)`), or
/// `None` when none was ever written (never provisioned, or a
/// socket-only service). Errors are swallowed to `None` — a consumer
/// deriving a connection port falls back to the catalog default, which
/// is the right degradation for a corrupt/absent file. Use
/// [`ServiceEndpoint::load`] directly when a parse error must surface.
#[must_use]
pub fn load_for(paths: &Paths, name: &str, version: &str) -> Option<ServiceEndpoint> {
    ServiceEndpoint::load(&paths.service_endpoint(name, version))
        .ok()
        .flatten()
}

/// The effective primary port for an instance: the recorded endpoint's
/// primary, or `default` (the catalog `Binding::Tcp` port) when none is
/// recorded.
#[must_use]
pub fn effective_primary(paths: &Paths, name: &str, version: &str, default: u16) -> u16 {
    load_for(paths, name, version).map_or(default, |e| e.primary)
}

/// The effective value of a named secondary port for an instance, or
/// `default` when unrecorded.
#[must_use]
pub fn effective_extra(paths: &Paths, name: &str, version: &str, label: &str, default: u16) -> u16 {
    load_for(paths, name, version)
        .and_then(|e| e.extra_port(label))
        .unwrap_or(default)
}

/// The ports one instance actually bound.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServiceEndpoint {
    /// The primary bound port — the catalog `Binding::Tcp` port, or the
    /// port it was relocated to. This is what health-probes and what
    /// `BOUGIE_SERVICE_<NAME>_PORT` reports.
    pub primary: u16,
    /// Named secondary ports that ride alongside the primary
    /// (`http` = mailpit's web UI, `transport` = opensearch/ES 9300).
    /// Empty for single-port services; omitted from the JSON when empty.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extra: BTreeMap<String, u16>,
}

impl ServiceEndpoint {
    /// A single-port endpoint.
    #[must_use]
    pub fn new(primary: u16) -> Self {
        Self { primary, extra: BTreeMap::new() }
    }

    /// Record a named secondary port (builder-style).
    #[must_use]
    pub fn with_extra(mut self, name: impl Into<String>, port: u16) -> Self {
        self.extra.insert(name.into(), port);
        self
    }

    /// A named secondary port, if recorded.
    #[must_use]
    pub fn extra_port(&self, name: &str) -> Option<u16> {
        self.extra.get(name).copied()
    }

    /// Load the endpoint from `path`, or `None` when the file doesn't
    /// exist (never provisioned / socket-only service). A present but
    /// unparseable file is an error — it signals corruption, not
    /// absence.
    pub fn load(path: &Path) -> Result<Option<Self>> {
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => {
                return Err(e).wrap_err_with(|| format!("reading {}", path.display()));
            }
        };
        let ep = serde_json::from_str(&text)
            .wrap_err_with(|| format!("parsing endpoint file {}", path.display()))?;
        Ok(Some(ep))
    }

    /// Atomically write the endpoint to `path` (write-tmp-then-rename),
    /// creating the parent dir. Same durability shape as the tenant
    /// ledger's `rewrite`.
    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .wrap_err_with(|| format!("creating {}", parent.display()))?;
        }
        let json = serde_json::to_string(self).wrap_err("serializing endpoint")?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, json.as_bytes())
            .wrap_err_with(|| format!("writing {}", tmp.display()))?;
        std::fs::rename(&tmp, path)
            .wrap_err_with(|| format!("renaming {} -> {}", tmp.display(), path.display()))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn round_trips_through_disk() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("svc").join("endpoint.json");
        let ep = ServiceEndpoint::new(9201).with_extra("transport", 9301);
        ep.save(&path).unwrap();
        let back = ServiceEndpoint::load(&path).unwrap().unwrap();
        assert_eq!(back, ep);
        assert_eq!(back.primary, 9201);
        assert_eq!(back.extra_port("transport"), Some(9301));
        assert_eq!(back.extra_port("nope"), None);
    }

    #[test]
    fn missing_file_loads_as_none() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("absent.json");
        assert_eq!(ServiceEndpoint::load(&path).unwrap(), None);
    }

    #[test]
    fn empty_extra_is_omitted_from_json() {
        let ep = ServiceEndpoint::new(7080);
        let json = serde_json::to_string(&ep).unwrap();
        assert_eq!(json, r#"{"primary":7080}"#);
    }

    #[test]
    fn unknown_keys_are_tolerated_for_forward_compat() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("ep.json");
        std::fs::write(&path, r#"{"primary":5673,"future_field":true}"#).unwrap();
        let ep = ServiceEndpoint::load(&path).unwrap().unwrap();
        assert_eq!(ep.primary, 5673);
    }

    #[test]
    fn corrupt_file_is_an_error_not_absence() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("ep.json");
        std::fs::write(&path, "not json").unwrap();
        assert!(ServiceEndpoint::load(&path).is_err());
    }
}
