//! Domain error types and the §8 exit-code map.
//!
//! Variants carry enough context for the user to diagnose without
//! reading source: which URL was being fetched, which sha was
//! expected vs received, which trust root was loaded. The runtime
//! wires `color_eyre` so the chain renders top-to-bottom.

use thiserror::Error;

/// Format a reqwest (or any `std::error::Error`) chain into a single string
/// that includes the root cause, e.g.:
///   "error sending request for url (…): dns error: failed to lookup address …"
///
/// Plain `e.to_string()` only shows the outermost message. This walks
/// `.source()` so callers see *why* the request failed.
pub fn error_chain(err: &dyn std::error::Error) -> String {
    use std::fmt::Write;
    let mut buf = err.to_string();
    let mut cur = err.source();
    while let Some(src) = cur {
        let _ = write!(buf, ": {src}");
        cur = src.source();
    }
    buf
}

#[derive(Debug, Error)]
pub enum BougieError {
    #[error("network failure while {operation}\n  detail: {detail}")]
    Network { operation: String, detail: String },

    #[error(
        "could not verify index signature\n  \
         index:   {url}\n  \
         trust root: sha256:{trust_root_fingerprint}\n  \
         reason:  {reason}\n  \
         hint:    {hint}"
    )]
    IndexSignature {
        url: String,
        trust_root_fingerprint: String,
        reason: String,
        hint: String,
    },

    #[error(
        "manifest sha256 mismatch\n  \
         url:      {url}\n  \
         expected: sha256:{expected}\n  \
         actual:   sha256:{actual}\n  \
         hint:     server-side desync; refetching may not help — surface to the index publisher"
    )]
    ManifestHashMismatch { url: String, expected: String, actual: String },

    #[error(
        "blob sha256 mismatch\n  \
         url:      {url}\n  \
         expected: sha256:{expected}\n  \
         actual:   sha256:{actual}\n  \
         hint:     download was retried once and still mismatched; check network for tampering or a stale CDN edge"
    )]
    BlobHashMismatch { url: String, expected: String, actual: String },

    #[error("resolution failed for {kind}: {detail}")]
    Resolution { kind: String, detail: String },

    #[error(
        "unsupported host target: {triple}\n  \
         hint:     {hint}"
    )]
    UnknownTarget { triple: String, hint: String },

    #[error(
        "yanked artifact selected: {tag}\n  \
         reason:   {reason}\n  \
         hint:     pin a non-yanked version, or pass --allow-yanked for forensic use"
    )]
    YankedSelected { tag: String, reason: String },

    #[error(
        "concurrent operation conflict\n  \
         lock:     {path}\n  \
         held by:  pid {pid}\n  \
         hint:     wait for the other bougie process to finish, or pass --lock-timeout=N for a longer wait"
    )]
    LockHeld { path: String, pid: u32 },

    #[error("filesystem error while {operation}: {detail}")]
    Filesystem { operation: String, detail: String },

    #[error("self-update failed: {detail}")]
    SelfUpdate { detail: String },

    /// No project marker found walking up from the invocation
    /// directory. `detail` carries the site's full message — which
    /// markers were looked for and the fitting hint differ per verb.
    #[error("{detail}")]
    NoProject { detail: String },

    /// A user-editable config source (composer.json, bougie.toml,
    /// server.toml) failed to parse or validate.
    #[error("invalid {path}\n  detail: {detail}")]
    Config { path: String, detail: String },

    /// An error frame from bougied, or a failure reaching it. `code`
    /// is the control-protocol error code (`service_start_failed`,
    /// `unknown_service`, …) — the wire's own closed vocabulary.
    #[error("bougied: {detail} ({code})")]
    Service { code: String, detail: String },
}

impl BougieError {
    pub fn exit_code(&self) -> u8 {
        match self {
            Self::Network { .. } => 10,
            Self::IndexSignature { .. } => 11,
            Self::ManifestHashMismatch { .. } => 12,
            Self::BlobHashMismatch { .. } => 13,
            Self::Resolution { .. } => 20,
            Self::UnknownTarget { .. } => 21,
            Self::YankedSelected { .. } => 22,
            Self::NoProject { .. } => 30,
            Self::Config { .. } => 31,
            Self::LockHeld { .. } => 40,
            Self::Filesystem { .. } => 50,
            Self::SelfUpdate { .. } => 60,
            Self::Service { .. } => 70,
        }
    }
}

