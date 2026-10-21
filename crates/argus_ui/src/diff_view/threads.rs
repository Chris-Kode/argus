//! Comment threads and composers inside the diff view (spec §3.1, §7, §9).
//!
//! - **Placement.** Each loaded file lists its blocks ([`FileThreads`]):
//!   one per thread on its path, plus the open composers. A block goes
//!   under the last row of its range's last line ([`layout::anchor_rows`]);
//!   a line that is no longer shown puts it at the top of the file. Blocks
//!   take whole rows of the virtual list; their height in rows is estimated
//!   first and then measured as they paint ([`DiffView::block_measured`]).
//!   Every re-placement keeps the reading position: the line or block row
//!   at the top of the viewport stays there.
//! - **Editing.** Composers (new comments), body edits and replies each own
//!   a [`TextEditor`]. Their text is mirrored into the [`CommentDrafts`]
//!   global as it is typed, so a draft survives closing the box or the diff
//!   view for as long as the app runs. `secondary-enter` saves, `escape`
//!   cancels (asking first when the draft is not empty).
//! - **Status.** Chips read the [`ReviewComments`] snapshot, which follows
//!   every change of the comment service, so a status change shows at once.
//!   Debug builds offer "Advance status" in the thread menu to walk the
//!   state machine by hand.
//! - **Runs.** An open thread carries a "Run · model · effort ▾" split
//!   button (spec §3.1): the main part (or `r` on the focused thread) asks
//!   the workspace to start a run ([`DiffViewEvent::RunRequested`]); the
//!   arrow opens the Run menu with every model and effort on offer.
//!   The run manager makes the choice the new default once the run starts.
//!
//! - **Closed threads** (resolved or dismissed) leave the diff, so it shows
//!   only what still needs the reviewer. "Show closed" in the toolbar puts
//!   them back ([`DiffView::toggle_closed_threads`]); revealing one from the
//!   Comments list does too.
//!
//! - **Outdated comments** (spec §7.3) are not shown inline: their code is
//!   gone. The Comments list shows them with their stored snippet. Its
//!   Re-attach action puts the view in re-attach mode
//!   ([`DiffView::start_reattach`]), scrolled to the old lines and with them
//!   selected: the next line or range picked (gutter "+", `c`, or a
//!   selection and Save) becomes the comment's new anchor.
//!
//! [`ReviewComments`]: crate::comments::ReviewComments

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use argus_diff::Side as DiffSide;
use argus_diff::layout::{self, InlineBlock, Row, RowKey};
use argus_diff::review::{
    AnchorLocation, Comment, CommentAnchor, CommentEvent, CommentId, CommentStatus, ReplyAuthor,
    ReviewKey,
};
use argus_git::{DiffStat, Sha};
use argus_runs::settings::{EFFORTS, MODELS};
use argus_store::{ReplyRun, RunId};
use gpui::{
    AnyElement, App, Context, Entity, FontWeight, Global, Hsla, MouseButton, MouseDownEvent,
    Pixels, SharedString, Subscription, Task, deferred, div, prelude::*, px,
};

use super::{
    Anchor, COMMENT_COLUMN_WIDTH, DiffView, DiffViewEvent, FileState, LineSelection, ListStatus,
    PROPOSAL_COMMENT_HINT, SPLIT_DIVIDER_WIDTH, side_index,
};
use crate::AppContext;
use crate::components::{
    accept_button, button, chip, disabled_button, disabled_button_with_icon, icon_button, kbd,
    primary_button, radius, reject_button, status_chip, text_tooltip,
};
use crate::followup::FollowupKind;
use crate::icons::{IconName, icon};
use crate::keymap::SubmitComment;
use crate::markdown::{self, MarkdownCache};
use crate::runtime::spawn_job;
use crate::session::{now_seconds, relative_time};
use crate::text_editor::{TextEditor, TextEditorEvent};
use crate::theme::ActiveTheme;

/// Rows a composer is assumed to take before it is measured.
const COMPOSER_ESTIMATE_ROWS: u32 = 8;
/// Narrowest a comment card gets before it falls back to the full width.
const MIN_CARD_WIDTH: Pixels = px(260.0);
/// Rows between the top of the viewport and a revealed thread: the sticky
/// file header, then the line the thread hangs under.
const REVEAL_OFFSET: usize = layout::HEADER_ROWS + 1;
/// Space between a card and the edge of its column.
const CARD_MARGIN: Pixels = px(8.0);
/// Widest a thread card gets (Panoptes #56 §5).
const MAX_CARD_WIDTH: Pixels = px(680.0);
/// Height of a thread card's header row.
const THREAD_HEADER_HEIGHT: Pixels = px(34.0);
/// Hint of a thread's reply input.
const REPLY_HINT: &str = "Ask Claude for changes";
/// A thread menu closed this recently by a click outside it is not reopened
/// by the same click landing on its own "⋯" button.
const MENU_REOPEN_GUARD: Duration = Duration::from_millis(250);

/// What a draft belongs to.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum DraftKey {
    /// A new comment on a line range of a review.
    New {
        /// The review (repo, base, head); `None` before it is known.
        review: Option<ReviewKey>,
        /// File path in head.
        file: String,
        /// Side of the range.
        side: DiffSide,
        /// First line.
        start: u32,
        /// Last line.
        end: u32,
    },
    /// The edited body of a comment.
    Edit(CommentId),
    /// A reply being written to a thread.
    Reply(CommentId),
}

/// Unsaved comment texts, kept in memory while the app runs.
#[derive(Default)]
pub struct CommentDrafts {
    drafts: HashMap<DraftKey, String>,
}

impl Global for CommentDrafts {}

impl CommentDrafts {
    /// The draft stored for `key`.
    pub fn get(&self, key: &DraftKey) -> Option<&str> {
        self.drafts.get(key).map(String::as_str)
    }

    fn read(cx: &App, key: &DraftKey) -> Option<String> {
        cx.try_global::<Self>()
            .and_then(|drafts| drafts.get(key))
            .map(str::to_owned)
    }

    fn write(cx: &mut App, key: &DraftKey, text: &str) {
        let drafts = cx.default_global::<Self>();
        if text.is_empty() {
            drafts.drafts.remove(key);
        } else {
            drafts.drafts.insert(key.clone(), text.to_owned());
        }
    }

    fn remove(cx: &mut App, key: &DraftKey) {
        if cx.has_global::<Self>() {
            cx.global_mut::<Self>().drafts.remove(key);
        }
    }
}

/// A block shown in a file's rows.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(super) enum BlockKey {
    /// A comment thread.
    Thread(CommentId),
    /// A new-comment composer.
    Composer(DraftKey),
}

/// An open editor (composer, body edit or reply).
pub(super) struct EditorSlot {
    editor: Entity<TextEditor>,
    draft: DraftKey,
    /// "Discard this draft?" is showing.
    confirm_discard: bool,
    /// The save is in flight.
    saving: bool,
    _subscription: Subscription,
}

/// A new-comment composer under a line range.
pub(super) struct Composer {
    file: usize,
    selection: LineSelection,
    slot: EditorSlot,
}

/// The blocks placed in one loaded file.
#[derive(Default)]
pub(super) struct FileThreads {
    /// Block of each [`Row::Inline::item`].
    items: Vec<BlockKey>,
    /// First row (in the file) of each block.
    first_rows: Vec<usize>,
    /// Rows of each block.
    rows: Vec<u32>,
    /// Commented line ranges per side ([`side_index`]), merged and sorted,
    /// for the gutter highlight.
    ranges: [Vec<(u32, u32)>; 2],
    /// Status marker per side ([`side_index`]) and line: the thread's last
    /// line, where it hangs. Lines ending several threads show the one that
    /// matters most ([`marker_rank`]).
    markers: [HashMap<u32, CommentStatus>; 2],
}

/// Comment state of the diff view.
#[derive(Default)]
pub(super) struct Threads {
    snapshot: Arc<Vec<Comment>>,
    /// Snapshot indices of the inline threads, by file path.
    by_path: HashMap<String, Vec<usize>>,
    /// Resolved and dismissed threads are shown inline too.
    show_closed: bool,
    /// Threads whose fold differs from the default (expanded, or collapsed
    /// for outdated ones).
    toggled: HashSet<CommentId>,
    /// Measured block heights.
    heights: HashMap<BlockKey, Pixels>,
    composers: Vec<Composer>,
    edits: HashMap<CommentId, EditorSlot>,
    replies: HashMap<CommentId, EditorSlot>,
    confirm_delete: Option<CommentId>,
    menu: Option<CommentId>,
    menu_closed: Option<(CommentId, Instant)>,
    /// The thread whose Run menu (model and effort) is open.
    run_menu: Option<CommentId>,
    run_menu_closed: Option<(CommentId, Instant)>,
    /// Model and effort picked in a thread's Run menu, until it runs.
    run_choice: HashMap<CommentId, (String, String)>,
    /// The thread last clicked or jumped to (accent border, debug actions).
    focused: Option<CommentId>,
    markdown: MarkdownCache,
    /// A thread to scroll to once its file is placed.
    pending_reveal: Option<CommentId>,
    /// The outdated comment being re-attached to a new range.
    reattach: Option<Reattach>,
    /// A re-attached comment to scroll to once the snapshot has it inline.
    reveal_reattached: Option<CommentId>,
    /// The proposal of each `Proposed` thread, for its inset.
    proposals: HashMap<CommentId, ProposalInfo>,
    /// Looks up the proposals not known yet; replacing it cancels a lookup
    /// in flight.
    proposals_task: Option<Task<()>>,
    /// The thread whose proposal asks "Reject this proposal?".
    confirm_reject: Option<CommentId>,
    /// Threads whose proposal is being rejected.
    rejecting: HashSet<CommentId>,
}

/// The proposal of a `Proposed` thread, as its inset shows it.
pub(super) struct ProposalInfo {
    /// `updated_at` of the comment it was looked up for; a newer comment
    /// (new reply, new run) looks it up again.
    updated_at: i64,
    /// The run holding the proposal, once looked up.
    run_id: Option<RunId>,
    /// Its size, when the worktree could be read.
    stat: Option<DiffStat>,
}

/// Re-attach mode: the next picked range becomes the comment's anchor.
pub(super) struct Reattach {
    id: CommentId,
    /// The save is in flight.
    saving: bool,
}

/// What [`DiffView::with_scroll_anchor`] keeps in place.
enum SavedTop {
    Row(RowKey),
    Block(BlockKey, u32),
}

fn markdown_cache(view: &mut DiffView) -> &mut MarkdownCache {
    &mut view.threads.markdown
}

impl DiffView {
    // ---- state ----------------------------------------------------------------

    /// Number of comments loaded for this review.
    pub fn comment_count(&self) -> usize {
        self.threads.snapshot.len()
    }

    fn comment(&self, id: CommentId) -> Option<&Comment> {
        self.threads
            .snapshot
            .iter()
            .find(|comment| comment.id == id)
    }

    /// The file a thread is placed in; `None` for outdated threads, which
    /// are not inline.
    fn comment_file(&self, id: CommentId) -> Option<usize> {
        let comment = self.comment(id).filter(|comment| !comment.outdated)?;
        self.path_index.get(&comment.anchor.file_path).copied()
    }

    /// Whether `comment` is inline: not outdated, and not closed unless
    /// closed threads are shown.
    fn is_inline(&self, comment: &Comment) -> bool {
        !comment.outdated && (self.threads.show_closed || !comment.is_closed())
    }

    /// Indexes the inline threads of the snapshot by file path, and drops
    /// the focus of a thread no longer inline.
    fn index_threads(&mut self) {
        let mut by_path: HashMap<String, Vec<usize>> = HashMap::new();
        for (index, comment) in self.threads.snapshot.iter().enumerate() {
            if self.is_inline(comment) {
                by_path
                    .entry(comment.anchor.file_path.clone())
                    .or_default()
                    .push(index);
            }
        }
        self.threads.by_path = by_path;
        // Keys act on the focused thread; a hidden one is out of reach.
        if self
            .threads
            .focused
            .is_some_and(|id| self.comment(id).is_none_or(|c| !self.is_inline(c)))
        {
            self.threads.focused = None;
        }
    }

