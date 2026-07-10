//! A tiny TCP-service stand-in for the port-fallback integration test
//! (`phase23_port_fallback`). Mimics just enough of Mailpit's surface for
//! the supervisor to bring it up as a `mailpit` instance:
//!
//! - parses `--smtp <addr>` and `--listen <addr>` (Mailpit's SMTP and web
//!   UI addresses — the exact flags `render_exec_args` emits) and ignores
//!   everything else (`--database <path>`),
//! - binds both loopback TCP ports so the (possibly relocated) ports are
//!   genuinely occupied by us,
//! - answers `GET /` on the `--listen` port with `200 OK`, which is the
//!   supervisor's mailpit health probe (`http_get(http_port, "/")`),
//! - waits for SIGTERM (the supervisor's stop path).
//!
//! Unix-only — the service supervisor it backs is Unix-only. On Windows
//! this compiles to a stub `main` so misuse is loud.

#[cfg(not(unix))]
fn main() {
    eprintln!("fake-tcp-service: this test fixture only runs on Unix");
    std::process::exit(1);
}

#[cfg(unix)]
fn main() {
    use std::io::{Read, Write};
    use std::net::TcpListener;

    let args: Vec<String> = std::env::args().collect();
    let mut smtp: Option<String> = None;
    let mut http: Option<String> = None;
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--smtp" if i + 1 < args.len() => {
                smtp = Some(args[i + 1].clone());
                i += 2;
            }
            "--listen" if i + 1 < args.len() => {
                http = Some(args[i + 1].clone());
                i += 2;
            }
            _ => i += 1,
        }
    }

    // Bind the SMTP port so the relocated address is really held by us —
    // the test asserts the service took the new port. Health doesn't probe
    // SMTP, so a bare accept-and-greet loop is enough.
    if let Some(addr) = smtp {
        match TcpListener::bind(&addr) {
            Ok(l) => {
                eprintln!("fake-tcp-service: smtp on {addr}");
                std::thread::spawn(move || {
                    for stream in l.incoming().flatten() {
                        let mut s = stream;
                        let _ = s.write_all(b"220 fake-mailpit\r\n");
                    }
                });
            }
            Err(e) => {
                eprintln!("fake-tcp-service: bind smtp {addr}: {e}");
                std::process::exit(2);
            }
        }
    }

    // The web/UI port is the health-probe target: answer 200 to every
    // request so `wait_for_health` goes healthy. This accept loop also
    // keeps the process alive until SIGTERM.
    let http_addr = http.expect("--listen <addr> is required");
    let listener = match TcpListener::bind(&http_addr) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("fake-tcp-service: bind http {http_addr}: {e}");
            std::process::exit(3);
        }
    };
    eprintln!("fake-tcp-service: http on {http_addr}");
    for stream in listener.incoming().flatten() {
        std::thread::spawn(move || {
            let mut s = stream;
            let mut buf = [0u8; 1024];
            let _ = s.read(&mut buf); // consume the request line(s)
            let _ = s.write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok",
            );
        });
    }
}
