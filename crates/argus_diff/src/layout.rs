//! Row model of the unified diff viewer (spec §4, §12).
//!
//! The viewer shows every file of a diff in one scrollable list. To lay out
//! only what is visible, each file is flattened into a list of uniform-height
//! [`Row`]s: a header, then hunk headers, lines and context expanders. Files
//! that were not diffed yet reserve [`estimate_rows`] placeholder rows, so the
//! total height is known without diffing anything.
//!
//! Everything here is pure and GPUI-free: the UI crate builds rows for a
//! loaded [`FileDiff`] with [`build_rows`] (on a worker thread), maps a row
//! back to its line with [`row_line`], and turns a gutter selection into a
//! comment range with [`select_range`].

use std::ops::Range;

use crate::file_diff::{DiffContent, FileDiff};
use crate::model::{FileStats, Side};
use crate::text::{LineKind, TextDiff};

/// Lines revealed by one click on "Expand up" / "Expand down".
pub const EXPAND_STEP: u32 = 20;

/// Rows a placeholder never goes below: the header and one row of body.
const MIN_FILE_ROWS: usize = 2;

/// Context lines a hunk usually carries (3 above, 3 below), for estimates.
const ESTIMATED_CONTEXT_ROWS: usize = 6;

/// One uniform-height row of a file in the viewer.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Row {
    /// Path, counts, collapse toggle and "Viewed" checkbox. Always the first
    /// row of a file.
    FileHeader,
    /// Space reserved for a file whose hunks are not loaded (yet, or any
    /// more after eviction).
    Placeholder,
    /// A one-row message instead of hunks: binary, too large, no content
    /// change, or a load error.
    Notice,
    /// `@@ -a,b +c,d @@` of hunk `hunk`.
    HunkHeader {
        /// Index into [`TextDiff::hunks`].
        hunk: u32,
    },
    /// A hunk line (or one soft-wrapped segment of it).
    Line {
        /// Index into [`TextDiff::lines`].
        index: u32,
        /// Soft-wrap segment, `0` for the first (or only) one.
        segment: u32,
    },
    /// An unchanged line revealed by expanding the context between hunks.
    Context {
        /// 1-based old line number.
        old: u32,
        /// 1-based new line number.
        new: u32,
        /// Soft-wrap segment, `0` for the first (or only) one.
        segment: u32,
    },
    /// "Expand up / down / all" between hunks: the hidden part of gap `gap`.
    Expander {
        /// Index into [`FileRows::gaps`].
        gap: u32,
    },
}

/// How lines are turned into rows.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct LayoutOptions {
    /// Display width of a tab stop, in columns (at least 1).
    pub tab_width: u32,
    /// Soft wrap at this many columns; `None` keeps one row per line (long
    /// lines scroll horizontally).
    pub wrap_columns: Option<u32>,
}

impl Default for LayoutOptions {
    fn default() -> Self {
        Self {
            tab_width: 4,
            wrap_columns: None,
        }
    }
}

/// Unchanged lines between two hunks (or before the first / after the last),
/// hidden until expanded.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Gap {
    /// 1-based old lines of the gap, end exclusive.
    pub old: Range<u32>,
    /// New line number of `old.start` (unchanged lines are equal on both
    /// sides, only shifted).
    pub new_start: u32,
}

impl Gap {
    /// Number of lines in the gap.
    pub fn len(&self) -> u32 {
        self.old.end - self.old.start
    }

    /// True when the hunks touch, so there is nothing to expand.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// New line number of old line `old` of this gap.
    pub fn new_line(&self, old: u32) -> u32 {
        self.new_start + (old - self.old.start)
    }
}

/// How much of a [`Gap`] the user revealed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct GapExpansion {
    /// Lines shown at the top of the gap (below the previous hunk).
    pub top: u32,
    /// Lines shown at the bottom of the gap (above the next hunk).
    pub bottom: u32,
}

impl GapExpansion {
    /// Lines still hidden in `gap`.
    pub fn hidden(&self, gap: &Gap) -> u32 {
        gap.len().saturating_sub(self.top + self.bottom)
    }
}

/// A context-expansion request.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Expand {
    /// Reveal [`EXPAND_STEP`] lines above the next hunk.
    Up,
    /// Reveal [`EXPAND_STEP`] lines below the previous hunk.
    Down,
    /// Reveal the whole gap.
    All,
}

/// Applies `how` to the expansion of `gap`.
pub fn expand(gap: &Gap, current: GapExpansion, how: Expand) -> GapExpansion {
    let hidden = current.hidden(gap);
    let mut next = current;
    match how {
        Expand::Up => next.bottom += hidden.min(EXPAND_STEP),
        Expand::Down => next.top += hidden.min(EXPAND_STEP),
        Expand::All => next.top += hidden,
    }
    next
}

