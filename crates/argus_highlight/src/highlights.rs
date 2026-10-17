//! Highlights of one whole text, split into per-line spans.

use std::ops::Range;
use std::sync::atomic::AtomicUsize;

use tree_sitter_highlight::{HighlightEvent, Highlighter as TsHighlighter};

use crate::error::HighlightError;
use crate::grammars;
use crate::kind::{HighlightKind, RECOGNIZED_NAMES};
use crate::language::Language;

/// One highlighted run of a line: byte offsets into the line text (without
/// its terminator) and what it is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Span {
    /// First byte, relative to the line start.
    pub start: u32,
    /// End byte (exclusive), relative to the line start.
    pub end: u32,
    /// The innermost capture covering these bytes.
    pub kind: HighlightKind,
}

impl Span {
    /// The byte range within the line.
    pub fn range(&self) -> Range<usize> {
        self.start as usize..self.end as usize
    }
}

/// Highlights of a whole blob, addressed by 1-based line number.
///
/// Lines are split exactly like the diff engine splits them: at `\n`, with
/// the terminator (`\n` or `\r\n`) excluded from every span. The parse
/// covers the full text, so constructs that open above a hunk (a block
/// comment, a multi-line string) color the lines inside it correctly.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Highlights {
    language: Language,
    /// Spans of line `i` (0-based) are `spans[offsets[i]..offsets[i + 1]]`.
    offsets: Vec<u32>,
    spans: Vec<Span>,
}

impl Highlights {
    /// The language the text was parsed as.
    pub fn language(&self) -> Language {
        self.language
    }

    /// Number of lines.
    pub fn line_count(&self) -> u32 {
        self.offsets.len().saturating_sub(1) as u32
    }

    /// Spans of 1-based line `line`, sorted and non-overlapping; empty when
    /// out of range.
    pub fn line(&self, line: u32) -> &[Span] {
        let Some(index) = (line as usize).checked_sub(1) else {
            return &[];
        };
        match (self.offsets.get(index), self.offsets.get(index + 1)) {
            (Some(&start), Some(&end)) => &self.spans[start as usize..end as usize],
            _ => &[],
        }
    }

    /// Spans of 1-based line `line` that overlap the byte range `bytes` of
    /// the line, found by binary search: a long minified line with
    /// thousands of spans costs only what its visible window holds.
    pub fn line_window(&self, line: u32, bytes: Range<usize>) -> &[Span] {
        let spans = self.line(line);
        let first = spans.partition_point(|span| span.end as usize <= bytes.start);
        let last = spans.partition_point(|span| (span.start as usize) < bytes.end);
        spans.get(first..last.max(first)).unwrap_or(&[])
    }

    /// Bytes held in memory, for cache budgets.
    pub fn memory_size(&self) -> usize {
        size_of::<Self>()
            + self.offsets.capacity() * size_of::<u32>()
            + self.spans.capacity() * size_of::<Span>()
    }
}

/// Parses `text` as `language` and splits the highlights into lines.
/// `cancel` is polled during parsing and highlighting; any non-zero value
/// stops the work with [`HighlightError::Cancelled`].
pub(crate) fn compute(
    highlighter: &mut TsHighlighter,
    language: Language,
    text: &str,
    cancel: &AtomicUsize,
) -> Result<Highlights, HighlightError> {
    let config = grammars::config(language).ok_or(HighlightError::Grammar(language.name()))?;
    let events = highlighter
        .highlight(config, text.as_bytes(), None, Some(cancel), |name| {
            Language::from_injection_name(name).and_then(grammars::config)
        })
        .map_err(convert)?;

    let mut splitter = LineSplitter::new(text, language);
    let mut stack: Vec<HighlightKind> = Vec::new();
    for event in events {
        match event.map_err(convert)? {
            HighlightEvent::HighlightStart(highlight) => {
                // Only recognized names are configured, so the index is in
                // range; a miss would just leave the text plain.
                let kind = RECOGNIZED_NAMES
                    .get(highlight.0)
                    .map_or(HighlightKind::Variable, |(_, kind)| *kind);
                stack.push(kind);
            }
            HighlightEvent::HighlightEnd => {
                stack.pop();
            }
            HighlightEvent::Source { start, end } => {
                if let Some(&kind) = stack.last() {
                    splitter.push(start..end, kind);
                }
            }
        }
    }
    Ok(splitter.finish())
}

