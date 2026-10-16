//! Line diff of one text file: hunks, line ops, line mapping and lazy
//! intra-line (word) diffs.
//!
//! Line numbers in the public API are 1-based, like git and like comment
//! anchors (spec §7.1). Internally changes are kept as 0-based token ranges.

use std::ops::Range;
use std::sync::OnceLock;

use imara_diff::{Diff, InternedInput, Token};

use crate::model::Side;
use crate::options::Algorithm;
use crate::xdiff;

/// Lines longer than this are not word-diffed; the whole line is marked.
const WORD_DIFF_MAX_LINE_BYTES: usize = 4096;

/// The text of one side, split into lines.
#[derive(Debug)]
pub struct SourceText {
    text: String,
    /// Byte offset of each line start; one entry per line.
    line_starts: Vec<u32>,
    missing_newline_at_eof: bool,
}

impl SourceText {
    fn new(bytes: Vec<u8>) -> Self {
        // Diffing ran on the raw bytes; the lossy conversion is for display
        // and keeps every `\n`, so line numbers are unaffected.
        let text = match String::from_utf8(bytes) {
            Ok(text) => text,
            Err(err) => String::from_utf8_lossy(err.as_bytes()).into_owned(),
        };
        let mut line_starts = Vec::new();
        if !text.is_empty() {
            line_starts.push(0);
            line_starts.extend(
                text.match_indices('\n')
                    .map(|(at, _)| (at + 1) as u32)
                    .filter(|&start| (start as usize) < text.len()),
            );
        }
        let missing_newline_at_eof = !text.is_empty() && !text.ends_with('\n');
        Self {
            text,
            line_starts,
            missing_newline_at_eof,
        }
    }

    /// Number of lines.
    pub fn line_count(&self) -> u32 {
        self.line_starts.len() as u32
    }

    /// Text of 1-based line `line`, without its line terminator (`\n` or
    /// `\r\n`). `None` when out of range.
    pub fn line(&self, line: u32) -> Option<&str> {
        let index = (line as usize).checked_sub(1)?;
        let start = *self.line_starts.get(index)? as usize;
        let end = self
            .line_starts
            .get(index + 1)
            .map_or(self.text.len(), |&end| end as usize);
        let line = &self.text[start..end];
        let line = line.strip_suffix('\n').unwrap_or(line);
        Some(line.strip_suffix('\r').unwrap_or(line))
    }

    /// The whole text.
    pub fn as_str(&self) -> &str {
        &self.text
    }

    /// True when the last line has no trailing newline
    /// (`\ No newline at end of file`).
    pub fn missing_newline_at_eof(&self) -> bool {
        self.missing_newline_at_eof
    }

    fn memory_size(&self) -> usize {
        self.text.capacity() + self.line_starts.capacity() * size_of::<u32>()
    }
}

/// What a diff line is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum LineKind {
    /// Unchanged line shown for context (both line numbers set).
    Context,
    /// Line only in the new side (`+`).
    Added,
    /// Line only in the old side (`-`).
    Removed,
}

/// One line of a hunk.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct DiffLine {
    /// Line op.
    pub kind: LineKind,
    /// 1-based line number on the old side (context and removed lines).
    pub old_line: Option<u32>,
    /// 1-based line number on the new side (context and added lines).
    pub new_line: Option<u32>,
}

/// A hunk: a group of changes with surrounding context.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Hunk {
    /// First old line covered (1-based). For a hunk with no old lines, the
    /// line before which the new lines are inserted.
    pub old_start: u32,
    /// Number of old lines (context + removed).
    pub old_len: u32,
    /// First new line covered (1-based). For a hunk with no new lines, the
    /// line before which the old lines were removed.
    pub new_start: u32,
    /// Number of new lines (context + added).
    pub new_len: u32,
    /// Range of this hunk's lines in [`TextDiff::lines`].
    pub lines: Range<usize>,
}

impl Hunk {
    /// Git's header: `@@ -old_start,old_len +new_start,new_len @@`, with
    /// git's conventions (`,1` omitted; an empty side reports the line
    /// before the hunk).
    pub fn header(&self) -> String {
        fn range(start: u32, len: u32) -> String {
            match len {
                0 => format!("{},0", start.saturating_sub(1)),
                1 => start.to_string(),
                _ => format!("{start},{len}"),
            }
        }
        format!(
            "@@ -{} +{} @@",
            range(self.old_start, self.old_len),
            range(self.new_start, self.new_len)
        )
    }

