//! The diff excerpt shown to Claude around a comment: the hunk(s) that
//! contain the commented lines plus some extra context, as unified diff text
//! with line numbers (spec §6.1).

use std::fmt::Write as _;
use std::ops::RangeInclusive;

use argus_diff::{LineKind, Side, TextDiff};

/// Rows of unchanged code shown above and below the hunks that contain the
/// commented lines.
pub const EXTRA_CONTEXT_LINES: usize = 20;

/// Excerpts longer than this (in characters) are trimmed to the commented
/// lines ± [`TRIMMED_CONTEXT_LINES`].
pub const MAX_EXCERPT_CHARS: usize = 30_000;

/// Rows kept around the commented lines when an excerpt is trimmed.
pub const TRIMMED_CONTEXT_LINES: usize = 50;

/// The diff around a comment, already rendered as text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiffExcerpt {
    /// Unified diff text: one `@@` header, then one row per line with the
    /// old and new line numbers, the `+`/`-`/space op and the line text.
    /// Commented rows start with `>`.
    pub text: String,
    /// Why the excerpt is not the full hunk context, if it is not.
    pub trimmed: Option<ExcerptTrim>,
}

/// How an excerpt was cut down.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExcerptTrim {
    /// The hunk context was larger than [`MAX_EXCERPT_CHARS`] and was cut to
    /// the commented lines ± [`TRIMMED_CONTEXT_LINES`].
    Oversized {
        /// Size of the untrimmed excerpt, in characters.
        original_chars: usize,
    },
    /// The file has no line diff to show (binary, too large, or the code is
    /// gone); the excerpt is the snippet saved with the comment.
    SnippetOnly,
}

/// One row of the merged old/new view of a file.
#[derive(Clone, Copy, Debug)]
struct Row {
    kind: LineKind,
    old: Option<u32>,
    new: Option<u32>,
}

impl Row {
    fn line_on(&self, side: Side) -> Option<u32> {
        match side {
            Side::Old => self.old,
            Side::New => self.new,
        }
    }
}

impl DiffExcerpt {
    /// Builds the excerpt for lines `lines` on `side` of `diff`.
    ///
    /// Takes every hunk that overlaps the commented lines, plus up to
    /// [`EXTRA_CONTEXT_LINES`] rows above and below. A comment on lines
    /// outside every hunk (expanded context) gets the same padding around
    /// its own lines. Excerpts over [`MAX_EXCERPT_CHARS`] are trimmed to
    /// the commented lines ± [`TRIMMED_CONTEXT_LINES`].
    ///
    /// Returns `None` when none of the lines exist in `diff` (the code is
    /// gone); callers then fall back to [`DiffExcerpt::from_snippet`].
    pub fn from_diff(diff: &TextDiff, side: Side, lines: RangeInclusive<u32>) -> Option<Self> {
        let rows = merged_rows(diff);
        let commented: Vec<usize> = rows
            .iter()
            .enumerate()
            .filter(|(_, row)| row.line_on(side).is_some_and(|line| lines.contains(&line)))
            .map(|(index, _)| index)
            .collect();
        let (&first, &last) = (commented.first()?, commented.last()?);

        // Widen to the whole hunks that hold the commented rows.
        let mut start = first;
        let mut end = last;
        for hunk in diff.hunks() {
            let range = hunk.range_on(side);
            let overlaps = range.start <= *lines.end() && *lines.start() < range.end;
            if !overlaps {
                continue;
            }
            let hunk_rows = rows
                .iter()
                .enumerate()
                .filter(|(_, row)| in_hunk(row, hunk));
            for (index, _) in hunk_rows {
                start = start.min(index);
                end = end.max(index);
            }
        }

        let window = pad(start, end, EXTRA_CONTEXT_LINES, rows.len());
        let text = render(diff, &rows, window, &lines, side);
        let original_chars = text.chars().count();
        if original_chars <= MAX_EXCERPT_CHARS {
            return Some(Self {
                text,
                trimmed: None,
            });
        }
        let window = pad(first, last, TRIMMED_CONTEXT_LINES, rows.len());
        Some(Self {
            text: render(diff, &rows, window, &lines, side),
            trimmed: Some(ExcerptTrim::Oversized { original_chars }),
        })
    }

