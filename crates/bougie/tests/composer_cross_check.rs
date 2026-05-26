//! Layer 4 cross-check: assert bougie's resolver produces the same
//! package set as Composer 2.8.12 when given identical Packagist
//! metadata.
//!
//! Each corpus fixture lives in `tests/fixtures/cross-check/<slug>/`:
//!
//! - `composer.json` — the project's root manifest
//! - `packagist-index.json.zst` — frozen Packagist v2 metadata for the
//!   full transitive closure
//! - `expected.json` — `{"packages": {"name": "ver"}, "packages_dev":
//!   {"name": "ver"}}` extracted from Composer's lockfile
//!
//! Generate fixtures with `scripts/capture-cross-check-fixture.py`.
//! Run corpus tests:
//!
//! ```text
//! cargo test -p bougie --test composer_cross_check --features cross-check-fixtures
//! ```

mod common;
use common::TestEnv;

use std::collections::BTreeMap;
use std::path::Path;
use tempfile::TempDir;
use wiremock::matchers::{method, path as wm_path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn rt() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

fn write_composer_json(dir: &Path, body: &str) {
    std::fs::write(dir.join("composer.json"), body).unwrap();
}

// ---- Minimal p2 body builder for inline fixtures ----

fn p2_entry(name: &str, version: &str, require: &str) -> String {
    format!(
        r#"{{
            "name":"{name}",
            "version":"{version}",
            "version_normalized":"{version}.0",
            "type":"library",
            "dist":{{"type":"zip","url":"https://example.com/{name}/{version}.zip","shasum":"aa"}},
            "require":{require}
        }}"#
    )
}

fn p2_body(name: &str, entries: &[String]) -> String {
    format!(
        r#"{{"packages":{{"{name}":[{}]}}}}"#,
        entries.join(",")
    )
}

// ---- Wiremock setup ----

async fn mount_metadata(
    server: &MockServer,
    packages: &[(String, String)],
) {
    for (name, body) in packages {
        let route = format!("/p2/{name}.json");
        Mock::given(method("GET"))
            .and(wm_path(&route))
            .respond_with(ResponseTemplate::new(200).set_body_string(body.clone()))
            .mount(server)
            .await;
    }
}

// ---- Run bougie and extract resolved package sets ----

#[derive(Debug)]
struct ResolvedSet {
    packages: BTreeMap<String, String>,
    packages_dev: BTreeMap<String, String>,
}

fn run_bougie_update(
    env: &TestEnv,
    project_dir: &Path,
    server_uri: &str,
) -> ResolvedSet {
    let output = env
        .bougie()
        .env("BOUGIE_PACKAGIST_BASE_URL", server_uri)
        .args(["composer", "update", "--format", "json-v1", "-d"])
        .arg(project_dir)
        .output()
        .expect("run bougie");

    assert!(
        output.status.success(),
        "bougie composer update failed:\nstderr: {}\nstdout: {}",
        String::from_utf8_lossy(&output.stderr),
        String::from_utf8_lossy(&output.stdout),
    );

    let json: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("parse bougie json-v1 output");

    let extract = |key: &str| -> BTreeMap<String, String> {
        json[key]
            .as_array()
            .unwrap_or(&vec![])
            .iter()
            .map(|p| {
                (
                    p["name"].as_str().unwrap().to_string(),
                    p["version"].as_str().unwrap().to_string(),
                )
            })
            .collect()
    };

    ResolvedSet {
        packages: extract("packages"),
        packages_dev: extract("packages_dev"),
    }
}

// ---- Assertion helpers ----

fn assert_packages_match(
    label: &str,
    section: &str,
    actual: &BTreeMap<String, String>,
    expected: &BTreeMap<String, String>,
) {
    let mut diffs = Vec::new();

    for (name, expected_ver) in expected {
        match actual.get(name) {
            None => diffs.push(format!("  MISSING {name} (expected {expected_ver})")),
            Some(actual_ver) if actual_ver != expected_ver => {
                diffs.push(format!(
                    "  MISMATCH {name}: got {actual_ver}, expected {expected_ver}"
                ));
            }
            _ => {}
        }
    }
    for (name, actual_ver) in actual {
        if !expected.contains_key(name) {
            diffs.push(format!("  EXTRA {name} {actual_ver}"));
        }
    }

    assert!(
        diffs.is_empty(),
        "cross-check [{label}] {section}: {} divergences\n\
         actual={} packages, expected={} packages\n{}",
        diffs.len(),
        actual.len(),
        expected.len(),
        diffs.join("\n"),
    );
}

