//! The materialized patch plan handed to the install orchestrator.
//!
//! Resolution ([`crate::resolve`]) yields abstract [`Patch`](crate::Patch)
//! rules. The host (the CLI bridge) then *materializes* each one — passing
//! local files through, downloading remote URLs, computing each patch file's
//! content hash — into a [`MaterializedPatch`], and groups them by target
//! package into a [`PatchPlan`].
//!
//! The plan also carries the **applied-state fingerprints** loaded from
//! `patches.lock.json`, which is what makes re-application correct: a package
//! is forced to re-extract pristine when its *desired* fingerprint (computed
//! here from the patches' content + depth) differs from the *applied* one.

use std::collections::BTreeMap;
use std::path::PathBuf;

use eyre::{Result, WrapErr, bail};

use crate::content_sha256;
use crate::model::{DepthSpec, FailureMode};

/// One patch resolved to a local file on disk, ready to apply.
#[derive(Debug, Clone)]
pub struct MaterializedPatch {
    /// Human description (PATCHES.txt + dedup).
    pub description: String,
    /// The original `url`/path string as declared, for reporting + the lock.
    pub origin: String,
    /// Absolute path to the patch file on disk (a passthrough local file or a
    /// downloaded copy in the cache).
    pub local_path: PathBuf,
    /// Lowercase hex sha256 of the patch *file* bytes — the thing that flips
    /// the fingerprint when a local patch is edited.
    pub content_sha256: String,
    /// Strip-depth selection.
    pub depth: DepthSpec,
}

/// A package-scoped patch whose source file **cannot be read yet** because it
/// lives inside a vendor package that is not installed at plan-build time —
/// the common case being a patch shipped as its own Composer package (a
/// dedicated "patches repo") after `rm -rf vendor/`. It carries everything
/// needed to [materialize](DeferredPatch::materialize) it once the owning
/// package has been extracted, at which point it becomes a regular
/// [`MaterializedPatch`]. See [`PatchPlan::resolve_deferred`].
///
/// Only ever `PatchScope::Package`: project-root (`patches/`-directory) patches
/// live under the project root, never inside `vendor/`, so they are always
/// materialized eagerly.
#[derive(Debug, Clone)]
pub struct DeferredPatch {
    /// Target package (`vendor/name`) this patch applies to.
    pub target: String,
    /// Human description (PATCHES.txt + reporting).
    pub description: String,
    /// The original declared path string, for reporting + the lock's human view.
    pub origin: String,
    /// Absolute path the source resolves to — read once the owning package is
    /// on disk.
    pub local_path: PathBuf,
    /// Strip-depth selection.
    pub depth: DepthSpec,
    /// Declared sha256 to verify once the bytes are readable (v2 expanded form).
    pub declared_sha256: Option<String>,
}

impl DeferredPatch {
    /// Read the now-present source file and turn this into a
    /// [`MaterializedPatch`], verifying any declared sha256. Errors if the
    /// file is still absent (a genuinely missing patch, now surfaced after the
    /// install rather than before it).
    pub fn materialize(&self) -> Result<MaterializedPatch> {
        let bytes = std::fs::read(&self.local_path).wrap_err_with(|| {
            format!(
                "patch file `{}` for `{}` not found after installing packages",
                self.local_path.display(),
                self.target,
            )
        })?;
        let sha = content_sha256(&bytes);
        if let Some(declared) = &self.declared_sha256
            && !declared.eq_ignore_ascii_case(&sha)
        {
            bail!(
                "patch `{}` for `{}`: sha256 mismatch (declared {declared}, actual {sha})",
                self.description,
                self.target,
            );
        }
        Ok(MaterializedPatch {
            description: self.description.clone(),
            origin: self.origin.clone(),
            local_path: self.local_path.clone(),
            content_sha256: sha,
            depth: self.depth,
        })
    }
}

