//! Event wire types (schema v1).
//!
//! Every field here is enumerated in `TELEMETRY.md`, which is the
//! public contract the collector validates against — free-form strings
//! never appear. Timestamps are hour-truncated by construction
//! ([`crate::clock::UtcHour`]).

use bougie_errors::BougieError;
use serde::Serialize;

/// Wire-schema version (independent of the consent version).
pub const SCHEMA: u32 = 1;

pub const OUTCOME_OK: &str = "ok";

/// Envelope fields shared by every event, flattened into each line.
#[derive(Debug, Clone, Serialize)]
pub struct Common {
    pub schema: u32,
    pub event: &'static str,
    /// Hour-truncated RFC 3339 UTC timestamp.
    pub ts: String,
    /// Anonymous install UUID, or `"unset"` before consent minted one.
    pub install_id: String,
    /// Per-process UUID correlating events from one invocation.
    pub invocation: String,
    pub bougie_version: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub build_sha: Option<&'static str>,
    pub os: &'static str,
    pub arch: &'static str,
    pub libc: &'static str,
    pub ci: bool,
    pub install_method: &'static str,
}

#[derive(Debug, Clone, Serialize)]
pub struct CommandEvent {
    #[serde(flatten)]
    pub common: Common,
    /// Stable subcommand name from the dispatcher's `command_name()`.
    pub name: &'static str,
    pub duration_ms: u64,
    /// `"ok"` or an error category from the `bougie-errors` taxonomy.
    pub outcome: &'static str,
    pub exit_code: u8,
}

/// Map a failed command's error to its telemetry category — the
/// category label and exit code are the *entire* error payload; no
/// message content ever leaves the machine outside the crash lane.
pub fn outcome_for_error(err: &eyre::Report) -> &'static str {
    match err.downcast_ref::<BougieError>() {
        Some(BougieError::Network { .. }) => "network",
        Some(BougieError::IndexSignature { .. }) => "index-signature",
        Some(BougieError::ManifestHashMismatch { .. }) => "manifest-hash",
        Some(BougieError::BlobHashMismatch { .. }) => "blob-hash",
        Some(BougieError::Resolution { .. }) => "resolution",
        Some(BougieError::UnknownTarget { .. }) => "unknown-target",
        Some(BougieError::YankedSelected { .. }) => "yanked",
        Some(BougieError::LockHeld { .. }) => "lock-held",
        Some(BougieError::Filesystem { .. }) => "filesystem",
        Some(BougieError::SelfUpdate { .. }) => "self-update",
        None => "other",
    }
}

pub fn os() -> &'static str {
    if cfg!(target_os = "linux") {
        "linux"
    } else if cfg!(target_os = "macos") {
        "macos"
    } else if cfg!(target_os = "windows") {
        "windows"
    } else {
        "other"
    }
}

pub fn arch() -> &'static str {
    if cfg!(target_arch = "x86_64") {
        "x86_64"
    } else if cfg!(target_arch = "aarch64") {
        "aarch64"
    } else {
        "other"
    }
}

pub fn libc() -> &'static str {
    if cfg!(all(target_os = "linux", target_env = "musl")) {
        "musl"
    } else if cfg!(all(target_os = "linux", target_env = "gnu")) {
        "gnu"
    } else {
        "none"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn outcome_covers_every_bougie_error_category() {
        let err = eyre::Report::new(BougieError::Resolution {
            kind: String::new(),
            detail: String::new(),
        });
        assert_eq!(outcome_for_error(&err), "resolution");
        assert_eq!(outcome_for_error(&eyre::eyre!("misc")), "other");
    }

    #[test]
    fn command_event_serializes_flat_with_no_free_text() {
        let ev = CommandEvent {
            common: Common {
                schema: SCHEMA,
                event: "command",
                ts: "2026-07-03T09:00:00Z".into(),
                install_id: "unset".into(),
                invocation: "00000000-0000-4000-8000-000000000000".into(),
                bougie_version: "0.40.0",
                build_sha: None,
                os: os(),
                arch: arch(),
                libc: libc(),
                ci: false,
                install_method: "unknown",
            },
            name: "sync",
            duration_ms: 1234,
            outcome: OUTCOME_OK,
            exit_code: 0,
        };
        let json: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&ev).unwrap()).unwrap();
        // Flattened envelope: no nested object, schema + event at top level.
        assert_eq!(json["schema"], 1);
        assert_eq!(json["event"], "command");
        assert_eq!(json["name"], "sync");
        // Absent build_sha is omitted, not null.
        assert!(json.get("build_sha").is_none());
    }
}
