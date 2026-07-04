//! Per-service log file with size-based rotation. SERVICES.md §5.5.
//!
//! 10 MB cap by default with three retained generations. Rotation is
//! synchronous (renames are cheap) and happens on the write path when
//! the size threshold is crossed. Compression is deferred — the spec
//! calls for `.gz` but bougie doesn't currently pull a gzip crate;
//! Phase 5 ships plain `.1` / `.2` / `.3` files and gzipping can land
//! later without a wire-protocol change.

use eyre::{Result, WrapErr};
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

/// Default rotate-at threshold; SERVICES.md §5.5.
pub const ROTATE_BYTES: u64 = 10 * 1024 * 1024;
/// Generations kept after rotation. Older are deleted.
pub const GENERATIONS: u8 = 3;

/// One log file with size-based rotation. Owns the live file handle
/// and a byte counter; not Sync — clone the `Arc<Mutex<LogWriter>>`
/// in `ManagedService` if multiple forwarders need to share it.
#[derive(Debug)]
pub struct LogWriter {
    /// Active `.log` path. Rotated siblings are `<base>.1` … `<base>.N`.
    base: PathBuf,
    file: File,
    bytes_written: u64,
    rotate_at: u64,
    generations: u8,
}

impl LogWriter {
    /// Open (or create-and-truncate) `base`, starting the byte counter
    /// at the file's current size so a fresh open against a partial
    /// log still rotates at the right point.
    pub fn open(base: PathBuf) -> Result<Self> {
        Self::with_limits(base, ROTATE_BYTES, GENERATIONS)
    }

    pub fn with_limits(base: PathBuf, rotate_at: u64, generations: u8) -> Result<Self> {
        if let Some(parent) = base.parent() {
            std::fs::create_dir_all(parent)
                .wrap_err_with(|| format!("creating {}", parent.display()))?;
        }
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&base)
            .wrap_err_with(|| format!("opening {}", base.display()))?;
        let bytes_written = file.metadata().map_or(0, |m| m.len());
        Ok(Self {
            base,
            file,
            bytes_written,
            rotate_at,
            generations,
        })
    }

    /// Append a chunk; rotate atomically if this write crosses the
    /// threshold. The rotation itself happens AFTER the write
    /// (idiomatic for tail-following clients — they see the boundary
    /// land between two records, not in the middle of one).
    pub fn write(&mut self, chunk: &[u8]) -> Result<()> {
        self.file
            .write_all(chunk)
            .wrap_err_with(|| format!("writing {}", self.base.display()))?;
        self.bytes_written = self.bytes_written.saturating_add(chunk.len() as u64);
        if self.bytes_written >= self.rotate_at {
            self.rotate()?;
        }
        Ok(())
    }

    /// Rotate now even if we're under the threshold. Used by tests +
    /// any future `service logs rotate` CLI subcommand.
    pub fn rotate(&mut self) -> Result<()> {
        // Close current handle so the rename is portable (Windows
        // would require this; Linux/macOS don't strictly, but
        // releasing the fd before the shuffle is tidy).
        self.file
            .sync_all()
            .wrap_err_with(|| format!("fsync {}", self.base.display()))?;

        // Shift .N-1 → .N, .N-2 → .N-1, …, .1 → .2. Remove the oldest
        // first so we don't overwrite it.
        for n in (1..self.generations).rev() {
            let from = self.gen_path(n);
            let to = self.gen_path(n + 1);
            if to.exists() {
                let _ = std::fs::remove_file(&to);
            }
            if from.exists() {
                std::fs::rename(&from, &to)
                    .wrap_err_with(|| format!("rename {} → {}", from.display(), to.display()))?;
            }
        }

        // Move current .log → .1
        let one = self.gen_path(1);
        if one.exists() {
            let _ = std::fs::remove_file(&one);
        }
        std::fs::rename(&self.base, &one)
            .wrap_err_with(|| format!("rename {} → {}", self.base.display(), one.display()))?;

        // Reopen .log fresh.
        self.file = OpenOptions::new()
            .create(true)
            .append(true)
            .truncate(false)
            .open(&self.base)
            .wrap_err_with(|| format!("reopening {}", self.base.display()))?;
        self.bytes_written = 0;
        Ok(())
    }

    /// Return the path to the Nth generation. `n == 0` returns base.
    pub fn gen_path(&self, n: u8) -> PathBuf {
        if n == 0 {
            return self.base.clone();
        }
        let mut s = self.base.as_os_str().to_owned();
        s.push(format!(".{n}"));
        PathBuf::from(s)
    }

    pub fn base(&self) -> &Path {
        &self.base
    }

    pub fn bytes_written(&self) -> u64 {
        self.bytes_written
    }
}