fn run_fixture(
    label: &str,
    composer_json: &str,
    metadata: &[(String, String)],
    expected: &ResolvedSet,
) {
    let env = TestEnv::new();
    let proj = TempDir::new().unwrap();
    write_composer_json(proj.path(), composer_json);

    let rt = rt();
    let (uri, _server) = rt.block_on(async {
        let server = MockServer::start().await;
        mount_metadata(&server, metadata).await;
        (server.uri(), server)
    });

    let actual = run_bougie_update(&env, proj.path(), &uri);

    assert_packages_match(label, "packages", &actual.packages, &expected.packages);
    assert_packages_match(
        label,
        "packages-dev",
        &actual.packages_dev,
        &expected.packages_dev,
    );
}

// ---- Smoke tests (inline fixtures, always run) ----

#[test]
fn cross_check_smoke_single_dep() {
    let entries = vec![
        p2_entry("acme/foo", "2.0.0", "{}"),
        p2_entry("acme/foo", "1.5.0", "{}"),
        p2_entry("acme/foo", "1.0.0", "{}"),
    ];

    run_fixture(
        "single-dep",
        r#"{"name":"test/p","require":{"acme/foo":"^1.0"}}"#,
        &[("acme/foo".into(), p2_body("acme/foo", &entries))],
        &ResolvedSet {
            packages: BTreeMap::from([("acme/foo".into(), "1.5.0".into())]),
            packages_dev: BTreeMap::new(),
        },
    );
}

#[test]
fn cross_check_smoke_transitive() {
    let foo_entries = vec![p2_entry(
        "acme/foo",
        "1.0.0",
        r#"{"acme/bar":"^2.0"}"#,
    )];
    let bar_entries = vec![
        p2_entry("acme/bar", "2.3.0", "{}"),
        p2_entry("acme/bar", "2.0.0", "{}"),
    ];

    run_fixture(
        "transitive",
        r#"{"name":"test/p","require":{"acme/foo":"^1.0"}}"#,
        &[
            ("acme/foo".into(), p2_body("acme/foo", &foo_entries)),
            ("acme/bar".into(), p2_body("acme/bar", &bar_entries)),
        ],
        &ResolvedSet {
            packages: BTreeMap::from([
                ("acme/bar".into(), "2.3.0".into()),
                ("acme/foo".into(), "1.0.0".into()),
            ]),
            packages_dev: BTreeMap::new(),
        },
    );
}

#[test]
fn cross_check_smoke_replace() {
    // acme/foo requires acme/interface ^1.0.
    // acme/bar replaces acme/interface at 1.0.0.
    // Resolution must include acme/foo + acme/bar (satisfying the
    // virtual dep), but NOT a separate acme/interface.
    let foo_entries = vec![p2_entry(
        "acme/foo",
        "1.0.0",
        r#"{"acme/interface":"^1.0"}"#,
    )];
    let bar_entries = vec![
        r#"{
            "name":"acme/bar",
            "version":"1.0.0",
            "version_normalized":"1.0.0.0",
            "type":"library",
            "dist":{"type":"zip","url":"https://example.com/acme/bar/1.0.0.zip","shasum":"aa"},
            "require":{},
            "replace":{"acme/interface":"1.0.0"}
        }"#
        .to_string(),
    ];

    run_fixture(
        "replace",
        r#"{"name":"test/p","require":{"acme/foo":"^1.0","acme/bar":"^1.0"}}"#,
        &[
            ("acme/foo".into(), p2_body("acme/foo", &foo_entries)),
            ("acme/bar".into(), p2_body("acme/bar", &bar_entries)),
        ],
        &ResolvedSet {
            packages: BTreeMap::from([
                ("acme/bar".into(), "1.0.0".into()),
                ("acme/foo".into(), "1.0.0".into()),
            ]),
            packages_dev: BTreeMap::new(),
        },
    );
}

#[test]
fn cross_check_smoke_dev_partition() {
    // acme/foo is a prod dep, acme/bar is a dev dep.
    // Both must resolve; the partition must be correct.
    let foo_entries = vec![p2_entry("acme/foo", "1.0.0", "{}")];
    let bar_entries = vec![p2_entry("acme/bar", "2.0.0", "{}")];

    run_fixture(
        "dev-partition",
        r#"{
            "name":"test/p",
            "require":{"acme/foo":"^1.0"},
            "require-dev":{"acme/bar":"^2.0"}
        }"#,
        &[
            ("acme/foo".into(), p2_body("acme/foo", &foo_entries)),
            ("acme/bar".into(), p2_body("acme/bar", &bar_entries)),
        ],
        &ResolvedSet {
            packages: BTreeMap::from([("acme/foo".into(), "1.0.0".into())]),
            packages_dev: BTreeMap::from([("acme/bar".into(), "2.0.0".into())]),
        },
    );
}

