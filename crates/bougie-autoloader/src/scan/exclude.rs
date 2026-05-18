//! Port of Composer's `exclude-from-classmap` pattern compilation.
//!
//! Each user-supplied glob (e.g. `tests/`, `**/Fixtures/**`, `../foo`)
//! becomes one regex alternative anchored at the absolute path of the
//! package's (or root's) install dir. All compiled alternatives are
//! OR-joined into a single regex applied to every candidate file
//! during scan.
//!
//! Compilation faithfully mirrors `AutoloadGenerator::parseAutoloadsType`'s
//! `exclude-from-classmap` branch (PCRE in PHP, `regex` crate here):
//!
//!   1. Normalize separators (`\` → `/`), strip surrounding `/`,
//!      collapse repeated `/`.
//!   2. `preg_quote` (Rust: [`regex::escape`]).
//!   3. Translate the quoted-glob form: `\*\*` → `.+?`, `\*` →
//!      `[^/]+?`.
//!   4. Peel leading `\./` / `\.\./` segments off the front into an
//!      "updir" prefix.
//!   5. `realpath(install_path + updir)` — drop the pattern if the
//!      target doesn't resolve.
//!   6. Final alternative: `{escape(resolved_install_path)}/{body}($|/)`.

use std::path::{Path, PathBuf};

use regex::bytes::Regex;

/// Compiled exclude-from-classmap set. `regex == None` means no
/// patterns applied — scan is unfiltered.
pub(crate) struct ExcludePatterns {
    regex: Option<Regex>,
}

impl ExcludePatterns {
    /// Build from per-pattern source. Each `(install_abs, raw)` pair
    /// is compiled and OR-joined. Patterns whose `install_abs +
    /// updir` doesn't resolve on disk are silently dropped, matching
    /// Composer's `realpath() === false` continue.
    pub(crate) fn build(patterns: &[(PathBuf, String)]) -> Self {
        let mut alternatives: Vec<String> = Vec::new();
        for (install, raw) in patterns {
            if let Some(re) = compile_one(install, raw) {
                alternatives.push(re);
            }
        }
        if alternatives.is_empty() {
            return Self { regex: None };
        }
        // Wrap each alternative in a non-capturing group to keep the
        // OR boundary unambiguous when alternatives end with `($|/)`.
        let combined = format!(
            "(?-u)({})",
            alternatives
                .iter()
                .map(|a| format!("(?:{a})"))
                .collect::<Vec<_>>()
                .join("|")
        );
        let regex = Regex::new(&combined).ok();
        Self { regex }
    }

    /// Returns true if `path` should be excluded from the classmap.
    pub(crate) fn matches(&self, path: &Path) -> bool {
        let Some(re) = &self.regex else {
            return false;
        };
        // Composer normalizes path strings to forward slashes before
        // applying the regex; mirror that so platforms behave the
        // same. On Unix `to_string_lossy` is essentially free.
        let s = path.to_string_lossy().replace('\\', "/");
        re.is_match(s.as_bytes())
    }
}

fn compile_one(install: &Path, raw: &str) -> Option<String> {
    // (1) normalize separators + trim + collapse
    let normalized: String = raw
        .replace('\\', "/")
        .split('/')
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("/");
    if normalized.is_empty() {
        return None;
    }

    // (2) preg_quote
    let escaped = regex::escape(&normalized);

    // (3) glob → regex
    let translated = escaped
        .replace("\\*\\*", ".+?")
        .replace("\\*", "[^/]+?");

    // (4) peel leading \\./ and \\.\\./ chunks into updir
    let (updir, body) = split_updir(&translated);

    // (5) resolve install + updir
    let combined = if updir.is_empty() {
        install.to_path_buf()
    } else {
        install.join(&updir)
    };
    let resolved = std::fs::canonicalize(&combined).ok()?;
    let resolved_str = resolved.to_string_lossy().replace('\\', "/");

    // (6) final alternative
    let prefix = regex::escape(&resolved_str);
    Some(format!("{prefix}/{body}($|/)"))
}