    /// Re-places the threads of every loaded file, keeping the reading
    /// position.
    fn place_all_threads(&mut self) {
        self.with_scroll_anchor(|this| {
            for file in 0..this.files.len() {
                if this.files[file].loaded().is_some() {
                    this.reveal_commented(file);
                    this.place_threads(file);
                }
            }
        });
    }

    /// Number of resolved and dismissed threads that have a place in the
    /// diff.
    fn closed_count(&self) -> usize {
        self.threads
            .snapshot
            .iter()
            .filter(|comment| !comment.outdated && comment.is_closed())
            .count()
    }

    /// Shows or hides the resolved and dismissed threads.
    pub fn toggle_closed_threads(&mut self, cx: &mut Context<Self>) {
        self.threads.show_closed = !self.threads.show_closed;
        self.index_threads();
        self.place_all_threads();
        cx.notify();
    }

    /// Inline comments anchored in `file`, by line and creation time.
    fn file_comments(&self, file: usize) -> Vec<&Comment> {
        let ListStatus::Ready(list) = &self.status else {
            return Vec::new();
        };
        let Some(entry) = list.entries().get(file) else {
            return Vec::new();
        };
        self.threads
            .by_path
            .get(&entry.path)
            .map(|indices| {
                indices
                    .iter()
                    .filter_map(|&index| self.threads.snapshot.get(index))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Line ranges that must be visible in `file`: the current comments
    /// and open composers.
    pub(super) fn commented_ranges(&self, file: usize) -> Vec<(DiffSide, u32, u32)> {
        let comments = self.file_comments(file).into_iter().map(|comment| {
            let anchor = &comment.anchor;
            (anchor.side, anchor.start_line, anchor.end_line)
        });
        let composers = self
            .threads
            .composers
            .iter()
            .filter(|composer| composer.file == file)
            .map(|composer| {
                let selection = &composer.selection;
                (selection.side, selection.start, selection.end)
            });
        comments.chain(composers).collect()
    }

    fn is_collapsed(&self, comment: &Comment) -> bool {
        self.threads.toggled.contains(&comment.id)
    }

    /// Whether `line` of `side` is under a comment in `file`.
    pub(super) fn in_comment_range(&self, file: usize, side: DiffSide, line: Option<u32>) -> bool {
        let Some(line) = line else {
            return false;
        };
        let ranges = &self.files[file].threads.ranges[side_index(side)];
        let at = ranges.partition_point(|&(start, _)| start <= line);
        at > 0 && ranges[at - 1].1 >= line
    }

    /// The status marker of `line` of `side` in `file`'s comment column.
    pub(super) fn comment_marker(
        &self,
        file: usize,
        side: DiffSide,
        line: Option<u32>,
    ) -> Option<CommentStatus> {
        let markers = &self.files[file].threads.markers[side_index(side)];
        markers.get(&line?).copied()
    }

    /// Number of current threads in `file`, loaded or not.
    pub(super) fn thread_count(&self, file: usize) -> usize {
        self.file_comments(file).len()
    }

    /// The first line of a comment of `file` that Claude is running.
    pub(super) fn running_line(&self, file: usize) -> Option<u32> {
        self.file_comments(file)
            .into_iter()
            .filter(|comment| comment.status == CommentStatus::Running)
            .map(|comment| comment.anchor.start_line)
            .min()
    }

    /// Follows the comment snapshot: re-parses changed bodies, forgets the
    /// state of deleted threads and re-places every loaded file.
    pub(super) fn comments_changed(&mut self, cx: &mut Context<Self>) {
        let snapshot = self.comments.read(cx).comments().clone();
        for comment in snapshot.iter() {
            self.threads
                .markdown
                .prepare(&comment.body_md, markdown_cache, cx);
            for reply in &comment.replies {
                self.threads
                    .markdown
                    .prepare(&reply.body_md, markdown_cache, cx);
            }
        }
        let ids: HashSet<CommentId> = snapshot.iter().map(|comment| comment.id).collect();
        if !self.threads.show_closed {
            // A thread closing here (Accept, Dismiss) leaves the diff; say
            // where it went.
            let was_open: HashSet<CommentId> = self
                .threads
                .snapshot
                .iter()
                .filter(|comment| !comment.is_closed())
                .map(|comment| comment.id)
                .collect();
            let closed = snapshot
                .iter()
                .find(|comment| comment.is_closed() && was_open.contains(&comment.id));
            if let Some(comment) = closed {
                self.hint = Some(
                    format!(
                        "Comment {} and hidden. “Show closed” in the toolbar brings it back.",
                        comment.status
                    )
                    .into(),
                );
            }
        }
        let threads = &mut self.threads;
        threads.toggled.retain(|id| ids.contains(id));
        threads.edits.retain(|id, _| ids.contains(id));
        threads.replies.retain(|id, _| ids.contains(id));
        threads.heights.retain(|key, _| match key {
            BlockKey::Thread(id) => ids.contains(id),
            BlockKey::Composer(_) => true,
        });
        for slot in [
            &mut threads.confirm_delete,
            &mut threads.menu,
            &mut threads.focused,
        ] {
            if slot.is_some_and(|id| !ids.contains(&id)) {
                *slot = None;
            }
        }
        if threads
            .reattach
            .as_ref()
            .is_some_and(|reattach| !ids.contains(&reattach.id))
        {
            threads.reattach = None;
        }
        threads.snapshot = snapshot;
        self.index_threads();
        self.load_proposals(cx);
        self.place_all_threads();
        self.reveal_pending(cx);
        if let Some(id) = self.threads.reveal_reattached
            && self.comment(id).is_none_or(|comment| !comment.outdated)
        {
            self.threads.reveal_reattached = None;
            if self.comment(id).is_some() {
                self.reveal_comment(id, cx);
            }
        }
        cx.notify();
    }

    /// Looks up, on the tokio runtime, the proposal run and size of every
    /// `Proposed` thread whose proposal is not known for its current state.
    fn load_proposals(&mut self, cx: &mut Context<Self>) {
        let proposed: HashMap<CommentId, i64> = self
            .threads
            .snapshot
            .iter()
            .filter(|comment| comment.status == CommentStatus::Proposed && !comment.outdated)
            .map(|comment| (comment.id, comment.updated_at))
            .collect();
        let threads = &mut self.threads;
        threads
            .proposals
            .retain(|id, info| proposed.get(id) == Some(&info.updated_at));
        threads.rejecting.retain(|id| proposed.contains_key(id));
        if threads
            .confirm_reject
            .is_some_and(|id| !proposed.contains_key(&id))
        {
            threads.confirm_reject = None;
        }
        let wanted: Vec<(CommentId, i64)> = proposed
            .into_iter()
            .filter(|(id, _)| {
                threads
                    .proposals
                    .get(id)
                    .is_none_or(|info| info.run_id.is_none())
            })
            .collect();
        if wanted.is_empty() {
            return;
        }
        let app = cx.global::<AppContext>();
        let (runs, store, worktrees) = (app.runs.clone(), app.store.runs(), app.worktrees.clone());
        let job = spawn_job(cx, async move {
            let mut found = Vec::with_capacity(wanted.len());
            for (id, updated_at) in wanted {
                let run_id = match runs.proposal_run(id).await {
                    Ok(run_id) => run_id,
                    Err(error) => {
                        tracing::debug!(comment = %id, %error, "no proposal run for the thread");
                        continue;
                    }
                };
                let stat = match store.get(run_id).await {
                    Ok(Some(run)) => match Sha::parse(&run.start_sha) {
                        Some(start) => worktrees
                            .staged_diff_stat(&run.worktree_path, &start)
                            .await
                            .inspect_err(|error| {
                                tracing::debug!(run = %run_id, %error, "cannot size the proposal");
                            })
                            .ok(),
                        None => None,
                    },
                    _ => None,
                };
                found.push((id, updated_at, run_id, stat));
            }
            found
        });
        self.threads.proposals_task = Some(cx.spawn(async move |this, cx| {
            let Ok(found) = job.await else {
                return;
            };
            this.update(cx, |this, cx| {
                this.threads.proposals_task = None;
                for (id, updated_at, run_id, stat) in found {
                    let current = this
                        .comment(id)
                        .is_some_and(|comment| comment.updated_at == updated_at);
                    if current {
                        this.threads.proposals.insert(
                            id,
                            ProposalInfo {
                                updated_at,
                                run_id: Some(run_id),
                                stat,
                            },
                        );
                        this.threads.heights.remove(&BlockKey::Thread(id));
                    }
                }
                cx.notify();
            })
            .ok();
        }));
    }

    /// Rejects a thread's proposal, as the proposal tab's Reject does: the
    /// run manager deletes the worktree and the comment goes back to Open.
    /// The rejection is not cut short if the view goes away.
    fn reject_proposal(&mut self, id: CommentId, cx: &mut Context<Self>) {
        self.threads.confirm_reject = None;
        let Some(run_id) = self.threads.proposals.get(&id).and_then(|info| info.run_id) else {
            return;
        };
        if !self.threads.rejecting.insert(id) {
            return;
        }
        let app = cx.global::<AppContext>();
        let runs = app.runs.clone();
        let handle = app.runtime.spawn(async move { runs.reject(run_id).await });
        cx.spawn(async move |this, cx| {
            let error = match handle.await {
                Ok(Ok(())) => None,
                Ok(Err(error)) => Some(error.to_string()),
                Err(error) => Some(error.to_string()),
            };
            this.update(cx, |this, cx| {
                this.threads.rejecting.remove(&id);
                if let Some(message) = error {
                    tracing::warn!(%message, "cannot reject the proposal");
                    this.hint = Some(format!("Could not reject the proposal: {message}").into());
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    /// Expands the context of a loaded file so its commented lines show.
    fn reveal_commented(&mut self, file: usize) {
        let ranges = self.commented_ranges(file);
        let slot = &mut self.files[file];
        let FileState::Loaded(loaded) = &mut slot.state else {
            return;
        };
        let Some(text) = loaded.diff.text() else {
            return;
        };
        let gaps = layout::gaps(text);
        let mut changed = false;
        for (side, start, end) in ranges {
            changed |= layout::reveal_lines(&gaps, &mut slot.expansions, side, start, end);
        }
        if changed {
            let options = loaded.layout;
            let rows = layout::build_rows(&loaded.diff, &slot.expansions, options);
            self.held_bytes = self.held_bytes.saturating_sub(loaded.bytes);
            **loaded = super::LoadedFile::new(loaded.diff.clone(), rows, options);
            self.held_bytes += loaded.bytes;
            slot.full_len = loaded.rows.rows.len();
        }
    }

    /// Puts the threads and composers of a loaded `file` into its rows.
    /// Callers rebuild the row index (see [`Self::with_scroll_anchor`]).
    pub(super) fn place_threads(&mut self, file: usize) {
        let mut blocks: Vec<(BlockKey, DiffSide, u32)> = Vec::new();
        let mut ranges: [Vec<(u32, u32)>; 2] = [Vec::new(), Vec::new()];
        let mut markers: [HashMap<u32, CommentStatus>; 2] = Default::default();
        for comment in self.file_comments(file) {
            let anchor = &comment.anchor;
            blocks.push((BlockKey::Thread(comment.id), anchor.side, anchor.end_line));
            ranges[side_index(anchor.side)].push((anchor.start_line, anchor.end_line));
            markers[side_index(anchor.side)]
                .entry(anchor.end_line)
                .and_modify(|status| {
                    if marker_rank(comment.status) > marker_rank(*status) {
                        *status = comment.status;
                    }
                })
                .or_insert(comment.status);
        }
        for composer in &self.threads.composers {
            if composer.file == file {
                let selection = &composer.selection;
                blocks.push((
                    BlockKey::Composer(composer.slot.draft.clone()),
                    selection.side,
                    selection.end,
                ));
            }
        }
        for side in &mut ranges {
            side.sort_unstable();
            let mut merged: Vec<(u32, u32)> = Vec::with_capacity(side.len());
            for &(start, end) in side.iter() {
                match merged.last_mut() {
                    Some(last) if start <= last.1.saturating_add(1) => last.1 = last.1.max(end),
                    _ => merged.push((start, end)),
                }
            }
            *side = merged;
        }
        let rows: Vec<u32> = blocks
            .iter()
            .map(|(key, ..)| self.block_rows(key))
            .collect();
        let slot = &mut self.files[file];
        let FileState::Loaded(loaded) = &mut slot.state else {
            slot.threads = FileThreads::default();
            return;
        };
        layout::strip_inline(&mut loaded.rows);
        let wanted: Vec<(DiffSide, u32)> =
            blocks.iter().map(|(_, side, end)| (*side, *end)).collect();
        let anchors = match loaded.diff.text() {
            Some(text) => layout::anchor_rows(text, &loaded.rows.rows, &wanted),
            None => vec![None; wanted.len()],
        };
        let inline: Vec<InlineBlock> = anchors
            .iter()
            .zip(&rows)
            .enumerate()
            .map(|(item, (anchor, &rows))| InlineBlock {
                after: anchor.unwrap_or(layout::HEADER_ROWS - 1),
                item: item as u32,
                rows,
            })
            .collect();
        let first_rows = layout::place_inline(&mut loaded.rows, &inline);
        slot.full_len = loaded.rows.rows.len();
        slot.threads = FileThreads {
            items: blocks.into_iter().map(|(key, ..)| key).collect(),
            first_rows,
            rows,
            ranges,
            markers,
        };
    }

    /// Rows a block takes: measured, or estimated until it paints.
    fn block_rows(&self, key: &BlockKey) -> u32 {
        let row_height = self.metrics.row_height;
        if let Some(height) = self.threads.heights.get(key) {
            return ((*height / row_height).ceil() as u32).max(1);
        }
        match key {
            BlockKey::Composer(_) => COMPOSER_ESTIMATE_ROWS,
            BlockKey::Thread(id) => {
                let Some(comment) = self.comment(*id) else {
                    return 2;
                };
                if self.is_collapsed(comment) {
                    return 2;
                }
                let lines = |text: &str| text.lines().count().max(1) as u32;
                let replies: u32 = comment
                    .replies
                    .iter()
                    .map(|reply| 2 + lines(&reply.body_md))
                    .sum();
                let reply_box = if self.threads.replies.contains_key(id) {
                    6
                } else {
                    2
                };
                4 + lines(&comment.body_md) + replies + reply_box
            }
        }
    }

    /// A block finished painting at `height`; re-places its file when that
    /// changes its row count.
    fn block_measured(
        &mut self,
        file: usize,
        key: BlockKey,
        height: Pixels,
        cx: &mut Context<Self>,
    ) {
        if self.threads.heights.get(&key) == Some(&height) {
            return;
        }
        self.threads.heights.insert(key.clone(), height);
        let Some(slot) = self.files.get(file) else {
            return;
        };
        let current = slot
            .threads
            .items
            .iter()
            .position(|item| *item == key)
            .map(|index| slot.threads.rows[index]);
        if current.is_some_and(|rows| rows != self.block_rows(&key)) {
            self.with_scroll_anchor(|this| this.place_threads(file));
            cx.notify();
        }
    }

    /// Runs `change` (which re-places blocks) keeping the line or block row
    /// at the top of the viewport where it is.
    fn with_scroll_anchor(&mut self, change: impl FnOnce(&mut Self)) {
        let anchor = self.anchor();
        let saved = anchor.and_then(|anchor| {
            let slot = self.files.get(anchor.file)?;
            let loaded = slot.loaded().filter(|_| !slot.collapsed)?;
            match loaded.rows.rows.get(anchor.row)? {
                Row::Inline { item, part } => Some(SavedTop::Block(
                    slot.threads.items.get(*item as usize)?.clone(),
                    *part,
                )),
                _ => Some(SavedTop::Row(layout::row_key(
                    &loaded.rows.rows,
                    anchor.row,
                ))),
            }
        });
        change(self);
        self.rebuild_starts();
        let Some(anchor) = anchor else {
            return;
        };
        let row = saved.and_then(|saved| {
            let slot = self.files.get(anchor.file)?;
            let loaded = slot.loaded().filter(|_| !slot.collapsed)?;
            match saved {
                SavedTop::Block(key, part) => {
                    let index = slot.threads.items.iter().position(|item| *item == key)?;
                    let rows = slot.threads.rows[index].max(1);
                    Some(slot.threads.first_rows[index] + part.min(rows - 1) as usize)
                }
                SavedTop::Row(key) => layout::find_row(&loaded.rows.rows, key),
            }
        });
        self.restore(Anchor {
            row: row.unwrap_or(anchor.row),
            ..anchor
        });
    }

    /// Re-places one file's blocks, keeping the reading position.
    fn replace_threads(&mut self, file: usize, cx: &mut Context<Self>) {
        self.with_scroll_anchor(|this| this.place_threads(file));
        cx.notify();
    }

    /// `local`, or the line row a comment block at `local` hangs under.
    pub(super) fn line_row_at_or_above(&self, file: usize, local: usize) -> usize {
        let Some(loaded) = self.files.get(file).and_then(|slot| slot.loaded()) else {
            return local;
        };
        let rows = &loaded.rows.rows;
        let mut at = local.min(rows.len().saturating_sub(1));
        while at > layout::HEADER_ROWS && matches!(rows.get(at), Some(Row::Inline { .. })) {
            at -= 1;
        }
        at
    }

    // ---- editors ----------------------------------------------------------

    fn new_editor_slot(
        &mut self,
        draft: DraftKey,
        text: String,
        placeholder: &'static str,
        cx: &mut Context<Self>,
    ) -> EditorSlot {
        let editor = cx.new(|cx| TextEditor::new(text, placeholder, cx));
        editor.update(cx, |editor, cx| editor.request_focus(cx));
        let target = draft.clone();
        let subscription = cx.subscribe(&editor, move |this, editor, event, cx| {
            this.editor_event(&target, &editor, *event, cx);
        });
        EditorSlot {
            editor,
            draft,
            confirm_discard: false,
            saving: false,
            _subscription: subscription,
        }
    }

    fn slot(&self, draft: &DraftKey) -> Option<&EditorSlot> {
        match draft {
            DraftKey::New { .. } => self
                .threads
                .composers
                .iter()
                .find(|composer| composer.slot.draft == *draft)
                .map(|composer| &composer.slot),
            DraftKey::Edit(id) => self.threads.edits.get(id),
            DraftKey::Reply(id) => self.threads.replies.get(id),
        }
    }

    fn slot_mut(&mut self, draft: &DraftKey) -> Option<&mut EditorSlot> {
        match draft {
            DraftKey::New { .. } => self
                .threads
                .composers
                .iter_mut()
                .find(|composer| composer.slot.draft == *draft)
                .map(|composer| &mut composer.slot),
            DraftKey::Edit(id) => self.threads.edits.get_mut(id),
            DraftKey::Reply(id) => self.threads.replies.get_mut(id),
        }
    }

    /// The file an editor's block is in.
    fn draft_file(&self, draft: &DraftKey) -> Option<usize> {
        match draft {
            DraftKey::New { .. } => self
                .threads
                .composers
                .iter()
                .find(|composer| composer.slot.draft == *draft)
                .map(|composer| composer.file),
            DraftKey::Edit(id) | DraftKey::Reply(id) => self.comment_file(*id),
        }
    }

    fn editor_event(
        &mut self,
        draft: &DraftKey,
        editor: &Entity<TextEditor>,
        event: TextEditorEvent,
        cx: &mut Context<Self>,
    ) {
        match event {
            TextEditorEvent::Edited => {
                let text = editor.read(cx).text().to_owned();
                CommentDrafts::write(cx, draft, &text);
                if let Some(slot) = self.slot_mut(draft) {
                    slot.confirm_discard = false;
                }
                cx.notify();
            }
            TextEditorEvent::Submit => self.submit_editor(draft, cx),
            TextEditorEvent::Cancel => self.cancel_editor(draft, cx),
        }
    }

    /// Opens (or focuses) a composer under `selection` of `file`.
    pub(super) fn open_composer(
        &mut self,
        file: usize,
        selection: LineSelection,
        cx: &mut Context<Self>,
    ) {
        if self.threads.reattach.is_some() {
            self.save_reattach(file, selection, cx);
            return;
        }
        let draft = DraftKey::New {
            review: self.comments.read(cx).key().cloned(),
            file: selection.file.clone(),
            side: selection.side,
            start: selection.start,
            end: selection.end,
        };
        if let Some(slot) = self.slot(&draft) {
            slot.editor
                .update(cx, |editor, cx| editor.request_focus(cx));
            return;
        }
        let text = CommentDrafts::read(cx, &draft).unwrap_or_default();
        let slot = self.new_editor_slot(draft, text, "Leave a comment… (Markdown)", cx);
        self.threads.composers.push(Composer {
            file,
            selection,
            slot,
        });
        self.replace_threads(file, cx);
    }

    /// `c`: comments on the gutter selection, or on the first line in view.
    pub fn comment_on_current_line(&mut self, cx: &mut Context<Self>) {
        if self.proposal {
            self.hint = Some(PROPOSAL_COMMENT_HINT.into());
            cx.notify();
            return;
        }
        if let Some(selection) = self.selection {
            match self.selection() {
                Some(Ok(lines)) => {
                    self.open_composer(selection.file, lines, cx);
                    return;
                }
                Some(Err(_)) => {
                    self.resolve_selection(cx);
                    cx.notify();
                    return;
                }
                None => {}
            }
        }
        if self.threads.reattach.is_some() {
            // Re-attaching to whatever line is on screen would be a guess.
            self.hint = Some("Select the lines to re-attach the comment to.".into());
            cx.notify();
            return;
        }
        let total = self.total_rows();
        // Below the sticky file header.
        let first = self.top_row() + layout::HEADER_ROWS;
        let screen = (self.list_bounds().size.height / self.metrics.row_height).ceil() as usize;
        for row in first..(first + screen.max(1)).min(total) {
            let file = self.file_at(row);
            let local = row - self.starts[file];
            let Some(loaded) = self.files[file].loaded() else {
                continue;
            };
            let Some(text) = loaded.diff.text() else {
                continue;
            };
            let side = match loaded.rows.rows.get(local) {
                Some(Row::Line { .. }) | Some(Row::Context { .. }) if !loaded.rows.split => None,
                Some(&split @ (Row::Split { .. } | Row::Context { .. })) => {
                    if layout::split_cell(text, split, DiffSide::New).is_some() {
                        Some(DiffSide::New)
                    } else {
                        Some(DiffSide::Old)
                    }
                }
                _ => continue,
            };
            self.selection = Some(super::Selection {
                file,
                side,
                anchor: local,
                head: local,
            });
            if let Some(lines) = self.resolve_selection(cx) {
                self.open_composer(file, lines, cx);
            }
            cx.notify();
            return;
        }
        self.hint = Some("Scroll to a line to comment on it.".into());
        cx.notify();
    }

    /// Saves an editor. A reply on a `Proposed` comment is sent as a
    /// follow-up run, unless `note_only`.
    fn submit_editor(&mut self, draft: &DraftKey, cx: &mut Context<Self>) {
        self.submit_editor_as(draft, false, cx);
    }

    fn submit_editor_as(&mut self, draft: &DraftKey, note_only: bool, cx: &mut Context<Self>) {
        let Some(slot) = self.slot(draft) else {
            return;
        };
        if slot.saving {
            return;
        }
        let body = slot.editor.read(cx).text().trim_end().to_owned();
        if body.trim().is_empty() {
            self.hint = Some("Write something before saving.".into());
            cx.notify();
            return;
        }
        let task = match draft {
            DraftKey::New { .. } => {
                let Some(anchor) = self.composer_anchor(draft) else {
                    self.hint = Some("This line cannot be commented on any more.".into());
                    cx.notify();
                    return;
                };
                self.comments
                    .update(cx, |comments, cx| comments.create(anchor, body, cx))
            }
            DraftKey::Edit(id) => {
                let id = *id;
                self.comments
                    .update(cx, |comments, cx| comments.edit(id, body, cx))
            }
            DraftKey::Reply(id) => {
                let id = *id;
                if !note_only && self.offers_followup(id) {
                    // The Run menu's choice; like a run, it is used up.
                    let choice = self.run_choice(id, cx);
                    self.threads.run_choice.remove(&id);
                    self.comments.update(cx, |comments, cx| {
                        comments.follow_up(id, FollowupKind::Resume(body), choice, cx)
                    })
                } else {
                    self.comments
                        .update(cx, |comments, cx| comments.reply(id, body, cx))
                }
            }
        };
        if let Some(slot) = self.slot_mut(draft) {
            slot.saving = true;
        }
        let draft = draft.clone();
        cx.spawn(async move |this, cx| {
            let saved = task.await;
            this.update(cx, |this, cx| {
                if saved {
                    CommentDrafts::remove(cx, &draft);
                    this.close_editor(&draft, cx);
                } else if let Some(slot) = this.slot_mut(&draft) {
                    slot.saving = false;
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    /// The anchor of a composer's range, captured from the loaded text.
    fn composer_anchor(&self, draft: &DraftKey) -> Option<CommentAnchor> {
        let composer = self
            .threads
            .composers
            .iter()
            .find(|composer| composer.slot.draft == *draft)?;
        self.capture_anchor(composer.file, &composer.selection)
    }

    /// The anchor of `selection` in `file`, captured from the loaded text at
    /// the head this view diffs.
    fn capture_anchor(&self, file: usize, selection: &LineSelection) -> Option<CommentAnchor> {
        let text = self.files.get(file)?.loaded()?.diff.text()?;
        let location = AnchorLocation {
            file_path: selection.file.clone(),
            old_file_path: selection.old_file.clone(),
            side: selection.side,
            start_line: selection.start,
            end_line: selection.end,
        };
        CommentAnchor::capture(
            text.source(selection.side),
            location,
            self.target.head_tip.to_hex(),
        )
        .map_err(|error| tracing::warn!(%error, "cannot anchor the comment"))
        .ok()
    }

    /// Escape: closes an empty editor, asks before dropping a draft, and a
    /// second escape goes back to editing.
    fn cancel_editor(&mut self, draft: &DraftKey, cx: &mut Context<Self>) {
        let unchanged = match draft {
            DraftKey::Edit(id) => self
                .comment(*id)
                .map(|comment| comment.body_md.trim_end().to_owned()),
            _ => None,
        };
        let Some(slot) = self.slot_mut(draft) else {
            return;
        };
        let text = slot.editor.read(cx).text();
        let untouched =
            text.trim().is_empty() || unchanged.is_some_and(|body| body == text.trim_end());
        if untouched {
            self.discard_editor(draft, cx);
        } else {
            slot.confirm_discard = !slot.confirm_discard;
            if !slot.confirm_discard {
                slot.editor
                    .update(cx, |editor, cx| editor.request_focus(cx));
            }
            cx.notify();
        }
    }

    fn keep_editing(&mut self, draft: &DraftKey, cx: &mut Context<Self>) {
        if let Some(slot) = self.slot_mut(draft) {
            slot.confirm_discard = false;
            slot.editor
                .update(cx, |editor, cx| editor.request_focus(cx));
        }
        cx.notify();
    }

    /// Drops an editor and its draft.
    fn discard_editor(&mut self, draft: &DraftKey, cx: &mut Context<Self>) {
        CommentDrafts::remove(cx, draft);
        self.close_editor(draft, cx);
    }

    /// Closes an editor, keeping its draft.
    fn close_editor(&mut self, draft: &DraftKey, cx: &mut Context<Self>) {
        let file = self.draft_file(draft);
        match draft {
            DraftKey::New { .. } => {
                self.threads
                    .composers
                    .retain(|composer| composer.slot.draft != *draft);
                self.threads
                    .heights
                    .remove(&BlockKey::Composer(draft.clone()));
            }
            DraftKey::Edit(id) => {
                self.threads.edits.remove(id);
                self.threads.heights.remove(&BlockKey::Thread(*id));
            }
            DraftKey::Reply(id) => {
                self.threads.replies.remove(id);
                self.threads.heights.remove(&BlockKey::Thread(*id));
            }
        }
        self.focus_requested = true;
        match file {
            Some(file) => self.replace_threads(file, cx),
            None => cx.notify(),
        }
    }

    // ---- thread actions ---------------------------------------------------

    fn focus_thread(&mut self, id: CommentId, cx: &mut Context<Self>) {
        if self.threads.focused != Some(id) {
            self.threads.focused = Some(id);
            cx.notify();
        }
    }

    fn toggle_thread(&mut self, id: CommentId, cx: &mut Context<Self>) {
        if !self.threads.toggled.remove(&id) {
            self.threads.toggled.insert(id);
        }
        self.threads.heights.remove(&BlockKey::Thread(id));
        if let Some(file) = self.comment_file(id) {
            self.replace_threads(file, cx);
        }
    }

    /// Whether every thread is folded.
    pub fn all_threads_collapsed(&self) -> bool {
        self.threads
            .snapshot
            .iter()
            .filter(|comment| self.is_inline(comment))
            .all(|comment| self.is_collapsed(comment))
    }

    /// Folds every thread, or unfolds them all when they already are.
    pub fn toggle_all_threads(&mut self, cx: &mut Context<Self>) {
        let collapse = !self.all_threads_collapsed();
        self.threads.toggled = self
            .threads
            .snapshot
            .iter()
            .filter(|comment| collapse && !comment.outdated)
            .map(|comment| comment.id)
            .collect();
        self.threads
            .heights
            .retain(|key, _| matches!(key, BlockKey::Composer(_)));
        self.with_scroll_anchor(|this| {
            for file in 0..this.files.len() {
                if this.files[file].loaded().is_some() {
                    this.place_threads(file);
                }
            }
        });
        cx.notify();
    }

    fn start_edit(&mut self, id: CommentId, cx: &mut Context<Self>) {
        self.threads.menu = None;
        if let Some(slot) = self.threads.edits.get(&id) {
            slot.editor
                .update(cx, |editor, cx| editor.request_focus(cx));
            return;
        }
        let Some(body) = self.comment(id).map(|comment| comment.body_md.clone()) else {
            return;
        };
        let draft = DraftKey::Edit(id);
        let text = CommentDrafts::read(cx, &draft).unwrap_or(body);
        let slot = self.new_editor_slot(draft, text, "Comment text (Markdown)", cx);
        self.threads.edits.insert(id, slot);
        if self
            .comment(id)
            .is_some_and(|comment| self.is_collapsed(comment))
        {
            self.threads.toggled.insert(id);
        }
        self.threads.heights.remove(&BlockKey::Thread(id));
        if let Some(file) = self.comment_file(id) {
            self.replace_threads(file, cx);
        }
    }

    fn start_reply(&mut self, id: CommentId, cx: &mut Context<Self>) {
        if let Some(slot) = self.threads.replies.get(&id) {
            slot.editor
                .update(cx, |editor, cx| editor.request_focus(cx));
            return;
        }
        let draft = DraftKey::Reply(id);
        let text = CommentDrafts::read(cx, &draft).unwrap_or_default();
        let slot = self.new_editor_slot(draft, text, REPLY_HINT, cx);
        self.threads.replies.insert(id, slot);
        self.threads.heights.remove(&BlockKey::Thread(id));
        if let Some(file) = self.comment_file(id) {
            self.replace_threads(file, cx);
        }
    }

    /// Whether a reply on `id` can be sent as a follow-up run: the comment
    /// has a proposal (its worktree is checked when the run starts).
    fn offers_followup(&self, id: CommentId) -> bool {
        self.comment(id)
            .is_some_and(|comment| comment.status == CommentStatus::Proposed && !comment.outdated)
    }

    /// Starts a fresh follow-up with the thread as context, after a resume
    /// found no session.
    fn run_fresh(&mut self, id: CommentId, cx: &mut Context<Self>) {
        let choice = self.run_choice(id, cx);
        self.comments
            .update(cx, |comments, cx| {
                comments.follow_up(id, FollowupKind::Fresh, choice, cx)
            })
            .detach();
        cx.notify();
    }

    /// Asks the workspace to open the comment's proposal in its own tab.
    fn view_proposal(&mut self, id: CommentId, cx: &mut Context<Self>) {
        self.threads.menu = None;
        cx.emit(super::DiffViewEvent::ViewProposal(id));
        cx.notify();
    }

    fn request_delete(&mut self, id: CommentId, cx: &mut Context<Self>) {
        self.threads.menu = None;
        self.threads.confirm_delete = Some(id);
        cx.notify();
    }

    fn delete_thread(&mut self, id: CommentId, cx: &mut Context<Self>) {
        self.threads.confirm_delete = None;
        self.comments
            .update(cx, |comments, cx| comments.delete(id, cx))
            .detach();
        CommentDrafts::remove(cx, &DraftKey::Edit(id));
        CommentDrafts::remove(cx, &DraftKey::Reply(id));
        cx.notify();
    }

    fn apply_event(&mut self, id: CommentId, event: CommentEvent, cx: &mut Context<Self>) {
        self.threads.menu = None;
        self.comments
            .update(cx, |comments, cx| comments.apply(id, event, cx))
            .detach();
        cx.notify();
    }

    /// Debug aid until runs exist (#20): moves the focused thread along
    /// the state machine (Open → Running → Proposed → Resolved; Dismissed →
    /// Open).
    pub fn advance_focused_status(&mut self, cx: &mut Context<Self>) {
        let Some(comment) = self.threads.focused.and_then(|id| self.comment(id)) else {
            self.hint = Some("Click a comment thread first.".into());
            cx.notify();
            return;
        };
        let event = match comment.status {
            CommentStatus::Open => CommentEvent::RunStarted,
            CommentStatus::Running => CommentEvent::RunSucceeded,
            CommentStatus::Proposed => CommentEvent::Accepted,
            CommentStatus::Dismissed => CommentEvent::Reopened,
            CommentStatus::Resolved => {
                self.hint = Some("Resolved is a final status.".into());
                cx.notify();
                return;
            }
        };
        let id = comment.id;
        self.apply_event(id, event, cx);
    }

    fn toggle_menu(&mut self, id: CommentId, cx: &mut Context<Self>) {
        if let Some((closed, at)) = self.threads.menu_closed.take()
            && closed == id
            && at.elapsed() < MENU_REOPEN_GUARD
        {
            return;
        }
        self.threads.menu = if self.threads.menu == Some(id) {
            None
        } else {
            Some(id)
        };
        cx.notify();
    }

    // ---- runs ---------------------------------------------------------------

    /// The model and effort a thread would run with: its Run menu pick, or
    /// the defaults from the settings (the last run's choice).
    fn run_choice(&self, id: CommentId, cx: &App) -> (String, String) {
        if let Some(choice) = self.threads.run_choice.get(&id) {
            return choice.clone();
        }
        let settings = cx.global::<AppContext>().runs.settings();
        (settings.default_model, settings.default_effort)
    }

    /// Why a comment cannot run now, or `None` when it can.
    fn run_blocker(&self, comment: &Comment, cx: &App) -> Option<String> {
        if comment.outdated {
            return Some("Outdated comments cannot run; re-anchor them first.".to_owned());
        }
        match comment.status {
            // A Proposed comment runs again as a follow-up (a reply).
            CommentStatus::Open | CommentStatus::Proposed => {}
            CommentStatus::Running => return Some("This comment is already running.".to_owned()),
            _ => return Some("Only open comments can run.".to_owned()),
        }
        cx.global::<AppContext>().runs.claude_status().run_blocker()
    }

    /// Asks the workspace to run the thread with its Run menu choice.
    fn request_run(&mut self, id: CommentId, cx: &mut Context<Self>) {
        self.threads.run_menu = None;
        self.threads.menu = None;
        let Some(comment) = self.comment(id) else {
            return;
        };
        if let Some(blocker) = self.run_blocker(comment, cx) {
            self.hint = Some(blocker.into());
            cx.notify();
            return;
        }
        if comment.status == CommentStatus::Proposed {
            // A follow-up starts from a reply; the choice stays for it.
            self.start_reply(id, cx);
            return;
        }
        let (model, effort) = self.run_choice(id, cx);
        self.threads.run_choice.remove(&id);
        self.threads.focused = Some(id);
        cx.emit(DiffViewEvent::RunRequested {
            comment_id: id,
            model,
            effort,
        });
        cx.notify();
    }

    /// `r`: runs the focused thread with its Run menu choice.
    pub fn run_focused_comment(&mut self, cx: &mut Context<Self>) {
        match self.threads.focused {
            Some(id) if self.comment(id).is_some() => self.request_run(id, cx),
            _ => {
                self.hint = Some("Click a comment thread first.".into());
                cx.notify();
            }
        }
    }

    fn toggle_run_menu(&mut self, id: CommentId, cx: &mut Context<Self>) {
        if let Some((closed, at)) = self.threads.run_menu_closed.take()
            && closed == id
            && at.elapsed() < MENU_REOPEN_GUARD
        {
            return;
        }
        self.threads.menu = None;
        self.threads.focused = Some(id);
        self.threads.run_menu = if self.threads.run_menu == Some(id) {
            None
        } else {
            Some(id)
        };
        cx.notify();
    }

    fn pick_run_option(
        &mut self,
        id: CommentId,
        model: Option<String>,
        effort: Option<String>,
        cx: &mut Context<Self>,
    ) {
        let (mut current_model, mut current_effort) = self.run_choice(id, cx);
        if let Some(model) = model {
            current_model = model;
        }
        if let Some(effort) = effort {
            current_effort = effort;
        }
        self.threads
            .run_choice
            .insert(id, (current_model, current_effort));
        cx.notify();
    }

    // ---- re-attach ----------------------------------------------------------

    /// Enters re-attach mode for an outdated comment: the next line or range
    /// picked in the diff becomes its anchor (spec §7.3).
    pub fn start_reattach(&mut self, id: CommentId, cx: &mut Context<Self>) {
        let Some(comment) = self.comment(id).cloned() else {
            return;
        };
        self.threads.reattach = Some(Reattach { id, saving: false });
        self.hint = None;
        self.focus_requested = true;
        self.show_old_place(&comment, cx);
        cx.notify();
    }

    /// Scrolls to where an outdated comment's code was and, when its file is
    /// loaded and those lines are not folded away, selects them: after a
    /// commit the code is usually still around there, so Save (or a small
    /// adjustment) re-attaches it.
    fn show_old_place(&mut self, comment: &Comment, cx: &mut Context<Self>) {
        let anchor = &comment.anchor;
        let Some(file) = [Some(&anchor.file_path), anchor.old_file_path.as_ref()]
            .into_iter()
            .flatten()
            .find_map(|path| self.path_index.get(path).copied())
        else {
            return;
        };
        self.reveal_file(file, cx);
        let Some(loaded) = self.files[file].loaded() else {
            return;
        };
        let Some(text) = loaded.diff.text() else {
            return;
        };
        let line_count = text.source(anchor.side).line_count().max(1);
        let start = anchor.start_line.min(line_count);
        let end = anchor.end_line.clamp(start, line_count);
        let rows = &loaded.rows.rows;
        let found = layout::anchor_rows(text, rows, &[(anchor.side, start), (anchor.side, end)]);
        let (Some(first), Some(last)) = (found[0], found[1]) else {
            return;
        };
        let side = loaded.rows.split.then_some(anchor.side);
        self.selection = Some(super::Selection {
            file,
            side,
            anchor: first,
            head: last,
        });
        if !matches!(self.selection(), Some(Ok(_))) {
            self.selection = None;
        }
        self.scroll_to_row(
            (self.starts[file] + first).saturating_sub(REVEAL_OFFSET),
            cx,
        );
        self.revealed_at = Some(self.scroll_top());
    }

    /// Whether the view is waiting for the lines to re-attach a comment to.
    pub fn is_reattaching(&self) -> bool {
        self.threads.reattach.is_some()
    }

    /// Leaves re-attach mode without changing the comment.
    pub fn cancel_reattach(&mut self, cx: &mut Context<Self>) {
        if self.threads.reattach.take().is_some() {
            cx.notify();
        }
    }

    /// Save in the re-attach bar: re-attaches to the gutter selection.
    fn save_reattach_selection(&mut self, cx: &mut Context<Self>) {
        let Some(file) = self.selection.map(|selection| selection.file) else {
            self.hint = Some("Select a line or range in the gutter first.".into());
            cx.notify();
            return;
        };
        if let Some(lines) = self.resolve_selection(cx) {
            self.save_reattach(file, lines, cx);
        }
        cx.notify();
    }

    /// Re-attaches the comment of re-attach mode to `selection`: the anchor,
    /// `anchor_sha`, context hash and snippet are re-captured there.
    fn save_reattach(&mut self, file: usize, selection: LineSelection, cx: &mut Context<Self>) {
        let Some(reattach) = self.threads.reattach.as_mut() else {
            return;
        };
        if reattach.saving {
            return;
        }
        let id = reattach.id;
        let Some(anchor) = self.capture_anchor(file, &selection) else {
            self.hint = Some("These lines cannot be commented on.".into());
            cx.notify();
            return;
        };
        if let Some(reattach) = self.threads.reattach.as_mut() {
            reattach.saving = true;
        }
        let task = self
            .comments
            .update(cx, |comments, cx| comments.reattach(id, anchor, cx));
        cx.spawn(async move |this, cx| {
            let saved = task.await;
            this.update(cx, |this, cx| {
                if saved {
                    this.threads.reattach = None;
                    this.selection = None;
                    this.threads.reveal_reattached = Some(id);
                    this.comments_changed(cx);
                } else if let Some(reattach) = this.threads.reattach.as_mut() {
                    reattach.saving = false;
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    /// The re-attach mode bar under the toolbar.
    pub(super) fn render_reattach_bar(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let reattach = self.threads.reattach.as_ref()?;
        let comment = self.comment(reattach.id)?;
        let colors = cx.theme().colors.clone();
        let first = comment
            .body_md
            .lines()
            .find(|line| !line.trim().is_empty())
            .unwrap_or_default()
            .to_owned();
        let picked = match self.selection() {
            Some(Ok(lines)) if lines.end > lines.start => {
                format!("{} lines {}–{}", lines.file, lines.start, lines.end)
            }
            Some(Ok(lines)) => format!("{} line {}", lines.file, lines.start),
            _ => "select a line or range in the gutter".to_owned(),
        };
        let save = if reattach.saving {
            disabled_button("reattach-save", "Saving…", cx).into_any_element()
        } else {
            primary_button("reattach-save", "Save", cx)
                .text_xs()
                .on_click(cx.listener(|this, _, _, cx| this.save_reattach_selection(cx)))
                .into_any_element()
        };
        Some(
            div()
                .flex()
                .flex_none()
                .items_center()
                .gap_2()
                .px_2()
                .py_1()
                .border_b_1()
                .border_color(colors.border_variant)
                .bg(colors.warning.opacity(0.08))
                .text_sm()
                .child(chip("Re-attach", colors.warning))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .overflow_hidden()
                        .text_ellipsis()
                        .whitespace_nowrap()
                        .text_color(colors.text_muted)
                        .child(format!("“{first}” → {picked}")),
                )
                .child(save)
                .child(
                    button("reattach-cancel", "Cancel", cx)
                        .text_xs()
                        .on_click(cx.listener(|this, _, _, cx| this.cancel_reattach(cx))),
                )
                .into_any_element(),
        )
    }

    // ---- navigation -------------------------------------------------------

    /// The global row of a placed thread.
    fn thread_row(&self, file: usize, id: CommentId) -> Option<usize> {
        let slot = self.files.get(file)?;
        if slot.collapsed {
            return None;
        }
        let index = slot
            .threads
            .items
            .iter()
            .position(|item| *item == BlockKey::Thread(id))?;
        Some(self.starts[file] + slot.threads.first_rows[index])
    }

    /// `]c`: scrolls the next thread below the viewport's top into view.
    pub fn next_comment(&mut self, cx: &mut Context<Self>) {
        self.step_comment(true, cx);
    }

    /// `[c`: scrolls the previous thread into view.
    pub fn previous_comment(&mut self, cx: &mut Context<Self>) {
        self.step_comment(false, cx);
    }

    fn step_comment(&mut self, forward: bool, cx: &mut Context<Self>) {
        if self.files.is_empty() {
            return;
        }
        // A revealed thread sits below the sticky header and its line.
        let reference = self.top_row() + REVEAL_OFFSET;
        let mut positions: Vec<(usize, u32, CommentId)> = self
            .threads
            .snapshot
            .iter()
            .filter(|comment| self.is_inline(comment))
            .filter_map(|comment| {
                let file = *self.path_index.get(&comment.anchor.file_path)?;
                let row = self
                    .thread_row(file, comment.id)
                    // Not laid out yet: somewhere in its file, by line.
                    .unwrap_or(self.starts[file] + layout::HEADER_ROWS);
                Some((row, comment.anchor.end_line, comment.id))
            })
            .collect();
        positions.sort_unstable_by_key(|&(row, line, _)| (row, line));
        let target = if forward {
            positions.iter().find(|(row, ..)| *row > reference)
        } else {
            positions.iter().rev().find(|(row, ..)| *row < reference)
        };
        match target {
            Some(&(_, _, id)) => self.reveal_comment(id, cx),
            None => {
                self.hint = Some(if forward {
                    "No more comments below.".into()
                } else {
                    "No more comments above.".into()
                });
                cx.notify();
            }
        }
    }

    /// Scrolls to a thread (unfolding its file and the thread), loading the
    /// file first when needed. Used by the comment list and `]c` / `[c`.
    pub fn reveal_comment(&mut self, id: CommentId, cx: &mut Context<Self>) {
        if self.comment(id).is_some_and(|comment| comment.outdated) {
            self.hint =
                Some("That comment is outdated: its code is no longer in this diff.".into());
            cx.notify();
            return;
        }
        let Some(file) = self.comment_file(id) else {
            self.hint = Some("That comment's file is not in this diff.".into());
            cx.notify();
            return;
        };
        self.threads.focused = Some(id);
        if !self.threads.show_closed && self.comment(id).is_some_and(Comment::is_closed) {
            self.threads.show_closed = true;
            self.index_threads();
            self.place_all_threads();
        }
        if self.files[file].collapsed {
            self.relayout(|this| this.files[file].collapsed = false);
        }
        if self
            .comment(id)
            .is_some_and(|comment| self.is_collapsed(comment))
        {
            self.threads.toggled.remove(&id);
            self.threads.heights.remove(&BlockKey::Thread(id));
            self.with_scroll_anchor(|this| this.place_threads(file));
        }
        match self.thread_row(file, id) {
            Some(row) => {
                self.threads.pending_reveal = None;
                self.scroll_to_row(row.saturating_sub(REVEAL_OFFSET), cx);
            }
            None => {
                self.threads.pending_reveal = Some(id);
                self.scroll_to_row(self.starts[file], cx);
            }
        }
        self.revealed_at = Some(self.scroll_top());
        cx.notify();
    }

    /// Finishes a [`Self::reveal_comment`] that waited for its file.
    pub(super) fn reveal_pending(&mut self, cx: &mut Context<Self>) {
        let Some(id) = self.threads.pending_reveal else {
            return;
        };
        let Some(file) = self.comment_file(id) else {
            self.threads.pending_reveal = None;
            return;
        };
        if let Some(row) = self.thread_row(file, id) {
            self.threads.pending_reveal = None;
            self.scroll_to_row(row.saturating_sub(REVEAL_OFFSET), cx);
            self.revealed_at = Some(self.scroll_top());
        }
    }

    // ---- rendering --------------------------------------------------------

    /// The comment summary, "Show closed" and "Collapse all" of the
    /// toolbar.
    pub(super) fn render_comments_toolbar(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        if self.threads.snapshot.is_empty() {
            return None;
        }
        let closed = self.closed_count();
        let count = self.threads.snapshot.len() - closed;
        let colors = cx.theme().colors.clone();
        let collapsed = self.all_threads_collapsed();
        let show_closed = self.threads.show_closed;
        Some(
            div()
                .flex()
                .flex_none()
                .items_center()
                .gap_1()
                .child(div().text_xs().text_color(colors.text_muted).child(format!(
                    "{count} comment{}",
                    if count == 1 { "" } else { "s" }
                )))
                .when(closed > 0, |toolbar| {
                    toolbar.child(
                        button(
                            "diff-show-closed",
                            if show_closed {
                                "Hide closed".to_owned()
                            } else {
                                format!("Show closed ({closed})")
                            },
                            cx,
                        )
                        .text_xs()
                        .tooltip(text_tooltip("Resolved and dismissed comments"))
                        .on_click(cx.listener(|this, _, _, cx| this.toggle_closed_threads(cx))),
                    )
                })
                .child(
                    button(
                        "diff-collapse-comments",
                        if collapsed {
                            "Expand comments"
                        } else {
                            "Collapse comments"
                        },
                        cx,
                    )
                    .text_xs()
                    .on_click(cx.listener(|this, _, _, cx| this.toggle_all_threads(cx))),
                )
                .into_any_element(),
        )
    }

    /// Item `item` of `file`'s blocks, drawn from block row `part` (the
    /// first visible one) downward.
    pub(super) fn render_inline_block(
        &self,
        file: usize,
        item: usize,
        part: u32,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let slot = &self.files[file];
        let (Some(key), Some(loaded)) = (slot.threads.items.get(item).cloned(), slot.loaded())
        else {
            return div().into_any_element();
        };
        let side = match &key {
            BlockKey::Thread(id) => self.comment(*id).map(|comment| comment.anchor.side),
            BlockKey::Composer(draft) => self
                .threads
                .composers
                .iter()
                .find(|composer| composer.slot.draft == *draft)
                .map(|composer| composer.selection.side),
        }
        .unwrap_or(DiffSide::New);
        let total = self.card_width();
        let (left, width) = if loaded.rows.split {
            let columns = self.split_columns();
            let indent = columns.number + COMMENT_COLUMN_WIDTH;
            let left = match side {
                DiffSide::Old => indent,
                DiffSide::New => columns.half + SPLIT_DIVIDER_WIDTH + indent,
            };
            (left, columns.half - indent - CARD_MARGIN)
        } else {
            let left = self.columns().gutter + COMMENT_COLUMN_WIDTH;
            (left, total - left - CARD_MARGIN)
        };
        let (left, width) = if width < MIN_CARD_WIDTH {
            (CARD_MARGIN, (total - CARD_MARGIN * 2.0).max(px(120.0)))
        } else {
            (left, width)
        };
        let width = width.min(MAX_CARD_WIDTH);
        let card = match &key {
            BlockKey::Thread(id) => match self.comment(*id) {
                Some(comment) => self.render_thread(comment, cx),
                None => div().into_any_element(),
            },
            BlockKey::Composer(draft) => match self
                .threads
                .composers
                .iter()
                .find(|composer| composer.slot.draft == *draft)
            {
                Some(composer) => self.render_composer(composer, cx),
                None => div().into_any_element(),
            },
        };
        let theme = cx.theme().clone();
        let view = cx.entity().downgrade();
        div()
            .absolute()
            .top(-(self.metrics.row_height * part as f32))
            .left(left)
            .w(width)
            .on_children_prepainted(move |bounds, _, cx| {
                let Some(height) = bounds.first().map(|bounds| bounds.size.height) else {
                    return;
                };
                let (view, key) = (view.clone(), key.clone());
                // Not while the list is being laid out: next effect cycle.
                cx.defer(move |cx| {
                    view.update(cx, |this, cx| this.block_measured(file, key, height, cx))
                        .ok();
                });
            })
            .child(
                div()
                    .py(px(4.0))
                    .font_family(theme.fonts.ui_family.clone())
                    .text_size(theme.fonts.ui_size * 0.95)
                    .line_height(theme.fonts.ui_size * 1.45)
                    .child(card),
            )
            .into_any_element()
    }

    fn render_thread(&self, comment: &Comment, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let colors = &theme.colors;
        let id = comment.id;
        let key = SharedString::from(format!("thread-{id}"));
        let collapsed = self.is_collapsed(comment);
        let focused = self.threads.focused == Some(id);
        let now = now_seconds();
        let menu_open = self.threads.menu == Some(id);
        let anchor = &comment.anchor;
        let lines = if anchor.is_range() {
            format!("Lines {}–{}", anchor.start_line, anchor.end_line)
        } else {
            format!("Line {}", anchor.start_line)
        };
        let side = match anchor.side {
            DiffSide::Old => "old side",
            DiffSide::New => "new side",
        };

        let chevron = div()
            .id((key.clone(), 0))
            .flex()
            .flex_none()
            .items_center()
            .justify_center()
            .size(px(18.0))
            .rounded(radius::XS)
            .cursor_pointer()
            .hover(|style| style.bg(colors.element_hover))
            .on_click(cx.listener(move |this, _, _, cx| this.toggle_thread(id, cx)))
            .child(icon(
                if collapsed {
                    IconName::CaretRight
                } else {
                    IconName::CaretDown
                },
                px(12.0),
                colors.text_muted,
            ));
        let preview = collapsed.then(|| {
            let first = comment
                .body_md
                .lines()
                .next()
                .unwrap_or_default()
                .to_owned();
            div()
                .flex_1()
                .min_w_0()
                .overflow_hidden()
                .text_ellipsis()
                .whitespace_nowrap()
                .text_color(colors.text_muted)
                .child(first)
        });
        let replies = (collapsed && !comment.replies.is_empty()).then(|| {
            let count = comment.replies.len();
            div()
                .flex_none()
                .text_xs()
                .text_color(colors.text_placeholder)
                .child(format!(
                    "{count} repl{}",
                    if count == 1 { "y" } else { "ies" }
                ))
        });
        let location = div()
            .id((key.clone(), 5))
            .flex_none()
            .whitespace_nowrap()
            .text_xs()
            .text_color(colors.text_muted)
            .child(lines)
            .tooltip(text_tooltip(format!("{} ({side})", anchor.file_path)));
        let menu_button = div()
            .relative()
            .flex_none()
            .child(
                icon_button((key.clone(), 1), IconName::DotsThree, "More actions", cx)
                    .when(menu_open, |button| button.bg(colors.element_selected))
                    .on_click(cx.listener(move |this, _, _, cx| this.toggle_menu(id, cx))),
            )
            .when(menu_open, |slot| {
                slot.child(
                    deferred(
                        div()
                            .absolute()
                            .top(px(24.0))
                            .right_0()
                            .child(self.render_menu(comment, &key, cx)),
                    )
                    .with_priority(2),
                )
            });
        // The actions keep their size; the preview (or a spacer) gives way.
        let header = div()
            .flex()
            .flex_none()
            .items_center()
            .gap_2()
            .min_w_0()
            .h(THREAD_HEADER_HEIGHT)
            .pl_2()
            .pr_1()
            .when(!collapsed, |header| {
                header.border_b_1().border_color(colors.border_variant)
            })
            .child(chevron)
            .child(location)
            .child(status_chip(comment.status, colors))
            .when(comment.outdated, |header| {
                header.child(chip("Outdated", colors.warning))
            })
            .children(preview)
            .when(!collapsed, |header| header.child(div().flex_1()))
            .children(replies)
            .children(self.render_run_button(comment, &key, cx))
            .child(menu_button);

        let proposal = self
            .threads
            .proposals
            .get(&id)
            .filter(|_| comment.status == CommentStatus::Proposed && !comment.outdated)
            .and_then(|info| info.run_id.map(|run_id| (run_id, info.stat)));
        // The inset goes in the Claude message of the run that produced the
        // proposal; a run that left no message (stopped) gets its own row.
        let inset_reply = proposal.and_then(|(run_id, _)| {
            comment
                .replies
                .iter()
                .rposition(|reply| reply.run.as_ref().is_some_and(|run| run.id == run_id))
        });

        let messages = (!collapsed).then(|| {
            let root_body = match self.threads.edits.get(&id) {
                Some(slot) => self.render_editor(slot, "Save", false, cx),
                None => self.render_body(
                    &comment.body_md,
                    SharedString::from(format!("{key}-body")),
                    cx,
                ),
            };
            let root = self.render_message(
                ReplyAuthor::User,
                None,
                relative_time(comment.created_at / 1000, now),
                root_body,
                None,
                false,
                cx,
            );
            let replies = comment
                .replies
                .iter()
                .enumerate()
                .map(|(index, reply)| {
                    let body = self.render_body(
                        &reply.body_md,
                        SharedString::from(format!("reply-{}", reply.id)),
                        cx,
                    );
                    let inset = proposal
                        .filter(|_| inset_reply == Some(index))
                        .map(|(run_id, stat)| self.render_proposal_inset(id, run_id, stat, cx));
                    self.render_message(
                        reply.author,
                        reply.run.as_ref(),
                        relative_time(reply.created_at / 1000, now),
                        body,
                        inset,
                        true,
                        cx,
                    )
                })
                .collect::<Vec<_>>();
            let orphan_inset = match (proposal, inset_reply) {
                (Some((run_id, stat)), None) => Some(
                    div()
                        .px_3()
                        .py_2()
                        .border_t_1()
                        .border_color(colors.border_variant)
                        .bg(colors.claude_soft)
                        .child(self.render_proposal_inset(id, run_id, stat, cx)),
                ),
                _ => None,
            };
            div()
                .flex()
                .flex_col()
                .child(root)
                .children(replies)
                .children(orphan_inset)
        });
        let session_lost = (!collapsed
            && comment.status != CommentStatus::Running
            && self.comments.read(cx).session_lost(id))
        .then(|| {
            div()
                .flex()
                .flex_wrap()
                .items_center()
                .gap_2()
                .px_3()
                .py_1p5()
                .border_t_1()
                .border_color(colors.error.opacity(0.4))
                .bg(colors.error.opacity(0.08))
                .text_xs()
                .child(
                    div()
                        .flex_1()
                        .min_w(px(160.0))
                        .text_color(colors.error)
                        .child(
                            "Claude could not resume this comment's session (session not \
                             found).",
                        ),
                )
                .child(
                    primary_button((key.clone(), 4), "Run fresh with the thread as context", cx)
                        .h(px(22.0))
                        .text_xs()
                        .on_click(cx.listener(move |this, _, _, cx| this.run_fresh(id, cx))),
                )
        });
        let confirm = (self.threads.confirm_delete == Some(id)).then(|| {
            confirm_bar(
                &key,
                "Delete this thread and its replies?",
                "Delete",
                cx.listener(move |this, _, _, cx| this.delete_thread(id, cx)),
                "Cancel",
                cx.listener(|this, _, _, cx| {
                    this.threads.confirm_delete = None;
                    cx.notify();
                }),
                colors.error,
                cx,
            )
        });
        let confirm_reject = (self.threads.confirm_reject == Some(id)).then(|| {
            confirm_bar(
                &SharedString::from(format!("{key}-reject")),
                "Reject this proposal? Its worktree is deleted and the comment goes back to Open.",
                "Reject",
                cx.listener(move |this, _, _, cx| this.reject_proposal(id, cx)),
                "Cancel",
                cx.listener(|this, _, _, cx| {
                    this.threads.confirm_reject = None;
                    cx.notify();
                }),
                colors.error,
                cx,
            )
        });
        let reply_box = (!collapsed).then(|| {
            let followup = self.offers_followup(id);
            let content = match self.threads.replies.get(&id) {
                Some(slot) if followup => {
                    self.render_editor(slot, "Send as follow-up run", true, cx)
                }
                Some(slot) => self.render_editor(slot, "Reply", false, cx),
                None => div()
                    .id((key.clone(), 2))
                    .flex()
                    .items_center()
                    .gap_2()
                    .h(px(30.0))
                    .pl_2()
                    .pr_1p5()
                    .rounded(radius::SM)
                    .border_1()
                    .border_color(colors.border)
                    .bg(colors.background)
                    .cursor_text()
                    .on_click(cx.listener(move |this, _, _, cx| this.start_reply(id, cx)))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .overflow_hidden()
                            .text_ellipsis()
                            .whitespace_nowrap()
                            .text_color(colors.text_placeholder)
                            .child(REPLY_HINT),
                    )
                    .child(kbd(submit_keys(), cx))
                    .into_any_element(),
            };
            div().px_3().py_2().child(content)
        });

        // Inline in the diff, not floating over it: a border and no shadow.
        div()
            .id(key.clone())
            .relative()
            .flex()
            .flex_col()
            .max_w(MAX_CARD_WIDTH)
            .rounded(radius::MD)
            .border_1()
            .border_color(colors.border)
            .bg(colors.elevated_surface)
            // Keeps the Claude bands inside the rounded corners.
            .overflow_hidden()
            .text_color(colors.text)
            .cursor_default()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _: &MouseDownEvent, _, cx| {
                    cx.stop_propagation();
                    this.focus_thread(id, cx);
                }),
            )
            .child(header)
            .children(messages)
            .children(session_lost)
            .children(confirm)
            .children(confirm_reject)
            .children(reply_box)
            .when(focused, |card| {
                card.child(
                    div()
                        .absolute()
                        .top_0()
                        .bottom_0()
                        .left_0()
                        .w(px(2.0))
                        .bg(colors.accent),
                )
            })
            .into_any_element()
    }

    /// One message of a thread: the author, the meta and the body. Claude's
    /// messages sit on a `claude_soft` band; `run` adds its model, effort
    /// and duration to the meta.
    #[allow(clippy::too_many_arguments)]
    fn render_message(
        &self,
        author: ReplyAuthor,
        run: Option<&ReplyRun>,
        time: String,
        body: AnyElement,
        inset: Option<AnyElement>,
        separated: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme().clone();
        let colors = &theme.colors;
        let claude = author == ReplyAuthor::Claude;
        let (name, name_color) = if claude {
            ("Claude", colors.claude_text)
        } else {
            ("You", colors.text)
        };
        let choice = run.filter(|_| claude).map(|run| {
            let choice = if run.effort.trim().is_empty() {
                run.model.clone()
            } else {
                format!("{}, {}", run.model, run.effort)
            };
            div()
                .flex_none()
                .font_family(theme.fonts.code_family.clone())
                .text_color(colors.text_muted)
                .child(choice)
        });
        let mut when = time;
        if let Some(ms) = run.filter(|_| claude).and_then(|run| run.duration_ms) {
            when.push_str(" · ran ");
            when.push_str(&format_ran(ms));
        }
        let meta = div()
            .flex()
            .items_center()
            .gap_2()
            .min_w_0()
            .overflow_hidden()
            .whitespace_nowrap()
            .text_xs()
            .child(
                div()
                    .flex_none()
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(name_color)
                    .child(name),
            )
            .children(choice)
            .child(
                div()
                    .min_w_0()
                    .overflow_hidden()
                    .text_ellipsis()
                    .text_color(colors.text_placeholder)
                    .child(when),
            );
        // The proposal is a row of the message under a hairline, not a box.
        let inset = inset.map(|inset| {
            div()
                .mt_1()
                .pt_2()
                .border_t_1()
                .border_color(colors.border_variant)
                .child(inset)
        });
        div()
            .flex()
            .flex_col()
            .gap_1()
            .min_w_0()
            .px_3()
            .py_2()
            .when(separated, |row| {
                row.border_t_1().border_color(colors.border_variant)
            })
            .when(claude, |row| row.bg(colors.claude_soft))
            .child(meta)
            .child(body)
            .children(inset)
            .into_any_element()
    }

    /// The proposal a run left, inside Claude's message: its size and the
    /// View diff / Reject / Accept decision of the proposal tab.
    fn render_proposal_inset(
        &self,
        id: CommentId,
        run_id: RunId,
        stat: Option<DiffStat>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let colors = cx.theme().colors.clone();
        let key = SharedString::from(format!("proposal-inset-{id}"));
        let rejecting = self.threads.rejecting.contains(&id);
        let size = stat.map(|stat| {
            div()
                .flex()
                .flex_none()
                .items_center()
                .gap_2()
                .text_xs()
                .child(div().text_color(colors.text_muted).child(format!(
                    "{} file{}",
                    stat.files,
                    if stat.files == 1 { "" } else { "s" }
                )))
                .child(
                    div()
                        .text_color(colors.diff_added_text)
                        .child(format!("+{}", stat.added)),
                )
                .child(
                    div()
                        .text_color(colors.diff_removed_text)
                        .child(format!("−{}", stat.removed)),
                )
        });
        let reject = if rejecting {
            disabled_button((key.clone(), 1), "Rejecting…", cx).into_any_element()
        } else {
            reject_button((key.clone(), 1), cx)
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.threads.confirm_reject = Some(id);
                    cx.notify();
                }))
                .into_any_element()
        };
        let accept = if rejecting {
            disabled_button_with_icon((key.clone(), 2), IconName::Check, "Accept", cx)
                .into_any_element()
        } else {
            accept_button((key.clone(), 2), "Accept", cx)
                .on_click(cx.listener(move |_, _, _, cx| {
                    cx.emit(DiffViewEvent::AcceptProposal(run_id));
                }))
                .into_any_element()
        };
        div()
            .flex()
            .flex_wrap()
            .items_center()
            .gap_2()
            .child(icon(IconName::GitPullRequest, px(14.0), colors.accent))
            .child(
                div()
                    .flex_none()
                    .text_sm()
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(colors.text)
                    .child("Proposal"),
            )
            .children(size)
            .child(div().flex_1())
            .child(
                div()
                    .flex()
                    .flex_none()
                    .items_center()
                    .gap_1()
                    .child(
                        button((key.clone(), 0), "View diff", cx).on_click(
                            cx.listener(move |this, _, _, cx| this.view_proposal(id, cx)),
                        ),
                    )
                    .child(reject)
                    .child(accept),
            )
            .into_any_element()
    }

    fn render_body(&self, body: &str, id: SharedString, cx: &mut Context<Self>) -> AnyElement {
        match self.threads.markdown.get(body) {
            Some(doc) => markdown::render(&doc, id, cx),
            None => div().child(body.to_owned()).into_any_element(),
        }
    }

    fn render_menu(
        &self,
        comment: &Comment,
        key: &SharedString,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let colors = cx.theme().colors.clone();
        let id = comment.id;
        let item = |index: usize, label: &'static str, enabled: bool| {
            div()
                .id((key.clone(), 10 + index))
                .px_2()
                .py_0p5()
                .rounded_sm()
                .whitespace_nowrap()
                .when(enabled, |item| {
                    item.cursor_pointer()
                        .text_color(colors.text)
                        .hover(|style| style.bg(colors.element_hover))
                })
                .when(!enabled, |item| item.text_color(colors.text_placeholder))
                .child(label)
        };
        let status_item = match comment.status {
            CommentStatus::Open | CommentStatus::Proposed => Some(
                item(2, "Dismiss", true).on_click(cx.listener(move |this, _, _, cx| {
                    this.apply_event(id, CommentEvent::Dismissed, cx)
                })),
            ),
            CommentStatus::Dismissed => Some(item(2, "Reopen", true).on_click(
                cx.listener(move |this, _, _, cx| this.apply_event(id, CommentEvent::Reopened, cx)),
            )),
            CommentStatus::Running | CommentStatus::Resolved => None,
        };
        let debug_item = cfg!(debug_assertions).then(|| {
            item(5, "Advance status (debug)", true).on_click(cx.listener(move |this, _, _, cx| {
                this.threads.focused = Some(id);
                this.threads.menu = None;
                this.advance_focused_status(cx);
            }))
        });
        div()
            .id((key.clone(), 9))
            .occlude()
            .flex()
            .flex_col()
            .min_w(px(180.0))
            .p_1()
            .rounded_md()
            .border_1()
            .border_color(colors.border)
            .bg(colors.elevated_surface)
            .shadow_lg()
            .on_mouse_down_out(cx.listener(move |this, _, _, cx| {
                if this.threads.menu == Some(id) {
                    this.threads.menu = None;
                    this.threads.menu_closed = Some((id, Instant::now()));
                    cx.notify();
                }
            }))
            .child(
                item(0, "Edit", true)
                    .on_click(cx.listener(move |this, _, _, cx| this.start_edit(id, cx))),
            )
            .child(
                item(1, "Delete…", true)
                    .on_click(cx.listener(move |this, _, _, cx| this.request_delete(id, cx))),
            )
            .children(status_item)
            .child(div().my_0p5().h(px(1.0)).bg(colors.border_variant))
            .child({
                let enabled = self.run_blocker(comment, cx).is_none();
                let (model, effort) = self.run_choice(id, cx);
                let label: SharedString = format!(
                    "{} with Claude ({})",
                    run_verb(comment.status),
                    choice_label(&model, &effort)
                )
                .into();
                item(3, "", enabled).child(label).when(enabled, |item| {
                    item.on_click(cx.listener(move |this, _, _, cx| this.request_run(id, cx)))
                })
            })
            .child(
                item(
                    4,
                    "View proposal",
                    comment.status == CommentStatus::Proposed,
                )
                .when(comment.status == CommentStatus::Proposed, |item| {
                    item.on_click(cx.listener(move |this, _, _, cx| this.view_proposal(id, cx)))
                }),
            )
            .children(debug_item)
            .into_any_element()
    }

    /// The "Run · sonnet · medium ▾" split button of an open thread: the
    /// main part runs with the shown choice, the arrow opens the Run menu.
    /// While the comment cannot run (outdated, Claude unavailable) the
    /// button is greyed out and a click shows the reason in the hint bar.
    /// A proposal's follow-up is sent from the reply box, so it has none.
    fn render_run_button(
        &self,
        comment: &Comment,
        key: &SharedString,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        if !matches!(
            comment.status,
            CommentStatus::Open | CommentStatus::Running | CommentStatus::Proposed
        ) || self.offers_followup(comment.id)
        {
            return None;
        }
        let colors = cx.theme().colors.clone();
        let id = comment.id;
        if comment.status == CommentStatus::Running {
            return Some(
                div()
                    .flex_none()
                    .px_1p5()
                    .text_xs()
                    .text_color(colors.warning)
                    .child("Running…")
                    .into_any_element(),
            );
        }
        let (model, effort) = self.run_choice(id, cx);
        let verb = run_verb(comment.status);
        let label = format!("{verb} · {}", choice_label(&model, &effort));
        if let Some(blocker) = self.run_blocker(comment, cx) {
            let hint: SharedString = blocker.into();
            return Some(
                div()
                    .id((key.clone(), 20))
                    .flex_none()
                    .flex()
                    .items_center()
                    .h(px(20.0))
                    .px_1p5()
                    .rounded_md()
                    .border_1()
                    .border_color(colors.border_variant)
                    .text_xs()
                    .text_color(colors.text_placeholder)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.hint = Some(hint.clone());
                        cx.notify();
                    }))
                    .child(label)
                    .into_any_element(),
            );
        }
        let open = self.threads.run_menu == Some(id);
        let (accent_border, accent_soft) = (colors.accent_border, colors.accent_soft);
        let split = div()
            .flex()
            .items_center()
            .h(px(20.0))
            .rounded_md()
            .border_1()
            .border_color(accent_border)
            .text_xs()
            .text_color(colors.text)
            .overflow_hidden()
            .child(
                div()
                    .id((key.clone(), 20))
                    .flex()
                    .items_center()
                    .h_full()
                    .px_1p5()
                    .cursor_pointer()
                    .whitespace_nowrap()
                    .hover(move |style| style.bg(accent_soft))
                    .on_click(cx.listener(move |this, _, _, cx| this.request_run(id, cx)))
                    .child(label),
            )
            .child(
                div()
                    .id((key.clone(), 21))
                    .flex()
                    .items_center()
                    .h_full()
                    .px_1()
                    .border_l_1()
                    .border_color(accent_border)
                    .cursor_pointer()
                    .when(open, |arrow| arrow.bg(colors.element_selected))
                    .hover(move |style| style.bg(accent_soft))
                    .on_click(cx.listener(move |this, _, _, cx| this.toggle_run_menu(id, cx)))
                    .child("▾"),
            );
        Some(
            div()
                .relative()
                .flex_none()
                .child(split)
                .when(open, |slot| {
                    slot.child(
                        deferred(
                            div()
                                .absolute()
                                .top(px(24.0))
                                .right_0()
                                .child(self.render_run_menu(id, key, cx)),
                        )
                        .with_priority(2),
                    )
                })
                .into_any_element(),
        )
    }

    /// The Run menu: every model and effort on offer, and Run.
    fn render_run_menu(
        &self,
        id: CommentId,
        key: &SharedString,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let colors = cx.theme().colors.clone();
        let (model, effort) = self.run_choice(id, cx);
        let section = |title: &'static str| {
            div()
                .px_2()
                .pt_1()
                .text_xs()
                .text_color(colors.text_placeholder)
                .child(title)
        };
        let option = |index: usize, label: String, selected: bool| {
            div()
                .id((key.clone(), 100 + index))
                .flex()
                .items_center()
                .gap_1()
                .px_2()
                .py_0p5()
                .rounded_sm()
                .whitespace_nowrap()
                .cursor_pointer()
                .text_color(colors.text)
                .hover(|style| style.bg(colors.element_hover))
                .child(
                    div()
                        .w(px(12.0))
                        .flex_none()
                        .text_color(colors.accent)
                        .child(if selected { "✓" } else { "" }),
                )
                .child(label)
        };
        let models = MODELS.iter().enumerate().map(|(index, &name)| {
            let name = name.to_owned();
            option(index, name.clone(), name == model).on_click(cx.listener(
                move |this, _, _, cx| this.pick_run_option(id, Some(name.clone()), None, cx),
            ))
        });
        let offset = MODELS.len();
        let efforts = EFFORTS.iter().enumerate().map(|(index, &name)| {
            let name = name.to_owned();
            option(offset + index, name.clone(), name == effort).on_click(cx.listener(
                move |this, _, _, cx| this.pick_run_option(id, None, Some(name.clone()), cx),
            ))
        });
        let effort_note = cx
            .global::<AppContext>()
            .runs
            .claude_status()
            .warning()
            .map(|_| {
                div()
                    .px_2()
                    .text_xs()
                    .text_color(colors.warning)
                    .child("This Claude Code ignores the effort.")
            });
        div()
            .id((key.clone(), 99))
            .occlude()
            .flex()
            .flex_col()
            .min_w(px(180.0))
            .p_1()
            .rounded_md()
            .border_1()
            .border_color(colors.border)
            .bg(colors.elevated_surface)
            .shadow_lg()
            .text_sm()
            .on_mouse_down_out(cx.listener(move |this, _, _, cx| {
                if this.threads.run_menu == Some(id) {
                    this.threads.run_menu = None;
                    this.threads.run_menu_closed = Some((id, Instant::now()));
                    cx.notify();
                }
            }))
            .child(section("MODEL"))
            .children(models)
            .child(section("EFFORT"))
            .children(efforts)
            .children(effort_note)
            .child(div().my_0p5().h(px(1.0)).bg(colors.border_variant))
            .child(
                div().p_0p5().child(
                    primary_button(
                        (key.clone(), 98),
                        format!(
                            "{} · {}",
                            run_verb(self.comment(id).map_or(CommentStatus::Open, |c| c.status)),
                            choice_label(&model, &effort)
                        ),
                        cx,
                    )
                    .w_full()
                    .h(px(24.0))
                    .text_xs()
                    .on_click(cx.listener(move |this, _, _, cx| this.request_run(id, cx))),
                ),
            )
            .into_any_element()
    }

    fn render_composer(&self, composer: &Composer, cx: &mut Context<Self>) -> AnyElement {
        let colors = cx.theme().colors.clone();
        let selection = &composer.selection;
        let lines = if selection.end > selection.start {
            format!("lines {}–{}", selection.start, selection.end)
        } else {
            format!("line {}", selection.start)
        };
        let side = match selection.side {
            DiffSide::Old => "old side",
            DiffSide::New => "new side",
        };
        div()
            .flex()
            .flex_col()
            .gap_2()
            .p_2()
            .rounded_md()
            .border_1()
            .border_color(colors.accent_border)
            .bg(colors.elevated_surface)
            .text_color(colors.text)
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .child(
                div()
                    .text_xs()
                    .text_color(colors.text_muted)
                    .child(format!("New comment on {lines} ({side})")),
            )
            .child(self.render_editor(&composer.slot, "Comment", false, cx))
            .into_any_element()
    }

    /// An editor with its discard confirmation and Cancel / save buttons.
    fn render_editor(
        &self,
        slot: &EditorSlot,
        save: &'static str,
        followup: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let followup_id = match &slot.draft {
            DraftKey::Reply(id) if followup => Some(*id),
            _ => None,
        };
        let colors = cx.theme().colors.clone();
        let key = SharedString::from(format!("editor-{:?}", slot.draft));
        let (cancel_draft, save_draft) = (slot.draft.clone(), slot.draft.clone());
        let note_draft = slot.draft.clone();
        let confirm = slot.confirm_discard.then(|| {
            let (discard, keep) = (slot.draft.clone(), slot.draft.clone());
            confirm_bar(
                &key,
                "Discard this draft?",
                "Discard",
                cx.listener(move |this, _, _, cx| this.discard_editor(&discard, cx)),
                "Keep editing",
                cx.listener(move |this, _, _, cx| this.keep_editing(&keep, cx)),
                colors.warning,
                cx,
            )
        });
        let save_keys = submit_keys();
        let save_button = if slot.saving {
            disabled_button((key.clone(), 1), "Saving…", cx).into_any_element()
        } else {
            let on_save = cx.listener(move |this, _, _, cx| this.submit_editor(&save_draft, cx));
            // Accept stays the one primary action while a proposal waits.
            if followup {
                button((key.clone(), 1), save, cx)
                    .on_click(on_save)
                    .into_any_element()
            } else {
                primary_button((key.clone(), 1), save, cx)
                    .h(px(24.0))
                    .on_click(on_save)
                    .into_any_element()
            }
        };
        // A reply on a proposal: the follow-up's model and effort, and the
        // note-only alternative.
        let followup_controls = followup_id.filter(|_| !slot.saving).map(|id| {
            // The thread's Run menu choice; the button opens the menu, which
            // opens upward since the reply box ends the thread.
            let (model, effort) = self.run_choice(id, cx);
            let open = self.threads.run_menu == Some(id);
            (
                div()
                    .relative()
                    .flex_none()
                    .child(
                        button((key.clone(), 3), choice_label(&model, &effort), cx)
                            .h(px(20.0))
                            .text_xs()
                            .text_color(colors.text_muted)
                            .when(open, |button| button.bg(colors.element_selected))
                            .on_click(
                                cx.listener(move |this, _, _, cx| this.toggle_run_menu(id, cx)),
                            ),
                    )
                    .when(open, |slot| {
                        slot.child(
                            deferred(
                                div()
                                    .absolute()
                                    .bottom(px(24.0))
                                    .right_0()
                                    .child(self.render_run_menu(id, &key, cx)),
                            )
                            .with_priority(2),
                        )
                    }),
                button((key.clone(), 2), "Add note only", cx).on_click(
                    cx.listener(move |this, _, _, cx| this.submit_editor_as(&note_draft, true, cx)),
                ),
            )
        });
        let (choice, note_button) = match followup_controls {
            Some((choice, note)) => (Some(choice), Some(note)),
            None => (None, None),
        };
        let hint = if followup {
            format!("{save_keys} to send as a follow-up · Esc to cancel")
        } else {
            format!("Markdown · {save_keys} to save · Esc to cancel")
        };
        div()
            .flex()
            .flex_col()
            .gap_1p5()
            .child(slot.editor.clone())
            .children(confirm)
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .justify_end()
                    .gap_2()
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(120.0))
                            .overflow_hidden()
                            .text_ellipsis()
                            .whitespace_nowrap()
                            .text_xs()
                            .text_color(colors.text_placeholder)
                            .child(hint),
                    )
                    .children(choice)
                    .child(button((key.clone(), 0), "Cancel", cx).on_click(
                        cx.listener(move |this, _, _, cx| this.cancel_editor(&cancel_draft, cx)),
                    ))
                    .children(note_button)
                    .child(save_button),
            )
            .into_any_element()
    }
}

/// A one-line question with two buttons, inside a card.
#[allow(clippy::too_many_arguments)]
fn confirm_bar(
    key: &SharedString,
    question: &'static str,
    yes: &'static str,
    on_yes: impl Fn(&gpui::ClickEvent, &mut gpui::Window, &mut App) + 'static,
    no: &'static str,
    on_no: impl Fn(&gpui::ClickEvent, &mut gpui::Window, &mut App) + 'static,
    color: Hsla,
    cx: &App,
) -> AnyElement {
    let colors = &cx.theme().colors;
    div()
        .flex()
        .items_center()
        .gap_2()
        .px_3()
        .py_1()
        .bg(color.opacity(0.1))
        .border_t_1()
        .border_color(color.opacity(0.4))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .overflow_hidden()
                .text_ellipsis()
                .whitespace_nowrap()
                .text_color(colors.text)
                .child(question),
        )
        .child(button((key.clone(), 20), no, cx).on_click(on_no))
        .child(
            button((key.clone(), 21), yes, cx)
                .text_color(color)
                .on_click(on_yes),
        )
        .into_any_element()
}

/// The keys of [`SubmitComment`], for the keycap of reply inputs.
fn submit_keys() -> String {
    crate::keymap::SHORTCUTS
        .iter()
        .find(|shortcut| shortcut.binding().action().as_any().is::<SubmitComment>())
        .map(|shortcut| shortcut.display_keys())
        .unwrap_or_default()
}

/// A run's duration for "ran 42s" / "ran 3m 5s".
fn format_ran(ms: i64) -> String {
    let seconds = (ms.max(0) + 500) / 1000;
    if seconds < 60 {
        format!("{seconds}s")
    } else {
        format!("{}m {}s", seconds / 60, seconds % 60)
    }
}

/// `sonnet · medium`, or just the model without an effort.
/// The verb of the Run button: a Proposed comment runs again as a
/// follow-up, which starts from a reply.
/// Which status a line ending several threads shows: Claude at work first,
/// then what waits for the user, then the quiet states.
fn marker_rank(status: CommentStatus) -> u8 {
    match status {
        CommentStatus::Running => 4,
        CommentStatus::Proposed => 3,
        CommentStatus::Open => 2,
        CommentStatus::Resolved => 1,
        CommentStatus::Dismissed => 0,
    }
}

fn run_verb(status: CommentStatus) -> &'static str {
    if status == CommentStatus::Proposed {
        "Follow up"
    } else {
        "Run"
    }
}

fn choice_label(model: &str, effort: &str) -> String {
    if effort.is_empty() {
        model.to_owned()
    } else {
        format!("{model} · {effort}")
    }
}