/// A project-root ("top-level") patch: applied once at the project root, but
/// coupled to every package its diff touches so a change re-extracts them all
/// pristine before it is re-applied as a unit.
#[derive(Debug, Clone)]
pub struct RootPatch {
    /// The materialized patch, applied at the project root.
    pub patch: MaterializedPatch,
    /// The packages the patch touches (sorted, deduped). Each contributes this
    /// patch to its re-application fingerprint.
    pub packages: Vec<String>,
}

/// The full set of patches to apply this run, keyed by target package, plus
/// the applied-state fingerprints from the previous run.
#[derive(Debug, Clone, Default)]
pub struct PatchPlan {
    /// Target package name → its patches, in apply order.
    pub patches: BTreeMap<String, Vec<MaterializedPatch>>,
    /// Project-root patches spanning multiple packages, in apply order.
    pub root_patches: Vec<RootPatch>,
    /// Package-scoped patches whose source file wasn't readable at plan-build
    /// time (it lives inside a not-yet-installed vendor package). Materialized
    /// and folded into [`patches`](Self::patches) by
    /// [`resolve_deferred`](Self::resolve_deferred) once the owning package has
    /// been extracted.
    pub deferred: Vec<DeferredPatch>,
    /// Package name → applied fingerprint, loaded from `patches.lock.json`.
    pub applied: BTreeMap<String, String>,
    /// What to do when a patch fails to apply.
    pub failure_mode: FailureMode,
    /// Suppress the per-directory `PATCHES.txt` report.
    pub skip_report: bool,
    /// Also emit the v2-shaped human view in `patches.lock.json`.
    pub write_lock: bool,
}

impl PatchPlan {
    /// Whether the plan declares any patch at all.
    pub fn is_empty(&self) -> bool {
        self.patches.is_empty() && self.root_patches.is_empty() && self.deferred.is_empty()
    }

    /// The target packages of the plan's deferred patches — forced into the
    /// install set so they are re-extracted pristine before the deferred patch
    /// is applied (on a clean `vendor/` they'd be extracted anyway; this covers
    /// the case where only the patch-owning package was missing).
    pub fn deferred_targets(&self) -> impl Iterator<Item = &str> {
        self.deferred.iter().map(|d| d.target.as_str())
    }

    /// Materialize every [`deferred`](Self::deferred) patch — now that the
    /// owning packages are on disk — and fold each into
    /// [`patches`](Self::patches) under its target, then clear the deferred
    /// list. Deferred patches append after any eagerly-materialized patches for
    /// the same target (a target patched by both a project `patches/` file and
    /// a vendor-shipped one is unusual; the ordering is deterministic across
    /// runs, which is what the fingerprint needs).
    pub fn resolve_deferred(&mut self) -> Result<()> {
        for d in std::mem::take(&mut self.deferred) {
            let mp = d.materialize()?;
            self.patches.entry(d.target).or_default().push(mp);
        }
        Ok(())
    }

    /// The packages this plan targets.
    pub fn targets(&self) -> impl Iterator<Item = &str> {
        self.patches.keys().map(String::as_str)
    }

    /// The root patches (if any) that touch `package`.
    fn root_patches_for<'a>(&'a self, package: &'a str) -> impl Iterator<Item = &'a MaterializedPatch> {
        self.root_patches
            .iter()
            .filter(move |rp| rp.packages.iter().any(|p| p == package))
            .map(|rp| &rp.patch)
    }

    /// The desired fingerprint for `package` given its patches — the
    /// package-scoped patches followed by any root patches that touch it.
    /// `None` when the package has no patches (the no-patch state, which must
    /// differ from any non-empty applied fingerprint to trigger a pristine
    /// restore).
    pub fn desired_fingerprint(&self, package: &str) -> Option<String> {
        let pkg = self.patches.get(package).map_or(&[][..], Vec::as_slice);
        if pkg.is_empty() && self.root_patches_for(package).next().is_none() {
            return None;
        }
        Some(fingerprint_iter(pkg.iter().chain(self.root_patches_for(package))))
    }

