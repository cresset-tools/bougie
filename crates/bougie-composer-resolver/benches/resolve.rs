//! Solve-phase microbenchmark for the magento2 closure.
//!
//! Reference number for the `RESOLVER_PERF_PLAN.md` PR series:
//! every later PR in that plan cites the improvement against the
//! baseline this bench reports. The bench times only the post-
//! pre-fetch `pubgrub::resolve` call; the metadata fan-out (already
//! parallel + network-bound) is excluded.
//!
//! Fixture: `tests/fixtures/magento2/composer.json` pulls in
//! `magento/community-edition 2.4.9` (released 2026-05-04). The
//! `packagist-index.json.zst` next to it is a captured snapshot of
//! the full transitive closure as of that date — 2510 packages, the
//! same shape the resolver sees against live packagist.org for a
//! real magento2 project. Field set is trimmed to what the solver
//! reads (`name`, `version`, `version_normalized`, `require`,
//! `require-dev`, `replace`, `provide`); dist / autoload / source /
//! extra / time are dropped so the on-disk fixture stays under 1 MB
//! compressed. See `scripts/capture-magento2-fixture.py` +
//! `scripts/trim-magento2-fixture.py` +
//! `scripts/consolidate-magento2-fixture.py` to refresh it.
//!
//! Gated behind the `bench-fixtures` cargo feature so the captured
//! bytes don't ship with `cargo install` / `cargo build`. Run with:
//!
//! ```text
//! cargo bench -p bougie-composer-resolver \
//!     --features bench-fixtures --bench resolve
//! ```

use std::collections::HashMap;
use std::hint::black_box;
use std::io::Read;
use std::sync::Arc;

use bougie_composer_resolver::metadata::{build_client, Repo};
use bougie_composer_resolver::update::ResolveProvider;
use bougie_composer_resolver::verify::PubGrubPackage;
use bougie_paths::Paths;
use criterion::{criterion_group, criterion_main, BatchSize, Criterion};
use pubgrub::resolve;
use serde_json::Value;
use tempfile::TempDir;
use wiremock::matchers::{method, path as wm_path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// The captured magento2 closure, zstd-compressed. Embedded directly
/// in the bench binary so the wiremock server can serve every package
/// without filesystem I/O on the hot path.
const INDEX_ZST: &[u8] = include_bytes!(
    "../tests/fixtures/magento2/packagist-index.json.zst"
);

const COMPOSER_JSON: &str =
    include_str!("../tests/fixtures/magento2/composer.json");

/// Decompress the consolidated fixture into a `name -> /p2 doc body`
/// map. Each value is the JSON body wiremock will serve at
/// `/p2/<name>.json`. We hand wiremock the bytes per route up front;
/// the route-matching cost is negligible compared to the resolve
/// itself.
fn load_fixture() -> HashMap<String, Vec<u8>> {
    let mut raw = Vec::with_capacity(INDEX_ZST.len() * 30);
    zstd::Decoder::new(INDEX_ZST)
        .expect("zstd decoder")
        .read_to_end(&mut raw)
        .expect("decompress fixture");
    let index: HashMap<String, Value> =
        serde_json::from_slice(&raw).expect("parse fixture index");

    // Each entry is a complete /p2/<name>.json document keyed by
    // `<vendor>/<name>`. Serialize each body once so the wiremock
    // handler can return a ready-made byte slice.
    index
        .into_iter()
        .map(|(name, doc)| {
            let body = serde_json::to_vec(&doc).expect("re-serialize");
            (name, body)
        })
        .collect()
}

/// Stand up a wiremock server with one route per fixture package.
/// Reused across every bench iteration — only the `ResolveProvider`
/// (and its caches) is rebuilt each iteration, so this setup cost is
/// paid once.
async fn mount_fixture_server(bodies: &HashMap<String, Vec<u8>>) -> MockServer {
    let server = MockServer::start().await;
    for (name, body) in bodies {
        // Each /p2/<vendor>/<package>.json route returns the
        // pre-serialized body. `take(usize::MAX)` keeps it streaming-
        // friendly even though we hand wiremock the full vec.
        let route = format!("/p2/{name}.json");
        Mock::given(method("GET"))
            .and(wm_path(route))
            .respond_with(
                ResponseTemplate::new(200).set_body_bytes(body.clone()),
            )
            .mount(&server)
            .await;
    }
    server
}

/// Construct a fresh `ResolveProvider` against the wiremock server
/// and drive its pre-fetch closure so the cache is fully populated.
/// Returns a provider ready to feed to `pubgrub::resolve`.
///
/// `criterion::iter_batched` invokes this once per measurement to
/// reset all of the provider's `RefCell` caches — what the perf
/// plan's PRs actually target.
fn build_and_prefetch(
    server_uri: &str,
    composer_json: &Value,
    tmp_paths: &Paths,
) -> ResolveProvider {
    let client = build_client().expect("build_client");
    let provider = ResolveProvider::build(
        client,
        tmp_paths.clone(),
        Repo::from_url(server_uri),
        composer_json,
        /* no_dev = */ true,
    )
    .expect("ResolveProvider::build");
    provider
        .pre_fetch_closure_silent()
        .expect("pre_fetch_closure");
    provider
}

fn resolve_magento2(c: &mut Criterion) {
    let bodies = load_fixture();
    let composer_json: Value =
        serde_json::from_str(COMPOSER_JSON).expect("parse composer.json");

    // Tokio runtime for wiremock + reqwest's async hooks. Reused
    // across all iterations.
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    let server = rt.block_on(mount_fixture_server(&bodies));
    let server_uri = server.uri();

    let tmp = TempDir::new().expect("tempdir");
    let paths = Paths::new(
        tmp.path().join("home"),
        tmp.path().join("cache"),
    );
    std::fs::create_dir_all(tmp.path().join("home")).unwrap();
    std::fs::create_dir_all(tmp.path().join("cache")).unwrap();
    let paths = Arc::new(paths);

    // Keep server + tmp alive for the bench's lifetime.
    let _server_guard = server;
    let _tmp_guard = tmp;

    let mut group = c.benchmark_group("magento2");
    // The solve is slow (multi-second on this fixture) and pre-fetch
    // setup is even slower; shrink the iteration budget so the bench
    // completes in a reasonable wall clock.
    group.sample_size(10);

    group.bench_function("resolve", |b| {
        b.iter_batched(
            || {
                build_and_prefetch(&server_uri, &composer_json, &paths)
            },
            |provider| {
                let root = provider.root_version();
                let solution =
                    resolve(&provider, PubGrubPackage::Root, root)
                        .expect("resolve magento2");
                black_box(solution);
            },
            BatchSize::PerIteration,
        );
    });
    group.finish();
}

criterion_group!(benches, resolve_magento2);
criterion_main!(benches);
