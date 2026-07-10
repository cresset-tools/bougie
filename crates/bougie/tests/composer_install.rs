//! Integration tests for `bougie composer install` (project install).
//!
//! Drives the real `bougie` binary via `assert_cmd`, with a wiremock
//! server standing in for Packagist. Each test stages a tiny project
//! (composer.json + composer.lock) pointing at the mock URL, runs
//! `bougie composer install -d <project>`, and asserts on the
//! resulting `vendor/` tree.

use assert_cmd::Command;
use std::io::Write;
use std::path::Path;
use tempfile::TempDir;
use wiremock::matchers::{method, path as wm_path};
use wiremock::{Mock, MockServer, ResponseTemplate};

mod common;
use common::TestEnv;

fn rt() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

fn sha1_hex(bytes: &[u8]) -> String {
    use sha1::Digest as _;
    let digest = sha1::Sha1::digest(bytes);
    let mut s = String::with_capacity(40);
    for b in digest {
        use std::fmt::Write as _;
        let _ = write!(s, "{b:02x}");
    }
    s
}

/// Build a fixture Composer dist zip wrapping entries in
/// `<top>/...`, with a single PSR-4 source file the autoloader will
/// pick up.
fn build_fixture_zip(top: &str) -> Vec<u8> {
    let mut buf: Vec<u8> = Vec::new();
    {
        let cursor = std::io::Cursor::new(&mut buf);
        let mut zw = zip::ZipWriter::new(cursor);
        let opts = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        zw.start_file(format!("{top}/composer.json"), opts).unwrap();
        zw.write_all(br#"{"name":"acme/foo","autoload":{"psr-4":{"Acme\\Foo\\":"src/"}}}"#)
            .unwrap();
        zw.start_file(format!("{top}/src/Foo.php"), opts).unwrap();
        zw.write_all(b"<?php\nnamespace Acme\\Foo; class Foo {}\n")
            .unwrap();
        zw.finish().unwrap();
    }
    buf
}

fn write_project_files(dir: &Path, composer_json: &str, composer_lock: &str) {
    std::fs::write(dir.join("composer.json"), composer_json).unwrap();
    std::fs::write(dir.join("composer.lock"), composer_lock).unwrap();
}

/// Build a fixture dist for a `magento2-component` package (à la
/// `magento/magento2-base`): a root skeleton (`index.php`, a `pub/`
/// tree, `bin/magento`) that the native deploy copies into the project
/// root per the package's `extra.map`.
fn build_magento_component_zip(top: &str) -> Vec<u8> {
    let mut buf: Vec<u8> = Vec::new();
    {
        let cursor = std::io::Cursor::new(&mut buf);
        let mut zw = zip::ZipWriter::new(cursor);
        let opts = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        let file = |zw: &mut zip::ZipWriter<_>, name: &str, body: &[u8]| {
            zw.start_file(format!("{top}/{name}"), opts).unwrap();
            zw.write_all(body).unwrap();
        };
        file(&mut zw, "composer.json", br#"{"name":"magento/magento2-base","type":"magento2-component"}"#);
        file(&mut zw, "index.php", b"<?php // Magento front controller\n");
        file(&mut zw, "pub/index.php", b"<?php // pub front controller\n");
        file(&mut zw, "pub/media/.htaccess", b"Deny from all\n");
        file(&mut zw, "bin/magento", b"#!/usr/bin/env php\n<?php // CLI\n");
        zw.finish().unwrap();
    }
    buf
}

#[test]
fn install_against_wiremock_dist_emits_vendor_and_autoload() {
    let env = TestEnv::new();
    let proj = TempDir::new().unwrap();

    // Fixture dist + valid sha1.
    let top = "acme-foo-aaaaaaaaaa";
    let body = build_fixture_zip(top);
    let hash = sha1_hex(&body);

    let rt = rt();
    let (uri, _server) = rt.block_on(async {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(wm_path("/dists/acme-foo.zip"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(body))
            .mount(&server)
            .await;
        (server.uri(), server)
    });

    // The composer.json's content-hash is computed by bougie's own
    // hasher; let the install verify it. We use a fixed minimal
    // composer.json + look up its actual hash at test time so the
    // lock embeds the right value.
    let composer_json = r#"{
    "name": "test/project",
    "require": {"acme/foo": "^1.0"}
}"#;
    let content_hash =
        bougie_composer::lockfile::content_hash(composer_json.as_bytes()).unwrap();
    let lock = format!(
        r#"{{
        "content-hash": "{content_hash}",
        "packages": [
            {{
                "name": "acme/foo",
                "version": "1.0.0",
                "dist": {{
                    "type": "zip",
                    "url": "{uri}/dists/acme-foo.zip",
                    "shasum": "{hash}"
                }},
                "type": "library",
                "autoload": {{"psr-4": {{"Acme\\Foo\\": "src/"}}}}
            }}
        ],
        "packages-dev": []
    }}"#
    );
    write_project_files(proj.path(), composer_json, &lock);

    let mut cmd = env.bougie();
    cmd.args(["composer", "install", "-d"])
        .arg(proj.path())
        .assert()
        .success();

    // The downloader extracted the zip into vendor/acme/foo, stripped
    // the wrapping `acme-foo-aaaaaaaaaa/` dir.
    let vendor_foo = proj.path().join("vendor").join("acme").join("foo");
    assert!(vendor_foo.is_dir());
    assert!(vendor_foo.join("composer.json").is_file());
    assert!(vendor_foo.join("src/Foo.php").is_file());
    assert!(!vendor_foo.join(top).exists());

    // bougie-autoloader emitted the standard surface.
    assert!(proj.path().join("vendor/autoload.php").is_file());
    assert!(proj.path().join("vendor/composer/autoload_psr4.php").is_file());
    assert!(proj.path().join("vendor/composer/installed.json").is_file());
    assert!(proj.path().join("vendor/composer/installed.php").is_file());
}

