//! Phase 2 integration test: hits a real php-fpm via the bougie server's
//! `FastCGI` dispatcher. Gated on a real bougie PHP install being present
//! at `$BOUGIE_HOME/installs/<resolved>/bin/php-fpm`; without one, the
//! test exits early with a stderr note and counts as a pass.
//!
//! The test fixture has no `bougie sync` artifacts, only the minimal
//! resolved-state marker we write here, so the test stands alone
//! against any installed PHP version the developer has.

mod common;

use common::TestEnv;
use std::fmt::Write as _;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command as StdCommand, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tempfile::TempDir;

fn discover_installed_php() -> Option<(String, PathBuf)> {
    let candidates: Vec<PathBuf> = std::env::var_os("BOUGIE_HOME")
        .map(|h| vec![PathBuf::from(h)])
        .unwrap_or_else(|| {
            // Default XDG resolution: $XDG_DATA_HOME/bougie or ~/.local/share/bougie.
            let xdg = std::env::var_os("XDG_DATA_HOME").map_or_else(
                || std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")),
                |x| Some(PathBuf::from(x)),
            );
            xdg.into_iter().map(|d| d.join("bougie")).collect()
        });

    for home in candidates {
        let installs = home.join("installs");
        let Ok(entries) = std::fs::read_dir(&installs) else { continue };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(name_str) = name.to_str() else { continue };
            let fpm = entry.path().join("bin").join("php-fpm");
            if fpm.is_file() {
                return Some((name_str.to_owned(), home));
            }
        }
    }
    None
}

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
    // Drained lines made visible while the server is still running.
    // Phase 4 tests inspect this to verify pool_reload / pool_idle_out
    // events fired without having to wait for full shutdown.
    live_stderr: Arc<std::sync::Mutex<Vec<String>>>,
    // Per-server XDG_RUNTIME_DIR. Owns the TempDir so `$XDG_RUNTIME_DIR/
    // bougie/server/{<project-hash>/...,control.sock}` are isolated
    // across tests run in parallel. Without this, `ServerPaths::from_env`
    // lands every parallel server in the same runtime root, and the
    // startup/shutdown `prune_project_dirs` calls in `server/run.rs`
    // delete peers' live pool dirs. Kept after shutdown so post-mortem
    // inspections of pool sockets stay possible.
    _runtime: TempDir,
}

impl ServerHandle {
    fn spawn(env: &TestEnv, config_path: &Path, bougie_home: &Path) -> Self {
        Self::spawn_with_extra_env(env, config_path, bougie_home, &[])
    }

    fn spawn_with_extra_env(
        env: &TestEnv,
        config_path: &Path,
        bougie_home: &Path,
        extra: &[(&str, &str)],
    ) -> Self {
        let runtime = TempDir::new().expect("tempdir for XDG_RUNTIME_DIR");
        let bin = assert_cmd::cargo::cargo_bin("bougie");
        let mut cmd = StdCommand::new(bin);
        cmd.args([
            "server",
            "run",
            "--config",
            config_path.to_str().unwrap(),
            "--listen",
            "127.0.0.1:0",
        ])
            .env("BOUGIE_HOME", bougie_home)
            .env("BOUGIE_CACHE", env.cache_path())
            .env("XDG_RUNTIME_DIR", runtime.path())
            .env_remove("RUST_LOG");
        for (k, v) in extra {
            cmd.env(*k, *v);
        }
        let mut child = cmd
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn bougie server");
        let stderr = child.stderr.take().expect("piped stderr");
        let mut reader: Box<dyn BufRead + Send> = Box::new(BufReader::new(stderr));
        let addr = wait_for_listening(&mut reader);
        let live_stderr = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let live_for_thread = Arc::clone(&live_stderr);
        let stderr_thread = std::thread::spawn(move || {
            let mut lines = Vec::new();
            for line in reader.lines().map_while(Result::ok) {
                live_for_thread.lock().unwrap().push(line.clone());
                lines.push(line);
            }
            lines
        });
        Self { child, addr, stderr: Some(stderr_thread), live_stderr, _runtime: runtime }
    }

    fn live_stderr_contains(&self, needle: &str) -> bool {
        let lines = self.live_stderr.lock().unwrap();
        lines.iter().any(|l| l.contains(needle))
    }

    fn wait_for_stderr(&self, needle: &str, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if self.live_stderr_contains(needle) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        false
    }

    fn url(&self, path: &str) -> String {
        format!("http://{}{path}", self.addr)
    }

