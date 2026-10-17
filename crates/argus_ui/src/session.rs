//! The open repository: its branches, the base/head selection, the
//! comparison it resolves to, the upstream counts, Fetch and the background
//! fetch of the head's upstream (spec §3.1, §5.1, §5.4).
//!
//! Every git and storage call runs on the tokio runtime through
//! [`spawn_job`]; this entity only stores results. Each kind of work keeps
//! one task slot, and replacing the slot cancels the previous job, so picking
//! a new base or head cancels the comparison still in flight. The diff
//! viewer (#7) hangs its diff and highlight jobs off
//! [`SessionEvent::TargetChanged`] the same way.

use std::sync::Arc;

use argus_diff::review::ReviewKey;
use argus_git::{Branch, BranchKind, GitError, GitService, Sha};
use gpui::{AppContext as _, Context, Entity, EventEmitter, SharedString, Subscription, Task};

use crate::AppContext;
use crate::comments::{ReviewComments, ReviewCommentsEvent};
use crate::jobs::BackgroundJobs;
use crate::runtime::{spawn_detached, spawn_job};

/// One side of the comparison.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Side {
    /// The branch the diff starts from (`base` in `base...head`).
    Base,
    /// The branch under review; must be local so Accept and Push can move it.
    Head,
}

impl Side {
    /// Lowercase label, as shown in the title bar.
    pub fn label(self) -> &'static str {
        match self {
            Side::Base => "base",
            Side::Head => "head",
        }
    }
}

/// A validated `base...head` pair, ready to be diffed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiffTarget {
    /// Base branch short name.
    pub base: String,
    /// Head branch short name (always a local branch).
    pub head: String,
    /// Base tip when the target was resolved.
    pub base_tip: Sha,
    /// Head tip when the target was resolved.
    pub head_tip: Sha,
    /// Best common ancestor; the diff is `merge_base..head_tip`.
    pub merge_base: Sha,
}

/// What the current selection resolves to.
#[derive(Clone, Debug)]
pub enum Comparison {
    /// The selection is incomplete; the text says what to pick.
    Incomplete(SharedString),
    /// The merge-base is being computed.
    Resolving,
    /// A valid pair.
    Ready(DiffTarget),
    /// The pair cannot be diffed; shown inline instead of a diff.
    Invalid(SharedString),
}

/// The branch list of the repository.
#[derive(Clone, Debug)]
pub enum BranchList {
    /// First load in progress.
    Loading,
    /// Local branches first, then remote-tracking ones, each sorted by name.
    Loaded(Arc<Vec<Branch>>),
    /// Listing failed.
    Failed(SharedString),
}

/// `head` relative to its upstream, for the Push button.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UpstreamStatus {
    /// Upstream short name, e.g. `origin/feature`.
    pub upstream: String,
    /// Commits on head that the upstream lacks.
    pub ahead: u32,
    /// Commits on the upstream that head lacks.
    pub behind: u32,
}

/// The confirmation shown when a remote-only branch is picked as head.
#[derive(Clone, Debug)]
pub struct TrackingPrompt {
    /// The remote-tracking branch the user picked, e.g. `origin/feature`.
    pub remote_branch: String,
    /// The local branch to create (or reuse), e.g. `feature`.
    pub local_name: String,
    /// A local branch of that name exists already; confirming selects it
    /// instead of creating one.
    pub local_exists: bool,
    /// Base to apply together with the new head.
    pub base: Option<String>,
    /// Creating the branch is in progress.
    pub creating: bool,
    /// Why the last attempt failed.
    pub error: Option<SharedString>,
}

/// Events for the workspace.
#[derive(Clone, Debug)]
pub enum SessionEvent {
    /// A message for the status bar.
    Notice {
        /// Text to show.
        message: SharedString,
        /// Error (red) rather than warning.
        is_error: bool,
    },
    /// The resolved diff target changed (or went away); the diff viewer
    /// reloads.
    TargetChanged(Option<DiffTarget>),
}

