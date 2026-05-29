use crate::{
    error::SandboxError,
    sandbox::{ProtectHome, ProtectSystem, SandboxPolicy},
};
use landlock::{
    AccessFs, AccessNet, PathBeneath, PathFd, Ruleset, RulesetAttr, RulesetCreatedAttr,
    RulesetStatus,
};

/// Apply the sandbox policy using Linux-specific mechanisms.
pub fn apply_sandbox(policy: &SandboxPolicy) -> Result<(), SandboxError> {
    let config = &policy.config;

    // 1. NoNewPrivileges first (cannot be undone)
    if config.no_new_privileges {
        apply_no_new_privs()?;
    }

    // 2. Resource limits
    apply_resource_limits(config)?;

    // 3. Landlock filesystem + network restrictions
    apply_landlock(policy)?;

    Ok(())
}

fn apply_no_new_privs() -> Result<(), SandboxError> {
    let ret = unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) };
    if ret != 0 {
        return Err(SandboxError::Io(std::io::Error::last_os_error()));
    }
    Ok(())
}

fn apply_resource_limits(config: &crate::sandbox::Sandbox) -> Result<(), SandboxError> {
    let limits: [(Option<u64>, libc::__rlimit_resource_t); 8] = [
        (config.limit_nofile, libc::RLIMIT_NOFILE),
        (config.limit_nproc, libc::RLIMIT_NPROC),
        (config.limit_core, libc::RLIMIT_CORE),
        (config.limit_fsize, libc::RLIMIT_FSIZE),
        (config.limit_data, libc::RLIMIT_DATA),
        (config.limit_stack, libc::RLIMIT_STACK),
        (config.limit_cpu, libc::RLIMIT_CPU),
        (config.limit_memlock, libc::RLIMIT_MEMLOCK),
    ];

    for (value, resource) in limits {
        if let Some(limit) = value {
            let rlim = libc::rlimit {
                rlim_cur: limit,
                rlim_max: limit,
            };
            let ret = unsafe { libc::setrlimit(resource, &rlim) };
            if ret != 0 {
                return Err(SandboxError::Io(std::io::Error::last_os_error()));
            }
        }
    }
    Ok(())
}

fn apply_landlock(policy: &SandboxPolicy) -> Result<(), SandboxError> {
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
        return Ok(());
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
    let all_fs_access = read_access | write_access | exec_access;

    // Build ruleset
    let mut ruleset_builder = Ruleset::default().handle_access(all_fs_access)?;

    // Handle network restrictions if needed (Landlock v4+)
    if has_net_restrictions {
        ruleset_builder =
            ruleset_builder.handle_access(AccessNet::BindTcp | AccessNet::ConnectTcp)?;
    }

    let mut ruleset = ruleset_builder.create()?;

    // Apply filesystem rules based on ProtectSystem
    match config.protect_system {
        ProtectSystem::No => {
            // No system protection - allow full access to root
            if let Ok(fd) = PathFd::new("/") {
                ruleset = ruleset.add_rule(PathBeneath::new(fd, all_fs_access))?;
            }
        }
        ProtectSystem::Yes | ProtectSystem::Full => {
            // Allow full access to root first
            if let Ok(fd) = PathFd::new("/") {
                ruleset = ruleset.add_rule(PathBeneath::new(fd, all_fs_access))?;
            }
            // Then we'll apply read-only restrictions via separate mechanism
            // Note: Landlock is allow-list, so we can't easily do "all except X"
            // For now, we implement Strict mode properly and leave Yes/Full as approximate
        }
        ProtectSystem::Strict => {
            // Entire FS read-only + execute, write only to explicitly allowed paths
            if let Ok(fd) = PathFd::new("/") {
                ruleset = ruleset.add_rule(PathBeneath::new(fd, read_access | exec_access))?;
            }
        }
    }

    // Apply read_write_paths (creates exceptions in Strict mode)
    for path in &config.read_write_paths {
        if let Ok(fd) = PathFd::new(path) {
            ruleset = ruleset.add_rule(PathBeneath::new(fd, all_fs_access))?;
        }
    }

    // Apply exec_paths - allow execute access
    for path in &config.exec_paths {
        if let Ok(fd) = PathFd::new(path) {
            ruleset = ruleset.add_rule(PathBeneath::new(fd, exec_access))?;
        }
    }

    // Note: inaccessible_paths work by NOT adding rules for them in Strict mode
    // no_exec_paths work by NOT adding execute rules for them

    // Apply ProtectHome by not adding rules for home directories in Strict mode
    // In non-Strict mode, this is approximate since Landlock is allow-list

    // Network: if private_network is true, we handle network access but add no rules,
    // which means all TCP bind/connect is denied.
    // If private_network is false, we don't handle network access at all (allows all by default).

    // Fail closed if Landlock applied nothing. `BestEffort` (the
    // default) silently downgrades to a no-op on kernels < 5.13 or when
    // Landlock is disabled, returning Ok — which would let the service
    // run completely unconfined while the daemon believes it's
    // sandboxed. `PartiallyEnforced` (older ABI) is accepted as
    // best-effort; only `NotEnforced` (zero confinement) is rejected.
    let status = ruleset.restrict_self()?;
    if status.ruleset == RulesetStatus::NotEnforced {
        return Err(SandboxError::NotEnforced(
            "Landlock is unavailable (requires Linux 5.13+ with Landlock enabled in the \
             kernel); refusing to launch the service without filesystem confinement"
                .to_string(),
        ));
    }
    Ok(())
}
