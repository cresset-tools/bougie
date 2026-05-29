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
    exec: OsString,
    argv: Vec<OsString>,
}

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

    let mut cmd = tokio::process::Command::new(&cfg.exec);
    cmd.args(&cfg.argv)
        .stdin(Stdio::null())
        // Inherit stdout/stderr from babysit — which itself inherits
        // bougied's piped fds, so the supervisor's log forwarders
        // still see the service's output.
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .kill_on_drop(false);

    // SAFETY: pre_exec runs after fork, before exec, in the child
    // address space. `setpgid(0, 0)` is async-signal-safe.
    #[allow(unsafe_code)]
    unsafe {
        cmd.pre_exec(|| {
            rustix::process::setpgid(None, None)
                .map_err(|e| std::io::Error::from_raw_os_error(e.raw_os_error()))
        });
    }

    let mut child = cmd.spawn().wrap_err_with(|| {
        format!(
            "spawning service {} ({})",
            cfg.service_name,
            cfg.exec.to_string_lossy()
        )
    })?;
    let pid_u32 = child
        .id()
        .ok_or_else(|| eyre!("spawned child has no pid"))?;
    let pid = i32::try_from(pid_u32)
        .wrap_err_with(|| format!("child pid {pid_u32} doesn't fit in pid_t (i32)"))?;
    // setpgid(0, 0) made the child its own pgrp leader; pgid == pid.
    let pgid = pid;

    // Report pgid first thing so bougied's read-timeout collapses to
    // a tight "did the child actually fork" check.
    control
        .write_all(format!("pgid={pgid}\n").as_bytes())
        .await
        .wrap_err("reporting pgid to bougied")?;
    control.flush().await.ok();

    tokio::select! {
        // Service exited on its own.
        status = child.wait() => {
            // `code()` is None for a signal-killed child; report the
            // signal as 128+signo (shell convention) rather than
            // collapsing every crash to -1 (which then clamps to 0 and
            // is indistinguishable from a clean exit).
            let code = status.ok().map_or(-1, |s| {
                use std::os::unix::process::ExitStatusExt;
                s.code()
                    .or_else(|| s.signal().map(|sig| 128 + sig))
                    .unwrap_or(-1)
            });
            let _ = control
                .write_all(format!("exited={code}\n").as_bytes())
                .await;
            let clamped = u8::try_from(code.clamp(0, 255))
                .expect("clamped to 0..=255 fits in u8");
            Ok(ExitCode::from(clamped))
        }
        // Control socket closed → bougied died. Take the group down.
        () = wait_socket_eof(&mut control) => {
            cleanup_group(pgid, cfg.grace_secs, &mut child).await;
            Ok(ExitCode::SUCCESS)
        }
        // Bougied asked us to stop cleanly.
        _ = sigterm.recv() => {
            cleanup_group(pgid, cfg.grace_secs, &mut child).await;
            Ok(ExitCode::SUCCESS)
        }
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

/// `killpg(pgid, SIGTERM)` → wait up to `grace_secs` → `killpg(SIGKILL)`.
/// Reaps the babysit's direct child (the service) after either signal.
async fn cleanup_group(pgid: i32, grace_secs: u64, child: &mut tokio::process::Child) {
    let Some(pgrp) = rustix::process::Pid::from_raw(pgid) else {
        return;
    };
    let _ = rustix::process::kill_process_group(pgrp, rustix::process::Signal::TERM);
    if tokio::time::timeout(Duration::from_secs(grace_secs), child.wait())
        .await
        .is_err()
    {
        let _ = rustix::process::kill_process_group(pgrp, rustix::process::Signal::KILL);
        let _ = child.wait().await;
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
