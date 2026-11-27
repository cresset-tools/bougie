//! Atomic file replacement: write a uniquely named temp file in the
//! target's directory, then `rename` it over the target.
//!
//! Readers see either the old file or the new one, never a truncated or
//! half-written one, and a crash mid-write leaves the old file intact.
//! Use it for any file another process may read while bougie rewrites
//! it, or whose corruption would outlive the process that wrote it.
//!
//! Details every hand-rolled copy of this got wrong at least once:
//!
//! - **The temp name is unique per writer** (`.<name>.<pid>.<seq>.tmp`,
//!   opened with `create_new`). A fixed name (`foo.partial`, `foo.tmp`)
//!   lets two concurrent writers interleave into one temp file, rename a
//!   torn file into place, or fail the second rename with `ENOENT`. Two
//!   targets that share a stem (`pkg.json` / `pkg.etag` with
//!   `with_extension("partial")`) also shared a temp file.
//! - **The temp file lives in the target's directory**, so the rename
//!   stays on one filesystem and is atomic.
//! - **The final mode is set before the content becomes visible.** With
//!   [`Mode::Exact`] the temp file is created with that mode, so a secret
//!   is never briefly world-readable (chmod-after-write was).
//! - **On error the temp file is removed.**
//!
//! It does not create the parent directory (callers differ on the mode
//! they want for it) and does not serialize read-modify-write cycles:
//! two writers that each load, modify and save still lose one update.
//! That needs a lock around the whole cycle ([`crate::lock`]).

use eyre::{Result, WrapErr, eyre};
use std::fs::{self, File, OpenOptions};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

/// Permission bits for the replacement file. Ignored on non-Unix.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Mode {
    /// What a fresh `fs::write` would get (`0o666` minus umask). A
    /// replaced file does not keep its previous mode.
    #[default]
    Default,
    /// Exactly these bits, regardless of umask. The temp file is created
    /// with them, so use this for files holding secrets (`0o600`).
    Exact(u32),
    /// The current file's bits if it exists, else these. For files the
    /// user or the system owns (`/etc/hosts`, `app/etc/env.php`).
    PreserveOr(u32),
}

/// Options for [`write_with`].
#[derive(Debug, Clone, Copy, Default)]
pub struct Options {
    pub mode: Mode,
    /// `fsync` the content before the rename. Without it a power loss
    /// can leave the new name pointing at an empty file on some
    /// filesystems; use it for state that is expensive or impossible to
    /// regenerate. Costs one disk flush per write.
    pub sync: bool,
}

/// Atomically replace `path` with `bytes`, default mode, no fsync.
pub fn write(path: &Path, bytes: impl AsRef<[u8]>) -> Result<()> {
    write_with(path, bytes, Options::default())
}

