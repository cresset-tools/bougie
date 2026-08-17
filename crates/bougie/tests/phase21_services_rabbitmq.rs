//! Phase 21: end-to-end `bougie service up rabbitmq` against a real
//! rabbitmq 4.2.6 binary (with bundled Erlang/OTP 27.3.4.11) from
//! the bougie index.
//!
//! Coverage:
//!   - bougied spawns rabbitmq-server, the AMQP listener binds
//!     127.0.0.1:5672,
//!   - per-tenant vhost + user + permissions land via `rabbitmqctl`,
//!   - tenants.json records the tenant, vhost, username, node, and the
//!     generated password (under `secrets.password`),
//!   - `service down --purge` removes the vhost + user from the
//!     live broker,
//!   - `bougie run` env injection exports
//!     `BOUGIE_SERVICE_RABBITMQ_URL` as a fully-formed AMQP DSN.
//!
//! Skipped under `BOUGIE_SKIP_REAL_RABBITMQ=1` for CI environments
//! where downloading the 47 MB tarball is undesirable.

mod common;

use assert_cmd::cargo::cargo_bin;
use common::TestEnv;
use common::project_with_composer;
use common::rabbitmq_fixture;
use std::fs;
use std::path::Path;
use std::process::Command;
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

/// Serialise rabbitmq tests within this binary. The Erlang VM boots
/// cold in ~5–10s on a warm box but contends sharply for CPU on CI
/// runners — and they all bind 5672, so parallelism is impossible
/// anyway.
fn rabbitmq_test_lock() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Erlang VM cold-start + mnesia bootstrap dominate timing; allow a
/// generous ceiling.
const STEP_TIMEOUT: Duration = Duration::from_mins(3);

fn should_skip() -> bool {
    std::env::var_os("BOUGIE_SKIP_REAL_RABBITMQ").is_some()
}

fn stop_daemon(env: &TestEnv) {
    let _ = env
        .bougie()
        .args(["service", "daemon", "stop"])
        .timeout(STEP_TIMEOUT)
        .assert();
    // Wait until the Erlang VM released the *node name*, not the catalog
    // port. The node name is what these tests actually serialise on: two
    // brokers can share a host but never a name, so the next test's `up`
    // dies with `duplicate_node_name` if the last one is still
    // registered. The port is a bad proxy for it — a broker outside this
    // suite (another bougie install, a distro rabbitmq) can hold 5672 for
    // the whole run while ours comes and goes on a relocated port, and
    // waiting on 5672 would then time out every time and hand the next
    // test a live predecessor. (4369/epmd itself is shared and reusable.)
    wait_for_node_free(Duration::from_secs(30));
}

/// Is bougie's Erlang node still registered with the local epmd? Asks
/// epmd's `NAMES` (`<<1:16, 110>>`) directly, the same query bougied's
/// own stale-node check makes. No epmd listening means no registrations.
fn node_registered() -> bool {
    use std::io::{Read, Write};
    use std::net::TcpStream;

    let addr = "127.0.0.1:4369".parse().expect("epmd addr");
    let Ok(mut sock) = TcpStream::connect_timeout(&addr, Duration::from_secs(2)) else {
        return false;
    };
    if sock.write_all(&[0, 1, 110]).is_err() {
        return false;
    }
    let mut reply = Vec::new();
    if sock.read_to_end(&mut reply).is_err() {
        return false;
    }
    String::from_utf8_lossy(&reply).lines().any(|line| {
        let mut fields = line.split_whitespace();
        fields.next() == Some("name")
            && fields.next() == Some("bougie")
            && fields.next() == Some("at")
    })
}