    /// Lines of this hunk on `side` (1-based, end exclusive).
    pub fn range_on(&self, side: Side) -> Range<u32> {
        match side {
            Side::Old => self.old_start..self.old_start + self.old_len,
            Side::New => self.new_start..self.new_start + self.new_len,
        }
    }
}

/// A contiguous change: 0-based line ranges removed from old and added in
/// new. One of the ranges may be empty.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct Change {
    old: Range<u32>,
    new: Range<u32>,
}

impl Change {
    fn on(&self, side: Side) -> &Range<u32> {
        match side {
            Side::Old => &self.old,
            Side::New => &self.new,
        }
    }

    fn other(&self, side: Side) -> &Range<u32> {
        match side {
            Side::Old => &self.new,
            Side::New => &self.old,
        }
    }
}

/// Changed byte ranges inside the paired lines of one hunk.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HunkWords {
    /// `(index into TextDiff::lines, changed byte ranges in that line's
    /// text)`, sorted by line index. Unpaired lines are not listed.
    pub lines: Vec<(usize, Vec<Range<usize>>)>,
}

impl HunkWords {
    /// Changed byte ranges of the diff line at `line_index`, empty when the
    /// line was not paired.
    pub fn ranges(&self, line_index: usize) -> &[Range<usize>] {
        self.lines
            .binary_search_by_key(&line_index, |(index, _)| *index)
            .map_or(&[], |at| &self.lines[at].1)
    }
}

/// Full line diff of a text file.
#[derive(Debug)]
pub struct TextDiff {
    old: SourceText,
    new: SourceText,
    changes: Vec<Change>,
    hunks: Vec<Hunk>,
    lines: Vec<DiffLine>,
    added: u32,
    removed: u32,
    /// `None` when word diffs are disabled; otherwise one lazy slot per hunk.
    words: Option<Box<[OnceLock<HunkWords>]>>,
}

/// Line counts only, for the file list.
pub(crate) fn count_lines(algorithm: Algorithm, old: &[u8], new: &[u8]) -> (u32, u32) {
    let input = InternedInput::new(old, new);
    match algorithm {
        Algorithm::Myers => {
            let (removed, added) = git_myers(&input);
            let count = |flags: &[bool]| flags.iter().filter(|&&changed| changed).count() as u32;
            (count(&added), count(&removed))
        }
        Algorithm::Histogram => {
            let diff = Diff::compute(imara_diff::Algorithm::Histogram, &input);
            (diff.count_additions(), diff.count_removals())
        }
    }
}

/// Line diff of `input` with `algorithm`, before postprocessing.
fn line_diff(algorithm: Algorithm, input: &InternedInput<&[u8]>) -> Diff {
    match algorithm {
        Algorithm::Myers => {
            let (removed, added) = git_myers(input);
            diff_from_flags(&removed, &added)
        }
        Algorithm::Histogram => Diff::compute(imara_diff::Algorithm::Histogram, input),
    }
}

