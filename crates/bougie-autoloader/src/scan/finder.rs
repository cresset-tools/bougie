//! Port of composer/class-map-generator's `PhpFileParser::findClasses`.
//!
//! Two-stage detection: a literal-driven prefilter on the raw bytes
//! to skip files that can't possibly declare a class, then the real
//! extraction on the cleaned source produced by [`super::cleaner`].
//!
//! Composer's extraction regex uses `(?<![\\$:>])` to reject matches
//! preceded by `\`, `$`, `:`, or `>` (i.e. namespaced references,
//! variables, static calls, and member accesses — not declarations).
//! The `regex` crate is automaton-based and doesn't support
//! lookbehind, so we strip the lookbehind from the pattern and
//! re-check the byte preceding each match by hand.

use regex::bytes::Regex;
use std::sync::OnceLock;

static PREFILTER: OnceLock<Regex> = OnceLock::new();
static EXTRACTOR: OnceLock<Regex> = OnceLock::new();

fn prefilter() -> &'static Regex {
    PREFILTER.get_or_init(|| {
        // Matches the raw source: if no `class` / `interface` / `trait`
        // / `enum` followed by whitespace exists anywhere, the file
        // can't declare anything. The `regex` crate's Teddy
        // multi-literal SIMD search makes this nearly free.
        Regex::new(r"(?i-u)\b(?:class|interface|trait|enum)\s").unwrap()
    })
}

fn extractor() -> &'static Regex {
    EXTRACTOR.get_or_init(|| {
        // Two alternatives:
        //   - a type declaration (class/interface/trait/enum + name)
        //   - a `namespace` directive (with optional namespace expr,
        //     terminated by `{` or `;`).
        // The PHP source uses possessive quantifiers (`++`, `*+`); the
        // `regex` crate doesn't support them but is automaton-based,
        // so greedy non-possessive quantifiers produce the same match.
        Regex::new(
            r"(?ix-u)
            (?:
                 \b (?P<type> class | interface | trait | enum )
                 \s+ (?P<name> [a-zA-Z_\x7f-\xff:] [a-zA-Z0-9_\x7f-\xff:\-]* )
               | \b (?P<ns> namespace )
                 (?P<nsname>
                     \s+ [a-zA-Z_\x7f-\xff] [a-zA-Z0-9_\x7f-\xff]*
                     (?: \s* \\ \s* [a-zA-Z_\x7f-\xff] [a-zA-Z0-9_\x7f-\xff]* )*
                 )?
                 \s* [\{;]
            )
            ",
        )
        .unwrap()
    })
}

