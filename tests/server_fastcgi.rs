//! Phase 2 integration test: hits a real php-fpm via the bougie server's
//! FastCGI dispatcher. Gated on a real bougie PHP install being present
//! at `$BOUGIE_HOME/installs/<resolved>/bin/php-fpm`; without one, the
//! test exits early with a stderr note and counts as a pass.
//!
//! The test fixture has no `bougie sync` artifacts, only the minimal
//! resolved-state marker we write here, so the test stands alone
//! against any installed PHP version the developer has.

mod common;

use common::TestEnv;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command as StdCommand, Stdio};
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
}

impl ServerHandle {
    fn spawn(env: &TestEnv, xdg_config: &Path, bougie_home: &Path) -> Self {
        let bin = assert_cmd::cargo::cargo_bin("bougie");
        let mut child = StdCommand::new(bin)
            .args(["server", "run", "--listen", "127.0.0.1:0"])
            .env("BOUGIE_HOME", bougie_home)
            .env("BOUGIE_CACHE", env.cache_path())
            .env("XDG_CONFIG_HOME", xdg_config)
            .env_remove("RUST_LOG")
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn bougie server");
        let stderr = child.stderr.take().expect("piped stderr");
        let mut reader: Box<dyn BufRead + Send> = Box::new(BufReader::new(stderr));
        let addr = wait_for_listening(&mut reader);
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
    std::fs::create_dir_all(proj.path().join(".bougie/state")).unwrap();
    std::fs::create_dir_all(proj.path().join(".bougie/conf.d")).unwrap();
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
    std::fs::write(proj.path().join(".bougie/state/resolved"), &resolved).unwrap();

    env.bougie()
        .env("XDG_CONFIG_HOME", xdg.path())
        .args([
            "server",
            "add",
            "fcgi-test.bougie.run",
            proj.path().to_str().unwrap(),
            "--root",
            "public",
        ])
        .assert()
        .success();

    let server = ServerHandle::spawn(&env, xdg.path(), &bougie_home);

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
    std::fs::create_dir_all(proj.path().join(".bougie/state")).unwrap();
    std::fs::create_dir_all(proj.path().join(".bougie/conf.d")).unwrap();
    std::fs::write(proj.path().join("public/index.php"), "<?php echo 'ok';").unwrap();
    std::fs::write(proj.path().join(".bougie/state/resolved"), &resolved).unwrap();

    env.bougie()
        .env("XDG_CONFIG_HOME", xdg.path())
        .args([
            "server",
            "add",
            "xdebug-test.bougie.run",
            proj.path().to_str().unwrap(),
            "--root",
            "public",
        ])
        .assert()
        .success();

    let server = ServerHandle::spawn(&env, xdg.path(), &bougie_home);

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
    std::fs::create_dir_all(proj.path().join(".bougie/state")).unwrap();
    std::fs::create_dir_all(proj.path().join(".bougie/conf.d")).unwrap();
    std::fs::write(proj.path().join("public/index.php"), "<?php echo 'ok';").unwrap();
    std::fs::write(proj.path().join(".bougie/state/resolved"), &resolved).unwrap();

    env.bougie()
        .env("XDG_CONFIG_HOME", xdg.path())
        .args([
            "server",
            "add",
            "xdebug-q.bougie.run",
            proj.path().to_str().unwrap(),
            "--root",
            "public",
        ])
        .assert()
        .success();

    let server = ServerHandle::spawn(&env, xdg.path(), &bougie_home);

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
