//! Phase 1 integration tests: spawn a real `bougie server` against an
//! ephemeral port, exercise the static-file path, traversal guards,
//! unknown-host routing, and clean SIGINT shutdown.

mod common;

use common::TestEnv;
use std::io::{BufRead, BufReader};
use std::process::{Command as StdCommand, Stdio};
use std::time::{Duration, Instant};
use tempfile::TempDir;

/// Wait for the server to print its "listening on" line, then return
/// the bound `127.0.0.1:PORT`. Tests bind to `127.0.0.1:0` so we have
/// to discover the actual port from the server's own stderr.
fn wait_for_listening(stderr: &mut Box<dyn BufRead + Send>) -> String {
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut line = String::new();
    while Instant::now() < deadline {
        line.clear();
        if stderr.read_line(&mut line).unwrap() == 0 {
            continue;
        }
        if let Some(rest) = line.find("http://").and_then(|i| line[i + 7..].split_whitespace().next()) {
            return rest.to_string();
        }
    }
    panic!("server didn't print a listening URL within 5s; last line: {line}");
}

struct ServerHandle {
    child: std::process::Child,
    addr: String,
    stderr: Option<std::thread::JoinHandle<Vec<String>>>,
}

impl ServerHandle {
    fn spawn(env: &TestEnv, config_path: &std::path::Path) -> Self {
        let bin = assert_cmd::cargo::cargo_bin("bougie");
        let mut child = StdCommand::new(bin)
            .args([
                "server",
                "run",
                "--config",
                config_path.to_str().unwrap(),
                "--listen",
                "127.0.0.1:0",
            ])
            .env("BOUGIE_HOME", env.home_path())
            .env("BOUGIE_CACHE", env.cache_path())
            .env_remove("RUST_LOG")
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn bougie server");
        let stderr = child.stderr.take().expect("piped stderr");
        let mut reader: Box<dyn BufRead + Send> = Box::new(BufReader::new(stderr));
        let addr = wait_for_listening(&mut reader);
        // Drain the rest of stderr in a thread so the pipe never fills.
        let stderr_thread = std::thread::spawn(move || {
            let mut lines = Vec::new();
            for line in reader.lines().map_while(Result::ok) {
                lines.push(line);
            }
            lines
        });
        Self { child, addr, stderr: Some(stderr_thread) }
    }

    fn url(&self, path: &str) -> String {
        format!("http://{}{path}", self.addr)
    }

    fn shutdown(mut self) -> Vec<String> {
        // Send SIGINT — phase 1 install handler should drain quickly.
        // Shell out to `kill` rather than `libc::kill` so the test stays
        // inside the workspace's `unsafe_code = "forbid"` lint.
        let _ = StdCommand::new("kill")
            .args(["-INT", &self.child.id().to_string()])
            .status();
        let status = self
            .child
            .wait_timeout(Duration::from_secs(7))
            .expect("wait on bougie server")
            .expect("server exited within grace");
        assert!(status.success(), "server exited non-zero: {status:?}");
        self.stderr.take().unwrap().join().unwrap_or_default()
    }
}

trait WaitTimeout {
    fn wait_timeout(
        &mut self,
        timeout: Duration,
    ) -> std::io::Result<Option<std::process::ExitStatus>>;
}

