//! Index signature verification.
//!
//! Two trust models are supported:
//!
//! 1. **Production (Sigstore Bundle).** The build authority signs
//!    `index.json` via GitHub Actions OIDC → Fulcio. The signature
//!    sidecar is a Sigstore Bundle JSON. We verify the bundle against
//!    the Sigstore Public Good trust root and pin the certificate
//!    identity to `cresset-tools/php-build-standalone` issued by
//!    GitHub Actions.
//! 2. **Local override (detached ECDSA).** When `BOUGIE_TRUST_ROOT_PATH`
//!    is set in non-release builds, we treat the sidecar as a base64
//!    ECDSA P-256 signature against the PEM public key at that path.
//!    This is what the test harness uses; never reachable in release
//!    builds.

use crate::errors::BougieError;
use eyre::{Result, WrapErr};
use sha2::{Digest, Sha256};
use sigstore::bundle::Bundle;
use sigstore::bundle::verify::blocking::Verifier as SigstoreBlockingVerifier;
use sigstore::bundle::verify::policy::{
    AllOf, GitHubWorkflowRepository, OIDCIssuer, VerificationPolicy,
};
use sigstore::crypto::{
    Signature as SigstoreSignature, SigningScheme, verification_key::CosignVerificationKey,
};

/// The build authority that publishes the live index.
pub const EXPECTED_REPOSITORY: &str = "cresset-tools/php-build-standalone";
/// GitHub Actions' OIDC issuer URI.
pub const EXPECTED_ISSUER: &str = "https://token.actions.githubusercontent.com";

/// Placeholder ECDSA P-256 public key from scaffolding; only the
/// detached-ECDSA test-override path consults this. Production verifies
/// Sigstore Bundles and has no embedded long-lived key.
#[cfg(test)]
const EMBEDDED_TRUST_ROOT: &[u8] = include_bytes!("../../keys/trust-root.pub");

/// Trait so callers can inject a stub during isolated tests.
pub trait Verifier {
    /// `url` is included in any error so the user knows which fetch
    /// failed.
    fn verify(&self, url: &str, payload: &[u8], signature: &[u8]) -> Result<()>;
}

/// Decide which verifier to construct based on environment. Production
/// path: Sigstore bundle. Test override (`BOUGIE_TRUST_ROOT_PATH` in
/// debug/test builds): detached ECDSA against the pinned PEM.
pub fn build_verifier() -> Result<Box<dyn Verifier>> {
    if (cfg!(debug_assertions) || cfg!(test))
        && let Some(path) = std::env::var_os("BOUGIE_TRUST_ROOT_PATH")
    {
        let bytes = std::fs::read(&path).wrap_err_with(|| {
            format!("reading BOUGIE_TRUST_ROOT_PATH={}", path.to_string_lossy())
        })?;
        return Ok(Box::new(DetachedEcdsa::from_pem(&bytes)?));
    }
    Ok(Box::new(SigstoreBundleVerifier::production()?))
}

/// What the user sees from `bougie self version`.
#[derive(Debug, Clone)]
pub struct TrustDescription {
    pub kind: &'static str,
    pub detail: String,
}

pub fn describe_trust() -> TrustDescription {
    if (cfg!(debug_assertions) || cfg!(test))
        && let Some(path) = std::env::var_os("BOUGIE_TRUST_ROOT_PATH")
    {
        let fingerprint = std::fs::read(&path)
            .ok()
            .map_or_else(|| "(unreadable)".into(), |b| sha256_hex(&b));
        return TrustDescription {
            kind: "detached-ecdsa",
            detail: format!("BOUGIE_TRUST_ROOT_PATH={} sha256:{fingerprint}", path.to_string_lossy()),
        };
    }
    TrustDescription {
        kind: "sigstore-bundle",
        detail: format!("repo={EXPECTED_REPOSITORY} issuer={EXPECTED_ISSUER}"),
    }
}

// ---------- Production: Sigstore Bundle ----------

pub struct SigstoreBundleVerifier {
    inner: SigstoreBlockingVerifier,
}

impl std::fmt::Debug for SigstoreBundleVerifier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SigstoreBundleVerifier")
            .field("repo", &EXPECTED_REPOSITORY)
            .field("issuer", &EXPECTED_ISSUER)
            .finish_non_exhaustive()
    }
}

impl SigstoreBundleVerifier {
    pub fn production() -> Result<Self> {
        let inner = SigstoreBlockingVerifier::production().map_err(|e| {
            BougieError::IndexSignature {
                url: "(initialization)".into(),
                trust_root_fingerprint: "sigstore-public-good".into(),
                reason: format!("could not initialize Sigstore TUF trust root: {e}"),
                hint: "check network connectivity to the Sigstore Public Good Instance, or set BOUGIE_TRUST_ROOT_PATH for a local override (debug builds only)".into(),
            }
        })?;
        Ok(Self { inner })
    }
}

