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
    /// `PATH_INFO` per the nginx `FastCGI` convention.
    Php {
        script_filename: PathBuf,
        script_name: String,
        path_info: String,
        /// Effective query string after any `[[host.rewrite]]` rule
        /// fired. `None` means "use the original request query." A
        /// rewrite that synthesises e.g. `?resource=foo` returns
        /// `Some("resource=foo")` so it isn't lost.
        rewritten_query: Option<String>,
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

    // Symlink-escape boundary. Requests resolve relative to the web root,
    // but a served file (reached through a symlink) only has to stay
    // inside the *project* — Magento dev mode symlinks `pub/static`
    // assets out to `vendor/`, `lib/web/`, and `app/`. Falls back to the
    // web root if the project path can't be canonicalized.
    let boundary = host.project.canonicalize().unwrap_or_else(|_| canonical_root.clone());

    let Some(decoded_cow) = percent_decode_str(request_path).decode_utf8().ok() else {
        return Resolution::Forbidden;
    };

    // Pass 1: unconditional `[[host.rewrite]]` rules (`only_if_missing =
    // false`). These are pure path rewrites — e.g. stripping Magento's
    // `/static/version<n>/` cache-buster segment — so the rewritten path
    // is what every subsequent step sees.
    let (p1_owned, q1) = apply_rewrites(&host.rewrites, &decoded_cow, RewritePass::Always);
    let path1: &str = p1_owned.as_deref().unwrap_or(&decoded_cow);
    let query1: &str = q1.as_deref().unwrap_or(query);

    // Step 1: a direct hit on disk. The (rewritten) path maps to a real
    // file, or to a directory with an index file. This wins over both
    // the `only_if_missing` rewrites and the generic front-controller
    // fallthrough — it's what lets already-materialised assets (CSS, JS,
    // and dev-mode-generated files like `requirejs-config.js`) serve
    // straight from disk.
    if let Some(direct) = resolve_direct(&canonical_root, &boundary, host, path1, q1.clone()) {
        return direct;
    }

    // Step 2: `only_if_missing` rewrites — nginx's `if (!-f
    // $request_filename) { rewrite … }`. Magento's `/static/` →
    // `static.php?resource=…` and `/media/` → `get.php?resource=…`
    // materialisers live here. They run only because step 1 found
    // nothing, and they take precedence over the generic front
    // controller so a missing `/static/` asset reaches `static.php`
    // rather than `index.php`. Applied to the post-pass-1 path so a
    // stripped version segment stays stripped.
    let (p2_owned, q2) = apply_rewrites(&host.rewrites, path1, RewritePass::Fallback);
    if let Some(path2) = p2_owned {
        let query2: String = q2.clone().unwrap_or_else(|| query1.to_string());
        return run_try_files(&canonical_root, &boundary, host, &path2, &query2, &q2);
    }

    // Step 3: the full `try_files` chain, including any front-controller
    // fallthrough (`/index.php$is_args$args`). This is the path for
    // ordinary application routes that aren't files on disk.
    let query1_owned = query1.to_string();
    run_try_files(&canonical_root, &boundary, host, path1, &query1_owned, &q1)
}

/// Resolve a path against the filesystem *directly* — a real file, or a
/// directory served via one of the host's `index` entries. Unlike
/// [`run_try_files`], this does not consult `try_files`, so it never
/// triggers a front-controller fallthrough. Returns `None` when nothing
/// on disk matches, letting the caller move on to the rewrite / fallback
/// steps. A traversal attempt still surfaces as `Some(Forbidden)`.
fn resolve_direct(
    canonical_root: &Path,
    boundary: &Path,
    host: &HostBlock,
    path: &str,
    rewritten_query: Option<String>,
) -> Option<Resolution> {
    match resolve_candidate(canonical_root, boundary, path, &host.index, path, false) {
        Resolution::NotFound => None,
        Resolution::Php { script_filename, script_name, path_info, .. } => Some(Resolution::Php {
            script_filename,
            script_name,
            path_info,
            rewritten_query,
        }),
        other => Some(other),
    }
}

/// Run a host's `try_files` chain against one resolved request path,
/// stamping `rewritten_query` onto any `Php` resolution so the
/// `FastCGI` `QUERY_STRING` reflects a rewrite that synthesised a
/// `?resource=…`.
fn run_try_files(
    canonical_root: &Path,
    boundary: &Path,
    host: &HostBlock,
    decoded: &str,
    effective_query: &str,
    rewritten_query: &Option<String>,
) -> Resolution {
    for pattern in &host.try_files {
        let expanded = expand_placeholders(pattern, decoded, effective_query);
        // A "fallthrough" pattern is one whose path-part (before any
        // query suffix) doesn't equal the request URI: `$uri` patterns
        // produce identical candidates, literal-prefix patterns like
        // `/index.php$is_args$args` don't.
        let candidate_path = expanded.split('?').next().unwrap_or(&expanded);
        let fallthrough = candidate_path != decoded;
        match resolve_candidate(canonical_root, boundary, &expanded, &host.index, decoded, fallthrough) {
            Resolution::NotFound => {}
            // Surface a rewritten query out to the router. Static
            // resolutions don't carry one because they don't consume
            // queries; Php gets the rewritten string so the FastCGI
            // QUERY_STRING reflects the rewrite.
            Resolution::Php { script_filename, script_name, path_info, .. } => {
                return Resolution::Php {
                    script_filename,
                    script_name,
                    path_info,
                    rewritten_query: rewritten_query.clone(),
                };
            }
            other => return other,
        }
    }
    Resolution::NotFound
}

