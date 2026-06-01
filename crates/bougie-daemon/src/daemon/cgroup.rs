//! Capability probe for the cgroup-v2 supervision backend.
//!
//! Phase 1 of `SUPERVISION_PLAN.md`: detect, once at startup, whether
//! this host gives us a usable *rootless* cgroup-v2 subtree to corral
//! each service in. The result is a [`SupervisionBackend`] the
//! supervisor records; **this phase changes no behaviour** — teardown
//! still goes through the babysit's process-group path regardless of
//! what we detect. Later phases consume the backend to place services in
//! per-service leaf cgroups and `cgroup.kill` them (catching daemonized
//! escapees like Erlang's `epmd` that `killpg` can't).
//!
//! Rootless cgroup management is *not* universal — it needs Linux +
//! cgroup-v2 unified + a delegated, writable cgroup (normally a logind
//! `user@$UID.service` subtree). When any piece is missing we fall back
//! to [`SupervisionBackend::ProcessGroup`], i.e. today's behaviour.

use std::fs::OpenOptions;
use std::io;
use std::os::fd::OwnedFd;
use std::path::{Path, PathBuf};

/// The strategy bougied uses to terminate a service's whole subtree.
/// Chosen once at startup by [`detect`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SupervisionBackend {
    /// cgroup v2 with `cgroup.kill` (kernel ≥ 5.14). `base` is our
    /// writable delegated cgroup; per-service leaf cgroups live under it.
    CgroupKill { base: PathBuf },
    /// cgroup v2 without `cgroup.kill` (kernel < 5.14): the teardown path
    /// freezes the leaf and SIGKILLs each pid in `cgroup.procs` instead.
    CgroupFreeze { base: PathBuf },
    /// No usable delegated cgroup (non-Linux, cgroup v1/hybrid, no
    /// delegation). Fall back to the process-group + `PR_SET_PDEATHSIG`
    /// floor the babysit already implements.
    ProcessGroup,
}

impl SupervisionBackend {
    /// Short stable label for logs / diagnostics.
    pub fn label(&self) -> &'static str {
        match self {
            Self::CgroupKill { .. } => "cgroup-kill",
            Self::CgroupFreeze { .. } => "cgroup-freeze",
            Self::ProcessGroup => "process-group",
        }
    }

    /// The delegated base cgroup, when one was found.
    pub fn base(&self) -> Option<&Path> {
        match self {
            Self::CgroupKill { base } | Self::CgroupFreeze { base } => Some(base),
            Self::ProcessGroup => None,
        }
    }

    /// Path of the per-service leaf cgroup, when a cgroup backend is
    /// active. `None` under `ProcessGroup`.
    pub fn leaf(&self, service: &str) -> Option<PathBuf> {
        self.base().map(|b| leaf_under(b, service))
    }

    /// Whether teardown can use the atomic `cgroup.kill` (vs the
    /// freeze+SIGKILL fallback).
    pub fn kill_supported(&self) -> bool {
        matches!(self, Self::CgroupKill { .. })
    }
}

/// Subdirectory under the delegated base that holds the per-service leaf
/// cgroups bougied manages. Namespaced so startup-reap can recognise its
/// own leaves and never touch a sibling's cgroups.
const SVC_DIR: &str = "bougie.svc";

/// Path of a service's leaf cgroup under a delegated `base`. Pure path
/// join — does not touch the filesystem.
pub fn leaf_under(base: &Path, service: &str) -> PathBuf {
    base.join(SVC_DIR).join(service)
}

/// Create the per-service leaf cgroup under `base` and return its path.
/// Idempotent — an existing leaf (left by a prior run) is reused; the
/// kernel auto-populates its interface files.
pub fn create_leaf(base: &Path, service: &str) -> io::Result<PathBuf> {
    let leaf = leaf_under(base, service);
    std::fs::create_dir_all(&leaf)?;
    Ok(leaf)
}

