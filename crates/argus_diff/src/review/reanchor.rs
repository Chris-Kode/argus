//! Re-anchoring (spec §7.3): after every diff refresh (Accept, Fetch, branch
//! change), each comment follows the code it talks about, or is flagged
//! outdated when that code is gone.
//!
//! For each comment, on the side it is anchored to (the head blob for `new`,
//! the merge-base blob for `old`):
//!
//! 1. The file is looked up in the new diff, following a detected rename.
//!    A file that is no longer in the diff (or has no blob on that side, or
//!    is binary) has no match.
//! 2. The comment's window (its snippet: the commented lines plus their
//!    context, exactly what `context_hash` covers) is compared at the stored
//!    position first: equal hash → unchanged.
//! 3. Otherwise the same-sized window slides over the file; the hash
//!    matches closest to the old start line wins (ties go to the earlier
//!    one).
//! 4. No match → `outdated = true`. A match on an outdated comment (e.g.
//!    after a revert) clears the flag.
//!
//! `Resolved` comments are moved too, but never flagged outdated: their code
//! is expected to change. A comment whose file could not be read (I/O
//! error) is left untouched.
//!
//! After a move the context hash and snippet are re-captured at the new
//! position, so the snippet always is the text the hash covers (the context
//! may grow or shrink near the start or end of the file). The commented
//! lines themselves are identical, and `anchor_sha` keeps the creation
//! commit.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use argus_git::CancelToken;
use argus_git::objects::ObjectReader;
use argus_store::{AnchorUpdate, CommentId, CommentStatus};

use crate::error::Result;
use crate::model::{self, FileEntry, Side};
use crate::review::anchor::{AnchorLocation, CommentAnchor, context_hash, normalize_line};
use crate::review::comment::{Comment, store_anchor};
use crate::session::{DiffSession, FileList, run_blocking};
use crate::text::SourceText;

/// What re-anchoring did to one comment.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum AnchorChangeKind {
    /// The code moved (or the file was renamed); the comment follows it.
    Moved,
    /// The code is gone: the comment is now outdated.
    Outdated,
    /// An outdated comment found its code again (e.g. after a revert).
    Restored,
}

/// One comment whose anchor or outdated flag changes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AnchorChange {
    /// The comment.
    pub id: CommentId,
    /// The anchor before re-anchoring.
    pub from: CommentAnchor,
    /// The outdated flag before re-anchoring.
    pub was_outdated: bool,
    /// The anchor after re-anchoring (equal to `from` when it only became
    /// outdated).
    pub to: CommentAnchor,
    /// The outdated flag after re-anchoring.
    pub outdated: bool,
}

impl AnchorChange {
    /// The kind of change.
    pub fn kind(&self) -> AnchorChangeKind {
        match (self.was_outdated, self.outdated) {
            (_, true) => AnchorChangeKind::Outdated,
            (true, false) => AnchorChangeKind::Restored,
            (false, false) => AnchorChangeKind::Moved,
        }
    }

    /// The row update persisting this change, guarded by the old anchor.
    pub(crate) fn to_update(&self) -> AnchorUpdate {
        AnchorUpdate {
            id: self.id,
            expected: store_anchor(&self.from),
            expected_outdated: self.was_outdated,
            anchor: store_anchor(&self.to),
            snippet: self.to.snippet.clone(),
            outdated: self.outdated,
        }
    }

    /// Applies the change to an in-memory comment.
    pub fn apply_to(&self, comment: &mut Comment) {
        comment.anchor = self.to.clone();
        comment.outdated = self.outdated;
    }
}

/// The text of one side of a file, as re-anchoring sees it.
#[derive(Debug)]
pub enum SideText {
    /// The file's text on that side.
    Text(SourceText),
    /// No text there: no blob on that side (added or deleted file),
    /// a binary file or a submodule. Comments there have no match.
    Missing,
    /// The text could not be read; comments there are left untouched.
    Unavailable,
}

