//! Tests for `conflict` handling: how conflicts constrain the solve, and
//! how a failed solve explains them.
//!
//! Each test serves a handful of packages from a wiremock `/p2/` repo
//! and runs the same solve the dry-run and lockfile paths use
//! ([`ResolveProvider::solve_with_conflict_retries`] plus the exhaustion
//! guard), returning either the chosen versions or the explanation the
//! user would see.

use super::*;
use crate::update::ResolutionStrategy;
use bougie_paths::Paths;
use serde_json::{Value, json};
use tempfile::TempDir;
use wiremock::matchers::{method, path as wm_path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// One version entry; `extras` carries `require`, `conflict`, … verbatim.
fn entry(name: &str, version: &str, extras: Value) -> Value {
    let mut e = json!({
        "name": name,
        "version": version,
        "version_normalized": format!("{version}.0"),
        "type": "library",
        "dist": {"type": "zip", "url": format!("https://e/{name}/{version}.zip"), "shasum": "aa"},
    });
    if let Value::Object(extra) = extras {
        let obj = e.as_object_mut().unwrap();
        for (k, v) in extra {
            obj.insert(k, v);
        }
    }
    e
}

/// A provider over a mock repository. Keeps the runtime and server alive
/// for as long as the provider fetches from them.
struct Fixture {
    provider: ResolveProvider,
    _server: MockServer,
    _rt: tokio::runtime::Runtime,
    _tmp: TempDir,
}

fn fixture(packages: &[(&str, Vec<Value>)], composer_json: &Value) -> Fixture {
    let tmp = TempDir::new().unwrap();
    let home = tmp.path().join("home");
    let cache = tmp.path().join("cache");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&cache).unwrap();
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let server = rt.block_on(async {
        let server = MockServer::start().await;
        for (name, versions) in packages {
            let body = json!({ "packages": { *name: versions } }).to_string();
            Mock::given(method("GET"))
                .and(wm_path(format!("/p2/{name}.json")))
                .respond_with(ResponseTemplate::new(200).set_body_string(body))
                .mount(&server)
                .await;
        }
        server
    });
    let provider = ResolveProvider::build(
        crate::metadata::build_client().unwrap(),
        Paths::new(home, cache),
        crate::metadata::Repo::from_url(server.uri()),
        composer_json,
        true,
    )
    .unwrap();
    Fixture {
        provider,
        _server: server,
        _rt: rt,
        _tmp: tmp,
    }
}

/// Run the production solve. `Ok` maps package name → chosen version
/// (normalized); `Err` is the explanation printed to the user.
fn solve(f: &Fixture) -> Result<FxHashMap<String, String>, String> {
    f.provider.pre_fetch_closure().unwrap();
    let root = f.provider.root_version();
    match f.provider.solve_with_conflict_retries(&root) {
        Ok(solution) => {
            f.provider
                .ensure_no_conflict_violations(&solution)
                .map_err(|e| e.to_string())?;
            Ok(solution
                .iter()
                .filter_map(|(p, v)| match p {
                    PubGrubPackage::Package(n) => Some((n.to_string(), v.to_string())),
                    PubGrubPackage::Root => None,
                })
                .collect())
        }
        Err(PubGrubError::NoSolution(tree)) => Err(f.provider.explain_no_solution(&tree)),
        Err(other) => panic!("unexpected solver error: {other:?}"),
    }
}

fn solved(f: &Fixture) -> FxHashMap<String, String> {
    solve(f).unwrap_or_else(|why| panic!("expected a solution, got:\n{why}"))
}

fn failed(f: &Fixture) -> String {
    match solve(f) {
        Ok(s) => panic!("expected no solution, got {s:?}"),
        Err(why) => why,
    }
}

/// The misleading message this module exists to prevent. (pubgrub's
/// "there is no version of X in <v | >v" is fine: it says v is X's only
/// version, next to the conflict that rules v out.)
fn assert_no_phantom_missing_version(why: &str) {
    assert!(
        !why.contains("no available version"),
        "a conflict must not be reported as a missing version:\n{why}"
    );
}

// --- Conflicts between two root requires (Composer-style problem) ---

