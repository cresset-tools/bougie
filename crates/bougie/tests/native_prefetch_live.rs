//! Live-network validation of the native-binary prefetcher: derives a
//! wick prefetch plan exactly as `bougie tool install cresset/wick`
//! would from its `extra.bougie.native-binary` metadata, runs the real
//! fetcher (mirror → GitHub), and checks the verified binary lands in
//! the launcher-cache layout.
//!
//! Ignored by default — it downloads a real release artifact. Run with
//! `cargo test -p bougie --test native_prefetch_live -- --ignored`.

use bougie_tool::prefetch::{plan, NativeBinarySpec, PlanDecision};

/// A wick release with published binaries for every declared target.
/// NOTE: released before archive signing existed, so it has NO `.sig`
/// bundles — which is exactly what the fail-closed test exploits.
const WICK_VERSION: &str = "v0.2.1";

fn wick_spec(sigstore_repository: Option<String>) -> NativeBinarySpec {
    NativeBinarySpec {
        name: "wick".into(),
        tag_prefix: "wick-v".into(),
        base_urls: vec![
            "https://releases.bougie.tools/github/wick/releases/download".into(),
            "https://github.com/cresset-tools/wick/releases/download".into(),
        ],
        targets: vec![
            "x86_64-unknown-linux-gnu".into(),
            "x86_64-unknown-linux-musl".into(),
            "aarch64-apple-darwin".into(),
            "x86_64-pc-windows-msvc".into(),
        ],
        sigstore_repository,
    }
}

#[test]
#[ignore = "downloads a real wick release from the mirror/GitHub"]
fn prefetches_wick_release_binary() {
    let spec = wick_spec(None);

    let host = bougie_platform::target::Triple::detect()
        .expect("host triple")
        .to_string();
    let cache_base = tempfile::TempDir::new().expect("temp cache base");

    let decision = plan(&spec, WICK_VERSION, &host, cache_base.path());
    let PlanDecision::Fetch(p) = decision else {
        panic!("expected a fetch plan for {host}, got {decision:?}");
    };

    let fetcher = bougie::commands::tool_callbacks::native_prefetcher();
    fetcher(&p).expect("prefetch should download, verify, and place the binary");

    let cached = cache_base
        .path()
        .join("wick")
        .join("0.2.1")
        .join(if host.contains("-windows-") {
            "wick.exe"
        } else {
            "wick"
        });
    assert_eq!(p.cache_file, cached);
    let meta = std::fs::metadata(&cached).expect("cached binary exists");
    assert!(
        meta.len() > 1024,
        "suspiciously small binary: {} bytes",
        meta.len()
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            meta.permissions().mode() & 0o111,
            0o111,
            "binary must be executable"
        );
    }
}

/// Fail-closed: when the spec pins a signing repository but the
/// release carries no `.sig` bundle (true for every pre-signing wick
/// release, and for a mirror serving 404s to force a downgrade), the
/// prefetch must error and place nothing in the cache.
#[test]
#[ignore = "downloads a real wick release from the mirror/GitHub"]
fn refuses_unsigned_release_when_signing_required() {
    let spec = wick_spec(Some("cresset-tools/wick".into()));
    let host = bougie_platform::target::Triple::detect()
        .expect("host triple")
        .to_string();
    let cache_base = tempfile::TempDir::new().expect("temp cache base");

    let decision = plan(&spec, WICK_VERSION, &host, cache_base.path());
    let PlanDecision::Fetch(p) = decision else {
        panic!("expected a fetch plan for {host}, got {decision:?}");
    };
    assert!(
        p.signing.is_some(),
        "plan must carry the signing requirement"
    );

    let fetcher = bougie::commands::tool_callbacks::native_prefetcher();
    let err = fetcher(&p).expect_err("unsigned release must fail closed");
    assert!(
        format!("{err:#}").contains("Sigstore"),
        "error should name the missing bundle: {err:#}"
    );
    assert!(
        !p.cache_file.exists(),
        "nothing may reach the cache on a failed verification"
    );
}
