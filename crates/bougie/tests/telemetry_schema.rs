//! Schema-sync checks (TELEMETRY.md is the public contract):
//!
//! 1. every verb `command_name()` can produce is in
//!    `bougie_telemetry::event::COMMAND_VOCAB` (what the collector
//!    accepts — it consumes the const once the crate is on crates.io);
//! 2. every outcome label is in `OUTCOME_VOCAB`;
//! 3. every field the event structs serialize is documented in
//!    TELEMETRY.md.
//!
//! These turn "the doc, the client, and the collector agree" from a
//! promise into a compile-gate: add a verb or a field without updating
//! the vocab/doc and CI fails here.

use bougie_telemetry::event::{
    self, CommandEvent, Common, CrashEvent, Enrichment, UpdateEvent, COMMAND_VOCAB, OUTCOME_VOCAB,
};

/// The dispatcher's verb names, scraped from `command_name()`'s match
/// arms. Source-scraping is deliberate: constructing all ~30 `Command`
/// variants here would be a third copy of the list, while the source
/// text is the single place a new arm must appear.
fn dispatcher_command_names() -> Vec<String> {
    let src = include_str!("../src/lib.rs");
    let start = src.find("fn command_name").expect("command_name in lib.rs");
    let body = &src[start..];
    let end = body.find("\n}").expect("command_name end");
    let mut names = Vec::new();
    for line in body[..end].lines() {
        if let Some(idx) = line.find("=> \"") {
            let rest = &line[idx + 4..];
            if let Some(q) = rest.find('"') {
                names.push(rest[..q].to_owned());
            }
        }
    }
    assert!(names.len() >= 20, "scrape looks broken: {names:?}");
    names
}

#[test]
fn every_dispatcher_verb_is_in_the_command_vocab() {
    for name in dispatcher_command_names() {
        assert!(
            COMMAND_VOCAB.contains(&name.as_str()),
            "command_name() can produce {name:?} but COMMAND_VOCAB (and \
             therefore the collector) doesn't accept it — add it to \
             bougie-telemetry's vocab and TELEMETRY.md"
        );
    }
    // The crash lane's fallback must stay accepted too.
    assert!(COMMAND_VOCAB.contains(&"unknown"));
}

#[test]
fn every_outcome_label_is_in_the_outcome_vocab() {
    use bougie::BougieError;
    let errors = [
        BougieError::Network { operation: String::new(), detail: String::new() },
        BougieError::IndexSignature {
            url: String::new(),
            trust_root_fingerprint: String::new(),
            reason: String::new(),
            hint: String::new(),
        },
        BougieError::ManifestHashMismatch {
            url: String::new(),
            expected: String::new(),
            actual: String::new(),
        },
        BougieError::BlobHashMismatch {
            url: String::new(),
            expected: String::new(),
            actual: String::new(),
        },
        BougieError::Resolution { kind: String::new(), detail: String::new() },
        BougieError::UnknownTarget { triple: String::new(), hint: String::new() },
        BougieError::YankedSelected { tag: String::new(), reason: String::new() },
        BougieError::LockHeld { path: String::new(), pid: 0 },
        BougieError::Filesystem { operation: String::new(), detail: String::new() },
        BougieError::SelfUpdate { detail: String::new() },
        BougieError::Vcs {
            operation: String::new(),
            url: String::new(),
            detail: String::new(),
            hint: String::new(),
        },
    ];
    for err in errors {
        let label = bougie_telemetry::outcome_for_error(&eyre::Report::new(err));
        assert!(OUTCOME_VOCAB.contains(&label), "{label:?} missing from OUTCOME_VOCAB");
    }
    assert!(OUTCOME_VOCAB.contains(&bougie_telemetry::OUTCOME_OK));
    assert!(OUTCOME_VOCAB.contains(&"other"));
}

/// Serialize fully-populated events and require every top-level field
/// to appear (backtick-quoted or as a table row) in TELEMETRY.md.
#[test]
fn every_serialized_field_is_documented_in_telemetry_md() {
    let doc_path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../TELEMETRY.md");
    let doc = std::fs::read_to_string(doc_path).expect("TELEMETRY.md at repo root");

    let common = Common {
        schema: event::SCHEMA,
        event: "command",
        ts: "2026-07-04T00:00:00Z".into(),
        install_id: "unset".into(),
        invocation: "00000000-0000-4000-8000-000000000000".into(),
        bougie_version: "0.0.0",
        build_sha: Some("012345678"),
        os: "linux",
        arch: "x86_64",
        libc: "gnu",
        ci: false,
        install_method: "installer",
    };
    let command = CommandEvent {
        common: common.clone(),
        name: "sync",
        duration_ms: 1,
        outcome: "ok",
        exit_code: 0,
        enrich: Enrichment {
            resolve_ms: Some(1),
            vendor_ms: Some(1),
            autoload_ms: Some(1),
            packages_installed: Some(1),
            download_bytes: Some(1),
            cache_hit_pct: Some(50),
            php_version: Some("8.4".into()),
            php_flavor: Some("standard".into()),
            php_source: Some("managed"),
            extensions: Some(vec!["gd".into()]),
            services: Some(vec!["redis".into()]),
            direct_deps: Some("1-5"),
            total_deps: Some("1-5"),
        },
    };
    let crash = CrashEvent {
        common: Common { event: "crash", ..common.clone() },
        command: "sync",
        fingerprint: "0123456789abcdef".into(),
        frames: vec!["bougie::run".into()],
        message: Some("m".into()),
    };
    let update = UpdateEvent {
        common: Common { event: "update", ..common },
        from_version: "0.48.0".into(),
        to_version: "0.49.0".into(),
    };

    for value in [
        serde_json::to_value(&command).unwrap(),
        serde_json::to_value(&crash).unwrap(),
        serde_json::to_value(&update).unwrap(),
    ] {
        for key in value.as_object().unwrap().keys() {
            assert!(
                doc.contains(&format!("`{key}`")) || doc.contains(&format!("`{key}` /"))
                    || doc.contains(&format!("/ `{key}`")),
                "event field `{key}` is not documented in TELEMETRY.md — \
                 the doc is the collector's allowlist contract"
            );
        }
    }
}
