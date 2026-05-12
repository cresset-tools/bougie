//! PHP-compatible JSON encoder. Byte-exact output for two PHP
//! `json_encode` flag combinations Composer relies on:
//!
//! - [`Mode::Hash`] — `json_encode($d, 0)`. The byte stream MD5'd into
//!   `composer.lock`'s `content-hash` (see [`super::lockfile::content_hash`]).
//!   Compact, forward slashes escaped to `\/`, every code point ≥ 0x80
//!   escaped to `\uXXXX` lowercase, surrogate pairs for code points
//!   > 0xFFFF. U+2028 / U+2029 fall under the same rule.
//!
//! - [`Mode::Pretty`] — `json_encode($d, JSON_PRETTY_PRINT |
//!   JSON_UNESCAPED_SLASHES | JSON_UNESCAPED_UNICODE)`. The flag set
//!   `Composer\Json\JsonFile::encode` defaults to when writing
//!   composer.json / composer.lock. 4-space indent, raw `/`, raw UTF-8
//!   for non-ASCII — except U+2028 / U+2029, which Composer keeps
//!   escaped (it doesn't pass `JSON_UNESCAPED_LINE_TERMINATORS`).
//!
//! Shared rules (both modes):
//!
//! - `"` → `\"`, `\` → `\\`.
//! - Named escapes for 0x08 → `\b`, 0x09 → `\t`, 0x0A → `\n`,
//!   0x0C → `\f`, 0x0D → `\r`.
//! - Other C0 control bytes (0x00..0x07, 0x0B, 0x0E..0x1F) →
//!   `\u00XX` lowercase.
//! - 0x7F (DEL) emitted raw — PHP's escape bitmap doesn't flag it.
//! - Object keys take the same string-escape rules as string values.
//! - Numbers: integers via plain decimal; floats via Rust's shortest-
//!   roundtrip `f64::to_string`, which matches PHP's `zend_gcvt` at
//!   `serialize_precision = -1` (the post-7.1 default). Both PHP and
//!   Rust render integer-valued floats without a fractional part
//!   (`1.0` → `"1"`).

use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// `json_encode($d, 0)`. Composer's content-hash byte stream.
    Hash,
    /// `json_encode($d, JSON_PRETTY_PRINT | JSON_UNESCAPED_SLASHES |
    /// JSON_UNESCAPED_UNICODE)`. Composer's file-write encoding.
    Pretty,
}

/// Encode a `serde_json::Value` to bytes matching PHP's `json_encode`
/// for the given flag combination.
pub fn encode(value: &Value, mode: Mode) -> Vec<u8> {
    let mut out = Vec::with_capacity(256);
    write_value(&mut out, value, mode, 0);
    out
}

fn write_value(out: &mut Vec<u8>, v: &Value, mode: Mode, depth: usize) {
    match v {
        Value::Null => out.extend_from_slice(b"null"),
        Value::Bool(true) => out.extend_from_slice(b"true"),
        Value::Bool(false) => out.extend_from_slice(b"false"),
        Value::Number(n) => write_number(out, n),
        Value::String(s) => write_string(out, s, mode),
        Value::Array(a) => write_array(out, a, mode, depth),
        Value::Object(o) => write_object(out, o, mode, depth),
    }
}

fn write_string(out: &mut Vec<u8>, s: &str, mode: Mode) {
    out.push(b'"');
    for c in s.chars() {
        write_char_escaped(out, c, mode);
    }
    out.push(b'"');
}

