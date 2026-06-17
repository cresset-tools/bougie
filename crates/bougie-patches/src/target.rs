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

use std::collections::BTreeSet;

use eyre::{Result, bail};

use crate::model::DepthSpec;

/// The inferred target of a `patches/` file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InferredTarget {
    /// `vendor/package` name.
    pub package: String,
    /// The `-pN` depth that lands the file inside the package's install dir:
    /// the `a/`/`b/` prefix (if any) plus the install path's components.
    pub depth: DepthSpec,
}

/// Infer the single target package for a patch, given the routed header path
/// of every file it touches and the locked packages' install paths.
///
/// `header_paths` are the verbatim `+++ b/…` / `--- a/…` tokens (still
/// carrying any `a/`/`b/` prefix). `install_paths` maps each package to its
/// project-relative install directory (e.g. `vendor/acme/widget`).
///
/// Errors when a file's path matches no install path (package-relative or
/// unknown) or when the files span more than one package (ambiguous).
pub fn infer_target(
    header_paths: &[&str],
    install_paths: &[(String, String)],
) -> Result<InferredTarget> {
    if header_paths.is_empty() {
        bail!("patch has no file headers to infer a target from");
    }

    let mut packages: BTreeSet<String> = BTreeSet::new();
    let mut chosen: Option<(String, usize)> = None;

    for raw in header_paths {
        let (ab, normalized) = strip_ab_prefix(raw);
        let best = install_paths
            .iter()
            .filter(|(_, ip)| is_path_prefix(ip, normalized))
            .max_by_key(|(_, ip)| ip.split('/').count());

        match best {
            Some((pkg, ip)) => {
                let depth = ab + ip.split('/').filter(|s| !s.is_empty()).count();
                packages.insert(pkg.clone());
                chosen = Some((pkg.clone(), depth));
            }
            None => bail!(
                "can't infer target package for patch path `{raw}` \
                 (it is package-relative or matches no installed package); \
                 declare it explicitly under `extra.patches`"
            ),
        }
    }

    if packages.len() > 1 {
        bail!(
            "patch spans multiple packages ({}); split it or declare each \
             under `extra.patches`",
            packages.into_iter().collect::<Vec<_>>().join(", ")
        );
    }

    // Exactly one package matched across all (non-empty) headers.
    let Some((package, depth)) = chosen else {
        bail!("could not infer a target package");
    };
    Ok(InferredTarget {
        package,
        depth: DepthSpec::Fixed(depth),
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
        assert_eq!(t.package, "acme/widget");
        // a/(1) + vendor/acme/widget(3) = 4.
        assert_eq!(t.depth, DepthSpec::Fixed(4));
    }

    #[test]
    fn infers_without_ab_prefix() {
        let t = infer_target(&["vendor/acme/widget/src/W.php"], &paths()).unwrap();
        assert_eq!(t.package, "acme/widget");
        assert_eq!(t.depth, DepthSpec::Fixed(3));
    }

    #[test]
    fn longest_prefix_wins_over_similar_name() {
        // Must not match acme/widget when the path is under widget-extra.
        let t = infer_target(&["b/vendor/acme/widget-extra/x.php"], &paths()).unwrap();
        assert_eq!(t.package, "acme/widget-extra");
    }

    #[test]
    fn remapped_install_path_matches() {
        let t = infer_target(
            &["a/app/design/frontend/Magento/theme/web/css/x.less"],
            &paths(),
        )
        .unwrap();
        assert_eq!(t.package, "magento/theme");
    }

    #[test]
    fn package_relative_path_errors() {
        let err = infer_target(&["a/Model/Foo.php"], &paths()).unwrap_err();
        assert!(format!("{err}").contains("extra.patches"), "{err}");
    }

    #[test]
    fn spanning_two_packages_errors() {
        let err = infer_target(
            &[
                "a/vendor/acme/widget/x.php",
                "a/vendor/acme/widget-extra/y.php",
            ],
            &paths(),
        )
        .unwrap_err();
        assert!(format!("{err}").contains("multiple packages"), "{err}");
    }
}
