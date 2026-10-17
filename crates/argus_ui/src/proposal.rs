//! The proposal tab: what a run changed, as its own diff, and the one
//! decision on it (spec §6.5).
//!
//! A [`ProposalView`] opens from "View proposal" on a `Proposed` comment.
//! The run manager snapshots the run's worktree and hands back a
//! [`Proposal`] whose diff (`start_sha → worktree`, new and deleted files
//! included) is shown by the same [`DiffView`] as the main diff, with
//! highlighting, in either mode. Files other than the commented one come
//! first, badged "outside comment scope".
//!
//! Above the diff: the comment (linking back to its thread in the main
//! diff), the run's model · effort · turns · cost · duration, Claude's final
//! summary, and a banner when the head branch moved since the run started.
//!
//! Actions: Accept (`a`), Reject (`x`, asks first), Reply (`r`) and Open in
//! editor. A reply is sent as a follow-up run by default (the same Claude
//! session, in this worktree, with the picked model and effort) or added as
//! a note only. While the follow-up runs, the diff follows its edits and
//! Accept / Reject wait; when it ends, the tab shows the combined proposal
//! of the latest run. A follow-up whose session is gone offers a fresh run
//! with the thread as context. A file watcher on the worktree takes a new snapshot after manual
//! edits, so the diff follows them. Every git call runs on the tokio
//! runtime; this view only stores results.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use argus_diff::Side as DiffSide;
use argus_diff::review::{CommentId, CommentStatus, ReplyAuthor};
use argus_runs::{Proposal, RunUpdate};
use argus_store::{RunId, RunStatus};
use gpui::{
    AnyElement, Context, Entity, EventEmitter, FocusHandle, Focusable, FontWeight, SharedString,
    Subscription, Task, Window, div, prelude::*, px,
};
use tokio::sync::broadcast::error::RecvError;

use crate::AppContext;
use crate::comments::ReviewComments;
use crate::components::{button, disabled_button, kbd, placeholder, primary_button};
use crate::diff_view::{DiffView, DiffViewEvent, DisplayOptions};
use crate::followup::{self, FollowupKind};
use crate::keymap::{Accept, PROPOSAL_CONTEXT, RejectProposal, ReplyToProposal, SHORTCUTS};
use crate::markdown::{self, Markdown, MarkdownCache};
use crate::runtime::spawn_job;
use crate::session::DiffTarget;
use crate::text_editor::{TextEditor, TextEditorEvent};
use crate::theme::ActiveTheme;

/// Changes on disk are collected this long before a new snapshot, so an
/// editor's save (often several writes) refreshes once, well within 1 s.
const REFRESH_DEBOUNCE: Duration = Duration::from_millis(150);

/// What the tab shows the proposal of.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProposalSource {
    /// The latest reviewable run of a comment.
    Comment(CommentId),
    /// One run.
    Run(RunId),
}

/// Events for the workspace.
#[derive(Clone, Debug)]
pub enum ProposalEvent {
    /// Show the comment's thread in the main diff.
    RevealComment(CommentId),
    /// A message for the status bar.
    Notice {
        /// Text to show.
        message: SharedString,
        /// Error (red) rather than warning.
        is_error: bool,
    },
    /// Accept was asked for the run.
    AcceptRequested(RunId),
    /// The user replied to the proposal; the reply is saved in the thread.
    Replied {
        /// The comment.
        comment_id: CommentId,
        /// The follow-up run the reply started; `None` for a note only.
        followup: Option<RunId>,
    },
    /// The proposal was rejected (here or elsewhere); the tab closes.
    Rejected(RunId),
}

/// The proposal and its diff view.
struct Loaded {
    proposal: Proposal,
    view: Entity<DiffView>,
    _subscription: Subscription,
    /// How many files are outside the comment's scope, once listed.
    out_of_scope: Option<usize>,
}

enum State {
    Loading,
    Ready(Box<Loaded>),
    Failed(SharedString),
    Rejected,
}

/// One proposal tab.
pub struct ProposalView {
    focus_handle: FocusHandle,
    source: ProposalSource,
    state: State,
    display: DisplayOptions,
    /// A review-less comment list: the proposal diff takes no comments.
    no_comments: Entity<ReviewComments>,
    markdown: MarkdownCache,
    body_doc: Option<Arc<Markdown>>,
    summary_doc: Option<Arc<Markdown>>,
    reply: Option<Entity<TextEditor>>,
    reply_open: bool,
    sending_reply: bool,
    _reply_subscription: Option<Subscription>,
    /// The follow-up running in this proposal's worktree.
    followup_run: Option<RunId>,
    /// The latest follow-up could not resume its Claude session.
    session_lost: bool,
    confirm_reject: bool,
    rejecting: bool,
    refreshing: bool,
    focus_requested: bool,
    load_task: Option<Task<()>>,
    watch_task: Option<Task<()>>,
    refresh_task: Option<Task<()>>,
    action_task: Option<Task<()>>,
    count_task: Option<Task<()>>,
    _runs_task: Task<()>,
}

impl EventEmitter<ProposalEvent> for ProposalView {}