/// Atomically replace `path` with `bytes` using `opts`.
pub fn write_with(path: &Path, bytes: impl AsRef<[u8]>, opts: Options) -> Result<()> {
    let dir = parent_dir(path)?;
    let name = path
        .file_name()
        .ok_or_else(|| eyre!("{} has no file name", path.display()))?
        .to_string_lossy()
        .into_owned();
    let mode = resolve_mode(path, opts.mode);
    let (tmp, mut file) = create_temp(dir, &name, mode)?;

    let result = (|| {
        file.write_all(bytes.as_ref())
            .wrap_err_with(|| format!("writing {}", tmp.display()))?;
        if opts.sync {
            file.sync_all()
                .wrap_err_with(|| format!("fsync {}", tmp.display()))?;
        }
        drop(file);
        fs::rename(&tmp, path)
            .wrap_err_with(|| format!("replacing {} with {}", path.display(), tmp.display()))
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

/// The directory the temp file goes in: the target's parent, or `.` for
/// a bare file name.
fn parent_dir(path: &Path) -> Result<&Path> {
    match path.parent() {
        Some(p) if p.as_os_str().is_empty() => Ok(Path::new(".")),
        Some(p) => Ok(p),
        None => Err(eyre!("{} has no parent directory", path.display())),
    }
}

/// Resolve [`Mode`] to concrete bits (`None` = leave to umask).
#[cfg(unix)]
fn resolve_mode(path: &Path, mode: Mode) -> Option<u32> {
    use std::os::unix::fs::PermissionsExt as _;
    match mode {
        Mode::Default => None,
        Mode::Exact(m) => Some(m),
        Mode::PreserveOr(fallback) => {
            Some(fs::metadata(path).map_or(fallback, |m| m.permissions().mode() & 0o7777))
        }
    }
}

#[cfg(not(unix))]
fn resolve_mode(_path: &Path, _mode: Mode) -> Option<u32> {
    None
}

/// Create `.<name>.<pid>.<seq>.tmp` in `dir`. `create_new` steps past a
/// leftover from a crashed process that had the same pid.
fn create_temp(dir: &Path, name: &str, mode: Option<u32>) -> Result<(PathBuf, File)> {
    static SEQ: AtomicU32 = AtomicU32::new(0);
    loop {
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        let tmp = dir.join(format!(".{name}.{}.{seq}.tmp", std::process::id()));
        let mut open = OpenOptions::new();
        open.write(true).create_new(true);
        #[cfg(unix)]
        if let Some(m) = mode {
            use std::os::unix::fs::OpenOptionsExt as _;
            open.mode(m);
        }
        match open.open(&tmp) {
            Ok(file) => {
                // The create mode is masked by umask; set the exact bits
                // (e.g. 0o755 under umask 077) before any content lands.
                #[cfg(unix)]
                if let Some(m) = mode {
                    use std::os::unix::fs::PermissionsExt as _;
                    if let Err(e) = file.set_permissions(fs::Permissions::from_mode(m)) {
                        let _ = fs::remove_file(&tmp);
                        return Err(e).wrap_err_with(|| format!("chmod {} {m:o}", tmp.display()));
                    }
                }
                #[cfg(not(unix))]
                let _ = mode;
                return Ok((tmp, file));
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e).wrap_err_with(|| format!("creating {}", tmp.display())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn leftovers(dir: &Path, keep: &[&str]) -> Vec<String> {
        fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| !keep.contains(&n.as_str()))
            .collect()
    }

    #[test]
    fn creates_and_replaces() {
        let td = TempDir::new().unwrap();
        let p = td.path().join("a.json");
        write(&p, "one").unwrap();
        assert_eq!(fs::read_to_string(&p).unwrap(), "one");
        write(&p, "two").unwrap();
        assert_eq!(fs::read_to_string(&p).unwrap(), "two");
        assert_eq!(leftovers(td.path(), &["a.json"]), Vec::<String>::new());
    }

    /// Concurrent writers of different content to one path: a reader
    /// always sees one writer's complete content, and every writer
    /// succeeds (a shared fixed temp name made renames fail with ENOENT).
    #[test]
    fn concurrent_writers_never_expose_a_partial_file() {
        use std::sync::atomic::AtomicBool;
        let td = TempDir::new().unwrap();
        let p = td.path().join("state.json");
        let bodies: Vec<String> = (0..4).map(|i| format!("{i}").repeat(64 * 1024)).collect();
        write(&p, &bodies[0]).unwrap();

        let stop = AtomicBool::new(false);
        let bad = std::thread::scope(|s| {
            for body in &bodies {
                let (p, stop) = (&p, &stop);
                s.spawn(move || {
                    while !stop.load(Ordering::Relaxed) {
                        write(p, body).unwrap();
                    }
                });
            }
            let bad = (0..5_000)
                .map(|_| fs::read_to_string(&p).unwrap())
                .find(|got| !bodies.contains(got));
            stop.store(true, Ordering::Relaxed);
            bad
        });
        assert!(bad.is_none(), "reader saw a partial file");
        assert_eq!(leftovers(td.path(), &["state.json"]), Vec::<String>::new());
    }

    /// Two targets sharing a stem must not share a temp file (the
    /// composer metadata cache wrote `pkg.json` and `pkg.etag` through
    /// one `pkg.partial`).
    #[test]
    fn same_stem_targets_do_not_collide() {
        let td = TempDir::new().unwrap();
        let json = td.path().join("pkg.json");
        let etag = td.path().join("pkg.etag");
        std::thread::scope(|s| {
            s.spawn(|| (0..500).for_each(|_| write(&json, "{\"ok\":true}").unwrap()));
            s.spawn(|| (0..500).for_each(|_| write(&etag, "W/\"abc\"").unwrap()));
        });
        assert_eq!(fs::read_to_string(&json).unwrap(), "{\"ok\":true}");
        assert_eq!(fs::read_to_string(&etag).unwrap(), "W/\"abc\"");
    }

    #[test]
    fn missing_parent_is_an_error_and_leaves_nothing() {
        let td = TempDir::new().unwrap();
        let missing = td.path().join("nope").join("x");
        assert!(write(&missing, "x").is_err());
        assert_eq!(leftovers(td.path(), &[]), Vec::<String>::new());
    }

    #[cfg(unix)]
    #[test]
    fn exact_mode_is_applied_regardless_of_umask_and_before_rename() {
        use std::os::unix::fs::PermissionsExt;
        let td = TempDir::new().unwrap();
        let p = td.path().join("secret.cnf");
        // A looser existing file is replaced by an exact-0600 one.
        fs::write(&p, "old").unwrap();
        fs::set_permissions(&p, fs::Permissions::from_mode(0o644)).unwrap();
        let opts = Options {
            mode: Mode::Exact(0o600),
            sync: true,
        };
        write_with(&p, "new", opts).unwrap();
        let mode = fs::metadata(&p).unwrap().permissions().mode() & 0o7777;
        assert_eq!(mode, 0o600);

        // Bits the umask would strip (group/other exec) still land.
        let tool = td.path().join("tool");
        let opts = Options {
            mode: Mode::Exact(0o755),
            sync: false,
        };
        write_with(&tool, "#!/bin/sh\n", opts).unwrap();
        let mode = fs::metadata(&tool).unwrap().permissions().mode() & 0o7777;
        assert_eq!(mode, 0o755);
    }

    #[cfg(unix)]
    #[test]
    fn preserve_mode_keeps_existing_bits_or_uses_fallback() {
        use std::os::unix::fs::PermissionsExt;
        let td = TempDir::new().unwrap();
        let p = td.path().join("env.php");
        let opts = Options {
            mode: Mode::PreserveOr(0o640),
            sync: false,
        };
        write_with(&p, "a", opts).unwrap();
        assert_eq!(
            fs::metadata(&p).unwrap().permissions().mode() & 0o7777,
            0o640
        );
        fs::set_permissions(&p, fs::Permissions::from_mode(0o600)).unwrap();
        write_with(&p, "b", opts).unwrap();
        assert_eq!(
            fs::metadata(&p).unwrap().permissions().mode() & 0o7777,
            0o600
        );
        assert_eq!(fs::read_to_string(&p).unwrap(), "b");
    }
}
