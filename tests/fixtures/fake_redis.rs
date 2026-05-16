//! A trivially-small "redis-server" stand-in for the Phase 3
//! integration tests. Parses `--unixsocket <path>`, opens a listener
//! on that path, and waits for SIGTERM. Speaks just enough of the
//! RESP protocol to acknowledge a SELECT+FLUSHDB sequence from
//! `provisioners::redis::deprovision --purge`.
//!
//! Unix-only — the supervisor tests it backs don't run on Windows in
//! Phase 1 (no Unix domain sockets, no SIGTERM). On Windows this
//! compiles to a stub `main` that errors out so misuse is loud.

#[cfg(not(unix))]
fn main() {
    eprintln!("fake-redis: this test fixture only runs on Unix");
    std::process::exit(1);
}

#[cfg(unix)]
use std::io::{Read, Write};
#[cfg(unix)]
use std::os::unix::net::{UnixListener, UnixStream};

#[cfg(unix)]
fn main() {
    let args: Vec<String> = std::env::args().collect();
    let mut socket_path: Option<String> = None;
    let mut i = 1;
    while i < args.len() {
        if args[i] == "--unixsocket" && i + 1 < args.len() {
            socket_path = Some(args[i + 1].clone());
            i += 2;
        } else {
            i += 1;
        }
    }
    let path = socket_path.expect("--unixsocket <path> required");
    let _ = std::fs::remove_file(&path);
    let listener = UnixListener::bind(&path).expect("bind unix socket");
    eprintln!("fake-redis: listening on {path}");

    // Single-threaded accept loop. Replies "+OK\r\n" to every line
    // received so the FLUSHDB exercise in the deprovision-with-purge
    // test path completes happily.
    loop {
        let (mut stream, _addr) = match listener.accept() {
            Ok(s) => s,
            Err(e) => {
                eprintln!("fake-redis: accept failed: {e}");
                continue;
            }
        };
        std::thread::spawn(move || handle(&mut stream));
    }
}

#[cfg(unix)]
fn handle(stream: &mut UnixStream) {
    let mut buf = [0u8; 1024];
    while let Ok(n) = stream.read(&mut buf) {
        if n == 0 {
            return;
        }
        // For every \r\n-terminated line, send back "+OK\r\n". Crude
        // but enough to satisfy any RESP-style client that doesn't
        // care about the payload.
        let recv = &buf[..n];
        let lines = recv.iter().filter(|&&b| b == b'\n').count();
        for _ in 0..lines.max(1) {
            if stream.write_all(b"+OK\r\n").is_err() {
                return;
            }
        }
    }
}