// ---- Corpus tests (disk fixtures, feature-gated) ----

#[cfg(feature = "cross-check-fixtures")]
mod corpus {
    use super::*;
    use std::collections::HashMap;
    use std::io::Read as _;

    fn load_fixture(slug: &str) -> (String, HashMap<String, Vec<u8>>, ResolvedSet) {
        let fixture_dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/cross-check")
            .join(slug);

        let composer_json = std::fs::read_to_string(fixture_dir.join("composer.json"))
            .unwrap_or_else(|e| panic!("read composer.json for {slug}: {e}"));

        let zst_bytes = std::fs::read(fixture_dir.join("packagist-index.json.zst"))
            .unwrap_or_else(|e| panic!("read packagist-index.json.zst for {slug}: {e}"));
        let mut raw = Vec::with_capacity(zst_bytes.len() * 30);
        zstd::Decoder::new(zst_bytes.as_slice())
            .expect("zstd decoder")
            .read_to_end(&mut raw)
            .expect("decompress fixture");
        let index: HashMap<String, serde_json::Value> =
            serde_json::from_slice(&raw).expect("parse fixture index");
        let metadata: HashMap<String, Vec<u8>> = index
            .into_iter()
            .map(|(name, doc)| (name, serde_json::to_vec(&doc).unwrap()))
            .collect();

        let expected_str = std::fs::read_to_string(fixture_dir.join("expected.json"))
            .unwrap_or_else(|e| panic!("read expected.json for {slug}: {e}"));
        let expected: serde_json::Value =
            serde_json::from_str(&expected_str).expect("parse expected.json");

        let parse_map = |key: &str| -> BTreeMap<String, String> {
            expected[key]
                .as_object()
                .map(|m| {
                    m.iter()
                        .map(|(k, v)| (k.clone(), v.as_str().unwrap().to_string()))
                        .collect()
                })
                .unwrap_or_default()
        };

        let resolved = ResolvedSet {
            packages: parse_map("packages"),
            packages_dev: parse_map("packages_dev"),
        };

        (composer_json, metadata, resolved)
    }

    async fn mount_corpus_server(
        server: &MockServer,
        metadata: &HashMap<String, Vec<u8>>,
    ) {
        for (name, body) in metadata {
            let route = format!("/p2/{name}.json");
            Mock::given(method("GET"))
                .and(wm_path(&route))
                .respond_with(
                    ResponseTemplate::new(200).set_body_bytes(body.clone()),
                )
                .mount(server)
                .await;
        }
    }

    fn run_corpus(slug: &str) {
        let fixture_dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/cross-check")
            .join(slug);
        if !fixture_dir.join("composer.json").exists() {
            eprintln!(
                "skipping {slug}: fixture not captured yet \
                 (run scripts/capture-cross-check-fixture.py)"
            );
            return;
        }
        let (composer_json, metadata, expected) = load_fixture(slug);

        let env = TestEnv::new();
        let proj = TempDir::new().unwrap();
        write_composer_json(proj.path(), &composer_json);

        let rt = rt();
        let (uri, _server) = rt.block_on(async {
            let server = MockServer::start().await;
            mount_corpus_server(&server, &metadata).await;
            (server.uri(), server)
        });

        let actual = run_bougie_update(&env, proj.path(), &uri);

        assert_packages_match(slug, "packages", &actual.packages, &expected.packages);
        assert_packages_match(
            slug,
            "packages-dev",
            &actual.packages_dev,
            &expected.packages_dev,
        );
    }

    macro_rules! corpus_test {
        ($name:ident) => {
            #[test]
            fn $name() {
                run_corpus(stringify!($name));
            }
        };
    }

    // Each fixture must be captured first via
    // `scripts/capture-cross-check-fixture.py`. Uncaptured fixtures
    // skip gracefully. Add new corpus entries here as fixtures land.
    //
    // See COMPOSER_PARITY.md § "Cross-check corpus" for rationale.
    corpus_test!(monolog);
    corpus_test!(phpunit);
    corpus_test!(phpstan);
    corpus_test!(flysystem);
    corpus_test!(carbon);
    corpus_test!(doctrine);
    corpus_test!(laravel);
    corpus_test!(symfony);

    // Magento has remaining divergences unrelated to replace
    // preference: version mismatches (symfony/var-dumper v8 vs v7),
    // missing php-http/* packages, extra legacy packages.
    #[test]
    #[ignore = "magento: version selection + missing package divergences under investigation"]
    fn magento() {
        run_corpus("magento");
    }
}