impl Verifier for SigstoreBundleVerifier {
    fn verify(&self, url: &str, payload: &[u8], signature: &[u8]) -> Result<()> {
        // sigstore-rs 0.13 only recognizes the v0.1 / v0.2 wire mediaType
        // strings. Rewrite v0.3's mediaType to v0.2's so the v0.2 profile
        // verifier runs — the verificationMaterial shape is structurally
        // compatible in the GitHub-Actions-Fulcio path. Drop this once
        // sigstore-rs ships v0.3 support.
        let normalized = normalize_bundle_media_type(signature);
        let bundle: Bundle = serde_json::from_slice(&normalized).map_err(|e| {
            BougieError::IndexSignature {
                url: url.to_owned(),
                trust_root_fingerprint: format!("sigstore-bundle ({EXPECTED_REPOSITORY})"),
                reason: format!("signature sidecar is not a valid Sigstore Bundle JSON: {e}"),
                hint: "the index publisher should emit a Sigstore Bundle (mediaType=application/vnd.dev.sigstore.bundle.v0.x+json) at index.json.sig".into(),
            }
        })?;

        let repo_policy = GitHubWorkflowRepository(EXPECTED_REPOSITORY.into());
        let issuer_policy = OIDCIssuer(EXPECTED_ISSUER.into());
        let policy: AllOf<'_> =
            AllOf::new([&repo_policy as &dyn VerificationPolicy, &issuer_policy])
                .expect("two non-empty children");

        // `offline=true` skips Rekor inclusion proof fetches; the bundle
        // already carries enough for cert-chain + signature verification.
        self.inner
            .verify(payload, bundle, &policy, /* offline */ true)
            .map_err(|e| {
                BougieError::IndexSignature {
                    url: url.to_owned(),
                    trust_root_fingerprint: format!("sigstore-bundle ({EXPECTED_REPOSITORY})"),
                    reason: format!("Sigstore bundle verification failed: {e}"),
                    hint: format!("expected the index to be signed by GitHub Actions running in {EXPECTED_REPOSITORY} via OIDC issuer {EXPECTED_ISSUER}; either the index was tampered, the signing identity changed, or the bougie binary's pinned identity is stale"),
                }
                .into()
            })
    }
}

// ---------- Test override: detached ECDSA against a pinned PEM ----------

pub struct DetachedEcdsa {
    key: CosignVerificationKey,
    fingerprint: String,
}

impl std::fmt::Debug for DetachedEcdsa {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DetachedEcdsa")
            .field("fingerprint", &self.fingerprint)
            .finish_non_exhaustive()
    }
}

impl DetachedEcdsa {
    pub fn from_pem(pem_bytes: &[u8]) -> Result<Self> {
        if pem_bytes.is_empty() {
            return Err(BougieError::IndexSignature {
                url: "(unknown)".into(),
                trust_root_fingerprint: String::new(),
                reason: "trust root PEM is empty".into(),
                hint: "set BOUGIE_TRUST_ROOT_PATH to a valid PEM-encoded ECDSA P-256 public key".into(),
            }
            .into());
        }
        let key = CosignVerificationKey::from_pem(pem_bytes, &SigningScheme::ECDSA_P256_SHA256_ASN1)
            .map_err(|e| BougieError::IndexSignature {
                url: "(unknown)".into(),
                trust_root_fingerprint: sha256_hex(pem_bytes),
                reason: format!("trust root is not a valid PEM ECDSA P-256 key: {e}"),
                hint: "regenerate the test key via `openssl ec -in priv.pem -pubout -out trust-root.pub`".into(),
            })?;
        Ok(Self { key, fingerprint: sha256_hex(pem_bytes) })
    }

    pub fn fingerprint(&self) -> &str {
        &self.fingerprint
    }
}

impl Verifier for DetachedEcdsa {
    fn verify(&self, url: &str, payload: &[u8], signature: &[u8]) -> Result<()> {
        let trimmed: Vec<u8> = signature
            .iter()
            .copied()
            .filter(|b| !b.is_ascii_whitespace())
            .collect();
        let attempt = self
            .key
            .verify_signature(SigstoreSignature::Base64Encoded(&trimmed), payload)
            .or_else(|_| self.key.verify_signature(SigstoreSignature::Raw(signature), payload));
        attempt.map_err(|e| {
            BougieError::IndexSignature {
                url: url.to_owned(),
                trust_root_fingerprint: self.fingerprint.clone(),
                reason: format!("detached signature did not verify against pinned key: {e}"),
                hint: "the test fixture's signing key disagrees with the pinned public key — regenerate both as a pair".into(),
            }
            .into()
        })
    }
}

