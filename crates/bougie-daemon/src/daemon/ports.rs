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

/// How far above a service's default port the allocator scans for a free
/// one when the default is taken. 64 leaves room for a handful of
/// coexisting instances / foreign squatters without wandering far into
/// unrelated services' territory.
pub const PORT_SCAN_SPAN: u16 = 64;

/// Choose the effective port for a service that prefers `default`.
///
/// Policy (see `INSTANCES_PLAN.md`):
/// 1. **Sticky** — if `recorded` is set and still bindable, reuse it, so
///    a restart doesn't strand config that cached the port (even once
///    the original default has freed up).
/// 2. Else the `default`, if it's free.
/// 3. Else scan `default+1 ..= default+PORT_SCAN_SPAN` for the first free
///    port.
///
/// `is_free` is injected rather than hardwired to [`port_in_use`] so a
/// caller allocating several ports for one instance can exclude the ones
/// it already picked this round, and so the policy is unit-testable
/// without binding real sockets. Returns `None` when nothing in the scan
/// window is free.
pub fn allocate_port(default: u16, recorded: Option<u16>, is_free: impl Fn(u16) -> bool) -> Option<u16> {
    if let Some(r) = recorded
        && r != 0
        && is_free(r)
    {
        return Some(r);
    }
    if is_free(default) {
        return Some(default);
    }
    let start = default.saturating_add(1);
    let end = default.saturating_add(PORT_SCAN_SPAN);
    (start..=end).find(|&p| p != 0 && is_free(p))
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

    // ---------- allocate_port ----------

    /// A `is_free` closure that treats the listed ports as occupied.
    fn taken(ports: &[u16]) -> impl Fn(u16) -> bool + '_ {
        move |p| !ports.contains(&p)
    }

    #[test]
    fn allocate_uses_default_when_free() {
        assert_eq!(allocate_port(9200, None, taken(&[])), Some(9200));
    }

    #[test]
    fn allocate_scans_upward_when_default_taken() {
        // 9200 and 9201 held → lands on 9202.
        assert_eq!(allocate_port(9200, None, taken(&[9200, 9201])), Some(9202));
    }

    #[test]
    fn allocate_reuses_recorded_port_even_when_default_is_free() {
        // Sticky: keep the relocated port so cached config stays valid.
        assert_eq!(allocate_port(9200, Some(9205), taken(&[])), Some(9205));
    }

    #[test]
    fn allocate_falls_back_when_recorded_port_now_taken() {
        // Recorded 9205 is occupied; default 9200 is free → default.
        assert_eq!(allocate_port(9200, Some(9205), taken(&[9205])), Some(9200));
    }

    #[test]
    fn allocate_rescans_when_recorded_and_default_both_taken() {
        assert_eq!(
            allocate_port(9200, Some(9205), taken(&[9205, 9200])),
            Some(9201)
        );
    }

    #[test]
    fn allocate_returns_none_when_whole_window_is_taken() {
        let window: Vec<u16> = (9200..=9200 + PORT_SCAN_SPAN).collect();
        assert_eq!(allocate_port(9200, None, taken(&window)), None);
    }

    #[test]
    fn allocate_multi_port_avoids_intra_instance_collision() {
        // Two ports for one instance, both defaulting to the same number
        // (contrived), with 9200 externally held: the caller threads a
        // `claimed` set so the second pick skips the first. Each call
        // gets a fresh closure so `claimed` can grow between them.
        let external = [9200u16];
        let mut claimed: Vec<u16> = Vec::new();
        let first =
            allocate_port(9200, None, |p| !external.contains(&p) && !claimed.contains(&p)).unwrap();
        claimed.push(first);
        let second =
            allocate_port(9200, None, |p| !external.contains(&p) && !claimed.contains(&p)).unwrap();
        assert_ne!(first, second);
        assert_eq!((first, second), (9201, 9202));
    }

    #[test]
    fn allocate_honours_a_real_probe() {
        // Bind :0 to hold a real port, then prove the allocator relocates
        // off it via the real `port_in_use` probe.
        let sock = TcpListener::bind("127.0.0.1:0").unwrap();
        let held = sock.local_addr().unwrap().port();
        // `is_free` is the negation of `port_in_use` — the polarity every
        // real caller must get right.
        let got = allocate_port(held, None, |p| !port_in_use(p));
        assert!(got.is_some());
        assert_ne!(got, Some(held));
    }
}
