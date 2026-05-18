//! Port of composer/class-map-generator's `PhpFileCleaner.php`.
//!
//! Walks PHP source bytes and emits a sanitized copy where strings,
//! comments, and heredocs are collapsed to the literal `null`. The
//! output is fed to the class-extraction regex in [`super::finder`];
//! sanitizing first prevents matching the `class` / `interface` /
//! `trait` / `enum` keywords inside string literals or comments.
//!
//! Hot loops use [`memchr`] for the per-state delimiter searches PHP
//! does with `strcspn`; the main "advance to interesting byte" sweep
//! uses a 256-byte lookup table because the reject set is wider than
//! `memchr3` supports.

use memchr::{memchr, memchr2};

/// Sanitize PHP source for classmap scanning. Returns owned bytes
/// (a fresh buffer; the cleaner doesn't borrow from `input`).
pub(crate) fn clean(input: &[u8]) -> Vec<u8> {
    let reject = build_reject_lut();
    let mut out = Vec::with_capacity(input.len());
    let mut i = 0;
    let len = input.len();

    'outer: while i < len {
        // Skip to next `<?` open tag. Everything before is HTML and
        // gets dropped from the cleaned output.
        while i < len {
            if input[i] == b'<' && i + 1 < len && input[i + 1] == b'?' {
                i += 2;
                break;
            }
            i += 1;
        }
        if i > len {
            break;
        }
        out.extend_from_slice(b"<?");

        while i < len {
            let c = input[i];

            if c == b'?' && i + 1 < len && input[i + 1] == b'>' {
                out.extend_from_slice(b"?>");
                i += 2;
                continue 'outer;
            }

            if c == b'"' {
                i = skip_string(input, i, b'"');
                out.extend_from_slice(b"null");
                continue;
            }

            if c == b'\'' {
                i = skip_string(input, i, b'\'');
                out.extend_from_slice(b"null");
                continue;
            }

            if c == b'<'
                && i + 1 < len
                && input[i + 1] == b'<'
                && let Some((consumed, delim)) = match_heredoc_start(input, i)
            {
                i += consumed;
                i = skip_heredoc(input, i, &delim);
                out.extend_from_slice(b"null");
                continue;
            }

            if c == b'/' && i + 1 < len {
                if input[i + 1] == b'/' {
                    i = skip_to_newline(input, i);
                    continue;
                }
                if input[i + 1] == b'*' {
                    i = skip_block_comment(input, i);
                    continue;
                }
            }

            // Boring byte: emit and fast-forward to the next reject
            // char (any of `?"'</` or the first byte of a class-shape
            // keyword: `c`/`i`/`t`/`e`).
            out.push(c);
            i += 1;
            let skipped = scan_reject(input, i, &reject);
            if skipped > 0 {
                out.extend_from_slice(&input[i..i + skipped]);
                i += skipped;
            }
        }
    }

    out
}

fn build_reject_lut() -> [bool; 256] {
    let mut lut = [false; 256];
    // Mirrors Composer's `$rejectChars = '?"\'</' . implode keys` over
    // the typeConfig: c (class), i (interface), t (trait), e (enum).
    for &b in b"?\"'</cite" {
        lut[b as usize] = true;
    }
    lut
}

fn scan_reject(input: &[u8], start: usize, lut: &[bool; 256]) -> usize {
    let mut j = start;
    let len = input.len();
    while j < len && !lut[input[j] as usize] {
        j += 1;
    }
    j - start
}

fn skip_string(input: &[u8], start: usize, delim: u8) -> usize {
    let len = input.len();
    let mut i = start + 1;
    while i < len {
        match memchr2(b'\\', delim, &input[i..]) {
            Some(off) => i += off,
            None => return len,
        }
        if i >= len {
            return len;
        }
        if input[i] == b'\\' {
            // Mirrors PHP: only `\\` and `\<delim>` consume two bytes;
            // a bare backslash followed by anything else is a single
            // byte. The next iteration's `memchr2` re-finds a stop.
            if i + 1 < len && (input[i + 1] == b'\\' || input[i + 1] == delim) {
                i += 2;
            } else {
                i += 1;
            }
            continue;
        }
        // input[i] == delim
        return i + 1;
    }
    len
}

fn skip_block_comment(input: &[u8], start: usize) -> usize {
    let len = input.len();
    let mut i = start + 2;
    while i < len {
        match memchr(b'*', &input[i..]) {
            Some(off) => i += off,
            None => return len,
        }
        if i + 1 < len && input[i + 1] == b'/' {
            return i + 2;
        }
        i += 1;
    }
    len
}

