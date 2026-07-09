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

/// Every subcommand name the dispatcher may record — the collector
/// validates `command.name` / `crash.command` against exactly this
/// list. A unit test in the `bougie` bin asserts `command_name()`
/// stays a subset; the collector consumes this const once it can
/// depend on this crate from crates.io.
pub const COMMAND_VOCAB: &[&str] = &[
    "init", "new", "ext", "add", "remove", "lock", "tree", "outdated", "sync", "run",
    "php", "node", "patches", "composer", "tool", "tool-exec", "cache", "self",
    "telemetry", "__telemetry-flush", "diagnose", "server", "service", "projects",
    "make", "format", "start", "stop", "unknown",
    // Retired spellings older clients still emit; the collector must
    // keep accepting them. `services` was renamed to `service`.
    "services",
];

/// Every outcome label [`outcome_for_error`] (plus `ok`) can produce,
/// with the reserved `usage`/`panic` codes. Same collector contract as
/// [`COMMAND_VOCAB`].
pub const OUTCOME_VOCAB: &[&str] = &[
    "ok", "network", "index-signature", "manifest-hash", "blob-hash", "resolution",
    "unknown-target", "yanked", "lock-held", "filesystem", "self-update", "no-project",
    "config", "service", "usage", "panic", "other",
];

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
    #[serde(flatten)]
    pub enrich: Enrichment,
}

/// Optional perf + ecosystem fields (TELEMETRY.md), attached by
/// commands via [`crate::probe`]; absent fields are omitted from the
/// wire entirely. Ecosystem fields (`php_*`, `extensions`, `services`,
/// `*_deps`) are additionally throttled to once per project per week.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Enrichment {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resolve_ms: Option<u64>,
    /// Vendor materialize/audit phase wall-clock.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vendor_ms: Option<u64>,
    /// Autoload-dump wall-clock (0 = freshness marker skipped it).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub autoload_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub packages_installed: Option<u32>,
    /// On-disk bytes of dist archives fetched this run.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub download_bytes: Option<u64>,
    /// Dist-cache hit share of this run's fetches, 0–100. Absent when
    /// nothing needed fetching.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_hit_pct: Option<u8>,
    /// Minor only (`8.4`), never the patch level.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub php_version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub php_flavor: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub php_source: Option<&'static str>,
    /// Closed vocabulary ([`crate::probe::EXTENSION_VOCAB`]).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub extensions: Option<Vec<String>>,
    /// Closed vocabulary ([`crate::probe::SERVICE_VOCAB`]).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub services: Option<Vec<String>>,
    /// Bucketed ([`crate::probe::bucket`]), never a raw count.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub direct_deps: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_deps: Option<&'static str>,
}

/// A scrubbed panic report (see `scrub.rs` for the guarantees: frames
/// are allowlisted symbols or build-relative offsets; the message has
/// paths, home-dir fragments, and long quoted spans redacted).
#[derive(Debug, Clone, Serialize)]
pub struct CrashEvent {
    #[serde(flatten)]
    pub common: Common,
    /// The verb that was running (same closed set as `command.name`).
    pub command: &'static str,
    /// 16-hex crash identity: `sha256(frames)` truncated.
    pub fingerprint: String,
    pub frames: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

impl Enrichment {
    /// Drop the per-project ecosystem fields (throttle hit), keeping
    /// the perf fields, which carry no project shape.
    pub fn strip_ecosystem(&mut self) {
        self.php_version = None;
        self.php_flavor = None;
        self.php_source = None;
        self.extensions = None;
        self.services = None;
        self.direct_deps = None;
        self.total_deps = None;
    }
}

/// Map a failed command's error to its telemetry category — the
/// category label and exit code are the *entire* error payload; no
/// message content ever leaves the machine outside the crash lane.
///
/// Classification walks the full cause chain: a typed [`BougieError`]
/// anywhere in it wins; otherwise the transport and io roots that
/// ad-hoc eyre wrapping buries are still recovered
/// (`reqwest::Error` → `network`, `std::io::Error` → `filesystem`).
/// Network is checked before io deliberately — transport errors often
/// carry an io root, and the transport is the failure's substance.
pub fn outcome_for_error(err: &eyre::Report) -> &'static str {
    if let Some(typed) = err.chain().find_map(|cause| cause.downcast_ref::<BougieError>()) {
        return match typed {
            BougieError::Network { .. } => "network",
            BougieError::IndexSignature { .. } => "index-signature",
            BougieError::ManifestHashMismatch { .. } => "manifest-hash",
            BougieError::BlobHashMismatch { .. } => "blob-hash",
            BougieError::Resolution { .. } => "resolution",
            BougieError::UnknownTarget { .. } => "unknown-target",
            BougieError::YankedSelected { .. } => "yanked",
            BougieError::LockHeld { .. } => "lock-held",
            BougieError::Filesystem { .. } => "filesystem",
            BougieError::SelfUpdate { .. } => "self-update",
            BougieError::NoProject { .. } => "no-project",
            BougieError::Config { .. } => "config",
            BougieError::Service { .. } => "service",
        };
    }
    if err.chain().any(<dyn std::error::Error>::is::<reqwest::Error>) {
        return "network";
    }
    if err.chain().any(<dyn std::error::Error>::is::<std::io::Error>) {
        return "filesystem";
    }
    "other"
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
    fn every_bougie_error_outcome_is_in_the_vocab() {
        let variants = [
            BougieError::NoProject { detail: String::new() },
            BougieError::Config { path: String::new(), detail: String::new() },
            BougieError::Service { code: String::new(), detail: String::new() },
        ];
        for v in variants {
            let outcome = outcome_for_error(&eyre::Report::new(v));
            assert!(OUTCOME_VOCAB.contains(&outcome), "{outcome} missing from OUTCOME_VOCAB");
            assert_ne!(outcome, "other");
        }
    }

