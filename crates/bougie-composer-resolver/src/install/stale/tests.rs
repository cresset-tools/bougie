//! Tests for removing packages that left the lock.

use super::*;
use serde_json::json;
use tempfile::TempDir;

fn lock_pkg(name: &str, ty: &str) -> LockPackage {
    serde_json::from_value(json!({"name": name, "version": "1.0.0", "type": ty})).unwrap()
}

/// Write `vendor/composer/installed.json` listing `(name, install-path)`.
fn write_installed_json(root: &Path, entries: &[(&str, Option<&str>)]) {
    let packages: Vec<Value> = entries
        .iter()
        .map(|(name, ip)| match ip {
            Some(ip) => json!({"name": name, "install-path": ip}),
            None => json!({"name": name}),
        })
        .collect();
    let dir = root.join("vendor/composer");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("installed.json"),
        json!({"packages": packages}).to_string(),
    )
    .unwrap();
}

/// Create a package directory with a marker file.
fn make_pkg_dir(root: &Path, rel: &str) -> PathBuf {
    let dir = root.join(rel);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("composer.json"), "{}").unwrap();
    dir
}

fn record(entries: &[(&str, &str)]) -> InstallRecord {
    InstallRecord {
        version: RECORD_VERSION,
        packages: entries
            .iter()
            .map(|(n, p)| ((*n).to_string(), (*p).to_string()))
            .collect(),
    }
}

// --- The record ---

#[test]
fn record_round_trips() {
    let tmp = TempDir::new().unwrap();
    let r = record(&[("acme/foo", "vendor/acme/foo")]);
    r.write(tmp.path()).unwrap();
    assert_eq!(InstallRecord::read(tmp.path()), Some(r));
    assert!(
        tmp.path()
            .join("vendor/bougie/state/installed-packages.json")
            .is_file()
    );
    assert!(
        !tmp.path()
            .join("vendor/bougie/state/installed-packages.json.tmp")
            .exists(),
        "the temp file is renamed into place"
    );
}

#[test]
fn missing_corrupt_or_foreign_record_reads_as_none() {
    let tmp = TempDir::new().unwrap();
    assert_eq!(InstallRecord::read(tmp.path()), None);

    let path = bougie_paths::project::installed_packages(tmp.path());
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, "not json").unwrap();
    assert_eq!(InstallRecord::read(tmp.path()), None);

    std::fs::write(
        &path,
        r#"{"version": 99, "packages": {"a/b": "vendor/a/b"}}"#,
    )
    .unwrap();
    assert_eq!(InstallRecord::read(tmp.path()), None);
}

#[test]
fn record_uses_installer_paths_and_skips_metapackages() {
    let pkgs = [
        lock_pkg("acme/lib", "library"),
        lock_pkg("acme/wp", "wordpress-plugin"),
        lock_pkg("acme/custom", "library"),
        lock_pkg("acme/meta", "metapackage"),
    ];
    let refs: Vec<&LockPackage> = pkgs.iter().collect();
    let ip = InstallerPaths::parse(&json!({
        "extra": {"installer-paths": {"custom/{$name}/": ["acme/custom"]}}
    }));
    let r = InstallRecord::for_packages(&refs, &ip);
    assert_eq!(
        r.packages,
        BTreeMap::from([
            ("acme/custom".to_string(), "custom/custom".to_string()),
            ("acme/lib".to_string(), "vendor/acme/lib".to_string()),
            ("acme/wp".to_string(), "wp-content/plugins/wp".to_string()),
        ])
    );
}

#[test]
fn normalize_relative_rejects_escapes() {
    assert_eq!(
        normalize_relative("vendor/composer/../acme/foo").as_deref(),
        Some("vendor/acme/foo")
    );
    assert_eq!(
        normalize_relative("vendor/composer/../../app/code/X").as_deref(),
        Some("app/code/X")
    );
    assert_eq!(
        normalize_relative("vendor/composer/./semver").as_deref(),
        Some("vendor/composer/semver")
    );
    assert_eq!(
        normalize_relative("vendor/composer/../../..").as_deref(),
        None
    );
    assert_eq!(normalize_relative("/etc").as_deref(), None);
    assert_eq!(
        normalize_relative("vendor/composer/../..").as_deref(),
        Some("")
    );
}