#[test]
fn rename_conflict_between_root_requires_is_named() {
    // The bug report: a renamed package declares `conflict: {old: *}`
    // and the root still requires the old name.
    let f = fixture(
        &[
            (
                "example/old-name",
                vec![entry("example/old-name", "1.0.0", json!({}))],
            ),
            (
                "example/new-name",
                vec![entry(
                    "example/new-name",
                    "2.0.0",
                    json!({"conflict": {"example/old-name": "*"}}),
                )],
            ),
        ],
        &json!({"require": {"example/old-name": "*", "example/new-name": "*"}}),
    );
    let why = failed(&f);
    assert!(why.contains("Problem 1"), "{why}");
    assert!(
        why.contains(
            "Root composer.json requires example/new-name * -> satisfiable by example/new-name[2.0.0]."
        ),
        "{why}"
    );
    assert!(
        why.contains(
            "Root composer.json requires example/old-name * -> satisfiable by example/old-name[1.0.0]."
        ),
        "{why}"
    );
    assert!(
        why.contains("example/new-name 2.0.0 conflicts with example/old-name 1.0.0."),
        "{why}"
    );
    assert!(
        !why.contains("Problem 2"),
        "one conflict, one problem:\n{why}"
    );
    assert_no_phantom_missing_version(&why);
}

#[test]
fn conflict_declared_by_the_other_root_require_is_named() {
    // Same pair, but the *old* package declares the conflict. Conflicts
    // are symmetric, so the explanation must not depend on which side
    // carries the declaration or on root require order.
    let f = fixture(
        &[
            (
                "acme/old",
                vec![entry(
                    "acme/old",
                    "1.0.0",
                    json!({"conflict": {"acme/new": ">=2.0"}}),
                )],
            ),
            ("acme/new", vec![entry("acme/new", "2.0.0", json!({}))]),
        ],
        &json!({"require": {"acme/new": "^2.0", "acme/old": "^1.0"}}),
    );
    let why = failed(&f);
    assert!(
        why.contains("acme/old 1.0.0 conflicts with acme/new 2.0.0."),
        "{why}"
    );
    assert_no_phantom_missing_version(&why);
}

#[test]
fn rename_conflict_without_the_old_name_resolves() {
    // Regression guard: the conflict must not make the new name
    // uninstallable when nothing requires the old one.
    let f = fixture(
        &[(
            "example/new-name",
            vec![entry(
                "example/new-name",
                "2.0.0",
                json!({"conflict": {"example/old-name": "*"}}),
            )],
        )],
        &json!({"require": {"example/new-name": "*"}}),
    );
    let s = solved(&f);
    assert_eq!(s["example/new-name"], "2.0.0.0");
    assert!(!s.contains_key("example/old-name"), "{s:?}");
}

#[test]
fn range_conflict_with_no_compatible_target_is_named() {
    let f = fixture(
        &[
            ("acme/old", vec![entry("acme/old", "1.0.0", json!({}))]),
            (
                "acme/new",
                vec![entry(
                    "acme/new",
                    "2.0.0",
                    json!({"conflict": {"acme/old": "<2.0"}}),
                )],
            ),
        ],
        &json!({"require": {"acme/new": "*", "acme/old": "*"}}),
    );
    let why = failed(&f);
    assert!(
        why.contains("acme/new 2.0.0 conflicts with acme/old 1.0.0."),
        "{why}"
    );
    assert_no_phantom_missing_version(&why);
}

#[test]
fn every_conflicting_version_pair_is_listed_up_to_a_cap() {
    // Seven versions of the declarer, all conflicting: five lines, then
    // a summary, rather than an unbounded wall of text.
    let versions: Vec<Value> = (1..=7)
        .map(|i| {
            entry(
                "acme/new",
                &format!("2.{i}.0"),
                json!({"conflict": {"acme/old": "*"}}),
            )
        })
        .collect();
    let f = fixture(
        &[
            ("acme/old", vec![entry("acme/old", "1.0.0", json!({}))]),
            ("acme/new", versions),
        ],
        &json!({"require": {"acme/new": "^2.0", "acme/old": "*"}}),
    );
    let why = failed(&f);
    assert_eq!(
        why.matches("conflicts with acme/old 1.0.0.").count(),
        MAX_CONFLICT_LINES,
        "{why}"
    );
    assert!(
        why.contains("… and 2 more conflicting combinations."),
        "{why}"
    );
}

