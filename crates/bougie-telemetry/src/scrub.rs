//! Crash-payload scrubbing: frame filtering and message redaction.
//!
//! The crash lane is the only place any error *detail* leaves the
//! machine, so everything here is deny-by-default: frames keep symbol
//! names only when they belong to bougie or the Rust runtime (else
//! they collapse to `[external]` or a build-relative offset), and
//! messages lose anything path-shaped or quoted before truncation.

use sha2::{Digest, Sha256};

pub const MAX_FRAMES: usize = 40;
pub const MAX_MESSAGE_CHARS: usize = 200;
pub const REDACTED: &str = "[redacted]";
pub const EXTERNAL: &str = "[external]";

/// Symbol-name prefixes that may ship verbatim: our own crates and
/// the Rust runtime. Everything else is not ours to leak.
const FRAME_PREFIXES: &[&str] =
    &["bougie", "bgx", "sandbox_run", "std::", "core::", "alloc::"];

/// Render a captured backtrace as shippable frames: allowlisted symbol
/// names (hash suffix stripped), `[external]` for foreign symbols
/// (consecutive runs collapse), or a `+0x…` module-relative offset
/// when symbols are stripped — release binaries are built with
/// `strip = "symbols"`, and offsets plus `build_sha` let the collector
/// symbolize against the dist artifact without the client shipping
/// anything readable.
pub fn frames(bt: &backtrace::Backtrace) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for frame in bt.frames() {
        if out.len() >= MAX_FRAMES {
            break;
        }
        let name = frame
            .symbols()
            .iter()
            .find_map(|s| s.name().map(|n| n.to_string()));
        let rendered = match name {
            Some(name) => {
                let name = strip_hash_suffix(&name);
                if allowed(name) {
                    name.to_owned()
                } else {
                    EXTERNAL.to_owned()
                }
            }
            None => match module_offset(frame) {
                Some(offset) => format!("+0x{offset:x}"),
                None => EXTERNAL.to_owned(),
            },
        };
        if rendered == EXTERNAL && out.last().is_some_and(|l| l == EXTERNAL) {
            continue; // collapse consecutive foreign frames
        }
        out.push(rendered);
    }
    out
}

fn module_offset(frame: &backtrace::BacktraceFrame) -> Option<usize> {
    let base = frame.module_base_address()? as usize;
    let ip = frame.ip() as usize;
    ip.checked_sub(base)
}

fn allowed(name: &str) -> bool {
    // Trait-impl symbols render as `<crate::T as trait::U>::f`.
    let name = name.trim_start_matches('<');
    FRAME_PREFIXES.iter().any(|p| name.starts_with(p))
}

/// Drop rustc's `::h0123456789abcdef` symbol-hash suffix — it differs
/// per build and adds nothing once `build_sha` is on the envelope.
fn strip_hash_suffix(name: &str) -> &str {
    if let Some(idx) = name.rfind("::h") {
        let tail = &name[idx + 3..];
        if tail.len() == 16 && tail.bytes().all(|b| b.is_ascii_hexdigit()) {
            return &name[..idx];
        }
    }
    name
}

/// First 16 hex chars of `sha256(frames)` — the crash identity used
/// for local per-day dedupe and server-side grouping.
pub fn fingerprint(frames: &[String]) -> String {
    use std::fmt::Write as _;
    let mut hasher = Sha256::new();
    for frame in frames {
        hasher.update(frame.as_bytes());
        hasher.update(b"\n");
    }
    let digest = hasher.finalize();
    let mut out = String::with_capacity(16);
    for b in digest.iter().take(8) {
        let _ = write!(out, "{b:02x}");
    }
    out
}

/// Redact a panic message: quoted spans longer than 12 chars,
/// path-shaped tokens, and anything containing the home directory all
/// become `[redacted]`; the result is truncated to
/// [`MAX_MESSAGE_CHARS`]. Standard panics ("index out of bounds: the
/// len is 3 but the index is 7") survive intact — those carry the
/// value.
pub fn message(raw: &str, home: Option<&str>) -> String {
    let quoted = redact_quoted(raw, 12);
    let mut tokens: Vec<String> = Vec::new();
    for token in quoted.split_whitespace() {
        let contains_home = home.is_some_and(|h| !h.is_empty() && token.contains(h));
        if contains_home || path_shaped(token) {
            if tokens.last().map(String::as_str) != Some(REDACTED) {
                tokens.push(REDACTED.to_owned());
            }
        } else {
            tokens.push(token.to_owned());
        }
    }
    let joined = tokens.join(" ");
    joined.chars().take(MAX_MESSAGE_CHARS).collect()
}