fn write_char_escaped(out: &mut Vec<u8>, c: char, mode: Mode) {
    let cp = c as u32;
    match c {
        '"' => out.extend_from_slice(b"\\\""),
        '\\' => out.extend_from_slice(b"\\\\"),
        '/' => match mode {
            Mode::Hash => out.extend_from_slice(b"\\/"),
            Mode::Pretty => out.push(b'/'),
        },
        '\u{08}' => out.extend_from_slice(b"\\b"),
        '\u{09}' => out.extend_from_slice(b"\\t"),
        '\u{0a}' => out.extend_from_slice(b"\\n"),
        '\u{0c}' => out.extend_from_slice(b"\\f"),
        '\u{0d}' => out.extend_from_slice(b"\\r"),
        _ if cp < 0x20 => write_unicode_escape(out, cp),
        // The guard `cp < 0x80` makes this cast lossless.
        _ if cp < 0x80 => out.push(u8::try_from(cp).expect("ascii by construction")),
        _ => match mode {
            Mode::Hash => write_unicode_escape_full(out, cp),
            Mode::Pretty => match cp {
                // PHP escapes U+2028 / U+2029 even with
                // JSON_UNESCAPED_UNICODE, unless
                // JSON_UNESCAPED_LINE_TERMINATORS is also set —
                // Composer doesn't pass it.
                0x2028 | 0x2029 => write_unicode_escape(out, cp),
                _ => {
                    let mut buf = [0u8; 4];
                    out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
                }
            },
        },
    }
}

/// Emit a single BMP code point as `\uXXXX` with lowercase hex.
fn write_unicode_escape(out: &mut Vec<u8>, code: u32) {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    out.extend_from_slice(b"\\u");
    out.push(HEX[((code >> 12) & 0xf) as usize]);
    out.push(HEX[((code >> 8) & 0xf) as usize]);
    out.push(HEX[((code >> 4) & 0xf) as usize]);
    out.push(HEX[(code & 0xf) as usize]);
}

/// Emit a code point as one `\uXXXX` (BMP) or a UTF-16 surrogate pair
/// (`\uHHHH\uLLLL`) for code points > 0xFFFF. Matches PHP's encoder:
/// `us -= 0x10000; high = (us >> 10) | 0xd800; low = (us & 0x3ff) | 0xdc00`.
fn write_unicode_escape_full(out: &mut Vec<u8>, code: u32) {
    if code <= 0xFFFF {
        write_unicode_escape(out, code);
    } else {
        let adjusted = code - 0x10000;
        let high = 0xd800 + (adjusted >> 10);
        let low = 0xdc00 + (adjusted & 0x3ff);
        write_unicode_escape(out, high);
        write_unicode_escape(out, low);
    }
}

fn write_number(out: &mut Vec<u8>, n: &serde_json::Number) {
    if let Some(i) = n.as_i64() {
        out.extend_from_slice(i.to_string().as_bytes());
    } else if let Some(u) = n.as_u64() {
        out.extend_from_slice(u.to_string().as_bytes());
    } else if let Some(f) = n.as_f64() {
        // PHP's zend_gcvt at serialize_precision=-1 yields the shortest
        // round-trip form. Rust's f64 Display does the same.
        out.extend_from_slice(f.to_string().as_bytes());
    } else {
        // serde_json::Number always carries one of the three above when
        // constructed from valid JSON; falling through is unreachable
        // in practice. Be defensive rather than panic.
        out.extend_from_slice(b"0");
    }
}

fn write_array(out: &mut Vec<u8>, a: &[Value], mode: Mode, depth: usize) {
    if a.is_empty() {
        out.extend_from_slice(b"[]");
        return;
    }
    match mode {
        Mode::Hash => {
            out.push(b'[');
            for (i, v) in a.iter().enumerate() {
                if i > 0 {
                    out.push(b',');
                }
                write_value(out, v, mode, depth + 1);
            }
            out.push(b']');
        }
        Mode::Pretty => {
            out.push(b'[');
            for (i, v) in a.iter().enumerate() {
                if i > 0 {
                    out.push(b',');
                }
                out.push(b'\n');
                indent(out, depth + 1);
                write_value(out, v, mode, depth + 1);
            }
            out.push(b'\n');
            indent(out, depth);
            out.push(b']');
        }
    }
}