fn wait_for_node_free(timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while node_registered() {
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    true
}

/// Wait for *this* broker's AMQP listener, at whatever port the daemon
/// recorded for it. Hardcoding 5672 would let a foreign broker holding
/// the catalog port satisfy the wait, so a test could sail past a
/// service that never started.
fn wait_for_broker(env: &TestEnv, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(port) = recorded_port(env)
            && wait_for_tcp(&format!("127.0.0.1:{port}"), Duration::from_millis(250))
        {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// The AMQP port the daemon recorded in `endpoint.json` — the catalog
/// default, or wherever the allocator relocated to when it was taken.
fn recorded_port(env: &TestEnv) -> Option<u16> {
    let text = fs::read_to_string(
        env.home_path()
            .join("state/services/rabbitmq/4.2.6/endpoint.json"),
    )
    .ok()?;
    let v: serde_json::Value = serde_json::from_str(&text).ok()?;
    v["primary"].as_u64()?.try_into().ok()
}

/// A loopback TCP port nothing is listening on, for a broker this test
/// starts by hand. Racy in principle, harmless here: the suite is
/// serialised and the port is claimed moments later.
fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .expect("bind ephemeral")
        .local_addr()
        .expect("local addr")
        .port()
}

fn services_up_or_dump(env: &TestEnv, proj_path: &Path, extra_args: &[&str]) {
    let mut args = vec!["service", "up"];
    args.extend_from_slice(extra_args);
    let res = env
        .bougie()
        .args(&args)
        .current_dir(proj_path)
        .timeout(STEP_TIMEOUT)
        .output()
        .expect("running bougie service up");
    if !res.status.success() {
        dump_rabbitmq_log(env, "services up failure");
        panic!(
            "services up failed (exit {:?}):\n--- stdout ---\n{}\n--- stderr ---\n{}",
            res.status.code(),
            String::from_utf8_lossy(&res.stdout),
            String::from_utf8_lossy(&res.stderr),
        );
    }
}

/// Best-effort dump of every log file under
/// `state/services/rabbitmq/4.2.6/log/` for diagnostics. rabbitmq writes
/// multiple files (`bougie@localhost.log`, `*_upgrade.log`, etc.)
/// — we glob the whole dir.
fn dump_rabbitmq_log(env: &TestEnv, label: &str) {
    let dir = env.home_path().join("state/services/rabbitmq/4.2.6/log");
    eprintln!("\n===== rabbitmq logs [{label}] @ {} =====", dir.display());
    let Ok(entries) = fs::read_dir(&dir) else {
        eprintln!("(no log dir yet)");
        eprintln!("===== end rabbitmq logs =====\n");
        return;
    };
    for entry in entries.flatten() {
        let p = entry.path();
        match fs::read_to_string(&p) {
            Ok(s) => {
                let tail = if s.len() > 8 * 1024 {
                    &s[s.len() - 8 * 1024..]
                } else {
                    &s[..]
                };
                eprintln!("--- {} ---", p.display());
                eprintln!("{tail}");
            }
            Err(e) => eprintln!("--- {} (read failed: {e}) ---", p.display()),
        }
    }
    eprintln!("===== end rabbitmq logs =====\n");
}

fn wait_for_tcp(addr: &str, timeout: Duration) -> bool {
    use std::net::TcpStream;
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if TcpStream::connect_timeout(&addr.parse().expect("addr"), Duration::from_millis(250))
            .is_ok()
        {
            return true;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    false
}

/// A rabbitmq-server running *outside* bougied's supervision, on the
/// same node name, cookie and store the daemon uses — what a broker that
/// outlived its daemon looks like from a fresh bougied's point of view.
/// Killed on drop so a failing assertion can't leave it holding the node
/// name for the rest of the suite.
struct UnsupervisedBroker {
    child: std::process::Child,
    bougie_home: std::path::PathBuf,
    /// Where its AMQP listener landed — a free port picked by the test,
    /// since the catalog default may belong to a broker outside the suite.
    port: u16,
}

impl Drop for UnsupervisedBroker {
    fn drop(&mut self) {
        // `rabbitmq-server` is a shell script that *forks* the BEAM
        // rather than exec'ing it, so killing the child alone orphans a
        // broker that still holds the node name — poison for every later
        // test in this binary. Stop the node itself first; the script
        // exits with it. A no-op once the eviction under test has run.
        if matches!(self.child.try_wait(), Ok(None)) {
            let _ = rabbitmqctl_at(&self.bougie_home, &["shutdown"]);
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl UnsupervisedBroker {
    /// Spawn one, with bougied's own env builder so the node name,
    /// cookie HOME and mnesia store are exactly the daemon's.
    fn spawn(env: &TestEnv) -> Self {
        let home = env.home_path();
        let svc = home.join("state/services/rabbitmq/4.2.6");
        // The tree `pre_start` would have created.
        for dir in ["data", "data/mnesia", "data/home", "log", "run", "conf"] {
            fs::create_dir_all(svc.join(dir)).expect("mkdir service state dir");
        }
        let paths = bougie_paths::Paths::new(home.to_path_buf(), home.to_path_buf());
        let log = fs::File::create(home.join("unsupervised-broker.log")).expect("broker log");
        let path = match std::env::var("PATH") {
            Ok(p) if !p.is_empty() => format!("/usr/bin:/bin:{p}"),
            _ => "/usr/bin:/bin".to_string(),
        };
        let port = free_port();
        let child = Command::new(home.join("store/rabbitmq-4.2.6/sbin/rabbitmq-server"))
            .env_clear()
            .env("HOME", svc.join("data/home"))
            .env("PATH", path)
            .envs(bougie_daemon::daemon::provisioners::rabbitmq::rabbitmq_env(
                &paths, port,
            ))
            .stdout(log.try_clone().expect("dup broker log"))
            .stderr(log)
            .spawn()
            .expect("spawning an unsupervised rabbitmq-server");
        Self {
            child,
            bougie_home: home.to_path_buf(),
            port,
        }
    }

    /// Did it exit within `timeout`? Reaps it if so.
    fn wait_for_exit(&mut self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            match self.child.try_wait() {
                Ok(Some(_)) => return true,
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(200));
                }
                _ => return false,
            }
        }
    }
}

/// Run `rabbitmqctl <args>` against the tenant broker. Used by tests
/// to introspect state the daemon doesn't surface via IPC (vhost
/// listing, user listing, etc.). Returns (stdout, stderr) and exit.
fn rabbitmqctl(env: &TestEnv, args: &[&str]) -> (i32, String, String) {
    rabbitmqctl_at(env.home_path(), args)
}

/// [`rabbitmqctl`] against a `BOUGIE_HOME` by path, for callers that
/// have no [`TestEnv`] to hand (the drop guard below).
fn rabbitmqctl_at(bougie_home: &Path, args: &[&str]) -> (i32, String, String) {
    let ctl = bougie_home.join("store/rabbitmq-4.2.6/sbin/rabbitmqctl");
    let home = bougie_home.join("state/services/rabbitmq/4.2.6/data/home");
    // Mirror bougied's ctl PATH (see rabbitmq.rs `ctl_path`): FHS
    // defaults plus the inherited PATH, so rabbitmqctl's shell launchers
    // find coreutils (`dirname`, `readlink`) on non-FHS hosts like
    // NixOS, where `/usr/bin:/bin` alone would break them.
    let path = match std::env::var("PATH") {
        Ok(p) if !p.is_empty() => format!("/usr/bin:/bin:{p}"),
        _ => "/usr/bin:/bin".to_string(),
    };
    let out = Command::new(&ctl)
        .args(args)
        // Mirror bougied's env so we hit the same node.
        .env_clear()
        .env("HOME", &home)
        .env("PATH", &path)
        .env("RABBITMQ_NODENAME", "bougie@localhost")
        .env("RABBITMQ_NODE_IP_ADDRESS", "127.0.0.1")
        .env("RABBITMQ_NODE_PORT", "5672")
        .env(
            "RABBITMQ_BASE",
            bougie_home.join("state/services/rabbitmq/4.2.6/data"),
        )
        .env(
            "RABBITMQ_MNESIA_BASE",
            bougie_home.join("state/services/rabbitmq/4.2.6/data/mnesia"),
        )
        .env(
            "RABBITMQ_LOG_BASE",
            bougie_home.join("state/services/rabbitmq/4.2.6/log"),
        )
        .output()
        .expect("rabbitmqctl");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).to_string(),
        String::from_utf8_lossy(&out.stderr).to_string(),
    )
}

#[test]
fn up_starts_rabbitmq_and_provisions_vhost_user() {
    if should_skip() {
        eprintln!("skipping: BOUGIE_SKIP_REAL_RABBITMQ set");
        return;
    }
    let _guard = rabbitmq_test_lock();
    let env = TestEnv::new();
    rabbitmq_fixture::install_into(env.home_path());
    let proj = project_with_composer("acme/blog");

    env.bougie()
        .args(["service", "add", "rabbitmq"])
        .current_dir(proj.path())
        .timeout(STEP_TIMEOUT)
        .assert()
        .success();
    services_up_or_dump(&env, proj.path(), &["--format", "json-v1"]);

    if !wait_for_broker(&env, Duration::from_mins(2)) {
        dump_rabbitmq_log(&env, "wait_for_tcp timeout");
        panic!("rabbitmq AMQP listener never bound 127.0.0.1:5672");
    }

    // Tenant ledger captures the vhost + username + a hex password.
    let tenants = env
        .home_path()
        .join("state/services/rabbitmq/4.2.6/tenants.json");
    let ledger = fs::read_to_string(&tenants).expect("tenants.json");
    let line = ledger.lines().next().expect("at least one tenant line");
    let t: serde_json::Value = serde_json::from_str(line).unwrap();
    assert_eq!(t["tenant"], "acme_blog");
    assert_eq!(t["alloc"]["vhost"], "acme_blog");
    assert_eq!(t["alloc"]["username"], "acme_blog");
    // The node the vhost/user were created on: a row missing this (or
    // naming another node) is re-provisioned rather than trusted.
    assert_eq!(t["alloc"]["node"], "bougie@localhost");
    let pw = t["secrets"]["password"].as_str().expect("password");
    assert_eq!(pw.len(), 48, "expected 48-char hex password, got {pw:?}");

    // The live broker confirms the vhost is there.
    let (code, stdout, stderr) = rabbitmqctl(&env, &["list_vhosts", "--no-table-headers"]);
    assert_eq!(code, 0, "list_vhosts stderr:\n{stderr}");
    assert!(
        stdout.contains("acme_blog"),
        "expected vhost in list_vhosts output:\n{stdout}"
    );

    // And the user — but the username's only meaningful in
    // combination with permissions, so check those instead.
    let (code, stdout, stderr) = rabbitmqctl(
        &env,
        &["list_user_permissions", "acme_blog", "--no-table-headers"],
    );
    assert_eq!(code, 0, "list_user_permissions stderr:\n{stderr}");
    assert!(
        stdout.contains("acme_blog"),
        "expected user permissions row:\n{stdout}"
    );

    stop_daemon(&env);
}

#[test]
fn down_purge_drops_vhost_and_user() {
    if should_skip() {
        eprintln!("skipping: BOUGIE_SKIP_REAL_RABBITMQ set");
        return;
    }
    let _guard = rabbitmq_test_lock();
    let env = TestEnv::new();
    rabbitmq_fixture::install_into(env.home_path());
    let proj = project_with_composer("acme/blog");
    env.bougie()
        .args(["service", "add", "rabbitmq"])
        .current_dir(proj.path())
        .timeout(STEP_TIMEOUT)
        .assert()
        .success();
    services_up_or_dump(&env, proj.path(), &[]);
    if !wait_for_broker(&env, Duration::from_mins(2)) {
        dump_rabbitmq_log(&env, "wait_for_tcp timeout");
        panic!("rabbitmq listener never bound");
    }

    env.bougie()
        .args(["service", "down", "--purge"])
        .current_dir(proj.path())
        .timeout(STEP_TIMEOUT)
        .assert()
        .success();

    // Tenant ledger should be empty (or missing).
    let tenants = env
        .home_path()
        .join("state/services/rabbitmq/4.2.6/tenants.json");
    let ledger = fs::read_to_string(&tenants).unwrap_or_default();
    assert!(
        ledger.lines().all(|l| l.trim().is_empty()),
        "tenants ledger should be empty after --purge; was\n{ledger}"
    );

    stop_daemon(&env);
}

#[test]
fn bougie_run_exports_rabbitmq_env_vars() {
    if should_skip() {
        eprintln!("skipping: BOUGIE_SKIP_REAL_RABBITMQ set");
        return;
    }
    let _guard = rabbitmq_test_lock();
    let env = TestEnv::new();
    rabbitmq_fixture::install_into(env.home_path());
    let proj = project_with_composer("acme/blog");
    env.bougie()
        .args(["service", "add", "rabbitmq"])
        .current_dir(proj.path())
        .timeout(STEP_TIMEOUT)
        .assert()
        .success();
    services_up_or_dump(&env, proj.path(), &[]);
    if !wait_for_broker(&env, Duration::from_mins(2)) {
        dump_rabbitmq_log(&env, "wait_for_tcp timeout");
        panic!("rabbitmq listener never bound");
    }

    let bougie_bin = cargo_bin("bougie");
    let out = Command::new(&bougie_bin)
        .args(["run", "--no-sync", "--", "/usr/bin/env"])
        .current_dir(proj.path())
        .env("BOUGIE_HOME", env.home_path())
        .env("BOUGIE_CACHE", env.cache_path())
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("BOUGIE_SERVICE_RABBITMQ_URL=amqp://acme_blog:"),
        "missing or malformed URL var; env was:\n{stdout}"
    );
    // The port the daemon recorded, not the catalog default: with the
    // default already taken on this host, the broker relocates and the
    // injected DSN has to follow it there.
    let port = recorded_port(&env).expect("endpoint.json primary port");
    assert!(
        stdout.contains(&format!("@127.0.0.1:{port}/acme_blog")),
        "URL missing authority/vhost (expected port {port}); env was:\n{stdout}"
    );
    assert!(
        stdout.contains("BOUGIE_SERVICE_RABBITMQ_VHOST=acme_blog"),
        "missing VHOST var; env was:\n{stdout}"
    );
    assert!(
        stdout.contains("BOUGIE_SERVICE_RABBITMQ_USER=acme_blog"),
        "missing USER var; env was:\n{stdout}"
    );
    assert!(
        stdout.contains("BOUGIE_SERVICE_RABBITMQ_PASSWORD="),
        "missing PASSWORD var; env was:\n{stdout}"
    );

    stop_daemon(&env);
}

