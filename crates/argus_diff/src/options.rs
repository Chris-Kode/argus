//! Tunables of a diff session.

pub use crate::renames::RenameOptions;

/// Files above this size (on either side) are not diffed unless the user
/// asks for it ("Large file, diff not shown" / "Load anyway").
pub const DEFAULT_LARGE_FILE_BYTES: u64 = 10 * 1024 * 1024;

/// Files above this size get no +/- counts in the file list (reading them
/// would cost too much memory on the background pool).
pub const DEFAULT_STATS_MAX_BYTES: u64 = 64 * 1024 * 1024;

/// Default memory budget of the per-session [`FileDiff`](crate::FileDiff)
/// cache.
pub const DEFAULT_CACHE_BYTES: usize = 256 * 1024 * 1024;

/// Line diff algorithm.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Algorithm {
    /// Myers with git's heuristics: git's default, so counts and hunks match
    /// `git diff` most closely.
    #[default]
    Myers,
    /// Histogram (like `git diff --histogram`): often more readable, and
    /// usually faster.
    Histogram,
}

/// Settings fixed for the lifetime of a [`DiffSession`](crate::DiffSession).
#[derive(Clone, Debug, PartialEq)]
pub struct DiffOptions {
    /// Line diff algorithm.
    pub algorithm: Algorithm,
    /// Rename detection, `None` to report renames as delete + add.
    pub renames: Option<RenameOptions>,
    /// Lines of context around each change, like `git diff -U<n>`.
    pub context_lines: u32,
    /// Size above which a file is shown as "Large file, diff not shown".
    pub large_file_bytes: u64,
    /// Size above which the file list shows no +/- counts for a file. The
    /// counts of large files below it are still computed in the background,
    /// so the list matches `git diff --numstat`.
    pub stats_max_bytes: u64,
    /// Memory budget of the `FileDiff` LRU cache, in bytes.
    pub cache_bytes: usize,
    /// Enables intra-line (word) diffs of paired changed lines. They are
    /// still computed lazily, per hunk, on first access.
    pub word_diff: bool,
    /// Worker threads computing the +/- counts; `0` picks one per core
    /// (capped at 8).
    pub stats_workers: usize,
}

impl Default for DiffOptions {
    fn default() -> Self {
        Self {
            algorithm: Algorithm::default(),
            renames: Some(RenameOptions::default()),
            context_lines: 3,
            large_file_bytes: DEFAULT_LARGE_FILE_BYTES,
            stats_max_bytes: DEFAULT_STATS_MAX_BYTES,
            cache_bytes: DEFAULT_CACHE_BYTES,
            word_diff: true,
            stats_workers: 0,
        }
    }
}

impl DiffOptions {
    pub(crate) fn worker_count(&self) -> usize {
        if self.stats_workers > 0 {
            return self.stats_workers;
        }
        std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4)
            .clamp(1, 8)
    }
}