/// Everything the first load reads from git.
struct Snapshot {
    branches: Vec<Branch>,
    remotes: Vec<String>,
    current: Option<String>,
    remote_default: Option<String>,
}

/// The repository open in the workspace.
pub struct RepoSession {
    record: argus_store::Repo,
    repo: argus_git::Repo,
    remotes: Vec<String>,
    branches: BranchList,
    base: Option<String>,
    head: Option<String>,
    comparison: Comparison,
    upstream: Option<UpstreamStatus>,
    tracking: Option<TrackingPrompt>,
    fetching: bool,
    comments: Entity<ReviewComments>,
    _comments_subscription: Subscription,
    load_task: Option<Task<()>>,
    refresh_task: Option<Task<()>>,
    compare_task: Option<Task<()>>,
    upstream_task: Option<Task<()>>,
    fetch_task: Option<Task<()>>,
    background_fetch_task: Option<Task<()>>,
    tracking_task: Option<Task<()>>,
}

impl EventEmitter<SessionEvent> for RepoSession {}

impl RepoSession {
    /// Starts a session for an opened repository and begins loading its
    /// branches. The base/head selection is restored from `record`, or
    /// defaulted on first open.
    pub fn new(record: argus_store::Repo, repo: argus_git::Repo, cx: &mut Context<Self>) -> Self {
        let comments = cx.new(ReviewComments::new);
        let comments_subscription = cx.subscribe(&comments, |_, _, event, cx| {
            if let ReviewCommentsEvent::Error(message) = event {
                cx.emit(SessionEvent::Notice {
                    message: message.clone(),
                    is_error: true,
                });
            }
        });
        let mut this = Self {
            record,
            repo,
            remotes: Vec::new(),
            branches: BranchList::Loading,
            base: None,
            head: None,
            comparison: Comparison::Incomplete("Loading branches…".into()),
            upstream: None,
            tracking: None,
            fetching: false,
            comments,
            _comments_subscription: comments_subscription,
            load_task: None,
            refresh_task: None,
            compare_task: None,
            upstream_task: None,
            fetch_task: None,
            background_fetch_task: None,
            tracking_task: None,
        };
        this.load(cx);
        this
    }

    // ---- accessors ----------------------------------------------------------

    /// The stored repository record.
    pub fn record(&self) -> &argus_store::Repo {
        &self.record
    }

    /// The comments of the current (repo, base, head) review (spec Q2):
    /// loaded whenever the comparison resolves, cleared when it is invalid.
    pub fn comments(&self) -> &Entity<ReviewComments> {
        &self.comments
    }

    /// The git repository handle.
    pub fn repo(&self) -> &argus_git::Repo {
        &self.repo
    }

    /// The branch list.
    pub fn branches(&self) -> &BranchList {
        &self.branches
    }

    /// The selected branch of `side`.
    pub fn selected(&self, side: Side) -> Option<&str> {
        match side {
            Side::Base => self.base.as_deref(),
            Side::Head => self.head.as_deref(),
        }
    }

    /// What the selection resolves to.
    pub fn comparison(&self) -> &Comparison {
        &self.comparison
    }

    /// Head relative to its upstream, when it has one.
    pub fn upstream(&self) -> Option<&UpstreamStatus> {
        self.upstream.as_ref()
    }

    /// The pending tracking-branch confirmation.
    pub fn tracking_prompt(&self) -> Option<&TrackingPrompt> {
        self.tracking.as_ref()
    }

    /// Whether a fetch is running.
    pub fn is_fetching(&self) -> bool {
        self.fetching
    }

    /// Whether the repository has any remote to fetch from.
    pub fn has_remotes(&self) -> bool {
        !self.remotes.is_empty()
    }

    /// The configured remotes, sorted.
    pub fn remotes(&self) -> &[String] {
        &self.remotes
    }

