//! Per-service babysitter shim. Bougied spawns this as
//! `current_exe()` with `argv[0] = "bougie-babysit"`; the `bougie`
//! binary's argv[0] shim routes here.
//!
//! Owns one supervised service: puts it in its own process group via
//! `setpgid(0, 0)`, then watches three signals — service exit, EOF on
//! the socketpair shared with bougied, or SIGTERM — and kills the
//! whole group when any of them fires. Bougied keeps the parent end
//! of the socketpair alive for the babysit's lifetime, so a bougied
//! crash auto-triggers a group-cleanup with zero on-disk state.
//!
//! Wire protocol (babysit → bougied, line-oriented over the
//! socketpair):
//!
//! - `pgid=<n>\n`  — emitted exactly once, right after spawn.
//! - `exited=<status>\n` — emitted on service self-exit.
//!
//! Bougied is not expected to write; the socket is one-way for our
//! purposes. It simply holds the peer end open until it exits.

use eyre::{eyre, Result, WrapErr};
use std::ffi::OsString;
use std::os::fd::FromRawFd;
use std::process::{ExitCode, Stdio};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[derive(Debug)]
struct Config {
    service_name: String,
    grace_secs: u64,
    control_fd: i32,
    /// Optional helper started in the service's process group *before*
    /// the main service — e.g. Erlang's `epmd` for rabbitmq. Started
    /// in-group (not daemonized) so the existing `killpg` teardown reaps
    /// it on every platform — the macOS-correct fix for the `epmd`
    /// escapee, where there's no cgroup backstop. No argv: helpers that
    /// need flags aren't in scope yet.
    sidecar: Option<OsString>,
    /// When set, wait for this loopback TCP port to accept before
    /// starting the main service, so the service finds the sidecar ready
    /// (e.g. rabbitmq must find epmd listening or it spawns its own
    /// daemonized one).
    sidecar_ready_port: Option<u16>,
    exec: OsString,
    argv: Vec<OsString>,
}

/// How long to wait for a `--sidecar-ready-port` to come up before
/// starting the main service anyway (best-effort).
const SIDECAR_READY_TIMEOUT: Duration = Duration::from_secs(10);

/// When the sidecar exits, how long to keep probing the ready-port to
/// decide whether the exit was benign (the port is still served — e.g.
/// `epmd` found another epmd already bound and bowed out) or fatal (the
/// helper the service depends on is genuinely gone). Short: a live port
/// answers on the first probe; a dead one refuses immediately, so this
/// only adds latency on the actual-failure path.
const SIDECAR_PORT_RECHECK: Duration = Duration::from_secs(1);

pub fn run(args: Vec<OsString>) -> Result<ExitCode> {
    let cfg = parse_args(args)?;
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .thread_name("bougie-babysit")
        .build()
        .wrap_err("building babysit tokio runtime")?;
    rt.block_on(serve(cfg))
}