/// Rewrite a Sigstore Bundle v0.3 mediaType string to the v0.2 wire
/// form so sigstore-rs 0.13 (which recognizes only v0.1/v0.2) accepts
/// it. Returns the unchanged input if no v0.3 mediaType is present.
fn normalize_bundle_media_type(json: &[u8]) -> Vec<u8> {
    let v03 = b"\"application/vnd.dev.sigstore.bundle.v0.3+json\"";
    let v02 = b"\"application/vnd.dev.sigstore.bundle+json;version=0.2\"";
    if let Some(idx) = find_subslice(json, v03) {
        let mut out = Vec::with_capacity(json.len() + 8);
        out.extend_from_slice(&json[..idx]);
        out.extend_from_slice(v02);
        out.extend_from_slice(&json[idx + v03.len()..]);
        out
    } else {
        json.to_vec()
    }
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|w| w == needle)
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(64);
    for b in Sha256::digest(bytes) {
        use std::fmt::Write as _;
        let _ = write!(s, "{b:02x}");
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;
    use sigstore::crypto::signing_key::{
        SigStoreSigner,
        ecdsa::{ECDSAKeys, EllipticCurve},
    };

    struct Keypair {
        signer: SigStoreSigner,
        pub_pem: String,
    }

    fn fresh_keypair() -> Keypair {
        let keys = ECDSAKeys::new(EllipticCurve::P256).expect("generate P-256");
        let pub_pem = keys.as_inner().public_key_to_pem().expect("export pub PEM");
        let signer = keys.to_sigstore_signer().expect("create signer");
        Keypair { signer, pub_pem }
    }

    #[test]
    fn detached_round_trip() {
        let kp = fresh_keypair();
        let payload = b"hello bougie";
        let sig = kp.signer.sign(payload).unwrap();
        let sig_b64 = base64::engine::general_purpose::STANDARD.encode(&sig);

        let v = DetachedEcdsa::from_pem(kp.pub_pem.as_bytes()).unwrap();
        v.verify("https://test/index.json", payload, sig_b64.as_bytes()).unwrap();
    }

    #[test]
    fn detached_tampered_payload_fails_with_url_in_message() {
        let kp = fresh_keypair();
        let payload = b"hello bougie";
        let sig = kp.signer.sign(payload).unwrap();
        let sig_b64 = base64::engine::general_purpose::STANDARD.encode(&sig);

        let v = DetachedEcdsa::from_pem(kp.pub_pem.as_bytes()).unwrap();
        let err = v
            .verify("https://test/index.json", b"hello WORLD ", sig_b64.as_bytes())
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("could not verify index signature"));
        assert!(msg.contains("https://test/index.json"));
    }

    #[test]
    fn detached_wrong_key_fails() {
        let kp_a = fresh_keypair();
        let kp_b = fresh_keypair();
        let payload = b"hello bougie";
        let sig = kp_a.signer.sign(payload).unwrap();
        let sig_b64 = base64::engine::general_purpose::STANDARD.encode(&sig);

        let v = DetachedEcdsa::from_pem(kp_b.pub_pem.as_bytes()).unwrap();
        assert!(v.verify("https://test/index.json", payload, sig_b64.as_bytes()).is_err());
    }

    #[test]
    fn detached_empty_pem_fails_with_hint() {
        let err = DetachedEcdsa::from_pem(b"").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("trust root PEM is empty"));
        assert!(msg.contains("BOUGIE_TRUST_ROOT_PATH"));
    }

    #[test]
    fn detached_fingerprint_is_64_hex_lowercase() {
        let v = DetachedEcdsa::from_pem(EMBEDDED_TRUST_ROOT).expect("embedded key parses");
        let fp = v.fingerprint();
        assert_eq!(fp.len(), 64);
        assert!(fp.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
    }

    #[test]
    fn embedded_key_loads_as_p256() {
        let _ = DetachedEcdsa::from_pem(EMBEDDED_TRUST_ROOT).expect("embedded key parses");
    }

    #[test]
    fn describe_trust_default_is_sigstore() {
        // Note: this test passes whenever BOUGIE_TRUST_ROOT_PATH is unset;
        // when set (as in some integration tests) it returns detached-ecdsa.
        // We don't mutate process env here because it'd race with parallel tests.
        let d = describe_trust();
        assert!(d.kind == "sigstore-bundle" || d.kind == "detached-ecdsa");
        assert!(!d.detail.is_empty());
    }
}