/// Git's Myers pipeline (`xdl_prepare_env` + `xdl_do_diff`): lines that do
/// not occur in the other file, and very frequent lines (blank lines,
/// braces) sitting among them, are marked changed before git's Myers
/// ([`xdiff::diff`]) runs on the rest. imara-diff does a similar pruning,
/// but counts occurrences after stripping the common prefix and suffix and
/// then runs a Myers with different heuristics, which makes its edit script
/// differ from git's on some inputs; doing both steps the git way keeps the
/// +/- counts identical to `git diff --numstat`.
///
/// Returns `(removed, added)` flags, one per line.
fn git_myers(input: &InternedInput<&[u8]>) -> (Vec<bool>, Vec<bool>) {
    const MAX_EQLIMIT: usize = 1024;
    let (before, after) = (&input.before, &input.after);
    let mut removed = vec![false; before.len()];
    let mut added = vec![false; after.len()];

    let mut prefix = 0;
    while prefix < before.len().min(after.len()) && before[prefix] == after[prefix] {
        prefix += 1;
    }
    let mut suffix = 0;
    while suffix < before.len().min(after.len()) - prefix
        && before[before.len() - 1 - suffix] == after[after.len() - 1 - suffix]
    {
        suffix += 1;
    }

    let tokens = input.interner.num_tokens() as usize;
    let mut in_before = vec![0usize; tokens];
    let mut in_after = vec![0usize; tokens];
    for token in before {
        in_before[token.0 as usize] += 1;
    }
    for token in after {
        in_after[token.0 as usize] += 1;
    }

    // Class per line of the middle part: 0 = no match, 1 = match,
    // 2 = matches very often.
    let classify = |side: &[Token], other_counts: &[usize], limit: usize| -> Vec<u8> {
        side.iter()
            .map(|token| match other_counts[token.0 as usize] {
                0 => 0,
                n if n >= limit => 2,
                _ => 1,
            })
            .collect()
    };
    let middle_before = &before[prefix..before.len() - suffix];
    let middle_after = &after[prefix..after.len() - suffix];
    let class_before = classify(
        middle_before,
        &in_after,
        bogo_sqrt(before.len()).min(MAX_EQLIMIT),
    );
    let class_after = classify(
        middle_after,
        &in_before,
        bogo_sqrt(after.len()).min(MAX_EQLIMIT),
    );

    let keep = |classes: &[u8], at: usize| {
        classes[at] == 1 || (classes[at] == 2 && !discard_multimatch(classes, at))
    };
    let reduce = |middle: &[Token], classes: &[u8], flags: &mut [bool]| {
        let mut tokens = Vec::with_capacity(middle.len());
        let mut index = Vec::with_capacity(middle.len());
        for (at, &token) in middle.iter().enumerate() {
            if keep(classes, at) {
                tokens.push(token);
                index.push(prefix + at);
            } else {
                flags[prefix + at] = true;
            }
        }
        (tokens, index)
    };
    let (reduced_before, index_before) = reduce(middle_before, &class_before, &mut removed);
    let (reduced_after, index_after) = reduce(middle_after, &class_after, &mut added);

    // git's own Myers on the reduced lines: imara-diff's would prune them a
    // second time and uses different cost heuristics (see `xdiff`).
    let mut reduced_removed = vec![false; reduced_before.len()];
    let mut reduced_added = vec![false; reduced_after.len()];
    xdiff::diff(
        &reduced_before,
        &reduced_after,
        &mut reduced_removed,
        &mut reduced_added,
    );
    for (&line, &changed) in index_before.iter().zip(&reduced_removed) {
        removed[line] |= changed;
    }
    for (&line, &changed) in index_after.iter().zip(&reduced_added) {
        added[line] |= changed;
    }
    (removed, added)
}

/// git: `xdl_bogosqrt`.
fn bogo_sqrt(mut n: usize) -> usize {
    let mut root = 1;
    while n > 0 {
        root <<= 1;
        n >>= 2;
    }
    root
}

/// git: `xdl_clean_mmatch`. A frequent line is discarded when it sits in a
/// run made mostly of lines without a match.
fn discard_multimatch(classes: &[u8], at: usize) -> bool {
    const WINDOW: usize = 100;
    const KEEP_RUN: usize = 4;
    let start = at.saturating_sub(WINDOW);
    let end = (at + WINDOW).min(classes.len() - 1);

    let (mut unmatched_before, mut frequent_before) = (0, 1);
    for &class in classes[start..at].iter().rev() {
        match class {
            0 => unmatched_before += 1,
            2 => frequent_before += 1,
            _ => break,
        }
    }
    if unmatched_before == 0 {
        return false;
    }
    let (mut unmatched_after, mut frequent_after) = (0, 1);
    for &class in &classes[at + 1..=end] {
        match class {
            0 => unmatched_after += 1,
            2 => frequent_after += 1,
            _ => break,
        }
    }
    if unmatched_after == 0 {
        return false;
    }
    let unmatched = unmatched_before + unmatched_after;
    let frequent = frequent_before + frequent_after;
    frequent * KEEP_RUN < frequent + unmatched
}