impl Focusable for ProposalView {
    fn focus_handle(&self, _: &gpui::App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl ProposalView {
    /// Opens the proposal of `source` and starts loading it; the tab takes
    /// the keyboard focus at its first render.
    pub fn new(source: ProposalSource, display: DisplayOptions, cx: &mut Context<Self>) -> Self {
        let mut runs = cx.global::<AppContext>().runs.subscribe_runs();
        let runs_task = cx.spawn(async move |this, cx| {
            loop {
                let update = match runs.recv().await {
                    Ok(update) => update,
                    Err(RecvError::Lagged(_)) => continue,
                    Err(RecvError::Closed) => break,
                };
                let alive = this
                    .update(cx, |this, cx| this.run_update(update, cx))
                    .is_ok();
                if !alive {
                    break;
                }
            }
        });
        let mut this = Self {
            focus_handle: cx.focus_handle(),
            source,
            state: State::Loading,
            display,
            no_comments: cx.new(ReviewComments::new),
            markdown: MarkdownCache::default(),
            body_doc: None,
            summary_doc: None,
            reply: None,
            reply_open: false,
            sending_reply: false,
            _reply_subscription: None,
            followup_run: None,
            session_lost: false,
            confirm_reject: false,
            rejecting: false,
            refreshing: false,
            focus_requested: true,
            load_task: None,
            watch_task: None,
            refresh_task: None,
            action_task: None,
            count_task: None,
            _runs_task: runs_task,
        };
        this.load(cx);
        this
    }

    /// Whether this tab shows `source` (by comment, or by run once loaded).
    pub fn shows(&self, source: ProposalSource) -> bool {
        if self.source == source {
            return true;
        }
        let State::Ready(loaded) = &self.state else {
            return false;
        };
        match source {
            ProposalSource::Run(run_id) => loaded.proposal.run.id == run_id,
            ProposalSource::Comment(id) => loaded.proposal.comment.id == id,
        }
    }

    /// The tab title.
    pub fn title(&self) -> SharedString {
        match &self.state {
            State::Ready(loaded) => {
                let anchor = &loaded.proposal.comment.anchor;
                let name = anchor
                    .file_path
                    .rsplit('/')
                    .next()
                    .unwrap_or(&anchor.file_path);
                format!("Proposal · {name}:{}", anchor.start_line).into()
            }
            State::Failed(_) => "Proposal (error)".into(),
            State::Loading => "Proposal…".into(),
            State::Rejected => "Proposal (rejected)".into(),
        }
    }

    /// The proposal's diff view, for the workspace's navigation keys.
    pub fn diff_view(&self) -> Option<Entity<DiffView>> {
        match &self.state {
            State::Ready(loaded) => Some(loaded.view.clone()),
            _ => None,
        }
    }

    /// Applies the workspace's display options (mode, wrap, tab width).
    pub fn set_options(&mut self, display: DisplayOptions, cx: &mut Context<Self>) {
        self.display = display;
        if let Some(view) = self.diff_view() {
            view.update(cx, |view, cx| view.set_options(display, cx));
        }
    }

    /// Takes the keyboard focus at the next render.
    pub fn request_focus(&mut self, cx: &mut Context<Self>) {
        self.focus_requested = true;
        cx.notify();
    }

    // ---- loading ------------------------------------------------------------

    fn load(&mut self, cx: &mut Context<Self>) {
        let runs = cx.global::<AppContext>().runs.clone();
        let source = self.source;
        let job = spawn_job(cx, async move {
            let run_id = match source {
                ProposalSource::Run(run_id) => run_id,
                ProposalSource::Comment(id) => runs.proposal_run(id).await?,
            };
            let proposal = runs.open_proposal(run_id).await?;
            let watch = runs.watch_proposal(&proposal);
            Ok::<_, argus_runs::ProposalError>((proposal, watch))
        });
        self.load_task = Some(cx.spawn(async move |this, cx| {
            let result = job.await;
            this.update(cx, |this, cx| {
                this.load_task = None;
                match result {
                    Ok(Ok((proposal, watch))) => {
                        let keep = this.diff_view().map(|view| {
                            let view = view.read(cx);
                            (view.viewed_paths(), view.active_path())
                        });
                        this.show(proposal, keep, cx);
                        this.start_watching(watch, cx);
                    }
                    Ok(Err(error)) => {
                        tracing::warn!(%error, "cannot open the proposal");
                        this.state = State::Failed(error.to_string().into());
                    }
                    Err(error) => this.state = State::Failed(error.to_string().into()),
                }
                cx.notify();
            })
            .ok();
        }));
    }

    /// Shows `proposal`, in a new diff view unless its tree is the one
    /// shown. `keep` carries the viewed marks and the reading position of
    /// the previous view.
    fn show(
        &mut self,
        proposal: Proposal,
        keep: Option<(HashSet<String>, Option<String>)>,
        cx: &mut Context<Self>,
    ) {
        if let State::Ready(loaded) = &mut self.state
            && loaded.proposal.tree == proposal.tree
        {
            // A follow-up that changed nothing still brings a new summary.
            if loaded.proposal.run.id != proposal.run.id {
                self.summary_doc = proposal.run.summary.as_deref().map(|summary| {
                    self.markdown
                        .prepare(summary, |this: &mut Self| &mut this.markdown, cx)
                });
            }
            loaded.proposal = proposal;
            cx.notify();
            return;
        }
        let body = proposal.comment.body_md.clone();
        self.body_doc = Some(self.markdown.prepare(
            &body,
            |this: &mut Self| &mut this.markdown,
            cx,
        ));
        self.summary_doc = proposal.run.summary.as_deref().map(|summary| {
            self.markdown
                .prepare(summary, |this: &mut Self| &mut this.markdown, cx)
        });
        let target = DiffTarget {
            base: proposal.start_sha.short(8),
            head: "worktree".to_owned(),
            base_tip: proposal.start_sha,
            head_tip: proposal.tree,
            merge_base: proposal.start_sha,
        };
        let (viewed, reveal) = keep.unwrap_or_default();
        let session = proposal.session.clone();
        let display = self.display;
        let no_comments = self.no_comments.clone();
        let view = cx.new(|cx| {
            let mut view =
                DiffView::new_proposal(session, target, display, viewed, no_comments, cx);
            if let Some(path) = reveal {
                view.reveal_path(path, cx);
            }
            view
        });
        let subscription = cx.subscribe(&view, |this, _, event: &DiffViewEvent, cx| {
            if let DiffViewEvent::OpenInEditor(path) = event {
                this.open_in_editor(path.clone(), cx);
            }
        });
        self.count_out_of_scope(&proposal, cx);
        self.state = State::Ready(Box::new(Loaded {
            proposal,
            view,
            _subscription: subscription,
            out_of_scope: None,
        }));
        cx.notify();
    }

    /// Counts the files outside the comment's scope once the (shared) file
    /// list is computed.
    fn count_out_of_scope(&mut self, proposal: &Proposal, cx: &mut Context<Self>) {
        let session = proposal.session.clone();
        let tree = proposal.tree;
        let job = spawn_job(cx, async move { session.file_list().await });
        self.count_task = Some(cx.spawn(async move |this, cx| {
            let Ok(Ok(list)) = job.await else {
                return;
            };
            this.update(cx, |this, cx| {
                if let State::Ready(loaded) = &mut this.state
                    && loaded.proposal.tree == tree
                {
                    loaded.out_of_scope = Some(list.out_of_scope_count());
                    cx.notify();
                }
            })
            .ok();
        }));
    }

    /// Follows edits in the worktree: each burst of changes takes one new
    /// snapshot.
    fn start_watching(&mut self, mut watch: argus_runs::WorktreeWatch, cx: &mut Context<Self>) {
        self.watch_task = Some(cx.spawn(async move |this, cx| {
            while watch.changed().await.is_some() {
                cx.background_executor().timer(REFRESH_DEBOUNCE).await;
                watch.drain();
                let alive = this.update(cx, |this, cx| this.refresh(cx)).is_ok();
                if !alive {
                    break;
                }
            }
        }));
    }

    /// Takes a new snapshot of the worktree and shows it if anything moved.
    fn refresh(&mut self, cx: &mut Context<Self>) {
        let State::Ready(loaded) = &self.state else {
            return;
        };
        if self.refresh_task.is_some() {
            // The running refresh is followed by the next change event.
            return;
        }
        let proposal = loaded.proposal.clone();
        let runs = cx.global::<AppContext>().runs.clone();
        let job = spawn_job(cx, async move { runs.refresh_proposal(&proposal).await });
        self.refreshing = true;
        self.refresh_task = Some(cx.spawn(async move |this, cx| {
            let result = job.await;
            this.update(cx, |this, cx| {
                this.refresh_task = None;
                this.refreshing = false;
                match result {
                    Ok(Ok(Some(proposal))) => {
                        let keep = this.diff_view().map(|view| {
                            let view = view.read(cx);
                            (view.viewed_paths(), view.active_path())
                        });
                        this.show(proposal, keep, cx);
                    }
                    Ok(Ok(None)) => {}
                    Ok(Err(error)) => {
                        tracing::warn!(%error, "cannot refresh the proposal");
                        cx.emit(ProposalEvent::Notice {
                            message: format!("Could not refresh the proposal: {error}").into(),
                            is_error: true,
                        });
                    }
                    Err(error) => tracing::warn!(%error, "proposal refresh failed"),
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    fn run_update(&mut self, update: RunUpdate, cx: &mut Context<Self>) {
        let comment_id = self.loaded().map(|loaded| loaded.proposal.comment.id);
        match update {
            RunUpdate::Rejected { run_id, .. } => {
                let ours = self
                    .loaded()
                    .is_some_and(|loaded| loaded.proposal.run.id == run_id);
                if ours {
                    self.state = State::Rejected;
                    self.watch_task = None;
                    cx.emit(ProposalEvent::Rejected(run_id));
                    cx.notify();
                }
            }
            RunUpdate::Started {
                run_id,
                comment_id: Some(id),
            } if Some(id) == comment_id => {
                // A follow-up of this comment edits this worktree.
                self.followup_run = Some(run_id);
                self.session_lost = false;
                self.confirm_reject = false;
                cx.notify();
            }
            RunUpdate::Finished(finished)
                if finished.comment_id.is_some() && finished.comment_id == comment_id =>
            {
                self.followup_run = None;
                self.session_lost = finished.session_not_found;
                if let Some(error) = finished.error.filter(|_| !finished.session_not_found) {
                    let first = error.lines().next().unwrap_or_default().to_owned();
                    self.notice(format!("The follow-up run failed: {first}"), true, cx);
                }
                // Show the latest run's combined proposal (or the earlier one
                // when the follow-up failed); the comment status is fresh too.
                if let Some(id) = comment_id {
                    self.source = ProposalSource::Comment(id);
                    self.load(cx);
                }
                cx.notify();
            }
            _ => {}
        }
    }

    /// Whether a follow-up is running in this proposal's worktree.
    fn followup_running(&self) -> bool {
        self.followup_run.is_some()
            || self
                .loaded()
                .is_some_and(|loaded| loaded.proposal.comment.status == CommentStatus::Running)
    }

    fn loaded(&self) -> Option<&Loaded> {
        match &self.state {
            State::Ready(loaded) => Some(loaded),
            _ => None,
        }
    }

    fn notice(&mut self, message: impl Into<SharedString>, is_error: bool, cx: &mut Context<Self>) {
        cx.emit(ProposalEvent::Notice {
            message: message.into(),
            is_error,
        });
    }

    // ---- actions ------------------------------------------------------------

    fn accept(&mut self, _: &Accept, _: &mut Window, cx: &mut Context<Self>) {
        let Some(run_id) = self.loaded().map(|loaded| loaded.proposal.run.id) else {
            return;
        };
        if self.followup_running() {
            self.notice("Wait for the follow-up run to finish.", false, cx);
            return;
        }
        cx.emit(ProposalEvent::AcceptRequested(run_id));
    }

    /// `x` / Reject: asks first; a second `x` (or the button) confirms.
    fn reject_action(&mut self, _: &RejectProposal, _: &mut Window, cx: &mut Context<Self>) {
        if self.followup_running() {
            self.notice("Wait for the follow-up run to finish.", false, cx);
        } else if self.confirm_reject {
            self.reject(cx);
        } else if self.loaded().is_some() {
            self.confirm_reject = true;
            cx.notify();
        }
    }

    fn reject(&mut self, cx: &mut Context<Self>) {
        let Some(run_id) = self.loaded().map(|loaded| loaded.proposal.run.id) else {
            return;
        };
        if self.rejecting {
            return;
        }
        self.confirm_reject = false;
        self.rejecting = true;
        // Nothing to watch once the worktree is deleted.
        self.watch_task = None;
        let runs = cx.global::<AppContext>().runs.clone();
        // A rejection must not be cut short by the tab closing.
        let handle = cx
            .global::<AppContext>()
            .runtime
            .spawn(async move { runs.reject(run_id).await });
        self.action_task = Some(cx.spawn(async move |this, cx| {
            let result = handle.await;
            this.update(cx, |this, cx| {
                this.rejecting = false;
                let error = match result {
                    Ok(Ok(())) => None,
                    Ok(Err(error)) => Some(error.to_string()),
                    Err(error) => Some(error.to_string()),
                };
                match error {
                    None => {
                        // The run update closes the tab; do it here too in
                        // case the broadcast lagged.
                        if !matches!(this.state, State::Rejected) {
                            this.state = State::Rejected;
                            cx.emit(ProposalEvent::Rejected(run_id));
                        }
                    }
                    Some(message) => {
                        tracing::warn!(%message, "cannot reject the proposal");
                        this.notice(
                            format!("Could not reject the proposal: {message}"),
                            true,
                            cx,
                        );
                        if let State::Ready(loaded) = &this.state {
                            let proposal = loaded.proposal.clone();
                            let runs = cx.global::<AppContext>().runs.clone();
                            let job = spawn_job(cx, async move { runs.watch_proposal(&proposal) });
                            this.action_task = Some(cx.spawn(async move |this, cx| {
                                if let Ok(watch) = job.await {
                                    this.update(cx, |this, cx| this.start_watching(watch, cx))
                                        .ok();
                                }
                            }));
                        }
                    }
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    fn reply_action(&mut self, _: &ReplyToProposal, _: &mut Window, cx: &mut Context<Self>) {
        self.open_reply(cx);
    }

    fn open_reply(&mut self, cx: &mut Context<Self>) {
        if self.loaded().is_none() {
            return;
        }
        let editor = match &self.reply {
            Some(editor) => editor.clone(),
            None => {
                let editor = cx.new(|cx| TextEditor::new("", "Reply to Claude…", cx));
                self._reply_subscription = Some(cx.subscribe(
                    &editor,
                    |this, _, event: &TextEditorEvent, cx| match event {
                        TextEditorEvent::Submit => this.send_reply(cx),
                        TextEditorEvent::Cancel => this.close_reply(cx),
                        TextEditorEvent::Edited => cx.notify(),
                    },
                ));
                self.reply = Some(editor.clone());
                editor
            }
        };
        self.reply_open = true;
        editor.update(cx, |editor, cx| editor.request_focus(cx));
        cx.notify();
    }

    /// Hides the reply box; its text stays for the next time it opens.
    fn close_reply(&mut self, cx: &mut Context<Self>) {
        self.reply_open = false;
        self.focus_requested = true;
        cx.notify();
    }

    /// Sends the reply as a follow-up run (`note_only = false`, the default
    /// of Enter) or saves it as a note in the thread.
    fn send_reply(&mut self, cx: &mut Context<Self>) {
        self.send_reply_as(false, cx);
    }

    fn send_reply_as(&mut self, note_only: bool, cx: &mut Context<Self>) {
        let (Some(editor), Some(loaded)) = (self.reply.clone(), self.loaded()) else {
            return;
        };
        if self.sending_reply {
            return;
        }
        let body = editor.read(cx).text().trim().to_owned();
        if body.is_empty() {
            return;
        }
        let comment_id = loaded.proposal.comment.id;
        let followup = !note_only;
        if followup && self.followup_running() {
            self.notice(
                "A follow-up is already running; add the reply as a note, or wait.",
                false,
                cx,
            );
            return;
        }
        let task: Task<Result<Option<RunId>, String>> = if followup {
            let choice = followup::current(cx);
            let start =
                followup::spawn_followup(comment_id, FollowupKind::Resume(body), choice, cx);
            cx.background_executor()
                .spawn(async move { start.await.map(Some) })
        } else {
            let comments = cx.global::<AppContext>().store.comments();
            let handle = cx.global::<AppContext>().runtime.spawn(async move {
                comments
                    .add_reply(comment_id, ReplyAuthor::User, body)
                    .await
            });
            cx.background_executor().spawn(async move {
                match handle.await {
                    Ok(Ok(_)) => Ok(None),
                    Ok(Err(error)) => Err(error.to_string()),
                    Err(error) => Err(error.to_string()),
                }
            })
        };
        self.sending_reply = true;
        self.action_task = Some(cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| {
                this.sending_reply = false;
                match result {
                    Ok(run) => {
                        if let Some(editor) = &this.reply {
                            editor.update(cx, |editor, cx| editor.set_text("", cx));
                        }
                        this.reply_open = false;
                        this.focus_requested = true;
                        if let Some(run_id) = run {
                            this.followup_run = Some(run_id);
                            this.session_lost = false;
                        }
                        cx.emit(ProposalEvent::Replied {
                            comment_id,
                            followup: run,
                        });
                    }
                    Err(message) if followup => {
                        this.notice(
                            format!("Could not start the follow-up run: {message}"),
                            true,
                            cx,
                        );
                    }
                    Err(message) => {
                        this.notice(format!("Could not save the reply: {message}"), true, cx);
                    }
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    /// Runs the follow-up fresh, with the thread as context, after a resume
    /// found no session.
    fn run_fresh(&mut self, cx: &mut Context<Self>) {
        let Some(comment_id) = self.loaded().map(|loaded| loaded.proposal.comment.id) else {
            return;
        };
        if self.followup_running() || self.sending_reply {
            return;
        }
        let choice = followup::current(cx);
        let start = followup::spawn_followup(comment_id, FollowupKind::Fresh, choice, cx);
        self.sending_reply = true;
        self.action_task = Some(cx.spawn(async move |this, cx| {
            let result = start.await;
            this.update(cx, |this, cx| {
                this.sending_reply = false;
                match result {
                    Ok(run_id) => {
                        this.followup_run = Some(run_id);
                        this.session_lost = false;
                        cx.emit(ProposalEvent::Replied {
                            comment_id,
                            followup: Some(run_id),
                        });
                    }
                    Err(message) => this.notice(
                        format!("Could not start the fresh run: {message}"),
                        true,
                        cx,
                    ),
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    /// Opens `path` (repository-relative) of the run's worktree in
    /// `$VISUAL` / `$EDITOR`, or with the system default application. The
    /// diff follows the saved edits through the worktree watcher.
    fn open_in_editor(&mut self, path: String, cx: &mut Context<Self>) {
        let Some(loaded) = self.loaded() else {
            return;
        };
        let file = loaded.proposal.worktree_file(&path);
        let editor = ["VISUAL", "EDITOR"]
            .into_iter()
            .filter_map(std::env::var_os)
            .map(|value| value.to_string_lossy().trim().to_owned())
            .find(|value| !value.is_empty());
        let Some(editor) = editor else {
            if file.exists() {
                cx.open_with_system(&file);
            } else {
                self.notice(format!("{path} does not exist in the worktree."), true, cx);
            }
            return;
        };
        let job = spawn_job(cx, launch_editor(editor, file.clone()));
        self.action_task = Some(cx.spawn(async move |this, cx| {
            let result = job.await;
            this.update(cx, |this, cx| match result {
                Ok(Ok(())) => {}
                Ok(Err(message)) => {
                    tracing::warn!(%message, "cannot start the editor; using the system default");
                    if file.exists() {
                        cx.open_with_system(&file);
                    }
                    this.notice(format!("Could not start $EDITOR: {message}"), true, cx);
                }
                Err(error) => tracing::warn!(%error, "editor launch task failed"),
            })
            .ok();
        }));
    }

    /// Opens the commented file, or the first changed file when the
    /// commented one is gone.
    fn open_commented_file(&mut self, cx: &mut Context<Self>) {
        let Some(loaded) = self.loaded() else {
            return;
        };
        let path = loaded.proposal.comment.anchor.file_path.clone();
        let fallback = loaded
            .view
            .read(cx)
            .active_path()
            .filter(|_| !loaded.proposal.worktree_file(&path).exists());
        self.open_in_editor(fallback.unwrap_or(path), cx);
    }

    // ---- rendering ----------------------------------------------------------

    fn render_header(&self, loaded: &Loaded, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let colors = &theme.colors;
        let proposal = &loaded.proposal;
        let comment = &proposal.comment;
        let run = &proposal.run;
        let comment_id = comment.id;
        let anchor = &comment.anchor;
        let lines = if anchor.end_line > anchor.start_line {
            format!("lines {}–{}", anchor.start_line, anchor.end_line)
        } else {
            format!("line {}", anchor.start_line)
        };
        let side = match anchor.side {
            DiffSide::Old => "old side",
            DiffSide::New => "new side",
        };
        let location = div()
            .id("proposal-location")
            .flex_none()
            .max_w_full()
            .overflow_hidden()
            .text_ellipsis()
            .whitespace_nowrap()
            .cursor_pointer()
            .font_family(theme.fonts.code_family.clone())
            .text_xs()
            .text_color(colors.accent)
            .hover(|style| style.underline())
            .on_click(cx.listener(move |_, _, _, cx| {
                cx.emit(ProposalEvent::RevealComment(comment_id));
            }))
            .child(format!("{}  {lines} · {side}  ↗", anchor.file_path));

        let mut meta = vec![run.model.clone()];
        if !run.effort.trim().is_empty() {
            meta.push(run.effort.clone());
        }
        if let Some(turns) = run.num_turns {
            meta.push(format!("{turns} turn{}", if turns == 1 { "" } else { "s" }));
        }
        if let Some(cost) = run.cost_usd {
            meta.push(format_cost(cost));
        }
        if let Some(ms) = run.duration_ms {
            meta.push(format_duration(ms));
        }
        let status_note = (run.status == RunStatus::Stopped).then(|| {
            div()
                .flex_none()
                .text_color(colors.warning)
                .child("Stopped: partial change")
        });
        let meta_row = div()
            .flex()
            .items_center()
            .gap_2()
            .text_xs()
            .text_color(colors.text_muted)
            .child(meta.join(" · "))
            .children(status_note);

        let banner = |id: &'static str, text: String, tint: gpui::Hsla| {
            div()
                .id(id)
                .px_2()
                .py_1()
                .rounded_md()
                .border_1()
                .border_color(tint.opacity(0.4))
                .bg(tint.opacity(0.1))
                .text_xs()
                .text_color(tint)
                .child(text)
        };
        let moved = proposal.head_moved().then(|| {
            banner(
                "proposal-head-moved",
                "The head branch has moved since this run started; Accept will try to apply on \
                 top."
                    .to_owned(),
                colors.accent,
            )
        });
        let scope = loaded.out_of_scope.filter(|&count| count > 0).map(|count| {
            banner(
                "proposal-scope",
                format!(
                    "{count} file{} outside the comment's scope (listed first). Claude should say \
                     why in its summary.",
                    if count == 1 { " is" } else { "s are" }
                ),
                colors.warning,
            )
        });

        let label = |text: &'static str| {
            div()
                .text_xs()
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(colors.text_muted)
                .child(text)
        };
        let body = match &self.body_doc {
            Some(doc) => markdown::render(doc, "proposal-comment".into(), cx),
            None => div().child(comment.body_md.clone()).into_any_element(),
        };
        let summary = match (&self.summary_doc, &run.summary) {
            (Some(doc), _) => markdown::render(doc, "proposal-summary".into(), cx),
            (None, Some(text)) => div().child(text.clone()).into_any_element(),
            (None, None) => div()
                .text_color(colors.text_placeholder)
                .child("Claude left no summary.")
                .into_any_element(),
        };

        div()
            .id("proposal-header")
            .flex()
            .flex_col()
            .flex_none()
            .gap_2()
            .max_h(px(320.0))
            .overflow_y_scroll()
            .px_3()
            .py_2()
            .border_b_1()
            .border_color(colors.border_variant)
            .bg(colors.surface)
            .text_sm()
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .min_w_0()
                            .child(label("COMMENT"))
                            .child(location),
                    )
                    .child(body),
            )
            .child(meta_row)
            .children(moved)
            .children(scope)
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(label("CLAUDE'S SUMMARY"))
                    .child(summary),
            )
            .into_any_element()
    }

    fn render_actions(&self, cx: &mut Context<Self>) -> AnyElement {
        let colors = cx.theme().colors.clone();
        let running = self.followup_running();
        let busy = self.rejecting || self.load_task.is_some() || running;
        let reject = if self.rejecting {
            disabled_button("proposal-reject", "Rejecting…", cx).into_any_element()
        } else if running {
            disabled_button("proposal-reject", "Reject", cx).into_any_element()
        } else {
            button("proposal-reject", "Reject", cx)
                .text_color(colors.error)
                .on_click(cx.listener(|this, _, _, cx| {
                    this.confirm_reject = true;
                    cx.notify();
                }))
                .child(kbd(shortcut_keys::<RejectProposal>(), cx))
                .into_any_element()
        };
        let accept = if busy {
            disabled_button("proposal-accept", "Accept", cx).into_any_element()
        } else {
            primary_button("proposal-accept", "Accept", cx)
                .h(px(24.0))
                .gap_1()
                .on_click(|_, window, cx| window.dispatch_action(Box::new(Accept), cx))
                .child(kbd(shortcut_keys::<Accept>(), cx))
                .into_any_element()
        };
        let reply = button("proposal-reply", "Reply", cx)
            .on_click(cx.listener(|this, _, _, cx| this.open_reply(cx)))
            .child(kbd(shortcut_keys::<ReplyToProposal>(), cx));
        let editor = button("proposal-open-editor", "Open in editor", cx)
            .on_click(cx.listener(|this, _, _, cx| this.open_commented_file(cx)));
        let refresh = if self.refreshing {
            disabled_button("proposal-refresh", "Refreshing…", cx).into_any_element()
        } else {
            button("proposal-refresh", "Refresh", cx)
                .text_color(colors.text_muted)
                .on_click(cx.listener(|this, _, _, cx| this.refresh(cx)))
                .into_any_element()
        };
        let confirm = self.confirm_reject.then(|| {
            div()
                .flex()
                .items_center()
                .gap_2()
                .px_3()
                .py_1()
                .bg(colors.error.opacity(0.1))
                .border_b_1()
                .border_color(colors.error.opacity(0.4))
                .text_sm()
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .text_color(colors.text)
                        .child(format!(
                            "Reject this proposal? Its worktree is deleted and the comment goes \
                             back to Open. Press {} again to confirm.",
                            shortcut_keys::<RejectProposal>()
                        )),
                )
                .child(
                    button("proposal-reject-cancel", "Cancel", cx).on_click(cx.listener(
                        |this, _, _, cx| {
                            this.confirm_reject = false;
                            cx.notify();
                        },
                    )),
                )
                .child(
                    button("proposal-reject-confirm", "Reject", cx)
                        .text_color(colors.error)
                        .on_click(cx.listener(|this, _, _, cx| this.reject(cx))),
                )
        });
        let followup_banner = if running {
            Some(
                div()
                    .px_3()
                    .py_1()
                    .border_b_1()
                    .border_color(colors.accent.opacity(0.4))
                    .bg(colors.accent.opacity(0.08))
                    .text_sm()
                    .text_color(colors.accent)
                    .child(
                        "Follow-up running: Claude resumes the session in this worktree; the \
                         diff follows its edits. Accept and Reject wait until it ends.",
                    )
                    .into_any_element(),
            )
        } else if self.session_lost {
            let fresh = if self.sending_reply {
                disabled_button("proposal-run-fresh", "Starting…", cx).into_any_element()
            } else {
                primary_button(
                    "proposal-run-fresh",
                    "Run fresh with the thread as context",
                    cx,
                )
                .h(px(24.0))
                .on_click(cx.listener(|this, _, _, cx| this.run_fresh(cx)))
                .into_any_element()
            };
            Some(
                div()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap_2()
                    .px_3()
                    .py_1()
                    .border_b_1()
                    .border_color(colors.error.opacity(0.4))
                    .bg(colors.error.opacity(0.08))
                    .text_sm()
                    .child(div().flex_1().min_w_0().text_color(colors.error).child(
                        "The follow-up failed: Claude could not resume this comment's \
                                 session (session not found). A fresh run starts a new session \
                                 whose prompt carries the comment and every reply.",
                    ))
                    .child(fresh)
                    .into_any_element(),
            )
        } else {
            None
        };
        let reply_box = self
            .reply
            .clone()
            .filter(|_| self.reply_open)
            .map(|editor| {
                let send = if self.sending_reply {
                    disabled_button("proposal-reply-send", "Sending…", cx).into_any_element()
                } else if running {
                    disabled_button("proposal-reply-send", "Send as follow-up run", cx)
                        .into_any_element()
                } else {
                    primary_button("proposal-reply-send", "Send as follow-up run", cx)
                        .h(px(24.0))
                        .on_click(cx.listener(|this, _, _, cx| this.send_reply(cx)))
                        .into_any_element()
                };
                let note = if self.sending_reply {
                    None
                } else {
                    Some(
                        button("proposal-reply-note", "Add note only", cx)
                            .on_click(cx.listener(|this, _, _, cx| this.send_reply_as(true, cx))),
                    )
                };
                div()
                    .flex()
                    .flex_col()
                    .gap_1p5()
                    .px_3()
                    .py_2()
                    .border_b_1()
                    .border_color(colors.border_variant)
                    .child(editor)
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .justify_end()
                            .gap_2()
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .overflow_hidden()
                                    .text_ellipsis()
                                    .whitespace_nowrap()
                                    .text_xs()
                                    .text_color(colors.text_placeholder)
                                    .child(
                                        "A follow-up resumes the same Claude session in this \
                                         worktree · Esc to close",
                                    ),
                            )
                            .child(followup::choice_buttons("proposal-reply", cx))
                            .child(
                                button("proposal-reply-cancel", "Cancel", cx)
                                    .on_click(cx.listener(|this, _, _, cx| this.close_reply(cx))),
                            )
                            .children(note)
                            .child(send),
                    )
            });
        div()
            .flex()
            .flex_col()
            .flex_none()
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap_1()
                    .px_2()
                    .py_1()
                    .border_b_1()
                    .border_color(colors.border_variant)
                    .child(accept)
                    .child(reject)
                    .child(reply)
                    .child(div().flex_1())
                    .child(editor)
                    .child(refresh),
            )
            .children(followup_banner)
            .children(confirm)
            .children(reply_box)
            .into_any_element()
    }
}

impl Render for ProposalView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if std::mem::take(&mut self.focus_requested) {
            window.focus(&self.focus_handle, cx);
        }
        let colors = cx.theme().colors.clone();
        let root = div()
            .id("proposal")
            .key_context(PROPOSAL_CONTEXT)
            .track_focus(&self.focus_handle)
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .min_w_0()
            .on_action(cx.listener(Self::accept))
            .on_action(cx.listener(Self::reject_action))
            .on_action(cx.listener(Self::reply_action))
            .on_action(cx.listener(|this, _: &crate::keymap::Dismiss, _, cx| {
                if this.confirm_reject {
                    this.confirm_reject = false;
                    cx.notify();
                } else {
                    cx.propagate();
                }
            }));
        let content: AnyElement = match &self.state {
            State::Loading => {
                placeholder("Snapshotting the run's worktree…", cx).into_any_element()
            }
            State::Failed(message) => div()
                .m_3()
                .px_3()
                .py_2()
                .rounded_md()
                .border_1()
                .border_color(colors.error.opacity(0.6))
                .bg(colors.error.opacity(0.08))
                .text_sm()
                .text_color(colors.error)
                .child(format!("Could not open the proposal: {message}"))
                .into_any_element(),
            State::Rejected => placeholder("This proposal was rejected.", cx).into_any_element(),
            State::Ready(loaded) => {
                let view = loaded.view.clone();
                let header = self.render_header(loaded, cx);
                let actions = self.render_actions(cx);
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_h_0()
                    .child(header)
                    .child(actions)
                    .child(view)
                    .into_any_element()
            }
        };
        root.child(content)
    }
}

/// Starts `editor` (a `$EDITOR`-style command line, possibly with
/// arguments) on `file` and reaps it in the background.
async fn launch_editor(editor: String, file: PathBuf) -> Result<(), String> {
    let mut command = if cfg!(unix) {
        let mut command = tokio::process::Command::new("sh");
        command
            .arg("-c")
            .arg(format!("{editor} \"$1\""))
            .arg("sh")
            .arg(&file);
        command
    } else {
        let mut command = tokio::process::Command::new(&editor);
        command.arg(&file);
        command
    };
    command
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    let mut child = command.spawn().map_err(|error| error.to_string())?;
    tokio::spawn(async move {
        match child.wait().await {
            Ok(status) if !status.success() => {
                tracing::warn!(%status, "the editor exited with an error");
            }
            Ok(_) => {}
            Err(error) => tracing::warn!(%error, "cannot wait for the editor"),
        }
    });
    Ok(())
}

/// `$0.0123`, or `$1.23` from a dollar up.
fn format_cost(cost: f64) -> String {
    if cost >= 1.0 {
        format!("${cost:.2}")
    } else {
        format!("${cost:.4}")
    }
}

/// `850 ms`, `42 s`, `3 m 12 s`, `1 h 4 m`.
fn format_duration(ms: i64) -> String {
    let ms = ms.max(0);
    if ms < 1000 {
        return format!("{ms} ms");
    }
    let seconds = ms / 1000;
    match seconds {
        0..60 => format!("{seconds} s"),
        60..3600 => format!("{} m {} s", seconds / 60, seconds % 60),
        _ => format!("{} h {} m", seconds / 3600, (seconds % 3600) / 60),
    }
}

/// The formatted keys bound to action `A`.
fn shortcut_keys<A: gpui::Action>() -> String {
    SHORTCUTS
        .iter()
        .find(|shortcut| shortcut.binding().action().as_any().is::<A>())
        .map(|shortcut| shortcut.display_keys())
        .unwrap_or_default()
}