    /// Whether `package` must be force-re-extracted because its patch set
    /// changed since the applied state (added / removed / edited patches, or
    /// the patch→no-patch transition).
    pub fn fingerprint_changed(&self, package: &str) -> bool {
        self.desired_fingerprint(package).as_deref() != self.applied.get(package).map(String::as_str)
    }

    /// The v2-shaped human view of the plan's patches:
    /// `target → [ { description, url, sha256, depth? } ]`. For the
    /// `write_lock` opt-in serialization in `patches.lock.json`.
    pub fn human_view(&self) -> serde_json::Value {
        let entry = |p: &MaterializedPatch| {
            let mut e = serde_json::Map::new();
            e.insert("description".into(), p.description.clone().into());
            e.insert("url".into(), p.origin.clone().into());
            e.insert("sha256".into(), p.content_sha256.clone().into());
            if let DepthSpec::Fixed(n) = p.depth {
                e.insert("depth".into(), n.into());
            }
            serde_json::Value::Object(e)
        };
        // Group by package, folding each root patch under every package it
        // touches so the view reflects what is applied to that tree.
        let mut grouped: BTreeMap<String, Vec<serde_json::Value>> = BTreeMap::new();
        for (target, patches) in &self.patches {
            grouped.entry(target.clone()).or_default().extend(patches.iter().map(entry));
        }
        for rp in &self.root_patches {
            let e = entry(&rp.patch);
            for pkg in &rp.packages {
                grouped.entry(pkg.clone()).or_default().push(e.clone());
            }
        }
        serde_json::Value::Object(
            grouped
                .into_iter()
                .map(|(k, v)| (k, serde_json::Value::Array(v)))
                .collect(),
        )
    }

    /// Every package that is either targeted now or was patched before — the
    /// union over which `fingerprint_changed` is meaningful. Used to force the
    /// stale ones into the install set.
    pub fn tracked_packages(&self) -> impl Iterator<Item = &str> {
        self.patches
            .keys()
            .chain(self.applied.keys())
            .map(String::as_str)
            .chain(
                self.root_patches
                    .iter()
                    .flat_map(|rp| rp.packages.iter().map(String::as_str)),
            )
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
    }
}

/// A stable fingerprint over a package's patches in apply order: each patch's
/// file content hash + its resolved depth. Editing a local patch flips the
/// content hash; reordering or changing depth flips the fingerprint too.
pub fn fingerprint(patches: &[MaterializedPatch]) -> String {
    fingerprint_iter(patches.iter())
}

