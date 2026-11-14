use super::*;

#[test]
fn parses_a_single_fully_expanded_version() {
    let body = br#"{
        "packages": {
            "monolog/monolog": [
                {
                    "name": "monolog/monolog",
                    "version": "3.0.0",
                    "version_normalized": "3.0.0.0",
                    "type": "library",
                    "dist": {
                        "type": "zip",
                        "url": "https://example/monolog-3.0.0.zip",
                        "shasum": "aa"
                    },
                    "require": {"php": ">=8.1"}
                }
            ]
        }
    }"#;
    let md = PackageMetadata::parse(body).unwrap();
    let versions = &md.packages["monolog/monolog"];
    assert_eq!(versions.len(), 1);
    assert_eq!(versions[0].version, "3.0.0");
    assert_eq!(versions[0].require.get("php").unwrap(), ">=8.1");
}

#[test]
fn expands_minified_composer_2_0_inheritance() {
    // Three versions: full first entry, then two sparse diffs.
    // The second version inherits everything except `version` /
    // `version_normalized`. The third version overrides `require` and
    // adds an extra `require-dev` while still inheriting `dist.type`
    // and `name`.
    let body = br#"{
        "minified": "composer/2.0",
        "packages": {
            "acme/foo": [
                {
                    "name": "acme/foo",
                    "version": "3.0.0",
                    "version_normalized": "3.0.0.0",
                    "type": "library",
                    "dist": {"type":"zip","url":"https://e/3.0.0.zip","shasum":"a"},
                    "require": {"php":">=8.1"}
                },
                {
                    "version": "2.5.0",
                    "version_normalized": "2.5.0.0",
                    "dist": {"type":"zip","url":"https://e/2.5.0.zip","shasum":"b"}
                },
                {
                    "version": "2.0.0",
                    "version_normalized": "2.0.0.0",
                    "dist": {"type":"zip","url":"https://e/2.0.0.zip","shasum":"c"},
                    "require": {"php":">=7.4"},
                    "require-dev": {"phpunit/phpunit":"^9"}
                }
            ]
        }
    }"#;
    let md = PackageMetadata::parse(body).unwrap();
    let v = &md.packages["acme/foo"];
    assert_eq!(v.len(), 3);

    // v0: explicit.
    assert_eq!(v[0].version, "3.0.0");
    assert_eq!(v[0].require.get("php").unwrap(), ">=8.1");

    // v1 inherits name + type + require from v0.
    assert_eq!(v[1].version, "2.5.0");
    assert_eq!(v[1].name, "acme/foo");
    assert_eq!(v[1].package_type.as_deref(), Some("library"));
    assert_eq!(v[1].require.get("php").unwrap(), ">=8.1");
    assert_eq!(v[1].dist.as_ref().unwrap().url, "https://e/2.5.0.zip");

    // v2 overrides require, adds require-dev, still inherits name+type.
    assert_eq!(v[2].version, "2.0.0");
    assert_eq!(v[2].require.get("php").unwrap(), ">=7.4");
    assert_eq!(v[2].require_dev.get("phpunit/phpunit").unwrap(), "^9");
    assert_eq!(v[2].name, "acme/foo");
}

#[test]
fn __unset_sentinel_in_minified_diff_resets_inherited_key() {
    // The second entry uses `"require": "__unset"` to wipe v0's
    // require map. Composer's `MetadataMinifier::expand` uses the
    // literal string `"__unset"` as the deletion sentinel — JSON
    // `null` is NOT special and would just set the key to null.
    let body = br#"{
        "minified": "composer/2.0",
        "packages": {
            "acme/bar": [
                {
                    "name": "acme/bar",
                    "version": "2.0.0",
                    "version_normalized": "2.0.0.0",
                    "type": "library",
                    "dist": {"type":"zip","url":"https://e/a","shasum":"a"},
                    "require": {"php":">=8.0","ext-mbstring":"*"}
                },
                {
                    "version": "1.0.0",
                    "version_normalized": "1.0.0.0",
                    "dist": {"type":"zip","url":"https://e/b","shasum":"b"},
                    "require": "__unset"
                }
            ]
        }
    }"#;
    let md = PackageMetadata::parse(body).unwrap();
    let v = &md.packages["acme/bar"];
    assert_eq!(v[1].version, "1.0.0");
    // require was reset, so the typed map is empty (serde default).
    assert!(v[1].require.is_empty(), "got {:?}", v[1].require);
    // Inherited fields survive.
    assert_eq!(v[1].name, "acme/bar");
}