fn split_updir(escaped: &str) -> (String, String) {
    let mut updir = String::new();
    let mut rest = escaped;
    loop {
        // After regex::escape, leading `.` becomes `\.` and `/`
        // stays `/`. So `../` is `\.\./` in the escaped form and
        // `./` is `\./`. Peel one chunk at a time.
        if let Some(stripped) = rest.strip_prefix("\\.\\./") {
            updir.push_str("../");
            rest = stripped;
        } else if let Some(stripped) = rest.strip_prefix("\\./") {
            updir.push_str("./");
            rest = stripped;
        } else {
            break;
        }
    }
    (updir, rest.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Set up a temp dir and run a closure with it as the workspace
    /// root. The closure gets paths it can use for install + the
    /// patterns; cleanup happens on drop.
    struct TmpRoot {
        path: PathBuf,
    }
    impl TmpRoot {
        fn new(label: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "bougie-exclude-test-{}-{label}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            std::fs::create_dir_all(&path).unwrap();
            // Resolve `/var/folders/...` → `/private/var/folders/...`
            // on macOS so the path we hand to `ExcludePatterns::build`
            // (which canonicalize's internally) matches what `matches`
            // sees later. No-op on Linux.
            let path = std::fs::canonicalize(&path).unwrap();
            Self { path }
        }
    }
    impl Drop for TmpRoot {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    #[test]
    fn empty_set_matches_nothing() {
        let p = ExcludePatterns::build(&[]);
        assert!(!p.matches(Path::new("/anything/at/all")));
    }

    #[test]
    fn simple_dir_pattern() {
        let root = TmpRoot::new("simple");
        std::fs::create_dir_all(root.path.join("tests")).unwrap();
        std::fs::write(root.path.join("tests/Foo.php"), b"<?php").unwrap();
        std::fs::create_dir_all(root.path.join("src")).unwrap();
        std::fs::write(root.path.join("src/Bar.php"), b"<?php").unwrap();

        let p = ExcludePatterns::build(&[(root.path.clone(), "tests/".into())]);
        assert!(p.matches(&root.path.join("tests/Foo.php")));
        assert!(!p.matches(&root.path.join("src/Bar.php")));
    }

    #[test]
    fn single_star_one_segment_only() {
        let root = TmpRoot::new("star");
        std::fs::create_dir_all(root.path.join("a/b")).unwrap();
        std::fs::create_dir_all(root.path.join("c")).unwrap();
        std::fs::write(root.path.join("a/b/Foo.php"), b"<?php").unwrap();
        std::fs::write(root.path.join("c/Bar.php"), b"<?php").unwrap();

        let p = ExcludePatterns::build(&[(root.path.clone(), "*/".into())]);
        // `*` is one path segment — matches `a`, `c` (top-level dirs)
        // but not `a/b` (two segments).
        assert!(p.matches(&root.path.join("a/Foo.php")));
        assert!(p.matches(&root.path.join("c/Bar.php")));
        // Composer's `*` is `[^/]+?` lazy — does not span `/`. So
        // `a/b/...` should still match because `*` consumes `a`.
        // The alternative is anchored at `<root>/*($|/)` so
        // `<root>/a/b/Foo.php` matches via `*=a` then `/b/Foo.php`
        // matched by `($|/)`'s trailing `/`.
        assert!(p.matches(&root.path.join("a/b/Foo.php")));
    }

    #[test]
    fn double_star_spans_segments() {
        let root = TmpRoot::new("dstar");
        std::fs::create_dir_all(root.path.join("deeply/nested/fixtures")).unwrap();
        std::fs::write(
            root.path.join("deeply/nested/fixtures/X.php"),
            b"<?php",
        )
        .unwrap();
        std::fs::create_dir_all(root.path.join("src")).unwrap();
        std::fs::write(root.path.join("src/Y.php"), b"<?php").unwrap();

        let p = ExcludePatterns::build(&[(root.path.clone(), "**/fixtures/**".into())]);
        assert!(p.matches(&root.path.join("deeply/nested/fixtures/X.php")));
        assert!(!p.matches(&root.path.join("src/Y.php")));
    }

    #[test]
    fn dropped_when_install_path_missing() {
        let root = TmpRoot::new("missing");
        // Pattern with leading `../` resolves to root/.. but if root
        // itself doesn't exist we drop.
        let nonexistent = root.path.join("not-a-dir");
        let p = ExcludePatterns::build(&[(nonexistent, "x".into())]);
        // Build silently dropped the only pattern → empty set.
        assert!(!p.matches(Path::new("/anything")));
    }
}
