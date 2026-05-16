//! Advisory file locks per CLI.md §10.
//!
//! - Global: `$BOUGIE_HOME/state/locks/global.lock` (BSD `flock(2)` on
//!   Unix, `LockFileEx` on Windows — both via `std::fs::File::try_lock`).
//! - Per-project: `<project>/.bougie/.lock`. Serializes `sync` within
//!   one project.
//!
//! The PID of the holder is written into the lock file at acquire time
//! so a contender can surface it in the timeout diagnostic.

use crate::errors::BougieError;
use eyre::{Result, WrapErr};
use std::fs::{File, OpenOptions, TryLockError};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const POLL_INTERVAL: Duration = Duration::from_millis(100);

/// Holds an exclusive `flock(2)` over the file. The lock is released on
/// drop (kernel releases when the underlying fd is closed).
pub struct ExclusiveGuard {
    file: File,
    path: PathBuf,
}

impl ExclusiveGuard {
    /// Acquire an exclusive flock with a poll-loop timeout.
    pub fn acquire(path: &Path, timeout: Duration) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .wrap_err_with(|| format!("creating {}", parent.display()))?;
        }
        let mut file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(path)
            .wrap_err_with(|| format!("opening {}", path.display()))?;
        let deadline = Instant::now() + timeout;
        loop {
            match file.try_lock() {
                Ok(()) => break,
                Err(TryLockError::WouldBlock) => {}
                Err(TryLockError::Error(e)) => {
                    return Err(eyre::eyre!("acquiring lock {}: {e}", path.display()));
                }
            }
            if Instant::now() >= deadline {
                let pid = read_holder_pid(path).unwrap_or(0);
                return Err(BougieError::LockHeld {
                    path: path.display().to_string(),
                    pid,
                }
                .into());
            }
            std::thread::sleep(POLL_INTERVAL);
        }
        // Stamp our PID for the next contender's diagnostic.
        let _ = file.set_len(0);
        let _ = file.seek(SeekFrom::Start(0));
        let _ = writeln!(&mut file, "{}", std::process::id());
        let _ = file.flush();
        Ok(Self { file, path: path.to_path_buf() })
    }
}

impl Drop for ExclusiveGuard {
    fn drop(&mut self) {
        // Best-effort: clear the PID so the next holder sees an empty
        // file before they write. The kernel-level flock is released
        // when self.file's fd is closed (right after this).
        let _ = self.file.set_len(0);
    }
}

impl std::fmt::Debug for ExclusiveGuard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExclusiveGuard")
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}

fn read_holder_pid(path: &Path) -> Option<u32> {
    let mut file = File::open(path).ok()?;
    let mut buf = String::new();
    file.read_to_string(&mut buf).ok()?;
    buf.trim().parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn acquire_and_release() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("global.lock");
        let g = ExclusiveGuard::acquire(&path, Duration::from_millis(100)).unwrap();
        drop(g);
        let _again = ExclusiveGuard::acquire(&path, Duration::from_millis(100)).unwrap();
    }

    #[test]
    fn pid_written_to_file() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("global.lock");
        let _g = ExclusiveGuard::acquire(&path, Duration::from_millis(100)).unwrap();
        let pid = read_holder_pid(&path).unwrap();
        assert_eq!(pid, std::process::id());
    }

    #[test]
    fn second_acquire_times_out_with_pid() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("global.lock");
        let _a = ExclusiveGuard::acquire(&path, Duration::from_millis(100)).unwrap();
        let err =
            ExclusiveGuard::acquire(&path, Duration::from_millis(150)).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("concurrent operation conflict"), "got: {msg}");
        assert!(msg.contains("held by:  pid"), "got: {msg}");
        assert!(msg.contains(&std::process::id().to_string()));
    }

    #[test]
    fn truncate_on_release_clears_pid() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("global.lock");
        {
            let _g = ExclusiveGuard::acquire(&path, Duration::from_millis(100)).unwrap();
        }
        let bytes = std::fs::read(&path).unwrap();
        assert!(bytes.is_empty());
    }
}