    /// The selected head when it is a local branch.
    pub fn local_head(&self) -> Option<&Branch> {
        self.head
            .as_deref()
            .and_then(|head| self.branch(head))
            .filter(|branch| branch.kind == BranchKind::Local)
    }

    /// Whether the local head has an upstream configured (its remote-tracking
    /// branch may not exist locally yet).
    pub fn head_has_upstream(&self) -> bool {
        self.local_head()
            .is_some_and(|branch| branch.upstream.is_some())
    }

    // ---- loading ------------------------------------------------------------

    fn load(&mut self, cx: &mut Context<Self>) {
        let repo = self.repo.clone();
        let job = spawn_job(cx, async move { read_snapshot(&repo).await });
        self.load_task = Some(cx.spawn(async move |this, cx| {
            let result = flatten(job.await);
            this.update(cx, |this, cx| match result {
                Ok(snapshot) => {
                    let (base, head) = initial_selection(
                        &snapshot,
                        this.record.last_base.as_deref(),
                        this.record.last_head.as_deref(),
                    );
                    this.remotes = snapshot.remotes;
                    this.branches = BranchList::Loaded(Arc::new(snapshot.branches));
                    this.apply_selection(base, head, cx);
                }
                Err(message) => {
                    tracing::warn!(%message, "failed to list branches");
                    this.branches = BranchList::Failed(message.clone().into());
                    this.comparison =
                        Comparison::Invalid(format!("Could not list branches: {message}").into());
                    cx.notify();
                }
            })
            .ok();
        }));
    }

    fn branch(&self, name: &str) -> Option<&Branch> {
        let BranchList::Loaded(branches) = &self.branches else {
            return None;
        };
        // Locals first, so a local branch shadows a same-named remote one.
        branches.iter().find(|branch| branch.name == name)
    }

    // ---- selection ----------------------------------------------------------

    /// The user picked `name` for `side`. A remote-only head opens the
    /// tracking-branch confirmation instead of changing the selection.
    pub fn pick(&mut self, side: Side, name: String, cx: &mut Context<Self>) {
        let (base, head) = match side {
            Side::Base => (Some(name), self.head.clone()),
            Side::Head => (self.base.clone(), Some(name)),
        };
        self.request_selection(base, head, cx);
    }

    /// Swaps base and head (⇄).
    pub fn swap(&mut self, cx: &mut Context<Self>) {
        let (base, head) = (self.head.clone(), self.base.clone());
        self.request_selection(base, head, cx);
    }

    fn request_selection(
        &mut self,
        base: Option<String>,
        head: Option<String>,
        cx: &mut Context<Self>,
    ) {
        if let Some(remote_head) = head
            .as_deref()
            .and_then(|name| self.branch(name))
            .filter(|branch| branch.kind == BranchKind::Remote)
        {
            let remote_branch = remote_head.name.clone();
            let local_name = argus_git::read::split_remote_branch(&self.remotes, &remote_branch)
                .map(|(_, branch)| branch.to_owned())
                .unwrap_or_else(|| remote_branch.clone());
            let local_exists = self
                .branch(&local_name)
                .is_some_and(|branch| branch.kind == BranchKind::Local);
            self.tracking = Some(TrackingPrompt {
                remote_branch,
                local_name,
                local_exists,
                base,
                creating: false,
                error: None,
            });
            cx.notify();
            return;
        }
        self.apply_selection(base, head, cx);
    }