/// [`fingerprint`] over an arbitrary iterator of patches — used to combine a
/// package's own patches with the root patches that touch it.
fn fingerprint_iter<'a>(patches: impl Iterator<Item = &'a MaterializedPatch>) -> String {
    let mut canonical = String::new();
    for p in patches {
        let depth = match p.depth {
            DepthSpec::Auto => "auto".to_string(),
            DepthSpec::Fixed(n) => n.to_string(),
        };
        canonical.push_str(&p.content_sha256);
        canonical.push(':');
        canonical.push_str(&depth);
        canonical.push('\n');
    }
    content_sha256(canonical.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mp(sha: &str, depth: DepthSpec) -> MaterializedPatch {
        MaterializedPatch {
            description: "d".into(),
            origin: "o".into(),
            local_path: PathBuf::from("/tmp/x.patch"),
            content_sha256: sha.into(),
            depth,
        }
    }

    #[test]
    fn fingerprint_is_order_and_content_sensitive() {
        let a = vec![mp("aa", DepthSpec::Auto), mp("bb", DepthSpec::Fixed(2))];
        let b = vec![mp("bb", DepthSpec::Fixed(2)), mp("aa", DepthSpec::Auto)];
        assert_ne!(fingerprint(&a), fingerprint(&b), "order matters");

        let c = vec![mp("aa", DepthSpec::Auto), mp("cc", DepthSpec::Fixed(2))];
        assert_ne!(fingerprint(&a), fingerprint(&c), "content matters");

        let d = vec![mp("aa", DepthSpec::Auto), mp("bb", DepthSpec::Fixed(2))];
        assert_eq!(fingerprint(&a), fingerprint(&d), "same inputs → same fp");
    }

    #[test]
    fn fingerprint_changed_detects_edit_add_remove() {
        let mut plan = PatchPlan::default();
        plan.patches
            .insert("vendor/p".into(), vec![mp("aa", DepthSpec::Auto)]);
        // No applied entry yet → changed (newly added).
        assert!(plan.fingerprint_changed("vendor/p"));

        // Record the applied fingerprint → no longer changed.
        let fp = plan.desired_fingerprint("vendor/p").unwrap();
        plan.applied.insert("vendor/p".into(), fp);
        assert!(!plan.fingerprint_changed("vendor/p"));

        // Edit the patch content → changed again.
        plan.patches
            .insert("vendor/p".into(), vec![mp("zz", DepthSpec::Auto)]);
        assert!(plan.fingerprint_changed("vendor/p"));
    }

    #[test]
    fn patch_to_no_patch_transition_is_a_change() {
        let mut plan = PatchPlan::default();
        // Previously patched, now no patches declared.
        plan.applied.insert("vendor/p".into(), "deadbeef".into());
        assert_eq!(plan.desired_fingerprint("vendor/p"), None);
        assert!(plan.fingerprint_changed("vendor/p"));
        // The package is tracked so it gets restored to pristine.
        let tracked: Vec<&str> = plan.tracked_packages().collect();
        assert_eq!(tracked, vec!["vendor/p"]);
    }

    #[test]
    fn resolve_deferred_reads_and_folds_into_patches() {
        let dir = tempfile::tempdir().unwrap();
        let patch = dir.path().join("vendor/acme/patches/fix.patch");
        std::fs::create_dir_all(patch.parent().unwrap()).unwrap();
        std::fs::write(&patch, b"--- a\n+++ b\n").unwrap();

        let mut plan = PatchPlan {
            deferred: vec![DeferredPatch {
                target: "acme/widget".into(),
                description: "Fix from vendor".into(),
                origin: "vendor/acme/patches/fix.patch".into(),
                local_path: patch.clone(),
                depth: DepthSpec::Auto,
                declared_sha256: None,
            }],
            ..PatchPlan::default()
        };
        // Before resolution: not empty, target surfaced, not yet in `patches`.
        assert!(!plan.is_empty());
        assert_eq!(plan.deferred_targets().collect::<Vec<_>>(), vec!["acme/widget"]);
        assert!(!plan.patches.contains_key("acme/widget"));

        plan.resolve_deferred().unwrap();

        // After: deferred drained, patch folded under its target with the hash.
        assert!(plan.deferred.is_empty());
        let widget = plan.patches.get("acme/widget").expect("folded in");
        assert_eq!(widget.len(), 1);
        assert_eq!(widget[0].content_sha256, content_sha256(b"--- a\n+++ b\n"));
        assert_eq!(widget[0].local_path, patch);
    }

    #[test]
    fn resolve_deferred_errors_on_missing_or_mismatched() {
        // Still-missing file → error (surfaced after install, not before).
        let dir = tempfile::tempdir().unwrap();
        let mut plan = PatchPlan {
            deferred: vec![DeferredPatch {
                target: "acme/widget".into(),
                description: "d".into(),
                origin: "o".into(),
                local_path: dir.path().join("nope.patch"),
                depth: DepthSpec::Auto,
                declared_sha256: None,
            }],
            ..PatchPlan::default()
        };
        assert!(plan.resolve_deferred().is_err());

        // Present file but wrong declared sha → error.
        let patch = dir.path().join("fix.patch");
        std::fs::write(&patch, b"data").unwrap();
        let mut plan = PatchPlan {
            deferred: vec![DeferredPatch {
                target: "acme/widget".into(),
                description: "d".into(),
                origin: "o".into(),
                local_path: patch,
                depth: DepthSpec::Auto,
                declared_sha256: Some("00".into()),
            }],
            ..PatchPlan::default()
        };
        assert!(plan.resolve_deferred().is_err());
    }
}