#[test]
fn install_deploys_magento_component_into_project_root() {
    // Native `magento/magento-composer-installer`: a magento2-component
    // package's `extra.map` files are copied into the project root,
    // `extra.chmod` masks applied, and `app/etc/vendor_path.php`
    // generated — without bougie running the plugin.
    let env = TestEnv::new();
    let proj = TempDir::new().unwrap();

    let top = "magento-magento2-base-bbbbbbbbbb";
    let body = build_magento_component_zip(top);
    let hash = sha1_hex(&body);

    let rt = rt();
    let (uri, _server) = rt.block_on(async {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(wm_path("/dists/magento2-base.zip"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(body))
            .mount(&server)
            .await;
        (server.uri(), server)
    });

    let composer_json = r#"{
    "name": "test/magento",
    "require": {"magento/magento2-base": "*"}
}"#;
    let content_hash =
        bougie_composer::lockfile::content_hash(composer_json.as_bytes()).unwrap();
    let lock = format!(
        r#"{{
        "content-hash": "{content_hash}",
        "packages": [
            {{
                "name": "magento/magento2-base",
                "version": "2.4.7",
                "type": "magento2-component",
                "dist": {{
                    "type": "zip",
                    "url": "{uri}/dists/magento2-base.zip",
                    "shasum": "{hash}"
                }},
                "extra": {{
                    "map": [
                        ["index.php", "index.php"],
                        ["pub", "pub"],
                        ["bin/magento", "bin/magento"]
                    ],
                    "chmod": [
                        {{"mask": "0755", "path": "bin/magento"}}
                    ]
                }}
            }}
        ],
        "packages-dev": []
    }}"#
    );
    write_project_files(proj.path(), composer_json, &lock);

    env.bougie()
        .args(["composer", "install", "-d"])
        .arg(proj.path())
        .assert()
        .success();

    // Package still extracted under vendor/.
    assert!(proj.path().join("vendor/magento/magento2-base/index.php").is_file());

    // extra.map copied the skeleton into the project root.
    assert!(proj.path().join("index.php").is_file());
    assert!(proj.path().join("pub/index.php").is_file());
    assert!(proj.path().join("pub/media/.htaccess").is_file());
    assert!(proj.path().join("bin/magento").is_file());

    // Generated (not mapped) — Magento bootstrap reads this.
    let vp = proj.path().join("app/etc/vendor_path.php");
    assert!(vp.is_file());
    let vp_contents = std::fs::read_to_string(&vp).unwrap();
    assert!(vp_contents.contains("return './vendor';"), "vendor_path.php: {vp_contents}");

    // extra.chmod applied (Unix only).
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(proj.path().join("bin/magento"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o755, "bin/magento should be 0755");
    }
}