/// Which expand directions make sense for gap `gap` of `gap_count`: the gap
/// before the first hunk only grows up, the one after the last only down.
/// Returns `(up, down)`.
pub fn expand_directions(gap: usize, gap_count: usize) -> (bool, bool) {
    let first = gap == 0;
    let last = gap + 1 == gap_count;
    (!last || first, !first)
}

/// The gaps of `diff`: one before each hunk plus one after the last; empty
/// when there are no hunks.
pub fn gaps(diff: &TextDiff) -> Vec<Gap> {
    let hunks = diff.hunks();
    let Some(last) = hunks.last() else {
        return Vec::new();
    };
    let mut gaps = Vec::with_capacity(hunks.len() + 1);
    let (mut old_at, mut new_at) = (1, 1);
    for hunk in hunks {
        // Before the first change the sides only differ by the shift the
        // hunk header reports.
        let start = old_at.min(hunk.old_start);
        gaps.push(Gap {
            old: start..hunk.old_start,
            new_start: hunk.new_start - (hunk.old_start - start),
        });
        old_at = hunk.old_start + hunk.old_len;
        new_at = hunk.new_start + hunk.new_len;
    }
    let old_count = diff.source(Side::Old).line_count();
    let end = (old_count + 1).max(old_at);
    debug_assert_eq!(old_at, last.old_start + last.old_len);
    gaps.push(Gap {
        old: old_at..end,
        new_start: new_at,
    });
    gaps
}

/// The rows of one loaded file, plus what the viewer needs to navigate them.
#[derive(Clone, Debug, Default)]
pub struct FileRows {
    /// Every row, starting with [`Row::FileHeader`].
    pub rows: Vec<Row>,
    /// Row indices of the hunk headers, ascending.
    pub hunk_rows: Vec<u32>,
    /// The gaps between hunks (see [`gaps`]).
    pub gaps: Vec<Gap>,
    /// Widest line in display columns (tabs expanded, CRLF mark included),
    /// for the horizontal scroll range.
    pub max_columns: u32,
    /// Decimal digits of the largest line number, for the gutter width.
    pub line_digits: u32,
}

impl FileRows {
    /// Approximate heap size, for the viewer's memory budget.
    pub fn memory_size(&self) -> usize {
        size_of::<Self>()
            + self.rows.capacity() * size_of::<Row>()
            + self.hunk_rows.capacity() * size_of::<u32>()
            + self.gaps.capacity() * size_of::<Gap>()
    }
}

/// Lays out a loaded file. `expansions` holds one entry per gap (missing
/// entries count as collapsed). Linear in the file's diff lines; run it off
/// the UI thread for the first layout of a file.
pub fn build_rows(
    diff: &FileDiff,
    expansions: &[GapExpansion],
    options: LayoutOptions,
) -> FileRows {
    let mut out = FileRows {
        rows: vec![Row::FileHeader],
        line_digits: 1,
        ..FileRows::default()
    };
    let DiffContent::Text(text) = &diff.content else {
        out.rows.push(Row::Notice);
        return out;
    };
    if text.hunks().is_empty() {
        out.rows.push(Row::Notice);
        return out;
    }
    let largest = text
        .source(Side::Old)
        .line_count()
        .max(text.source(Side::New).line_count());
    out.line_digits = digits(largest);
    out.gaps = gaps(text);
    let tab_width = options.tab_width.max(1);
    let wrap = options.wrap_columns.map(|columns| columns.max(1));

    let mut segments = |line_text: &str, crlf: bool, max_columns: &mut u32| -> u32 {
        let width = display_width(line_text, tab_width) + u32::from(crlf);
        *max_columns = (*max_columns).max(width);
        match wrap {
            Some(columns) => width.div_ceil(columns).max(1),
            None => 1,
        }
    };

    let push_gap = |out: &mut FileRows,
                    gap_index: usize,
                    segments: &mut dyn FnMut(&str, bool, &mut u32) -> u32| {
        let gap = &out.gaps[gap_index];
        if gap.is_empty() {
            return;
        }
        let expansion = expansions.get(gap_index).copied().unwrap_or_default();
        let top = expansion.top.min(gap.len());
        let bottom = expansion.bottom.min(gap.len() - top);
        let hidden = gap.len() - top - bottom;
        let new_side = text.source(Side::New);
        let (gap_old, gap_new_start) = (gap.old.clone(), gap.new_start);
        let mut context = |old: u32, out: &mut FileRows| {
            let new = gap_new_start + (old - gap_old.start);
            let line = new_side.line(new).unwrap_or_default();
            let count = segments(line, new_side.ends_with_crlf(new), &mut out.max_columns);
            for segment in 0..count {
                out.rows.push(Row::Context { old, new, segment });
            }
        };
        for old in gap_old.start..gap_old.start + top {
            context(old, out);
        }
        if hidden > 0 {
            out.rows.push(Row::Expander {
                gap: gap_index as u32,
            });
        }
        for old in gap_old.end - bottom..gap_old.end {
            context(old, out);
        }
    };

    for (hunk_index, hunk) in text.hunks().iter().enumerate() {
        push_gap(&mut out, hunk_index, &mut segments);
        out.hunk_rows.push(out.rows.len() as u32);
        out.rows.push(Row::HunkHeader {
            hunk: hunk_index as u32,
        });
        for index in hunk.lines.clone() {
            let line = &text.lines()[index];
            let count = segments(
                text.line_text(line),
                line_has_crlf(text, line.kind, line.old_line, line.new_line),
                &mut out.max_columns,
            );
            for segment in 0..count {
                out.rows.push(Row::Line {
                    index: index as u32,
                    segment,
                });
            }
        }
    }
    push_gap(&mut out, text.hunks().len(), &mut segments);
    out.rows.shrink_to_fit();
    out
}