// --- The sweep ---

#[test]
fn removes_a_package_listed_only_in_installed_json() {
    // No record yet (first install after upgrading bougie): the old
    // behavior, driven by installed.json, still works.
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    let old = make_pkg_dir(root, "vendor/acme/old");
    let keep = make_pkg_dir(root, "vendor/acme/keep");
    write_installed_json(
        root,
        &[
            ("acme/old", Some("../acme/old")),
            ("acme/keep", Some("../acme/keep")),
        ],
    );

    let wanted = record(&[("acme/keep", "vendor/acme/keep")]);
    assert_eq!(remove_stale_packages(root, &wanted), 1);
    assert!(!old.exists());
    assert!(keep.exists());
}

#[test]
fn removes_a_package_installed_json_forgot() {
    // The bug: an autoloader dump (bougie server, dump-autoload) rewrote
    // installed.json from the new lock, so it no longer lists acme/old.
    // The record still does.
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    let old = make_pkg_dir(root, "vendor/acme/old");
    make_pkg_dir(root, "vendor/acme/new");
    record(&[("acme/old", "vendor/acme/old")])
        .write(root)
        .unwrap();
    write_installed_json(root, &[("acme/new", Some("../acme/new"))]);

    let wanted = record(&[("acme/new", "vendor/acme/new")]);
    assert_eq!(remove_stale_packages(root, &wanted), 1);
    assert!(!old.exists(), "the forgotten package must still be removed");
    assert!(root.join("vendor/acme/new").exists());
}

#[test]
fn removes_a_relocated_package_at_its_recorded_path() {
    // installer-paths moved acme/plugin out of vendor/. The sweep used to
    // try vendor/acme/plugin and leave the real directory behind.
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    let plugin = make_pkg_dir(root, "wp-content/plugins/plugin");
    record(&[("acme/plugin", "wp-content/plugins/plugin")])
        .write(root)
        .unwrap();

    assert_eq!(remove_stale_packages(root, &record(&[])), 1);
    assert!(!plugin.exists());
    assert!(
        root.join("wp-content/plugins").is_dir(),
        "parents outside vendor/ are left alone"
    );
}

#[test]
fn removes_a_relocated_package_from_installed_json_install_path() {
    // Same, with only installed.json to go on: its install-path is
    // relative to vendor/composer.
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    let module = make_pkg_dir(root, "app/code/Acme/Module");
    write_installed_json(root, &[("acme/module", Some("../../app/code/Acme/Module"))]);

    assert_eq!(remove_stale_packages(root, &record(&[])), 1);
    assert!(!module.exists());
}

#[test]
fn record_path_wins_over_installed_json() {
    // installed.json was regenerated with a different (current) path;
    // the record knows where the package actually went.
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    let real = make_pkg_dir(root, "custom/old");
    record(&[("acme/old", "custom/old")]).write(root).unwrap();
    write_installed_json(root, &[("acme/old", Some("../acme/old"))]);

    assert_eq!(remove_stale_packages(root, &record(&[])), 1);
    assert!(!real.exists());
}

#[test]
fn falls_back_to_vendor_name_without_an_install_path() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    let old = make_pkg_dir(root, "vendor/acme/old");
    write_installed_json(root, &[("acme/old", None)]);

    assert_eq!(remove_stale_packages(root, &record(&[])), 1);
    assert!(!old.exists());
}