#[test]
fn install_relocates_package_via_composer_installers() {
    // Native `composer/installers`: a `magento-theme` package installs
    // to `app/design/frontend/<name>` (built-in location) instead of
    // vendor/, and the generated autoloader anchors its PSR-4 path on
    // $baseDir (project root) rather than $vendorDir.
    let env = TestEnv::new();
    let proj = TempDir::new().unwrap();

    let top = "acme-theme-cccccccccc";
    let body = build_fixture_zip(top); // psr-4 Acme\Foo\ => src/, name acme/foo
    let hash = sha1_hex(&body);

    let rt = rt();
    let (uri, _server) = rt.block_on(async {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(wm_path("/dists/acme-theme.zip"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(body))
            .mount(&server)
            .await;
        (server.uri(), server)
    });

    let composer_json = r#"{
    "name": "test/project",
    "require": {"acme/foo": "^1.0"}
}"#;
    let content_hash =
        bougie_composer::lockfile::content_hash(composer_json.as_bytes()).unwrap();
    let lock = format!(
        r#"{{
        "content-hash": "{content_hash}",
        "packages": [
            {{
                "name": "acme/foo",
                "version": "1.0.0",
                "type": "magento-theme",
                "dist": {{
                    "type": "zip",
                    "url": "{uri}/dists/acme-theme.zip",
                    "shasum": "{hash}"
                }},
                "autoload": {{"psr-4": {{"Acme\\Foo\\": "src/"}}}}
            }}
        ],
        "packages-dev": []
    }}"#
    );
    write_project_files(proj.path(), composer_json, &lock);

    env.bougie()
        .args(["composer", "install", "-d"])
        .arg(proj.path())
        .assert()
        .success();

    // Relocated, NOT under vendor/.
    let relocated = proj.path().join("app/design/frontend/foo");
    assert!(relocated.is_dir(), "package should install to app/design/frontend/foo");
    assert!(relocated.join("src/Foo.php").is_file());
    assert!(!proj.path().join("vendor/acme/foo").exists());

    // Autoloader anchors the relocated package on $baseDir.
    let psr4 = std::fs::read_to_string(proj.path().join("vendor/composer/autoload_psr4.php")).unwrap();
    assert!(
        psr4.contains("$baseDir . '/app/design/frontend/foo/src'"),
        "autoload_psr4.php should anchor relocated pkg on $baseDir: {psr4}"
    );

    // installed.php records the relocated install-path (relative to vendor/composer).
    let installed = std::fs::read_to_string(proj.path().join("vendor/composer/installed.php")).unwrap();
    assert!(
        installed.contains("../../app/design/frontend/foo"),
        "installed.php install-path should point at the relocated dir: {installed}"
    );
}