/// Re-anchors `comments` over the diff of `session`, on tokio's blocking
/// pool. Returns only the comments whose anchor or outdated flag changes;
/// persist them with [`CommentService::reanchor`] (which calls this), or
/// through [`AnchorChange`]'s fields.
///
/// Cancelled when the session is.
///
/// [`CommentService::reanchor`]: crate::review::CommentService::reanchor
pub async fn reanchor(
    comments: Arc<Vec<Comment>>,
    session: &DiffSession,
) -> Result<Vec<AnchorChange>> {
    if comments.is_empty() {
        return Ok(Vec::new());
    }
    let list = session.file_list().await?;
    let repo = session.repo().clone();
    let guard = session.cancel_token().drop_guard();
    let token = guard.token().clone();
    run_blocking(move || {
        let started = Instant::now();
        let reader = repo.object_reader();
        let changes = reanchor_in(&comments, &list, &token, |entry, side| {
            read_side(&reader, entry, side)
        })?;
        tracing::debug!(
            comments = comments.len(),
            changed = changes.len(),
            elapsed_ms = started.elapsed().as_millis() as u64,
            "re-anchored comments"
        );
        Ok(changes)
    })
    .await
}

/// The re-anchoring algorithm over a file list, with `load` providing the
/// text of a file on one side (called at most once per file and side).
/// Blocking: run on a worker thread. Stops with
/// [`DiffError::Cancelled`](crate::DiffError::Cancelled) when `cancel`
/// fires.
pub fn reanchor_in(
    comments: &[Comment],
    list: &FileList,
    cancel: &CancelToken,
    mut load: impl FnMut(&FileEntry, Side) -> SideText,
) -> Result<Vec<AnchorChange>> {
    let mut texts: HashMap<(usize, Side), FileText> = HashMap::new();
    let mut changes = Vec::new();
    for comment in comments {
        cancel.check()?;
        let anchor = &comment.anchor;
        let located = match find_entry(list, anchor) {
            None => Located::NoMatch,
            Some(entry) => {
                let text = texts
                    .entry((entry.index, anchor.side))
                    .or_insert_with(|| FileText::new(load(entry, anchor.side)));
                match text.locate(anchor) {
                    Located::Found(start) => recapture(text, entry, anchor, start)
                        .map_or(Located::NoMatch, Located::Moved),
                    other => other,
                }
            }
        };
        let change = match located {
            Located::Unreadable => None,
            Located::Moved(to) if to == *anchor && !comment.outdated => None,
            Located::Moved(to) => Some((to, false)),
            Located::Found(_) | Located::NoMatch
                if comment.outdated || comment.status == CommentStatus::Resolved =>
            {
                None
            }
            Located::Found(_) | Located::NoMatch => Some((anchor.clone(), true)),
        };
        if let Some((to, outdated)) = change {
            changes.push(AnchorChange {
                id: comment.id,
                from: anchor.clone(),
                was_outdated: comment.outdated,
                to,
                outdated,
            });
        }
    }
    Ok(changes)
}

/// The entry of the new diff holding the anchor's file, following renames.
fn find_entry<'a>(list: &'a FileList, anchor: &CommentAnchor) -> Option<&'a FileEntry> {
    let entries = list.entries();
    let renamed_from = |path: &str| {
        entries
            .iter()
            .find(|entry| entry.old_path.as_deref() == Some(path))
    };
    match anchor.side {
        Side::New => list
            .index_of_path(&anchor.file_path)
            .map(|index| &entries[index])
            // Renamed in head since the comment was made.
            .or_else(|| renamed_from(&anchor.file_path))
            // Renamed again: the merge-base path is still the same.
            .or_else(|| anchor.old_file_path.as_deref().and_then(renamed_from)),
        Side::Old => {
            let path = anchor.path_on_side();
            entries
                .iter()
                .find(|entry| entry.path_on(Side::Old) == path)
                .or_else(|| list.get(&anchor.file_path))
        }
    }
}

/// Reads one side of `entry` from the object database.
fn read_side(reader: &ObjectReader, entry: &FileEntry, side: Side) -> SideText {
    let tree_entry = match side {
        Side::Old => entry.old,
        Side::New => entry.new,
    };
    let Some(tree_entry) = tree_entry.filter(|tree_entry| tree_entry.kind.is_blob()) else {
        return SideText::Missing;
    };
    match reader.blob(tree_entry.id) {
        Ok(bytes) if model::is_binary(&bytes) => SideText::Missing,
        Ok(bytes) => SideText::Text(SourceText::new(bytes)),
        Err(error) => {
            tracing::warn!(path = %entry.path, %error, "cannot read a file to re-anchor its comments");
            SideText::Unavailable
        }
    }
}

