//! End-to-end tests: installing a new lock removes packages that left it.
//!
//! The bug: an autoloader dump between `bougie lock` and the next sync —
//! `bougie server` reacting to the lock change, or `bougie composer
//! dump-autoload` — rewrote `installed.json` from the new lock. Sync
//! decides what to remove from that file, so the old package's directory
//! stayed in `vendor/`.
//!
//! These use `bougie composer update --no-install` (the lock step) and
//! `bougie composer install` (the install step `bougie sync` runs), which
//! need no PHP toolchain. Every package comes from path repositories.

use std::path::PathBuf;
use tempfile::TempDir;
use wiremock::MockServer;

mod common;
use common::TestEnv;

struct Project {
    _tmp: TempDir,
    root: PathBuf,
    app: PathBuf,
    packagist: MockServer,
    _rt: tokio::runtime::Runtime,
}

/// `pkgs/<dir>` path packages plus an empty Packagist stand-in so
/// nothing reaches the network.
fn project(packages: &[(&str, &str)]) -> Project {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().to_path_buf();
    for (dir, composer_json) in packages {
        let d = root.join("pkgs").join(dir);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("composer.json"), composer_json).unwrap();
    }
    let app = root.join("app");
    std::fs::create_dir_all(&app).unwrap();
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let packagist = rt.block_on(MockServer::start());
    Project {
        _tmp: tmp,
        root,
        app,
        packagist,
        _rt: rt,
    }
}

impl Project {
    /// Point `app/composer.json` at `require`, with `symlink` as given.
    fn require(&self, require: &str, symlink: bool, extra: &str) {
        std::fs::write(
            self.app.join("composer.json"),
            format!(
                r#"{{
                    "name": "test/app",
                    "repositories": [{{"type": "path", "url": "../pkgs/*",
                                      "options": {{"symlink": {symlink}}}}}],
                    "require": {require}
                    {extra}
                }}"#
            ),
        )
        .unwrap();
    }

    fn bougie(&self, env: &TestEnv, args: &[&str]) -> String {
        let out = env
            .bougie()
            .env("BOUGIE_PACKAGIST_BASE_URL", self.packagist.uri())
            .args(["composer"])
            .args(args)
            .arg("-d")
            .arg(&self.app)
            .output()
            .unwrap();
        let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
        let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
        assert!(
            out.status.success(),
            "bougie composer {args:?} failed:\n{stderr}"
        );
        format!("{stdout}{stderr}")
    }

    /// Lock, then install — what `bougie lock && bougie sync` does.
    fn lock_then_install(&self, env: &TestEnv) -> String {
        self.bougie(env, &["update", "--no-install"]);
        self.bougie(env, &["install"])
    }

    fn exists(&self, rel: &str) -> bool {
        std::fs::symlink_metadata(self.app.join(rel)).is_ok()
    }

    fn installed_json_names(&self) -> Vec<String> {
        let v: serde_json::Value = serde_json::from_slice(
            &std::fs::read(self.app.join("vendor/composer/installed.json")).unwrap(),
        )
        .unwrap();
        v["packages"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| p["name"].as_str().unwrap().to_string())
            .collect()
    }
}

/// The report's packages: a renamed module that conflicts with its old name.
fn rename_project() -> Project {
    project(&[
        (
            "old",
            r#"{"name": "acme/old-thing", "version": "1.0.0", "type": "magento2-module"}"#,
        ),
        (
            "new",
            r#"{"name": "acme/new-thing", "version": "2.0.0", "type": "magento2-module",
                "conflict": {"acme/old-thing": "*"}}"#,
        ),
    ])
}