impl WaitTimeout for std::process::Child {
    fn wait_timeout(
        &mut self,
        timeout: Duration,
    ) -> std::io::Result<Option<std::process::ExitStatus>> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(status) = self.try_wait()? {
                return Ok(Some(status));
            }
            if Instant::now() >= deadline {
                return Ok(None);
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

fn write_fixture(proj: &std::path::Path) {
    let public = proj.join("public");
    std::fs::create_dir_all(&public).unwrap();
    std::fs::write(public.join("index.html"), "<h1>hello</h1>").unwrap();
    std::fs::write(public.join("style.css"), "body { color: red }").unwrap();
    std::fs::write(public.join("script.php"), "<?php phpinfo();").unwrap();
    // A file outside the web root that traversal must not reach.
    std::fs::write(proj.join("secret.txt"), "shh").unwrap();
}

/// Pre-seed a minimal `server.toml` in `<dir>/server.toml` with one
/// `[[host]]` block, returning the file path. Replaces the
/// retired `bougie server add` CLI in test setup. `root` is optional;
/// when `None` we omit the field (server defaults to `.`).
fn seed_server_toml(
    dir: &std::path::Path,
    hostname: &str,
    project: &std::path::Path,
    root: Option<&str>,
) -> std::path::PathBuf {
    std::fs::create_dir_all(dir).expect("mkdir config dir");
    let project = std::fs::canonicalize(project).expect("canonicalize project");
    let root_line = root.map(|r| format!("root = \"{r}\"\n")).unwrap_or_default();
    let body = format!(
        "[server]\nlisten = \"127.0.0.1:7080\"\nlog_format = \"text\"\n\n[[host]]\nhostname = \"{hostname}\"\nproject = \"{project}\"\n{root_line}",
        project = project.display(),
    );
    let path = dir.join("server.toml");
    std::fs::write(&path, body).expect("write server.toml");
    path
}

fn http_get(url: &str, host: &str) -> (u16, std::collections::HashMap<String, String>, Vec<u8>) {
    // Use a blocking reqwest client. The crate is already in bougie's
    // deps for the production code.
    let client = reqwest::blocking::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();
    let resp = client.get(url).header("Host", host).send().unwrap();
    let status = resp.status().as_u16();
    let headers: std::collections::HashMap<String, String> = resp
        .headers()
        .iter()
        .map(|(k, v)| (k.as_str().to_owned(), v.to_str().unwrap_or("").to_owned()))
        .collect();
    let body = resp.bytes().unwrap().to_vec();
    (status, headers, body)
}

#[test]
fn static_file_round_trip() {
    let env = TestEnv::new();
    let cfg_dir = TempDir::new().unwrap();
    let proj = TempDir::new().unwrap();
    write_fixture(proj.path());

    let cfg = seed_server_toml(cfg_dir.path(), "myapp.bougie.run", proj.path(), Some("public"));

    let server = ServerHandle::spawn(&env, &cfg);

    let (status, headers, body) = http_get(&server.url("/"), "myapp.bougie.run");
    assert_eq!(status, 200);
    assert!(String::from_utf8_lossy(&body).contains("hello"));
    assert_eq!(headers.get("content-type").map(String::as_str), Some("text/html"));
    assert_eq!(headers.get("cache-control").map(String::as_str), Some("no-cache"));

    let (status, headers, body) = http_get(&server.url("/style.css"), "myapp.bougie.run");
    assert_eq!(status, 200);
    assert!(String::from_utf8_lossy(&body).contains("color: red"));
    assert_eq!(headers.get("content-type").map(String::as_str), Some("text/css"));

    let _ = server.shutdown();
}

#[test]
fn missing_file_is_404() {
    let env = TestEnv::new();
    let cfg_dir = TempDir::new().unwrap();
    let proj = TempDir::new().unwrap();
    write_fixture(proj.path());
    let cfg = seed_server_toml(cfg_dir.path(), "myapp.bougie.run", proj.path(), Some("public"));
    let server = ServerHandle::spawn(&env, &cfg);
    let (status, _, body) = http_get(&server.url("/nope.txt"), "myapp.bougie.run");
    assert_eq!(status, 404);
    assert!(String::from_utf8_lossy(&body).contains("not found"));
    let _ = server.shutdown();
}

#[test]
fn unknown_host_is_404() {
    let env = TestEnv::new();
    let cfg_dir = TempDir::new().unwrap();
    let proj = TempDir::new().unwrap();
    write_fixture(proj.path());
    let cfg = seed_server_toml(cfg_dir.path(), "myapp.bougie.run", proj.path(), None);
    let server = ServerHandle::spawn(&env, &cfg);
    let (status, _, body) = http_get(&server.url("/"), "ghost.bougie.run");
    assert_eq!(status, 404);
    assert!(String::from_utf8_lossy(&body).contains("unknown host"));
    let _ = server.shutdown();
}

#[test]
fn php_request_without_resolved_php_returns_502() {
    // Phase 2 replaces the phase-1 501 stub with a real FastCGI
    // dispatcher. With no `vendor/bougie/state/resolved` in the project,
    // the pool manager can't find a php-fpm binary and surfaces the
    // failure as 502 — actionable for the user.
    let env = TestEnv::new();
    let cfg_dir = TempDir::new().unwrap();
    let proj = TempDir::new().unwrap();
    write_fixture(proj.path());
    let cfg = seed_server_toml(cfg_dir.path(), "myapp.bougie.run", proj.path(), Some("public"));
    let server = ServerHandle::spawn(&env, &cfg);
    let (status, _, body) = http_get(&server.url("/script.php"), "myapp.bougie.run");
    assert_eq!(status, 502);
    let body_str = String::from_utf8_lossy(&body).to_lowercase();
    assert!(
        body_str.contains("php-fpm") || body_str.contains("bougie sync"),
        "got: {body_str}"
    );
    let _ = server.shutdown();
}

#[cfg(unix)]
#[test]
fn php_fpm_startup_failure_surfaces_stderr() {
    use std::os::unix::fs::PermissionsExt;

    let env = TestEnv::new();
    let cfg_dir = TempDir::new().unwrap();
    let proj = TempDir::new().unwrap();
    write_fixture(proj.path());

    // Resolve the project against a fake install whose php-fpm prints
    // a complaint and dies — the shape of a real master failing on a
    // broken pool conf or an unloadable extension.
    let state_dir = proj.path().join("vendor/bougie/state");
    std::fs::create_dir_all(&state_dir).unwrap();
    std::fs::write(state_dir.join("resolved"), "9.9.9-nts\n").unwrap();
    let bin_dir = env.home_path().join("installs").join("9.9.9-nts").join("bin");
    std::fs::create_dir_all(&bin_dir).unwrap();
    let fpm = bin_dir.join("php-fpm");
    std::fs::write(
        &fpm,
        "#!/bin/sh\necho \"ERROR: simulated fpm startup failure\" >&2\nexit 78\n",
    )
    .unwrap();
    std::fs::set_permissions(&fpm, std::fs::Permissions::from_mode(0o755)).unwrap();

    let cfg = seed_server_toml(cfg_dir.path(), "myapp.bougie.run", proj.path(), Some("public"));
    let server = ServerHandle::spawn(&env, &cfg);

    // The 502 body carries php-fpm's own words, not a bare timeout.
    let (status, _, body) = http_get(&server.url("/script.php"), "myapp.bougie.run");
    assert_eq!(status, 502);
    let body_str = String::from_utf8_lossy(&body);
    assert!(body_str.contains("exited during startup"), "got: {body_str}");
    assert!(body_str.contains("simulated fpm startup failure"), "got: {body_str}");

    let stderr = server.shutdown();
    let joined = stderr.join("\n");
    // Both the forwarded fpm stderr and the failure summary are
    // prefixed with the *vhost*, so `bougie server`'s host-scoped log
    // attach (a per-line substring filter) shows them.
    assert!(
        joined.contains("[fpm:myapp.bougie.run:normal] ERROR: simulated fpm startup failure"),
        "{joined}"
    );
    assert!(
        joined.contains("[fpm:myapp.bougie.run:normal] php-fpm failed to start"),
        "{joined}"
    );
}

// Regression: php-fpm's stdout/stderr pipes must be drained from the
// moment it spawns. The master only binds its socket after config
// parse + opcache.preload, so a chatty preload (Magento emits ~860KB
// of unlinked-class warnings) used to fill the 64KB pipe buffer and
// block php-fpm mid-startup — every spawn then died as a bare 2s
// timeout with its output lost. With the drain wired before the
// readiness wait, the writer finishes and the failure is reported
// fast, with php-fpm's own words.
#[cfg(unix)]
#[test]
fn chatty_fpm_startup_does_not_deadlock_on_full_pipe() {
    use std::os::unix::fs::PermissionsExt;

    let env = TestEnv::new();
    let cfg_dir = TempDir::new().unwrap();
    let proj = TempDir::new().unwrap();
    write_fixture(proj.path());

    let state_dir = proj.path().join("vendor/bougie/state");
    std::fs::create_dir_all(&state_dir).unwrap();
    std::fs::write(state_dir.join("resolved"), "9.9.9-nts\n").unwrap();
    let bin_dir = env.home_path().join("installs").join("9.9.9-nts").join("bin");
    std::fs::create_dir_all(&bin_dir).unwrap();
    let fpm = bin_dir.join("php-fpm");
    // ~1.1MB of warnings >> the 64KB pipe buffer, then a fatal line.
    std::fs::write(
        &fpm,
        "#!/bin/sh\n\
         i=0\n\
         while [ $i -lt 20000 ]; do\n\
         echo \"Warning: Cannot link class Some\\\\Generated\\\\Proxy $i in preload\"\n\
         i=$((i+1))\n\
         done >&2\n\
         echo \"FATAL: preload emitted too much noise\" >&2\n\
         exit 78\n",
    )
    .unwrap();
    std::fs::set_permissions(&fpm, std::fs::Permissions::from_mode(0o755)).unwrap();

    let cfg = seed_server_toml(cfg_dir.path(), "myapp.bougie.run", proj.path(), Some("public"));
    let server = ServerHandle::spawn(&env, &cfg);

    let (status, _, body) = http_get(&server.url("/script.php"), "myapp.bougie.run");
    assert_eq!(status, 502);
    let body_str = String::from_utf8_lossy(&body);
    // The old code deadlocked here and reported "didn't create its
    // listen socket ... within 2s" with no output at all.
    assert!(body_str.contains("exited during startup"), "got: {body_str}");
    assert!(
        body_str.contains("FATAL: preload emitted too much noise"),
        "got: {body_str}"
    );

    let stderr = server.shutdown();
    let joined = stderr.join("\n");
    assert!(
        joined.contains("[fpm:myapp.bougie.run:normal] FATAL: preload emitted too much noise"),
        "fpm stderr should be forwarded to the server log"
    );
}

#[test]
fn sigint_drains_and_exits_cleanly() {
    let env = TestEnv::new();
    let cfg_dir = TempDir::new().unwrap();
    let proj = TempDir::new().unwrap();
    write_fixture(proj.path());
    let cfg = seed_server_toml(cfg_dir.path(), "myapp.bougie.run", proj.path(), Some("public"));
    let server = ServerHandle::spawn(&env, &cfg);
    // Serve one request before shutting down so we exercise the drain
    // path with real in-flight state.
    let (status, _, _) = http_get(&server.url("/"), "myapp.bougie.run");
    assert_eq!(status, 200);
    let stderr = server.shutdown();
    let joined = stderr.join("\n");
    assert!(joined.contains("SIGINT") || joined.contains("shutting down"), "{joined}");
}
