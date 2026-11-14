#[cfg(target_os = "linux")]
mod linux;

#[cfg(target_os = "macos")]
mod macos;

#[cfg(target_os = "linux")]
pub use linux::{Prepared, prepare};

#[cfg(target_os = "macos")]
pub use macos::{Prepared, prepare};

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
compile_error!("sandbox-run only supports Linux and macOS");

/// Whether the running kernel enforces Landlock (Linux 5.13+ with
/// Landlock enabled). See [`linux::landlock_available`].
#[cfg(target_os = "linux")]
pub use linux::landlock_available;