/// Create the leaf and open its `cgroup.procs` for writing. Writing
/// `"0"` to the returned fd from a freshly-forked child's `pre_exec`
/// moves that child — and therefore the service it's about to exec, plus
/// every descendant — into the leaf, so nothing can fork its way out.
/// The caller keeps the fd alive across the spawn.
pub fn open_leaf_procs(base: &Path, service: &str) -> io::Result<(PathBuf, OwnedFd)> {
    let leaf = create_leaf(base, service)?;
    let file = OpenOptions::new().write(true).open(leaf.join("cgroup.procs"))?;
    Ok((leaf, OwnedFd::from(file)))
}

/// SIGKILL every process in `leaf` (escapees included) and remove the
/// cgroup. Best-effort and blocking — run it off the async runtime
/// (`spawn_blocking`); the `rmdir` retry sleeps because kill→reap is
/// asynchronous. `kill_supported` picks `cgroup.kill` over the
/// freeze+SIGKILL fallback for pre-5.14 kernels.
pub fn kill_and_remove(leaf: &Path, kill_supported: bool) {
    if kill_supported {
        let _ = std::fs::write(leaf.join("cgroup.kill"), b"1");
    } else {
        freeze_and_kill(leaf);
    }
    remove_leaf(leaf);
}

/// Pre-5.14 fallback: freeze the cgroup so nothing forks mid-sweep,
/// SIGKILL each member, then thaw (so any survivor can still be reaped).
fn freeze_and_kill(leaf: &Path) {
    let _ = std::fs::write(leaf.join("cgroup.freeze"), b"1");
    if let Ok(procs) = std::fs::read_to_string(leaf.join("cgroup.procs")) {
        for pid in procs.lines().filter_map(|l| l.trim().parse::<i32>().ok()) {
            if let Some(p) = rustix::process::Pid::from_raw(pid) {
                let _ = rustix::process::kill_process(p, rustix::process::Signal::KILL);
            }
        }
    }
    let _ = std::fs::write(leaf.join("cgroup.freeze"), b"0");
}

/// `rmdir` the leaf, retrying briefly: a cgroup is only removable once
/// empty, and the kernel reaps killed members asynchronously after the
/// kill returns.
fn remove_leaf(leaf: &Path) {
    for _ in 0..200 {
        match std::fs::remove_dir(leaf) {
            Ok(()) => return,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return,
            Err(_) => std::thread::sleep(std::time::Duration::from_millis(5)),
        }
    }
    let _ = std::fs::remove_dir(leaf);
}

/// Kill + remove every leftover service leaf under `base/bougie.svc/`.
/// Called once at daemon startup: the flock singleton guarantees no
/// other bougied is live, so any leaves present are orphans from a dead
/// previous instance. Returns the names reaped (for logging).
pub fn reap_stale_leaves(base: &Path, kill_supported: bool) -> Vec<String> {
    let svc_dir = base.join(SVC_DIR);
    let Ok(entries) = std::fs::read_dir(&svc_dir) else {
        return Vec::new();
    };
    let mut reaped = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            kill_and_remove(&path, kill_supported);
            if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                reaped.push(name.to_string());
            }
        }
    }
    reaped
}

/// `CGROUP2_SUPER_MAGIC` — the `statfs` `f_type` of a cgroup-v2 mount.
#[cfg(target_os = "linux")]
const CGROUP2_SUPER_MAGIC: u64 = 0x6367_7270;

/// The conventional unified cgroup-v2 mount point.
#[cfg(target_os = "linux")]
const CGROUP_ROOT: &str = "/sys/fs/cgroup";

/// Probe the host for a usable rootless cgroup-v2 backend.
///
/// Linux-only; every other target returns
/// [`SupervisionBackend::ProcessGroup`] (macOS/Windows have no cgroups).
#[must_use]
pub fn detect() -> SupervisionBackend {
    #[cfg(not(target_os = "linux"))]
    {
        SupervisionBackend::ProcessGroup
    }
    #[cfg(target_os = "linux")]
    {
        if !is_cgroup2(Path::new(CGROUP_ROOT)) {
            return SupervisionBackend::ProcessGroup;
        }
        let Some(base) = self_cgroup_base() else {
            return SupervisionBackend::ProcessGroup;
        };
        probe_at(&base, true)
    }
}