#[test]
fn partially_conflicting_root_requires_are_not_reported_as_a_conflict_problem() {
    // acme/new 2.0 conflicts with acme/old, but acme/new 1.0 does not:
    // the pair can coexist, so it solves (with the older acme/new).
    let f = fixture(
        &[
            ("acme/old", vec![entry("acme/old", "1.0.0", json!({}))]),
            (
                "acme/new",
                vec![
                    entry("acme/new", "2.0.0", json!({"conflict": {"acme/old": "*"}})),
                    entry("acme/new", "1.0.0", json!({})),
                ],
            ),
        ],
        &json!({"require": {"acme/new": "*", "acme/old": "*"}}),
    );
    let s = solved(&f);
    assert_eq!(s["acme/new"], "1.0.0.0");
    assert_eq!(s["acme/old"], "1.0.0.0");
}

// --- Conflicts the solver sees directly ---

#[test]
fn range_conflict_steers_the_solver_to_a_compatible_target() {
    // `lowest` would pick acme/old 1.0.0, which acme/new conflicts with.
    // The conflict is a solver constraint, so 2.0.0 is picked up front.
    let mut f = fixture(
        &[
            (
                "acme/old",
                vec![
                    entry("acme/old", "2.0.0", json!({})),
                    entry("acme/old", "1.0.0", json!({})),
                ],
            ),
            (
                "acme/new",
                vec![entry(
                    "acme/new",
                    "1.0.0",
                    json!({"conflict": {"acme/old": "<2.0"}}),
                )],
            ),
        ],
        &json!({"require": {"acme/new": "*", "acme/old": "*"}}),
    );
    f.provider.set_resolution(ResolutionStrategy::Lowest);
    let s = solved(&f);
    assert_eq!(s["acme/old"], "2.0.0.0");
    assert!(
        f.provider.conflict_excludes.borrow().is_empty(),
        "no post-solve retry should have been needed"
    );
}

#[test]
fn self_version_conflict_excludes_the_matching_target_version() {
    // `"self.version"` means "the declarer's own version".
    let mut f = fixture(
        &[
            (
                "acme/a",
                vec![entry(
                    "acme/a",
                    "1.0.0",
                    json!({"conflict": {"acme/b": "self.version"}}),
                )],
            ),
            (
                "acme/b",
                vec![
                    entry("acme/b", "2.0.0", json!({})),
                    entry("acme/b", "1.0.0", json!({})),
                ],
            ),
        ],
        &json!({"require": {"acme/a": "*", "acme/b": "*"}}),
    );
    f.provider.set_resolution(ResolutionStrategy::Lowest);
    let s = solved(&f);
    assert_eq!(s["acme/b"], "2.0.0.0");
}

#[test]
fn require_narrowed_by_a_conflict_keeps_the_require_bounds() {
    // acme/a requires acme/b ^1.0 and conflicts with acme/b <1.2. Both
    // edges target acme/b; they must intersect to >=1.2,<2.0. Collecting
    // them into pubgrub's map let the conflict overwrite the require,
    // and 2.0.0 got picked.
    let f = fixture(
        &[
            (
                "acme/a",
                vec![entry(
                    "acme/a",
                    "1.0.0",
                    json!({"require": {"acme/b": "^1.0"}, "conflict": {"acme/b": "<1.2"}}),
                )],
            ),
            (
                "acme/b",
                ["2.0.0", "1.3.0", "1.2.0", "1.1.0", "1.0.0"]
                    .iter()
                    .map(|v| entry("acme/b", v, json!({})))
                    .collect(),
            ),
        ],
        &json!({"require": {"acme/a": "*"}}),
    );
    let s = solved(&f);
    assert_eq!(s["acme/b"], "1.3.0.0");
}

#[test]
fn require_narrowed_by_a_conflict_to_nothing_has_no_solution() {
    // ^1.0 minus <2.0 leaves nothing; the only remaining version is 2.0.0,
    // which the require excludes. Previously the conflict overwrote the
    // require and 2.0.0 was locked.
    let f = fixture(
        &[
            (
                "acme/a",
                vec![entry(
                    "acme/a",
                    "1.0.0",
                    json!({"require": {"acme/b": "^1.0"}, "conflict": {"acme/b": "<2.0"}}),
                )],
            ),
            (
                "acme/b",
                vec![
                    entry("acme/b", "2.0.0", json!({})),
                    entry("acme/b", "1.0.0", json!({})),
                ],
            ),
        ],
        &json!({"require": {"acme/a": "*"}}),
    );
    let why = failed(&f);
    assert!(why.contains("acme/a"), "{why}");
}