/// Regression for cresset-tools/bougie#31.
///
/// `bougie down` without `--purge` removes the tenant from the ledger
/// but leaves the user/vhost in rabbitmq's mnesia store (matches the
/// survives-down semantics of mariadb / opensearch). A subsequent
/// `bougie up` re-provisions; `add_user` errors "already exists", so the
/// provisioner chains `change_password` to re-assert the ledger's
/// password on the persisted user — keeping the broker and the ledger in
/// sync (otherwise AMQP login via `BOUGIE_SERVICE_RABBITMQ_PASSWORD`
/// returns `ACCESS_REFUSED`).
///
/// The password is now *derived* (deterministic), so re-up yields the
/// **same** password — this test asserts that stability (a captured
/// env.php keeps working) and that `change_password` re-asserts it on the
/// broker (healing any drift, e.g. an older random-password install).
#[test]
fn re_up_after_plain_down_resyncs_password_to_broker() {
    if should_skip() {
        eprintln!("skipping: BOUGIE_SKIP_REAL_RABBITMQ set");
        return;
    }
    let _guard = rabbitmq_test_lock();
    let env = TestEnv::new();
    rabbitmq_fixture::install_into(env.home_path());
    let proj = project_with_composer("acme/blog");

    env.bougie()
        .args(["service", "add", "rabbitmq"])
        .current_dir(proj.path())
        .timeout(STEP_TIMEOUT)
        .assert()
        .success();
    services_up_or_dump(&env, proj.path(), &[]);
    if !wait_for_broker(&env, Duration::from_mins(2)) {
        dump_rabbitmq_log(&env, "wait_for_tcp timeout (first up)");
        panic!("rabbitmq listener never bound");
    }

    // Capture password A for sanity-checking the change later.
    let tenants_path = env
        .home_path()
        .join("state/services/rabbitmq/4.2.6/tenants.json");
    let first_ledger = fs::read_to_string(&tenants_path).expect("first tenants.json");
    let first_line = first_ledger.lines().next().expect("first tenant line");
    let pw_a =
        serde_json::from_str::<serde_json::Value>(first_line).unwrap()["secrets"]["password"]
            .as_str()
            .unwrap()
            .to_owned();

    // `bougie down` (no --purge) wipes the ledger and stops the
    // broker (last-tenant-out shuts the global service down). The
    // user/vhost are persisted in mnesia and survive — that's the
    // precondition that triggers the bug. We can't query the broker
    // while it's stopped; we verify the survives-down invariant
    // implicitly via the duplicate `add_user` failure path that the
    // re-up below exercises.
    env.bougie()
        .args(["service", "down"])
        .current_dir(proj.path())
        .timeout(STEP_TIMEOUT)
        .assert()
        .success();

    // Re-`up`: provision sees no ledger row, *re-derives* the same
    // password, calls add_user → duplicate (user persisted in mnesia) →
    // chains change_password to re-assert it on the broker.
    services_up_or_dump(&env, proj.path(), &[]);
    if !wait_for_broker(&env, Duration::from_mins(2)) {
        dump_rabbitmq_log(&env, "wait_for_tcp timeout (second up)");
        panic!("rabbitmq listener never bound on re-up");
    }
    // Sanity-check the survives-down invariant now that the broker
    // is reachable again: the user should still be present (we
    // didn't `--purge`).
    let (code, stdout, _) = rabbitmqctl(&env, &["list_users", "--no-table-headers"]);
    assert_eq!(code, 0);
    assert!(
        stdout.contains("acme_blog"),
        "user should survive a non-purge down; list_users was:\n{stdout}"
    );

    let second_ledger = fs::read_to_string(&tenants_path).expect("second tenants.json");
    let second_line = second_ledger.lines().next().expect("second tenant line");
    let pw_b =
        serde_json::from_str::<serde_json::Value>(second_line).unwrap()["secrets"]["password"]
            .as_str()
            .unwrap()
            .to_owned();
    // The password is derived, so re-provisioning yields the *same*
    // value — this is the guarantee that a captured app/etc/env.php keeps
    // authenticating after a down/up cycle.
    assert_eq!(
        pw_a, pw_b,
        "derived password must be stable across a non-purge down/up so env.php stays valid"
    );

    // And the broker authenticates it after re-up: `change_password`
    // re-asserted the derived password on the persisted mnesia user
    // (this is what heals an older random-password install).
    let (code, stdout, stderr) = rabbitmqctl(&env, &["authenticate_user", "acme_blog", &pw_b]);
    assert_eq!(
        code, 0,
        "broker should accept the ledger's password after re-up; stdout=`{stdout}` stderr=`{stderr}`"
    );

    stop_daemon(&env);
}

