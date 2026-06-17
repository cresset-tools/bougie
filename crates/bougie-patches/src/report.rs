//! Outcome reporting for a patch application: per-file actions, aggregate
//! line stats, and (Phase B) the `PATCHES.txt` writer.

/// What happened to one file when a patch was applied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileOutcome {
    /// The on-disk path, relative to the apply base dir.
    pub path: String,
    /// The action taken.
    pub action: FileAction,
}

/// The on-disk effect of a single file diff.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileAction {
    /// An existing file was rewritten.
    Modified,
    /// A new file was created.
    Created,
    /// An existing file was removed.
    Deleted,
}

/// The result of applying one patch (which may touch several files).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApplyReport {
    /// The `-p` level that successfully applied.
    pub depth_used: usize,
    /// Per-file outcomes, in patch order.
    pub files: Vec<FileOutcome>,
    /// Total lines added across all hunks.
    pub lines_added: usize,
    /// Total lines deleted across all hunks.
    pub lines_deleted: usize,
    /// Total hunks applied across all files.
    pub hunks_applied: usize,
}
