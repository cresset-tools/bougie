use std::io;
use std::os::fd::{AsRawFd, OwnedFd};
use std::path::{Path, PathBuf};

use crate::{
    error::SandboxError,
    sandbox::{ProtectHome, ProtectSystem, SandboxPolicy},
};
use landlock::{
    ABI, Access, AccessFs, AccessNet, BitFlags, CompatLevel, Compatible, PathBeneath, PathFd,
    Ruleset, RulesetAttr, RulesetCreated, RulesetCreatedAttr,
};

// libc's setrlimit takes the resource argument as
// `__rlimit_resource_t` (a u32 alias) on glibc but as `c_int` on
// musl — the typedef is glibc-only in the `libc` crate. Alias the
// per-target type once so the call site doesn't have to know which.
#[cfg(target_env = "gnu")]
type RlimitResource = libc::__rlimit_resource_t;
#[cfg(not(target_env = "gnu"))]
type RlimitResource = libc::c_int;

/// Everything the child needs, computed in the parent before `fork`.
///
/// Building a Landlock ruleset walks the filesystem and allocates; neither is
/// safe between `fork` and `exec` in a multi-threaded process, where only
/// async-signal-safe calls are allowed (another thread may have held the
/// allocator lock at the moment of the fork, and it is never released in the
/// child). So the ruleset is built here, and [`Prepared::apply`] is left with
/// three kinds of plain syscall: `prctl`, `setrlimit` and
/// `landlock_restrict_self`.
#[derive(Debug)]
pub struct Prepared {
    /// `NoNewPrivileges=` was asked for: failing to set it is an error.
    no_new_privs_required: bool,
    /// A ruleset was requested. Landlock needs `no_new_privs` to restrict an
    /// unprivileged process, so it is set even when the option was not asked
    /// for (and even when the kernel turns out not to enforce Landlock, as the
    /// `landlock` crate's own `restrict_self` does).
    landlock_requested: bool,
    limits: [Option<(RlimitResource, u64)>; 8],
    /// The ruleset to enforce. `None` when the policy has no filesystem or
    /// network restrictions, or when the kernel does not enforce Landlock
    /// (fail open, see `build_ruleset`).
    ruleset: Option<OwnedFd>,
}

/// Build the ruleset and collect the limits. Runs in the parent.
pub fn prepare(policy: &SandboxPolicy) -> Result<Prepared, SandboxError> {
    let config = &policy.config;
    let limits = [
        (config.limit_nofile, libc::RLIMIT_NOFILE),
        (config.limit_nproc, libc::RLIMIT_NPROC),
        (config.limit_core, libc::RLIMIT_CORE),
        (config.limit_fsize, libc::RLIMIT_FSIZE),
        (config.limit_data, libc::RLIMIT_DATA),
        (config.limit_stack, libc::RLIMIT_STACK),
        (config.limit_cpu, libc::RLIMIT_CPU),
        (config.limit_memlock, libc::RLIMIT_MEMLOCK),
    ]
    .map(|(value, resource)| value.map(|limit| (resource, limit)));

    let ruleset = build_ruleset(policy)?;
    Ok(Prepared {
        no_new_privs_required: config.no_new_privileges,
        landlock_requested: ruleset.is_some(),
        limits,
        ruleset: ruleset.and_then(Option::<OwnedFd>::from),
    })
}

impl Prepared {
    /// Confine the calling process. Runs in the child, after `fork` and before
    /// `exec`, and is async-signal-safe: no allocation, no locks, nothing but
    /// direct syscalls. `io::Error::last_os_error` only reads `errno`.
    pub fn apply(&self) -> io::Result<()> {
        // 1. NoNewPrivileges first (cannot be undone)
        if self.no_new_privs_required || self.landlock_requested {
            let ret = unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) };
            // Only fatal when it was asked for. When it is here for Landlock's
            // sake, `landlock_restrict_self` below reports the real failure.
            if ret != 0 && self.no_new_privs_required {
                return Err(io::Error::last_os_error());
            }
        }

        // 2. Resource limits
        for (resource, limit) in self.limits.iter().flatten() {
            let rlim = libc::rlimit {
                rlim_cur: *limit,
                rlim_max: *limit,
            };
            let ret = unsafe { libc::setrlimit(*resource, &rlim) };
            if ret != 0 {
                return Err(io::Error::last_os_error());
            }
        }

        // 3. Landlock filesystem + network restrictions
        if let Some(fd) = &self.ruleset {
            let ret =
                unsafe { libc::syscall(libc::SYS_landlock_restrict_self, fd.as_raw_fd(), 0u32) };
            if ret != 0 {
                return Err(io::Error::last_os_error());
            }
        }
        Ok(())
    }
}