/// Rows reserved for a file that is not loaded yet: its header, one hunk
/// header, the +/- lines and the usual context. A guess; the real layout
/// replaces it once the file is diffed.
pub fn estimate_rows(stats: Option<&FileStats>) -> usize {
    match stats {
        Some(FileStats::Text { added, removed }) if added + removed > 0 => {
            2 + (*added as usize + *removed as usize) + ESTIMATED_CONTEXT_ROWS
        }
        _ => MIN_FILE_ROWS,
    }
}

/// What a line row shows: its op, numbers, text and soft-wrap segment.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RowLine<'a> {
    /// Context, added or removed.
    pub kind: LineKind,
    /// 1-based old line number (context and removed lines).
    pub old_line: Option<u32>,
    /// 1-based new line number (context and added lines).
    pub new_line: Option<u32>,
    /// Text without its line terminator.
    pub text: &'a str,
    /// The line ends with `\r\n`.
    pub crlf: bool,
    /// Soft-wrap segment of this row.
    pub segment: u32,
    /// Index into [`TextDiff::lines`] for hunk lines, for word highlights.
    pub diff_index: Option<usize>,
}

/// The line shown by `row`, `None` for headers, expanders and notices.
pub fn row_line(text: &TextDiff, row: Row) -> Option<RowLine<'_>> {
    match row {
        Row::Line { index, segment } => {
            let line = text.lines().get(index as usize)?;
            Some(RowLine {
                kind: line.kind,
                old_line: line.old_line,
                new_line: line.new_line,
                text: text.line_text(line),
                crlf: line_has_crlf(text, line.kind, line.old_line, line.new_line),
                segment,
                diff_index: Some(index as usize),
            })
        }
        Row::Context { old, new, segment } => {
            let source = text.source(Side::New);
            Some(RowLine {
                kind: LineKind::Context,
                old_line: Some(old),
                new_line: Some(new),
                text: source.line(new).unwrap_or_default(),
                crlf: source.ends_with_crlf(new),
                segment,
                diff_index: None,
            })
        }
        _ => None,
    }
}

fn line_has_crlf(text: &TextDiff, kind: LineKind, old: Option<u32>, new: Option<u32>) -> bool {
    match (kind, old, new) {
        (LineKind::Removed, Some(old), _) => text.source(Side::Old).ends_with_crlf(old),
        (_, _, Some(new)) => text.source(Side::New).ends_with_crlf(new),
        _ => false,
    }
}

/// A line range on one side, as a comment anchors it (spec §7.1).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct LineRange {
    /// Old for ranges made of removed (and context) lines, new otherwise.
    pub side: Side,
    /// First line, 1-based.
    pub start: u32,
    /// Last line, 1-based, inclusive.
    pub end: u32,
}

/// Why a gutter selection cannot become a [`LineRange`]. The message is the
/// hint shown to the user.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum RangeError {
    /// The rows between the ends include a hunk header or hidden lines.
    #[error("A selection cannot cross a hunk header. Select lines within one hunk.")]
    CrossesHunk,
    /// One end is a removed line and the other an added line.
    #[error(
        "Select lines on one side: removed lines belong to the old side, added lines to the new side."
    )]
    MixedSides,
    /// An end is not a code line.
    #[error("Select code lines in the gutter.")]
    NotALine,
}