/// Builds an imara [`Diff`] with exactly the given change flags, so that its
/// postprocessing (slider and indent heuristics) can run on them. Each kept
/// line pair gets one shared, unique token and each changed line its own,
/// which leaves Myers a single possible answer.
fn diff_from_flags(removed: &[bool], added: &[bool]) -> Diff {
    let mut next = 0u32;
    let mut fresh = || {
        next += 1;
        Token(next - 1)
    };
    let mut before = Vec::with_capacity(removed.len());
    let mut after = Vec::with_capacity(added.len());
    let (mut i, mut j) = (0, 0);
    while i < removed.len() || j < added.len() {
        if i < removed.len() && removed[i] {
            before.push(fresh());
            i += 1;
        } else if j < added.len() && added[j] {
            after.push(fresh());
            j += 1;
        } else {
            let shared = fresh();
            before.push(shared);
            after.push(shared);
            i += 1;
            j += 1;
        }
    }
    let mut diff = Diff::default();
    diff.compute_with(imara_diff::Algorithm::Myers, &before, &after, next);
    diff
}

impl TextDiff {
    /// Diffs two texts.
    pub(crate) fn compute(
        algorithm: Algorithm,
        old: Vec<u8>,
        new: Vec<u8>,
        context: u32,
        word_diff: bool,
    ) -> Self {
        let changes = {
            let input = InternedInput::new(old.as_slice(), new.as_slice());
            let mut diff = line_diff(algorithm, &input);
            // Git's default `diff.indentHeuristic`: slide ambiguous hunks to
            // the most readable position.
            diff.postprocess_lines(&input);
            diff.hunks()
                .map(|hunk| Change {
                    old: hunk.before,
                    new: hunk.after,
                })
                .collect::<Vec<_>>()
        };
        let old = SourceText::new(old);
        let new = SourceText::new(new);
        let (hunks, lines) = build_hunks(&changes, old.line_count(), context);
        let added = changes.iter().map(|c| c.new.len() as u32).sum();
        let removed = changes.iter().map(|c| c.old.len() as u32).sum();
        let words = word_diff.then(|| (0..hunks.len()).map(|_| OnceLock::new()).collect());
        Self {
            old,
            new,
            changes,
            hunks,
            lines,
            added,
            removed,
            words,
        }
    }

    /// Text of `side`.
    pub fn source(&self, side: Side) -> &SourceText {
        match side {
            Side::Old => &self.old,
            Side::New => &self.new,
        }
    }

    /// Hunks, in file order.
    pub fn hunks(&self) -> &[Hunk] {
        &self.hunks
    }

    /// Every hunk line, in order; each hunk owns a contiguous range.
    pub fn lines(&self) -> &[DiffLine] {
        &self.lines
    }

    /// Lines added.
    pub fn added(&self) -> u32 {
        self.added
    }

    /// Lines removed.
    pub fn removed(&self) -> u32 {
        self.removed
    }

    /// Text of a diff line, without its terminator.
    pub fn line_text(&self, line: &DiffLine) -> &str {
        let found = match line.kind {
            LineKind::Removed => line.old_line.and_then(|n| self.old.line(n)),
            LineKind::Added | LineKind::Context => line.new_line.and_then(|n| self.new.line(n)),
        };
        found.unwrap_or_default()
    }

    /// Whether git would print `\ No newline at end of file` after `line`.
    pub fn missing_newline_after(&self, line: &DiffLine) -> bool {
        let at_end = |source: &SourceText, number: Option<u32>| {
            source.missing_newline_at_eof && number == Some(source.line_count())
        };
        match line.kind {
            LineKind::Removed => at_end(&self.old, line.old_line),
            LineKind::Added => at_end(&self.new, line.new_line),
            LineKind::Context => {
                at_end(&self.old, line.old_line) || at_end(&self.new, line.new_line)
            }
        }
    }

    /// Maps a 1-based line on `side` to the same line on the other side.
    /// `None` when the line was added/removed (it has no counterpart) or is
    /// out of range.
    pub fn map_line(&self, side: Side, line: u32) -> Option<u32> {
        if line == 0 || line > self.source(side).line_count() {
            return None;
        }
        let index = line - 1;
        let before = self
            .changes
            .partition_point(|change| change.on(side).start <= index);
        let Some(change) = before.checked_sub(1).map(|at| &self.changes[at]) else {
            return Some(line);
        };
        if change.on(side).contains(&index) {
            return None;
        }
        let mapped = index + change.other(side).end - change.on(side).end;
        Some(mapped + 1)
    }

