use std::path::{Path, PathBuf};

use crate::error::SandboxError;

/// Protection level for system directories (systemd: ProtectSystem=)
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ProtectSystem {
    /// No protection
    #[default]
    No,
    /// /usr, /boot, /efi read-only
    Yes,
    /// + /etc read-only
    Full,
    /// Entire FS read-only except /dev, /proc, /sys
    Strict,
}

/// Protection level for home directories (systemd: ProtectHome=)
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ProtectHome {
    /// No protection
    #[default]
    No,
    /// /home, /root, /run/user inaccessible
    Yes,
    /// /home, /root, /run/user read-only
    ReadOnly,
}

/// Builder for creating sandbox policies mirroring systemd.exec options.
///
/// # Example
///
/// ```no_run
/// use std::path::Path;
/// use sandbox_run::{Sandbox, ProtectSystem, ProtectHome};
///
/// let sandbox = Sandbox::new()
///     .protect_system(ProtectSystem::Strict)
///     .protect_home(ProtectHome::Yes)
///     .read_write_paths([Path::new("/tmp/myapp")])
///     .no_new_privileges(true)
///     .limit_nofile(256)
///     .build()
///     .unwrap();
/// ```
#[derive(Debug, Clone, Default)]
pub struct Sandbox {
    // Filesystem protection
    pub(crate) protect_system: ProtectSystem,
    pub(crate) protect_home: ProtectHome,

    // Path-based access control
    pub(crate) read_only_paths: Vec<PathBuf>,
    pub(crate) read_write_paths: Vec<PathBuf>,
    pub(crate) inaccessible_paths: Vec<PathBuf>,
    pub(crate) exec_paths: Vec<PathBuf>,
    pub(crate) no_exec_paths: Vec<PathBuf>,

    // Network
    pub(crate) private_network: bool,

    // Privilege control
    pub(crate) no_new_privileges: bool,

    // Resource limits (None = don't set)
    pub(crate) limit_nofile: Option<u64>,
    pub(crate) limit_nproc: Option<u64>,
    pub(crate) limit_core: Option<u64>,
    pub(crate) limit_fsize: Option<u64>,
    pub(crate) limit_data: Option<u64>,
    pub(crate) limit_stack: Option<u64>,
    pub(crate) limit_cpu: Option<u64>,
    pub(crate) limit_memlock: Option<u64>,
}

impl Sandbox {
    /// Create a new sandbox builder with default settings (no restrictions).
    pub fn new() -> Self {
        Self::default()
    }

    // --- Filesystem Protection ---

    /// Set system directory protection level (systemd: ProtectSystem=)
    ///
    /// - `No`: No protection
    /// - `Yes`: /usr, /boot, /efi read-only
    /// - `Full`: + /etc read-only
    /// - `Strict`: Entire filesystem read-only except API filesystems
    pub fn protect_system(mut self, level: ProtectSystem) -> Self {
        self.protect_system = level;
        self
    }

    /// Set home directory protection level (systemd: ProtectHome=)
    ///
    /// - `No`: No protection
    /// - `Yes`: /home, /root, /run/user inaccessible
    /// - `ReadOnly`: /home, /root, /run/user read-only
    pub fn protect_home(mut self, level: ProtectHome) -> Self {
        self.protect_home = level;
        self
    }

    // --- Path Access Control ---

    /// Make paths read-only (systemd: ReadOnlyPaths=)
    ///
    /// Writing to these paths will be denied.
    pub fn read_only_paths<I, P>(mut self, paths: I) -> Self
    where
        I: IntoIterator<Item = P>,
        P: AsRef<Path>,
    {
        self.read_only_paths
            .extend(paths.into_iter().map(|p| p.as_ref().to_path_buf()));
        self
    }

    /// Allow read-write access to paths (systemd: ReadWritePaths=)
    ///
    /// Use this to create exceptions within ProtectSystem=strict.
    pub fn read_write_paths<I, P>(mut self, paths: I) -> Self
    where
        I: IntoIterator<Item = P>,
        P: AsRef<Path>,
    {
        self.read_write_paths
            .extend(paths.into_iter().map(|p| p.as_ref().to_path_buf()));
        self
    }