fn write_object(out: &mut Vec<u8>, o: &serde_json::Map<String, Value>, mode: Mode, depth: usize) {
    if o.is_empty() {
        // Working from `serde_json::Value`, arrays and objects are
        // distinct types, so an empty Map is unambiguously `{}`.
        out.extend_from_slice(b"{}");
        return;
    }
    match mode {
        Mode::Hash => {
            out.push(b'{');
            for (i, (k, v)) in o.iter().enumerate() {
                if i > 0 {
                    out.push(b',');
                }
                write_string(out, k, mode);
                out.push(b':');
                write_value(out, v, mode, depth + 1);
            }
            out.push(b'}');
        }
        Mode::Pretty => {
            out.push(b'{');
            for (i, (k, v)) in o.iter().enumerate() {
                if i > 0 {
                    out.push(b',');
                }
                out.push(b'\n');
                indent(out, depth + 1);
                write_string(out, k, mode);
                out.extend_from_slice(b": ");
                write_value(out, v, mode, depth + 1);
            }
            out.push(b'\n');
            indent(out, depth);
            out.push(b'}');
        }
    }
}

fn indent(out: &mut Vec<u8>, depth: usize) {
    for _ in 0..depth {
        out.extend_from_slice(b"    ");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn enc(v: &Value, mode: Mode) -> String {
        String::from_utf8(encode(v, mode)).expect("UTF-8 output")
    }

    // ---- string escape rules ------------------------------------------------

    #[test]
    fn hash_escapes_forward_slash() {
        // PHP: json_encode("a/b", 0) === "\"a\\/b\""
        assert_eq!(enc(&json!("a/b"), Mode::Hash), "\"a\\/b\"");
    }

    #[test]
    fn pretty_leaves_forward_slash_raw() {
        // JSON_UNESCAPED_SLASHES is set for the file-write encoding.
        assert_eq!(enc(&json!("a/b"), Mode::Pretty), "\"a/b\"");
    }

    #[test]
    fn both_modes_escape_double_quote_and_backslash() {
        assert_eq!(enc(&json!("\""), Mode::Hash), "\"\\\"\"");
        assert_eq!(enc(&json!("\""), Mode::Pretty), "\"\\\"\"");
        assert_eq!(enc(&json!("\\"), Mode::Hash), "\"\\\\\"");
        assert_eq!(enc(&json!("\\"), Mode::Pretty), "\"\\\\\"");
    }

    #[test]
    fn named_c0_escapes() {
        // PHP names 0x08 0x09 0x0a 0x0c 0x0d; everything else < 0x20
        // takes the generic \u00XX form.
        assert_eq!(enc(&json!("\u{08}"), Mode::Hash), "\"\\b\"");
        assert_eq!(enc(&json!("\u{09}"), Mode::Hash), "\"\\t\"");
        assert_eq!(enc(&json!("\u{0a}"), Mode::Hash), "\"\\n\"");
        assert_eq!(enc(&json!("\u{0c}"), Mode::Hash), "\"\\f\"");
        assert_eq!(enc(&json!("\u{0d}"), Mode::Hash), "\"\\r\"");
        assert_eq!(enc(&json!("\u{01}"), Mode::Hash), "\"\\u0001\"");
        assert_eq!(enc(&json!("\u{0b}"), Mode::Hash), "\"\\u000b\"");
        assert_eq!(enc(&json!("\u{1f}"), Mode::Hash), "\"\\u001f\"");
    }

    #[test]
    fn del_0x7f_is_raw() {
        // PHP's escape bitmap leaves 0x60..0x7F clear, so DEL is raw.
        let out = encode(&json!("\u{7f}"), Mode::Hash);
        assert_eq!(out, vec![b'"', 0x7f, b'"']);
    }

    #[test]
    fn hash_escapes_non_ascii_bmp_lowercase_hex() {
        // U+00E9 (é) → é with lowercase digits.
        assert_eq!(enc(&json!("é"), Mode::Hash), "\"\\u00e9\"");
        // U+201C (left double quote)
        assert_eq!(enc(&json!("\u{201c}"), Mode::Hash), "\"\\u201c\"");
    }

    #[test]
    fn hash_emits_surrogate_pair_for_supplementary_plane() {
        // U+1F4A9 ("pile of poo") — common load-bearing supplementary
        // code point. Adjusted = 0x0F4A9; high = 0xd83d, low = 0xdca9.
        assert_eq!(enc(&json!("\u{1f4a9}"), Mode::Hash), "\"\\ud83d\\udca9\"");
    }

    #[test]
    fn pretty_emits_non_ascii_raw_utf8() {
        // JSON_UNESCAPED_UNICODE keeps non-ASCII as raw UTF-8 bytes…
        let out = encode(&json!("é"), Mode::Pretty);
        assert_eq!(out, vec![b'"', 0xc3, 0xa9, b'"']);
    }

    #[test]
    fn pretty_still_escapes_line_terminators() {
        // …except U+2028 / U+2029, which Composer keeps escaped because
        // JsonFile::encode doesn't pass JSON_UNESCAPED_LINE_TERMINATORS.
        assert_eq!(enc(&json!("\u{2028}"), Mode::Pretty), "\"\\u2028\"");
        assert_eq!(enc(&json!("\u{2029}"), Mode::Pretty), "\"\\u2029\"");
    }

    // ---- structural shapes --------------------------------------------------

    #[test]
    fn empty_collections() {
        assert_eq!(enc(&json!([]), Mode::Hash), "[]");
        assert_eq!(enc(&json!({}), Mode::Hash), "{}");
        assert_eq!(enc(&json!([]), Mode::Pretty), "[]");
        assert_eq!(enc(&json!({}), Mode::Pretty), "{}");
    }

    #[test]
    fn hash_compact_no_whitespace() {
        let v = json!({"name": "acme/widget", "require": {"php": "^8.3"}});
        assert_eq!(
            enc(&v, Mode::Hash),
            "{\"name\":\"acme\\/widget\",\"require\":{\"php\":\"^8.3\"}}"
        );
    }

    #[test]
    fn hash_preserves_nested_key_order() {
        // serde_json with preserve_order keeps insertion order; the
        // encoder must not re-sort nested keys (only the top level
        // gets ksort'd, and that happens upstream in `content_hash`).
        let v = json!({"require": {"php": "^8.3", "ext-redis": "*"}});
        let out = enc(&v, Mode::Hash);
        assert!(out.contains("\"php\":\"^8.3\",\"ext-redis\":\"*\""));
    }

    #[test]
    fn pretty_uses_four_space_indent() {
        let v = json!({"a": 1, "b": [2, 3]});
        // Composer's JsonFile defaults to 4-space INDENT_DEFAULT.
        let expected = "{\n    \"a\": 1,\n    \"b\": [\n        2,\n        3\n    ]\n}";
        assert_eq!(enc(&v, Mode::Pretty), expected);
    }

    // ---- numbers ------------------------------------------------------------

    #[test]
    fn integers_plain_decimal() {
        assert_eq!(enc(&json!(0), Mode::Hash), "0");
        assert_eq!(enc(&json!(-7), Mode::Hash), "-7");
        assert_eq!(enc(&json!(1_000_000_000_i64), Mode::Hash), "1000000000");
    }

    #[test]
    fn floats_shortest_roundtrip_with_lowercase_e() {
        // 0.1 round-trips to "0.1" in both PHP and Rust.
        assert_eq!(enc(&json!(0.1_f64), Mode::Hash), "0.1");
        // Integer-valued floats lose the fractional part — both PHP
        // (without JSON_PRESERVE_ZERO_FRACTION) and Rust default to
        // this. serde_json may parse `1.0` back as Number::F64(1.0)
        // which displays as "1".
        let one: Value = serde_json::from_str("1.0").unwrap();
        assert_eq!(enc(&one, Mode::Hash), "1");
    }

    // ---- null / bool --------------------------------------------------------

    #[test]
    fn null_true_false() {
        assert_eq!(enc(&Value::Null, Mode::Hash), "null");
        assert_eq!(enc(&json!(true), Mode::Hash), "true");
        assert_eq!(enc(&json!(false), Mode::Hash), "false");
    }
}
