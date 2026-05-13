//! `try_files` resolution + static file serving. Spec: SERVER.md §5.
//!
//! For phase 1 we serve any file that resolves under the host's web
//! root via `try_files`. `.php` files return 501 Not Implemented with a
//! pointer to phase 2 — the `FastCGI` dispatch layer that will handle
//! them lands there.

use percent_encoding::percent_decode_str;
use std::path::{Path, PathBuf};

use super::config::HostBlock;

#[derive(Debug)]
pub enum Resolution {
    /// A regular file that should be served as static content. The
    /// path has been verified to be under the host's web root (both
    /// lexically and via `canonicalize`).
    Static { path: PathBuf },
    /// A `.php` script matched. `script_filename` is the canonical
    /// path on disk; `script_name` is the URI portion that maps to
    /// the script (leading `/`); `path_info` is the request URI
    /// portion *after* the script when a front-controller pattern
    /// fell through (empty for direct `.php` hits). The router uses
    /// these to populate `SCRIPT_FILENAME`, `SCRIPT_NAME`, and
    /// `PATH_INFO` per the nginx FastCGI convention.
    Php {
        script_filename: PathBuf,
        script_name: String,
        path_info: String,
    },
    /// No `try_files` entry resolved to a real file.
    NotFound,
    /// A traversal attempt or symlink escape. The request is rejected
    /// with 403 before any filesystem read.
    Forbidden,
}

/// Resolve a request URI against a host's `try_files`.
///
/// `request_path` is the URI path (still percent-encoded).
/// `query` is the query string without the leading `?` (may be empty).
pub fn resolve(host: &HostBlock, request_path: &str, query: &str) -> Resolution {
    let web_root = host.project.join(&host.root);
    let Ok(canonical_root) = web_root.canonicalize() else {
        // Project root doesn't exist on disk. Surface as 404 — the
        // listener already validated the config; this is a runtime
        // problem the operator will see in logs.
        return Resolution::NotFound;
    };

    let Some(decoded) = percent_decode_str(request_path).decode_utf8().ok() else {
        return Resolution::Forbidden;
    };

    for pattern in &host.try_files {
        let expanded = expand_placeholders(pattern, &decoded, query);
        // A "fallthrough" pattern is one whose path-part (before any
        // query suffix) doesn't equal the request URI: `$uri` patterns
        // produce identical candidates, literal-prefix patterns like
        // `/index.php$is_args$args` don't.
        let candidate_path = expanded.split('?').next().unwrap_or(&expanded);
        let fallthrough = candidate_path != decoded;
        let original_uri_decoded: &str = &decoded;
        match resolve_candidate(
            &canonical_root,
            &expanded,
            &host.index,
            original_uri_decoded,
            fallthrough,
        ) {
            Resolution::NotFound => {}
            other => return other,
        }
    }
    Resolution::NotFound
}

fn expand_placeholders(pattern: &str, uri: &str, query: &str) -> String {
    pattern
        .replace("$uri", uri)
        .replace("$is_args", if query.is_empty() { "" } else { "?" })
        .replace("$args", query)
}

fn resolve_candidate(
    canonical_root: &Path,
    candidate: &str,
    index: &[String],
    original_uri: &str,
    fallthrough: bool,
) -> Resolution {
    // Strip the query string if the pattern carried `$is_args$args`.
    let candidate = candidate.split('?').next().unwrap_or(candidate);

    let Some(rel) = safe_relative(candidate) else {
        return Resolution::Forbidden;
    };
    let resolved = canonical_root.join(&rel);

    let Ok(meta) = std::fs::symlink_metadata(&resolved) else {
        return Resolution::NotFound;
    };

    if meta.file_type().is_symlink() {
        // Re-canonicalize and verify it stays under the web root.
        let Ok(canonical) = resolved.canonicalize() else {
            return Resolution::Forbidden;
        };
        if !canonical.starts_with(canonical_root) {
            return Resolution::Forbidden;
        }
        return classify(canonical, &rel, original_uri, fallthrough);
    }

    if meta.is_dir() {
        for entry in index {
            let with_index = resolved.join(entry);
            if with_index.is_file() {
                let Ok(canonical) = with_index.canonicalize() else {
                    return Resolution::Forbidden;
                };
                if !canonical.starts_with(canonical_root) {
                    return Resolution::Forbidden;
                }
                let mut rel_with_index = rel.clone();
                rel_with_index.push(entry);
                // Directory-index hits are direct, even though the URI
                // didn't end with the index filename.
                return classify(canonical, &rel_with_index, original_uri, false);
            }
        }
        return Resolution::NotFound;
    }

    let Ok(canonical) = resolved.canonicalize() else {
        return Resolution::Forbidden;
    };
    if !canonical.starts_with(canonical_root) {
        return Resolution::Forbidden;
    }
    classify(canonical, &rel, original_uri, fallthrough)
}

fn classify(
    canonical: PathBuf,
    rel: &Path,
    original_uri: &str,
    fallthrough: bool,
) -> Resolution {
    if canonical.extension().is_some_and(|e| e.eq_ignore_ascii_case("php")) {
        let script_name = format!("/{}", rel.display());
        let path_info = if fallthrough { original_uri.to_owned() } else { String::new() };
        return Resolution::Php { script_filename: canonical, script_name, path_info };
    }
    Resolution::Static { path: canonical }
}

