//! Unit tests for the parallel dist downloader.
//!
//! Each test spins up a `wiremock` server (already in the workspace
//! lockfile, used by `phase7_sync`) on a per-test tokio runtime. The
//! production code is blocking so the test driver constructs the
//! runtime, sets up the mock, then calls into the blocking
//! `fetch_and_extract_dists` from the main thread.

use super::*;
use bougie_fetch::{ArchiveKind, DownloadBar};
use bougie_paths::Paths;
use sha1::Digest as _;
use std::io::Write as _;
use tempfile::TempDir;
use wiremock::matchers::{header, method, path as wm_path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn rt() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

fn sha1_hex(bytes: &[u8]) -> String {
    let digest = sha1::Sha1::digest(bytes);
    let mut s = String::with_capacity(40);
    for b in digest {
        use std::fmt::Write as _;
        let _ = write!(s, "{b:02x}");
    }
    s
}

/// Build a small zip whose entries live under `acme-foo-abc1234/` so
/// `strip_prefix = "acme-foo-abc1234"` lands them at the dest root —
/// mirrors Packagist's standard dist layout.
fn build_fixture_zip(top: &str) -> Vec<u8> {
    let mut buf: Vec<u8> = Vec::new();
    {
        let cursor = std::io::Cursor::new(&mut buf);
        let mut zw = zip::ZipWriter::new(cursor);
        let opts = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        zw.start_file(format!("{top}/composer.json"), opts).unwrap();
        zw.write_all(br#"{"name":"acme/foo"}"#).unwrap();
        zw.start_file(format!("{top}/src/Foo.php"), opts).unwrap();
        zw.write_all(b"<?php class Foo {}\n").unwrap();
        zw.finish().unwrap();
    }
    buf
}

/// Build a gzip-compressed tar whose entries live under `top/` — the
/// shape a Composer `tar` dist (satis / private repo / commercial vendor)
/// takes. Compression is deliberately not reflected in the extension; the
/// extractor sniffs it, like PHP's `PharData`.
fn build_fixture_tar_gz(top: &str) -> Vec<u8> {
    let mut tar_buf: Vec<u8> = Vec::new();
    {
        let mut b = tar::Builder::new(&mut tar_buf);
        for (name, body) in [
            ("composer.json", &br#"{"name":"acme/foo"}"#[..]),
            ("src/Foo.php", &b"<?php class Foo {}\n"[..]),
        ] {
            let mut h = tar::Header::new_gnu();
            h.set_size(body.len() as u64);
            h.set_mode(0o644);
            h.set_cksum();
            b.append_data(&mut h, format!("{top}/{name}"), body).unwrap();
        }
        b.finish().unwrap();
    }
    let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    enc.write_all(&tar_buf).unwrap();
    enc.finish().unwrap()
}

/// Build a [`Paths`] rooted at `tmp` so the test owns its cache dir.
fn paths_in(tmp: &Path) -> Paths {
    let home = tmp.join("home");
    let cache = tmp.join("cache");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&cache).unwrap();
    Paths::new(home, cache)
}

#[test]
fn downloads_single_zip_and_extracts_with_strip_prefix() {
    let tmp = TempDir::new().unwrap();
    let paths = paths_in(tmp.path());
    let vendor_dest = tmp.path().join("vendor").join("acme").join("foo");

    let body = build_fixture_zip("acme-foo-abc1234");
    let hash = sha1_hex(&body);

    let rt = rt();
    let (uri, _server) = rt.block_on(async {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(wm_path("/dists/acme-foo.zip"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(body.clone()))
            .mount(&server)
            .await;
        let uri = server.uri();
        (uri, server)
    });

    let url = format!("{uri}/dists/acme-foo.zip");
    let client = reqwest::blocking::Client::new();
    let bar = DownloadBar::hidden();
    let dists = [DistRequest {
        package_name: "acme/foo",
        url: &url,
        sha1: &hash,
        reference: "",
        archive: ArchiveKind::Zip,
        strip_prefix: Some("acme-foo-abc1234"),
        vendor_dest: &vendor_dest,
        auth_header: None,
            auth_header_name: None,
            project_root: tmp.path(),
            fallbacks: &[],
    }];

    fetch_and_extract_dists(&client, &paths, &dists, &bar).unwrap();

    assert!(vendor_dest.join("composer.json").is_file());
    assert!(vendor_dest.join("src/Foo.php").is_file());
    assert!(!vendor_dest.join("acme-foo-abc1234").exists());
    let cached = paths.cache_composer_dist().join(format!("{hash}.zip"));
    assert!(cached.is_file(), "cache file should be retained for reuse");
}

#[test]
fn downloads_tar_dist_and_extracts_with_detected_prefix() {
    // Issue #420: a `tar` dist (here gzip-compressed, wrapper name only
    // known at runtime). `strip_prefix: None` exercises detection, and the
    // Tar arm sniffs the codec.
    let tmp = TempDir::new().unwrap();
    let paths = paths_in(tmp.path());
    let vendor_dest = tmp.path().join("vendor").join("mirasvit").join("module-email");

    let top = "mirasvit-module-email-abc1234";
    let body = build_fixture_tar_gz(top);
    let hash = sha1_hex(&body);

    let rt = rt();
    let (uri, _server) = rt.block_on(async {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(wm_path("/dists/module-email.tar"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(body.clone()))
            .mount(&server)
            .await;
        let uri = server.uri();
        (uri, server)
    });

    let url = format!("{uri}/dists/module-email.tar");
    let client = reqwest::blocking::Client::new();
    let bar = DownloadBar::hidden();
    let dists = [DistRequest {
        package_name: "mirasvit/module-email",
        url: &url,
        sha1: &hash,
        reference: "",
        archive: ArchiveKind::Tar,
        strip_prefix: None,
        vendor_dest: &vendor_dest,
        auth_header: None,
        auth_header_name: None,
        project_root: tmp.path(),
        fallbacks: &[],
    }];

    fetch_and_extract_dists(&client, &paths, &dists, &bar).unwrap();

    assert!(vendor_dest.join("composer.json").is_file());
    assert!(vendor_dest.join("src/Foo.php").is_file());
    assert!(!vendor_dest.join(top).exists(), "wrapper dir should be stripped");
    let cached = paths.cache_composer_dist().join(format!("{hash}.tar"));
    assert!(cached.is_file(), "tar dist should be cached under a .tar key");
}

#[test]
fn falls_back_to_next_candidate_when_primary_url_fails() {
    // Composer's dist-mirror semantics: the orchestrator puts a
    // preferred mirror in `url` and the remaining candidates in
    // `fallbacks`. A 404 from the first candidate must not fail the
    // install — the downloader moves on, and each candidate carries
    // its *own* pre-rendered auth (mirror and origin usually live on
    // different hosts with different credentials).
    let tmp = TempDir::new().unwrap();
    let paths = paths_in(tmp.path());
    let vendor_dest = tmp.path().join("vendor").join("acme").join("foo");

    let body = build_fixture_zip("acme-foo-abc1234");
    let hash = sha1_hex(&body);

    let rt = rt();
    let (uri, _server) = rt.block_on(async {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(wm_path("/mirror/acme-foo.zip"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;
        // "token:hunter2" — the fallback requires its own credentials.
        Mock::given(method("GET"))
            .and(wm_path("/origin/acme-foo.zip"))
            .and(header("authorization", "Basic dG9rZW46aHVudGVyMg=="))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(body.clone()))
            .mount(&server)
            .await;
        let uri = server.uri();
        (uri, server)
    });

    let primary = format!("{uri}/mirror/acme-foo.zip");
    let fallbacks = [DistCandidate {
        url: format!("{uri}/origin/acme-foo.zip"),
        auth_header: Some("Basic dG9rZW46aHVudGVyMg==".to_owned()),
        auth_header_name: Some("authorization"),
    }];
    let client = reqwest::blocking::Client::new();
    let bar = DownloadBar::hidden();
    let dists = [DistRequest {
        package_name: "acme/foo",
        url: &primary,
        sha1: &hash,
        reference: "",
        archive: ArchiveKind::Zip,
        strip_prefix: Some("acme-foo-abc1234"),
        vendor_dest: &vendor_dest,
        auth_header: None,
        auth_header_name: None,
        project_root: tmp.path(),
        fallbacks: &fallbacks,
    }];

    let outcomes = fetch_and_extract_dists(&client, &paths, &dists, &bar).unwrap();
    assert_eq!(outcomes.len(), 1);
    assert!(
        matches!(outcomes[0], DistOutcome::Downloaded { bytes } if bytes > 0),
        "{outcomes:?}"
    );
    assert!(vendor_dest.join("composer.json").is_file());
}

#[test]
fn all_candidates_failing_surfaces_candidate_count() {
    // When every candidate URL fails, the error must say so — the
    // per-URL context alone would name only the *last* URL tried,
    // hiding that a mirror was attempted first.
    let tmp = TempDir::new().unwrap();
    let paths = paths_in(tmp.path());
    let vendor_dest = tmp.path().join("vendor").join("acme").join("foo");

    let rt = rt();
    let (uri, _server) = rt.block_on(async {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;
        let uri = server.uri();
        (uri, server)
    });

    let primary = format!("{uri}/mirror/acme-foo.zip");
    let fallbacks = [DistCandidate {
        url: format!("{uri}/origin/acme-foo.zip"),
        auth_header: None,
        auth_header_name: None,
    }];
    let client = reqwest::blocking::Client::new();
    let bar = DownloadBar::hidden();
    let dists = [DistRequest {
        package_name: "acme/foo",
        url: &primary,
        sha1: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        reference: "",
        archive: ArchiveKind::Zip,
        strip_prefix: None,
        vendor_dest: &vendor_dest,
        auth_header: None,
        auth_header_name: None,
        project_root: tmp.path(),
        fallbacks: &fallbacks,
    }];

    let err = fetch_and_extract_dists(&client, &paths, &dists, &bar)
        .expect_err("every candidate 404s");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("all 2 candidate URLs failed"),
        "expected candidate-count context in error, got: {msg}"
    );
    assert!(
        msg.contains("/origin/acme-foo.zip"),
        "expected last-tried URL in error chain, got: {msg}"
    );
}

#[test]
fn cache_hit_short_circuits_network() {
    // Pre-populate the cache; the mock server returns 500 if hit, so
    // any HTTP attempt would fail the test loudly.
    let tmp = TempDir::new().unwrap();
    let paths = paths_in(tmp.path());
    let vendor_dest = tmp.path().join("vendor").join("acme").join("foo");

    let body = build_fixture_zip("acme-foo-deadbeef");
    let hash = sha1_hex(&body);
    let cache_dir = paths.cache_composer_dist();
    std::fs::create_dir_all(&cache_dir).unwrap();
    std::fs::write(cache_dir.join(format!("{hash}.zip")), &body).unwrap();

    let rt = rt();
    let (uri, _server) = rt.block_on(async {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(wm_path("/dists/acme-foo.zip"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;
        let uri = server.uri();
        (uri, server)
    });

    let url = format!("{uri}/dists/acme-foo.zip");
    let client = reqwest::blocking::Client::new();
    let bar = DownloadBar::hidden();
    let dists = [DistRequest {
        package_name: "acme/foo",
        url: &url,
        sha1: &hash,
        reference: "",
        archive: ArchiveKind::Zip,
        strip_prefix: Some("acme-foo-deadbeef"),
        vendor_dest: &vendor_dest,
        auth_header: None,
            auth_header_name: None,
            project_root: tmp.path(),
            fallbacks: &[],
    }];

    let outcomes = fetch_and_extract_dists(&client, &paths, &dists, &bar).unwrap();
    assert_eq!(outcomes, vec![DistOutcome::CacheHit]);
    assert!(vendor_dest.join("composer.json").is_file());
}

#[test]
fn hash_mismatch_aborts_install_cleanly() {
    let tmp = TempDir::new().unwrap();
    let paths = paths_in(tmp.path());
    let vendor_dest = tmp.path().join("vendor").join("acme").join("foo");

    let body = build_fixture_zip("acme-foo-aaaa");
    // Claim the wrong hash — bougie-fetch must reject the bytes and
    // leave neither a `.partial` nor a cached zip behind.
    let wrong_hash = "0000000000000000000000000000000000000000";

    let rt = rt();
    let (uri, _server) = rt.block_on(async {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(wm_path("/dists/acme-foo.zip"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(body))
            .mount(&server)
            .await;
        let uri = server.uri();
        (uri, server)
    });

    let url = format!("{uri}/dists/acme-foo.zip");
    let client = reqwest::blocking::Client::new();
    let bar = DownloadBar::hidden();
    let dists = [DistRequest {
        package_name: "acme/foo",
        url: &url,
        sha1: wrong_hash,
        reference: "",
        archive: ArchiveKind::Zip,
        strip_prefix: Some("acme-foo-aaaa"),
        vendor_dest: &vendor_dest,
        auth_header: None,
            auth_header_name: None,
            project_root: tmp.path(),
            fallbacks: &[],
    }];

    let err = fetch_and_extract_dists(&client, &paths, &dists, &bar)
        .expect_err("hash mismatch must error");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("hash") || msg.contains("Hash") || msg.contains("sha1"),
        "expected hash-mismatch context in error, got: {msg}"
    );

    // No cached zip, no leftover `.partial`. The directory might exist
    // (we mkdir it before the fetch) but it must not contain the
    // failed download under either filename.
    let cache_dir = paths.cache_composer_dist();
    let cached = cache_dir.join(format!("{wrong_hash}.zip"));
    let partial = cache_dir.join(format!("{wrong_hash}.partial"));
    assert!(!cached.exists(), "no cached zip for failed hash");
    assert!(!partial.exists(), "no leftover .partial");
    // `vendor_dest` must not exist either — we never get to extract.
    assert!(!vendor_dest.exists());
}

#[test]
fn parallel_four_dists_share_one_bar() {
    let tmp = TempDir::new().unwrap();
    let paths = paths_in(tmp.path());

    // Four distinct packages, each with its own fixture zip and dest.
    let pkgs: Vec<(String, String, Vec<u8>)> = (0..4)
        .map(|i| {
            let top = format!("acme-pkg{i}-aaaa");
            let body = build_fixture_zip(&top);
            let hash = sha1_hex(&body);
            (top, hash, body)
        })
        .collect();

    let rt = rt();
    let (uri, _server) = rt.block_on(async {
        let server = MockServer::start().await;
        for (i, (_, _, body)) in pkgs.iter().enumerate() {
            Mock::given(method("GET"))
                .and(wm_path(format!("/p{i}.zip")))
                .respond_with(ResponseTemplate::new(200).set_body_bytes(body.clone()))
                .mount(&server)
                .await;
        }
        let uri = server.uri();
        (uri, server)
    });

    let urls: Vec<String> = (0..4).map(|i| format!("{uri}/p{i}.zip")).collect();
    let names: Vec<String> = (0..4).map(|i| format!("acme/pkg{i}")).collect();
    let dests: Vec<PathBuf> = (0..4)
        .map(|i| tmp.path().join("vendor").join("acme").join(format!("pkg{i}")))
        .collect();

    let dists: Vec<DistRequest<'_>> = (0..4)
        .map(|i| DistRequest {
            package_name: &names[i],
            url: &urls[i],
            sha1: &pkgs[i].1,
            reference: "",
            archive: ArchiveKind::Zip,
            strip_prefix: Some(&pkgs[i].0),
            vendor_dest: &dests[i],
            auth_header: None,
            auth_header_name: None,
            project_root: tmp.path(),
            fallbacks: &[],
        })
        .collect();

    let client = reqwest::blocking::Client::new();
    let bar = DownloadBar::hidden();
    fetch_and_extract_dists(&client, &paths, &dists, &bar).unwrap();

    for dest in &dests {
        assert!(dest.join("composer.json").is_file());
        assert!(dest.join("src/Foo.php").is_file());
    }
}

#[test]
fn extract_strips_top_level_directory_via_auto_detect() {
    // Tighter check than the success test: verify that *no* file with
    // the top-level directory component as a path segment survives in
    // `vendor_dest`. Also exercises the `strip_prefix = None` branch
    // (the production path — the install command never knows the
    // wrapping dir up-front, so it always lets the downloader detect).
    let tmp = TempDir::new().unwrap();
    let paths = paths_in(tmp.path());
    let vendor_dest = tmp.path().join("vendor").join("acme").join("strip");

    let top = "acme-strip-9876543";
    let body = build_fixture_zip(top);
    let hash = sha1_hex(&body);

    let rt = rt();
    let (uri, _server) = rt.block_on(async {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(wm_path("/d.zip"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(body))
            .mount(&server)
            .await;
        (server.uri(), server)
    });

    let url = format!("{uri}/d.zip");
    let client = reqwest::blocking::Client::new();
    let bar = DownloadBar::hidden();
    let dists = [DistRequest {
        package_name: "acme/strip",
        url: &url,
        sha1: &hash,
        reference: "",
        archive: ArchiveKind::Zip,
        strip_prefix: None,
        vendor_dest: &vendor_dest,
        auth_header: None,
            auth_header_name: None,
            project_root: tmp.path(),
            fallbacks: &[],
    }];
    let outcomes = fetch_and_extract_dists(&client, &paths, &dists, &bar).unwrap();
    assert_eq!(outcomes.len(), 1);
    assert!(
        matches!(outcomes[0], DistOutcome::Downloaded { bytes } if bytes > 0),
        "{outcomes:?}"
    );

    // Walk vendor_dest; assert no path component equals the stripped
    // top-level name.
    let mut count = 0usize;
    for entry in walkdir(&vendor_dest) {
        count += 1;
        for part in entry.components() {
            if let std::path::Component::Normal(p) = part {
                assert_ne!(p.to_str(), Some(top), "stripped prefix leaked: {entry:?}");
            }
        }
    }
    assert!(count > 0, "no files extracted");
}

#[test]
fn dist_request_auth_header_is_sent_on_get() {
    // Wiremock only responds with the ZIP when the request carries
    // the exact `Authorization` header the caller pre-rendered. With
    // it, download + extract succeeds. Without it (the default
    // `auth_header: None` everywhere else), the same wiremock would
    // fall through to a 401 — proves the field is actually wired
    // through to the HTTP request.
    let tmp = TempDir::new().unwrap();
    let paths = paths_in(tmp.path());
    let vendor_dest = tmp.path().join("vendor").join("acme").join("foo");

    let body = build_fixture_zip("acme-foo-abc");
    let hash = sha1_hex(&body);

    let rt = rt();
    let (uri, _server) = rt.block_on(async {
        let server = MockServer::start().await;
        // Auth-gated path: only matches when the header is present
        // and exact. Register *first* so wiremock evaluates it before
        // the default 401 fall-through.
        Mock::given(method("GET"))
            .and(wm_path("/dists/acme-foo.zip"))
            .and(header("Authorization", "Basic dXNlcjpwYXNz"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(body.clone()))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(wm_path("/dists/acme-foo.zip"))
            .respond_with(ResponseTemplate::new(401))
            .mount(&server)
            .await;
        (server.uri(), server)
    });

    let url = format!("{uri}/dists/acme-foo.zip");
    let client = reqwest::blocking::Client::new();
    let bar = DownloadBar::hidden();
    let auth = "Basic dXNlcjpwYXNz";
    let dists = [DistRequest {
        package_name: "acme/foo",
        url: &url,
        sha1: &hash,
        reference: "",
        archive: ArchiveKind::Zip,
        strip_prefix: Some("acme-foo-abc"),
        vendor_dest: &vendor_dest,
        auth_header: Some(auth),
        auth_header_name: None,
            project_root: tmp.path(),
            fallbacks: &[],
    }];

    fetch_and_extract_dists(&client, &paths, &dists, &bar)
        .expect("download must succeed when the Authorization header matches");
    assert!(vendor_dest.join("composer.json").is_file());
}

#[test]
fn dist_request_without_auth_fails_when_server_requires_it() {
    // Mirror of the above with `auth_header: None`: the wiremock now
    // matches only the unauthenticated path (401), and the install
    // surfaces an HTTP error rather than silently succeeding.
    let tmp = TempDir::new().unwrap();
    let paths = paths_in(tmp.path());
    let vendor_dest = tmp.path().join("vendor").join("acme").join("foo");

    let body = build_fixture_zip("acme-foo-abc");
    let hash = sha1_hex(&body);

    let rt = rt();
    let (uri, _server) = rt.block_on(async {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(wm_path("/dists/acme-foo.zip"))
            .and(header("Authorization", "Basic dXNlcjpwYXNz"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(body.clone()))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(wm_path("/dists/acme-foo.zip"))
            .respond_with(ResponseTemplate::new(401))
            .mount(&server)
            .await;
        (server.uri(), server)
    });

    let url = format!("{uri}/dists/acme-foo.zip");
    let client = reqwest::blocking::Client::new();
    let bar = DownloadBar::hidden();
    let dists = [DistRequest {
        package_name: "acme/foo",
        url: &url,
        sha1: &hash,
        reference: "",
        archive: ArchiveKind::Zip,
        strip_prefix: Some("acme-foo-abc"),
        vendor_dest: &vendor_dest,
        auth_header: None,
            auth_header_name: None,
            project_root: tmp.path(),
            fallbacks: &[],
    }];

    let err = fetch_and_extract_dists(&client, &paths, &dists, &bar)
        .expect_err("unauthenticated request must fail");
    let msg = format!("{err:#}");
    assert!(msg.contains("401"), "{msg}");
}

#[test]
fn rewrite_github_api_zipball_to_codeload() {
    let url = "https://api.github.com/repos/Seldaek/monolog/zipball/c915e2634718dbc8a4a15c61b0e62e7a44e14448";
    let rewritten = rewrite_github_dist_url(url);
    assert_eq!(
        rewritten,
        "https://codeload.github.com/Seldaek/monolog/legacy.zip/c915e2634718dbc8a4a15c61b0e62e7a44e14448",
    );
}

#[test]
fn rewrite_leaves_non_github_urls_unchanged() {
    let urls = [
        "https://repo.packagist.org/archives/vendor/pkg.zip",
        "https://gitlab.example.com/api/v4/projects/1/packages/composer/archives/foo.zip",
        "https://example.test/acme-foo.zip",
    ];
    for url in urls {
        assert_eq!(rewrite_github_dist_url(url).as_ref(), url);
    }
}

#[test]
fn rewrite_leaves_github_non_zipball_urls_unchanged() {
    let url = "https://api.github.com/repos/owner/repo/tarball/abc123";
    assert_eq!(rewrite_github_dist_url(url).as_ref(), url);
}

#[test]
fn rewrite_handles_org_scoped_repos() {
    let url = "https://api.github.com/repos/symfony/console/zipball/3156577f46a38aa1b9323aad223de7a9cd426782";
    assert_eq!(
        rewrite_github_dist_url(url),
        "https://codeload.github.com/symfony/console/legacy.zip/3156577f46a38aa1b9323aad223de7a9cd426782",
    );
}

#[test]
fn local_artifact_dist_is_copied_and_extracted() {
    let tmp = TempDir::new().unwrap();
    let paths = paths_in(tmp.path());
    let vendor_dest = tmp.path().join("vendor").join("vsourz").join("imagegallery");

    let body = build_fixture_zip("acme-foo-abc1234");
    let hash = sha1_hex(&body);

    // Project layout: `<root>/artifacts/vsourz-imagegallery-1.0.1-p1.zip`,
    // with the URL stored relative — exactly how Composer's
    // `type: artifact` repository serializes into composer.lock.
    let artifacts_dir = tmp.path().join("artifacts");
    std::fs::create_dir_all(&artifacts_dir).unwrap();
    let zip_path = artifacts_dir.join("vsourz-imagegallery-1.0.1-p1.zip");
    std::fs::write(&zip_path, &body).unwrap();

    let url = "artifacts/vsourz-imagegallery-1.0.1-p1.zip";
    let client = reqwest::blocking::Client::new();
    let bar = DownloadBar::hidden();
    let dists = [DistRequest {
        package_name: "vsourz/imagegallery",
        url,
        sha1: &hash,
        reference: "",
        archive: ArchiveKind::Zip,
        strip_prefix: Some("acme-foo-abc1234"),
        vendor_dest: &vendor_dest,
        auth_header: None,
        auth_header_name: None,
        project_root: tmp.path(),
        fallbacks: &[],
    }];

    fetch_and_extract_dists(&client, &paths, &dists, &bar).unwrap();

    assert!(vendor_dest.join("composer.json").is_file());
    assert!(vendor_dest.join("src/Foo.php").is_file());
    let cached = paths.cache_composer_dist().join(format!("{hash}.zip"));
    assert!(cached.is_file(), "expected cached copy at {}", cached.display());
}

#[test]
fn local_artifact_dist_missing_file_errors_clearly() {
    let tmp = TempDir::new().unwrap();
    let paths = paths_in(tmp.path());
    let vendor_dest = tmp.path().join("vendor").join("acme").join("missing");

    let client = reqwest::blocking::Client::new();
    let bar = DownloadBar::hidden();
    let dists = [DistRequest {
        package_name: "acme/missing",
        url: "artifacts/does-not-exist.zip",
        sha1: "0000000000000000000000000000000000000000",
        reference: "",
        archive: ArchiveKind::Zip,
        strip_prefix: Some("x"),
        vendor_dest: &vendor_dest,
        auth_header: None,
        auth_header_name: None,
        project_root: tmp.path(),
        fallbacks: &[],
    }];

    let err = fetch_and_extract_dists(&client, &paths, &dists, &bar)
        .expect_err("missing artifact must surface as an error");
    let msg = format!("{err:#}");
    assert!(msg.contains("type: artifact"), "{msg}");
    assert!(msg.contains("does-not-exist.zip"), "{msg}");
}

#[test]
fn local_artifact_dist_sha1_mismatch_errors() {
    let tmp = TempDir::new().unwrap();
    let paths = paths_in(tmp.path());
    let vendor_dest = tmp.path().join("vendor").join("acme").join("foo");

    let body = build_fixture_zip("acme-foo-abc1234");
    let artifacts_dir = tmp.path().join("artifacts");
    std::fs::create_dir_all(&artifacts_dir).unwrap();
    let zip_path = artifacts_dir.join("acme-foo.zip");
    std::fs::write(&zip_path, &body).unwrap();

    let client = reqwest::blocking::Client::new();
    let bar = DownloadBar::hidden();
    let dists = [DistRequest {
        package_name: "acme/foo",
        url: "artifacts/acme-foo.zip",
        sha1: "deadbeefdeadbeefdeadbeefdeadbeefdeadbeef",
        reference: "",
        archive: ArchiveKind::Zip,
        strip_prefix: Some("acme-foo-abc1234"),
        vendor_dest: &vendor_dest,
        auth_header: None,
        auth_header_name: None,
        project_root: tmp.path(),
        fallbacks: &[],
    }];

    let err = fetch_and_extract_dists(&client, &paths, &dists, &bar)
        .expect_err("sha1 mismatch must surface");
    let msg = format!("{err:#}");
    assert!(msg.contains("sha1 mismatch"), "{msg}");
}

/// Minimal recursive walk: returns every path under `root` relative
/// to `root`. Avoids pulling `walkdir` into the test deps just for
/// this one assertion.
fn walkdir(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in rd.flatten() {
            let p = entry.path();
            let rel = p.strip_prefix(root).unwrap().to_path_buf();
            if p.is_dir() {
                stack.push(p);
            } else {
                out.push(rel);
            }
        }
    }
    out
}

#[test]
fn cache_key_distinguishes_dists_without_shasum_or_reference() {
    // A Composer `type: package` repo entry can carry only `dist.url` +
    // `dist.type` — no shasum, no reference. Hashing the (empty)
    // reference alone collapsed every such dist onto one cache file, so
    // the first package's archive got reused for all the others. The key
    // must fold in the package name + URL so distinct packages differ.
    let tmp = TempDir::new().unwrap();
    let cache_root = tmp.path();
    let vendor = tmp.path().join("vendor");
    let proj = tmp.path();

    let js = DistRequest {
        package_name: "acme/js-widget",
        url: "https://example.test/js-widget.zip",
        sha1: "",
        reference: "",
        archive: ArchiveKind::Zip,
        strip_prefix: None,
        vendor_dest: &vendor,
        auth_header: None,
        auth_header_name: None,
        project_root: proj,
        fallbacks: &[],
    };
    let css = DistRequest {
        package_name: "acme/css-kit",
        url: "https://example.test/css-kit.zip",
        ..js
    };

    let js_path = cache_path_for(cache_root, &js);
    let css_path = cache_path_for(cache_root, &css);
    assert_ne!(
        js_path, css_path,
        "distinct no-shasum/no-reference dists must not share a cache file"
    );

    // A shasum still content-addresses (and wins over the url fallback).
    let hashed = DistRequest { sha1: "abc123def456", ..js };
    assert_eq!(
        cache_path_for(cache_root, &hashed),
        cache_root.join("abc123def456.zip")
    );
}