/// Build the Landlock ruleset for `policy`, or `None` when the policy restricts
/// neither the filesystem nor the network.
fn build_ruleset(policy: &SandboxPolicy) -> Result<Option<RulesetCreated>, SandboxError> {
    let config = &policy.config;

    // Check if any restrictions apply
    let has_fs_restrictions = config.protect_system != ProtectSystem::No
        || config.protect_home != ProtectHome::No
        || !config.read_only_paths.is_empty()
        || !config.read_write_paths.is_empty()
        || !config.inaccessible_paths.is_empty()
        || !config.exec_paths.is_empty()
        || !config.no_exec_paths.is_empty();

    let has_net_restrictions = config.private_network;

    if !has_fs_restrictions && !has_net_restrictions {
        return Ok(None);
    }

    // Define access rights
    let read_access = AccessFs::ReadFile | AccessFs::ReadDir;
    let write_access = AccessFs::WriteFile
        | AccessFs::RemoveFile
        | AccessFs::RemoveDir
        | AccessFs::MakeChar
        | AccessFs::MakeDir
        | AccessFs::MakeReg
        | AccessFs::MakeSock
        | AccessFs::MakeFifo
        | AccessFs::MakeBlock
        | AccessFs::MakeSym
        | AccessFs::Truncate;
    let exec_access = AccessFs::Execute;
    // `Refer` (cross-directory rename/link) is ABI v2+ (kernel ≥5.19).
    // Landlock denies *all* reparenting by default whenever a ruleset is
    // in force, unless the ruleset both handles `Refer` and grants it on
    // the source and destination directories. Without it, every
    // `rename(2)`/`link(2)` that crosses a directory fails inside a
    // sandboxed service — even entirely within its own writable tree
    // (e.g. mariadb `RENAME TABLE` across per-database subdirs, or any
    // tmp-dir→final-dir atomic-move pattern). Granting it alongside the
    // write rights fixes that; `BestEffort` compatibility (the
    // `Ruleset::default()` level) silently drops it on pre-5.19 kernels,
    // where cross-directory reparenting isn't restricted anyway.
    let all_fs_access = read_access | write_access | exec_access | AccessFs::Refer;

    // Build ruleset
    let mut ruleset_builder = Ruleset::default().handle_access(all_fs_access)?;

    // Handle network restrictions if needed (Landlock v4+)
    if has_net_restrictions {
        ruleset_builder =
            ruleset_builder.handle_access(AccessNet::BindTcp | AccessNet::ConnectTcp)?;
    }

    let mut ruleset = ruleset_builder.create()?;

    // Paths carved entirely out of the base grant. Landlock is
    // allow-list only — the sole way to make a subtree inaccessible is
    // to never grant it and instead grant each of its siblings on the
    // way down (`grant_beneath_excluding`). This is what makes
    // `inaccessible_paths` and `ProtectHome` actually deny rather than
    // silently no-op.
    let mut deny: Vec<PathBuf> = config.inaccessible_paths.clone();
    if config.protect_home == ProtectHome::Yes {
        // systemd ProtectHome=yes: the standard home trees are
        // inaccessible. (`ReadOnly` is a no-op under Strict, where the
        // whole FS is already read-only; under a writable base it can't
        // be expressed by an allow-list and stays best-effort.)
        deny.push(PathBuf::from("/home"));
        deny.push(PathBuf::from("/root"));
        deny.push(PathBuf::from("/run/user"));
    }
    let deny = canonicalize_existing(&deny);

    // Base access granted across the whole filesystem minus `deny`.
    let base_access = match config.protect_system {
        // Strict: read + execute everywhere; writes only through the
        // explicit read_write_paths carve-ins below.
        ProtectSystem::Strict => read_access | exec_access,
        // No / Yes / Full: full access as the base. Yes/Full's intent
        // (system dirs read-only) can't be expressed by an allow-list
        // and stays best-effort; the security-relevant controls
        // (inaccessible_paths, ProtectHome=yes) are enforced via `deny`.
        ProtectSystem::No | ProtectSystem::Yes | ProtectSystem::Full => all_fs_access,
    };
    ruleset = grant_beneath_excluding(ruleset, Path::new("/"), base_access, &deny)?;

    // Carve-ins: explicit grants that re-open access to paths sitting
    // *under* a denied subtree — e.g. a service's own data/run/log dirs
    // and the read-only store, which live under $HOME and would
    // otherwise be hidden by ProtectHome=yes. Landlock unions rule
    // rights, and nothing broader grants these paths (their parents are
    // carved out), so these rules define their exact access.
    for path in &config.read_write_paths {
        if let Ok(fd) = PathFd::new(path) {
            ruleset = ruleset.add_rule(PathBeneath::new(fd, all_fs_access))?;
        }
    }
    for path in &config.read_only_paths {
        if let Ok(fd) = PathFd::new(path) {
            ruleset = ruleset.add_rule(PathBeneath::new(fd, read_access | exec_access))?;
        }
    }
    for path in &config.exec_paths {
        if let Ok(fd) = PathFd::new(path) {
            ruleset = ruleset.add_rule(PathBeneath::new(fd, exec_access))?;
        }
    }

    // `no_exec_paths` would need the exec grant carved around them the
    // same way `deny` carves the read grant; left best-effort for now
    // (no caller uses it). Network: handling BindTcp/ConnectTcp while
    // adding no net rules denies all TCP; not handling it allows all.

    // Fail OPEN. `BestEffort` (the default) downgrades to a no-op on
    // kernels < 5.13 or when Landlock is disabled: the created ruleset
    // then carries no descriptor and `Prepared::apply` skips
    // `landlock_restrict_self`. By request we proceed unconfined rather
    // than refusing to launch the service. The daemon probes
    // `landlock_available()` up front and logs once when services will
    // run without confinement. A genuine error from the kernel (not mere
    // non-enforcement) is still propagated.
    Ok(Some(ruleset))
}