fn convert(error: tree_sitter_highlight::Error) -> HighlightError {
    match error {
        tree_sitter_highlight::Error::Cancelled => HighlightError::Cancelled,
        error => HighlightError::Failed(error.to_string()),
    }
}

/// Number of lines of `text`, split like [`Highlights`] does.
pub(crate) fn line_count(text: &str) -> usize {
    let breaks = memchr_count(text.as_bytes());
    if text.is_empty() {
        0
    } else if text.ends_with('\n') {
        breaks
    } else {
        breaks + 1
    }
}

fn memchr_count(bytes: &[u8]) -> usize {
    bytes.iter().filter(|&&byte| byte == b'\n').count()
}

/// Cuts text-wide byte ranges (in increasing order) into per-line spans.
struct LineSplitter<'a> {
    text: &'a str,
    language: Language,
    /// Start byte of each line.
    starts: Vec<usize>,
    /// The line the next range starts at or after.
    line: usize,
    offsets: Vec<u32>,
    spans: Vec<Span>,
}

impl<'a> LineSplitter<'a> {
    fn new(text: &'a str, language: Language) -> Self {
        let mut starts = Vec::new();
        if !text.is_empty() {
            starts.push(0);
            starts.extend(
                text.match_indices('\n')
                    .map(|(at, _)| at + 1)
                    .filter(|&start| start < text.len()),
            );
        }
        Self {
            text,
            language,
            starts,
            line: 0,
            offsets: vec![0],
            spans: Vec::new(),
        }
    }

    fn next_start(&self, line: usize) -> usize {
        self.starts
            .get(line + 1)
            .copied()
            .unwrap_or(self.text.len())
    }

    /// End of line `line`'s content: before `\n` / `\r\n`.
    fn content_end(&self, line: usize) -> usize {
        let start = self.starts[line];
        let end = self.next_start(line);
        let content = &self.text[start..end];
        let content = content.strip_suffix('\n').unwrap_or(content);
        let content = content.strip_suffix('\r').unwrap_or(content);
        start + content.len()
    }

    /// Closes lines until `line` is the current one.
    fn advance_to(&mut self, line: usize) {
        while self.line < line {
            self.offsets.push(self.spans.len() as u32);
            self.line += 1;
        }
    }

    fn push(&mut self, range: Range<usize>, kind: HighlightKind) {
        if self.starts.is_empty() {
            return;
        }
        let mut at = range.start;
        while at < range.end {
            let line = self.line + self.starts[self.line..].partition_point(|&start| start <= at);
            let Some(line) = line.checked_sub(1) else {
                return;
            };
            self.advance_to(line);
            let line_start = self.starts[line];
            let piece_end = range.end.min(self.content_end(line));
            if at < piece_end {
                let start = (at - line_start) as u32;
                let end = (piece_end - line_start) as u32;
                let line_spans_start = *self.offsets.last().unwrap_or(&0) as usize;
                let on_this_line = self.spans.len() > line_spans_start;
                match self.spans.last_mut() {
                    // Adjacent runs of one kind merge into one span.
                    Some(last) if on_this_line && last.end == start && last.kind == kind => {
                        last.end = end;
                    }
                    _ => self.spans.push(Span { start, end, kind }),
                }
            }
            let next = self.next_start(line);
            if next <= at || line + 1 >= self.starts.len() {
                return;
            }
            at = next;
        }
    }

    fn finish(mut self) -> Highlights {
        let lines = self.starts.len();
        self.advance_to(lines);
        self.offsets.shrink_to_fit();
        self.spans.shrink_to_fit();
        Highlights {
            language: self.language,
            offsets: self.offsets,
            spans: self.spans,
        }
    }
}
