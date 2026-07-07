//! Loopback TCP port probing + best-effort holder attribution.
//!
//! Used by the supervisor's pre-start conflict check and by `bougie
//! diagnose`'s ports section. Std-only by design: the probe is a bind
//! attempt, and attribution parses `/proc` on Linux (same-user
//! visibility only) — no `lsof`/`ss` shell-outs.

use std::net::{Ipv4Addr, SocketAddrV4, TcpListener};

/// The process found listening on a probed port.
#[derive(Debug, Clone)]
pub struct PortHolder {
    pub pid: u32,
    /// Process name from `/proc/<pid>/comm` (kernel-truncated to 15
    /// chars); `?` when unreadable.
    pub comm: String,
}

/// True when `127.0.0.1:<port>` cannot be bound right now. A wildcard
/// (`0.0.0.0`) listener collides with the loopback bind, so it is
/// detected too. `TcpListener::bind` sets `SO_REUSEADDR` on Unix,
/// which keeps lingering `TIME_WAIT` sockets from reading as "in
/// use" — only a live listener trips this.
pub fn port_in_use(port: u16) -> bool {
    TcpListener::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, port)).is_err()
}

/// Human fragment naming whoever holds `port`, for error messages:
/// `beam.smp (pid 4321)`, or `another process` when attribution is
/// unavailable (non-Linux, or a different user's process).
pub fn describe_holder(port: u16) -> String {
    match holder_of(port) {
        Some(h) => format!("{} (pid {})", h.comm, h.pid),
        None => "another process".to_owned(),
    }
}

/// Best-effort: the process listening on `port` (any local address).
/// Linux only; same-user processes only (`/proc/<pid>/fd` of other
/// users is unreadable without privileges).
#[cfg(target_os = "linux")]
pub fn holder_of(port: u16) -> Option<PortHolder> {
    let inodes = listener_inodes(port);
    if inodes.is_empty() {
        return None;
    }
    let targets: Vec<String> = inodes.iter().map(|i| format!("socket:[{i}]")).collect();
    for entry in std::fs::read_dir("/proc").ok()?.flatten() {
        let name = entry.file_name();
        let Some(pid) = name.to_str().and_then(|s| s.parse::<u32>().ok()) else {
            continue;
        };
        let Ok(fds) = std::fs::read_dir(entry.path().join("fd")) else {
            continue; // EACCES: someone else's process
        };
        for fd in fds.flatten() {
            let Ok(link) = std::fs::read_link(fd.path()) else {
                continue;
            };
            if link
                .to_str()
                .is_some_and(|l| targets.iter().any(|t| t == l))
            {
                let comm = std::fs::read_to_string(format!("/proc/{pid}/comm"))
                    .map_or_else(|_| "?".to_owned(), |c| c.trim().to_owned());
                return Some(PortHolder { pid, comm });
            }
        }
    }
    None
}

#[cfg(not(target_os = "linux"))]
pub fn holder_of(_port: u16) -> Option<PortHolder> {
    None
}

/// Socket inodes of LISTEN entries on `port` in `/proc/net/tcp{,6}`.
/// Table format: whitespace-separated, `local_address` is
/// `<hex-addr>:<hex-port>` at field 1, state at field 3 (`0A` =
/// LISTEN), inode at field 9.
#[cfg(target_os = "linux")]
fn listener_inodes(port: u16) -> Vec<u64> {
    let mut inodes = Vec::new();
    for table in ["/proc/net/tcp", "/proc/net/tcp6"] {
        let Ok(text) = std::fs::read_to_string(table) else {
            continue;
        };
        for line in text.lines().skip(1) {
            let fields: Vec<&str> = line.split_whitespace().collect();
            if fields.len() < 10 || fields[3] != "0A" {
                continue;
            }
            let Some((_, port_hex)) = fields[1].rsplit_once(':') else {
                continue;
            };
            if u16::from_str_radix(port_hex, 16) != Ok(port) {
                continue;
            }
            if let Ok(inode) = fields[9].parse::<u64>() {
                inodes.push(inode);
            }
        }
    }
    inodes
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn free_ephemeral_port_reads_free() {
        // Bind :0 to learn a free port, drop the listener, probe it.
        let sock = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = sock.local_addr().unwrap().port();
        drop(sock);
        assert!(!port_in_use(port));
    }

    #[test]
    fn held_port_reads_in_use() {
        let sock = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = sock.local_addr().unwrap().port();
        assert!(port_in_use(port));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn holder_attribution_finds_our_own_listener() {
        let sock = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = sock.local_addr().unwrap().port();
        let holder = holder_of(port).expect("own listener should be attributable");
        assert_eq!(holder.pid, std::process::id());
        let described = describe_holder(port);
        assert!(
            described.contains(&format!("pid {}", std::process::id())),
            "{described}"
        );
    }

    #[test]
    fn unheld_port_has_no_holder() {
        let sock = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = sock.local_addr().unwrap().port();
        drop(sock);
        assert!(holder_of(port).is_none());
        assert_eq!(describe_holder(port), "another process");
    }
}