#[test]
fn install_warns_on_unhandled_composer_installers_type() {
    // A composer/installers framework type bougie doesn't relocate
    // (cakephp-plugin) should still install (to vendor/, the Composer
    // default) but emit a warning so the misplacement is visible.
    let env = TestEnv::new();
    let proj = TempDir::new().unwrap();

    let top = "acme-foo-aaaaaaaaaa";
    let body = build_fixture_zip(top);
    let hash = sha1_hex(&body);

    let rt = rt();
    let (uri, _server) = rt.block_on(async {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(wm_path("/dists/acme-foo.zip"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(body))
            .mount(&server)
            .await;
        (server.uri(), server)
    });

    let composer_json = r#"{"name":"test/cake","require":{"acme/foo":"^1.0"}}"#;
    let content_hash =
        bougie_composer::lockfile::content_hash(composer_json.as_bytes()).unwrap();
    let lock = format!(
        r#"{{
        "content-hash": "{content_hash}",
        "packages": [
            {{
                "name": "acme/foo",
                "version": "1.0.0",
                "type": "cakephp-plugin",
                "dist": {{"type":"zip","url":"{uri}/dists/acme-foo.zip","shasum":"{hash}"}},
                "autoload": {{"psr-4": {{"Acme\\Foo\\": "src/"}}}}
            }}
        ],
        "packages-dev": []
    }}"#
    );
    write_project_files(proj.path(), composer_json, &lock);

    let output = env
        .bougie()
        .args(["composer", "install", "-d"])
        .arg(proj.path())
        .output()
        .expect("run bougie");
    assert!(output.status.success(), "stderr: {}", String::from_utf8_lossy(&output.stderr));
    // Default vendor/ placement (not relocated).
    assert!(proj.path().join("vendor/acme/foo").is_dir());
    // Warning names the package and the framework.
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("warning:"), "{stderr}");
    assert!(stderr.contains("acme/foo") && stderr.contains("cakephp"), "{stderr}");
}

#[test]
fn install_runs_native_laravel_package_discovery() {
    // With laravel/framework installed, bougie reproduces
    // `artisan package:discover`: it writes bootstrap/cache/packages.php
    // from each package's extra.laravel and clears the stale compiled
    // caches — without running the post-autoload-dump script.
    let env = TestEnv::new();
    let proj = TempDir::new().unwrap();

    let top = "acme-foo-aaaaaaaaaa";
    let body = build_fixture_zip(top);
    let hash = sha1_hex(&body);

    let rt = rt();
    let (uri, _server) = rt.block_on(async {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(wm_path("/dists/pkg.zip"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(body))
            .mount(&server)
            .await;
        (server.uri(), server)
    });

    // A stale compiled cache that discovery must remove.
    std::fs::create_dir_all(proj.path().join("bootstrap/cache")).unwrap();
    std::fs::write(proj.path().join("bootstrap/cache/config.php"), b"<?php return [];").unwrap();

    let composer_json = r#"{"name":"test/laravel","require":{"laravel/framework":"^11.0","acme/pkg":"^1.0"}}"#;
    let content_hash =
        bougie_composer::lockfile::content_hash(composer_json.as_bytes()).unwrap();
    let lock = format!(
        r#"{{
        "content-hash": "{content_hash}",
        "packages": [
            {{
                "name": "acme/pkg",
                "version": "1.0.0",
                "type": "library",
                "dist": {{"type":"zip","url":"{uri}/dists/pkg.zip","shasum":"{hash}"}},
                "extra": {{"laravel": {{"providers": ["Acme\\Pkg\\PkgServiceProvider"]}}}}
            }},
            {{
                "name": "laravel/framework",
                "version": "11.0.0",
                "type": "library",
                "dist": {{"type":"zip","url":"{uri}/dists/pkg.zip","shasum":"{hash}"}}
            }}
        ],
        "packages-dev": []
    }}"#
    );
    write_project_files(proj.path(), composer_json, &lock);

    env.bougie()
        .args(["composer", "install", "-d"])
        .arg(proj.path())
        .assert()
        .success();

    // Manifest generated with the package's provider; framework itself
    // (no extra.laravel) is absent.
    let manifest = proj.path().join("bootstrap/cache/packages.php");
    assert!(manifest.is_file(), "bootstrap/cache/packages.php should be generated");
    let contents = std::fs::read_to_string(&manifest).unwrap();
    assert!(contents.starts_with("<?php return array ("), "{contents}");
    assert!(contents.contains("'acme/pkg'"), "{contents}");
    assert!(contents.contains("Acme\\\\Pkg\\\\PkgServiceProvider"), "{contents}");
    assert!(!contents.contains("laravel/framework"), "framework has no extra.laravel: {contents}");

    // Stale compiled cache cleared.
    assert!(!proj.path().join("bootstrap/cache/config.php").exists(), "config.php should be cleared");
}