/// The `rabbit@localhost` → `bougie@localhost` node rename.
///
/// rabbitmq keys its metadata store by node name, so the renamed node
/// boots with an empty store: the vhost and user a pre-rename ledger row
/// names are simply not there. Trusting that row would hand the project
/// credentials for a vhost that doesn't exist and every AMQP login would
/// fail with `ACCESS_REFUSED`, so `up` must re-provision instead —
/// under the row's *recorded* tenant name, and (thanks to the derived
/// password) with the same password a captured env.php already holds.
///
/// Staged by doctoring a real install into its pre-rename shape: strip
/// `alloc.node` from the ledger row and wipe the mnesia store, which is
/// exactly the state the rename produces.
#[test]
fn up_reprovisions_a_tenant_row_from_the_previous_node() {
    if should_skip() {
        eprintln!("skipping: BOUGIE_SKIP_REAL_RABBITMQ set");
        return;
    }
    let _guard = rabbitmq_test_lock();
    let env = TestEnv::new();
    rabbitmq_fixture::install_into(env.home_path());
    let proj = project_with_composer("acme/blog");

    env.bougie()
        .args(["service", "add", "rabbitmq"])
        .current_dir(proj.path())
        .timeout(STEP_TIMEOUT)
        .assert()
        .success();
    services_up_or_dump(&env, proj.path(), &[]);
    if !wait_for_broker(&env, Duration::from_mins(2)) {
        dump_rabbitmq_log(&env, "wait_for_tcp timeout (first up)");
        panic!("rabbitmq listener never bound");
    }

    let tenants_path = env
        .home_path()
        .join("state/services/rabbitmq/4.2.6/tenants.json");
    let ledger = fs::read_to_string(&tenants_path).expect("tenants.json");
    let mut row: serde_json::Value =
        serde_json::from_str(ledger.lines().next().expect("a tenant line")).unwrap();
    let pw_before = row["secrets"]["password"].as_str().unwrap().to_owned();

    stop_daemon(&env);

    // Age the row: pre-rename provisioning stamped no node.
    row["alloc"]
        .as_object_mut()
        .expect("alloc object")
        .remove("node")
        .expect("row should have been stamped with its node");
    fs::write(&tenants_path, format!("{row}\n")).expect("rewriting tenants.json");
    // And take the store with it — a renamed node opens a fresh one.
    let mnesia = env
        .home_path()
        .join("state/services/rabbitmq/4.2.6/data/mnesia");
    fs::remove_dir_all(&mnesia).expect("wiping the mnesia store");

    services_up_or_dump(&env, proj.path(), &[]);
    if !wait_for_broker(&env, Duration::from_mins(2)) {
        dump_rabbitmq_log(&env, "wait_for_tcp timeout (up after rename)");
        panic!("rabbitmq listener never bound after the node rename");
    }

    // The vhost is back on the new node...
    let (code, stdout, stderr) = rabbitmqctl(&env, &["list_vhosts", "--no-table-headers"]);
    assert_eq!(code, 0, "list_vhosts stderr:\n{stderr}");
    assert!(
        stdout.contains("acme_blog"),
        "vhost should be re-created on the renamed node:\n{stdout}"
    );

    // ...the row is stamped with it, so this happens exactly once...
    let ledger = fs::read_to_string(&tenants_path).expect("tenants.json after re-provision");
    assert_eq!(ledger.lines().count(), 1, "one row per project:\n{ledger}");
    let row: serde_json::Value = serde_json::from_str(ledger.lines().next().unwrap()).unwrap();
    assert_eq!(row["tenant"], "acme_blog");
    assert_eq!(row["alloc"]["vhost"], "acme_blog");
    assert_eq!(row["alloc"]["node"], "bougie@localhost");

    // ...and the password an installed env.php holds still authenticates.
    assert_eq!(
        row["secrets"]["password"].as_str().unwrap(),
        pw_before,
        "derived password must survive the node rename"
    );
    let (code, stdout, stderr) = rabbitmqctl(&env, &["authenticate_user", "acme_blog", &pw_before]);
    assert_eq!(
        code, 0,
        "broker should accept the pre-rename password; stdout=`{stdout}` stderr=`{stderr}`"
    );

    stop_daemon(&env);
}