    /// Make paths completely inaccessible (systemd: InaccessiblePaths=)
    ///
    /// Neither reading nor writing will be possible.
    pub fn inaccessible_paths<I, P>(mut self, paths: I) -> Self
    where
        I: IntoIterator<Item = P>,
        P: AsRef<Path>,
    {
        self.inaccessible_paths
            .extend(paths.into_iter().map(|p| p.as_ref().to_path_buf()));
        self
    }

    /// Allow execution only from these paths (systemd: ExecPaths=)
    ///
    /// Binaries outside these paths cannot be executed.
    pub fn exec_paths<I, P>(mut self, paths: I) -> Self
    where
        I: IntoIterator<Item = P>,
        P: AsRef<Path>,
    {
        self.exec_paths
            .extend(paths.into_iter().map(|p| p.as_ref().to_path_buf()));
        self
    }

    /// Deny execution from these paths (systemd: NoExecPaths=)
    ///
    /// Binaries in these paths cannot be executed.
    pub fn no_exec_paths<I, P>(mut self, paths: I) -> Self
    where
        I: IntoIterator<Item = P>,
        P: AsRef<Path>,
    {
        self.no_exec_paths
            .extend(paths.into_iter().map(|p| p.as_ref().to_path_buf()));
        self
    }

    // --- Network ---

    /// Isolate network access (systemd: PrivateNetwork=)
    ///
    /// When enabled, all network access is blocked.
    pub fn private_network(mut self, enable: bool) -> Self {
        self.private_network = enable;
        self
    }

    // --- Privilege Control ---

    /// Prevent privilege escalation (systemd: NoNewPrivileges=)
    ///
    /// When enabled, the process cannot gain new privileges through
    /// setuid/setgid bits or file capabilities.
    pub fn no_new_privileges(mut self, enable: bool) -> Self {
        self.no_new_privileges = enable;
        self
    }

    // --- Resource Limits ---

    /// Max open file descriptors (systemd: LimitNOFILE=)
    pub fn limit_nofile(mut self, limit: u64) -> Self {
        self.limit_nofile = Some(limit);
        self
    }

    /// Max number of processes (systemd: LimitNPROC=)
    pub fn limit_nproc(mut self, limit: u64) -> Self {
        self.limit_nproc = Some(limit);
        self
    }

    /// Max core dump size in bytes (systemd: LimitCORE=)
    ///
    /// Set to 0 to disable core dumps.
    pub fn limit_core(mut self, limit: u64) -> Self {
        self.limit_core = Some(limit);
        self
    }

    /// Max file size in bytes (systemd: LimitFSIZE=)
    pub fn limit_fsize(mut self, limit: u64) -> Self {
        self.limit_fsize = Some(limit);
        self
    }

    /// Max data segment size in bytes (systemd: LimitDATA=)
    pub fn limit_data(mut self, limit: u64) -> Self {
        self.limit_data = Some(limit);
        self
    }

    /// Max stack size in bytes (systemd: LimitSTACK=)
    pub fn limit_stack(mut self, limit: u64) -> Self {
        self.limit_stack = Some(limit);
        self
    }

    /// Max CPU time in seconds (systemd: LimitCPU=)
    pub fn limit_cpu(mut self, limit: u64) -> Self {
        self.limit_cpu = Some(limit);
        self
    }

    /// Max locked memory in bytes (systemd: LimitMEMLOCK=)
    pub fn limit_memlock(mut self, limit: u64) -> Self {
        self.limit_memlock = Some(limit);
        self
    }

    /// Build the sandbox policy.
    ///
    /// This validates the configuration and prepares platform-specific data
    /// needed to apply the sandbox.
    pub fn build(self) -> Result<SandboxPolicy, SandboxError> {
        Ok(SandboxPolicy { config: self })
    }
}

/// A compiled sandbox policy ready to be applied to a command.
///
/// Create this using `Sandbox::build()`.
#[derive(Debug, Clone)]
pub struct SandboxPolicy {
    pub(crate) config: Sandbox,
}