fn parse_args(args: Vec<OsString>) -> Result<Config> {
    let mut service_name: Option<String> = None;
    let mut grace_secs: Option<u64> = None;
    let mut control_fd: Option<i32> = None;
    let mut sidecar: Option<OsString> = None;
    let mut sidecar_ready_port: Option<u16> = None;
    let mut rest: Option<Vec<OsString>> = None;

    let mut iter = args.into_iter();
    while let Some(a) = iter.next() {
        if a == "--" {
            rest = Some(iter.collect());
            break;
        }
        let key = a.to_string_lossy().into_owned();
        if !key.starts_with("--") {
            return Err(eyre!(
                "expected a `--flag` or `--` separator, got `{key}`"
            ));
        }
        let val = iter
            .next()
            .ok_or_else(|| eyre!("flag {key} requires a value"))?;
        match key.as_str() {
            "--service-name" => service_name = Some(val.to_string_lossy().into_owned()),
            "--grace-secs" => {
                grace_secs = Some(
                    val.to_string_lossy()
                        .parse()
                        .map_err(|e| eyre!("--grace-secs: {e}"))?,
                );
            }
            "--control-fd" => {
                control_fd = Some(
                    val.to_string_lossy()
                        .parse()
                        .map_err(|e| eyre!("--control-fd: {e}"))?,
                );
            }
            "--sidecar" => sidecar = Some(val),
            "--sidecar-ready-port" => {
                sidecar_ready_port = Some(
                    val.to_string_lossy()
                        .parse()
                        .map_err(|e| eyre!("--sidecar-ready-port: {e}"))?,
                );
            }
            _ => return Err(eyre!("unknown flag: {key}")),
        }
    }

    let mut rest = rest
        .ok_or_else(|| eyre!("missing `--` separator before service argv"))?
        .into_iter();
    let exec = rest.next().ok_or_else(|| eyre!("missing service exec"))?;
    let argv: Vec<OsString> = rest.collect();

    Ok(Config {
        service_name: service_name.ok_or_else(|| eyre!("missing --service-name"))?,
        grace_secs: grace_secs.ok_or_else(|| eyre!("missing --grace-secs"))?,
        control_fd: control_fd.ok_or_else(|| eyre!("missing --control-fd"))?,
        sidecar,
        sidecar_ready_port,
        exec,
        argv,
    })
}

