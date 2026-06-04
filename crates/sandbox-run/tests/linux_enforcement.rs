//! Landlock enforcement tests (regression coverage for #208).
//!
//! These spawn a real `cat` / `sh` under a policy and assert the kernel
//! actually denies or allows the access — the only way to prove the
//! allow-list carving in `platform::linux` works, since a buggy ruleset
//! compiles and "runs" just fine while enforcing nothing.
//!
//! Every test is gated on [`sandbox_run::landlock_available`]: on a
//! kernel without Landlock (< 5.13 or disabled) `apply_sandbox` fails
//! open, so the assertions below couldn't hold — we skip rather than
//! flake. This mirrors production, where the daemon warns and proceeds
//! unconfined instead of refusing to launch.
#![cfg(target_os = "linux")]

use std::path::Path;
use std::process::Stdio;

use sandbox_run::{Command, ProtectHome, ProtectSystem, Sandbox, landlock_available};

/// Read `path` inside `sandbox` via `cat`; returns whether it succeeded.
fn can_read(path: &Path, sandbox: sandbox_run::SandboxPolicy) -> bool {
    Command::new("cat")
        .arg(path)
        .sandbox(sandbox)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("spawn cat")
        .success()
}

/// Write to `path` inside `sandbox` via `sh -c`; returns whether it succeeded.
fn can_write(path: &Path, sandbox: sandbox_run::SandboxPolicy) -> bool {
    Command::new("sh")
        .arg("-c")
        .arg(format!("echo x > {}", path.display()))
        .sandbox(sandbox)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("spawn sh")
        .success()
}

#[test]
fn inaccessible_paths_denies_reads() {
    if !landlock_available() {
        eprintln!("skipping: Landlock not enforced on this kernel");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let secret = dir.path().join("secret.txt");
    std::fs::write(&secret, "topsecret").unwrap();

    // Permissive base, single denied subtree.
    let policy = Sandbox::new()
        .protect_system(ProtectSystem::No)
        .inaccessible_paths([dir.path()])
        .no_new_privileges(true)
        .build()
        .unwrap();
    assert!(!can_read(&secret, policy), "inaccessible_paths must deny the read");

    // Control: the same file is readable with no sandbox restrictions.
    let open = Sandbox::new()
        .protect_system(ProtectSystem::No)
        .read_write_paths([dir.path()])
        .no_new_privileges(true)
        .build()
        .unwrap();
    assert!(can_read(&secret, open), "sanity: file is readable when granted");
}

/// The daemon's exact shape: a sensitive tree is carved out, but the
/// service's own data dir lives *under* it and is carved back in. Proves
/// (a) the carve-out denies siblings and (b) the nested carve-in is
/// reachable despite its parent being denied (Landlock allows traversing
/// a non-granted directory to reach a granted path beneath it).
#[test]
fn carve_in_under_denied_subtree_is_reachable() {
    if !landlock_available() {
        eprintln!("skipping: Landlock not enforced on this kernel");
        return;
    }
    let home = tempfile::tempdir().unwrap();
    let data = home.path().join("service/data");
    std::fs::create_dir_all(&data).unwrap();
    let allowed = data.join("ok.txt");
    std::fs::write(&allowed, "x").unwrap();
    let secret = home.path().join("secret.txt");
    std::fs::write(&secret, "topsecret").unwrap();

    let policy = Sandbox::new()
        .protect_system(ProtectSystem::No)
        .inaccessible_paths([home.path()])
        .read_write_paths([data.as_path()])
        .no_new_privileges(true)
        .build()
        .unwrap();

    // The carve-in is readable and writable...
    let again = Sandbox::new()
        .protect_system(ProtectSystem::No)
        .inaccessible_paths([home.path()])
        .read_write_paths([data.as_path()])
        .no_new_privileges(true)
        .build()
        .unwrap();
    assert!(can_read(&allowed, policy), "carved-in data dir must stay readable");
    assert!(
        can_write(&allowed, again),
        "carved-in data dir must stay writable"
    );

    // ...while a sibling under the denied tree is not.
    let policy2 = Sandbox::new()
        .protect_system(ProtectSystem::No)
        .inaccessible_paths([home.path()])
        .read_write_paths([data.as_path()])
        .no_new_privileges(true)
        .build()
        .unwrap();
    assert!(
        !can_read(&secret, policy2),
        "sibling under the denied tree must stay hidden"
    );
}

/// A read-only carve-in under a denied tree is readable but not writable.
#[test]
fn read_only_paths_deny_writes() {
    if !landlock_available() {
        eprintln!("skipping: Landlock not enforced on this kernel");
        return;
    }
    let home = tempfile::tempdir().unwrap();
    let ro = home.path().join("store");
    std::fs::create_dir_all(&ro).unwrap();
    let file = ro.join("index.json");
    std::fs::write(&file, "{}").unwrap();

    let read_policy = Sandbox::new()
        .protect_system(ProtectSystem::No)
        .inaccessible_paths([home.path()])
        .read_only_paths([ro.as_path()])
        .no_new_privileges(true)
        .build()
        .unwrap();
    assert!(can_read(&file, read_policy), "read_only_paths must allow reads");

    let write_policy = Sandbox::new()
        .protect_system(ProtectSystem::No)
        .inaccessible_paths([home.path()])
        .read_only_paths([ro.as_path()])
        .no_new_privileges(true)
        .build()
        .unwrap();
    assert!(
        !can_write(&file, write_policy),
        "read_only_paths must deny writes"
    );
}

/// Headline #208 fix: `ProtectHome::Yes` under `ProtectSystem::Strict`
/// hides `$HOME` content (e.g. `~/.ssh`) instead of silently no-opping.
#[test]
fn protect_home_yes_denies_home_content() {
    if !landlock_available() {
        eprintln!("skipping: Landlock not enforced on this kernel");
        return;
    }
    let Some(home) = std::env::var_os("HOME").map(std::path::PathBuf::from) else {
        eprintln!("skipping: HOME not set");
        return;
    };
    if !home.starts_with("/home") && !home.starts_with("/root") {
        // ProtectHome=yes denies the standard /home, /root, /run/user
        // trees; if the test user's HOME is elsewhere there's nothing to
        // assert against.
        eprintln!("skipping: HOME ({}) is outside the protected trees", home.display());
        return;
    }
    let secret = home.join(format!(".sandbox_run_test_secret_{}", std::process::id()));
    std::fs::write(&secret, "topsecret").unwrap();

    let policy = Sandbox::new()
        .protect_system(ProtectSystem::Strict)
        .protect_home(ProtectHome::Yes)
        .no_new_privileges(true)
        .build()
        .unwrap();
    let denied = !can_read(&secret, policy);
    let _ = std::fs::remove_file(&secret);
    assert!(denied, "ProtectHome::Yes must hide files under $HOME");
}