    fn shutdown(mut self) -> Vec<String> {
        let _ = StdCommand::new("kill")
            .args(["-INT", &self.child.id().to_string()])
            .status();
        let deadline = Instant::now() + Duration::from_secs(7);
        loop {
            if let Ok(Some(status)) = self.child.try_wait() {
                assert!(status.success(), "server exited non-zero: {status:?}");
                break;
            }
            if Instant::now() >= deadline {
                let _ = self.child.kill();
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        self.stderr.take().unwrap().join().unwrap_or_default()
    }
}

fn http(method: &str, url: &str, host: &str, body: Option<(&str, &[u8])>)
    -> (u16, std::collections::HashMap<String, String>, Vec<u8>)
{
    http_with_headers(method, url, host, &[], body)
}

fn http_with_headers(
    method: &str,
    url: &str,
    host: &str,
    extra: &[(&str, &str)],
    body: Option<(&str, &[u8])>,
) -> (u16, std::collections::HashMap<String, String>, Vec<u8>) {
    let client = reqwest::blocking::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();
    let mut req = client.request(method.parse().unwrap(), url).header("Host", host);
    for (k, v) in extra {
        req = req.header(*k, *v);
    }
    if let Some((ct, b)) = body {
        req = req.header("Content-Type", ct).body(b.to_vec());
    }
    let resp = req.send().unwrap();
    let status = resp.status().as_u16();
    let headers: std::collections::HashMap<String, String> = resp
        .headers()
        .iter()
        .map(|(k, v)| (k.as_str().to_owned(), v.to_str().unwrap_or("").to_owned()))
        .collect();
    (status, headers, resp.bytes().unwrap().to_vec())
}

#[test]
fn fastcgi_round_trip_with_real_php_fpm() {
    let Some((resolved, bougie_home)) = discover_installed_php() else {
        eprintln!("(skipped: no bougie-installed php-fpm found on this system)");
        return;
    };

    let env = TestEnv::new();
    let xdg = TempDir::new().unwrap();
    let proj = TempDir::new().unwrap();

    // Write a minimal project fixture: web root + index.php that echoes
    // CGI params back, plus the resolved-state marker the pool manager
    // needs to find the interpreter.
    std::fs::create_dir_all(proj.path().join("public")).unwrap();
    std::fs::create_dir_all(proj.path().join("vendor/bougie/state")).unwrap();
    std::fs::create_dir_all(proj.path().join("vendor/bougie/conf.d")).unwrap();
    std::fs::write(
        proj.path().join("public/index.php"),
        r#"<?php
header('Content-Type: text/plain');
echo "METHOD=" . $_SERVER['REQUEST_METHOD'] . "\n";
echo "SCRIPT=" . $_SERVER['SCRIPT_NAME'] . "\n";
echo "PI=" . ($_SERVER['PATH_INFO'] ?? '') . "\n";
echo "Q=" . ($_SERVER['QUERY_STRING'] ?? '') . "\n";
echo "BODY=" . file_get_contents('php://input') . "\n";
"#,
    )
    .unwrap();
    std::fs::write(proj.path().join("vendor/bougie/state/resolved"), &resolved).unwrap();

    let cfg = seed_single_host(xdg.path(), "fcgi-test.bougie.run", proj.path());
    let server = ServerHandle::spawn(&env, &cfg, &bougie_home);

    // GET front-controller fallthrough.
    let (status, headers, body) =
        http("GET", &server.url("/users/42?page=1"), "fcgi-test.bougie.run", None);
    assert_eq!(status, 200);
    assert_eq!(headers.get("x-bougie-pool").map(String::as_str), Some("normal"));
    let body_str = String::from_utf8_lossy(&body);
    assert!(body_str.contains("METHOD=GET"));
    assert!(body_str.contains("SCRIPT=/index.php"));
    assert!(body_str.contains("PI=/users/42"));
    assert!(body_str.contains("Q=page=1"));

    // POST body round-trip.
    let (status, _, body) = http(
        "POST",
        &server.url("/index.php"),
        "fcgi-test.bougie.run",
        Some(("application/json", b"{\"hi\":1}")),
    );
    assert_eq!(status, 200);
    let body_str = String::from_utf8_lossy(&body);
    assert!(body_str.contains("METHOD=POST"));
    assert!(body_str.contains("BODY={\"hi\":1}"), "got: {body_str}");

    let _ = server.shutdown();
}

#[test]
fn xdebug_session_cookie_routes_to_xdebug_pool() {
    let Some((resolved, bougie_home)) = discover_installed_php() else {
        eprintln!("(skipped: no bougie-installed php-fpm found on this system)");
        return;
    };

    let env = TestEnv::new();
    let xdg = TempDir::new().unwrap();
    let proj = TempDir::new().unwrap();

    std::fs::create_dir_all(proj.path().join("public")).unwrap();
    std::fs::create_dir_all(proj.path().join("vendor/bougie/state")).unwrap();
    std::fs::create_dir_all(proj.path().join("vendor/bougie/conf.d")).unwrap();
    std::fs::write(proj.path().join("public/index.php"), "<?php echo 'ok';").unwrap();
    std::fs::write(proj.path().join("vendor/bougie/state/resolved"), &resolved).unwrap();

    let cfg = seed_single_host(xdg.path(), "xdebug-test.bougie.run", proj.path());
    let server = ServerHandle::spawn(&env, &cfg, &bougie_home);

    // No cookie → normal pool.
    let (status, headers, _) =
        http("GET", &server.url("/index.php"), "xdebug-test.bougie.run", None);
    assert_eq!(status, 200);
    assert_eq!(headers.get("x-bougie-pool").map(String::as_str), Some("normal"));

    // XDEBUG_SESSION cookie → xdebug pool.
    let (status, headers, _) = http_with_headers(
        "GET",
        &server.url("/index.php"),
        "xdebug-test.bougie.run",
        &[("Cookie", "XDEBUG_SESSION=phpstorm")],
        None,
    );
    assert_eq!(status, 200);
    assert_eq!(headers.get("x-bougie-pool").map(String::as_str), Some("xdebug"));

    // X-Bougie-Force-Xdebug header path too.
    let (status, headers, _) = http_with_headers(
        "GET",
        &server.url("/index.php"),
        "xdebug-test.bougie.run",
        &[("X-Bougie-Force-Xdebug", "1")],
        None,
    );
    assert_eq!(status, 200);
    assert_eq!(headers.get("x-bougie-pool").map(String::as_str), Some("xdebug"));

    // Back to no cookie → still routes to the original normal pool
    // (both pools coexist; switching is per-request, no restart).
    let (status, headers, _) =
        http("GET", &server.url("/index.php"), "xdebug-test.bougie.run", None);
    assert_eq!(status, 200);
    assert_eq!(headers.get("x-bougie-pool").map(String::as_str), Some("normal"));

    let _ = server.shutdown();
}

/// Build a fixture project: web root + index.php + vendor/bougie/state/resolved.
fn make_php_project(resolved: &str) -> TempDir {
    let proj = TempDir::new().unwrap();
    std::fs::create_dir_all(proj.path().join("public")).unwrap();
    std::fs::create_dir_all(proj.path().join("vendor/bougie/state")).unwrap();
    std::fs::create_dir_all(proj.path().join("vendor/bougie/conf.d")).unwrap();
    std::fs::write(proj.path().join("public/index.php"), "<?php echo 'ok';").unwrap();
    std::fs::write(proj.path().join("vendor/bougie/state/resolved"), resolved).unwrap();
    proj
}

/// Write a `[server]` block so a test can pin idle/max overrides
/// before the bougie subprocess starts. Returns the written-to path
/// so callers can pass it as `--config`.
fn write_server_toml(xdg: &Path, body: &str, hosts: &[(&str, &Path)]) -> PathBuf {
    let cfg = xdg.join("bougie/server.toml");
    std::fs::create_dir_all(cfg.parent().unwrap()).unwrap();
    let mut s = body.to_string();
    s.push('\n');
    for (host, project) in hosts {
        write!(
            s,
            "[[host]]\nhostname = \"{host}\"\nproject = \"{}\"\nroot = \"public\"\n\n",
            project.display()
        )
        .unwrap();
    }
    std::fs::write(&cfg, s).unwrap();
    cfg
}

/// Convenience for the common "default `[server]` block + one
/// `[[host]]` with root=public" shape that the retired
/// `bougie server add` CLI used to produce.
fn seed_single_host(xdg: &Path, hostname: &str, project: &Path) -> PathBuf {
    write_server_toml(
        xdg,
        "[server]\nlisten = \"127.0.0.1:7080\"\nlog_format = \"text\"\n",
        &[(hostname, project)],
    )
}

#[test]
fn lru_evicts_oldest_when_cap_hit() {
    let Some((resolved, bougie_home)) = discover_installed_php() else {
        eprintln!("(skipped: no bougie-installed php-fpm found on this system)");
        return;
    };

    let env = TestEnv::new();
    let xdg = TempDir::new().unwrap();
    let proj_a = make_php_project(&resolved);
    let proj_b = make_php_project(&resolved);

    let cfg = write_server_toml(
        xdg.path(),
        "[server]\nmax_concurrent_pools = 1\nidle_pool_timeout = \"1h\"\n",
        &[
            ("a.bougie.run", proj_a.path()),
            ("b.bougie.run", proj_b.path()),
        ],
    );

    let server = ServerHandle::spawn(&env, &cfg, &bougie_home);

    let (s, _, _) = http("GET", &server.url("/index.php"), "a.bougie.run", None);
    assert_eq!(s, 200);
    let (s, _, _) = http("GET", &server.url("/index.php"), "b.bougie.run", None);
    assert_eq!(s, 200);

    assert!(
        server.wait_for_stderr("pool_evicted", Duration::from_secs(3)),
        "expected pool_evicted event in stderr"
    );

    let _ = server.shutdown();
}

#[test]
fn idle_pool_is_reaped() {
    let Some((resolved, bougie_home)) = discover_installed_php() else {
        eprintln!("(skipped: no bougie-installed php-fpm found on this system)");
        return;
    };

    let env = TestEnv::new();
    let xdg = TempDir::new().unwrap();
    let proj = make_php_project(&resolved);

    let cfg = write_server_toml(
        xdg.path(),
        // 1s idle timeout, paired with a fast reaper period so the
        // test doesn't sit for 10s.
        "[server]\nidle_pool_timeout = \"1s\"\nmax_concurrent_pools = 16\n",
        &[("idle.bougie.run", proj.path())],
    );

    let server = ServerHandle::spawn_with_extra_env(
        &env,
        &cfg,
        &bougie_home,
        &[("BOUGIE_SERVER_REAPER_PERIOD_MS", "200")],
    );

    let (s, _, _) = http("GET", &server.url("/index.php"), "idle.bougie.run", None);
    assert_eq!(s, 200);

    assert!(
        server.wait_for_stderr("pool_idle_out", Duration::from_secs(5)),
        "expected pool_idle_out event"
    );

    // A subsequent request should still succeed — the pool just
    // cold-starts again.
    let (s, _, _) = http("GET", &server.url("/index.php"), "idle.bougie.run", None);
    assert_eq!(s, 200);

    let _ = server.shutdown();
}

#[test]
fn confd_change_triggers_reload() {
    let Some((resolved, bougie_home)) = discover_installed_php() else {
        eprintln!("(skipped: no bougie-installed php-fpm found on this system)");
        return;
    };

    let env = TestEnv::new();
    let xdg = TempDir::new().unwrap();
    let proj = make_php_project(&resolved);

    let cfg = seed_single_host(xdg.path(), "reload.bougie.run", proj.path());
    let server = ServerHandle::spawn(&env, &cfg, &bougie_home);

    let (s, _, _) = http("GET", &server.url("/index.php"), "reload.bougie.run", None);
    assert_eq!(s, 200);

    // Drop a new conf.d fragment — phase 2 doesn't actually have to
    // load anything from it; we only want to verify the watcher
    // dispatch + SIGUSR2.
    std::fs::write(
        proj.path().join("vendor/bougie/conf.d/99-test.ini"),
        "; managed by bougie server test — touch trigger\n",
    )
    .unwrap();

    assert!(
        server.wait_for_stderr("pool_reload", Duration::from_secs(3)),
        "expected pool_reload event after conf.d write"
    );

    // Pool should still serve requests post-reload — SIGUSR2 doesn't
    // kill the master.
    let (s, _, _) = http("GET", &server.url("/index.php"), "reload.bougie.run", None);
    assert_eq!(s, 200);

    let _ = server.shutdown();
}

#[test]
fn xdebug_query_param_routes_to_xdebug_pool() {
    let Some((resolved, bougie_home)) = discover_installed_php() else {
        eprintln!("(skipped: no bougie-installed php-fpm found on this system)");
        return;
    };

    let env = TestEnv::new();
    let xdg = TempDir::new().unwrap();
    let proj = TempDir::new().unwrap();

    std::fs::create_dir_all(proj.path().join("public")).unwrap();
    std::fs::create_dir_all(proj.path().join("vendor/bougie/state")).unwrap();
    std::fs::create_dir_all(proj.path().join("vendor/bougie/conf.d")).unwrap();
    std::fs::write(proj.path().join("public/index.php"), "<?php echo 'ok';").unwrap();
    std::fs::write(proj.path().join("vendor/bougie/state/resolved"), &resolved).unwrap();

    let cfg = seed_single_host(xdg.path(), "xdebug-q.bougie.run", proj.path());
    let server = ServerHandle::spawn(&env, &cfg, &bougie_home);

    let (status, headers, _) = http(
        "GET",
        &server.url("/index.php?XDEBUG_SESSION_START=phpstorm"),
        "xdebug-q.bougie.run",
        None,
    );
    assert_eq!(status, 200);
    assert_eq!(headers.get("x-bougie-pool").map(String::as_str), Some("xdebug"));

    let _ = server.shutdown();
}