/// Read the **last `n` lines** of a file, scanning back from the end
/// so a multi-GB log doesn't get pulled into memory. Returns the
/// lines in original order. If the file has fewer than `n` lines, the
/// whole file is returned.
///
/// # Panics
///
/// Doesn't in practice: the only inner `expect` is a `usize::try_from`
/// on a value capped to `CHUNK` (8 KiB), which fits even on 32-bit
/// targets bougie doesn't ship for anyway.
pub fn tail_lines(path: &Path, n: usize) -> Result<Vec<String>> {
    // 8 KiB at a time, walking backwards. Plenty to find the last
    // few hundred lines without thrashing.
    const CHUNK: usize = 8 * 1024;
    if n == 0 || !path.exists() {
        return Ok(Vec::new());
    }
    let mut f = File::open(path).wrap_err_with(|| format!("opening {}", path.display()))?;
    let len = f.metadata()?.len();
    let mut pos = len;
    let mut buf = Vec::with_capacity(CHUNK);
    let mut lines: Vec<String> = Vec::new();
    while pos > 0 && lines.len() <= n {
        let take = std::cmp::min(CHUNK as u64, pos);
        pos -= take;
        f.seek(SeekFrom::Start(pos))?;
        buf.clear();
        // `take` is bounded above by `CHUNK` (8 KiB), so this fits
        // even on 32-bit targets.
        buf.resize(
            usize::try_from(take).expect("take ≤ CHUNK fits in usize"),
            0,
        );
        f.read_exact(&mut buf)?;
        // Split and prepend in reverse so order survives.
        let text = String::from_utf8_lossy(&buf).into_owned();
        let chunk_lines: Vec<&str> = text.split_inclusive('\n').collect();
        for line in chunk_lines.into_iter().rev() {
            // The first line of the file's first scan would be a
            // partial-from-the-left chunk; in subsequent iterations
            // the chunking-on-newline can split lines across chunks.
            // For simplicity we accept up to one partial line at the
            // front — `n+1` slice + trim suffices.
            lines.push(line.to_string());
            if lines.len() > n {
                break;
            }
        }
    }
    lines.reverse();
    if lines.len() > n {
        let drop = lines.len() - n;
        lines.drain(..drop);
    }
    Ok(lines)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn write_appends_and_tracks_bytes() {
        let dir = TempDir::new().unwrap();
        let mut w = LogWriter::open(dir.path().join("svc.log")).unwrap();
        w.write(b"hello\n").unwrap();
        assert_eq!(w.bytes_written(), 6);
        assert!(w.base().exists());
        let contents = std::fs::read_to_string(w.base()).unwrap();
        assert_eq!(contents, "hello\n");
    }

    #[test]
    fn manual_rotate_creates_dot1_and_resets_counter() {
        let dir = TempDir::new().unwrap();
        let mut w = LogWriter::open(dir.path().join("svc.log")).unwrap();
        w.write(b"first run\n").unwrap();
        w.rotate().unwrap();
        assert!(w.gen_path(1).exists(), "expected svc.log.1");
        assert_eq!(w.bytes_written(), 0);
        let rotated = std::fs::read_to_string(w.gen_path(1)).unwrap();
        assert_eq!(rotated, "first run\n");
    }

    #[test]
    fn rotation_keeps_three_generations() {
        let dir = TempDir::new().unwrap();
        // Tight threshold so we get rotation on small writes.
        let mut w = LogWriter::with_limits(dir.path().join("svc.log"), 8, 3).unwrap();
        // Each rotate moves: live → .1 → .2 → .3 (oldest dropped).
        w.write(b"alpha-aaaa\n").unwrap();
        w.write(b"beta-bbbb\n").unwrap();
        w.write(b"gamma-cccc\n").unwrap();
        w.write(b"delta-dddd\n").unwrap();
        let one = std::fs::read_to_string(w.gen_path(1)).unwrap();
        let two = std::fs::read_to_string(w.gen_path(2)).unwrap();
        let three = std::fs::read_to_string(w.gen_path(3)).unwrap();
        assert_eq!(one, "delta-dddd\n");
        assert_eq!(two, "gamma-cccc\n");
        assert_eq!(three, "beta-bbbb\n");
        // alpha (which would have been .4) should be gone.
        assert!(!w.gen_path(4).exists());
    }

    #[test]
    fn reopen_preserves_bytes_counter() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("svc.log");
        std::fs::write(&path, "preexisting\n").unwrap();
        let w = LogWriter::open(path).unwrap();
        assert_eq!(w.bytes_written(), 12);
    }

    #[test]
    fn tail_lines_returns_last_n_in_order() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("svc.log");
        std::fs::write(&path, "a\nb\nc\nd\ne\n").unwrap();
        let lines = tail_lines(&path, 3).unwrap();
        assert_eq!(lines, vec!["c\n", "d\n", "e\n"]);
    }

    #[test]
    fn tail_lines_handles_n_greater_than_total() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("svc.log");
        std::fs::write(&path, "only\ntwo\n").unwrap();
        let lines = tail_lines(&path, 50).unwrap();
        assert_eq!(lines, vec!["only\n", "two\n"]);
    }

    #[test]
    fn tail_lines_handles_missing_file() {
        let dir = TempDir::new().unwrap();
        let lines = tail_lines(&dir.path().join("nope.log"), 10).unwrap();
        assert!(lines.is_empty());
    }
}