#[test]
fn install_errors_when_laravel_post_autoload_dump_drifts() {
    // bougie reproduces Laravel's package:discover + clearCompiled. If a
    // Laravel project's post-autoload-dump declares anything else (a step
    // bougie can't reproduce — future Laravel change or app script), it
    // must fail fast rather than silently leave the app half-configured.
    let env = TestEnv::new();
    let proj = TempDir::new().unwrap();

    // Custom step BETWEEN the default steps → blocks reproduction.
    let composer_json = r#"{
    "name": "test/laravel",
    "require": {"laravel/framework": "^11.0"},
    "scripts": {
        "post-autoload-dump": [
            "Illuminate\\Foundation\\ComposerScripts::postAutoloadDump",
            "@php artisan vendor:publish --tag=laravel-assets --force",
            "@php artisan package:discover --ansi"
        ]
    }
}"#;
    let content_hash =
        bougie_composer::lockfile::content_hash(composer_json.as_bytes()).unwrap();
    // Dist URL is unreachable on purpose — the guard must fire before any
    // download is attempted.
    let lock = format!(
        r#"{{
        "content-hash": "{content_hash}",
        "packages": [
            {{
                "name": "laravel/framework",
                "version": "11.0.0",
                "type": "library",
                "dist": {{"type":"zip","url":"https://example/never.zip","shasum":"00"}}
            }}
        ],
        "packages-dev": []
    }}"#
    );
    write_project_files(proj.path(), composer_json, &lock);

    let output = env
        .bougie()
        .args(["composer", "install", "-d"])
        .arg(proj.path())
        .output()
        .expect("run bougie");
    assert!(!output.status.success(), "expected install to fail on drift");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("post-autoload-dump"), "{stderr}");
    assert!(stderr.contains("vendor:publish"), "{stderr}");
    // Nothing should have been installed (fail-fast before extraction).
    assert!(!proj.path().join("vendor/laravel").exists(), "must not extract on drift");
}

#[test]
fn install_allows_trailing_custom_post_autoload_dump_step() {
    // A custom step AFTER the default Laravel steps doesn't block native
    // discovery — install succeeds and packages.php is generated.
    let env = TestEnv::new();
    let proj = TempDir::new().unwrap();

    let top = "acme-foo-aaaaaaaaaa";
    let body = build_fixture_zip(top);
    let hash = sha1_hex(&body);
    let rt = rt();
    let (uri, _server) = rt.block_on(async {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(wm_path("/dists/pkg.zip"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(body))
            .mount(&server)
            .await;
        (server.uri(), server)
    });

    let composer_json = r#"{
    "name": "test/laravel",
    "require": {"laravel/framework": "^11.0"},
    "scripts": {
        "post-autoload-dump": [
            "Illuminate\\Foundation\\ComposerScripts::postAutoloadDump",
            "@php artisan package:discover --ansi",
            "@php artisan ziggy:generate"
        ]
    }
}"#;
    let content_hash =
        bougie_composer::lockfile::content_hash(composer_json.as_bytes()).unwrap();
    let lock = format!(
        r#"{{
        "content-hash": "{content_hash}",
        "packages": [
            {{
                "name": "laravel/framework",
                "version": "11.0.0",
                "type": "library",
                "dist": {{"type":"zip","url":"{uri}/dists/pkg.zip","shasum":"{hash}"}},
                "extra": {{"laravel": {{"providers": ["Illuminate\\Foundation\\Providers\\FoundationServiceProvider"]}}}}
            }}
        ],
        "packages-dev": []
    }}"#
    );
    write_project_files(proj.path(), composer_json, &lock);

    env.bougie()
        .args(["composer", "install", "-d"])
        .arg(proj.path())
        .assert()
        .success();
    assert!(proj.path().join("bootstrap/cache/packages.php").is_file());
}

