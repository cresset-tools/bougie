//! A sandboxed process execution library for Linux and macOS.
//!
//! This crate provides a `Command` API similar to `std::process::Command` but with
//! the ability to restrict process execution using platform-native sandboxing:
//!
//! - **Linux**: Uses [Landlock](https://landlock.io/) (requires kernel 5.13+)
//! - **macOS**: Uses the native sandbox framework
//!
//! The API mirrors systemd's execution environment options where applicable.
//!
//! # Example
//!
//! ```no_run
//! use std::path::Path;
//! use sandbox_run::{Command, Sandbox, ProtectSystem, ProtectHome};
//!
//! // Create a sandbox with systemd-style options
//! let sandbox = Sandbox::new()
//!     .protect_system(ProtectSystem::Strict)
//!     .protect_home(ProtectHome::Yes)
//!     .read_write_paths([Path::new("/tmp/myapp")])
//!     .private_network(true)
//!     .no_new_privileges(true)
//!     .limit_nofile(256)
//!     .build()
//!     .unwrap();
//!
//! // Run a command under the sandbox
//! let status = Command::new("myapp")
//!     .sandbox(sandbox)
//!     .status()
//!     .expect("failed to execute process");
//! ```
//!
//! # Systemd-Compatible Options
//!
//! | Sandbox Method | systemd Directive |
//! |----------------|-------------------|
//! | `protect_system()` | `ProtectSystem=` |
//! | `protect_home()` | `ProtectHome=` |
//! | `read_only_paths()` | `ReadOnlyPaths=` |
//! | `read_write_paths()` | `ReadWritePaths=` |
//! | `inaccessible_paths()` | `InaccessiblePaths=` |
//! | `exec_paths()` | `ExecPaths=` |
//! | `no_exec_paths()` | `NoExecPaths=` |
//! | `private_network()` | `PrivateNetwork=` |
//! | `no_new_privileges()` | `NoNewPrivileges=` |
//! | `limit_nofile()` | `LimitNOFILE=` |
//! | `limit_nproc()` | `LimitNPROC=` |
//! | `limit_core()` | `LimitCORE=` |
//! | `limit_fsize()` | `LimitFSIZE=` |
//!
//! # Platform Notes
//!
//! ## Linux
//!
//! The Linux implementation uses Landlock, which is an allow-list based sandboxing
//! mechanism. `ProtectSystem::Strict` works best as it makes the entire filesystem
//! read-only and you can add exceptions with `read_write_paths()`.
//!
//! ## macOS
//!
//! The macOS implementation uses the native sandbox framework with generated profiles.
//! All protection modes work well on macOS.

mod command;
mod error;
mod platform;
mod sandbox;

pub use command::Command;
pub use error::SandboxError;
pub use sandbox::{ProtectHome, ProtectSystem, Sandbox, SandboxPolicy};

/// Whether the running kernel enforces Landlock (Linux 5.13+ with
/// Landlock enabled). Callers that fail open when sandboxing is
/// unavailable can use this to warn that spawned processes will run
/// unconfined.
#[cfg(target_os = "linux")]
pub use platform::landlock_available;

/// Apply a compiled policy to the **current** process. Intended for
/// integrations that build their own `pre_exec` closure (e.g. when
/// running under a custom runtime that owns the child handle, like
/// `tokio::process::Command`). Must be called in the child after fork
/// and before exec.
pub fn apply_sandbox(policy: &SandboxPolicy) -> Result<(), SandboxError> {
    platform::apply_sandbox(policy)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use std::process::Stdio;

    #[test]
    fn test_command_without_sandbox() {
        let output = Command::new("echo")
            .arg("hello")
            .stdout(Stdio::piped())
            .output()
            .expect("failed to execute echo");

        assert!(output.status.success());
        assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "hello");
    }

    #[test]
    fn test_sandbox_builder_fluent_api() {
        let sandbox = Sandbox::new()
            .protect_system(ProtectSystem::Strict)
            .protect_home(ProtectHome::Yes)
            .read_write_paths([Path::new("/tmp")])
            .private_network(true)
            .no_new_privileges(true)
            .limit_nofile(256)
            .limit_nproc(10)
            .limit_core(0)
            .build()
            .expect("failed to build sandbox");

        // Just verify the sandbox builds successfully
        let _ = sandbox;
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn test_protect_home_blocks_home_access() {
        let sandbox = Sandbox::new()
            .protect_home(ProtectHome::Yes)
            .build()
            .expect("failed to build sandbox");

        // Try to list home directory
        let home = std::env::var("HOME").unwrap_or_else(|_| "/Users".to_string());
        let output = Command::new("ls")
            .arg(&home)
            .sandbox(sandbox)
            .stderr(Stdio::piped())
            .output()
            .expect("failed to execute ls");

        // Should fail because home is inaccessible
        assert!(!output.status.success());
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn test_private_network_blocks_network() {
        let sandbox = Sandbox::new()
            .private_network(true)
            .build()
            .expect("failed to build sandbox");

        // Try to make a network connection
        let output = Command::new("curl")
            .arg("--connect-timeout")
            .arg("1")
            .arg("http://127.0.0.1:1")
            .sandbox(sandbox)
            .stderr(Stdio::piped())
            .output()
            .expect("failed to execute curl");

        // Should fail because network is blocked
        // Note: curl may fail with different exit codes, but the sandbox should deny the connection
        assert!(!output.status.success());
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn test_inaccessible_paths() {
        // Create a test file
        let temp_dir = std::env::temp_dir();
        let test_file = temp_dir.join("sandbox_test_inaccessible.txt");
        std::fs::write(&test_file, "secret content").expect("failed to write test file");

        let sandbox = Sandbox::new()
            .inaccessible_paths([&test_file])
            .build()
            .expect("failed to build sandbox");

        let output = Command::new("cat")
            .arg(&test_file)
            .sandbox(sandbox)
            .stderr(Stdio::piped())
            .output()
            .expect("failed to execute cat");

        // Clean up
        let _ = std::fs::remove_file(&test_file);

        // Should fail because file is inaccessible
        assert!(!output.status.success());
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn test_read_only_paths() {
        let temp_dir = std::env::temp_dir();
        let test_file = temp_dir.join("sandbox_test_readonly.txt");
        std::fs::write(&test_file, "initial content").expect("failed to write test file");

        let sandbox = Sandbox::new()
            .read_only_paths([&temp_dir])
            .build()
            .expect("failed to build sandbox");

        // Try to write to the file - should fail
        let output = Command::new("sh")
            .arg("-c")
            .arg(format!("echo 'new content' > {}", test_file.display()))
            .sandbox(sandbox)
            .stderr(Stdio::piped())
            .output()
            .expect("failed to execute sh");

        // Clean up
        let _ = std::fs::remove_file(&test_file);

        // Should fail because the path is read-only
        assert!(!output.status.success());
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn test_protect_system_strict() {
        // With strict mode, only read_write_paths are writable
        let sandbox = Sandbox::new()
            .protect_system(ProtectSystem::Strict)
            .read_write_paths([Path::new("/tmp")])
            .build()
            .expect("failed to build sandbox");

        // Create a temp file to verify /tmp is writable
        let output = Command::new("sh")
            .arg("-c")
            .arg("echo test > /tmp/sandbox_test_strict.txt && cat /tmp/sandbox_test_strict.txt")
            .sandbox(sandbox)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .expect("failed to execute sh");

        // Clean up
        let _ = std::fs::remove_file("/tmp/sandbox_test_strict.txt");

        assert!(
            output.status.success(),
            "Command failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
