//! Sigstore signature verification of the index root.
//!
//! Bougie pins a public key into the binary at build time and uses it
//! to verify a detached signature over the bytes of `index.json`. The
//! signature lives at `index.json.sig` (sidecar). Algorithm: ECDSA
//! P-256 with SHA-256 (sigstore's default).
//!
//! In `cfg(debug_assertions)` or `cfg(test)` builds, `BOUGIE_TRUST_ROOT_PATH`
//! overrides the embedded key — used by the integration test harness.

use crate::errors::BougieError;
use eyre::{Result, WrapErr};
use sha2::{Digest, Sha256};
use sigstore::crypto::{
    verification_key::CosignVerificationKey, Signature as SigstoreSignature, SigningScheme,
};

/// Phase 4 ships a placeholder ECDSA P-256 public key generated alongside the
/// scaffolding. The matching private key is held by the build authority's CI
/// (when one exists); for now no signed root is published yet.
const EMBEDDED_TRUST_ROOT: &[u8] = include_bytes!("../../keys/trust-root.pub");

#[derive(Debug, Clone)]
pub struct TrustRoot {
    pem: Vec<u8>,
    fingerprint: String,
}

impl TrustRoot {
    /// Resolve the active trust root: env override (only in non-release
    /// builds) or the embedded key.
    pub fn from_env_or_embedded() -> Result<Self> {
        if (cfg!(debug_assertions) || cfg!(test))
            && let Some(path) = std::env::var_os("BOUGIE_TRUST_ROOT_PATH")
        {
            let bytes = std::fs::read(&path).wrap_err_with(|| {
                format!("reading BOUGIE_TRUST_ROOT_PATH={}", path.to_string_lossy())
            })?;
            return Ok(Self::from_pem(&bytes));
        }
        Ok(Self::from_pem(EMBEDDED_TRUST_ROOT))
    }

    pub fn from_pem(pem_bytes: &[u8]) -> Self {
        let digest = Sha256::digest(pem_bytes);
        let mut fp = String::with_capacity(64);
        for b in digest {
            use std::fmt::Write as _;
            let _ = write!(fp, "{b:02x}");
        }
        Self { pem: pem_bytes.to_vec(), fingerprint: fp }
    }

    pub fn fingerprint(&self) -> &str {
        &self.fingerprint
    }

    pub fn is_empty(&self) -> bool {
        self.pem.is_empty()
    }
}

/// Verifier abstraction so phase-7 sync code can swap a stub during
/// tests if needed. Production path is [`Sigstore::new`].
pub trait Verifier {
    fn verify(&self, payload: &[u8], signature: &[u8]) -> Result<()>;
}

pub struct Sigstore {
    key: CosignVerificationKey,
}

impl std::fmt::Debug for Sigstore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Sigstore").finish_non_exhaustive()
    }
}

impl Sigstore {
    pub fn new(trust_root: &TrustRoot) -> Result<Self> {
        if trust_root.is_empty() {
            return Err(BougieError::IndexSignature.into());
        }
        let key = CosignVerificationKey::from_pem(
            &trust_root.pem,
            &SigningScheme::ECDSA_P256_SHA256_ASN1,
        )
        .map_err(|e| eyre::eyre!("trust root is not a valid PEM ECDSA-P256 key: {e}"))?;
        Ok(Self { key })
    }
}

impl Verifier for Sigstore {
    fn verify(&self, payload: &[u8], signature: &[u8]) -> Result<()> {
        // The sidecar may carry a base64-encoded signature with optional
        // surrounding whitespace, or raw signature bytes. Try base64 first.
        let trimmed: Vec<u8> = signature
            .iter()
            .copied()
            .filter(|b| !b.is_ascii_whitespace())
            .collect();
        let result = self
            .key
            .verify_signature(SigstoreSignature::Base64Encoded(&trimmed), payload)
            .or_else(|_| {
                self.key
                    .verify_signature(SigstoreSignature::Raw(signature), payload)
            });
        result.map_err(|_| BougieError::IndexSignature.into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;
    use sigstore::crypto::signing_key::{
        ecdsa::{ECDSAKeys, EllipticCurve},
        SigStoreSigner,
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
    fn verify_round_trip() {
        let kp = fresh_keypair();
        let payload = b"hello bougie";
        let sig = kp.signer.sign(payload).unwrap();
        let sig_b64 = base64::engine::general_purpose::STANDARD.encode(&sig);

        let tr = TrustRoot::from_pem(kp.pub_pem.as_bytes());
        let v = Sigstore::new(&tr).unwrap();
        v.verify(payload, sig_b64.as_bytes()).unwrap();
    }

    #[test]
    fn tampered_payload_fails() {
        let kp = fresh_keypair();
        let payload = b"hello bougie";
        let sig = kp.signer.sign(payload).unwrap();
        let sig_b64 = base64::engine::general_purpose::STANDARD.encode(&sig);

        let tr = TrustRoot::from_pem(kp.pub_pem.as_bytes());
        let v = Sigstore::new(&tr).unwrap();
        let err = v.verify(b"hello WORLD ", sig_b64.as_bytes()).unwrap_err();
        assert!(err.to_string().contains("index signature failure"));
    }

    #[test]
    fn wrong_key_fails() {
        let kp_a = fresh_keypair();
        let kp_b = fresh_keypair();
        let payload = b"hello bougie";
        let sig = kp_a.signer.sign(payload).unwrap();
        let sig_b64 = base64::engine::general_purpose::STANDARD.encode(&sig);

        let tr = TrustRoot::from_pem(kp_b.pub_pem.as_bytes());
        let v = Sigstore::new(&tr).unwrap();
        assert!(v.verify(payload, sig_b64.as_bytes()).is_err());
    }

    #[test]
    fn empty_trust_root_construction_fails() {
        let tr = TrustRoot::from_pem(b"");
        assert!(Sigstore::new(&tr).is_err());
    }

    #[test]
    fn fingerprint_is_64_hex_lowercase() {
        let tr = TrustRoot::from_pem(b"any bytes");
        let fp = tr.fingerprint();
        assert_eq!(fp.len(), 64);
        assert!(fp.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
    }

    #[test]
    fn embedded_key_loads_as_p256() {
        let tr = TrustRoot::from_pem(EMBEDDED_TRUST_ROOT);
        Sigstore::new(&tr).expect("embedded key parses");
    }
}
