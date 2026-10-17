//! Diff model for Argus: file list, hunks, line mapping and comment
//! anchoring (spec §5.1, §12).
//!
//! A [`DiffSession`] fixes the merge-base and head commits of a three-dot
//! diff. Opening a diff is staged so that thousands of files stay fast:
//!
//! 1. [`DiffSession::file_list`]: one tree-to-tree diff (`gix`) plus rename
//!    detection (git's algorithm, capped by candidate pairs) gives every
//!    changed path and its [`ChangeKind`].
//! 2. A worker pool then computes each file's +/- counts and binary flag,
//!    streamed through [`DiffSession::stats_updates`].
//! 3. [`DiffSession::file_diff`] computes one file's hunks lazily, the first
//!    time it is shown, and keeps it in a memory-bounded LRU cache.
//! 4. [`layout`] flattens a loaded file into the uniform-height rows the
//!    viewer virtualizes, and maps gutter selections back to line ranges.
//!
//! Nothing here runs on the caller's thread: list and hunks are computed on
//! tokio's blocking pool, counts on dedicated worker threads. Every job
//! holds a child of the session's [`CancelToken`]; dropping the last
//! session handle cancels them all.
//!
//! # Fidelity with `git diff`
//!
//! The engine is meant to agree with `git diff -M base...head`:
//!
//! - **File set and rename pairs**: rename detection is a port of git's
//!   `diffcore-rename` (exact renames, unique file names, then the
//!   similarity matrix with git's span-hash score, four candidates per
//!   destination and best-first assignment). The candidate-pair cap
//!   ([`RenameOptions::max_candidate_pairs`]) is git's `diff.renameLimit`
//!   squared; above it, exact and same-file-name renames are still found,
//!   like git.
//! - **+/- counts**: with [`Algorithm::Myers`] (the default) lines go
//!   through git's preprocessing (`xdl_cleanup_records`) and then a port of
//!   git's Myers with its cost heuristics (`xdl_split`), so counts equal
//!   `git diff --numstat`. [`Algorithm::Histogram`]
//!   is imara-diff's own and may differ from `git diff --histogram` by a few
//!   lines.
//! - **Hunks**: git's grouping rule (changes at most `2 × context` lines
//!   apart share a hunk) and header format, after imara-diff's indent
//!   heuristic. Where git's slider placement differs, both outputs are
//!   valid. **Comparison rule:** two hunk lists for a file are equivalent
//!   when they have the same +/- counts and, applied to the old text, both
//!   produce the new text; byte-identical output is not required.
//! - **Binary files**: git's NUL-in-the-first-8000-bytes heuristic;
//!   `.gitattributes` (`binary`, `-diff`) are not consulted.
//! - Typechanges (file ↔ symlink) are one [`ChangeKind::TypeChanged`]
//!   entry, as in `--numstat`, where git's patch output prints a deletion
//!   plus an addition. Line text is shown without its `\r\n` / `\n`.

mod cache;
mod error;
mod file_diff;
pub mod layout;
mod model;
mod options;
mod renames;
pub mod review;
mod session;
mod text;
mod xdiff;

pub use argus_git::CancelToken;
pub use argus_git::objects::{EntryKind, TreeEntry};
pub use error::{DiffError, Result};
pub use file_diff::{DiffContent, FileDiff, LoadMode};
pub use model::{ChangeKind, FileEntry, FileStats, Side};
pub use options::{
    Algorithm, DEFAULT_CACHE_BYTES, DEFAULT_LARGE_FILE_BYTES, DEFAULT_STATS_MAX_BYTES, DiffOptions,
    RenameOptions,
};
pub use session::{DiffSession, FileList, StatsEvent};
pub use text::{DiffLine, Hunk, HunkWords, LineKind, SourceText, TextDiff};
