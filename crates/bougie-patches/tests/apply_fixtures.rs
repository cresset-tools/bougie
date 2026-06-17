//! Corpus-driven apply tests.
//!
//! Each directory under `tests/fixtures/<case>/` is one case:
//!
//! ```text
//! patch.diff          the patch to apply
//! before/             the pristine input tree (may be absent for create-only)
//! after/              the expected tree after applying
//! depth               optional: an explicit `-pN` level (one integer); else Auto
//! crosscheck          optional marker (any content): also apply with GNU `patch`
//!                     at the depth bougie chose and assert it yields `after/`,
//!                     validating fuzzy fidelity against the reference tool.
//! ```
//!
//! The runner copies `before/` into a temp dir, applies the patch, and asserts
//! the result equals `after/` byte-for-byte.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use bougie_patches::model::DepthSpec;
use bougie_patches::{ApplyOptions, apply_patch_text};

#[test]
fn apply_corpus() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let mut cases: Vec<PathBuf> = fs::read_dir(&root)
        .expect("fixtures dir")
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.is_dir())
        .collect();
    cases.sort();
    assert!(!cases.is_empty(), "no fixtures found in {}", root.display());

    for case in cases {
        run_case(&case);
    }
}

fn run_case(case: &Path) {
    let name = case.file_name().unwrap().to_string_lossy().into_owned();
    let patch = fs::read_to_string(case.join("patch.diff"))
        .unwrap_or_else(|e| panic!("[{name}] reading patch.diff: {e}"));

    let depth = match fs::read_to_string(case.join("depth")) {
        Ok(s) => DepthSpec::Fixed(s.trim().parse().expect("depth must be an integer")),
        Err(_) => DepthSpec::Auto,
    };

    // Stage the `before/` tree in a temp dir.
    let work = tempfile::tempdir().unwrap();
    let before = case.join("before");
    if before.exists() {
        copy_tree(&before, work.path());
    }

    let opts = ApplyOptions {
        depth,
        ..ApplyOptions::default()
    };
    let report = apply_patch_text(work.path(), &patch, &opts)
        .unwrap_or_else(|e| panic!("[{name}] apply failed: {e:?}"));

    let expected = read_tree(&case.join("after"));
    let actual = read_tree(work.path());
    assert_eq!(actual, expected, "[{name}] applied tree mismatch");

    if case.join("crosscheck").exists() {
        crosscheck_gnu_patch(&name, case, &patch, report.depth_used, &expected);
    }
}

/// Apply the same patch with GNU `patch -pN` and assert it matches `after/`.
///
/// Skipped unless GNU `patch` is on PATH: this is a fidelity *cross-check*
/// against the reference implementation, and BSD `patch` (the default on
/// macOS) diverges on file create/delete and fuzz handling — bougie's own
/// apply is already asserted against `after/` in [`run_case`], which runs
/// everywhere.
fn crosscheck_gnu_patch(
    name: &str,
    case: &Path,
    patch: &str,
    depth: usize,
    expected: &BTreeMap<String, Vec<u8>>,
) {
    if !is_gnu_patch() {
        eprintln!("[{name}] skipping GNU-patch cross-check: GNU `patch` not found");
        return;
    }
    let work = tempfile::tempdir().unwrap();
    let before = case.join("before");
    if before.exists() {
        copy_tree(&before, work.path());
    }
    let status = Command::new("patch")
        .current_dir(work.path())
        .arg(format!("-p{depth}"))
        .arg("--no-backup-if-mismatch")
        .arg("-i")
        .arg("-") // patch from stdin
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .and_then(|mut child| {
            use std::io::Write;
            child
                .stdin
                .take()
                .unwrap()
                .write_all(patch.as_bytes())
                .unwrap();
            child.wait()
        })
        .unwrap_or_else(|e| panic!("[{name}] spawning GNU patch: {e}"));
    assert!(status.success(), "[{name}] GNU patch -p{depth} failed");
    let gnu = read_tree(work.path());
    assert_eq!(
        &gnu, expected,
        "[{name}] GNU patch produced a tree differing from after/"
    );
}

/// Whether the `patch` on PATH is GNU patch (BSD patch, e.g. on macOS,
/// diverges on create/delete and fuzz, so the cross-check is GNU-only).
fn is_gnu_patch() -> bool {
    Command::new("patch")
        .arg("--version")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).contains("GNU"))
        .unwrap_or(false)
}

fn copy_tree(from: &Path, to: &Path) {
    for entry in walk(from) {
        let rel = entry.strip_prefix(from).unwrap();
        let dst = to.join(rel);
        fs::create_dir_all(dst.parent().unwrap()).unwrap();
        fs::copy(&entry, &dst).unwrap();
    }
}

/// Read every file under `root` into a `rel-path → bytes` map (ignores the
/// patcher's transient temp files).
fn read_tree(root: &Path) -> BTreeMap<String, Vec<u8>> {
    let mut map = BTreeMap::new();
    if !root.exists() {
        return map;
    }
    for entry in walk(root) {
        let rel = entry
            .strip_prefix(root)
            .unwrap()
            .to_string_lossy()
            .replace('\\', "/");
        if rel.contains(".bougie-patch-") {
            continue;
        }
        map.insert(rel, fs::read(&entry).unwrap());
    }
    map
}

fn walk(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                stack.push(path);
            } else {
                out.push(path);
            }
        }
    }
    out.sort();
    out
}