#[test]
fn install_resolves_and_writes_lock_when_missing() {
    // Composer-compatible behavior: missing composer.lock triggers
    // an in-process resolve + write rather than a hard error.
    // Mirrors `Composer\Installer::run`'s
    // "No composer.lock file present. Updating dependencies to
    // latest instead..." path.
    let env = TestEnv::new();
    let proj = TempDir::new().unwrap();

    let top = "acme-foo-aaaaaaaaaa";
    let zip_body = build_fixture_zip(top);
    let dist_hash = sha1_hex(&zip_body);

    let rt = rt();
    let (uri, _server) = rt.block_on(async {
        let server = MockServer::start().await;
        let server_uri = server.uri();
        // Metadata response with the dist URL pointing at this same
        // mock server — assembled now that we know the URI.
        let metadata = format!(
            r#"{{"packages":{{"acme/foo":[{{
                "name":"acme/foo",
                "version":"1.0.0",
                "version_normalized":"1.0.0.0",
                "type":"library",
                "dist":{{"type":"zip","url":"{server_uri}/dists/acme-foo.zip","shasum":"{dist_hash}"}},
                "autoload":{{"psr-4":{{"Acme\\Foo\\":"src/"}}}}
            }}]}}}}"#,
        );
        Mock::given(method("GET"))
            .and(wm_path("/p2/acme/foo.json"))
            .respond_with(ResponseTemplate::new(200).set_body_string(metadata))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(wm_path("/dists/acme-foo.zip"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(zip_body))
            .mount(&server)
            .await;
        (server_uri, server)
    });

    std::fs::write(
        proj.path().join("composer.json"),
        r#"{"name":"test/lonely","require":{"acme/foo":"^1.0"}}"#,
    )
    .unwrap();

    let output = env
        .bougie()
        .env("BOUGIE_PACKAGIST_BASE_URL", &uri)
        .args(["composer", "install", "-d"])
        .arg(proj.path())
        .output()
        .expect("run bougie");
    assert!(
        output.status.success(),
        "expected install to succeed; stderr={}",
        String::from_utf8_lossy(&output.stderr),
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("composer.lock not found"),
        "missing fallback warning: {stderr}",
    );
    assert!(proj.path().join("composer.lock").is_file());
    assert!(proj.path().join("vendor/acme/foo").is_dir());
}