/// A broker that outlived its daemon must not wedge the service forever.
///
/// Erlang node names live in `epmd`, which is host-wide and outlives any
/// one broker, so an escapee keeps `bougie@localhost` claimed and every
/// respawn dies in prelaunch with `duplicate_node_name` — through the
/// whole restart-backoff ladder, since nothing in that loop removes the
/// squatter. The supervisor now evicts it before spawning, but only
/// after proving it's ours (it answers to this install's Erlang cookie).
///
/// Staged with a rabbitmq-server started outside bougied on the daemon's
/// own env: to a fresh bougied, that is exactly an escaped broker.
#[test]
fn up_evicts_an_unsupervised_broker_holding_the_node_name() {
    if should_skip() {
        eprintln!("skipping: BOUGIE_SKIP_REAL_RABBITMQ set");
        return;
    }
    let _guard = rabbitmq_test_lock();
    let env = TestEnv::new();
    rabbitmq_fixture::install_into(env.home_path());
    let proj = project_with_composer("acme/blog");

    let mut squatter = UnsupervisedBroker::spawn(&env);
    if !wait_for_tcp(
        &format!("127.0.0.1:{}", squatter.port),
        Duration::from_mins(2),
    ) {
        dump_rabbitmq_log(&env, "unsupervised broker never bound");
        panic!(
            "the unsupervised broker never bound 127.0.0.1:{}",
            squatter.port
        );
    }

    // bougied knows nothing about that process — no child handle, no
    // state slot. Bringing the service up has to work anyway.
    env.bougie()
        .args(["service", "add", "rabbitmq"])
        .current_dir(proj.path())
        .timeout(STEP_TIMEOUT)
        .assert()
        .success();
    services_up_or_dump(&env, proj.path(), &[]);

    // Evicted, not left running alongside — two brokers can't share the
    // node name, so a still-live squatter would mean we never started.
    assert!(
        squatter.wait_for_exit(Duration::from_secs(30)),
        "the unsupervised broker should have been shut down before the respawn"
    );

    // And what's serving now is bougied's own node: the tenant landed on
    // it, which only works over a `rabbitmqctl` round trip to the broker
    // the daemon just started.
    let (code, stdout, stderr) = rabbitmqctl(&env, &["list_vhosts", "--no-table-headers"]);
    assert_eq!(code, 0, "list_vhosts stderr:\n{stderr}");
    assert!(
        stdout.contains("acme_blog"),
        "expected the tenant vhost on the respawned broker:\n{stdout}"
    );

    stop_daemon(&env);
}