/// The new anchor at `start` (same length as the old one), with its context
/// hash and snippet re-captured there.
fn recapture(
    text: &FileText,
    entry: &FileEntry,
    anchor: &CommentAnchor,
    start: u32,
) -> Option<CommentAnchor> {
    let SideText::Text(source) = &text.text else {
        return None;
    };
    let location = AnchorLocation {
        file_path: entry.path.clone(),
        old_file_path: entry.old_path.clone(),
        side: anchor.side,
        start_line: start,
        end_line: start + (anchor.end_line - anchor.start_line),
    };
    CommentAnchor::capture(source, location, anchor.anchor_sha.clone()).ok()
}

/// Outcome of looking for one comment's code.
enum Located {
    /// The window matches with the commented range starting at this line.
    Found(u32),
    /// The re-captured anchor at the match.
    Moved(CommentAnchor),
    /// The code is gone.
    NoMatch,
    /// The file could not be read.
    Unreadable,
}

/// One side of a file, read once for all its comments.
struct FileText {
    text: SideText,
}

impl FileText {
    fn new(text: SideText) -> Self {
        Self { text }
    }

    /// Where the anchor's window matches in this text, closest to the old
    /// start line.
    fn locate(&self, anchor: &CommentAnchor) -> Located {
        let source = match &self.text {
            SideText::Text(source) => source,
            SideText::Missing => return Located::NoMatch,
            SideText::Unavailable => return Located::Unreadable,
        };
        let line_count = source.line_count();
        let range_len = anchor.end_line.saturating_sub(anchor.start_line) + 1;
        // The window is the snippet: `before` context lines, the range, then
        // the context below.
        let window = anchor.snippet.split('\n').count() as u32;
        let before = anchor.start_line - anchor.snippet_start_line();
        if before + range_len > window || range_len > line_count {
            return match closest_capture_match(source, anchor, range_len) {
                Some(start) => Located::Found(start),
                None => Located::NoMatch,
            };
        }
        let matches_at = |first: u32| {
            first >= 1
                && first + window - 1 <= line_count
                && context_hash((first..first + window).map(|line| source.line(line).unwrap_or("")))
                    == anchor.context_hash
        };
        let stored = anchor.start_line - before;
        if matches_at(stored) {
            return Located::Found(anchor.start_line);
        }
        // Scan outwards from the stored position, so the first match is
        // the closest one (the earlier one on a tie). The first line is
        // compared before hashing the whole window.
        let mut key = String::new();
        normalize_line(anchor.snippet.split('\n').next().unwrap_or(""), &mut key);
        let mut scratch = String::new();
        let mut plausible = |first: u32| {
            let Some(line) = source.line(first) else {
                return false;
            };
            scratch.clear();
            normalize_line(line, &mut scratch);
            scratch == key && matches_at(first)
        };
        let last_first = (line_count + 1).saturating_sub(window);
        let reach = stored.abs_diff(1).max(stored.abs_diff(last_first));
        let best = (1..=reach).find_map(|distance| {
            let above = stored.checked_sub(distance).filter(|&first| first >= 1);
            let below = Some(stored + distance).filter(|&first| first <= last_first);
            above
                .filter(|&first| plausible(first))
                .or_else(|| below.filter(|&first| plausible(first)))
        });
        match best {
            Some(first) => Located::Found(first + before),
            None => Located::NoMatch,
        }
    }
}

/// Fallback for a snippet that does not cover the range (it should always
/// do): compares the capture-time window at every start.
fn closest_capture_match(
    source: &SourceText,
    anchor: &CommentAnchor,
    range_len: u32,
) -> Option<u32> {
    let line_count = source.line_count();
    if range_len > line_count {
        return None;
    }
    (1..=line_count - range_len + 1)
        .filter(|&start| {
            crate::review::anchor::context_hash_at(source, start, start + range_len - 1).as_deref()
                == Some(anchor.context_hash.as_str())
        })
        .min_by_key(|&start| (start.abs_diff(anchor.start_line), start))
}