async fn serve(cfg: Config) -> Result<ExitCode> {
    // Wrap the inherited fd. Bougied's pre_exec dup2's the child end
    // of the socketpair onto this fd before exec'ing us.
    //
    // SAFETY: bougied guarantees `control_fd` is open and unique to
    // this process; we take ownership via FromRawFd and never share it.
    #[allow(unsafe_code)]
    let control = unsafe { std::os::unix::net::UnixStream::from_raw_fd(cfg.control_fd) };
    control
        .set_nonblocking(true)
        .wrap_err("setting control fd non-blocking")?;
    let mut control = tokio::net::UnixStream::from_std(control)
        .wrap_err("wrapping control fd as tokio stream")?;

    // Install the SIGTERM handler BEFORE spawning the service. Bougied
    // (or a test harness) is free to send SIGTERM at any point after
    // we report `pgid=`; if the handler isn't yet installed, SIGTERM's
    // default disposition takes the babysit down without cleanup and
    // leaves the service running as an orphan in its own pgrp. This
    // race was the root cause of the `babysit_kills_group_on_sigterm`
    // flake reported in issue #34.
    let mut sigterm =
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .wrap_err("installing SIGTERM handler")?;
    // Also catch SIGINT and treat it exactly like SIGTERM. When bougied
    // runs in the foreground, Ctrl-C delivers SIGINT to the whole
    // terminal foreground process group — bougied *and* every babysit
    // (the babysit shares bougied's group; it doesn't `setsid` away).
    // Without a handler, SIGINT's default disposition takes the babysit
    // down before `cleanup_group` runs; pdeathsig then reaps the leader
    // but any forked descendant (e.g. rabbitmq's beam helpers) escapes
    // and reparents to pid 1. Catching it here means the group is always
    // torn down, whatever signal arrives.
    let mut sigint =
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())
            .wrap_err("installing SIGINT handler")?;

    // Build the supervised process group. With a sidecar, it's created
    // by the sidecar (spawned first so it's listening before the main
    // service starts) and the main service joins it. Without one, the
    // main service creates the group itself. Either way the babysit
    // stays *outside* the group so its `killpg` teardown doesn't signal
    // itself.
    let mut sidecar_child = None;
    let (pgid, mut child) = if let Some(sidecar_exec) = &cfg.sidecar {
        let sc = spawn_in_group(sidecar_exec, &[], None).wrap_err_with(|| {
            format!("spawning sidecar {}", sidecar_exec.to_string_lossy())
        })?;
        let sc_pid = sc.id().ok_or_else(|| eyre!("sidecar has no pid"))?;
        let group = i32::try_from(sc_pid)
            .wrap_err_with(|| format!("sidecar pid {sc_pid} doesn't fit in pid_t"))?;
        sidecar_child = Some(sc);

        if let Some(port) = cfg.sidecar_ready_port
            && !wait_for_port(port, SIDECAR_READY_TIMEOUT).await
        {
            eprintln!(
                "[babysit:{}] sidecar port {port} not ready within {SIDECAR_READY_TIMEOUT:?}; \
                 starting service anyway",
                cfg.service_name
            );
        }

        let main = spawn_in_group(&cfg.exec, &cfg.argv, Some(group)).wrap_err_with(|| {
            format!("spawning service {} ({})", cfg.service_name, cfg.exec.to_string_lossy())
        })?;
        (group, main)
    } else {
        let main = spawn_in_group(&cfg.exec, &cfg.argv, None).wrap_err_with(|| {
            format!("spawning service {} ({})", cfg.service_name, cfg.exec.to_string_lossy())
        })?;
        let pid_u32 = main.id().ok_or_else(|| eyre!("spawned child has no pid"))?;
        let pid = i32::try_from(pid_u32)
            .wrap_err_with(|| format!("child pid {pid_u32} doesn't fit in pid_t (i32)"))?;
        (pid, main)
    };

    // Report pgid first thing so bougied's read-timeout collapses to
    // a tight "did the child actually fork" check.
    control
        .write_all(format!("pgid={pgid}\n").as_bytes())
        .await
        .wrap_err("reporting pgid to bougied")?;
    control.flush().await.ok();

    // Watch the main service. With a sidecar we also watch *it* — but a
    // sidecar exit is fatal only if it takes the readiness port down
    // with it. A bare `epmd` exits 0 the instant it finds another epmd
    // already bound to its port (a leak from a prior run, a system
    // Erlang, another BEAM app on the box); that's benign — the port is
    // still served and the broker is happily using it — so tearing the
    // unit down there (as we used to, unconditionally) kills a perfectly
    // healthy service. Re-probe the port on a sidecar exit and only fail
    // the unit when it's actually gone. Looped so a benign sidecar exit
    // disarms that arm and we keep supervising the main service.
    let outcome = loop {
        tokio::select! {
            // Main service exited on its own.
            status = child.wait() => {
                let code = exit_status_code(status);
                let _ = control.write_all(format!("exited={code}\n").as_bytes()).await;
                // Sweep the rest of the group (a sidecar, plus any
                // descendants the leader left behind). No-op for a
                // single-process service.
                cleanup_group(pgid, cfg.grace_secs).await;
                let clamped = u8::try_from(code.clamp(0, 255)).expect("clamped to 0..=255 fits in u8");
                break ExitCode::from(clamped);
            }
            // Sidecar exited. Reap it and disarm this arm (it's gone
            // either way), then decide whether the unit is broken.
            () = wait_optional_child(sidecar_child.as_mut()) => {
                sidecar_child = None;
                let port_still_served = match cfg.sidecar_ready_port {
                    Some(port) => wait_for_port(port, SIDECAR_PORT_RECHECK).await,
                    // No port to verify against — can't tell benign from
                    // fatal, so keep the old conservative stance.
                    None => false,
                };
                if port_still_served {
                    eprintln!(
                        "[babysit:{}] sidecar exited but its ready-port is still served \
                         (another instance owns it); leaving the service running",
                        cfg.service_name
                    );
                    continue;
                }
                eprintln!(
                    "[babysit:{}] sidecar exited and its port is down; tearing down service",
                    cfg.service_name
                );
                cleanup_group(pgid, cfg.grace_secs).await;
                let _ = control.write_all(b"exited=70\n").await;
                break ExitCode::from(70);
            }
            // Control socket closed → bougied died. Take the group down.
            () = wait_socket_eof(&mut control) => {
                cleanup_group(pgid, cfg.grace_secs).await;
                break ExitCode::SUCCESS;
            }
            // Bougied asked us to stop cleanly (SIGTERM), or a foreground
            // Ctrl-C hit our group (SIGINT reaches every babysit sharing
            // bougied's terminal foreground group). Either tears it down.
            () = async { tokio::select! { _ = sigterm.recv() => {}, _ = sigint.recv() => {} } } => {
                cleanup_group(pgid, cfg.grace_secs).await;
                break ExitCode::SUCCESS;
            }
        }
    };

    // Reap the now-dead children so they don't linger as zombies held by
    // this (about-to-exit) process.
    let _ = child.wait().await;
    if let Some(mut sc) = sidecar_child {
        let _ = sc.wait().await;
    }
    Ok(outcome)
}

