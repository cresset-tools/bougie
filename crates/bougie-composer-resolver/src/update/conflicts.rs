//! Composer `conflict` handling that spans more than one solve: the
//! post-solve retry loop, and the no-solution explanation.
//!
//! Most conflicts reach pubgrub directly as dependency edges (see
//! `push_conflict_map`). The rest are caught after a solve by
//! `check_conflict_violations`. Either way, a failed resolution has to
//! *name* the conflict. Before this module it said the declaring package
//! had "no available version", which sends people looking at their
//! repositories and stability flags instead of at the `conflict` entry.

use std::collections::BTreeSet;
use std::fmt::Write as _;

use crate::hash::{FxHashMap, FxHashSet};
use composer_semver::constraint::Constraint;
use composer_semver::version::Version;
use eyre::eyre;
use pubgrub::{
    DefaultStringReporter, DerivationTree, Derived, External, Map, PubGrubError, ReportFormatter,
    Reporter, SelectedDependencies, Term, resolve,
};

use super::{
    ComposerRange, LockPackage, PackageName, PubGrubPackage, ResolveProvider, format_version_list,
    is_platform, split_stability_flag, to_range,
};

/// A pubgrub solution for this provider.
pub(super) type Solution = SelectedDependencies<PubGrubPackage, Version>;

/// How many times a solve is retried after excluding versions that
/// violate a declared conflict.
const MAX_CONFLICT_RETRIES: usize = 10;

/// Conflict lines listed per problem before the rest are summarized.
const MAX_CONFLICT_LINES: usize = 5;

/// A declared `conflict` that a solution violated: `declarer` at
/// `version` lists `target` in its `conflict` map, and `target` at
/// `target_version` was in the same solution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ConflictViolation {
    pub(super) declarer: PackageName,
    pub(super) version: Version,
    pub(super) target: PackageName,
    pub(super) target_version: Version,
    /// `target_version` as the repository spells it (`1.0.0`, not the
    /// normalized `1.0.0.0`).
    pub(super) target_pretty: String,
    /// The declarer's raw conflict constraint (`*`, `<2.0`).
    pub(super) constraint: String,
}

impl ConflictViolation {
    /// The reason attached to the excluded version. pubgrub carries it
    /// in `External::Custom`, which [`ConflictAwareFormatter`] renders
    /// as `<declarer> <version> <reason>`.
    pub(super) fn reason(&self) -> String {
        format!("conflicts with {} {}", self.target, self.target_pretty)
    }
}

/// Dependency edges `(declarer, target)` that came from `conflict`.
#[derive(Debug, Clone, Default)]
pub(crate) struct ConflictEdge {
    /// Raw conflict constraints seen across the declarer's versions.
    pub(super) constraints: BTreeSet<String>,
    /// Some declarer version also requires or replaces the target, so
    /// the edge is a narrowed require rather than a pure conflict.
    pub(super) mixed: bool,
}

/// The pretty (repository-spelled) version of `name` at `version`, or
/// the normalized form when the entry isn't cached.
pub(super) fn pretty_version_in(
    cache: &FxHashMap<PackageName, Vec<(Version, LockPackage)>>,
    name: &str,
    version: &Version,
) -> String {
    cache
        .get(name)
        .and_then(|vs| vs.iter().find(|(v, _)| v == version))
        .map_or_else(|| version.to_string(), |(_, e)| e.version.clone())
}