    /// Confirms the tracking-branch prompt: creates the local branch (unless
    /// it exists), reloads the branch list and selects it as head.
    pub fn confirm_tracking(&mut self, cx: &mut Context<Self>) {
        let Some(prompt) = self.tracking.as_mut() else {
            return;
        };
        if prompt.creating {
            return;
        }
        if prompt.local_exists {
            let prompt = self.tracking.take();
            if let Some(prompt) = prompt {
                self.apply_selection(prompt.base, Some(prompt.local_name), cx);
            }
            return;
        }
        prompt.creating = true;
        prompt.error = None;
        let (local_name, remote_branch) = (prompt.local_name.clone(), prompt.remote_branch.clone());
        let repo = self.repo.clone();
        let git = cx.global::<AppContext>().git.clone();
        let job = spawn_job(cx, async move {
            argus_git::write::create_tracking_branch(&repo, &local_name, &remote_branch).await?;
            // The open handle holds a config snapshot without the new
            // branch's upstream; re-open so the Push counts see it.
            let repo = git.reload(&repo).await?;
            let branches = argus_git::read::branches(&repo).await?;
            Ok((repo, branches))
        });
        self.tracking_task = Some(cx.spawn(async move |this, cx| {
            let result = flatten(job.await);
            this.update(cx, |this, cx| match result {
                Ok((repo, branches)) => {
                    this.repo = repo;
                    this.branches = BranchList::Loaded(Arc::new(branches));
                    if let Some(prompt) = this.tracking.take() {
                        cx.emit(SessionEvent::Notice {
                            message: format!(
                                "Created local branch `{}` tracking `{}`.",
                                prompt.local_name, prompt.remote_branch
                            )
                            .into(),
                            is_error: false,
                        });
                        this.apply_selection(prompt.base, Some(prompt.local_name), cx);
                    }
                }
                Err(message) => {
                    tracing::warn!(%message, "failed to create tracking branch");
                    if let Some(prompt) = this.tracking.as_mut() {
                        prompt.creating = false;
                        prompt.error = Some(message.into());
                    }
                    cx.notify();
                }
            })
            .ok();
        }));
        cx.notify();
    }

    /// Cancels the tracking-branch prompt; the selection stays unchanged.
    pub fn cancel_tracking(&mut self, cx: &mut Context<Self>) {
        if self.tracking.take().is_some() {
            self.tracking_task = None;
            cx.notify();
        }
    }

    /// Sets the selection, remembers it for the repository and resolves it.
    fn apply_selection(
        &mut self,
        base: Option<String>,
        head: Option<String>,
        cx: &mut Context<Self>,
    ) {
        let changed = base != self.record.last_base || head != self.record.last_head;
        self.base = base;
        self.head = head;
        if changed {
            self.record.last_base = self.base.clone();
            self.record.last_head = self.head.clone();
            let store = cx.global::<AppContext>().store.repos();
            let (id, base, head) = (self.record.id, self.base.clone(), self.head.clone());
            spawn_detached(cx, async move {
                if let Err(error) = store.set_last_branches(id, base, head).await {
                    tracing::warn!(%error, "failed to remember the branch selection");
                }
            });
        }
        self.resolve(cx);
        self.refresh_upstream(cx);
    }

    /// Validates the selection and computes its merge-base. Replacing the
    /// task cancels a comparison still in flight.
    fn resolve(&mut self, cx: &mut Context<Self>) {
        let previous = match &self.comparison {
            Comparison::Ready(target) => Some(target.clone()),
            _ => None,
        };
        match self
            .validate()
            .map(|(base, head)| (base.clone(), head.clone()))
        {
            Ok((base, head)) => {
                self.comparison = Comparison::Resolving;
                let repo = self.repo.clone();
                let (base_name, head_name) = (base.name.clone(), head.name.clone());
                let (base_tip, head_tip) = (base.tip, head.tip);
                let (base_ref, head_ref) = (base.full_name.clone(), head.full_name.clone());
                let job = spawn_job(cx, async move {
                    argus_git::read::merge_base(&repo, &base_ref, &head_ref).await
                });
                BackgroundJobs::update(cx, |jobs| jobs.diffing = true);
                self.compare_task = Some(cx.spawn(async move |this, cx| {
                    let result = job.await;
                    this.update(cx, |this, cx| {
                        BackgroundJobs::update(cx, |jobs| jobs.diffing = false);
                        this.compare_task = None;
                        this.comparison = match result {
                            Ok(Ok(merge_base)) => Comparison::Ready(DiffTarget {
                                base: base_name,
                                head: head_name,
                                base_tip,
                                head_tip,
                                merge_base,
                            }),
                            Ok(Err(GitError::UnrelatedHistories { .. })) => Comparison::Invalid(
                                format!(
                                    "`{base_name}` and `{head_name}` have unrelated histories: \
                                     there is no merge-base to diff from."
                                )
                                .into(),
                            ),
                            Ok(Err(error)) => Comparison::Invalid(error.to_string().into()),
                            Err(error) => Comparison::Invalid(error.to_string().into()),
                        };
                        let target = match &this.comparison {
                            Comparison::Ready(target) => Some(target.clone()),
                            _ => None,
                        };
                        this.show_comments(target.as_ref(), cx);
                        cx.emit(SessionEvent::TargetChanged(target));
                        cx.notify();
                    })
                    .ok();
                }));
            }
            Err(comparison) => {
                if self.compare_task.take().is_some() {
                    BackgroundJobs::update(cx, |jobs| jobs.diffing = false);
                }
                self.comparison = comparison;
                self.show_comments(None, cx);
            }
        }
        if previous.is_some() {
            cx.emit(SessionEvent::TargetChanged(None));
        }
        cx.notify();
    }