    /// Like [`map_line`](Self::map_line), but a changed line maps to the
    /// first line of the counterpart change (or the line just after it when
    /// that side is empty), clamped to the file. Used to place a comment
    /// whose line has no exact counterpart.
    pub fn map_line_nearest(&self, side: Side, line: u32) -> u32 {
        if let Some(mapped) = self.map_line(side, line) {
            return mapped;
        }
        let other = match side {
            Side::Old => Side::New,
            Side::New => Side::Old,
        };
        let index = line.saturating_sub(1);
        let before = self
            .changes
            .partition_point(|change| change.on(side).start <= index);
        let target = match before.checked_sub(1).map(|at| &self.changes[at]) {
            Some(change) if change.on(side).contains(&index) => change.other(side).start + 1,
            Some(change) => index + change.other(side).end - change.on(side).end + 1,
            None => line,
        };
        target.clamp(1, self.source(other).line_count().max(1))
    }

    /// Old line number of a new line (see [`map_line`](Self::map_line)).
    pub fn new_to_old(&self, line: u32) -> Option<u32> {
        self.map_line(Side::New, line)
    }

    /// New line number of an old line (see [`map_line`](Self::map_line)).
    pub fn old_to_new(&self, line: u32) -> Option<u32> {
        self.map_line(Side::Old, line)
    }

    /// Index of the hunk whose lines on `side` include 1-based `line`.
    pub fn hunk_containing(&self, side: Side, line: u32) -> Option<usize> {
        let after = self
            .hunks
            .partition_point(|hunk| hunk.range_on(side).start <= line);
        let at = after.checked_sub(1)?;
        self.hunks[at].range_on(side).contains(&line).then_some(at)
    }

    /// Whether intra-line diffs were enabled for this diff.
    pub fn has_word_diff(&self) -> bool {
        self.words.is_some()
    }

    /// Intra-line changes of hunk `hunk`, computed on first access. `None`
    /// when word diffs are disabled or `hunk` is out of range.
    ///
    /// Within each run of removed lines followed by added lines, the i-th
    /// removed line is paired with the i-th added line and the two are
    /// word-diffed. Cheap, but call it off the UI thread for huge hunks.
    pub fn intra_line(&self, hunk: usize) -> Option<&HunkWords> {
        let slot = self.words.as_ref()?.get(hunk)?;
        Some(slot.get_or_init(|| self.compute_words(&self.hunks[hunk])))
    }

    fn compute_words(&self, hunk: &Hunk) -> HunkWords {
        let mut out = HunkWords::default();
        let lines = &self.lines[hunk.lines.clone()];
        let mut at = 0;
        while at < lines.len() {
            if lines[at].kind != LineKind::Removed {
                at += 1;
                continue;
            }
            let removed_start = at;
            while at < lines.len() && lines[at].kind == LineKind::Removed {
                at += 1;
            }
            let added_start = at;
            while at < lines.len() && lines[at].kind == LineKind::Added {
                at += 1;
            }
            let pairs = (added_start - removed_start).min(at - added_start);
            for pair in 0..pairs {
                let old_index = hunk.lines.start + removed_start + pair;
                let new_index = hunk.lines.start + added_start + pair;
                let old_text = self.line_text(&self.lines[old_index]);
                let new_text = self.line_text(&self.lines[new_index]);
                let (old_ranges, new_ranges) = word_ranges(old_text, new_text);
                out.lines.push((old_index, old_ranges));
                out.lines.push((new_index, new_ranges));
            }
        }
        out.lines.sort_by_key(|(index, _)| *index);
        out
    }

    /// Approximate heap size, for the cache budget.
    pub(crate) fn memory_size(&self) -> usize {
        size_of::<Self>()
            + self.old.memory_size()
            + self.new.memory_size()
            + self.changes.capacity() * size_of::<Change>()
            + self.hunks.capacity() * size_of::<Hunk>()
            + self.lines.capacity() * size_of::<DiffLine>()
    }
}