/// Is `mount` a cgroup-v2 (unified) filesystem? `false` on any error or
/// on cgroup v1 / hybrid.
#[cfg(target_os = "linux")]
fn is_cgroup2(mount: &Path) -> bool {
    rustix::fs::statfs(mount).is_ok_and(|s| {
        // `f_type` is a platform integer; the magic is a small positive
        // constant, so the cast is lossless in practice.
        #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
        let ty = s.f_type as u64;
        ty == CGROUP2_SUPER_MAGIC
    })
}

/// bougied's own cgroup, as an absolute path under [`CGROUP_ROOT`].
///
/// We deliberately base off *our own* cgroup (from `/proc/self/cgroup`)
/// rather than hardcoding `user@$UID.service`: bougied may itself run
/// inside a transient scope, and the delegated, writable subtree is
/// wherever we already are.
#[cfg(target_os = "linux")]
fn self_cgroup_base() -> Option<PathBuf> {
    let content = std::fs::read_to_string("/proc/self/cgroup").ok()?;
    let rel = parse_self_cgroup(&content)?;
    // `rel` is rooted at the cgroup hierarchy ("/...") — strip the
    // leading slash so `join` appends rather than resets to absolute.
    Some(Path::new(CGROUP_ROOT).join(rel.trim_start_matches('/')))
}

/// Extract the cgroup-v2 path from `/proc/<pid>/cgroup` content. The v2
/// entry is the line beginning `0::`. Returns `None` on a v1-only host
/// (no `0::` line).
fn parse_self_cgroup(content: &str) -> Option<&str> {
    content.lines().find_map(|line| line.strip_prefix("0::"))
}

/// Core probe, split out from [`detect`] so it can be unit-tested
/// against a synthetic directory tree (`statfs` can't be faked). Given a
/// candidate `base` cgroup and whether the mount is cgroup2, decide the
/// backend by testing the two things that vary per host:
///
/// 1. **Delegation** — can we create (and remove) a child cgroup under
///    `base`? A writable subtree is exactly what delegation grants.
/// 2. **Kill primitive** — every non-root v2 cgroup exposes `cgroup.kill`
///    on kernels ≥ 5.14 and `cgroup.freeze` since 5.2. `base` is a
///    non-root (delegated) cgroup, so its interface files report the
///    kernel's capability.
fn probe_at(base: &Path, is_cgroup2: bool) -> SupervisionBackend {
    if !is_cgroup2 || !can_create_child(base) {
        return SupervisionBackend::ProcessGroup;
    }
    if base.join("cgroup.kill").exists() {
        SupervisionBackend::CgroupKill { base: base.to_path_buf() }
    } else if base.join("cgroup.freeze").exists() {
        SupervisionBackend::CgroupFreeze { base: base.to_path_buf() }
    } else {
        SupervisionBackend::ProcessGroup
    }
}

