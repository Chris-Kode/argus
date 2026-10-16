//! File list model: changed files, their kind and their +/- counts.

use argus_git::Sha;
use argus_git::objects::{EntryKind, ObjectReader, TreeChangeStatus, TreeEntry};

use crate::error::Result;
use crate::renames::Paired;

/// How a file changed between the merge-base and head.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ChangeKind {
    /// New file.
    Added,
    /// Removed file.
    Deleted,
    /// Content changed (the executable bit may have changed too).
    Modified,
    /// Moved, possibly with content and mode changes; see
    /// [`FileEntry::old_path`].
    Renamed,
    /// Only the executable bit changed; the content is identical.
    ModeChanged,
    /// The entry type changed: file ↔ symlink ↔ submodule.
    TypeChanged,
}

/// One side (old or new) of a diff.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Side {
    /// The merge-base side (left, `-`).
    Old,
    /// The head side (right, `+`).
    New,
}

/// A changed file in the three-dot diff.
#[derive(Clone, Debug, PartialEq)]
pub struct FileEntry {
    /// Position in the session's file list.
    pub index: usize,
    /// Path in head (the merge-base path for deletions).
    pub path: String,
    /// Path in the merge-base when the file was renamed.
    pub old_path: Option<String>,
    /// Change kind.
    pub kind: ChangeKind,
    /// The entry in the merge-base tree, `None` when added.
    pub old: Option<TreeEntry>,
    /// The entry in the head tree, `None` when deleted.
    pub new: Option<TreeEntry>,
}

impl FileEntry {
    pub(crate) fn from_paired(index: usize, paired: Paired) -> Self {
        let Paired { change, old_path } = paired;
        let kind = match change.status {
            _ if old_path.is_some() => ChangeKind::Renamed,
            TreeChangeStatus::Added => ChangeKind::Added,
            TreeChangeStatus::Deleted => ChangeKind::Deleted,
            TreeChangeStatus::Modified => match (change.old, change.new) {
                (Some(old), Some(new)) if category(old.kind) != category(new.kind) => {
                    ChangeKind::TypeChanged
                }
                (Some(old), Some(new)) if old.id == new.id && old.kind != new.kind => {
                    ChangeKind::ModeChanged
                }
                _ => ChangeKind::Modified,
            },
        };
        Self {
            index,
            path: change.path,
            old_path,
            kind,
            old: change.old,
            new: change.new,
        }
    }

    /// The path on `side`: `old_path` for the old side of a rename.
    pub fn path_on(&self, side: Side) -> &str {
        match (side, &self.old_path) {
            (Side::Old, Some(old_path)) => old_path,
            _ => &self.path,
        }
    }

    /// Mode on each side, when it changed (e.g. `100644` → `100755`).
    pub fn mode_change(&self) -> Option<(EntryKind, EntryKind)> {
        match (self.old, self.new) {
            (Some(old), Some(new)) if old.kind != new.kind => Some((old.kind, new.kind)),
            _ => None,
        }
    }

    /// True when both sides hold the same object, so there is no content
    /// diff (pure renames and mode changes).
    pub fn content_unchanged(&self) -> bool {
        matches!((self.old, self.new), (Some(old), Some(new)) if old.id == new.id)
    }
}

/// Files and executables diff against each other; links and submodules only
/// against their own kind.
fn category(kind: EntryKind) -> u8 {
    match kind {
        EntryKind::File | EntryKind::Executable => 0,
        EntryKind::Symlink => 1,
        EntryKind::Submodule => 2,
    }
}

/// The +/- summary of one file, streamed in after the file list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FileStats {
    /// Text diff: lines added and removed, as in `git diff --numstat`.
    Text {
        /// Lines added.
        added: u32,
        /// Lines removed.
        removed: u32,
    },
    /// Binary content on at least one side: no line counts, only sizes.
    Binary {
        /// Old blob size in bytes (0 when added).
        old_size: u64,
        /// New blob size in bytes (0 when deleted).
        new_size: u64,
    },
    /// Above [`DiffOptions::stats_max_bytes`](crate::DiffOptions): no counts.
    TooLarge {
        /// Old blob size in bytes (0 when added).
        old_size: u64,
        /// New blob size in bytes (0 when deleted).
        new_size: u64,
    },
    /// The counts could not be computed (e.g. a missing object).
    Failed(String),
}

/// Content of one side, ready to diff.
pub(crate) struct SideContent {
    pub bytes: Vec<u8>,
    pub size: u64,
}

/// Sizes of both sides without reading the content.
pub(crate) fn sizes(reader: &ObjectReader, entry: &FileEntry) -> Result<(u64, u64)> {
    Ok((side_size(reader, entry.old)?, side_size(reader, entry.new)?))
}

fn side_size(reader: &ObjectReader, side: Option<TreeEntry>) -> Result<u64> {
    match side {
        None => Ok(0),
        Some(entry) if entry.kind.is_blob() => Ok(reader.size(entry.id)?),
        Some(entry) => Ok(submodule_text(entry.id).len() as u64),
    }
}

/// Reads one side. Submodules diff like git shows them:
/// `Subproject commit <sha>`.
pub(crate) fn load_side(reader: &ObjectReader, side: Option<TreeEntry>) -> Result<SideContent> {
    let bytes = match side {
        None => Vec::new(),
        Some(entry) if entry.kind.is_blob() => reader.blob(entry.id)?,
        Some(entry) => submodule_text(entry.id).into_bytes(),
    };
    Ok(SideContent {
        size: bytes.len() as u64,
        bytes,
    })
}

fn submodule_text(id: Sha) -> String {
    format!("Subproject commit {id}\n")
}

/// Git's heuristic (`buffer_is_binary`): a NUL byte within the first 8000
/// bytes.
pub(crate) fn is_binary(bytes: &[u8]) -> bool {
    const FIRST_FEW_BYTES: usize = 8000;
    bytes[..bytes.len().min(FIRST_FEW_BYTES)].contains(&0)
}