fn path_shaped(token: &str) -> bool {
    let t = token.trim_matches(|c: char| ",.;:()".contains(c));
    t.starts_with('/')
        || t.starts_with("./")
        || t.starts_with("../")
        || t.starts_with('~')
        || t.contains(":\\")
        || t.contains("\\\\")
        // Multi-segment relative paths (`vendor/acme/pkg`) are as
        // identifying as absolute ones.
        || t.matches('/').count() >= 2
}

/// Replace the contents of `"…"`, `'…'`, and `` `…` `` spans longer
/// than `max` chars (quotes included in the replacement).
fn redact_quoted(raw: &str, max: usize) -> String {
    let chars: Vec<char> = raw.chars().collect();
    let mut out = String::with_capacity(raw.len());
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if (c == '"' || c == '\'' || c == '`')
            && let Some(close) = chars[i + 1..].iter().position(|&x| x == c)
            && close > max
        {
            out.push_str(REDACTED);
            i += close + 2;
            continue;
        }
        out.push(c);
        i += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn standard_panic_messages_survive() {
        let msg = "index out of bounds: the len is 3 but the index is 7";
        assert_eq!(message(msg, Some("/home/user")), msg);
        let msg = "called `unwrap()` on a `None` value";
        assert_eq!(message(msg, None), msg);
    }

    #[test]
    fn paths_are_redacted() {
        let scrubbed = message("failed to read /home/user/project/composer.json now", None);
        assert!(!scrubbed.contains("composer.json"), "{scrubbed}");
        assert!(scrubbed.contains(REDACTED));
        assert!(scrubbed.starts_with("failed to read"));

        for raw in [
            "open ./relative/path failed",
            "open ~/dotfile failed",
            "open C:\\Users\\x\\file failed",
            "open vendor/acme/package failed",
        ] {
            let scrubbed = message(raw, None);
            assert!(scrubbed.contains(REDACTED), "{raw} -> {scrubbed}");
            assert!(!scrubbed.contains("file"), "{raw} -> {scrubbed}");
        }
    }

    #[test]
    fn home_dir_is_redacted_wherever_it_appears() {
        let scrubbed = message("state at /root-of/homedir-thing broke", Some("homedir"));
        assert!(!scrubbed.contains("homedir"), "{scrubbed}");
    }

    #[test]
    fn long_quoted_strings_are_redacted() {
        let scrubbed = message(r#"bad value "package/name-goes-here" in input"#, None);
        assert!(!scrubbed.contains("package/name"), "{scrubbed}");
        // Short quoted spans (rust panics quote method names) survive.
        let scrubbed = message("called `unwrap()` on nothing", None);
        assert!(scrubbed.contains("`unwrap()`"));
    }

    #[test]
    fn message_is_capped() {
        let long = "x".repeat(500);
        assert_eq!(message(&long, None).chars().count(), MAX_MESSAGE_CHARS);
    }

    #[test]
    fn no_slash_rooted_token_survives_any_input() {
        // Property-style sweep: whatever the shape, nothing that
        // starts with `/` may pass through.
        let inputs = [
            "a /b c", "x //e", "/", "/a/b/c",
            "wrapped (/tmp/x) parens", "comma, /var/y, end",
        ];
        for input in inputs {
            let scrubbed = message(input, None);
            for token in scrubbed.split_whitespace() {
                assert!(
                    !token.trim_matches(|c: char| ",.;:()".contains(c)).starts_with('/'),
                    "{input:?} -> {scrubbed:?}"
                );
            }
        }
    }

    #[test]
    fn frame_names_filter_and_strip_hashes() {
        assert!(allowed("bougie_telemetry::spool::Spool::append"));
        assert!(allowed("<bougie_cli::Cli as clap::Parser>::parse"));
        assert!(allowed("std::panicking::begin_panic"));
        assert!(!allowed("openssl::ssl::connect"));
        assert_eq!(
            strip_hash_suffix("bougie::run::h0123456789abcdef"),
            "bougie::run"
        );
        assert_eq!(strip_hash_suffix("bougie::run::hnothex"), "bougie::run::hnothex");
    }

    #[test]
    fn fingerprints_are_stable_and_hex() {
        let frames = vec!["a".to_owned(), "b".to_owned()];
        let fp = fingerprint(&frames);
        assert_eq!(fp.len(), 16);
        assert!(fp.bytes().all(|b| b.is_ascii_hexdigit()));
        assert_eq!(fp, fingerprint(&frames));
        assert_ne!(fp, fingerprint(&["a".to_owned()]));
    }
}