#[test]
fn non_minified_response_is_returned_as_is() {
    // No `minified` field → every version stands alone. v1 omits
    // `require` entirely, so the typed map is empty for v1, while v0
    // keeps its own.
    let body = br#"{
        "packages": {
            "acme/baz": [
                {
                    "name": "acme/baz",
                    "version": "2.0.0",
                    "version_normalized": "2.0.0.0",
                    "type": "library",
                    "dist": {"type":"zip","url":"https://e/a","shasum":"a"},
                    "require": {"php":">=8.0"}
                },
                {
                    "name": "acme/baz",
                    "version": "1.0.0",
                    "version_normalized": "1.0.0.0",
                    "type": "library",
                    "dist": {"type":"zip","url":"https://e/b","shasum":"b"}
                }
            ]
        }
    }"#;
    let md = PackageMetadata::parse(body).unwrap();
    let v = &md.packages["acme/baz"];
    assert_eq!(v[0].require.get("php").unwrap(), ">=8.0");
    assert!(v[1].require.is_empty());
}

#[test]
fn unknown_minified_marker_is_treated_as_non_minified() {
    // Defensive: a future Composer might bump to `composer/2.1`. We
    // don't pretend to know its semantics — fall through to "each
    // entry stands alone" rather than apply 2.0's algorithm blindly.
    let body = br#"{
        "minified": "composer/2.1",
        "packages": {
            "acme/qux": [
                {
                    "name": "acme/qux",
                    "version": "2.0.0",
                    "version_normalized": "2.0.0.0",
                    "type": "library",
                    "dist": {"type":"zip","url":"https://e/a","shasum":"a"}
                }
            ]
        }
    }"#;
    let md = PackageMetadata::parse(body).unwrap();
    assert_eq!(md.packages["acme/qux"].len(), 1);
}

#[test]
fn empty_packages_map_parses() {
    let body = br#"{"minified":"composer/2.0","packages":{}}"#;
    let md = PackageMetadata::parse(body).unwrap();
    assert!(md.packages.is_empty());
}

#[test]
fn malformed_json_errors_with_context() {
    let body = br"{not json";
    let err = PackageMetadata::parse(body).unwrap_err();
    let msg = format!("{err:#}");
    assert!(msg.contains("Packagist v2"), "{msg}");
}

#[test]
fn minified_expansion_inherits_require_across_versions() {
    // Simulates the magento/community-edition pattern:
    // v2 (newest) has require with deps.
    // v1 (older) has no require field → inherits from v2.
    let body = br#"{
        "minified": "composer/2.0",
        "packages": {
            "test/pkg": [
                {
                    "name": "test/pkg",
                    "version": "2.0.0",
                    "version_normalized": "2.0.0.0",
                    "require": {
                        "acme/foo": "^1.0",
                        "acme/bar": "^2.0"
                    },
                    "replace": {
                        "acme/replaced": "2.0.0"
                    }
                },
                {
                    "version": "1.0.0",
                    "version_normalized": "1.0.0.0",
                    "replace": {
                        "acme/other-replaced": "1.0.0"
                    }
                }
            ]
        }
    }"#;
    let md = PackageMetadata::parse(body).unwrap();
    let versions = md.packages.get("test/pkg").unwrap();
    assert_eq!(versions.len(), 2);

    let v2 = &versions[0];
    assert_eq!(v2.version, "2.0.0");
    assert_eq!(v2.require.len(), 2);
    assert_eq!(v2.require.get("acme/foo").unwrap(), "^1.0");

    let v1 = &versions[1];
    assert_eq!(v1.version, "1.0.0");
    // v1 should inherit require from v2 (minified format)
    assert_eq!(v1.require.len(), 2, "v1 should inherit require from v2");
    assert_eq!(v1.require.get("acme/foo").unwrap(), "^1.0");
    assert_eq!(v1.require.get("acme/bar").unwrap(), "^2.0");
    // v1's replace should be its own (not inherited from v2)
    assert_eq!(v1.replace.get("acme/other-replaced").unwrap(), "1.0.0");
    assert!(!v1.replace.contains_key("acme/replaced"));
}
