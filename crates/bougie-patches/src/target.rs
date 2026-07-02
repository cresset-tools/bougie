//! Target inference for the zero-config `patches/` directory.
//!
//! A `patches/*.patch` file carries no package key — bougie infers the target
//! by matching the diff's header paths against the install paths of the locked
//! packages. The host supplies `(package name, install path)` pairs (computed
//! from the lock via `bougie_installers::install_path`); this module stays
//! FS/PHP-agnostic.
//!
//! Inference only works for **project-root-relative** patches whose paths
//! contain a recognizable install-path prefix (`vendor/<v>/<p>/…` or a Magento
//! type→path remap). Package-relative patches (`Model/Foo.php`, no package
//! identity) cannot be targeted and produce a precise error pointing the user
//! at an explicit `extra.patches` entry.
//!
//! A patch whose files resolve to **one** package is applied inside that
//! package's install directory (respecting `composer/installers` remaps). A
//! patch whose files span **several** packages is a legitimate *top-level*
//! patch — its paths are already project-root relative, so it applies at the
//! project root as a unit ([`PatchScope::Root`]); the touched packages are
//! coupled for pristine re-extraction.

use std::collections::BTreeSet;

use eyre::{Result, bail};

use crate::model::{DepthSpec, PatchScope};

/// The inferred target of a `patches/` file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InferredTarget {
    /// A display/grouping key for [`crate::Patch::target`]: the single package
    /// name, or (for a root patch) a comma-joined list of touched packages.
    pub target: String,
    /// Where the patch applies — a single package's dir, or the project root.
    pub scope: PatchScope,
    /// The `-pN` depth that lands the file paths correctly under the scope's
    /// base directory: for a package it strips the `a/`/`b/` prefix *plus* the
    /// install path; for a root patch it strips only the `a/`/`b/` prefix.
    pub depth: DepthSpec,
}

/// Infer where a `patches/` file applies, given the routed header path of
/// every file it touches and the locked packages' install paths.
///
/// `header_paths` are the verbatim `+++ b/…` / `--- a/…` tokens (still
/// carrying any `a/`/`b/` prefix). `install_paths` maps each package to its
/// project-relative install directory (e.g. `vendor/acme/widget`).
///
/// Returns a single-package target when every file lands in one package, or a
/// project-root ([`PatchScope::Root`]) target when they span several. Errors
/// only when a file's path matches no install path (package-relative or
/// unknown) — such a patch carries no package identity to route by.
pub fn infer_target(
    header_paths: &[&str],
    install_paths: &[(String, String)],
) -> Result<InferredTarget> {
    if header_paths.is_empty() {
        bail!("patch has no file headers to infer a target from");
    }

    let mut packages: BTreeSet<String> = BTreeSet::new();
    // The fixed depth for the single-package case (ab prefix + install-path
    // components) and the set of ab-prefix widths seen (for the root case).
    let mut single_depth: usize = 0;
    let mut ab_prefixes: BTreeSet<usize> = BTreeSet::new();

    for raw in header_paths {
        let (ab, normalized) = strip_ab_prefix(raw);
        ab_prefixes.insert(ab);
        let best = install_paths
            .iter()
            .filter(|(_, ip)| is_path_prefix(ip, normalized))
            .max_by_key(|(_, ip)| ip.split('/').count());

        match best {
            Some((pkg, ip)) => {
                single_depth = ab + ip.split('/').filter(|s| !s.is_empty()).count();
                packages.insert(pkg.clone());
            }
            None => bail!(
                "can't infer target package for patch path `{raw}` \
                 (it is package-relative or matches no installed package); \
                 declare it explicitly under `extra.patches`"
            ),
        }
    }

    // Exactly one package: apply inside its (possibly remapped) install dir.
    let packages: Vec<String> = packages.into_iter().collect();
    if let [package] = packages.as_slice() {
        return Ok(InferredTarget {
            target: package.clone(),
            scope: PatchScope::Package,
            depth: DepthSpec::Fixed(single_depth),
        });
    }

    // Several packages: a top-level patch, applied at the project root. Its
    // paths are already project-root relative, so we strip only the `a/`/`b/`
    // prefix. A uniform prefix pins the depth; a mixed one (rare) falls back to
    // the cweagans probe loop.
    let mut prefixes = ab_prefixes.into_iter();
    let depth = match (prefixes.next(), prefixes.next()) {
        (Some(n), None) => DepthSpec::Fixed(n),
        _ => DepthSpec::Auto,
    };
    Ok(InferredTarget {
        target: packages.join(", "),
        scope: PatchScope::Root { packages },
        depth,
    })
}