/// Build + spawn a process in a process group. `join_pgid = None` makes
/// it a new group leader (`setpgid(0,0)`); `Some(g)` joins group `g`.
/// stdout/stderr inherit the babysit's piped fds so the supervisor's log
/// forwarders see the output.
fn spawn_in_group(
    exec: &OsString,
    argv: &[OsString],
    join_pgid: Option<i32>,
) -> std::io::Result<tokio::process::Child> {
    let mut cmd = tokio::process::Command::new(exec);
    cmd.args(argv)
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .kill_on_drop(false);

    // SAFETY: pre_exec runs after fork, before exec, in the child
    // address space. Every call is async-signal-safe: `setpgid`,
    // `prctl(PR_SET_PDEATHSIG)`, `getppid`.
    #[allow(unsafe_code)]
    unsafe {
        cmd.pre_exec(move || {
            // Place the child in its group: a fresh one (leader) when
            // `join_pgid` is None, else join the existing group so the
            // babysit's `killpg` reaps the whole unit at once.
            let pgid_arg = match join_pgid {
                None => None,
                Some(g) => Some(
                    rustix::process::Pid::from_raw(g)
                        .ok_or_else(|| std::io::Error::other("invalid sidecar pgid"))?,
                ),
            };
            rustix::process::setpgid(None, pgid_arg)
                .map_err(|e| std::io::Error::from_raw_os_error(e.raw_os_error()))?;
            // Parent-death signal: if the babysit is itself SIGKILLed
            // (OOM killer, `kill -9`) no cleanup handler runs, so have
            // the kernel SIGKILL this process when its parent dies — it
            // can't outlive its supervisor. Survives the upcoming execve
            // (no set-uid binary). Linux-only; macOS relies on the
            // `killpg` cleanup paths.
            #[cfg(target_os = "linux")]
            {
                rustix::process::set_parent_process_death_signal(Some(
                    rustix::process::Signal::KILL,
                ))
                .map_err(|e| std::io::Error::from_raw_os_error(e.raw_os_error()))?;
                // Race: the babysit may have died between our fork and
                // the prctl above. Reparenting to init means it's gone —
                // refuse to exec rather than spawn an instant orphan.
                if rustix::process::getppid().is_none_or(|p| p.is_init()) {
                    return Err(std::io::Error::other(
                        "babysit exited before the child could exec",
                    ));
                }
            }
            Ok(())
        });
    }

    cmd.spawn()
}

/// Exit code from a waited child: `code()` for a normal exit, `128+signo`
/// for a signal-kill (shell convention), `-1` otherwise.
fn exit_status_code(status: std::io::Result<std::process::ExitStatus>) -> i32 {
    status.ok().map_or(-1, |s| {
        use std::os::unix::process::ExitStatusExt;
        s.code().or_else(|| s.signal().map(|sig| 128 + sig)).unwrap_or(-1)
    })
}

/// Resolve when `child` exits. When there's no sidecar, never resolves
/// (so the `select!` arm is inert) — `std::future::pending` parks it.
async fn wait_optional_child(child: Option<&mut tokio::process::Child>) {
    match child {
        Some(c) => {
            let _ = c.wait().await;
        }
        None => std::future::pending::<()>().await,
    }
}

