//! Phase 6 integration: control socket + `bougie server list` live merge.
//! Spawns bougie server, hits the unix control socket directly with a
//! `status` request, then runs `bougie server list` as a separate
//! process and confirms it picks up the live block.

mod common;

use common::TestEnv;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{Command as StdCommand, Stdio};
use std::time::{Duration, Instant};
use tempfile::TempDir;

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

fn wait_for_socket(path: &std::path::Path, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    while !path.exists() {
        if Instant::now() >= deadline {
            panic!("control socket {} didn't appear within {timeout:?}", path.display());
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

#[test]
fn control_socket_status_returns_listen_port_and_hosts() {
    let env = TestEnv::new();
    let xdg = TempDir::new().unwrap();
    let runtime = TempDir::new().unwrap();
    let proj = TempDir::new().unwrap();
    std::fs::create_dir_all(proj.path().join("public")).unwrap();
    std::fs::write(proj.path().join("public/index.html"), "<h1>hi</h1>").unwrap();

    env.bougie()
        .env("XDG_CONFIG_HOME", xdg.path())
        .args([
            "server",
            "add",
            "ctrl.bougie.run",
            proj.path().to_str().unwrap(),
            "--root",
            "public",
        ])
        .assert()
        .success();

    let bin = assert_cmd::cargo::cargo_bin("bougie");
    let mut child = StdCommand::new(&bin)
        .args(["server", "run", "--listen", "127.0.0.1:0"])
        .env("BOUGIE_HOME", env.home_path())
        .env("BOUGIE_CACHE", env.cache_path())
        .env("XDG_CONFIG_HOME", xdg.path())
        .env("XDG_RUNTIME_DIR", runtime.path())
        .env_remove("RUST_LOG")
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn bougie server");
    let stderr = child.stderr.take().expect("piped stderr");
    let mut reader: Box<dyn BufRead + Send> = Box::new(BufReader::new(stderr));
    let _addr = wait_for_listening(&mut reader);

    let socket: PathBuf = runtime.path().join("bougie/server/control.sock");
    wait_for_socket(&socket, Duration::from_secs(2));

    // Connect synchronously via std's UnixStream — no need for tokio
    // in the test client.
    let mut stream = UnixStream::connect(&socket).expect("connect control sock");
    stream.write_all(b"{\"v\":1,\"method\":\"status\"}\n").unwrap();
    stream.shutdown(std::net::Shutdown::Write).unwrap();
    let mut body = String::new();
    stream.read_to_string(&mut body).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(body.trim()).unwrap();
    assert_eq!(parsed["schema_version"], 1);
    assert_eq!(parsed["ok"], true);
    assert!(parsed["listen_port"].as_u64().unwrap() > 0);
    let hosts: Vec<String> = parsed["hosts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_owned())
        .collect();
    assert!(hosts.contains(&"ctrl.bougie.run".into()));
    // No requests served yet → no pools.
    assert_eq!(parsed["pools"].as_array().unwrap().len(), 0);

    // `bougie server list` should pick up the live block now.
    let out = env
        .bougie()
        .env("XDG_CONFIG_HOME", xdg.path())
        .env("XDG_RUNTIME_DIR", runtime.path())
        .args(["server", "list"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let text = String::from_utf8(out).unwrap();
    assert!(text.contains("ctrl.bougie.run"), "missing host: {text}");
    assert!(text.contains("running on :"), "missing live block: {text}");

    // json-v1 list carries the `live` block.
    let json_out = env
        .bougie()
        .env("XDG_CONFIG_HOME", xdg.path())
        .env("XDG_RUNTIME_DIR", runtime.path())
        .args(["--format", "json-v1", "server", "list"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let v: serde_json::Value = serde_json::from_slice(&json_out).unwrap();
    assert!(v["live"]["listen_port"].is_u64(), "json: {v}");

    let _ = StdCommand::new("kill")
        .args(["-INT", &child.id().to_string()])
        .status();
    let _ = child.wait();
}

#[test]
fn list_falls_back_when_no_server_running() {
    let env = TestEnv::new();
    let xdg = TempDir::new().unwrap();
    let runtime = TempDir::new().unwrap();
    let proj = TempDir::new().unwrap();

    env.bougie()
        .env("XDG_CONFIG_HOME", xdg.path())
        .args([
            "server",
            "add",
            "alone.bougie.run",
            proj.path().to_str().unwrap(),
        ])
        .assert()
        .success();

    // No running server → list still works, just no live block.
    let out = env
        .bougie()
        .env("XDG_CONFIG_HOME", xdg.path())
        .env("XDG_RUNTIME_DIR", runtime.path())
        .args(["server", "list"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let text = String::from_utf8(out).unwrap();
    assert!(text.contains("alone.bougie.run"));
    assert!(text.contains("no server running"));
}

#[test]
fn invalid_request_returns_error_response() {
    let env = TestEnv::new();
    let xdg = TempDir::new().unwrap();
    let runtime = TempDir::new().unwrap();
    let proj = TempDir::new().unwrap();
    std::fs::create_dir_all(proj.path().join("public")).unwrap();

    env.bougie()
        .env("XDG_CONFIG_HOME", xdg.path())
        .args([
            "server",
            "add",
            "x.bougie.run",
            proj.path().to_str().unwrap(),
            "--root",
            "public",
        ])
        .assert()
        .success();

    let bin = assert_cmd::cargo::cargo_bin("bougie");
    let mut child = StdCommand::new(&bin)
        .args(["server", "run", "--listen", "127.0.0.1:0"])
        .env("BOUGIE_HOME", env.home_path())
        .env("BOUGIE_CACHE", env.cache_path())
        .env("XDG_CONFIG_HOME", xdg.path())
        .env("XDG_RUNTIME_DIR", runtime.path())
        .env_remove("RUST_LOG")
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let stderr = child.stderr.take().unwrap();
    let mut reader: Box<dyn BufRead + Send> = Box::new(BufReader::new(stderr));
    wait_for_listening(&mut reader);

    let socket = runtime.path().join("bougie/server/control.sock");
    wait_for_socket(&socket, Duration::from_secs(2));

    let mut stream = UnixStream::connect(&socket).unwrap();
    stream.write_all(b"this is not json\n").unwrap();
    stream.shutdown(std::net::Shutdown::Write).unwrap();
    let mut body = String::new();
    stream.read_to_string(&mut body).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(body.trim()).unwrap();
    assert_eq!(parsed["ok"], false);
    assert!(parsed["error"].as_str().unwrap().contains("invalid request"));

    let _ = StdCommand::new("kill")
        .args(["-INT", &child.id().to_string()])
        .status();
    let _ = child.wait();
}