#[test]
fn keeps_wanted_packages_and_shared_paths() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    let shared = make_pkg_dir(root, "vendor/acme/shared");
    let nested_parent = make_pkg_dir(root, "custom/parent");
    record(&[
        ("acme/wanted", "vendor/acme/wanted"),
        // Left the lock, but the new package lives at the same path.
        ("acme/renamed-from", "vendor/acme/shared"),
        // Left the lock, but its path is an ancestor of a wanted path.
        ("acme/outer", "custom"),
    ])
    .write(root)
    .unwrap();
    make_pkg_dir(root, "vendor/acme/wanted");

    let wanted = record(&[
        ("acme/wanted", "vendor/acme/wanted"),
        ("acme/renamed-to", "vendor/acme/shared"),
        ("acme/inner", "custom/parent"),
    ]);
    assert_eq!(remove_stale_packages(root, &wanted), 0);
    assert!(shared.exists());
    assert!(nested_parent.exists());
    assert!(root.join("vendor/acme/wanted").exists());
}

#[test]
fn never_removes_protected_or_escaping_paths() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("project");
    std::fs::create_dir_all(&root).unwrap();
    let outside = make_pkg_dir(tmp.path(), "outside");
    make_pkg_dir(&root, "vendor/acme/keep");
    make_pkg_dir(&root, "vendor/bin");
    record(&[
        ("evil/root", ""),
        ("evil/vendor", "vendor"),
        ("evil/composer", "vendor/composer"),
        ("evil/bin", "vendor/bin"),
        ("evil/bougie", "vendor/bougie"),
        ("evil/escape", "../outside"),
    ])
    .write(&root)
    .unwrap();
    write_installed_json(&root, &[("evil/escape2", Some("../../../outside"))]);

    assert_eq!(remove_stale_packages(&root, &record(&[])), 0);
    assert!(outside.exists());
    assert!(root.join("vendor/acme/keep").exists());
    assert!(root.join("vendor/bin").exists());
    assert!(root.join("vendor/composer/installed.json").exists());
    assert!(bougie_paths::project::installed_packages(&root).exists());
}

#[test]
fn prunes_an_emptied_vendor_namespace_only() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    make_pkg_dir(root, "vendor/gone/pkg");
    make_pkg_dir(root, "vendor/acme/old");
    make_pkg_dir(root, "vendor/acme/other");
    record(&[
        ("gone/pkg", "vendor/gone/pkg"),
        ("acme/old", "vendor/acme/old"),
    ])
    .write(root)
    .unwrap();

    let wanted = record(&[("acme/other", "vendor/acme/other")]);
    assert_eq!(remove_stale_packages(root, &wanted), 2);
    assert!(!root.join("vendor/gone").exists(), "empty namespace pruned");
    assert!(root.join("vendor/acme/other").exists(), "non-empty kept");
    assert!(root.join("vendor").is_dir());
}

#[test]
fn missing_directories_are_not_counted() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    record(&[("acme/old", "vendor/acme/old")])
        .write(root)
        .unwrap();
    assert_eq!(remove_stale_packages(root, &record(&[])), 0);
}

#[test]
fn first_install_removes_nothing() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    let stray = make_pkg_dir(root, "vendor/someone/manual-clone");
    // No record, no installed.json: bougie never installed anything here,
    // so it must not touch what it finds.
    assert_eq!(remove_stale_packages(root, &record(&[])), 0);
    assert!(stray.exists());
}

#[cfg(unix)]
#[test]
fn removes_a_symlinked_path_package_without_touching_its_source() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("project");
    let source = make_pkg_dir(tmp.path(), "pkgs/old");
    std::fs::create_dir_all(root.join("vendor/acme")).unwrap();
    std::os::unix::fs::symlink(&source, root.join("vendor/acme/old")).unwrap();
    record(&[("acme/old", "vendor/acme/old")])
        .write(&root)
        .unwrap();

    assert_eq!(remove_stale_packages(&root, &record(&[])), 1);
    assert!(
        std::fs::symlink_metadata(root.join("vendor/acme/old")).is_err(),
        "link removed"
    );
    assert!(source.join("composer.json").is_file(), "source untouched");
}
