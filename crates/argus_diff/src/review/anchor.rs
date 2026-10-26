//! Comment anchors: the code location a comment points at, with the context
//! hash and snippet used to re-find it after the code moves (spec §7.1, §7.3).

use std::ops::RangeInclusive;

use xxhash_rust::xxh3::Xxh3;

use crate::model::Side;
use crate::review::error::{ReviewError, ReviewResult};
use crate::text::SourceText;

/// Lines of context hashed (and kept in the snippet) above and below the
/// commented range.
pub const CONTEXT_LINES: u32 = 3;

/// Where a comment points in the diff, plus what is needed to re-anchor it
/// and to show it once its code is gone.
///
/// A comment on a whole file rather than on lines ([`CommentAnchor::file`])
/// has `start_line == end_line == 0`, an empty context hash and an empty
/// snippet.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommentAnchor {
    /// Path on the new side (the merge-base path for deleted files).
    pub file_path: String,
    /// Path on the old side, when the file was renamed.
    pub old_file_path: Option<String>,
    /// Side of the diff the line numbers refer to.
    pub side: Side,
    /// First commented line, 1-based; `0` for a file comment.
    pub start_line: u32,
    /// Last commented line, 1-based, inclusive; `0` for a file comment.
    pub end_line: u32,
    /// Head SHA (hex) when the comment was created.
    pub anchor_sha: String,
    /// [`context_hash`] of the commented lines and their context.
    pub context_hash: String,
    /// The raw commented lines plus their context, joined by `\n`; it starts
    /// at line [`CommentAnchor::snippet_start_line`].
    pub snippet: String,
}

impl CommentAnchor {
    /// Captures an anchor on `side` of a file whose text on that side is
    /// `source`: validates the range, then computes the context hash and the
    /// snippet.
    ///
    /// Fails with [`ReviewError::InvalidRange`] unless
    /// `1 <= start_line <= end_line <= source.line_count()`.
    pub fn capture(
        source: &SourceText,
        location: AnchorLocation,
        anchor_sha: impl Into<String>,
    ) -> ReviewResult<Self> {
        let AnchorLocation {
            file_path,
            old_file_path,
            side,
            start_line,
            end_line,
        } = location;
        let line_count = source.line_count();
        if start_line == 0 || end_line < start_line || end_line > line_count {
            return Err(ReviewError::InvalidRange {
                start_line,
                end_line,
                line_count,
            });
        }
        let window = context_window(start_line, end_line, line_count);
        let lines = || window.clone().map(|line| source.line(line).unwrap_or(""));
        Ok(Self {
            file_path,
            old_file_path,
            side,
            start_line,
            end_line,
            anchor_sha: anchor_sha.into(),
            context_hash: context_hash(lines()),
            snippet: lines().collect::<Vec<_>>().join("\n"),
        })
    }

    /// An anchor on the whole file at `file_path` rather than on lines.
    /// `side` is the side the file is read from: `Old` for a deleted file,
    /// `New` otherwise.
    pub fn file(
        file_path: String,
        old_file_path: Option<String>,
        side: Side,
        anchor_sha: impl Into<String>,
    ) -> Self {
        Self {
            file_path,
            old_file_path,
            side,
            start_line: 0,
            end_line: 0,
            anchor_sha: anchor_sha.into(),
            context_hash: String::new(),
            snippet: String::new(),
        }
    }

    /// Whether the comment is on the whole file rather than on lines.
    pub fn is_file(&self) -> bool {
        self.start_line == 0
    }

    /// The path of the file on the anchored side.
    pub fn path_on_side(&self) -> &str {
        match (self.side, &self.old_file_path) {
            (Side::Old, Some(old)) => old,
            _ => &self.file_path,
        }
    }

    /// The commented lines.
    pub fn lines(&self) -> RangeInclusive<u32> {
        self.start_line..=self.end_line
    }

    /// The line number of the snippet's first line, at creation time.
    pub fn snippet_start_line(&self) -> u32 {
        self.start_line.saturating_sub(CONTEXT_LINES).max(1)
    }

    /// Whether the comment covers more than one line.
    pub fn is_range(&self) -> bool {
        self.end_line > self.start_line
    }
}

/// The location part of an anchor, as picked in the gutter.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AnchorLocation {
    /// Path on the new side (the merge-base path for deleted files).
    pub file_path: String,
    /// Path on the old side, when the file was renamed.
    pub old_file_path: Option<String>,
    /// Side the line numbers refer to.
    pub side: Side,
    /// First line, 1-based.
    pub start_line: u32,
    /// Last line, 1-based, inclusive.
    pub end_line: u32,
}

/// The lines hashed for a comment on `start_line..=end_line`: the range plus
/// [`CONTEXT_LINES`] above and below, clamped to the file.
pub fn context_window(start_line: u32, end_line: u32, line_count: u32) -> RangeInclusive<u32> {
    let first = start_line.saturating_sub(CONTEXT_LINES).max(1);
    let last = end_line.saturating_add(CONTEXT_LINES).min(line_count);
    first..=last
}

/// Hashes a window of lines (xxh3, 64-bit, lowercase hex) after normalizing
/// whitespace: trailing whitespace is trimmed and every run of spaces or tabs
/// becomes one space. Re-indenting or realigning code keeps the hash; any
/// other edit, including adding or removing a line, changes it.
pub fn context_hash<'a>(lines: impl IntoIterator<Item = &'a str>) -> String {
    let mut hasher = Xxh3::new();
    let mut normalized = String::new();
    for (index, line) in lines.into_iter().enumerate() {
        normalized.clear();
        normalize_line(line, &mut normalized);
        if index > 0 {
            hasher.update(b"\n");
        }
        hasher.update(normalized.as_bytes());
    }
    format!("{:016x}", hasher.digest())
}

/// [`context_hash`] of the window around `start_line..=end_line` in `source`.
/// `None` when the range is not inside the file.
pub fn context_hash_at(source: &SourceText, start_line: u32, end_line: u32) -> Option<String> {
    let line_count = source.line_count();
    if start_line == 0 || end_line < start_line || end_line > line_count {
        return None;
    }
    let window = context_window(start_line, end_line, line_count);
    Some(context_hash(
        window.map(|line| source.line(line).unwrap_or("")),
    ))
}

pub(crate) fn normalize_line(line: &str, out: &mut String) {
    let mut in_blank = false;
    for ch in line.trim_end().chars() {
        if ch == ' ' || ch == '\t' {
            if !in_blank {
                out.push(' ');
                in_blank = true;
            }
        } else {
            out.push(ch);
            in_blank = false;
        }
    }
}