#[test]
fn indirect_conflict_edge_reads_as_a_conflict_in_the_explanation() {
    // acme/new comes in through acme/wrapper, so the root-pair check
    // doesn't apply and the explanation is pubgrub's derivation tree.
    // Its conflict edge must read "conflicts with", not "depends on".
    let f = fixture(
        &[
            ("acme/old", vec![entry("acme/old", "1.0.0", json!({}))]),
            (
                "acme/wrapper",
                vec![entry(
                    "acme/wrapper",
                    "1.0.0",
                    json!({"require": {"acme/new": "*"}}),
                )],
            ),
            (
                "acme/new",
                vec![entry(
                    "acme/new",
                    "2.0.0",
                    json!({"conflict": {"acme/old": "*"}}),
                )],
            ),
        ],
        &json!({"require": {"acme/wrapper": "*", "acme/old": "*"}}),
    );
    let why = failed(&f);
    assert!(why.contains("conflicts with acme/old *"), "{why}");
    assert!(!why.contains("depends on acme/old"), "{why}");
    assert!(why.contains("acme/wrapper"), "{why}");
    assert_no_phantom_missing_version(&why);
}

// --- Conflicts only the post-solve check sees ---

/// Root requires acme/new and acme/user; acme/user pulls in acme/old.
/// acme/old is neither a root require nor required by acme/new, so
/// acme/new's conflict on it is checked after the solve.
fn indirect_fixture(new_versions: Vec<Value>) -> Fixture {
    fixture(
        &[
            ("acme/old", vec![entry("acme/old", "1.0.0", json!({}))]),
            (
                "acme/user",
                vec![entry(
                    "acme/user",
                    "1.0.0",
                    json!({"require": {"acme/old": "^1.0"}}),
                )],
            ),
            ("acme/new", new_versions),
        ],
        &json!({"require": {"acme/new": "*", "acme/user": "*"}}),
    )
}

#[test]
fn post_solve_conflict_falls_back_to_an_older_declarer_version() {
    let f = indirect_fixture(vec![
        entry("acme/new", "2.0.0", json!({"conflict": {"acme/old": "*"}})),
        entry("acme/new", "1.0.0", json!({})),
    ]);
    let s = solved(&f);
    assert_eq!(s["acme/new"], "1.0.0.0");
    assert_eq!(s["acme/old"], "1.0.0.0");
    let excludes = f.provider.conflict_excludes.borrow();
    let v = excludes.values().next().expect("one exclusion recorded");
    assert_eq!(v.declarer.as_str(), "acme/new");
    assert_eq!(v.target.as_str(), "acme/old");
    assert_eq!(v.target_pretty, "1.0.0");
    assert_eq!(v.constraint, "*");
}

#[test]
fn post_solve_conflict_without_fallback_names_the_conflict() {
    // The second half of the bug: the retry used to hide acme/new 2.0.0
    // from the solver, which then said acme/new had no available version.
    let f = indirect_fixture(vec![entry(
        "acme/new",
        "2.0.0",
        json!({"conflict": {"acme/old": "*"}}),
    )]);
    let why = failed(&f);
    assert!(why.contains("acme/new"), "{why}");
    assert!(why.contains("conflicts with acme/old 1.0.0"), "{why}");
    assert_no_phantom_missing_version(&why);
}

#[test]
fn exhausted_conflict_retries_are_an_error_not_a_solution() {
    // Twelve declarer versions, each conflicting with the indirect
    // target: each retry excludes one, and the bound runs out first.
    // The lock path used to skip this guard and could write the
    // violating solution.
    let versions = (1..=12)
        .rev()
        .map(|i| {
            entry(
                "acme/new",
                &format!("1.{i}.0"),
                json!({"conflict": {"acme/old": "*"}}),
            )
        })
        .collect();
    let f = indirect_fixture(versions);
    let why = failed(&f);
    assert!(
        why.contains("could not satisfy declared package conflicts"),
        "{why}"
    );
    assert!(why.contains("acme/new"), "{why}");
    assert!(why.contains("conflicts with acme/old 1.0.0"), "{why}");
}

