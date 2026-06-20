//! Composer `Constraint` → pubgrub `Ranges<Version>` conversion.
//!
//! Encodes Composer's prerelease-vs-stable matching asymmetry into
//! boundary marker choice — pubgrub's set operations (intersection,
//! complement, union) work generically over the resulting `Ranges`,
//! so we only need to get the boundaries right at the leaves.
//!
//! Rules (where `X-dev` and `X-stable` mean `X.with_suffix(Suffix::Dev|Stable)`):
//!
//! | Constraint atom                  | Range                                |
//! | -------------------------------- | ------------------------------------ |
//! | `==X`                            | `singleton(X-as-parsed)`             |
//! | `!=X`                            | `singleton(X).complement()`          |
//! | `>X`                             | `strictly_higher_than(X-stable)`     |
//! | `>=X` *(explicit lower bound)*   | `higher_than(X-dev)`                 |
//! | `>=X` *(synthesized from partial)*| `higher_than(X-stable)`             |
//! | `<X`                             | `strictly_lower_than(X-dev)`         |
//! | `<=X`                            | `lower_than(X-stable)`               |
//! | `Any`                            | `Ranges::full()`                     |
//! | `And(items)`                     | intersection-fold                    |
//! | `Or(items)`                      | union-fold                           |
//!
//! The `dev` / `stable` boundary marker trick is what implements
//! "prereleases of X satisfy `^X` but not `1` (partial)" and "`<X`
//! rejects X-prereleases" — see [`composer_semver::constraint`] for
//! the matching tests on the same rules.
//!
//! Branch-only versions (`dev-feature-foo`) have no numeric body, so
//! they're encoded as singletons. Combining them through pubgrub's
//! `Ranges` works because two distinct branches sort distinctly
//! (lex order over the body), so the intervals don't collide.

use composer_semver::constraint::Constraint;
use composer_semver::version::{CmpOp, Suffix, Version, VersionKind};
use pubgrub::Ranges;

pub type ComposerRange = Ranges<Version>;

/// Convert a Composer constraint into the equivalent pubgrub
/// `Ranges<Version>`. The conversion preserves [`Constraint::matches`]
/// semantics — see module docs for the boundary-marker table.
pub fn to_range(c: &Constraint) -> ComposerRange {
    match c {
        Constraint::Any => Ranges::full(),
        Constraint::Op { op, version, explicit_lower_bound } => {
            atom_to_range(*op, version, *explicit_lower_bound)
        }
        Constraint::And(items) => items
            .iter()
            .map(to_range)
            .fold(Ranges::full(), |acc, r| acc.intersection(&r)),
        Constraint::Or(items) => items
            .iter()
            .map(to_range)
            .fold(Ranges::empty(), |acc, r| acc.union(&r)),
    }
}

fn atom_to_range(op: CmpOp, target: &Version, explicit_lower: bool) -> ComposerRange {
    // Branch targets only support equality / inequality — there's no
    // ordering across branches in Composer, and `Branch < Numeric`
    // is the only inter-class rule (handled by `Version: Ord`).
    if matches!(target.kind, VersionKind::Branch(_)) {
        return match op {
            CmpOp::Eq => Ranges::singleton(target.clone()),
            CmpOp::Ne => Ranges::singleton(target.clone()).complement(),
            // For inequalities involving a branch, the only versions
            // that satisfy by Ord are numerics (which are > Branch).
            // We approximate as "everything strictly greater" which
            // is what `Ord` would compute. Phase B β doesn't exercise
            // these in its test suite; the encoding is here for
            // completeness rather than fidelity.
            CmpOp::Gt | CmpOp::Ge => Ranges::strictly_higher_than(target.clone()),
            CmpOp::Lt | CmpOp::Le => Ranges::strictly_lower_than(target.clone()),
        };
    }

    let dev_marker = target.with_suffix(Suffix::Dev).unwrap_or_else(|| target.clone());
    let stable_marker = target.with_suffix(Suffix::Stable).unwrap_or_else(|| target.clone());

    match op {
        CmpOp::Eq => Ranges::singleton(target.clone()),
        CmpOp::Ne => Ranges::singleton(target.clone()).complement(),
        CmpOp::Gt => Ranges::strictly_higher_than(stable_marker),
        CmpOp::Ge => {
            if explicit_lower {
                Ranges::higher_than(dev_marker)
            } else {
                Ranges::higher_than(stable_marker)
            }
        }
        CmpOp::Lt => Ranges::strictly_lower_than(dev_marker),
        CmpOp::Le => Ranges::lower_than(stable_marker),
    }
}

#[cfg(test)]
mod tests;