    /// Loads the comments keyed by the target's (repo, base, head).
    fn show_comments(&mut self, target: Option<&DiffTarget>, cx: &mut Context<Self>) {
        let key = target.map(|target| ReviewKey::new(self.record.id, &target.base, &target.head));
        self.comments
            .update(cx, |comments, cx| comments.set_review(key, cx));
    }

    /// Checks the selection without touching git: both sides picked and
    /// known, different, and head local.
    fn validate(&self) -> Result<(&Branch, &Branch), Comparison> {
        if let BranchList::Failed(message) = &self.branches {
            return Err(Comparison::Invalid(message.clone()));
        }
        let (Some(base), Some(head)) = (self.base.as_deref(), self.head.as_deref()) else {
            let missing = match (&self.base, &self.head) {
                (None, None) => "Pick a base and a head branch in the title bar.",
                (None, Some(_)) => "Pick a base branch in the title bar.",
                _ => "Pick a head branch in the title bar to compare against the base.",
            };
            return Err(Comparison::Incomplete(missing.into()));
        };
        if base == head {
            return Err(Comparison::Invalid(
                format!("Base and head are both `{base}`. Pick two different branches.").into(),
            ));
        }
        let missing = |name: &str| {
            Comparison::Invalid(format!("Branch `{name}` does not exist anymore.").into())
        };
        let base_branch = self.branch(base).ok_or_else(|| missing(base))?;
        let head_branch = self.branch(head).ok_or_else(|| missing(head))?;
        if head_branch.kind != BranchKind::Local {
            return Err(Comparison::Invalid(
                format!(
                    "Head `{head}` is a remote-tracking branch. Pick a local branch: Accept and \
                     Push need one."
                )
                .into(),
            ));
        }
        Ok((base_branch, head_branch))
    }

    /// Recomputes head's ahead/behind counts against its upstream.
    fn refresh_upstream(&mut self, cx: &mut Context<Self>) {
        let upstream = self
            .head
            .as_deref()
            .and_then(|head| self.branch(head))
            .filter(|branch| branch.kind == BranchKind::Local)
            .and_then(|head| {
                let upstream = self.branch(head.upstream.as_deref()?)?;
                Some((
                    head.full_name.clone(),
                    upstream.full_name.clone(),
                    upstream.name.clone(),
                ))
            });
        let Some((head_ref, upstream_ref, upstream_name)) = upstream else {
            self.upstream = None;
            self.upstream_task = None;
            cx.notify();
            return;
        };
        let repo = self.repo.clone();
        let job = spawn_job(cx, async move {
            argus_git::read::ahead_behind(&repo, &head_ref, &upstream_ref).await
        });
        self.upstream_task = Some(cx.spawn(async move |this, cx| {
            let result = flatten(job.await);
            this.update(cx, |this, cx| {
                this.upstream = match result {
                    Ok((ahead, behind)) => Some(UpstreamStatus {
                        upstream: upstream_name,
                        ahead,
                        behind,
                    }),
                    Err(message) => {
                        tracing::warn!(%message, "failed to count ahead/behind");
                        None
                    }
                };
                cx.notify();
            })
            .ok();
        }));
    }