/// Extract fully-qualified class names from a PHP source buffer.
/// Returns `Vec<String>` — Composer guarantees class names are
/// ASCII-clean enough to be valid UTF-8 by the time they're emitted.
pub(crate) fn find_classes(input: &[u8]) -> Vec<String> {
    if !prefilter().is_match(input) {
        return vec![];
    }
    let cleaned = super::cleaner::clean(input);
    let mut classes: Vec<String> = vec![];
    // Empty namespace == top-level. Composer's tracker treats the
    // initial state the same way (`$namespace = '';` then prepends).
    let mut current_ns = String::new();

    for caps in extractor().captures_iter(&cleaned) {
        let mat = caps.get(0).unwrap();
        // Manual `(?<![\\$:>])` — Composer's lookbehind dropped from
        // the pattern, reapplied here.
        if mat.start() > 0 {
            let prev = cleaned[mat.start() - 1];
            if matches!(prev, b'\\' | b'$' | b':' | b'>') {
                continue;
            }
        }

        if caps.name("ns").is_some() {
            if let Some(nsname) = caps.name("nsname") {
                // Strip interior whitespace — `namespace  Foo \ Bar;` is
                // legal but the canonical form has no whitespace. Decode
                // the remaining bytes as UTF-8 (same as the class-name
                // path below) — `b as char` would reinterpret a multibyte
                // UTF-8 namespace as Latin-1 and re-encode it as mojibake,
                // diverging byte-for-byte from Composer.
                let mut ns_bytes: Vec<u8> = Vec::with_capacity(nsname.as_bytes().len() + 1);
                for &b in nsname.as_bytes() {
                    if !matches!(b, b' ' | b'\t' | b'\r' | b'\n') {
                        ns_bytes.push(b);
                    }
                }
                let mut s = String::from_utf8(ns_bytes)
                    .unwrap_or_else(|e| String::from_utf8_lossy(&e.into_bytes()).into_owned());
                s.push('\\');
                current_ns = s;
            } else {
                // `namespace { ... }` — anonymous (root) namespace.
                current_ns = String::new();
            }
            continue;
        }

        let Some(name_m) = caps.name("name") else {
            continue;
        };
        let name_bytes = name_m.as_bytes();
        // Bytes from a regex that captured `[a-zA-Z_\x7f-\xff:][...]*` —
        // ascii-or-high-byte. They're not guaranteed valid UTF-8 in
        // the high-byte case, but no real-world class name relies on
        // non-UTF-8 bytes; fall back to lossy and move on.
        let mut name = String::from_utf8(name_bytes.to_vec())
            .unwrap_or_else(|e| String::from_utf8_lossy(&e.into_bytes()).into_owned());

        // `class extends`, `class implements` — these are anonymous-
        // class continuations, not declarations. Composer drops them.
        if name.eq_ignore_ascii_case("extends") || name.eq_ignore_ascii_case("implements") {
            continue;
        }

        // XHP class: leading colon, transform `-` → `_` and `:` → `__`,
        // then prepend `xhp`. (https://github.com/facebook/xhp)
        if name.starts_with(':') {
            let inner: String = name[1..]
                .chars()
                .flat_map(|c| match c {
                    '-' => "_".chars().collect::<Vec<_>>(),
                    ':' => "__".chars().collect::<Vec<_>>(),
                    other => vec![other],
                })
                .collect();
            name = format!("xhp{inner}");
        }

        // `enum Foo: int { ... }` — the regex captures the typed-enum
        // suffix. Trim everything after the last `:` (which the
        // declaration name would never legitimately contain).
        if let Some(ty) = caps.name("type")
            && ty.as_bytes().eq_ignore_ascii_case(b"enum")
            && let Some(idx) = name.rfind(':')
        {
            name.truncate(idx);
        }

        let mut full = current_ns.clone();
        full.push_str(&name);
        // Composer ltrims a leading `\` — only happens if the
        // namespace was literally `\`, which is invalid but cheap to
        // tolerate.
        let trimmed = full.trim_start_matches('\\').to_string();
        classes.push(trimmed);
    }

    classes
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_when_no_class_keyword() {
        assert!(find_classes(b"<?php\n$x = 1;\n").is_empty());
    }

    #[test]
    fn finds_two_classes_in_one_file() {
        let src = b"<?php\nnamespace Acme\\Scanner;\nclass Alpha {}\nclass Beta {}\n";
        let classes = find_classes(src);
        assert_eq!(classes, vec!["Acme\\Scanner\\Alpha", "Acme\\Scanner\\Beta"]);
    }

    #[test]
    fn top_level_class_no_namespace() {
        let src = b"<?php\nclass Foo {}\n";
        assert_eq!(find_classes(src), vec!["Foo"]);
    }

    #[test]
    fn skips_class_in_comment() {
        let src = b"<?php\n// class Hidden {}\nclass Real {}\n";
        assert_eq!(find_classes(src), vec!["Real"]);
    }

    #[test]
    fn skips_class_in_string() {
        let src = b"<?php\n$s = 'class Hidden {}';\nclass Real {}\n";
        assert_eq!(find_classes(src), vec!["Real"]);
    }

    #[test]
    fn interface_and_trait() {
        let src = b"<?php\nnamespace N;\ninterface I {}\ntrait T {}\n";
        let classes = find_classes(src);
        assert_eq!(classes, vec!["N\\I", "N\\T"]);
    }

    #[test]
    fn enum_with_type_suffix() {
        let src = b"<?php\nnamespace N;\nenum Color: int { case Red = 1; }\n";
        let classes = find_classes(src);
        assert_eq!(classes, vec!["N\\Color"]);
    }

    #[test]
    fn lookbehind_rejects_static_reference() {
        // `Foo::class` — `class` is preceded by `:` and must not match.
        let src = b"<?php\nnamespace N;\n$x = Foo::class;\nclass Real {}\n";
        let classes = find_classes(src);
        assert_eq!(classes, vec!["N\\Real"]);
    }
}