    /// An excerpt made of the snippet saved with the comment, for files
    /// without a line diff. `first_line` is the snippet's first line number
    /// on `side`, and `lines` the commented lines.
    pub fn from_snippet(
        snippet: &str,
        first_line: u32,
        side: Side,
        lines: RangeInclusive<u32>,
    ) -> Self {
        let mut text = String::new();
        let side_name = match side {
            Side::Old => "old",
            Side::New => "new",
        };
        let _ = writeln!(text, "(saved snippet, {side_name} side)");
        for (offset, line) in snippet.split('\n').enumerate() {
            let number = first_line + offset as u32;
            let mark = if lines.contains(&number) { '>' } else { ' ' };
            let _ = writeln!(text, "{mark}{number:>6} | {line}");
        }
        Self {
            text,
            trimmed: Some(ExcerptTrim::SnippetOnly),
        }
    }
}

fn in_hunk(row: &Row, hunk: &argus_diff::Hunk) -> bool {
    row.old
        .is_some_and(|line| hunk.range_on(Side::Old).contains(&line))
        || row
            .new
            .is_some_and(|line| hunk.range_on(Side::New).contains(&line))
}

fn pad(start: usize, end: usize, by: usize, len: usize) -> RangeInclusive<usize> {
    start.saturating_sub(by)..=(end + by).min(len.saturating_sub(1))
}

/// Every line of the file in diff order: unchanged gaps between hunks are
/// filled in as context rows, so the excerpt can extend past a hunk.
fn merged_rows(diff: &TextDiff) -> Vec<Row> {
    let old_count = diff.source(Side::Old).line_count();
    let mut rows = Vec::with_capacity(old_count as usize + diff.added() as usize);
    let (mut old, mut new) = (1u32, 1u32);
    let context = |old: u32, new: u32| Row {
        kind: LineKind::Context,
        old: Some(old),
        new: Some(new),
    };
    for hunk in diff.hunks() {
        // `old_start` is the first old line of the hunk, or for an empty
        // old side the line the insertion sits before; either way the
        // unchanged gap ends right before it.
        let gap_end = hunk.old_start.saturating_sub(1);
        while old <= gap_end && old <= old_count {
            rows.push(context(old, new));
            old += 1;
            new += 1;
        }
        for line in &diff.lines()[hunk.lines.clone()] {
            rows.push(Row {
                kind: line.kind,
                old: line.old_line,
                new: line.new_line,
            });
        }
        old = hunk.old_start + hunk.old_len;
        new = hunk.new_start + hunk.new_len;
    }
    while old <= old_count {
        rows.push(context(old, new));
        old += 1;
        new += 1;
    }
    rows
}

fn render(
    diff: &TextDiff,
    rows: &[Row],
    window: RangeInclusive<usize>,
    lines: &RangeInclusive<u32>,
    side: Side,
) -> String {
    let rows = &rows[window];
    let first_on = |pick: fn(&Row) -> Option<u32>| rows.iter().find_map(pick);
    let count_on = |pick: fn(&Row) -> Option<u32>| rows.iter().filter_map(pick).count() as u32;
    let old_len = count_on(|row| row.old);
    let new_len = count_on(|row| row.new);
    let header = argus_diff::Hunk {
        old_start: first_on(|row| row.old).unwrap_or(1),
        old_len,
        new_start: first_on(|row| row.new).unwrap_or(1),
        new_len,
        lines: 0..0,
    }
    .header();

    let width = rows
        .iter()
        .flat_map(|row| [row.old, row.new])
        .flatten()
        .max()
        .unwrap_or(1)
        .to_string()
        .len();
    let mut text = String::new();
    let _ = writeln!(text, "{header}");
    for row in rows {
        let line = argus_diff::DiffLine {
            kind: row.kind,
            old_line: row.old,
            new_line: row.new,
        };
        let commented = row.line_on(side).is_some_and(|line| lines.contains(&line));
        let mark = if commented { '>' } else { ' ' };
        let op = match row.kind {
            LineKind::Context => ' ',
            LineKind::Added => '+',
            LineKind::Removed => '-',
        };
        let number = |n: Option<u32>| n.map_or_else(String::new, |n| n.to_string());
        let _ = writeln!(
            text,
            "{mark}{:>width$} {:>width$} {op}{}",
            number(row.old),
            number(row.new),
            diff.line_text(&line),
        );
    }
    text
}