/// Turns a gutter selection from row `anchor` to row `head` (either order)
/// into a line range.
///
/// Both ends must be lines and every row between them must be a line too:
/// crossing a hunk header or an expander is rejected. The side is new when
/// both ends have a new line number (added or context lines), old when both
/// have an old one (removed or context lines, so a range of deleted lines is
/// on the old side), and the range is rejected otherwise.
pub fn select_range(
    text: &TextDiff,
    rows: &[Row],
    anchor: usize,
    head: usize,
) -> Result<LineRange, RangeError> {
    let (first, last) = (anchor.min(head), anchor.max(head));
    let line_at = |index: usize| {
        rows.get(index)
            .and_then(|row| row_line(text, *row))
            .ok_or(RangeError::NotALine)
    };
    let (start, end) = (line_at(first)?, line_at(last)?);
    if rows[first..=last]
        .iter()
        .any(|row| !matches!(row, Row::Line { .. } | Row::Context { .. }))
    {
        return Err(RangeError::CrossesHunk);
    }
    let range = |side: Side, a: u32, b: u32| LineRange {
        side,
        start: a.min(b),
        end: a.max(b),
    };
    match (start.new_line, end.new_line, start.old_line, end.old_line) {
        (Some(a), Some(b), _, _) => Ok(range(Side::New, a, b)),
        (_, _, Some(a), Some(b)) => Ok(range(Side::Old, a, b)),
        _ => Err(RangeError::MixedSides),
    }
}

/// Display width of `text` in columns, with tabs expanded to the next
/// multiple of `tab_width`. Every other character counts as one column (the
/// code font is monospace).
pub fn display_width(text: &str, tab_width: u32) -> u32 {
    let tab_width = tab_width.max(1);
    text.chars().fold(0, |column, ch| {
        column
            + match ch {
                '\t' => tab_width - column % tab_width,
                _ => 1,
            }
    })
}

/// A window of display columns of one line, ready to draw: tabs expanded,
/// control characters made visible, trailing whitespace and CRLF marked.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DisplayLine {
    /// The text to draw.
    pub text: String,
    /// Byte range of `text` showing trailing whitespace marks (`·`, `→`).
    pub trailing_whitespace: Option<Range<usize>>,
    /// Byte range of `text` showing the CRLF mark (`␍`).
    pub crlf: Option<Range<usize>>,
    /// `(source byte, display byte)` of each drawn source character, plus a
    /// final entry for the end of the source.
    offsets: Vec<(usize, usize)>,
    /// Display bytes covered by the source text (before the CRLF mark).
    source_end: usize,
}

impl DisplayLine {
    /// Maps a byte range of the source line (e.g. a word-diff range) to the
    /// drawn text, clipped to the window. `None` when nothing of it is drawn.
    pub fn map_range(&self, source: Range<usize>) -> Option<Range<usize>> {
        let map = |byte: usize| {
            let at = self.offsets.partition_point(|&(src, _)| src < byte);
            self.offsets
                .get(at)
                .map_or(self.source_end, |&(_, display)| display)
        };
        let range = map(source.start)..map(source.end);
        (!range.is_empty()).then_some(range)
    }
}

/// Lays out the display columns `columns` of `text` (soft-wrap segment or
/// horizontally scrolled window). `crlf` adds a `␍` mark after the text.
pub fn display_window(text: &str, crlf: bool, tab_width: u32, columns: Range<u32>) -> DisplayLine {
    let tab_width = tab_width.max(1);
    let trailing_start = text.trim_end_matches([' ', '\t']).len();
    let mut out = DisplayLine::default();
    let mut trailing: Option<usize> = None;
    let mut column = 0u32;
    for (byte, ch) in text.char_indices() {
        if column >= columns.end {
            break;
        }
        let width = match ch {
            '\t' => tab_width - column % tab_width,
            _ => 1,
        };
        let (start, end) = (column, column + width);
        column = end;
        if end <= columns.start {
            continue;
        }
        out.offsets.push((byte, out.text.len()));
        let is_trailing = byte >= trailing_start;
        if is_trailing && trailing.is_none() {
            trailing = Some(out.text.len());
        }
        let shown = end.min(columns.end) - start.max(columns.start);
        match ch {
            '\t' => {
                let marked = is_trailing && start >= columns.start;
                if marked {
                    out.text.push('→');
                }
                let spaces = shown - u32::from(marked);
                out.text.extend(std::iter::repeat_n(' ', spaces as usize));
            }
            ' ' if is_trailing => out.text.push('·'),
            // Control characters would draw nothing or garble the row; show
            // their Unicode control pictures instead.
            ch if (ch as u32) < 0x20 => {
                out.text
                    .push(char::from_u32(0x2400 + ch as u32).unwrap_or('?'));
            }
            '\u{7f}' => out.text.push('␡'),
            ch => out.text.push(ch),
        }
    }
    out.source_end = out.text.len();
    out.offsets.push((text.len(), out.source_end));
    out.trailing_whitespace = trailing.map(|start| start..out.source_end);
    if crlf && columns.contains(&display_width(text, tab_width)) {
        let start = out.text.len();
        out.text.push('␍');
        out.crlf = Some(start..out.text.len());
    }
    out
}

/// Decimal digits of `n` (at least 1).
fn digits(n: u32) -> u32 {
    n.checked_ilog10().map_or(1, |log| log + 1)
}