/// Grant `access` to everything under `dir` except the subtrees listed
/// in `deny` (and everything beneath them).
///
/// Landlock has no "deny" rule: access is the union of every matching
/// `PathBeneath` grant, so a broad grant can't be narrowed by a more
/// specific one. To exclude a subtree we therefore never grant it and
/// instead grant each sibling along the path to it, recursing only into
/// the ancestor directories that actually contain a denied path. The
/// walk touches just those ancestors (e.g. `/` then `/home` for
/// `/home/<user>`), not the whole filesystem.
///
/// `deny` entries must be canonical absolute paths (see
/// `canonicalize_existing`).
fn grant_beneath_excluding(
    mut ruleset: RulesetCreated,
    dir: &Path,
    access: BitFlags<AccessFs>,
    deny: &[PathBuf],
) -> Result<RulesetCreated, SandboxError> {
    // If no denied path lives at or under `dir`, grant the whole subtree
    // in one rule and stop descending.
    if !deny.iter().any(|d| d.starts_with(dir)) {
        if let Ok(fd) = PathFd::new(dir) {
            ruleset = ruleset.add_rule(PathBeneath::new(fd, access))?;
        }
        return Ok(ruleset);
    }

    // Otherwise descend: grant children clear of any denied path,
    // recurse into children that contain one, and skip exact matches
    // (the holes themselves). If the directory can't be read we grant
    // nothing under it — failing toward more confinement, not less.
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Ok(ruleset);
    };
    for entry in entries.flatten() {
        // Resolve symlinks so the comparisons against the canonical
        // `deny` set are accurate (a symlinked `/home` must still match)
        // and so the grant targets the real path rather than the link.
        // A child that can't be canonicalized (e.g. a dangling symlink)
        // is left ungranted — again erring toward confinement.
        let Ok(child) = std::fs::canonicalize(entry.path()) else {
            continue;
        };
        if deny.contains(&child) {
            continue;
        }
        if deny.iter().any(|d| d.starts_with(&child)) {
            ruleset = grant_beneath_excluding(ruleset, &child, access, deny)?;
        } else if let Ok(fd) = PathFd::new(&child) {
            ruleset = ruleset.add_rule(PathBeneath::new(fd, access))?;
        }
    }
    Ok(ruleset)
}

/// Canonicalize deny paths and drop any that don't resolve. A path that
/// doesn't exist has nothing to hide, and canonical paths are required
/// so the prefix checks in `grant_beneath_excluding` line up with the
/// canonical entries returned by `read_dir`/`PathFd`.
fn canonicalize_existing(paths: &[PathBuf]) -> Vec<PathBuf> {
    paths
        .iter()
        .filter_map(|p| std::fs::canonicalize(p).ok())
        .collect()
}

/// Whether the running kernel actually enforces Landlock (Linux 5.13+
/// with Landlock enabled). This is a non-restricting probe: it builds a
/// throwaway ruleset under `HardRequirement` compatibility — which
/// errors instead of silently downgrading when the feature is missing —
/// and never calls `restrict_self`, so the calling process is
/// unaffected.
///
/// Intended for callers that fail open (see `apply_landlock`) but want
/// to warn an operator that spawned processes will run unconfined.
#[must_use]
pub fn landlock_available() -> bool {
    Ruleset::default()
        .set_compatibility(CompatLevel::HardRequirement)
        .handle_access(AccessFs::from_all(ABI::V1))
        .and_then(|r| r.create())
        .is_ok()
}
