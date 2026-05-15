#![allow(unsafe_code)]
//! End-to-end tests for the `bougie-babysit` argv[0] role.
//!
//! Each test spawns the bougie binary through a symlink so
//! `role_from_argv0` routes into `babysit::run`. A socketpair is
//! handed to the child as fd 3 (dup2'd in pre_exec). The parent end
//! stays in this process and is used both to read the `pgid=` line
//! the babysit emits and to simulate "bougied died" by dropping it.

use std::io::{BufRead, BufReader, Read};
use std::os::fd::AsRawFd;
use std::os::unix::fs::symlink;
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
/// fd 3 in the child. Returns (child, parent_end_of_socketpair).
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

/// dup2 via a tiny extern; avoids pulling libc as a dev-dep when we
/// already need a single syscall.
#[allow(non_snake_case)]
fn libc_dup2(src: i32, dst: i32) -> i32 {
    unsafe extern "C" {
        fn dup2(oldfd: i32, newfd: i32) -> i32;
    }
    unsafe { dup2(src, dst) }
}

fn read_pgid_line(sock: &mut UnixStream) -> i32 {
    sock.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let mut reader = BufReader::new(sock);
    let mut line = String::new();
    reader.read_line(&mut line).expect("reading pgid line");
    let line = line.trim();
    let pgid = line
        .strip_prefix("pgid=")
        .unwrap_or_else(|| panic!("expected pgid= line, got {line:?}"))
        .parse::<i32>()
        .expect("parsing pgid");
    pgid
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
    let mut rest = String::new();
    sock.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
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

    let died = wait_until(|| !pgrp_alive(pgid), Duration::from_secs(6));
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
    let bpid = child.id() as i32;
    assert_eq!(kill_pid(bpid, 15), 0, "kill(babysit, SIGTERM) failed");

    let died = wait_until(|| !pgrp_alive(pgid), Duration::from_secs(6));
    assert!(died, "process group {pgid} still alive after SIGTERM");

    let status = child.wait().expect("waiting on babysit");
    assert!(status.success(), "babysit should exit 0 after SIGTERM cleanup, got {status:?}");
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
    let bpid = child.id() as i32;
    assert_ne!(
        pgid, bpid,
        "service pgid {pgid} should differ from babysit pid {bpid}"
    );

    // Clean up.
    drop(sock);
    let _ = wait_until(|| !pgrp_alive(pgid), Duration::from_secs(6));
    let _ = child.wait();
}