impl ResolveProvider {
    /// Solve, then re-solve while the solution violates a declared
    /// conflict that pubgrub could not see (one whose target is not a
    /// root require or a require of the declarer). Each round excludes
    /// the violating declarer versions; `get_dependencies` then reports
    /// them as unavailable with the conflict as the reason.
    ///
    /// Shared by the dry-run and lockfile paths so both apply the same
    /// retry bound and the same exhaustion check.
    // Same `Err` type as `pubgrub::resolve`, which this wraps.
    #[allow(clippy::result_large_err)]
    pub(super) fn solve_with_conflict_retries(
        &self,
        root: &Version,
    ) -> Result<Solution, PubGrubError<Self>> {
        let mut result = resolve(self, PubGrubPackage::Root, root.clone());
        for _retry in 0..MAX_CONFLICT_RETRIES {
            let Ok(ref solution) = result else { break };
            let pairs: Vec<_> = solution.iter().collect();
            let violations = self.check_conflict_violations(&pairs);
            if violations.is_empty() {
                break;
            }
            tracing::debug!(
                count = violations.len(),
                excluded = ?violations,
                "conflict violations found, re-solving",
            );
            {
                let mut excludes = self.conflict_excludes.borrow_mut();
                for v in violations {
                    excludes.insert((v.declarer.clone(), v.version.clone()), v);
                }
            }
            self.reset_for_re_solve();
            result = resolve(self, PubGrubPackage::Root, root.clone());
        }
        result
    }

    /// Error when `solution` still violates a declared conflict, which
    /// happens only when [`Self::solve_with_conflict_retries`] ran out
    /// of retries. Writing that solution would produce a lock Composer
    /// itself refuses to install.
    pub(super) fn ensure_no_conflict_violations(&self, solution: &Solution) -> eyre::Result<()> {
        let pairs: Vec<_> = solution.iter().collect();
        let remaining = self.check_conflict_violations(&pairs);
        if remaining.is_empty() {
            return Ok(());
        }
        let detail = remaining
            .iter()
            .map(|v| {
                format!(
                    "{}@{} conflicts with {} {}",
                    v.declarer, v.version, v.target, v.target_pretty
                )
            })
            .collect::<Vec<_>>()
            .join(", ");
        Err(eyre!(
            "could not satisfy declared package conflicts after {MAX_CONFLICT_RETRIES} \
             resolution attempts; still violated: {detail}"
        ))
    }

    /// The explanation for a `NoSolution`: Composer-style problems when
    /// the root requires can be checked directly, otherwise pubgrub's
    /// derivation tree with conflict edges worded as conflicts.
    pub(crate) fn explain_no_solution(
        &self,
        tree: &DerivationTree<PubGrubPackage, ComposerRange, String>,
    ) -> String {
        self.analyze_resolution_problems().unwrap_or_else(|| {
            let edges = self.conflict_edges.borrow();
            DefaultStringReporter::report_with_formatter(
                tree,
                &ConflictAwareFormatter { edges: &edges },
            )
        })
    }

    /// Problems where two root requires cannot be installed together
    /// because of a `conflict` either of them declares: every in-range
    /// version of one conflicts with every in-range version of the
    /// other. Matches Composer's wording, e.g. for a renamed package
    /// that declares `"conflict": {"old/name": "*"}`:
    ///
    /// ```text
    ///   Problem 1
    ///     - Root composer.json requires new/name * -> satisfiable by new/name[2.0.0].
    ///     - Root composer.json requires old/name * -> satisfiable by old/name[1.0.0].
    ///     - new/name 2.0.0 conflicts with old/name 1.0.0.
    /// ```
    pub(super) fn collect_declared_conflict_problems(
        &self,
        problems: &mut Vec<String>,
        unsatisfiable: &FxHashSet<String>,
    ) {
        let candidates: Vec<(&PackageName, Vec<(Version, LockPackage)>)> = self
            .root_deps
            .iter()
            .filter(|(name, _)| {
                !is_platform(name.as_str()) && !unsatisfiable.contains(name.as_str())
            })
            .filter_map(|(name, range)| {
                let in_range: Vec<_> = self
                    .peek_cached_versions(name.as_str())?
                    .into_iter()
                    .filter(|(v, _)| range.contains(v))
                    .collect();
                (!in_range.is_empty()).then_some((name, in_range))
            })
            .collect();

        for (i, (a, a_versions)) in candidates.iter().enumerate() {
            for (b, b_versions) in &candidates[i + 1..] {
                let Some(lines) = every_pair_conflicts(a, a_versions, b, b_versions) else {
                    continue;
                };
                let mut problem = format!("  Problem {}", problems.len() + 1);
                for (name, versions) in [(a, a_versions), (b, b_versions)] {
                    let constraint = self.raw_constraint_for(name.as_str());
                    let list = format_version_list(versions.iter().map(|(_, e)| e.version.clone()));
                    let _ = write!(
                        problem,
                        "\n    - Root composer.json requires {name} {constraint} \
                         -> satisfiable by {name}[{list}]."
                    );
                }
                for line in lines.iter().take(MAX_CONFLICT_LINES) {
                    let _ = write!(problem, "\n    - {line}");
                }
                if lines.len() > MAX_CONFLICT_LINES {
                    let _ = write!(
                        problem,
                        "\n    - … and {} more conflicting combinations.",
                        lines.len() - MAX_CONFLICT_LINES
                    );
                }
                problems.push(problem);
            }
        }
    }
}