/// Strip a leading `a/` or `b/` (the conventional diff prefix). Returns
/// `(stripped_count, rest)` — `1` when a prefix was removed, else `0`.
fn strip_ab_prefix(path: &str) -> (usize, &str) {
    if let Some(rest) = path.strip_prefix("a/").or_else(|| path.strip_prefix("b/")) {
        (1, rest)
    } else {
        (0, path)
    }
}

/// Whether `prefix` is a leading path-component prefix of `path` (so
/// `vendor/foo` matches `vendor/foo/x` but not `vendor/foobar/x`).
fn is_path_prefix(prefix: &str, path: &str) -> bool {
    let pc: Vec<&str> = prefix.split('/').filter(|s| !s.is_empty()).collect();
    let mut comps = path.split('/').filter(|s| !s.is_empty());
    for want in pc {
        match comps.next() {
            Some(have) if have == want => {}
            _ => return false,
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths() -> Vec<(String, String)> {
        vec![
            ("acme/widget".into(), "vendor/acme/widget".into()),
            ("acme/widget-extra".into(), "vendor/acme/widget-extra".into()),
            ("magento/theme".into(), "app/design/frontend/Magento/theme".into()),
        ]
    }

    #[test]
    fn infers_git_style_with_ab_prefix() {
        let t = infer_target(&["a/vendor/acme/widget/src/W.php"], &paths()).unwrap();
        assert_eq!(t.scope, PatchScope::Package);
        assert_eq!(t.target, "acme/widget");
        // a/(1) + vendor/acme/widget(3) = 4.
        assert_eq!(t.depth, DepthSpec::Fixed(4));
    }

    #[test]
    fn infers_without_ab_prefix() {
        let t = infer_target(&["vendor/acme/widget/src/W.php"], &paths()).unwrap();
        assert_eq!(t.scope, PatchScope::Package);
        assert_eq!(t.target, "acme/widget");
        assert_eq!(t.depth, DepthSpec::Fixed(3));
    }

    #[test]
    fn longest_prefix_wins_over_similar_name() {
        // Must not match acme/widget when the path is under widget-extra.
        let t = infer_target(&["b/vendor/acme/widget-extra/x.php"], &paths()).unwrap();
        assert_eq!(t.target, "acme/widget-extra");
        assert_eq!(t.scope, PatchScope::Package);
    }

    #[test]
    fn remapped_install_path_matches() {
        let t = infer_target(
            &["a/app/design/frontend/Magento/theme/web/css/x.less"],
            &paths(),
        )
        .unwrap();
        assert_eq!(t.target, "magento/theme");
        assert_eq!(t.scope, PatchScope::Package);
    }

    #[test]
    fn package_relative_path_errors() {
        let err = infer_target(&["a/Model/Foo.php"], &paths()).unwrap_err();
        assert!(format!("{err}").contains("extra.patches"), "{err}");
    }

    #[test]
    fn spanning_two_packages_infers_a_root_patch() {
        // A top-level patch touching two packages applies at the project root
        // (strip only the `a/` prefix, depth 1) rather than erroring.
        let t = infer_target(
            &[
                "a/vendor/acme/widget/x.php",
                "a/vendor/acme/widget-extra/y.php",
            ],
            &paths(),
        )
        .unwrap();
        assert_eq!(
            t.scope,
            PatchScope::Root {
                packages: vec!["acme/widget".into(), "acme/widget-extra".into()]
            }
        );
        assert_eq!(t.depth, DepthSpec::Fixed(1));
        assert_eq!(t.target, "acme/widget, acme/widget-extra");
    }

    #[test]
    fn root_patch_without_ab_prefix_has_depth_zero() {
        let t = infer_target(
            &["vendor/acme/widget/x.php", "vendor/acme/widget-extra/y.php"],
            &paths(),
        )
        .unwrap();
        assert!(matches!(t.scope, PatchScope::Root { .. }));
        assert_eq!(t.depth, DepthSpec::Fixed(0));
    }

    #[test]
    fn root_patch_with_mixed_prefixes_falls_back_to_auto() {
        let t = infer_target(
            &["a/vendor/acme/widget/x.php", "vendor/acme/widget-extra/y.php"],
            &paths(),
        )
        .unwrap();
        assert!(matches!(t.scope, PatchScope::Root { .. }));
        assert_eq!(t.depth, DepthSpec::Auto);
    }

    #[test]
    fn unknown_package_in_a_span_still_errors() {
        // If one file resolves to no package at all, we can't route it — even
        // when a sibling file does resolve.
        let err = infer_target(
            &["a/vendor/acme/widget/x.php", "a/Model/Unknown.php"],
            &paths(),
        )
        .unwrap_err();
        assert!(format!("{err}").contains("extra.patches"), "{err}");
    }
}