/// Decode a URI path into a relative `PathBuf`, rejecting any segment
/// that would escape the web root.
fn safe_relative(path: &str) -> Option<PathBuf> {
    let mut buf = PathBuf::new();
    for seg in path.split('/') {
        if seg.is_empty() || seg == "." {
            continue;
        }
        if seg == ".." {
            return None;
        }
        // Reject embedded separators, NUL bytes, and Windows-style
        // backslashes. axum already gave us a decoded URI path so any
        // residual `\` is a deliberate injection attempt.
        if seg.contains('\\') || seg.contains('\0') || seg.contains('/') {
            return None;
        }
        buf.push(seg);
    }
    Some(buf)
}

/// Mime type for a static file. Falls back to `application/octet-stream`.
pub fn mime_for(path: &Path) -> &'static str {
    mime_guess::from_path(path)
        .first_raw()
        .unwrap_or("application/octet-stream")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn fixture(layout: &[(&str, &str)]) -> TempDir {
        let dir = TempDir::new().unwrap();
        for (path, body) in layout {
            let full = dir.path().join(path);
            if let Some(parent) = full.parent() {
                fs::create_dir_all(parent).unwrap();
            }
            fs::write(&full, body).unwrap();
        }
        dir
    }

    fn host(project: &Path, root: &str, try_files: &[&str], index: &[&str]) -> HostBlock {
        HostBlock {
            hostname: "test".into(),
            project: project.to_path_buf(),
            root: root.into(),
            index: index.iter().map(|s| (*s).to_string()).collect(),
            try_files: try_files.iter().map(|s| (*s).to_string()).collect(),
            aliases: Vec::new(),
        }
    }

    #[test]
    fn serves_existing_file() {
        let d = fixture(&[("public/style.css", "body{}")]);
        let h = host(d.path(), "public", &["$uri"], &[]);
        match resolve(&h, "/style.css", "") {
            Resolution::Static { path } => assert!(path.ends_with("style.css")),
            other => panic!("expected Static, got {other:?}"),
        }
    }

    #[test]
    fn missing_file_returns_not_found() {
        let d = fixture(&[("public/style.css", "body{}")]);
        let h = host(d.path(), "public", &["$uri"], &[]);
        assert!(matches!(resolve(&h, "/missing.css", ""), Resolution::NotFound));
    }

    #[test]
    fn directory_resolves_to_index() {
        let d = fixture(&[("public/index.html", "<html>")]);
        let h = host(d.path(), "public", &["$uri", "$uri/"], &["index.html"]);
        match resolve(&h, "/", "") {
            Resolution::Static { path } => assert!(path.ends_with("index.html")),
            other => panic!("expected Static index, got {other:?}"),
        }
    }

    #[test]
    fn traversal_rejected() {
        let d = fixture(&[("public/style.css", "body{}"), ("secret", "shh")]);
        let h = host(d.path(), "public", &["$uri"], &[]);
        assert!(matches!(resolve(&h, "/../secret", ""), Resolution::Forbidden));
    }

    #[test]
    fn encoded_traversal_rejected() {
        let d = fixture(&[("public/style.css", "body{}"), ("secret", "shh")]);
        let h = host(d.path(), "public", &["$uri"], &[]);
        assert!(matches!(resolve(&h, "/%2E%2E/secret", ""), Resolution::Forbidden));
    }

    #[test]
    fn try_files_falls_through_to_front_controller() {
        let d = fixture(&[("public/index.php", "<?php phpinfo();")]);
        let h = host(
            d.path(),
            "public",
            &["$uri", "$uri/", "/index.php$is_args$args"],
            &["index.php"],
        );
        match resolve(&h, "/users/42", "page=1") {
            Resolution::Php { script_filename, script_name, path_info } => {
                assert!(script_filename.ends_with("index.php"));
                assert_eq!(script_name, "/index.php");
                assert_eq!(path_info, "/users/42");
            }
            other => panic!("expected Php fallthrough, got {other:?}"),
        }
    }

    #[test]
    fn nonexistent_root_is_not_found() {
        let d = fixture(&[]);
        let h = host(&d.path().join("nope"), ".", &["$uri"], &[]);
        assert!(matches!(resolve(&h, "/anything", ""), Resolution::NotFound));
    }

    #[test]
    fn php_file_classified_correctly() {
        let d = fixture(&[("public/info.php", "<?php phpinfo();")]);
        let h = host(d.path(), "public", &["$uri"], &[]);
        match resolve(&h, "/info.php", "") {
            Resolution::Php { script_filename, script_name, path_info } => {
                assert!(script_filename.ends_with("info.php"));
                assert_eq!(script_name, "/info.php");
                assert_eq!(path_info, "");
            }
            other => panic!("expected Php, got {other:?}"),
        }
    }

    #[test]
    fn safe_relative_rejects_dots() {
        assert!(safe_relative("../etc/passwd").is_none());
        assert!(safe_relative("/foo/../bar").is_none());
    }

    #[test]
    fn safe_relative_keeps_normal_path() {
        let p = safe_relative("/foo/bar.txt").unwrap();
        assert_eq!(p, PathBuf::from("foo/bar.txt"));
    }
}
