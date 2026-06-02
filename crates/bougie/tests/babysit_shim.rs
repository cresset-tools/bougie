#![allow(unsafe_code)]
//! End-to-end tests for the `bougie-babysit` argv[0] role.
//!
//! Each test spawns the bougie binary through a symlink so
//! `role_from_argv0` routes into `babysit::run`. A socketpair is
//! handed to the child as fd 3 (dup2'd in `pre_exec`). The parent end
//! stays in this process and is used both to read the `pgid=` line
//! the babysit emits and to simulate "bougied died" by dropping it.

use std::io::Read;
use std::os::fd::AsRawFd;
use std::os::unix::fs::{symlink, PermissionsExt};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};
use tempfile::TempDir;

fn bougie_bin() -> PathBuf {
    assert_cmd::cargo::cargo_bin("bougie")
}

/// Create a symlink named `bougie-babysit` -> bougie binary.
fn babysit_shim(td: &TempDir) -> PathBuf {
    let link = td.path().join("bougie-babysit");
    symlink(bougie_bin(), &link).expect("symlinking bougie-babysit -> bougie");
    link
}

/// Spawn the babysit shim with one end of a `socketpair` dup2'd onto
/// fd 3 in the child. Returns (child, `parent_end_of_socketpair`).
fn spawn_babysit(shim: &PathBuf, grace_secs: u64, exec: &str, exec_args: &[&str]) -> (std::process::Child, UnixStream) {
    let (parent, child_end) = UnixStream::pair().expect("socketpair");
    let child_raw = child_end.as_raw_fd();

    let mut cmd = Command::new(shim);
    cmd.arg("--service-name")
        .arg("test")
        .arg("--grace-secs")
        .arg(grace_secs.to_string())
        .arg("--control-fd")
        .arg("3")
        .arg("--")
        .arg(exec)
        .args(exec_args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit());

    // SAFETY: pre_exec runs after fork, before exec. We only call
    // dup2, which is async-signal-safe.
    unsafe {
        cmd.pre_exec(move || {
            // dup2 also clears CLOEXEC on the destination, so fd 3
            // survives exec.
            let rc = libc_dup2(child_raw, 3);
            if rc < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }

    let child = cmd.spawn().expect("spawn babysit");
    // Parent doesn't need the child end any more — close it so the
    // peer side stays held only by the child.
    drop(child_end);
    (child, parent)
}

/// Like [`spawn_babysit`] but with a `--sidecar` (started in the same
/// group, ahead of the main service). No `--sidecar-ready-port` so the
/// main service starts immediately (the helper has nothing to wait on).
fn spawn_babysit_sidecar(
    shim: &PathBuf,
    grace_secs: u64,
    sidecar: &str,
    exec: &str,
    exec_args: &[&str],
) -> (std::process::Child, UnixStream) {
    let (parent, child_end) = UnixStream::pair().expect("socketpair");
    let child_raw = child_end.as_raw_fd();

    let mut cmd = Command::new(shim);
    cmd.arg("--service-name")
        .arg("test")
        .arg("--grace-secs")
        .arg(grace_secs.to_string())
        .arg("--control-fd")
        .arg("3")
        .arg("--sidecar")
        .arg(sidecar)
        .arg("--")
        .arg(exec)
        .args(exec_args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit());

    // SAFETY: pre_exec runs after fork, before exec; dup2 is async-signal-safe.
    unsafe {
        cmd.pre_exec(move || {
            let rc = libc_dup2(child_raw, 3);
            if rc < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }

    let child = cmd.spawn().expect("spawn babysit");
    drop(child_end);
    (child, parent)
}

/// Like [`spawn_babysit_sidecar`] but also passes `--sidecar-ready-port`,
/// so babysit waits for `port` before starting the service and re-probes
/// it if the sidecar later exits.
fn spawn_babysit_sidecar_port(
    shim: &PathBuf,
    grace_secs: u64,
    sidecar: &str,
    ready_port: u16,
    exec: &str,
    exec_args: &[&str],
) -> (std::process::Child, UnixStream) {
    let (parent, child_end) = UnixStream::pair().expect("socketpair");
    let child_raw = child_end.as_raw_fd();

    let mut cmd = Command::new(shim);
    cmd.arg("--service-name")
        .arg("test")
        .arg("--grace-secs")
        .arg(grace_secs.to_string())
        .arg("--control-fd")
        .arg("3")
        .arg("--sidecar")
        .arg(sidecar)
        .arg("--sidecar-ready-port")
        .arg(ready_port.to_string())
        .arg("--")
        .arg(exec)
        .args(exec_args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit());

    // SAFETY: pre_exec runs after fork, before exec; dup2 is async-signal-safe.
    unsafe {
        cmd.pre_exec(move || {
            let rc = libc_dup2(child_raw, 3);
            if rc < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }

    let child = cmd.spawn().expect("spawn babysit");
    drop(child_end);
    (child, parent)
}

/// dup2 via a tiny extern; avoids pulling libc as a dev-dep when we
/// already need a single syscall.
#[allow(non_snake_case)]
fn libc_dup2(src: i32, dst: i32) -> i32 {
    unsafe extern "C" {
        fn dup2(oldfd: i32, newfd: i32) -> i32;
    }
    unsafe { dup2(src, dst) }
}

/// Read one `\n`-terminated line from the socket WITHOUT a `BufReader`.
/// A `BufReader` would issue an 8 KiB read on the underlying fd and
/// buffer everything it got — when the function returns and the
/// `BufReader` is dropped, any bytes past the first newline are
/// silently discarded. Under GHA load the babysit frequently writes
/// `pgid=…\nexited=…\n` back-to-back before this thread is scheduled
/// to read, so a BufReader-based reader would consume both lines in
/// one syscall and lose the `exited=` line — which is exactly the
/// `"expected exited= line, got """` flake reported in issue #34.
/// Read byte-by-byte instead so the kernel buffer keeps any
/// subsequent lines visible to the next reader on this socket.
fn read_pgid_line(sock: &mut UnixStream) -> i32 {
    sock.set_read_timeout(Some(Duration::from_secs(15))).unwrap();
    let mut line = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        sock.read_exact(&mut byte).expect("reading pgid line");
        if byte[0] == b'\n' {
            break;
        }
        line.push(byte[0]);
    }
    let line = std::str::from_utf8(&line).expect("pgid line is utf-8");
    
    line
        .strip_prefix("pgid=")
        .unwrap_or_else(|| panic!("expected pgid= line, got {line:?}"))
        .parse::<i32>()
        .expect("parsing pgid")
}

/// Returns true iff the given process group has any live members.
fn pgrp_alive(pgid: i32) -> bool {
    // `kill(-pgid, 0)` probes existence: returns 0 if the group has
    // members, ESRCH otherwise.
    kill0(-pgid) == 0
}

fn kill0(target: i32) -> i32 {
    unsafe extern "C" {
        fn kill(pid: i32, sig: i32) -> i32;
    }
    unsafe { kill(target, 0) }
}

fn kill_pid(pid: i32, sig: i32) -> i32 {
    unsafe extern "C" {
        fn kill(pid: i32, sig: i32) -> i32;
    }
    unsafe { kill(pid, sig) }
}

fn wait_until<F: FnMut() -> bool>(mut f: F, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if f() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    f()
}

#[test]
fn babysit_reports_pgid_then_exit_on_clean_service_exit() {
    let td = TempDir::new().unwrap();
    let shim = babysit_shim(&td);
    // `/bin/sh -c 'exit 0'` exits immediately.
    let (mut child, mut sock) = spawn_babysit(&shim, 5, "/bin/sh", &["-c", "exit 0"]);

    let pgid = read_pgid_line(&mut sock);
    assert!(pgid > 0, "pgid should be positive, got {pgid}");

    // Continue reading the socket: babysit should also report exit.
    // 15s read timeout — generous enough that a heavily contended
    // GHA runner doesn't EOF us before the babysit's tokio runtime
    // sees child.wait() return and emits the `exited=` line. (issue #34)
    let mut rest = String::new();
    sock.set_read_timeout(Some(Duration::from_secs(15))).unwrap();
    sock.read_to_string(&mut rest).ok();
    assert!(
        rest.contains("exited="),
        "expected exited= line, got {rest:?}"
    );

    let status = child.wait().expect("waiting on babysit");
    assert!(status.success(), "babysit should exit 0 on clean service exit, got {status:?}");
}

#[test]
fn babysit_kills_group_on_socket_close() {
    let td = TempDir::new().unwrap();
    let shim = babysit_shim(&td);
    // Forking fake service: background a long sleep, then itself
    // exits. The backgrounded sleep stays in the same process group
    // because we don't `setpgid` again. The babysit's killpg should
    // catch BOTH the shell parent and the orphaned `sleep`.
    //
    // We `exec sleep` so the shell stays alive (otherwise its child
    // becomes the entire group and the shell exiting reaps too early).
    let (mut child, mut sock) =
        spawn_babysit(&shim, 2, "/bin/sh", &["-c", "exec /bin/sleep 60"]);

    let pgid = read_pgid_line(&mut sock);
    assert!(pgrp_alive(pgid), "group should be alive after spawn");

    // Drop the parent socket — babysit should see EOF and tear down.
    drop(sock);

    // 30s deadline: grace_secs is 2, so on any healthy host the group
    // is dead in well under a second. The generous bound is for
    // contended CI runners where the babysit's tokio thread may not
    // be scheduled promptly between EOF detection and killpg. Still
    // bounds correctness — a real regression (deadlocked babysit,
    // missing killpg) won't sneak past 30s. (issue #34)
    let died = wait_until(|| !pgrp_alive(pgid), Duration::from_secs(30));
    assert!(died, "process group {pgid} still alive after socket close + grace");

    let status = child.wait().expect("waiting on babysit");
    assert!(status.success(), "babysit should exit 0 after socket-close cleanup, got {status:?}");
}

#[test]
fn babysit_kills_group_on_sigterm() {
    let td = TempDir::new().unwrap();
    let shim = babysit_shim(&td);
    let (mut child, mut sock) =
        spawn_babysit(&shim, 2, "/bin/sh", &["-c", "exec /bin/sleep 60"]);

    let pgid = read_pgid_line(&mut sock);
    assert!(pgrp_alive(pgid));

    // SIGTERM to the babysit. It must proxy to the group.
    let bpid = i32::try_from(child.id()).expect("test pid fits in i32");
    assert_eq!(kill_pid(bpid, 15), 0, "kill(babysit, SIGTERM) failed");

    // 30s deadline for SIGTERM-propagation + group teardown — see the
    // matching comment on the socket-close test above. (issue #34)
    let died = wait_until(|| !pgrp_alive(pgid), Duration::from_secs(30));
    assert!(died, "process group {pgid} still alive after SIGTERM");

    let status = child.wait().expect("waiting on babysit");
    assert!(status.success(), "babysit should exit 0 after SIGTERM cleanup, got {status:?}");
}

#[cfg(target_os = "linux")]
#[test]
fn babysit_sigkill_takes_service_down_via_pdeathsig() {
    let td = TempDir::new().unwrap();
    let shim = babysit_shim(&td);
    // Keep `sock` (the bougied end) alive for the whole test so the
    // socket-EOF cleanup path can't fire — that way the ONLY thing that
    // can kill the service is PR_SET_PDEATHSIG.
    let (mut child, mut sock) =
        spawn_babysit(&shim, 2, "/bin/sh", &["-c", "exec /bin/sleep 60"]);
    let pgid = read_pgid_line(&mut sock);
    assert!(pgrp_alive(pgid), "service group should be alive after spawn");

    // SIGKILL the babysit: none of its cleanup handlers can run (SIGKILL
    // is uncatchable, and we hold `sock` open so there's no EOF to act
    // on even if it could). The service must still die — that's the
    // kernel delivering the parent-death SIGKILL.
    let bpid = i32::try_from(child.id()).expect("test pid fits in i32");
    assert_eq!(kill_pid(bpid, 9), 0, "kill(babysit, SIGKILL) failed");

    let died = wait_until(|| !pgrp_alive(pgid), Duration::from_secs(30));
    assert!(
        died,
        "service group {pgid} survived a babysit SIGKILL — PR_SET_PDEATHSIG didn't fire"
    );

    // `sock` must outlive the assertion above so the test isn't secretly
    // exercising the socket-EOF path instead of pdeathsig.
    drop(sock);
    let _ = child.wait();
}

#[test]
fn babysit_sidecar_runs_in_group_and_is_reaped() {
    // The macOS-correct path: a co-located helper (rabbitmq's epmd) must
    // run in the service's process group so plain `killpg` reaps it —
    // no cgroup, no pdeathsig. Here the sidecar is a tiny no-arg script
    // that execs sleep (babysit passes the sidecar no argv, like epmd).
    let td = TempDir::new().unwrap();
    let shim = babysit_shim(&td);
    let sidecar = td.path().join("sidecar");
    std::fs::write(&sidecar, "#!/bin/sh\nexec /bin/sleep 600\n").unwrap();
    std::fs::set_permissions(&sidecar, std::fs::Permissions::from_mode(0o755)).unwrap();

    let (mut child, mut sock) = spawn_babysit_sidecar(
        &shim,
        2,
        sidecar.to_str().unwrap(),
        "/bin/sh",
        &["-c", "exec /bin/sleep 600"],
    );
    let pgid = read_pgid_line(&mut sock);
    // The sidecar created the group, so pgid == the sidecar's pid: that
    // process is alive and the group has members (sidecar + main).
    assert_eq!(kill0(pgid), 0, "sidecar (group leader {pgid}) should be alive");
    assert!(pgrp_alive(pgid), "group should be alive after spawn");

    // bougied "dies": babysit `killpg`s the whole group — the sidecar
    // included — with no cgroup/pdeathsig in play (the macOS scenario).
    drop(sock);
    let died = wait_until(|| !pgrp_alive(pgid), Duration::from_secs(30));
    assert!(died, "group {pgid} (incl. sidecar) still alive after teardown");
    // `pgrp_alive` going false means the kernel dropped the leader from
    // the *group* — but the leader pid lingers as a zombie (child of
    // babysit) until reaped, and on macOS that reap can trail the group
    // transition by a few ms. Poll for the pid to actually disappear
    // rather than asserting in that window, or this races (the group is
    // gone, but `kill(pid, 0)` still finds the zombie).
    assert!(
        wait_until(|| kill0(pgid) != 0, Duration::from_secs(5)),
        "sidecar pid {pgid} should have been reaped"
    );

    let status = child.wait().expect("waiting on babysit");
    assert!(status.success(), "babysit should exit 0 after cleanup, got {status:?}");
}

#[test]
fn babysit_keeps_service_when_sidecar_exits_but_port_still_served() {
    // Regression (cresset-tools/bougie): a bare `epmd` exits 0 the moment
    // it finds another epmd already bound to its port. babysit must NOT
    // tear the (healthy) service down on that benign exit — it has to
    // re-probe the port and only fail the unit when the port is actually
    // gone. Simulate it: a sidecar that exits immediately, plus a stand-in
    // listener holding the ready-port open so the re-probe sees it served.
    let td = TempDir::new().unwrap();
    let shim = babysit_shim(&td);

    // Stand-in for "another epmd already owns the port": a listener we
    // keep bound for the whole test (bound is enough — connect succeeds
    // off the accept backlog without us calling accept()).
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind ready-port stand-in");
    let port = listener.local_addr().unwrap().port();

    // Sidecar that exits 0 right away, like epmd bowing out.
    let sidecar = td.path().join("sidecar");
    std::fs::write(&sidecar, "#!/bin/sh\nexit 0\n").unwrap();
    std::fs::set_permissions(&sidecar, std::fs::Permissions::from_mode(0o755)).unwrap();

    let (mut child, mut sock) = spawn_babysit_sidecar_port(
        &shim,
        2,
        sidecar.to_str().unwrap(),
        port,
        "/bin/sh",
        &["-c", "exec /bin/sleep 600"],
    );
    let pgid = read_pgid_line(&mut sock);

    // The service must still be running a couple of seconds after the
    // sidecar bowed out: babysit re-probed the port, saw it served, and
    // stayed up. (`wait_until` returns false here iff the group never
    // died within the window — which is exactly what we want.)
    assert!(
        !wait_until(|| !pgrp_alive(pgid), Duration::from_secs(2)),
        "service group {pgid} was torn down despite the ready-port still being served"
    );
    assert!(pgrp_alive(pgid), "service group {pgid} should still be alive");
    assert!(
        child.try_wait().unwrap().is_none(),
        "babysit exited despite a benign sidecar exit"
    );

    // Clean teardown via socket EOF: babysit reaps the group and exits 0.
    drop(sock);
    drop(listener);
    let died = wait_until(|| !pgrp_alive(pgid), Duration::from_secs(30));
    assert!(died, "group {pgid} still alive after teardown");
    let status = child.wait().expect("waiting on babysit");
    assert!(status.success(), "babysit should exit 0 after clean teardown, got {status:?}");
}

#[test]
fn babysit_service_runs_in_its_own_pgid() {
    let td = TempDir::new().unwrap();
    let shim = babysit_shim(&td);
    let (mut child, mut sock) =
        spawn_babysit(&shim, 2, "/bin/sh", &["-c", "exec /bin/sleep 60"]);
    let pgid = read_pgid_line(&mut sock);

    // The reported pgid must NOT equal the babysit's own pid: the
    // service is in a separate group. (Same group would mean killpg
    // from babysit would also signal itself, which is what we want
    // to avoid.)
    let bpid = i32::try_from(child.id()).expect("test pid fits in i32");
    assert_ne!(
        pgid, bpid,
        "service pgid {pgid} should differ from babysit pid {bpid}"
    );

    // Clean up.
    drop(sock);
    let _ = wait_until(|| !pgrp_alive(pgid), Duration::from_secs(30));
    let _ = child.wait();
}