#[test]
fn conflicts_on_platform_packages_are_ignored() {
    // `php`/`ext-*` conflicts are out of scope, as before: neither the
    // solver edge nor the post-solve check applies them.
    let f = fixture(
        &[(
            "acme/a",
            vec![entry(
                "acme/a",
                "1.0.0",
                json!({"conflict": {"php": "*", "ext-redis": "*"}}),
            )],
        )],
        &json!({"require": {"acme/a": "*", "php": "*"}}),
    );
    let s = solved(&f);
    assert_eq!(s["acme/a"], "1.0.0.0");
}

// --- The report formatter on its own ---

fn pkg(name: &str) -> PubGrubPackage {
    PubGrubPackage::Package(PackageName::from(name))
}

fn exact(v: &str) -> ComposerRange {
    ComposerRange::singleton(Version::parse(v).unwrap())
}

fn edges(mixed: bool, constraints: &[&str]) -> FxHashMap<(PackageName, PackageName), ConflictEdge> {
    let mut m = FxHashMap::default();
    m.insert(
        (PackageName::from("acme/new"), PackageName::from("acme/old")),
        ConflictEdge {
            constraints: constraints.iter().map(|c| (*c).to_string()).collect(),
            mixed,
        },
    );
    m
}

#[test]
fn formatter_words_pure_conflict_edges_as_conflicts() {
    let e = edges(false, &["<2.0"]);
    let f = ConflictAwareFormatter { edges: &e };
    let ext = External::FromDependencyOf(
        pkg("acme/new"),
        exact("2.0.0"),
        pkg("acme/old"),
        ComposerRange::higher_than(Version::parse("2.0").unwrap()),
    );
    assert_eq!(
        f.format_external(&ext),
        format!("acme/new {} conflicts with acme/old <2.0", exact("2.0.0"))
    );
}

#[test]
fn formatter_keeps_depends_on_for_mixed_and_unknown_edges() {
    let e = edges(true, &["<2.0"]);
    let f = ConflictAwareFormatter { edges: &e };
    let ext = External::FromDependencyOf(
        pkg("acme/new"),
        exact("2.0.0"),
        pkg("acme/old"),
        ComposerRange::full(),
    );
    assert!(f.format_external(&ext).contains("depends on acme/old"));

    let none = FxHashMap::default();
    let f = ConflictAwareFormatter { edges: &none };
    assert!(f.format_external(&ext).contains("depends on acme/old"));
}

#[test]
fn formatter_falls_back_to_the_range_when_constraints_differ() {
    // Different declarer versions spelled the conflict differently; the
    // excluded range is the honest summary.
    let e = edges(false, &["<2.0", "<2.1"]);
    let f = ConflictAwareFormatter { edges: &e };
    let allowed = ComposerRange::higher_than(Version::parse("2.0").unwrap());
    let ext = External::FromDependencyOf(
        pkg("acme/new"),
        ComposerRange::full(),
        pkg("acme/old"),
        allowed.clone(),
    );
    assert_eq!(
        f.format_external(&ext),
        format!("acme/new conflicts with acme/old {}", allowed.complement())
    );
}

#[test]
fn formatter_renders_excluded_versions_with_their_conflict() {
    let none = FxHashMap::default();
    let f = ConflictAwareFormatter { edges: &none };
    let ext = External::Custom(
        pkg("acme/new"),
        exact("2.0.0"),
        "conflicts with acme/old 1.0.0".to_string(),
    );
    assert_eq!(
        f.format_external(&ext),
        format!("acme/new {} conflicts with acme/old 1.0.0", exact("2.0.0"))
    );
}

#[test]
fn violation_reason_uses_the_pretty_target_version() {
    let v = ConflictViolation {
        declarer: PackageName::from("acme/new"),
        version: Version::parse("2.0.0").unwrap(),
        target: PackageName::from("acme/old"),
        target_version: Version::parse("1.0.0").unwrap(),
        target_pretty: "v1.0.0".into(),
        constraint: "*".into(),
    };
    assert_eq!(v.reason(), "conflicts with acme/old v1.0.0");
}
