//! Inline script metadata for self-contained PHP scripts.
//!
//! Mirrors Python's PEP 723 (`uv run --script`): a single `.php` file
//! carries its own `composer.json` requires in a comment block so it can
//! run with no surrounding project, no `vendor/` next to it, and a
//! `bougie` shebang. PHP's CLI skips a leading `#!` line and treats `#`
//! as a line comment, so the whole block is syntactically inert.
//!
//! ```php
//! #!/usr/bin/env -S bougie run --script
//! <?php
//! # /// script
//! # {
//! #   "require": {
//! #     "php": ">=8.2",
//! #     "monolog/monolog": "^3.0"
//! #   }
//! # }
//! # ///
//!
//! $log = new Monolog\Logger('app');
//! ```
//!
//! The delimiters follow PEP 723 (`# /// <type>` … `# ///`); the body is
//! a `composer.json` subset (JSON), parsed by the same model the resolver
//! consumes, so `php` / `ext-*` / `minimum-stability` / `repositories`
//! all work with zero translation.

use eyre::{Result, bail};

/// The `composer.json` body extracted from a script's `# /// script`
/// block. The text is comment-prefix-stripped and validated to parse as a
/// JSON object; it is written verbatim as the ephemeral env's
/// `composer.json`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InlineMetadata {
    /// The reconstructed `composer.json` text (one JSON object).
    pub composer_json: String,
}

/// The block type bougie recognizes. PEP 723 uses `script`; we keep that
/// keyword for familiarity even though the body is composer.json.
const BLOCK_TYPE: &str = "script";

/// Parse a script's source for a single `# /// script` … `# ///` block.
///
/// Returns:
/// - `Ok(Some(_))` when exactly one well-formed block is present and its
///   body is a JSON object.
/// - `Ok(None)` when no block is present (the file is an ordinary script
///   or project file).
/// - `Err(_)` when a block is opened but malformed: unterminated, a
///   non-comment line inside it, a second block, or a body that isn't a
///   JSON object.
pub fn parse_inline_metadata(source: &str) -> Result<Option<InlineMetadata>> {
    let mut lines = source.lines().enumerate();
    let mut found: Option<String> = None;

    while let Some((open_idx, line)) = lines.next() {
        match marker_label(line) {
            Some(label) if label == BLOCK_TYPE => {
                if found.is_some() {
                    bail!(
                        "multiple `# /// {BLOCK_TYPE}` blocks (line {}); a script may declare \
                         its metadata only once",
                        open_idx + 1
                    );
                }
                found = Some(collect_block(&mut lines, open_idx)?);
            }
            // A bare `# ///` (close) or `# /// other` outside any open
            // block is not metadata we own — ignore it and keep scanning.
            _ => {}
        }
    }

    let Some(body) = found else {
        return Ok(None);
    };

    let value: serde_json::Value = serde_json::from_str(&body).map_err(|e| {
        eyre::eyre!("inline `# /// {BLOCK_TYPE}` block is not valid JSON: {e}")
    })?;
    if !value.is_object() {
        bail!("inline `# /// {BLOCK_TYPE}` block must be a JSON object (a composer.json subset)");
    }

    Ok(Some(InlineMetadata { composer_json: body }))
}

/// Consume comment lines after an opening marker until the closing
/// `# ///`, returning the reconstructed (prefix-stripped) body. `lines`
/// is positioned just past the opening marker.
fn collect_block(
    lines: &mut std::iter::Enumerate<std::str::Lines<'_>>,
    open_idx: usize,
) -> Result<String> {
    let mut body = String::new();
    for (idx, line) in lines.by_ref() {
        // A close marker (`# ///`) ends the block. A nested non-empty
        // marker (`# /// foo`) is malformed.
        if let Some(label) = marker_label(line) {
            if label.is_empty() {
                return Ok(body);
            }
            bail!(
                "unexpected `# /// {label}` inside an open metadata block (line {})",
                idx + 1
            );
        }
        let Some(content) = strip_comment(line) else {
            bail!(
                "line {} is not a comment but the `# /// {BLOCK_TYPE}` block opened on line {} \
                 is not closed; every block line must start with `#` and the block must end \
                 with `# ///`",
                idx + 1,
                open_idx + 1
            );
        };
        body.push_str(content);
        body.push('\n');
    }
    bail!(
        "unterminated `# /// {BLOCK_TYPE}` block opened on line {} (missing closing `# ///`)",
        open_idx + 1
    )
}

/// Replace a script's `# /// script` block body with `new_body` (a JSON
/// object), re-applying the `# ` comment prefix to each line. Preserves
/// everything outside the block (shebang, `<?php`, the markers, the code)
/// verbatim. Used by `bougie add --script` to edit the inline metadata in
/// place. Errors if there is no complete block to replace.
pub fn replace_block_body(source: &str, new_body: &str) -> Result<String> {
    let lines: Vec<&str> = source.lines().collect();
    let mut open = None;
    let mut close = None;
    for (idx, line) in lines.iter().enumerate() {
        match marker_label(line) {
            Some(label) if label == BLOCK_TYPE && open.is_none() => open = Some(idx),
            Some(label) if label.is_empty() && open.is_some() => {
                close = Some(idx);
                break;
            }
            _ => {}
        }
    }
    let (Some(open), Some(close)) = (open, close) else {
        bail!("no complete `# /// {BLOCK_TYPE}` block to replace");
    };

    let mut out = String::new();
    // Everything up to and including the opening marker, verbatim.
    for line in &lines[..=open] {
        out.push_str(line);
        out.push('\n');
    }
    // The new body, re-prefixed as comments (a bare `#` for blank lines).
    for body_line in new_body.lines() {
        if body_line.is_empty() {
            out.push_str("#\n");
        } else {
            out.push_str("# ");
            out.push_str(body_line);
            out.push('\n');
        }
    }
    // The closing marker and everything after it, verbatim.
    for line in &lines[close..] {
        out.push_str(line);
        out.push('\n');
    }
    Ok(out)
}