#[test]
fn install_extracts_composer_plugin_files_and_warns_about_hooks() {
    // End-to-end check that a composer-plugin package's FILES install
    // like any other package's (plugins can double as runtime libraries
    // — php-http/discovery's classes are called by opensearch-php at
    // runtime) while the hooks-not-run warning reaches the CLI as a
    // `warning:` stderr line.
    let env = TestEnv::new();
    let proj = TempDir::new().unwrap();

    let top = "acme-plugin-aaaaaaaaaa";
    let mut body: Vec<u8> = Vec::new();
    {
        let cursor = std::io::Cursor::new(&mut body);
        let mut zw = zip::ZipWriter::new(cursor);
        let opts = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        zw.start_file(format!("{top}/composer.json"), opts).unwrap();
        zw.write_all(br#"{"name":"acme/plugin","type":"composer-plugin"}"#)
            .unwrap();
        zw.start_file(format!("{top}/src/Discovery.php"), opts).unwrap();
        zw.write_all(b"<?php // runtime half of a dual-purpose plugin\n")
            .unwrap();
        zw.finish().unwrap();
    }
    let zip_sha1 = sha1_hex(&body);

    let rt = rt();
    let (uri, _server) = rt.block_on(async {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(wm_path("/dists/acme-plugin.zip"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(body))
            .mount(&server)
            .await;
        (server.uri(), server)
    });

    let composer_json = r#"{"name":"test/plug","require":{}}"#;
    let hash =
        bougie_composer::lockfile::content_hash(composer_json.as_bytes()).unwrap();
    let lock = format!(
        r#"{{
            "content-hash": "{hash}",
            "packages": [
                {{
                    "name": "acme/plugin",
                    "version": "1.0.0",
                    "type": "composer-plugin",
                    "dist": {{
                        "type": "zip",
                        "url": "{uri}/dists/acme-plugin.zip",
                        "shasum": "{zip_sha1}"
                    }}
                }}
            ],
            "packages-dev": []
        }}"#
    );
    write_project_files(proj.path(), composer_json, &lock);

    let output = env
        .bougie()
        .args(["composer", "install", "-d"])
        .arg(proj.path())
        .output()
        .expect("run bougie");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "exit status: {:?}\nstderr: {stderr}", output.status);
    assert!(stderr.contains("warning:"), "{stderr}");
    assert!(stderr.contains("acme/plugin"), "{stderr}");
    assert!(stderr.contains("install-time hooks"), "{stderr}");
    // The runtime half of the package must be on disk.
    assert!(
        proj.path().join("vendor/acme/plugin/src/Discovery.php").is_file(),
        "plugin package files must be extracted into vendor/",
    );
}

#[test]
fn lock_verify_returns_zero_on_valid_lock() {
    let env = TestEnv::new();
    let proj = TempDir::new().unwrap();
    let composer_json = r#"{"name":"test/ok","require":{"acme/foo":"^1.2"}}"#;
    let content_hash =
        bougie_composer::lockfile::content_hash(composer_json.as_bytes()).unwrap();
    let lock = format!(
        r#"{{
            "content-hash": "{content_hash}",
            "packages": [
                {{
                    "name": "acme/foo",
                    "version": "1.2.3",
                    "dist": {{"type":"zip","url":"https://e/f.zip","shasum":"aa"}}
                }}
            ],
            "packages-dev": []
        }}"#
    );
    write_project_files(proj.path(), composer_json, &lock);

    let output = env
        .bougie()
        .args(["composer", "install", "--lock-verify", "-d"])
        .arg(proj.path())
        .output()
        .expect("run bougie");
    assert!(output.status.success(), "stderr: {}", String::from_utf8_lossy(&output.stderr));
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("valid"), "{stdout}");
    // No vendor/ should be created for the verify path.
    assert!(!proj.path().join("vendor").exists());
}

#[test]
fn lock_verify_returns_non_zero_on_invalid_lock() {
    let env = TestEnv::new();
    let proj = TempDir::new().unwrap();
    // Root says ^2 but lock pins 1.5 — invalid.
    let composer_json = r#"{"name":"test/bad","require":{"acme/foo":"^2"}}"#;
    let content_hash =
        bougie_composer::lockfile::content_hash(composer_json.as_bytes()).unwrap();
    let lock = format!(
        r#"{{
            "content-hash": "{content_hash}",
            "packages": [
                {{
                    "name": "acme/foo",
                    "version": "1.5.0",
                    "dist": {{"type":"zip","url":"https://e/f.zip","shasum":"aa"}}
                }}
            ],
            "packages-dev": []
        }}"#
    );
    write_project_files(proj.path(), composer_json, &lock);

    let output = env
        .bougie()
        .args(["composer", "install", "--lock-verify", "-d"])
        .arg(proj.path())
        .output()
        .expect("run bougie");
    assert!(!output.status.success(), "expected non-zero exit");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("INVALID"), "{stdout}");
    assert!(stdout.contains("acme/foo"), "must name the conflicting pkg: {stdout}");
}

/// Use the binary `Command` API directly here so `cargo build -p
/// bougie --tests` still exercises this file even with --quiet.
#[allow(dead_code)]
fn _ensure_command_imported(_: Command) {}
