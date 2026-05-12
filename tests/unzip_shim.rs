//! End-to-end test for the `unzip` argv[0] role.
//!
//! Builds a fixture zip containing a regular file, an executable, and a
//! symlink; invokes the bougie binary via a symlink named `unzip`; then
//! asserts the extraction matches Composer's expectations: file tree
//! present, mode bits preserved, symlink restored as a symlink (not a
//! file containing the target path).

use std::io::Write;
use std::os::unix::fs::{symlink, PermissionsExt};
use std::path::PathBuf;
use std::process::Command;
use tempfile::TempDir;
use zip::write::SimpleFileOptions;

/// Path to the freshly-built `bougie` binary, courtesy of assert_cmd.
fn bougie_bin() -> PathBuf {
    assert_cmd::cargo::cargo_bin("bougie")
}

/// Create a symlink named `unzip` next to a per-test temp dir that
/// points at the real bougie binary. Invoking through it makes argv[0]
/// resolve to `unzip` so `role_from_argv0` picks the unzip role.
fn unzip_shim(td: &TempDir) -> PathBuf {
    let link = td.path().join("unzip");
    symlink(bougie_bin(), &link).expect("symlinking unzip -> bougie");
    link
}

/// Build a minimal fixture archive in `tmp/fixture.zip`. Contents:
///   - `hello.txt` (mode 0o644, "hello\n")
///   - `bin/script.sh` (mode 0o755, "#!/bin/sh\necho hi\n")
///   - `link.txt` -> `hello.txt` (symlink)
fn build_fixture(tmp: &std::path::Path) -> PathBuf {
    use zip::CompressionMethod;

    let zip_path = tmp.join("fixture.zip");
    let f = std::fs::File::create(&zip_path).expect("create fixture zip");
    let mut z = zip::ZipWriter::new(f);

    let regular = SimpleFileOptions::default()
        .compression_method(CompressionMethod::Deflated)
        .unix_permissions(0o644);
    z.start_file("hello.txt", regular).unwrap();
    z.write_all(b"hello\n").unwrap();

    let executable = SimpleFileOptions::default()
        .compression_method(CompressionMethod::Deflated)
        .unix_permissions(0o755);
    z.start_file("bin/script.sh", executable).unwrap();
    z.write_all(b"#!/bin/sh\necho hi\n").unwrap();

    // The zip crate's `add_symlink` sets the S_IFLNK bit and writes the
    // target as the file body; `ZipArchive::extract` then reproduces it
    // as a real symlink on disk.
    let symlink_opts = SimpleFileOptions::default().compression_method(CompressionMethod::Stored);
    z.add_symlink("link.txt", "hello.txt", symlink_opts).unwrap();

    z.finish().expect("finalising fixture zip");
    zip_path
}

#[test]
fn unzip_extracts_composer_invocation_with_modes_and_symlink() {
    let td = TempDir::new().unwrap();
    let zip_path = build_fixture(td.path());
    let dest = td.path().join("out");
    let shim = unzip_shim(&td);

    // The exact invocation Composer's ZipDownloader uses:
    //   unzip -qq <file> -d <dir>
    let status = Command::new(&shim)
        .arg("-qq")
        .arg(&zip_path)
        .arg("-d")
        .arg(&dest)
        .status()
        .expect("spawning unzip shim");
    assert!(status.success(), "unzip shim exited non-zero: {status:?}");

    // Regular file present with expected contents and mode.
    let hello = dest.join("hello.txt");
    let body = std::fs::read_to_string(&hello).expect("hello.txt should exist");
    assert_eq!(body, "hello\n");
    let mode = std::fs::metadata(&hello).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o644, "hello.txt mode bits should be preserved");

    // Executable bit survives the round-trip — this is the load-bearing
    // property for Composer's `bin/` entries (vendor/bin/phpunit etc.).
    let script = dest.join("bin").join("script.sh");
    let smode = std::fs::metadata(&script).unwrap().permissions().mode() & 0o777;
    assert_eq!(smode, 0o755, "bin/script.sh should be executable");

    // Symlink restored as a real symlink (not a file containing the path).
    let link = dest.join("link.txt");
    let meta = std::fs::symlink_metadata(&link).expect("link.txt should exist");
    assert!(
        meta.file_type().is_symlink(),
        "link.txt should be a symlink, got {meta:?}"
    );
    let target = std::fs::read_link(&link).unwrap();
    assert_eq!(target, std::path::PathBuf::from("hello.txt"));
}

#[test]
fn unzip_writes_trace_when_requested() {
    let td = TempDir::new().unwrap();
    let zip_path = build_fixture(td.path());
    let dest = td.path().join("out");
    let tracefile = td.path().join("trace.log");
    let shim = unzip_shim(&td);

    let status = Command::new(&shim)
        .env("BOUGIE_TRACE_UNZIP", &tracefile)
        .arg("-qq")
        .arg(&zip_path)
        .arg("-d")
        .arg(&dest)
        .status()
        .expect("spawning unzip shim");
    assert!(status.success());

    let trace = std::fs::read_to_string(&tracefile).expect("trace file should exist");
    assert!(trace.starts_with("extract "), "trace line: {trace:?}");
    assert!(trace.contains("fixture.zip"));
}

#[test]
fn unzip_rejects_unknown_flag() {
    let td = TempDir::new().unwrap();
    let shim = unzip_shim(&td);

    let out = Command::new(&shim)
        .arg("--not-a-flag")
        .arg("anything.zip")
        .output()
        .expect("spawning unzip shim");
    assert!(!out.status.success(), "expected nonzero exit on bad flag");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("unsupported flag"),
        "stderr was: {stderr}"
    );
}