fn skip_to_newline(input: &[u8], start: usize) -> usize {
    let len = input.len();
    match memchr2(b'\r', b'\n', &input[start..]) {
        Some(off) => start + off,
        None => len,
    }
}

/// Try to match a heredoc/nowdoc opener at `start` (which must point
/// at the first `<` of `<<<`). On success returns the number of bytes
/// consumed by the opener line and the delimiter identifier itself.
fn match_heredoc_start(input: &[u8], start: usize) -> Option<(usize, Vec<u8>)> {
    let len = input.len();
    if start + 2 >= len || input[start + 2] != b'<' {
        return None;
    }
    let mut j = start + 3;
    while j < len && (input[j] == b' ' || input[j] == b'\t') {
        j += 1;
    }
    let quote = if j < len && (input[j] == b'\'' || input[j] == b'"') {
        let q = input[j];
        j += 1;
        Some(q)
    } else {
        None
    };
    let ident_start = j;
    if j >= len || !is_ident_start(input[j]) {
        return None;
    }
    j += 1;
    while j < len && is_ident_cont(input[j]) {
        j += 1;
    }
    let ident = input[ident_start..j].to_vec();
    if let Some(q) = quote {
        if j >= len || input[j] != q {
            return None;
        }
        j += 1;
    }
    // Mandatory newline terminator (CRLF / LF / CR).
    if j < len && input[j] == b'\r' {
        if j + 1 < len && input[j + 1] == b'\n' {
            j += 2;
        } else {
            j += 1;
        }
    } else if j < len && input[j] == b'\n' {
        j += 1;
    } else {
        return None;
    }
    Some((j - start, ident))
}

/// Scan to the end of a heredoc body. PHP 7.3+ flexible heredocs allow
/// the closing identifier to be indented by leading spaces/tabs and to
/// be followed by any non-identifier byte (including EOF).
fn skip_heredoc(input: &[u8], start: usize, delim: &[u8]) -> usize {
    let len = input.len();
    let mut i = start;
    while i < len {
        match input[i] {
            b' ' | b'\t' => {
                i += 1;
                continue;
            }
            c if c == delim[0] => {
                if i + delim.len() <= len && &input[i..i + delim.len()] == delim {
                    let after = i + delim.len();
                    if after >= len || !is_ident_cont(input[after]) {
                        return after;
                    }
                }
            }
            _ => {}
        }
        // Skip past this line + any trailing newline run.
        i = skip_to_newline(input, i);
        while i < len && (input[i] == b'\r' || input[i] == b'\n') {
            i += 1;
        }
    }
    len
}

fn is_ident_start(c: u8) -> bool {
    c == b'_' || c.is_ascii_alphabetic() || c >= 0x80
}

fn is_ident_cont(c: u8) -> bool {
    c == b'_' || c.is_ascii_alphanumeric() || c >= 0x80
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn passes_through_plain_class() {
        let src = b"<?php\nnamespace A;\nclass Foo {}\n";
        let cleaned = clean(src);
        let s = std::str::from_utf8(&cleaned).unwrap();
        assert!(s.contains("class Foo"));
        assert!(s.contains("namespace A"));
    }

    #[test]
    fn strings_become_null() {
        let src = b"<?php $x = 'class Hidden {}'; class Real {}";
        let cleaned = clean(src);
        let s = std::str::from_utf8(&cleaned).unwrap();
        assert!(!s.contains("Hidden"));
        assert!(s.contains("class Real"));
    }

    #[test]
    fn line_comments_dropped() {
        let src = b"<?php // class Commented {}\nclass Real {}";
        let cleaned = clean(src);
        let s = std::str::from_utf8(&cleaned).unwrap();
        assert!(!s.contains("Commented"));
        assert!(s.contains("class Real"));
    }

    #[test]
    fn block_comments_dropped() {
        let src = b"<?php /* class Commented {} */ class Real {}";
        let cleaned = clean(src);
        let s = std::str::from_utf8(&cleaned).unwrap();
        assert!(!s.contains("Commented"));
        assert!(s.contains("class Real"));
    }

    #[test]
    fn heredoc_body_becomes_null() {
        let src = b"<?php $x = <<<EOT\nclass Hidden {}\nEOT;\nclass Real {}";
        let cleaned = clean(src);
        let s = std::str::from_utf8(&cleaned).unwrap();
        assert!(!s.contains("Hidden"));
        assert!(s.contains("class Real"));
    }

    #[test]
    fn nowdoc_body_becomes_null() {
        let src = b"<?php $x = <<<'EOT'\nclass Hidden {}\nEOT;\nclass Real {}";
        let cleaned = clean(src);
        let s = std::str::from_utf8(&cleaned).unwrap();
        assert!(!s.contains("Hidden"));
        assert!(s.contains("class Real"));
    }
}
