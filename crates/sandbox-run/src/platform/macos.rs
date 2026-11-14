use crate::{
    error::SandboxError,
    sandbox::{ProtectHome, ProtectSystem, SandboxPolicy},
};
use std::ffi::CString;
use std::io;
use std::path::Path;

/// Everything the child needs, computed in the parent before `fork`.
///
/// Generating the profile canonicalizes paths, reads the environment and
/// allocates; none of that is safe between `fork` and `exec` in a
/// multi-threaded process. So the profile is generated and converted to a C
/// string here, and [`Prepared::apply`] is left with `setrlimit` and the one
/// libsandbox call.
#[derive(Debug)]
pub struct Prepared {
    limits: [Option<(libc::c_int, u64)>; 8],
    profile: CString,
}

/// Generate the profile and collect the limits. Runs in the parent.
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

    let profile = CString::new(generate_profile(policy)?)
        .map_err(|_| SandboxError::InvalidPath("a sandbox path contains a NUL byte".to_string()))?;
    Ok(Prepared { limits, profile })
}

impl Prepared {
    /// Confine the calling process. Runs in the child, after `fork` and before
    /// `exec`. Nothing on the success path allocates on the Rust side;
    /// `sandbox_init_with_parameters` itself is outside our control. A failure
    /// allocates to carry libsandbox's message, which is acceptable on a path
    /// that ends with the child exiting.
    pub fn apply(&self) -> io::Result<()> {
        // 1. Resource limits (before sandbox)
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

        // 2. macOS sandbox profile
        macos_sandbox_sys::init_with_profile(&self.profile, 0)
            .map_err(|msg| io::Error::other(SandboxError::MacOS(msg)))
    }
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
            // Deny all writes first; the explicit read_write_paths
            // re-allows are emitted later, after ProtectHome, so they
            // also override its read+write deny when an RW path lives
            // under $HOME (every bougie service's data/run/log/conf
            // does).
            profile.push_str("(deny file-write*)\n");
        }
    }

    // ProtectHome — emitted before path-specific allows so the latter
    // win under sbpl's last-match-wins semantics.
    //
    // We deny `file-read-data` rather than the broader `file-read*`:
    // the wildcard covers `file-read-metadata` too, which breaks
    // namei() traversal in POSIX `/bin/sh` and causes `cd
    // /allowed/path` to fail with ENOTDIR even when the target is in
    // a re-allowed subpath. (rabbitmq's `sbin/rabbitmq-env` shell
    // helper tripped this.) Denying read-data preserves content
    // confidentiality — file *bytes* in `$HOME` stay unreadable —
    // while letting the kernel stat path components for traversal,
    // which is what cd/chdir/dyld@rpath actually need. The cost is
    // that filenames under $HOME become visible; acceptable for a
    // dev sandbox where the real concern is leaking source / secrets
    // / credentials by content.
    match config.protect_home {
        ProtectHome::No => {}
        ProtectHome::Yes => {
            if let Ok(home) = std::env::var("HOME") {
                profile.push_str(&format!(
                    "(deny file-read-data file-write* (subpath \"{}\"))\n",
                    escape_path(&home)
                ));
            }
            profile.push_str("(deny file-read-data file-write* (regex #\"^/Users/[^/]+\"))\n");
        }
        ProtectHome::ReadOnly => {
            if let Ok(home) = std::env::var("HOME") {
                profile.push_str(&format!(
                    "(deny file-write* (subpath \"{}\"))\n",
                    escape_path(&home)
                ));
            }
            profile.push_str("(deny file-write* (regex #\"^/Users/[^/]+\"))\n");
        }
    }

    // ReadWritePaths — explicit read+write allow. Emitted AFTER
    // ProtectHome so the allow wins over any home-wide deny when an
    // RW path lives under $HOME. The `file-read-data` allow mirrors
    // the deny above so the override applies to the exact operation
    // sbpl matches; the `file-read*` half handles the other read
    // sub-ops not denied by ProtectHome.
    if matches!(config.protect_system, ProtectSystem::Strict) {
        for path in &config.read_write_paths {
            if let Ok(canonical) = canonicalize_path(path) {
                profile.push_str(&format!(
                    "(allow file-read* file-read-data file-write* (subpath \"{}\"))\n",
                    escape_path(&canonical)
                ));
            }
        }
    }

    // ReadOnlyPaths — explicit read-allow + write-deny. Without the
    // read-allow, dyld can't resolve dependent dylibs in
    // `$BOUGIE_HOME/store/*` (e.g. redis-server loading openssl via
    // `@rpath/libssl.3.dylib`). Both `file-read*` and the explicit
    // `file-read-data` are emitted: see the comment on ProtectHome
    // above for why matching the exact operation matters.
    for path in &config.read_only_paths {
        if let Ok(canonical) = canonicalize_path(path) {
            profile.push_str(&format!(
                "(allow file-read* file-read-data (subpath \"{}\"))\n",
                escape_path(&canonical)
            ));
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