fn switch_with_dump_between(symlink: bool) {
    let env = TestEnv::new();
    let p = rename_project();
    p.require(r#"{"acme/old-thing": "*"}"#, symlink, "");
    p.lock_then_install(&env);
    assert!(p.exists("vendor/acme/old-thing"));

    p.require(r#"{"acme/new-thing": "*"}"#, symlink, "");
    p.bougie(&env, &["update", "--no-install"]);
    // What `bougie server` does on the lock change.
    p.bougie(&env, &["dump-autoload"]);
    assert_eq!(
        p.installed_json_names(),
        ["acme/new-thing"],
        "precondition: the dump already dropped the old package from installed.json"
    );
    let out = p.bougie(&env, &["install"]);

    assert!(p.exists("vendor/acme/new-thing"), "{out}");
    assert!(
        !p.exists("vendor/acme/old-thing"),
        "the old package must be removed even though installed.json forgot it:\n{out}"
    );
    assert!(
        p.root.join("pkgs/old/composer.json").is_file(),
        "source untouched"
    );
}

#[test]
fn removes_the_old_package_after_a_dump_between_lock_and_install() {
    switch_with_dump_between(false);
}

#[test]
fn removes_the_old_symlinked_package_after_a_dump_between_lock_and_install() {
    switch_with_dump_between(true);
}

#[test]
fn removes_the_old_package_switching_back_and_forth() {
    // The report's steps 2 and 3: switch one way, then back, with a dump
    // before each install.
    let env = TestEnv::new();
    let p = rename_project();
    for (want, gone) in [
        ("acme/old-thing", "acme/new-thing"),
        ("acme/new-thing", "acme/old-thing"),
        ("acme/old-thing", "acme/new-thing"),
    ] {
        p.require(&format!(r#"{{"{want}": "*"}}"#), false, "");
        p.bougie(&env, &["update", "--no-install"]);
        p.bougie(&env, &["dump-autoload"]);
        p.bougie(&env, &["install"]);
        assert!(p.exists(&format!("vendor/{want}")), "{want} installed");
        assert!(!p.exists(&format!("vendor/{gone}")), "{gone} removed");
    }
}

#[test]
fn plain_lock_then_install_still_removes_and_reports_it() {
    let env = TestEnv::new();
    let p = rename_project();
    p.require(r#"{"acme/old-thing": "*"}"#, false, "");
    p.lock_then_install(&env);
    p.require(r#"{"acme/new-thing": "*"}"#, false, "");
    let out = p.lock_then_install(&env);
    assert!(!p.exists("vendor/acme/old-thing"), "{out}");
    assert!(out.contains("1 removed"), "{out}");
}

#[test]
fn install_writes_the_install_record() {
    let env = TestEnv::new();
    let p = rename_project();
    p.require(r#"{"acme/old-thing": "*"}"#, false, "");
    p.lock_then_install(&env);
    let record: serde_json::Value = serde_json::from_slice(
        &std::fs::read(p.app.join("vendor/bougie/state/installed-packages.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(
        record["packages"]["acme/old-thing"], "vendor/acme/old-thing",
        "{record}"
    );
}

#[test]
fn removes_a_package_relocated_by_installer_paths() {
    // The related bug: removal always deleted vendor/<name>, so a package
    // that installer-paths moved elsewhere stayed on disk.
    let env = TestEnv::new();
    let p = project(&[
        (
            "plugin",
            r#"{"name": "acme/plugin", "version": "1.0.0", "type": "library"}"#,
        ),
        ("lib", r#"{"name": "acme/lib", "version": "1.0.0"}"#),
    ]);
    let extra = r#","extra": {"installer-paths": {"plugins/{$name}/": ["acme/plugin"]}}"#;
    p.require(r#"{"acme/plugin": "*", "acme/lib": "*"}"#, false, extra);
    p.lock_then_install(&env);
    assert!(p.exists("plugins/plugin"), "precondition: relocated");
    assert!(!p.exists("vendor/acme/plugin"));

    p.require(r#"{"acme/lib": "*"}"#, false, extra);
    let out = p.lock_then_install(&env);
    assert!(
        !p.exists("plugins/plugin"),
        "relocated package removed:\n{out}"
    );
    assert!(p.exists("vendor/acme/lib"));
    assert!(p.exists("plugins"), "parent outside vendor/ left alone");
}

#[test]
fn leaves_directories_bougie_did_not_install() {
    let env = TestEnv::new();
    let p = rename_project();
    p.require(r#"{"acme/old-thing": "*"}"#, false, "");
    p.lock_then_install(&env);
    let manual = p.app.join("vendor/someone/manual-clone");
    std::fs::create_dir_all(&manual).unwrap();
    std::fs::write(
        manual.join("composer.json"),
        r#"{"name":"someone/manual-clone"}"#,
    )
    .unwrap();

    p.require(r#"{"acme/new-thing": "*"}"#, false, "");
    p.lock_then_install(&env);
    assert!(manual.join("composer.json").is_file());
    assert!(!p.exists("vendor/acme/old-thing"));
}