/// Which class of `[[host.rewrite]]` rules a pass applies.
#[derive(Debug, Clone, Copy)]
enum RewritePass {
    /// Unconditional rules (`only_if_missing = false`) — run before any
    /// disk lookup.
    Always,
    /// Fallback rules (`only_if_missing = true`) — run only after
    /// `try_files` found nothing on disk.
    Fallback,
}

/// Apply the first matching rewrite belonging to `pass`. Returns
/// `(path_override, query_override)`:
///  - `path_override = Some(p)` when a rewrite fired and replaced the
///    URI path; `None` means "use the original path".
///  - `query_override = Some(q)` when the rewrite target carried a
///    `?query` suffix; `None` means "use the original query".
fn apply_rewrites(
    rewrites: &[crate::server::config::RewriteRule],
    uri: &str,
    pass: RewritePass,
) -> (Option<String>, Option<String>) {
    let want_fallback = matches!(pass, RewritePass::Fallback);
    for rule in rewrites {
        if rule.only_if_missing != want_fallback {
            continue;
        }
        let Ok(re) = regex::Regex::new(&rule.pattern) else {
            continue;
        };
        if !re.is_match(uri) {
            continue;
        }
        let replaced = re.replace(uri, rule.target.as_str()).into_owned();
        let (path_part, query_part) = match replaced.split_once('?') {
            Some((p, q)) => (p.to_string(), Some(q.to_string())),
            None => (replaced, None),
        };
        return (Some(path_part), query_part);
    }
    (None, None)
}

fn expand_placeholders(pattern: &str, uri: &str, query: &str) -> String {
    pattern
        .replace("$uri", uri)
        .replace("$is_args", if query.is_empty() { "" } else { "?" })
        .replace("$args", query)
}