    // ---- fetch --------------------------------------------------------------

    /// Fetches every remote, then refreshes the branch list, the upstream
    /// counts, and the comparison if either tip moved.
    pub fn fetch(&mut self, cx: &mut Context<Self>) {
        if self.fetching {
            return;
        }
        if self.remotes.is_empty() {
            cx.emit(SessionEvent::Notice {
                message: "This repository has no remote to fetch from.".into(),
                is_error: false,
            });
            return;
        }
        self.fetching = true;
        BackgroundJobs::update(cx, |jobs| jobs.fetching = true);
        let repo = self.repo.clone();
        let remotes = self.remotes.clone();
        let job = spawn_job(cx, async move {
            let mut errors = Vec::new();
            for remote in &remotes {
                let result = argus_git::read::fetch(&repo, remote, |line| {
                    tracing::debug!(%line, "fetch progress");
                })
                .await;
                if let Err(error) = result {
                    tracing::warn!(%remote, %error, "fetch failed");
                    errors.push(error.to_string());
                }
            }
            let branches = argus_git::read::branches(&repo).await;
            (errors, branches)
        });
        self.fetch_task = Some(cx.spawn(async move |this, cx| {
            let result = job.await;
            this.update(cx, |this, cx| {
                this.fetching = false;
                BackgroundJobs::update(cx, |jobs| jobs.fetching = false);
                let (errors, branches) = match result {
                    Ok(result) => result,
                    Err(error) => (vec![error.to_string()], Err(GitError::Cancelled)),
                };
                match branches {
                    Ok(branches) => this.replace_branches(branches, cx),
                    Err(GitError::Cancelled) => {}
                    Err(error) => cx.emit(SessionEvent::Notice {
                        message: format!("Could not refresh branches: {error}").into(),
                        is_error: true,
                    }),
                }
                let (message, is_error) = if errors.is_empty() {
                    ("Fetched.".to_owned(), false)
                } else {
                    (format!("Fetch failed: {}", errors.join("; ")), true)
                };
                cx.emit(SessionEvent::Notice {
                    message: message.into(),
                    is_error,
                });
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    /// Fetches only the head's upstream branch, quietly (the 60 s background
    /// fetch, spec §5.4), then refreshes the branches and the Push counts.
    /// Skipped while a Fetch runs or when the head has no upstream; failures
    /// are only logged.
    pub fn fetch_head_upstream(&mut self, cx: &mut Context<Self>) {
        if self.fetching || self.background_fetch_task.is_some() || !self.head_has_upstream() {
            return;
        }
        let Some(head) = self.local_head().map(|branch| branch.name.clone()) else {
            return;
        };
        let repo = self.repo.clone();
        let job = spawn_job(cx, async move {
            if let Some(upstream) = argus_git::push::upstream_of(&repo, &head).await? {
                let (log, outcome) =
                    argus_git::push::fetch_upstream(repo.workdir(), &upstream).await?;
                if outcome == argus_git::push::FetchOutcome::Failed {
                    tracing::info!(command = %log.command, output = %log.output, "background fetch failed");
                }
            }
            argus_git::read::branches(&repo).await
        });
        self.background_fetch_task = Some(cx.spawn(async move |this, cx| {
            let result = flatten(job.await);
            this.update(cx, |this, cx| {
                this.background_fetch_task = None;
                match result {
                    Ok(branches) => this.replace_branches(branches, cx),
                    Err(message) => tracing::info!(%message, "background fetch failed"),
                }
                cx.notify();
            })
            .ok();
        }));
    }

    /// Re-opens the repository (so config written by `git push
    /// --set-upstream` is seen) and re-reads the branches, after a Push: the
    /// diff and the comment anchors follow a rebased head, and the Push
    /// counts are recomputed.
    pub fn reload_after_push(&mut self, cx: &mut Context<Self>) {
        let repo = self.repo.clone();
        let git = cx.global::<AppContext>().git.clone();
        let job = spawn_job(cx, async move {
            let repo = git.reload(&repo).await?;
            let branches = argus_git::read::branches(&repo).await?;
            Ok((repo, branches))
        });
        self.refresh_task = Some(cx.spawn(async move |this, cx| {
            let result = flatten(job.await);
            this.update(cx, |this, cx| {
                this.refresh_task = None;
                match result {
                    Ok((repo, branches)) => {
                        this.repo = repo;
                        this.replace_branches(branches, cx);
                    }
                    Err(message) => cx.emit(SessionEvent::Notice {
                        message: format!("Could not refresh branches: {message}").into(),
                        is_error: true,
                    }),
                }
                cx.notify();
            })
            .ok();
        }));
    }

    /// Re-reads the branches after Argus moved one (an Accept): the
    /// comparison re-resolves if a tip moved, which refreshes the diff and
    /// re-anchors the comments, and the Push counts are recomputed.
    pub fn refresh_branches(&mut self, cx: &mut Context<Self>) {
        let repo = self.repo.clone();
        let job = spawn_job(cx, async move { argus_git::read::branches(&repo).await });
        self.refresh_task = Some(cx.spawn(async move |this, cx| {
            let result = flatten(job.await);
            this.update(cx, |this, cx| {
                this.refresh_task = None;
                match result {
                    Ok(branches) => this.replace_branches(branches, cx),
                    Err(message) => cx.emit(SessionEvent::Notice {
                        message: format!("Could not refresh branches: {message}").into(),
                        is_error: true,
                    }),
                }
                cx.notify();
            })
            .ok();
        }));
    }

    /// Installs a fresh branch list and re-resolves only what moved.
    fn replace_branches(&mut self, branches: Vec<Branch>, cx: &mut Context<Self>) {
        let tip =
            |name: Option<&str>, this: &Self| name.and_then(|n| this.branch(n)).map(|b| b.tip);
        let before = (
            tip(self.base.as_deref(), self),
            tip(self.head.as_deref(), self),
        );
        self.branches = BranchList::Loaded(Arc::new(branches));
        let after = (
            tip(self.base.as_deref(), self),
            tip(self.head.as_deref(), self),
        );
        let stale = !matches!(self.comparison, Comparison::Ready(_));
        if before != after || stale {
            self.resolve(cx);
        }
        self.refresh_upstream(cx);
    }

    /// Cancels every job of the session, e.g. before another repository
    /// replaces it.
    pub fn shutdown(&mut self, cx: &mut Context<Self>) {
        if self.compare_task.take().is_some() {
            BackgroundJobs::update(cx, |jobs| jobs.diffing = false);
        }
        if self.fetch_task.take().is_some() {
            BackgroundJobs::update(cx, |jobs| jobs.fetching = false);
        }
        self.load_task = None;
        self.refresh_task = None;
        self.background_fetch_task = None;
        self.upstream_task = None;
        self.tracking_task = None;
        self.show_comments(None, cx);
        cx.global::<AppContext>().git.forget(self.repo.id());
    }
}

/// Reads branches, remotes, the checked-out branch and the remote's default
/// branch.
async fn read_snapshot(repo: &argus_git::Repo) -> argus_git::Result<Snapshot> {
    let branches = argus_git::read::branches(repo).await?;
    let remotes = argus_git::read::remote_names(repo).await?;
    let current = argus_git::read::current_branch(repo).await?;
    let mut remote_default = None;
    // `origin` first, then the others: the first remote with a HEAD wins.
    let mut ordered: Vec<&String> = remotes.iter().filter(|r| *r == "origin").collect();
    ordered.extend(remotes.iter().filter(|r| *r != "origin"));
    for remote in ordered {
        match argus_git::read::remote_default_branch(repo, remote).await {
            Ok(Some(branch)) => {
                remote_default = Some(branch);
                break;
            }
            Ok(None) => {}
            Err(error) => tracing::debug!(%remote, %error, "no remote HEAD"),
        }
    }
    Ok(Snapshot {
        branches,
        remotes,
        current,
        remote_default,
    })
}

/// The selection on open: the remembered pair when its branches still exist,
/// otherwise base = the remote's default branch (or `main` / `master`) and
/// head = the checked-out branch unless it is the base.
fn initial_selection(
    snapshot: &Snapshot,
    last_base: Option<&str>,
    last_head: Option<&str>,
) -> (Option<String>, Option<String>) {
    let find = |name: &str| snapshot.branches.iter().find(|b| b.name == name);
    let exists = |name: &&str| find(name).is_some();

    let base = last_base.filter(exists).map(str::to_owned).or_else(|| {
        let mut candidates: Vec<&str> = Vec::new();
        if let Some(default) = snapshot.remote_default.as_deref() {
            candidates.push(default);
        }
        candidates.extend(["main", "master", "origin/main", "origin/master"]);
        candidates.into_iter().find(exists).map(str::to_owned)
    });

    let head = last_head
        .filter(|name| find(name).is_some_and(|b| b.kind == BranchKind::Local))
        .map(str::to_owned)
        .or_else(|| {
            let current = snapshot.current.as_deref()?;
            find(current).filter(|b| b.kind == BranchKind::Local)?;
            let is_base = base.as_deref().is_some_and(|base| {
                base == current
                    || argus_git::read::split_remote_branch(&snapshot.remotes, base)
                        .is_some_and(|(_, branch)| branch == current)
            });
            (!is_base).then(|| current.to_owned())
        });
    (base, head)
}

/// Collapses a runtime join error and a git error into one message.
fn flatten<T>(result: Result<argus_git::Result<T>, tokio::task::JoinError>) -> Result<T, String> {
    match result {
        Ok(Ok(value)) => Ok(value),
        Ok(Err(error)) => Err(error.to_string()),
        Err(error) => Err(error.to_string()),
    }
}

/// Opens the repository containing `path` and records its working-tree root
/// in `repos`. Runs on the tokio runtime.
pub async fn open_repository(
    git: GitService,
    store: argus_store::StorageService,
    path: std::path::PathBuf,
) -> Result<(argus_store::Repo, argus_git::Repo), String> {
    let repo = git.open(path).await.map_err(|error| error.to_string())?;
    let repos = store.repos();
    let record = async {
        let id = repos.upsert(repo.workdir()).await?;
        repos.touch_last_opened(id).await?;
        repos.get(id).await
    }
    .await
    .map_err(|error| error.to_string())?
    .ok_or_else(|| "the repository record disappeared".to_owned())?;
    Ok((record, repo))
}

/// Unix seconds now, for relative tip times.
pub fn now_seconds() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs() as i64)
        .unwrap_or(0)
}

/// `3 days ago`-style text for a unix time.
pub fn relative_time(time: i64, now: i64) -> String {
    let seconds = (now - time).max(0);
    let (value, unit) = match seconds {
        0..60 => return "just now".to_owned(),
        60..3_600 => (seconds / 60, "minute"),
        3_600..86_400 => (seconds / 3_600, "hour"),
        86_400..2_592_000 => (seconds / 86_400, "day"),
        2_592_000..31_536_000 => (seconds / 2_592_000, "month"),
        _ => (seconds / 31_536_000, "year"),
    };
    let plural = if value == 1 { "" } else { "s" };
    format!("{value} {unit}{plural} ago")
}