/// Poll a loopback TCP port until it accepts a connection, or the
/// timeout elapses. Returns whether it came up.
async fn wait_for_port(port: u16, timeout: Duration) -> bool {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if tokio::net::TcpStream::connect(("127.0.0.1", port)).await.is_ok() {
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Returns when the peer end of the socket closes (EOF). Any data
/// sent by bougied is currently ignored — the channel is one-way.
async fn wait_socket_eof(s: &mut tokio::net::UnixStream) {
    let mut buf = [0u8; 64];
    loop {
        match s.read(&mut buf).await {
            Ok(0) | Err(_) => return,
            Ok(_) => {}
        }
    }
}

/// `killpg(pgid, SIGTERM)` → wait up to `grace_secs` for the *whole
/// group* to drain → `killpg(SIGKILL)`. Group-centric (no child handle)
/// so it reaps every member — the service, a sidecar, and any
/// descendants still in the group — not just the leader. The caller
/// separately `wait()`s its direct children to clear the zombies.
async fn cleanup_group(pgid: i32, grace_secs: u64) {
    let Some(pgrp) = rustix::process::Pid::from_raw(pgid) else {
        return;
    };
    let _ = rustix::process::kill_process_group(pgrp, rustix::process::Signal::TERM);
    // Poll for the group to empty. `test_kill_process_group` is
    // `kill(-pgid, 0)`: `Ok` while members remain, `Err(ESRCH)` once
    // gone.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(grace_secs);
    while rustix::process::test_kill_process_group(pgrp).is_ok() {
        if tokio::time::Instant::now() >= deadline {
            let _ = rustix::process::kill_process_group(pgrp, rustix::process::Signal::KILL);
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn os(s: &str) -> OsString {
        OsString::from(s)
    }

    #[test]
    fn parse_args_happy_path() {
        let cfg = parse_args(vec![
            os("--service-name"),
            os("redis"),
            os("--grace-secs"),
            os("7"),
            os("--control-fd"),
            os("3"),
            os("--"),
            os("/bin/sleep"),
            os("300"),
        ])
        .unwrap();
        assert_eq!(cfg.service_name, "redis");
        assert_eq!(cfg.grace_secs, 7);
        assert_eq!(cfg.control_fd, 3);
        assert_eq!(cfg.exec, OsString::from("/bin/sleep"));
        assert_eq!(cfg.argv, vec![OsString::from("300")]);
        // No sidecar by default.
        assert_eq!(cfg.sidecar, None);
        assert_eq!(cfg.sidecar_ready_port, None);
    }

    #[test]
    fn parse_args_with_sidecar() {
        let cfg = parse_args(vec![
            os("--service-name"),
            os("rabbitmq"),
            os("--grace-secs"),
            os("10"),
            os("--control-fd"),
            os("3"),
            os("--sidecar"),
            os("/store/erlang/bin/epmd"),
            os("--sidecar-ready-port"),
            os("4369"),
            os("--"),
            os("/store/rabbitmq/sbin/rabbitmq-server"),
        ])
        .unwrap();
        assert_eq!(cfg.sidecar, Some(OsString::from("/store/erlang/bin/epmd")));
        assert_eq!(cfg.sidecar_ready_port, Some(4369));
        assert_eq!(cfg.exec, OsString::from("/store/rabbitmq/sbin/rabbitmq-server"));
        assert!(cfg.argv.is_empty());
    }

    #[test]
    fn parse_args_rejects_missing_separator() {
        let err = parse_args(vec![
            os("--service-name"),
            os("redis"),
            os("--grace-secs"),
            os("5"),
            os("--control-fd"),
            os("3"),
            os("/bin/sleep"),
        ])
        .unwrap_err();
        assert!(err.to_string().contains("separator"));
    }

    #[test]
    fn parse_args_rejects_missing_flag_value() {
        let err = parse_args(vec![os("--service-name")]).unwrap_err();
        assert!(err.to_string().contains("requires a value"));
    }

    #[test]
    fn parse_args_rejects_unknown_flag() {
        let err = parse_args(vec![os("--whatever"), os("x"), os("--")]).unwrap_err();
        assert!(err.to_string().contains("unknown flag"));
    }
}
