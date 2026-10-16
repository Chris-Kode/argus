//! The lazily computed diff of one file.

use argus_git::objects::ObjectReader;

use crate::error::Result;
use crate::model::{self, FileEntry, FileStats};
use crate::options::DiffOptions;
use crate::text::{self, TextDiff};

/// How hard to try when a file is large.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum LoadMode {
    /// Files above [`DiffOptions::large_file_bytes`] come back as
    /// [`DiffContent::TooLarge`].
    #[default]
    Normal,
    /// The "Load anyway" action: diff regardless of size.
    Force,
}

/// What can be shown for a file.
#[derive(Debug)]
pub enum DiffContent {
    /// A line diff. Pure renames and mode changes have no hunks.
    Text(TextDiff),
    /// "Binary file changed": no hunks, only sizes.
    Binary {
        /// Old size in bytes (0 when added).
        old_size: u64,
        /// New size in bytes (0 when deleted).
        new_size: u64,
    },
    /// "Large file, diff not shown", until loaded with [`LoadMode::Force`].
    TooLarge {
        /// Old size in bytes (0 when added).
        old_size: u64,
        /// New size in bytes (0 when deleted).
        new_size: u64,
    },
}

/// The diff of one file of the session.
#[derive(Debug)]
pub struct FileDiff {
    /// The file-list entry this diff belongs to.
    pub entry: FileEntry,
    /// Hunks, or why there are none.
    pub content: DiffContent,
}

impl FileDiff {
    /// The line diff, when the file is text and was loaded.
    pub fn text(&self) -> Option<&TextDiff> {
        match &self.content {
            DiffContent::Text(text) => Some(text),
            _ => None,
        }
    }

    /// The +/- summary matching this diff; `None` for the large-file
    /// placeholder, whose counts come from the file list instead.
    pub fn stats(&self) -> Option<FileStats> {
        match &self.content {
            DiffContent::Text(text) => Some(FileStats::Text {
                added: text.added(),
                removed: text.removed(),
            }),
            &DiffContent::Binary { old_size, new_size } => {
                Some(FileStats::Binary { old_size, new_size })
            }
            DiffContent::TooLarge { .. } => None,
        }
    }

    /// Approximate heap size, for the cache budget.
    pub fn memory_size(&self) -> usize {
        let paths =
            self.entry.path.capacity() + self.entry.old_path.as_ref().map_or(0, String::capacity);
        size_of::<Self>()
            + paths
            + match &self.content {
                DiffContent::Text(text) => text.memory_size(),
                _ => 0,
            }
    }
}

/// Computes the diff of `entry`. Blocking: run on a worker thread.
pub(crate) fn compute(
    reader: &ObjectReader,
    entry: &FileEntry,
    options: &DiffOptions,
    mode: LoadMode,
) -> Result<FileDiff> {
    let content = 'content: {
        if mode == LoadMode::Normal {
            let (old_size, new_size) = model::sizes(reader, entry)?;
            if old_size.max(new_size) > options.large_file_bytes {
                break 'content DiffContent::TooLarge { old_size, new_size };
            }
        }
        let old = model::load_side(reader, entry.old)?;
        let new = model::load_side(reader, entry.new)?;
        if model::is_binary(&old.bytes) || model::is_binary(&new.bytes) {
            break 'content DiffContent::Binary {
                old_size: old.size,
                new_size: new.size,
            };
        }
        DiffContent::Text(TextDiff::compute(
            options.algorithm,
            old.bytes,
            new.bytes,
            options.context_lines,
            options.word_diff,
        ))
    };
    Ok(FileDiff {
        entry: entry.clone(),
        content,
    })
}

/// Computes only the +/- summary of `entry`. Blocking: run on a worker
/// thread.
pub(crate) fn compute_stats(
    reader: &ObjectReader,
    entry: &FileEntry,
    options: &DiffOptions,
) -> Result<FileStats> {
    if entry.content_unchanged() {
        return Ok(FileStats::Text {
            added: 0,
            removed: 0,
        });
    }
    let (old_size, new_size) = model::sizes(reader, entry)?;
    if old_size.max(new_size) > options.stats_max_bytes {
        return Ok(FileStats::TooLarge { old_size, new_size });
    }
    let old = model::load_side(reader, entry.old)?;
    let new = model::load_side(reader, entry.new)?;
    if model::is_binary(&old.bytes) || model::is_binary(&new.bytes) {
        return Ok(FileStats::Binary { old_size, new_size });
    }
    let (added, removed) = text::count_lines(options.algorithm, &old.bytes, &new.bytes);
    Ok(FileStats::Text { added, removed })
}
