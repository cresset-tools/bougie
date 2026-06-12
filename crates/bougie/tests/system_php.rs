//! End-to-end: `bougie sync --no-managed-php` selects a discovered
//! system PHP, writes the `resolved-php-path` marker, and the `php`
//! shim execs that interpreter.

#![cfg(unix)]

use assert_cmd::Command;
use std::os::unix::fs::PermissionsExt;
use tempfile::TempDir;

/// Write an executable `php` shell stub that answers `-v` / `-m` like a
/// real CLI and echoes a recognizable banner for any other invocation
/// (so `bougie run -- php …` can be asserted). PHP's version flag is
/// lowercase `-v` — uppercase `-V` is rejected by the real CLI.
fn write_php_stub(dir: &std::path::Path, version: &str, extra_args_marker: &str) {
    let php = dir.join("php");
    std::fs::write(
        &php,
        format!(
            "#!/bin/sh\n\
             case \"$1\" in\n\
               -v) echo 'PHP {version} (cli) (built: x) (NTS)';;\n\
               -m) printf '[PHP Modules]\\nCore\\ncurl\\njson\\n';;\n\
               *) echo '{extra_args_marker}';;\n\
             esac\n"
        ),
    )
    .unwrap();
    std::fs::set_permissions(&php, std::fs::Permissions::from_mode(0o755)).unwrap();
}

/// PATH with `extra` prepended to the inherited PATH.
fn path_with(extra: &std::path::Path) -> std::ffi::OsString {
    let base = std::env::var_os("PATH").unwrap_or_default();
    let mut joined = std::ffi::OsString::from(extra);
    joined.push(":");
    joined.push(base);
    joined
}

fn bougie(home: &TempDir, cache: &TempDir) -> Command {
    let mut cmd = Command::cargo_bin("bougie").expect("bougie binary");
    cmd.env("BOUGIE_HOME", home.path())
        .env("BOUGIE_CACHE", cache.path())
        .env_remove("RUST_LOG");
    cmd
}

#[test]
fn sync_no_managed_php_uses_system_interpreter() {
    let home = TempDir::new().unwrap();
    let cache = TempDir::new().unwrap();
    let proj = TempDir::new().unwrap();
    let stub_dir = TempDir::new().unwrap();

    // A distinctive version no real machine PHP will report, so
    // discovery can only satisfy the constraint via our stub.
    write_php_stub(stub_dir.path(), "8.3.99", "STUB-PHP-RAN");

    std::fs::write(
        proj.path().join("composer.json"),
        r#"{"name":"acme/blog","require":{"php":"8.3.99","ext-curl":"*"}}"#,
    )
    .unwrap();

    bougie(&home, &cache)
        .current_dir(proj.path())
        .env("PATH", path_with(stub_dir.path()))
        .args(["--verbose", "sync", "--no-managed-php", "--offline"])
        .assert()
        .success();

    // The system-PHP marker is written with the stub's absolute path…
    let marker = proj
        .path()
        .join(".bougie/state/resolved-php-path");
    let recorded = std::fs::read_to_string(&marker).expect("resolved-php-path written");
    let stub_php = std::fs::canonicalize(stub_dir.path().join("php")).unwrap();
    assert_eq!(recorded.trim(), stub_php.to_string_lossy());

    // …and the version/flavor marker matches the probed banner.
    let resolved =
        std::fs::read_to_string(proj.path().join(".bougie/state/resolved")).unwrap();
    assert_eq!(resolved.trim(), "8.3.99-nts");

    // The `php` shim execs the system interpreter.
    bougie(&home, &cache)
        .current_dir(proj.path())
        .env("PATH", path_with(stub_dir.path()))
        .args(["run", "--no-sync", "--", "php", "-r", "noop"])
        .assert()
        .success()
        .stdout(predicates::str::contains("STUB-PHP-RAN"));
}

#[test]
fn php_list_shows_discovered_system_php() {
    let home = TempDir::new().unwrap();
    let cache = TempDir::new().unwrap();
    let stub_dir = TempDir::new().unwrap();
    write_php_stub(stub_dir.path(), "8.3.99", "STUB-PHP-RAN");

    // `--only-installed` skips the network index fetch; system PHPs are
    // still surfaced alongside managed installs.
    bougie(&home, &cache)
        .env("PATH", path_with(stub_dir.path()))
        .args(["php", "list", "--only-installed"])
        .assert()
        .success()
        .stdout(predicates::str::contains("8.3.99"));
}

#[test]
fn ext_add_no_managed_php_errors_with_guidance() {
    let home = TempDir::new().unwrap();
    let cache = TempDir::new().unwrap();
    let proj = TempDir::new().unwrap();
    let stub_dir = TempDir::new().unwrap();
    write_php_stub(stub_dir.path(), "8.3.99", "STUB-PHP-RAN");

    std::fs::create_dir_all(proj.path().join(".bougie")).unwrap();
    std::fs::write(
        proj.path().join("composer.json"),
        r#"{"name":"acme/blog","require":{"php":"8.3.99"}}"#,
    )
    .unwrap();

    bougie(&home, &cache)
        .current_dir(proj.path())
        .env("PATH", path_with(stub_dir.path()))
        .args(["ext", "add", "redis", "--no-managed-php"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("managed PHP"));
}

#[test]
fn sync_no_managed_php_missing_extension_errors() {
    let home = TempDir::new().unwrap();
    let cache = TempDir::new().unwrap();
    let proj = TempDir::new().unwrap();
    let stub_dir = TempDir::new().unwrap();

    // Stub loads only curl/json — not redis.
    write_php_stub(stub_dir.path(), "8.3.99", "STUB-PHP-RAN");

    std::fs::write(
        proj.path().join("composer.json"),
        r#"{"name":"acme/blog","require":{"php":"8.3.99","ext-redis":"*"}}"#,
    )
    .unwrap();

    bougie(&home, &cache)
        .current_dir(proj.path())
        .env("PATH", path_with(stub_dir.path()))
        .args(["sync", "--no-managed-php", "--offline"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("ext-redis"));
}