/// If `line` is a block-delimiter comment (`#` then optional whitespace
/// then `///`), return the label after the slashes (`""` for a closing
/// `# ///`, `"script"` for an opening `# /// script`). Returns `None` for
/// any other line, including ordinary `# ...` comment content.
fn marker_label(line: &str) -> Option<String> {
    let rest = line.trim_end().strip_prefix('#')?;
    let rest = rest.trim_start().strip_prefix("///")?;
    Some(rest.trim().to_string())
}

/// Strip the comment prefix from a block content line: a leading `#` plus
/// at most one following space (PEP 723's rule). Returns `None` if the
/// line isn't a comment.
fn strip_comment(line: &str) -> Option<&str> {
    let rest = line.strip_prefix('#')?;
    Some(rest.strip_prefix(' ').unwrap_or(rest))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SCRIPT: &str = r#"#!/usr/bin/env -S bougie run --script
<?php
# /// script
# {
#   "require": {
#     "php": ">=8.2",
#     "monolog/monolog": "^3.0"
#   }
# }
# ///

echo "hi";
"#;

    #[test]
    fn extracts_composer_json_body() {
        let meta = parse_inline_metadata(SCRIPT).unwrap().unwrap();
        let v: serde_json::Value = serde_json::from_str(&meta.composer_json).unwrap();
        assert_eq!(v["require"]["php"], ">=8.2");
        assert_eq!(v["require"]["monolog/monolog"], "^3.0");
    }

    #[test]
    fn no_block_is_none() {
        assert!(parse_inline_metadata("<?php\necho 1;\n").unwrap().is_none());
    }

    #[test]
    fn empty_object_is_valid() {
        let src = "<?php\n# /// script\n# {}\n# ///\n";
        let meta = parse_inline_metadata(src).unwrap().unwrap();
        assert_eq!(meta.composer_json.trim(), "{}");
    }

    #[test]
    fn bare_hash_line_is_empty_content() {
        // A `#` with no following space is a valid empty body line.
        let src = "# /// script\n#\n# {\"require\":{}}\n#\n# ///\n";
        let meta = parse_inline_metadata(src).unwrap().unwrap();
        let v: serde_json::Value = serde_json::from_str(&meta.composer_json).unwrap();
        assert!(v["require"].is_object());
    }

    #[test]
    fn unterminated_block_errors() {
        let src = "# /// script\n# {}\n";
        let err = parse_inline_metadata(src).unwrap_err().to_string();
        assert!(err.contains("unterminated"), "{err}");
    }

    #[test]
    fn non_comment_line_inside_block_errors() {
        let src = "# /// script\n# {\nnot a comment\n# }\n# ///\n";
        let err = parse_inline_metadata(src).unwrap_err().to_string();
        assert!(err.contains("not a comment"), "{err}");
    }

    #[test]
    fn second_block_errors() {
        let src = "# /// script\n# {}\n# ///\n# /// script\n# {}\n# ///\n";
        let err = parse_inline_metadata(src).unwrap_err().to_string();
        assert!(err.contains("multiple"), "{err}");
    }

    #[test]
    fn non_object_body_errors() {
        let src = "# /// script\n# [1, 2, 3]\n# ///\n";
        let err = parse_inline_metadata(src).unwrap_err().to_string();
        assert!(err.contains("JSON object"), "{err}");
    }

    #[test]
    fn invalid_json_body_errors() {
        let src = "# /// script\n# { not json\n# ///\n";
        let err = parse_inline_metadata(src).unwrap_err().to_string();
        assert!(err.contains("not valid JSON"), "{err}");
    }

    #[test]
    fn tolerates_no_space_after_hash_marker() {
        let src = "#/// script\n#{}\n#///\n";
        let meta = parse_inline_metadata(src).unwrap().unwrap();
        assert_eq!(meta.composer_json.trim(), "{}");
    }

    #[test]
    fn replace_block_body_preserves_surroundings_and_reparses() {
        let new = parse_inline_metadata(&replace_block_body(SCRIPT, "{\n  \"require\": {\n    \"php\": \">=8.4\"\n  }\n}").unwrap())
            .unwrap()
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(&new.composer_json).unwrap();
        assert_eq!(v["require"]["php"], ">=8.4");

        let rewritten = replace_block_body(SCRIPT, "{}").unwrap();
        // Shebang, <?php, and the trailing code all survive.
        assert!(rewritten.starts_with("#!/usr/bin/env -S bougie run --script\n"));
        assert!(rewritten.contains("<?php\n"));
        assert!(rewritten.contains("echo \"hi\";"));
        // The block markers survive and the body is just the new object.
        assert!(rewritten.contains("# /// script\n# {}\n# ///"));
    }

    #[test]
    fn replace_block_body_errors_without_block() {
        let err = replace_block_body("<?php\necho 1;\n", "{}").unwrap_err().to_string();
        assert!(err.contains("no complete"), "{err}");
    }
}
