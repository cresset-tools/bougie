use crate::{
    error::SandboxError,
    sandbox::{ProtectHome, ProtectSystem, SandboxPolicy},
};
use std::path::Path;

/// Apply the sandbox policy using macOS sandbox profiles.
pub fn apply_sandbox(policy: &SandboxPolicy) -> Result<(), SandboxError> {
    let config = &policy.config;

    // 1. Resource limits (before sandbox)
    apply_resource_limits(config)?;

    // 2. macOS sandbox profile
    let profile = generate_profile(policy)?;
    macos_sandbox_sys::create_sandbox_with_parameters(profile, 0, &[]).map_err(SandboxError::MacOS)
}

fn apply_resource_limits(config: &crate::sandbox::Sandbox) -> Result<(), SandboxError> {
    let limits: [(Option<u64>, libc::c_int); 8] = [
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

fn generate_profile(policy: &SandboxPolicy) -> Result<String, SandboxError> {
    let config = &policy.config;
    let mut profile = String::from("(version 1)\n");
    profile.push_str("(allow default)\n");

    // ProtectSystem
    match config.protect_system {
        ProtectSystem::No => {}
        ProtectSystem::Yes => {
            profile.push_str("(deny file-write* (subpath \"/usr\"))\n");
            profile.push_str("(deny file-write* (subpath \"/System\"))\n");
            profile.push_str("(deny file-write* (subpath \"/Library\"))\n");
        }
        ProtectSystem::Full => {
            profile.push_str("(deny file-write* (subpath \"/usr\"))\n");
            profile.push_str("(deny file-write* (subpath \"/System\"))\n");
            profile.push_str("(deny file-write* (subpath \"/Library\"))\n");
            profile.push_str("(deny file-write* (subpath \"/etc\"))\n");
            profile.push_str("(deny file-write* (subpath \"/private/etc\"))\n");
        }
        ProtectSystem::Strict => {
            // Deny all writes first
            profile.push_str("(deny file-write*)\n");
            // Allow writes to explicitly allowed paths
            for path in &config.read_write_paths {
                if let Ok(canonical) = canonicalize_path(path) {
                    profile.push_str(&format!(
                        "(allow file-write* (subpath \"{}\"))\n",
                        escape_path(&canonical)
                    ));
                }
            }
        }
    }

    // ProtectHome
    match config.protect_home {
        ProtectHome::No => {}
        ProtectHome::Yes => {
            // Make home directories inaccessible
            if let Ok(home) = std::env::var("HOME") {
                profile.push_str(&format!(
                    "(deny file-read* file-write* (subpath \"{}\"))\n",
                    escape_path(&home)
                ));
            }
            profile.push_str("(deny file-read* file-write* (regex #\"^/Users/[^/]+\"))\n");
        }
        ProtectHome::ReadOnly => {
            // Make home directories read-only
            if let Ok(home) = std::env::var("HOME") {
                profile.push_str(&format!(
                    "(deny file-write* (subpath \"{}\"))\n",
                    escape_path(&home)
                ));
            }
            profile.push_str("(deny file-write* (regex #\"^/Users/[^/]+\"))\n");
        }
    }

    // ReadOnlyPaths - deny write access
    for path in &config.read_only_paths {
        if let Ok(canonical) = canonicalize_path(path) {
            profile.push_str(&format!(
                "(deny file-write* (subpath \"{}\"))\n",
                escape_path(&canonical)
            ));
        }
    }

    // InaccessiblePaths - deny all access
    for path in &config.inaccessible_paths {
        if let Ok(canonical) = canonicalize_path(path) {
            profile.push_str(&format!(
                "(deny file-read* file-write* (subpath \"{}\"))\n",
                escape_path(&canonical)
            ));
        }
    }

    // NoExecPaths - deny execution
    for path in &config.no_exec_paths {
        if let Ok(canonical) = canonicalize_path(path) {
            profile.push_str(&format!(
                "(deny process-exec* (subpath \"{}\"))\n",
                escape_path(&canonical)
            ));
        }
    }

    // ExecPaths - in strict mode, only allow execution from these paths
    // This requires denying all exec first, then allowing specific paths
    if !config.exec_paths.is_empty() {
        profile.push_str("(deny process-exec*)\n");
        for path in &config.exec_paths {
            if let Ok(canonical) = canonicalize_path(path) {
                profile.push_str(&format!(
                    "(allow process-exec* (subpath \"{}\"))\n",
                    escape_path(&canonical)
                ));
            }
        }
        // Also allow execution from system paths for basic functionality
        profile.push_str("(allow process-exec* (subpath \"/usr\"))\n");
        profile.push_str("(allow process-exec* (subpath \"/bin\"))\n");
        profile.push_str("(allow process-exec* (subpath \"/sbin\"))\n");
        profile.push_str("(allow process-exec* (subpath \"/System\"))\n");
    }

    // PrivateNetwork - deny all network access
    if config.private_network {
        profile.push_str("(deny network*)\n");
    }

    Ok(profile)
}

fn canonicalize_path(path: &Path) -> Result<String, SandboxError> {
    path.canonicalize()
        .map(|p| p.to_string_lossy().into_owned())
        .map_err(|e| {
            SandboxError::InvalidPath(format!("Failed to canonicalize path {:?}: {}", path, e))
        })
}

fn escape_path(path: &str) -> String {
    // Escape backslashes and double quotes for the sandbox profile S-expression
    path.replace('\\', "\\\\").replace('"', "\\\"")
}