/// Chain-walking on purpose, mirroring telemetry's
/// `outcome_for_error`: a typed error buried under wrapping layers
/// must yield the same exit code as one at the report root.
pub fn exit_code_for(err: &eyre::Report) -> u8 {
    err.chain()
        .find_map(|cause| cause.downcast_ref::<BougieError>())
        .map_or(1, BougieError::exit_code)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_variant_has_distinct_code() {
        let codes = [
            BougieError::Network { operation: String::new(), detail: String::new() }.exit_code(),
            BougieError::IndexSignature {
                url: String::new(),
                trust_root_fingerprint: String::new(),
                reason: String::new(),
                hint: String::new(),
            }
            .exit_code(),
            BougieError::ManifestHashMismatch {
                url: String::new(),
                expected: String::new(),
                actual: String::new(),
            }
            .exit_code(),
            BougieError::BlobHashMismatch {
                url: String::new(),
                expected: String::new(),
                actual: String::new(),
            }
            .exit_code(),
            BougieError::Resolution { kind: String::new(), detail: String::new() }.exit_code(),
            BougieError::UnknownTarget { triple: String::new(), hint: String::new() }
                .exit_code(),
            BougieError::YankedSelected { tag: String::new(), reason: String::new() }
                .exit_code(),
            BougieError::LockHeld { path: String::new(), pid: 0 }.exit_code(),
            BougieError::Filesystem { operation: String::new(), detail: String::new() }
                .exit_code(),
            BougieError::SelfUpdate { detail: String::new() }.exit_code(),
            BougieError::NoProject { detail: String::new() }.exit_code(),
            BougieError::Config { path: String::new(), detail: String::new() }.exit_code(),
            BougieError::Service { code: String::new(), detail: String::new() }.exit_code(),
        ];
        let mut sorted = codes;
        sorted.sort_unstable();
        for w in sorted.windows(2) {
            assert_ne!(w[0], w[1], "duplicate exit code {}", w[0]);
        }
    }

    #[test]
    fn exit_code_for_wrapped_bougie_error() {
        let report = eyre::Report::new(BougieError::BlobHashMismatch {
            url: "u".into(),
            expected: "e".into(),
            actual: "a".into(),
        });
        assert_eq!(exit_code_for(&report), 13);
    }

    #[test]
    fn exit_code_for_unknown_error_defaults_to_one() {
        let report = eyre::eyre!("something else");
        assert_eq!(exit_code_for(&report), 1);
    }

    #[test]
    fn exit_code_for_chain_buried_bougie_error() {
        let report = eyre::Report::new(BougieError::Service {
            code: "service_start_failed".into(),
            detail: "rabbitmq: exited during startup".into(),
        })
        .wrap_err("starting declared services");
        assert_eq!(exit_code_for(&report), 70);
    }

    #[derive(Debug, Error)]
    #[error("connection failed")]
    struct Outer {
        #[source]
        cause: Inner,
    }

    #[derive(Debug, Error)]
    #[error("dns lookup failed")]
    struct Inner;

    #[test]
    fn error_chain_walks_sources() {
        let e = Outer { cause: Inner };
        let chain = error_chain(&e);
        assert_eq!(chain, "connection failed: dns lookup failed");
    }

    #[test]
    fn error_chain_single_error() {
        let e = std::io::Error::new(std::io::ErrorKind::NotFound, "file gone");
        assert_eq!(error_chain(&e), "file gone");
    }

    #[test]
    fn signature_error_message_includes_hint() {
        let e = BougieError::IndexSignature {
            url: "https://example/index.json".into(),
            trust_root_fingerprint: "abc".into(),
            reason: "bad sig".into(),
            hint: "rotate the key".into(),
        };
        let s = e.to_string();
        assert!(s.contains("https://example/index.json"));
        assert!(s.contains("sha256:abc"));
        assert!(s.contains("bad sig"));
        assert!(s.contains("rotate the key"));
    }
}