    #[test]
    fn typed_error_classifies_through_wrap_layers() {
        let err = eyre::Report::new(BougieError::LockHeld { path: String::new(), pid: 42 })
            .wrap_err("syncing the project")
            .wrap_err("outer context");
        assert_eq!(outcome_for_error(&err), "lock-held");
    }

    #[test]
    fn io_rooted_chain_classifies_as_filesystem() {
        let err = eyre::Report::new(std::io::Error::other("disk fell off"))
            .wrap_err("materializing vendor tree");
        assert_eq!(outcome_for_error(&err), "filesystem");
    }

    #[test]
    fn network_rooted_chain_classifies_as_network() {
        // A builder-stage reqwest error: no socket is touched, but the
        // type is the same one a transport failure surfaces as.
        let req = reqwest::blocking::get("no-scheme").unwrap_err();
        let err = eyre::Report::new(req).wrap_err("fetching packagist metadata");
        assert_eq!(outcome_for_error(&err), "network");
    }

    #[test]
    fn network_wins_over_the_io_root_of_a_transport_error() {
        // A real refused connection: reqwest outermost, an ECONNREFUSED
        // io::Error at the root. Both types sit in one chain — the
        // network-before-io check order is what keeps this out of
        // `filesystem`.
        let port = {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            listener.local_addr().unwrap().port()
            // Dropped here: connecting now gets ECONNREFUSED.
        };
        let req = reqwest::blocking::get(format!("http://127.0.0.1:{port}/")).unwrap_err();
        let err = eyre::Report::new(req).wrap_err("mirror fetch");
        assert!(
            err.chain().any(|cause| cause.is::<std::io::Error>()),
            "test premise: the transport error should carry an io root"
        );
        assert_eq!(outcome_for_error(&err), "network");
    }

    #[test]
    fn stringified_causes_stay_other() {
        // Flattening a typed error into a message destroys the type —
        // the classifier cannot recover it. Documented limitation; the
        // fix is removing the flattening at the call site.
        let io = std::io::Error::other("gone");
        let err = eyre::eyre!("copying vendor tree: {io}");
        assert_eq!(outcome_for_error(&err), "other");
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
            enrich: Enrichment {
                resolve_ms: Some(88),
                php_version: Some("8.4".into()),
                ..Enrichment::default()
            },
        };
        let json: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&ev).unwrap()).unwrap();
        // Flattened envelope: no nested object, schema + event at top level.
        assert_eq!(json["schema"], 1);
        assert_eq!(json["event"], "command");
        assert_eq!(json["name"], "sync");
        // Absent optionals are omitted, not null.
        assert!(json.get("build_sha").is_none());
        assert!(json.get("vendor_ms").is_none());
        // Set enrichment flattens to the top level.
        assert_eq!(json["resolve_ms"], 88);
        assert_eq!(json["php_version"], "8.4");
    }
}
