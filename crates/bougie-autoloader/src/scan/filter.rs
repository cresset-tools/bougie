//! Per-class namespace filter for `-o` mode (PSR-4 / PSR-0 directory
//! scans into the classmap).
//!
//! Port of composer/class-map-generator's
//! `ClassMapGenerator::filterByNamespace`. The rule is: when a PSR-*
//! directory is scanned into the classmap, a class declaration is
//! only kept if the class's name (minus the namespace prefix the
//! mapping declares) matches the file's path under the mapping's
//! base directory.
//!
//! E.g. for PSR-4 prefix `Acme\\` rooted at `vendor/acme/src/`:
//!   - `Acme\Foo` in `vendor/acme/src/Foo.php` → keep
//!   - `Acme\Foo` in `vendor/acme/src/wrong/Foo.php` → drop
//!   - `Other\Foo` in `vendor/acme/src/Foo.php` → drop
//!
//! When *no* class in a file passes the filter, Composer warns and
//! returns zero classes for the file. We mirror the empty-return but
//! drop the warning (bougie isn't a CLI yet, so there's nowhere
//! useful to surface it).

use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub(crate) enum NamespaceFilter {
    /// No filter — classmap directory scan, every class is kept.
    None,
    /// PSR-4: namespace prefix + base dir. Class path is the rest of
    /// the FQN with `\` → `/`.
    Psr4 { namespace: String, base: PathBuf },
    /// PSR-0: namespace prefix + base dir. Class path is the rest of
    /// the FQN with `\` → `/` for the namespace segment and `_` →
    /// `/` for the class segment.
    Psr0 { namespace: String, base: PathBuf },
}

impl NamespaceFilter {
    /// Returns true if this class should be kept for this file path.
    pub(crate) fn accepts(&self, class: &str, file: &Path) -> bool {
        let (ns, base, is_psr0) = match self {
            Self::None => return true,
            Self::Psr4 { namespace, base } => (namespace.as_str(), base.as_path(), false),
            Self::Psr0 { namespace, base } => (namespace.as_str(), base.as_path(), true),
        };

        // `realSubPath`: path of `file` relative to `base`, with
        // the file extension stripped.
        let Ok(rel) = file.strip_prefix(base) else {
            return false;
        };
        let rel_str: String = rel.to_string_lossy().replace('\\', "/");
        let real_sub_path = match rel_str.rfind('.') {
            Some(idx) => &rel_str[..idx],
            None => rel_str.as_str(),
        };

        // `subPath`: derived from the class name.
        let sub_path = if is_psr0 {
            // PSR-0: namespace-part separators `\` → `/`; class-part
            // (after the last `\`) translates underscores → `/` too.
            if !ns.is_empty() && !class.starts_with(ns) {
                return false;
            }
            match class.rfind('\\') {
                Some(idx) => {
                    let namespace_part = &class[..=idx];
                    let class_part = &class[idx + 1..];
                    let mut out = namespace_part.replace('\\', "/");
                    out.push_str(&class_part.replace('_', "/"));
                    out
                }
                None => class.replace('_', "/"),
            }
        } else {
            // PSR-4: drop the prefix from the class, replace `\` with `/`.
            if !ns.is_empty() && !class.starts_with(ns) {
                return false;
            }
            let sub_namespace = if ns.is_empty() {
                class
            } else {
                &class[ns.len()..]
            };
            sub_namespace.replace('\\', "/")
        };

        sub_path == real_sub_path
    }
}

/// Composer's filterByNamespace semantics: if zero classes in a file
/// pass the filter, return empty (no class wins); if at least one
/// passes, return the passing subset.
pub(crate) fn apply(
    filter: &NamespaceFilter,
    classes: Vec<String>,
    file: &Path,
) -> Vec<String> {
    if matches!(filter, NamespaceFilter::None) {
        return classes;
    }
    let kept: Vec<String> = classes
        .into_iter()
        .filter(|c| filter.accepts(c, file))
        .collect();
    // Composer also returns [] (with a warning) when zero classes
    // match, but the `.filter()` already produces [] in that case —
    // no extra work needed beyond skipping the warning.
    kept
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn psr4(ns: &str, base: &str) -> NamespaceFilter {
        NamespaceFilter::Psr4 {
            namespace: ns.to_string(),
            base: PathBuf::from(base),
        }
    }

    fn psr0(ns: &str, base: &str) -> NamespaceFilter {
        NamespaceFilter::Psr0 {
            namespace: ns.to_string(),
            base: PathBuf::from(base),
        }
    }

    #[test]
    fn none_accepts_everything() {
        assert!(NamespaceFilter::None.accepts("Anything", Path::new("/x/y.php")));
    }

    #[test]
    fn psr4_matching_path_passes() {
        let f = psr4("Acme\\", "/v/acme/src");
        assert!(f.accepts("Acme\\Foo", Path::new("/v/acme/src/Foo.php")));
        assert!(f.accepts(
            "Acme\\Sub\\Bar",
            Path::new("/v/acme/src/Sub/Bar.php")
        ));
    }

    #[test]
    fn psr4_wrong_path_rejected() {
        let f = psr4("Acme\\", "/v/acme/src");
        assert!(!f.accepts("Acme\\Foo", Path::new("/v/acme/src/wrong/Foo.php")));
        assert!(!f.accepts("Other\\Foo", Path::new("/v/acme/src/Foo.php")));
    }

    #[test]
    fn psr4_empty_namespace_takes_class_directly() {
        let f = psr4("", "/v/root/src");
        assert!(f.accepts("Foo", Path::new("/v/root/src/Foo.php")));
        assert!(f.accepts("A\\B", Path::new("/v/root/src/A/B.php")));
    }

    #[test]
    fn psr0_underscores_in_class_become_dirs() {
        // Classic PSR-0: `Acme_Foo_Bar` → `Acme/Foo/Bar.php`.
        let f = psr0("", "/v/legacy");
        assert!(f.accepts("Acme_Foo_Bar", Path::new("/v/legacy/Acme/Foo/Bar.php")));
    }

    #[test]
    fn psr0_namespace_separator_translates_too() {
        let f = psr0("Legacy\\", "/v/legacy");
        assert!(f.accepts(
            "Legacy\\Acme_Foo",
            Path::new("/v/legacy/Legacy/Acme/Foo.php")
        ));
    }

    #[test]
    fn apply_returns_subset() {
        let f = psr4("Acme\\", "/v/acme/src");
        let kept = apply(
            &f,
            vec!["Acme\\Foo".into(), "Other\\Bar".into()],
            Path::new("/v/acme/src/Foo.php"),
        );
        assert_eq!(kept, vec!["Acme\\Foo".to_string()]);
    }

    #[test]
    fn apply_returns_empty_when_none_match() {
        let f = psr4("Acme\\", "/v/acme/src");
        let kept = apply(
            &f,
            vec!["Other\\Bar".into()],
            Path::new("/v/acme/src/Foo.php"),
        );
        assert!(kept.is_empty());
    }
}