/// `Some(lines)` when every version in `a_versions` conflicts with every
/// version in `b_versions` (through a `conflict` declared by either
/// side), one `"x 1.0 conflicts with y 2.0."` line per pair; `None` as
/// soon as one pair can coexist.
fn every_pair_conflicts(
    a: &PackageName,
    a_versions: &[(Version, LockPackage)],
    b: &PackageName,
    b_versions: &[(Version, LockPackage)],
) -> Option<Vec<String>> {
    let mut lines = Vec::new();
    for (av, ae) in a_versions {
        for (bv, be) in b_versions {
            let line = declared_conflict(a, ae, b, bv, be)
                .or_else(|| declared_conflict(b, be, a, av, ae))?;
            lines.push(line);
        }
    }
    Some(lines)
}

/// The conflict line when `declarer` (entry `decl`) declares a conflict
/// matching `target` at `target_version`.
fn declared_conflict(
    declarer: &PackageName,
    decl: &LockPackage,
    target: &PackageName,
    target_version: &Version,
    target_entry: &LockPackage,
) -> Option<String> {
    let raw = decl.conflict.get(target.as_str())?;
    let effective = if raw == "self.version" {
        decl.version.as_str()
    } else {
        raw.as_str()
    };
    let (cleaned, _) = split_stability_flag(effective);
    let constraint = Constraint::parse(cleaned).ok()?;
    to_range(&constraint).contains(target_version).then(|| {
        format!(
            "{declarer} {} conflicts with {target} {}.",
            decl.version, target_entry.version
        )
    })
}

/// pubgrub's default report wording, except that dependency edges which
/// came from a `conflict` read "conflicts with" instead of "depends on",
/// and excluded versions read "`<package> <version>` conflicts with …".
///
/// Every method is reimplemented rather than delegated because the
/// default formatter's `explain_*` methods call its *own*
/// `format_external`, which would bypass the override.
struct ConflictAwareFormatter<'a> {
    edges: &'a FxHashMap<(PackageName, PackageName), ConflictEdge>,
}

type Ext = External<PubGrubPackage, ComposerRange, String>;
type Terms = Map<PubGrubPackage, Term<ComposerRange>>;
type Der = Derived<PubGrubPackage, ComposerRange, String>;