/// Groups changes into hunks with `context` lines around them, merging
/// changes whose contexts would touch or overlap (git: `xdl_get_hunk`).
fn build_hunks(changes: &[Change], old_count: u32, context: u32) -> (Vec<Hunk>, Vec<DiffLine>) {
    let mut hunks = Vec::new();
    let mut lines = Vec::new();
    let mut first = 0;
    while first < changes.len() {
        let mut last = first;
        while last + 1 < changes.len()
            && changes[last + 1].old.start - changes[last].old.end <= 2 * context
        {
            last += 1;
        }
        let head = &changes[first];
        let tail = &changes[last];
        let leading = head.old.start.min(context);
        let trailing = (old_count - tail.old.end).min(context);
        let (mut old_at, mut new_at) = (head.old.start - leading, head.new.start - leading);
        let (old_begin, new_begin) = (old_at, new_at);
        let start = lines.len();

        for change in &changes[first..=last] {
            while old_at < change.old.start {
                push_context(&mut lines, &mut old_at, &mut new_at);
            }
            for old in change.old.clone() {
                lines.push(DiffLine {
                    kind: LineKind::Removed,
                    old_line: Some(old + 1),
                    new_line: None,
                });
            }
            for new in change.new.clone() {
                lines.push(DiffLine {
                    kind: LineKind::Added,
                    old_line: None,
                    new_line: Some(new + 1),
                });
            }
            old_at = change.old.end;
            new_at = change.new.end;
        }
        for _ in 0..trailing {
            push_context(&mut lines, &mut old_at, &mut new_at);
        }

        hunks.push(Hunk {
            old_start: old_begin + 1,
            old_len: old_at - old_begin,
            new_start: new_begin + 1,
            new_len: new_at - new_begin,
            lines: start..lines.len(),
        });
        first = last + 1;
    }
    (hunks, lines)
}

fn push_context(lines: &mut Vec<DiffLine>, old_at: &mut u32, new_at: &mut u32) {
    lines.push(DiffLine {
        kind: LineKind::Context,
        old_line: Some(*old_at + 1),
        new_line: Some(*new_at + 1),
    });
    *old_at += 1;
    *new_at += 1;
}

/// Byte ranges that differ between two paired lines, merged when adjacent.
fn word_ranges(old: &str, new: &str) -> (Vec<Range<usize>>, Vec<Range<usize>>) {
    if old.len() > WORD_DIFF_MAX_LINE_BYTES || new.len() > WORD_DIFF_MAX_LINE_BYTES {
        return (whole(old), whole(new));
    }
    let input = InternedInput::new(
        imara_diff::sources::words(old),
        imara_diff::sources::words(new),
    );
    let mut diff = Diff::compute(imara_diff::Algorithm::Myers, &input);
    diff.postprocess_no_heuristic(&input);
    let offsets = |tokens: &[imara_diff::Token]| {
        let mut offsets = Vec::with_capacity(tokens.len() + 1);
        let mut at = 0;
        offsets.push(0);
        for token in tokens {
            at += input.interner[*token].len();
            offsets.push(at);
        }
        offsets
    };
    let old_offsets = offsets(&input.before);
    let new_offsets = offsets(&input.after);
    let mut old_ranges: Vec<Range<usize>> = Vec::new();
    let mut new_ranges: Vec<Range<usize>> = Vec::new();
    for hunk in diff.hunks() {
        push_merged(
            &mut old_ranges,
            old_offsets[hunk.before.start as usize]..old_offsets[hunk.before.end as usize],
        );
        push_merged(
            &mut new_ranges,
            new_offsets[hunk.after.start as usize]..new_offsets[hunk.after.end as usize],
        );
    }
    (old_ranges, new_ranges)
}

fn whole(text: &str) -> Vec<Range<usize>> {
    if text.is_empty() {
        Vec::new()
    } else {
        std::iter::once(0..text.len()).collect()
    }
}

fn push_merged(ranges: &mut Vec<Range<usize>>, range: Range<usize>) {
    if range.is_empty() {
        return;
    }
    match ranges.last_mut() {
        Some(last) if last.end >= range.start => last.end = last.end.max(range.end),
        _ => ranges.push(range),
    }
}