/// Probe-counter so concurrent probes (e.g. parallel tests sharing this
/// PID) never collide on the probe cgroup name.
static PROBE_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Can we create a child cgroup under `base`? Creates a uniquely-named
/// probe directory and immediately removes it. A failure means the
/// subtree isn't delegated to us (or isn't writable), so cgroups aren't
/// usable rootless here.
fn can_create_child(base: &Path) -> bool {
    let seq = PROBE_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let probe = base.join(format!("bougie.probe.{}.{seq}", std::process::id()));
    // Clear any leftover from a crashed prior probe with the same name.
    let _ = std::fs::remove_dir(&probe);
    match std::fs::create_dir(&probe) {
        Ok(()) => {
            let _ = std::fs::remove_dir(&probe);
            true
        }
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use tempfile::TempDir;

    #[test]
    fn parse_self_cgroup_v2_line() {
        let s = "0::/user.slice/user-1000.slice/user@1000.service/app.scope\n";
        assert_eq!(
            parse_self_cgroup(s),
            Some("/user.slice/user-1000.slice/user@1000.service/app.scope")
        );
    }

    #[test]
    fn parse_self_cgroup_hybrid_picks_v2_line() {
        // Hybrid host: v1 controller lines plus the unified `0::` line.
        let s = "3:cpu,cpuacct:/foo\n1:name=systemd:/bar\n0::/unified\n";
        assert_eq!(parse_self_cgroup(s), Some("/unified"));
    }

    #[test]
    fn parse_self_cgroup_v1_only_is_none() {
        assert_eq!(parse_self_cgroup("3:cpu:/foo\n1:name=systemd:/bar\n"), None);
    }

    #[test]
    fn not_cgroup2_is_process_group() {
        let td = TempDir::new().unwrap();
        fs::write(td.path().join("cgroup.kill"), "").unwrap();
        // Even with the kill file present, a non-cgroup2 mount is a
        // hard no.
        assert_eq!(probe_at(td.path(), false), SupervisionBackend::ProcessGroup);
    }

    #[test]
    fn cgroup2_with_kill_file_selects_cgroup_kill() {
        let td = TempDir::new().unwrap();
        fs::write(td.path().join("cgroup.kill"), "").unwrap();
        assert_eq!(
            probe_at(td.path(), true),
            SupervisionBackend::CgroupKill { base: td.path().to_path_buf() }
        );
    }

    #[test]
    fn cgroup2_with_only_freeze_selects_cgroup_freeze() {
        let td = TempDir::new().unwrap();
        fs::write(td.path().join("cgroup.freeze"), "0").unwrap();
        assert_eq!(
            probe_at(td.path(), true),
            SupervisionBackend::CgroupFreeze { base: td.path().to_path_buf() }
        );
    }

    #[test]
    fn cgroup2_without_any_kill_primitive_is_process_group() {
        let td = TempDir::new().unwrap();
        // Writable + cgroup2 but neither interface file present.
        assert_eq!(probe_at(td.path(), true), SupervisionBackend::ProcessGroup);
    }

    #[test]
    fn non_writable_base_is_process_group() {
        // An un-delegated cgroup: present, with cgroup.kill, but not
        // writable → we can't create a leaf, so cgroups are unusable.
        // Root ignores mode bits, so skip there.
        if rustix::process::geteuid().is_root() {
            return;
        }
        let td = TempDir::new().unwrap();
        let base = td.path().join("ro");
        fs::create_dir(&base).unwrap();
        fs::write(base.join("cgroup.kill"), "").unwrap();
        fs::set_permissions(&base, fs::Permissions::from_mode(0o500)).unwrap();

        let got = probe_at(&base, true);

        // Restore write so TempDir cleanup can remove it.
        let _ = fs::set_permissions(&base, fs::Permissions::from_mode(0o700));
        assert_eq!(got, SupervisionBackend::ProcessGroup);
    }

    #[test]
    fn backend_label_and_base_accessors() {
        let p = PathBuf::from("/sys/fs/cgroup/x");
        let k = SupervisionBackend::CgroupKill { base: p.clone() };
        assert_eq!(k.label(), "cgroup-kill");
        assert_eq!(k.base(), Some(p.as_path()));
        assert_eq!(SupervisionBackend::ProcessGroup.label(), "process-group");
        assert_eq!(SupervisionBackend::ProcessGroup.base(), None);
    }

    /// `detect()` must never panic and must return a real variant on any
    /// host (CI included). We don't assert *which* — that's
    /// environment-dependent — only that it runs cleanly.
    #[test]
    fn detect_runs_without_panicking() {
        let _ = detect();
    }

    #[test]
    fn create_leaf_is_idempotent_and_under_svc_dir() {
        let td = TempDir::new().unwrap();
        let leaf = create_leaf(td.path(), "redis").unwrap();
        assert!(leaf.is_dir());
        assert_eq!(leaf, td.path().join("bougie.svc").join("redis"));
        // Second call reuses the existing leaf, no error.
        assert_eq!(create_leaf(td.path(), "redis").unwrap(), leaf);
    }

    #[test]
    fn leaf_and_kill_supported_track_the_variant() {
        let p = PathBuf::from("/sys/fs/cgroup/x");
        let k = SupervisionBackend::CgroupKill { base: p.clone() };
        assert_eq!(k.leaf("redis"), Some(p.join("bougie.svc").join("redis")));
        assert!(k.kill_supported());

        let f = SupervisionBackend::CgroupFreeze { base: p };
        assert!(!f.kill_supported());

        assert_eq!(SupervisionBackend::ProcessGroup.leaf("redis"), None);
        assert!(!SupervisionBackend::ProcessGroup.kill_supported());
    }

    #[test]
    fn reap_stale_leaves_without_svc_dir_is_empty() {
        let td = TempDir::new().unwrap();
        assert!(reap_stale_leaves(td.path(), true).is_empty());
    }

    /// Real-kernel proof that `cgroup.kill` reaps a process that escaped
    /// the process group — the whole reason for the cgroup backend.
    /// Runs only where we actually have a delegated cgroup-v2 subtree
    /// with `cgroup.kill`; **skips loudly** (never a silent pass)
    /// elsewhere, e.g. CI runners without delegation.
    #[test]
    fn cgroup_kill_reaps_a_setsid_escapee() {
        use std::os::unix::process::CommandExt;
        use std::process::{Command, Stdio};

        let backend = detect();
        let SupervisionBackend::CgroupKill { base } = &backend else {
            eprintln!(
                "SKIP cgroup_kill_reaps_a_setsid_escapee: backend is {} \
                 (need a delegated cgroup-v2 subtree with cgroup.kill)",
                backend.label()
            );
            return;
        };

        let svc = format!("itest-escapee-{}", std::process::id());
        let leaf = create_leaf(base, &svc).expect("create leaf");
        let procs = leaf.join("cgroup.procs");

        // Spawn `sleep` that `setsid()`s in pre_exec → its own session
        // leader, escaping any process group. `killpg` of our group
        // can't reach it; only a cgroup-wide kill can. The Command's
        // child pid IS the sleep (setsid doesn't fork when not already a
        // group leader), so we can address it directly.
        let mut cmd = Command::new("sleep");
        cmd.arg("30").stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
        #[allow(unsafe_code)]
        unsafe {
            cmd.pre_exec(|| {
                rustix::process::setsid()
                    .map(|_| ())
                    .map_err(|e| std::io::Error::from_raw_os_error(e.raw_os_error()))
            });
        }
        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                let _ = std::fs::remove_dir(&leaf);
                eprintln!("SKIP cgroup_kill_reaps_a_setsid_escapee: spawn sleep: {e}");
                return;
            }
        };
        let pid = i32::try_from(child.id()).expect("pid fits in i32");

        // Move the escapee into the leaf. Capture the error — a failure
        // here means cgroups aren't usable the way the backend assumes,
        // which must surface, not silently pass.
        let moved = std::fs::write(&procs, pid.to_string());
        let kill_child = || {
            if let Some(p) = rustix::process::Pid::from_raw(pid) {
                let _ = rustix::process::kill_process(p, rustix::process::Signal::KILL);
            }
        };
        if let Err(e) = moved {
            kill_child();
            let _ = child.wait();
            let _ = std::fs::remove_dir(&leaf);
            panic!("moving escapee into leaf cgroup failed: {e}");
        }

        let members = std::fs::read_to_string(&procs).unwrap_or_default();
        let is_member = members.lines().any(|l| l.trim() == pid.to_string());
        if !is_member {
            kill_child();
            let _ = child.wait();
            let _ = std::fs::remove_dir(&leaf);
            panic!("escapee pid {pid} not in leaf cgroup.procs: {members:?}");
        }

        // The payoff: cgroup.kill sweeps it and the leaf is removed. A
        // cgroup is only removable once empty, so "leaf gone" proves the
        // escapee was killed.
        kill_and_remove(&leaf, true);
        let removed = !leaf.exists();
        kill_child(); // belt-and-suspenders if the assert is about to fail
        let _ = child.wait();
        assert!(removed, "leaf {} not removed — escapee survived cgroup.kill", leaf.display());
    }
}