impl ConflictAwareFormatter<'_> {
    /// `Some(constraint)` when `owner → dep` is a pure conflict edge.
    fn conflict_constraint(
        &self,
        owner: &PubGrubPackage,
        dep: &PubGrubPackage,
        dep_range: &ComposerRange,
    ) -> Option<String> {
        let (PubGrubPackage::Package(o), PubGrubPackage::Package(d)) = (owner, dep) else {
            return None;
        };
        let edge = self.edges.get(&(o.clone(), d.clone()))?;
        if edge.mixed {
            return None;
        }
        // One raw constraint across the owner's versions reads best; if
        // they differ, fall back to the excluded range itself.
        Some(
            match edge.constraints.iter().collect::<Vec<_>>().as_slice() {
                [only] => (*only).clone(),
                _ => dep_range.complement().to_string(),
            },
        )
    }

    fn package_and_range(package: &PubGrubPackage, range: &ComposerRange) -> String {
        if *range == ComposerRange::full() {
            package.to_string()
        } else {
            format!("{package} {range}")
        }
    }

    fn terms(&self, terms: &Terms) -> String {
        let terms_vec: Vec<_> = terms.iter().collect();
        match terms_vec.as_slice() {
            [] => "version solving failed".into(),
            [(package, Term::Positive(range))] => format!("{package} {range} is forbidden"),
            [(package, Term::Negative(range))] => format!("{package} {range} is mandatory"),
            [(p1, Term::Positive(r1)), (p2, Term::Negative(r2))]
            | [(p2, Term::Negative(r2)), (p1, Term::Positive(r1))] => self.external(
                &External::FromDependencyOf((*p1).clone(), r1.clone(), (*p2).clone(), r2.clone()),
            ),
            slice => {
                let str_terms: Vec<_> = slice.iter().map(|(p, t)| format!("{p} {t}")).collect();
                str_terms.join(", ") + " are incompatible"
            }
        }
    }

    fn external(&self, external: &Ext) -> String {
        match external {
            External::FromDependencyOf(p, set_p, dep, set_dep) => {
                match self.conflict_constraint(p, dep, set_dep) {
                    Some(c) => format!(
                        "{} conflicts with {dep} {c}",
                        Self::package_and_range(p, set_p)
                    ),
                    None => external.to_string(),
                }
            }
            External::Custom(p, set, reason) => {
                format!("{} {reason}", Self::package_and_range(p, set))
            }
            _ => external.to_string(),
        }
    }
}

impl ReportFormatter<PubGrubPackage, ComposerRange, String> for ConflictAwareFormatter<'_> {
    type Output = String;

    fn format_external(&self, external: &Ext) -> String {
        self.external(external)
    }

    fn format_terms(&self, terms: &Terms) -> String {
        self.terms(terms)
    }

    fn explain_both_external(&self, external1: &Ext, external2: &Ext, current: &Terms) -> String {
        format!(
            "Because {} and {}, {}.",
            self.external(external1),
            self.external(external2),
            self.terms(current)
        )
    }

    fn explain_both_ref(
        &self,
        ref_id1: usize,
        derived1: &Der,
        ref_id2: usize,
        derived2: &Der,
        current: &Terms,
    ) -> String {
        format!(
            "Because {} ({ref_id1}) and {} ({ref_id2}), {}.",
            self.terms(&derived1.terms),
            self.terms(&derived2.terms),
            self.terms(current)
        )
    }

    fn explain_ref_and_external(
        &self,
        ref_id: usize,
        derived: &Der,
        external: &Ext,
        current: &Terms,
    ) -> String {
        format!(
            "Because {} ({ref_id}) and {}, {}.",
            self.terms(&derived.terms),
            self.external(external),
            self.terms(current)
        )
    }

    fn and_explain_external(&self, external: &Ext, current: &Terms) -> String {
        format!(
            "And because {}, {}.",
            self.external(external),
            self.terms(current)
        )
    }

    fn and_explain_ref(&self, ref_id: usize, derived: &Der, current: &Terms) -> String {
        format!(
            "And because {} ({ref_id}), {}.",
            self.terms(&derived.terms),
            self.terms(current)
        )
    }

    fn and_explain_prior_and_external(
        &self,
        prior_external: &Ext,
        external: &Ext,
        current: &Terms,
    ) -> String {
        format!(
            "And because {} and {}, {}.",
            self.external(prior_external),
            self.external(external),
            self.terms(current)
        )
    }
}

#[cfg(test)]
mod tests;