fn resolve_candidate(
    canonical_root: &Path,
    boundary: &Path,
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
        // Re-canonicalize and verify it stays inside the project. Magento
        // developer mode materialises `pub/static` assets as symlinks
        // into `vendor/`, `lib/web/`, and `app/` — outside the web root
        // but inside the project — so the escape boundary is the project
        // root, not the web root. A target outside the project (e.g.
        // `/etc/passwd`) is still rejected.
        let Ok(canonical) = resolved.canonicalize() else {
            return Resolution::Forbidden;
        };
        if !canonical.starts_with(boundary) {
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
                if !canonical.starts_with(boundary) {
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
    if !canonical.starts_with(boundary) {
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
        return Resolution::Php {
            script_filename: canonical,
            script_name,
            path_info,
            rewritten_query: None,
        };
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
            rewrites: Vec::new(),
        }
    }

    /// The Magento rewrite set the bougie-server provisioner seeds:
    /// version-strip (always) + `static.php`/`get.php` fallbacks
    /// (only-if-missing). Kept in sync with
    /// `bougie-daemon`'s `framework_rewrites`.
    fn magento_rewrites() -> Vec<crate::server::config::RewriteRule> {
        use crate::server::config::RewriteRule;
        vec![
            RewriteRule {
                pattern: r"^/static/version[^/]+/(.*)$".into(),
                target: "/static/$1".into(),
                only_if_missing: false,
            },
            RewriteRule {
                pattern: r"^/static/(.*)$".into(),
                target: "/static.php?resource=$1".into(),
                only_if_missing: true,
            },
            RewriteRule {
                pattern: r"^/media/(.*)$".into(),
                target: "/get.php?resource=$1".into(),
                only_if_missing: true,
            },
        ]
    }

    #[test]
    fn magento_static_missing_falls_through_to_static_php() {
        let d = fixture(&[("pub/static.php", "<?php")]);
        let mut h = host(d.path(), "pub", &["$uri", "/static.php$is_args$args"], &[]);
        h.rewrites = magento_rewrites();
        // `css/styles.css` isn't on disk → version stripped, miss, then
        // the only-if-missing fallback hands it to static.php.
        match resolve(&h, "/static/version123/css/styles.css", "") {
            Resolution::Php { script_filename, path_info, rewritten_query, .. } => {
                assert!(
                    script_filename.ends_with("static.php"),
                    "wrong script: {script_filename:?}"
                );
                assert_eq!(rewritten_query.as_deref(), Some("resource=css/styles.css"));
                assert_eq!(path_info, "");
            }
            other => panic!("expected Php, got {other:?}"),
        }
    }

    #[test]
    fn magento_static_versioned_file_on_disk_serves_directly() {
        // A deployed/generated asset lives at the *versionless* path on
        // disk. The request carries a cache-buster version segment; the
        // version-strip rewrite must map it onto the real file and serve
        // it directly rather than shadowing it with static.php. This is
        // the requirejs-config.js regression.
        let d = fixture(&[
            ("pub/static.php", "<?php"),
            ("pub/static/adminhtml/Theme/en_US/requirejs-config.js", "// generated"),
        ]);
        let mut h = host(d.path(), "pub", &["$uri", "/static.php$is_args$args"], &[]);
        h.rewrites = magento_rewrites();
        match resolve(
            &h,
            "/static/version1780320129/adminhtml/Theme/en_US/requirejs-config.js",
            "",
        ) {
            Resolution::Static { path } => {
                assert!(path.ends_with("requirejs-config.js"), "wrong file: {path:?}");
            }
            other => panic!("expected Static (served from disk), got {other:?}"),
        }
    }

    #[cfg(unix)]
    #[test]
    fn dev_mode_symlink_into_project_serves_directly() {
        // Magento developer mode materialises `pub/static` assets as
        // symlinks pointing at the module source under `vendor/` — outside
        // the web root but inside the project. The version-strip rewrite
        // maps the request onto the symlink, which must serve directly
        // (it's the user's own code) rather than 403.
        let d = fixture(&[
            ("pub/static.php", "<?php"),
            ("vendor/mageos/module/js/widget.js", "// real source"),
        ]);
        let link = d.path().join("pub/static/Mageos_Module/js/widget.js");
        std::fs::create_dir_all(link.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(d.path().join("vendor/mageos/module/js/widget.js"), &link)
            .unwrap();
        let mut h = host(d.path(), "pub", &["$uri", "/static.php$is_args$args"], &[]);
        h.rewrites = magento_rewrites();
        match resolve(&h, "/static/version42/Mageos_Module/js/widget.js", "") {
            Resolution::Static { path } => assert!(path.ends_with("widget.js"), "got {path:?}"),
            other => panic!("expected Static via symlink, got {other:?}"),
        }
    }

    #[cfg(unix)]
    #[test]
    fn symlink_escaping_project_is_forbidden() {
        // A symlink whose target leaves the project entirely (the classic
        // `/etc/passwd` exfil) must still be rejected — the boundary
        // widened to the project root, not to the whole filesystem.
        let outside = TempDir::new().unwrap();
        std::fs::write(outside.path().join("secret"), "shh").unwrap();
        let d = fixture(&[("pub/index.php", "<?php")]);
        let link = d.path().join("pub/leak");
        std::os::unix::fs::symlink(outside.path().join("secret"), &link).unwrap();
        let h = host(d.path(), "pub", &["$uri"], &[]);
        assert!(matches!(resolve(&h, "/leak", ""), Resolution::Forbidden));
    }

    #[test]
    fn magento_media_on_disk_serves_directly() {
        let d = fixture(&[("pub/get.php", "<?php"), ("pub/media/logo.png", "PNG")]);
        let mut h = host(d.path(), "pub", &["$uri", "/index.php$is_args$args"], &["index.php"]);
        h.rewrites = magento_rewrites();
        match resolve(&h, "/media/logo.png", "") {
            Resolution::Static { path } => assert!(path.ends_with("logo.png")),
            other => panic!("expected Static, got {other:?}"),
        }
    }

    #[test]
    fn magento_media_missing_falls_through_to_get_php() {
        let d = fixture(&[("pub/get.php", "<?php")]);
        let mut h = host(d.path(), "pub", &["$uri", "/index.php$is_args$args"], &["index.php"]);
        h.rewrites = magento_rewrites();
        match resolve(&h, "/media/catalog/cache/thumb.jpg", "") {
            Resolution::Php { script_filename, rewritten_query, .. } => {
                assert!(script_filename.ends_with("get.php"), "wrong script: {script_filename:?}");
                assert_eq!(rewritten_query.as_deref(), Some("resource=catalog/cache/thumb.jpg"));
            }
            other => panic!("expected Php, got {other:?}"),
        }
    }

    #[test]
    fn rewrite_only_fires_when_pattern_matches() {
        use crate::server::config::RewriteRule;
        let d = fixture(&[("pub/style.css", "x"), ("pub/static.php", "<?php")]);
        let mut h = host(d.path(), "pub", &["$uri"], &[]);
        h.rewrites.push(RewriteRule {
            pattern: r"^/static/(.*)$".into(),
            target: "/static.php?resource=$1".into(),
            only_if_missing: true,
        });
        // `/style.css` doesn't match `^/static/…` — must serve as-is.
        match resolve(&h, "/style.css", "") {
            Resolution::Static { path } => assert!(path.ends_with("style.css")),
            other => panic!("expected Static, got {other:?}"),
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
            Resolution::Php { script_filename, script_name, path_info, .. } => {
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
            Resolution::Php { script_filename, script_name, path_info, .. } => {
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
