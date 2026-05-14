#[cfg(target_os = "linux")]
mod linux;

#[cfg(target_os = "macos")]
mod macos;

use crate::{error::SandboxError, sandbox::SandboxPolicy};

/// Apply the sandbox policy to the current process.
///
/// This should be called in the child process after fork but before exec.
/// After this call, the process will be restricted according to the policy.
pub fn apply_sandbox(policy: &SandboxPolicy) -> Result<(), SandboxError> {
    #[cfg(target_os = "linux")]
    {
        linux::apply_sandbox(policy)
    }

    #[cfg(target_os = "macos")]
    {
        macos::apply_sandbox(policy)
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = policy;
        compile_error!("sandbox-run only supports Linux and macOS")
    }
}
