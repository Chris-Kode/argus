//! [`DiffSession`]: one three-dot diff with fixed commits, its file list,
//! the streamed +/- counts and the lazily computed per-file diffs.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use argus_git::objects::{self, ThreeDot};
use argus_git::{CancelToken, Repo, Sha};
use tokio::sync::{OnceCell, mpsc};

use crate::cache::DiffCache;
use crate::error::{DiffError, Result};
use crate::file_diff::{self, DiffContent, FileDiff, LoadMode};
use crate::model::{FileEntry, FileStats};
use crate::options::DiffOptions;
use crate::renames;

/// Stats are flushed to subscribers at least this often while computing...
const STATS_FLUSH_INTERVAL: Duration = Duration::from_millis(16);
/// ...or as soon as a worker has this many ready.
const STATS_FLUSH_BATCH: usize = 256;

/// The changed files of a session, sorted by path. A
/// [scoped](DiffSession::scoped) session lists the files outside its scope
/// first, each group by path.
#[derive(Debug)]
pub struct FileList {
    entries: Vec<FileEntry>,
    by_path: HashMap<String, usize>,
    renames_limited: bool,
    /// By index; empty when the session has no scope.
    out_of_scope: Vec<bool>,
}

impl FileList {
    /// Every changed file; `entries()[i].index == i`.
    pub fn entries(&self) -> &[FileEntry] {
        &self.entries
    }

    /// Number of changed files.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// True when base and head have the same tree.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Index of the entry whose path (new path, or old path of a rename) is
    /// `path`.
    pub fn index_of(&self, path: &str) -> Option<usize> {
        self.by_path.get(path).copied().or_else(|| {
            self.entries
                .iter()
                .position(|entry| entry.old_path.as_deref() == Some(path))
        })
    }

    /// Index of the entry whose new path (the merge-base path for
    /// deletions) is exactly `path`; renames' old paths are not matched.
    pub fn index_of_path(&self, path: &str) -> Option<usize> {
        self.by_path.get(path).copied()
    }

    /// The entry for `path` (see [`index_of`](Self::index_of)).
    pub fn get(&self, path: &str) -> Option<&FileEntry> {
        self.index_of(path).map(|index| &self.entries[index])
    }

    /// True when inexact rename detection was skipped because the number of
    /// candidate pairs exceeded the cap; renames then show as add + delete.
    pub fn renames_limited(&self) -> bool {
        self.renames_limited
    }

    /// Whether the entry at `index` is outside the session's scope (neither
    /// its path nor its old path is in it). Always `false` without a scope.
    pub fn is_out_of_scope(&self, index: usize) -> bool {
        self.out_of_scope.get(index).copied().unwrap_or(false)
    }

    /// How many entries are outside the session's scope.
    pub fn out_of_scope_count(&self) -> usize {
        self.out_of_scope.iter().filter(|&&out| out).count()
    }
}

/// An update of the +/- counts stream (see [`DiffSession::stats_updates`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StatsEvent {
    /// Counts for some entries, by file-list index.
    Batch(Vec<(usize, FileStats)>),
    /// Every entry has counts; no more batches follow (except for a later
    /// "Load anyway" of a large file).
    Finished,
}

/// One diff between two fixed commits.
///
/// Cheap to clone; clones share the file list, counts and cache. When the
/// last clone is dropped, every background job of the session is
/// cancelled. A refresh (new commits on either branch) means creating a new
/// session.
#[derive(Clone)]
pub struct DiffSession {
    inner: Arc<Inner>,
}

struct Inner {
    repo: Repo,
    refs: Option<(String, String)>,
    /// Paths in scope; the other files are listed first.
    scope: Option<Arc<HashSet<String>>>,
    old: Sha,
    new: Sha,
    options: DiffOptions,
    cancel: CancelToken,
    list: OnceCell<Arc<FileList>>,
    stats: Arc<StatsHub>,
    cache: Mutex<DiffCache>,
}

impl Drop for Inner {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

impl std::fmt::Debug for DiffSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DiffSession")
            .field("refs", &self.inner.refs)
            .field("old", &self.inner.old)
            .field("new", &self.inner.new)
            .finish_non_exhaustive()
    }
}

impl DiffSession {
    /// Opens the three-dot diff `base_ref...head_ref`: resolves the head SHA
    /// and the merge-base once and keeps them for the whole session.
    pub async fn new(
        repo: &Repo,
        base_ref: &str,
        head_ref: &str,
        options: DiffOptions,
    ) -> Result<Self> {
        let ThreeDot {
            merge_base, head, ..
        } = objects::three_dot(repo, base_ref, head_ref).await?;
        tracing::debug!(base_ref, head_ref, %merge_base, %head, "opened diff session");
        Ok(Self::build(
            repo,
            Some((base_ref.to_owned(), head_ref.to_owned())),
            None,
            merge_base,
            head,
            options,
        ))
    }

    /// A two-dot diff between two known commits (e.g. a run's start commit
    /// and its result).
    pub fn between(repo: &Repo, old: Sha, new: Sha, options: DiffOptions) -> Self {
        Self::build(repo, None, None, old, new, options)
    }

    /// A two-dot diff between two commits or trees (e.g. a run's start
    /// commit and the snapshot of its worktree) that lists the files outside
    /// `scope` first, so they are noticed (spec §14). A file is in scope when
    /// its path or its old path is.
    pub fn scoped(
        repo: &Repo,
        old: Sha,
        new: Sha,
        options: DiffOptions,
        scope: impl IntoIterator<Item = String>,
    ) -> Self {
        let scope = Arc::new(scope.into_iter().collect());
        Self::build(repo, None, Some(scope), old, new, options)
    }

    fn build(
        repo: &Repo,
        refs: Option<(String, String)>,
        scope: Option<Arc<HashSet<String>>>,
        old: Sha,
        new: Sha,
        options: DiffOptions,
    ) -> Self {
        let cache = DiffCache::new(options.cache_bytes);
        Self {
            inner: Arc::new(Inner {
                repo: repo.clone(),
                refs,
                scope,
                old,
                new,
                options,
                cancel: CancelToken::new(),
                list: OnceCell::new(),
                stats: Arc::new(StatsHub::default()),
                cache: Mutex::new(cache),
            }),
        }
    }

    /// The repository.
    pub fn repo(&self) -> &Repo {
        &self.inner.repo
    }

    /// `(base_ref, head_ref)` as given to [`new`](Self::new); `None` for
    /// [`between`](Self::between).
    pub fn refs(&self) -> Option<(&str, &str)> {
        self.inner
            .refs
            .as_ref()
            .map(|(base, head)| (base.as_str(), head.as_str()))
    }

    /// Left side of the diff: the merge-base.
    pub fn merge_base(&self) -> Sha {
        self.inner.old
    }

    /// Right side of the diff: the head commit at session creation.
    pub fn head(&self) -> Sha {
        self.inner.new
    }

    /// The session settings.
    pub fn options(&self) -> &DiffOptions {
        &self.inner.options
    }

    /// A token cancelled with the session, for jobs run on its behalf.
    pub(crate) fn cancel_token(&self) -> CancelToken {
        self.inner.cancel.child()
    }

    /// Cancels every running and future job of this session.
    pub fn cancel(&self) {
        self.inner.cancel.cancel();
    }

    /// The changed files (tree-to-tree diff of merge-base vs head, with
    /// rename detection). Computed once on a blocking thread; later calls
    /// return the same list.
    ///
    /// The first successful call also starts the background pool that
    /// computes the +/- counts; follow them with
    /// [`stats_updates`](Self::stats_updates).
    pub async fn file_list(&self) -> Result<Arc<FileList>> {
        let inner = &self.inner;
        let list = inner
            .list
            .get_or_try_init(|| async {
                let repo = inner.repo.clone();
                let (old, new) = (inner.old, inner.new);
                let renames = inner.options.renames;
                let workers = inner.options.worker_count();
                let scope = inner.scope.clone();
                let guard = inner.cancel.child().drop_guard();
                let token = guard.token().clone();
                let started = Instant::now();
                let list = run_blocking(move || {
                    let changes = objects::tree_diff_blocking(&repo, old, new, &token)?;
                    token.check()?;
                    let detected = renames::detect(&repo, changes, renames, workers, &token)?;
                    Ok(build_list(detected, scope.as_deref()))
                })
                .await?;
                tracing::debug!(
                    files = list.len(),
                    renames_limited = list.renames_limited,
                    elapsed_ms = started.elapsed().as_millis() as u64,
                    "computed file list"
                );
                let list = Arc::new(list);
                spawn_stats_pool(
                    inner.repo.clone(),
                    list.clone(),
                    inner.stats.clone(),
                    inner.options.clone(),
                    inner.cancel.child(),
                );
                Ok::<_, DiffError>(list)
            })
            .await?;
        Ok(list.clone())
    }

    /// Subscribes to the +/- counts. The receiver first gets one batch with
    /// every count already known, then new batches as workers produce them,
    /// then [`StatsEvent::Finished`]. Entries without counts yet are shown
    /// as "…" by the UI.
    pub fn stats_updates(&self) -> mpsc::UnboundedReceiver<StatsEvent> {
        self.inner.stats.subscribe()
    }

    /// Counts of the entry at `index`, if already computed.
    pub fn stats(&self, index: usize) -> Option<FileStats> {
        self.inner.stats.get(index)
    }

    /// The diff of `path` (new path, or the old path of a rename), computed
    /// on a blocking thread on first use and then served from the LRU cache.
    ///
    /// Dropping the future cancels the job if it has not started yet.
    pub async fn file_diff(&self, path: &str, mode: LoadMode) -> Result<Arc<FileDiff>> {
        let list = self.file_list().await?;
        let index = list.index_of(path).ok_or_else(|| DiffError::FileNotFound {
            path: path.to_owned(),
        })?;
        self.file_diff_at(index, mode).await
    }

    /// Same as [`file_diff`](Self::file_diff), by file-list index.
    pub async fn file_diff_at(&self, index: usize, mode: LoadMode) -> Result<Arc<FileDiff>> {
        let list = self.file_list().await?;
        let entry = list
            .entries
            .get(index)
            .cloned()
            .ok_or_else(|| DiffError::FileNotFound {
                path: format!("#{index}"),
            })?;
        if let Some(hit) = self.lock_cache().get(index) {
            return Ok(hit);
        }

        let guard = self.inner.cancel.child().drop_guard();
        let token = guard.token().clone();
        let repo = self.inner.repo.clone();
        let options = self.inner.options.clone();
        let diff = run_blocking(move || {
            token.check()?;
            let reader = repo.object_reader();
            file_diff::compute(&reader, &entry, &options, mode)
        })
        .await?;
        let diff = Arc::new(diff);

        if !matches!(diff.content, DiffContent::TooLarge { .. }) {
            self.lock_cache().insert(index, diff.clone());
        }
        if let (LoadMode::Force, Some(stats)) = (mode, diff.stats()) {
            // A forced load fills in counts the list had to skip.
            self.inner.stats.update(index, stats);
        }
        Ok(diff)
    }

    fn lock_cache(&self) -> std::sync::MutexGuard<'_, DiffCache> {
        self.inner
            .cache
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }
}

/// Runs `work` on tokio's blocking pool.
pub(crate) async fn run_blocking<T, F>(work: F) -> Result<T>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T> + Send + 'static,
{
    match tokio::task::spawn_blocking(work).await {
        Ok(result) => result,
        Err(err) if err.is_cancelled() => Err(DiffError::Cancelled),
        Err(err) => Err(DiffError::Task(err.to_string())),
    }
}

fn build_list(detected: renames::Detected, scope: Option<&HashSet<String>>) -> FileList {
    let in_scope = |paired: &renames::Paired| {
        scope.is_none_or(|scope| {
            scope.contains(&paired.change.path)
                || paired
                    .old_path
                    .as_ref()
                    .is_some_and(|old| scope.contains(old))
        })
    };
    let mut changes = detected.changes;
    // Renames moved to their new path; restore path order, with the files
    // outside the scope first (`false` sorts before `true`).
    changes.sort_by(|a, b| (in_scope(a), &a.change.path).cmp(&(in_scope(b), &b.change.path)));
    let out_of_scope = match scope {
        Some(_) => changes.iter().map(|paired| !in_scope(paired)).collect(),
        None => Vec::new(),
    };
    let entries: Vec<FileEntry> = changes
        .into_iter()
        .enumerate()
        .map(|(index, paired)| FileEntry::from_paired(index, paired))
        .collect();
    let by_path = entries
        .iter()
        .map(|entry| (entry.path.clone(), entry.index))
        .collect();
    FileList {
        entries,
        by_path,
        renames_limited: detected.limited,
        out_of_scope,
    }
}

/// Shared state between the stats workers and their subscribers.
#[derive(Default)]
struct StatsHub {
    state: Mutex<HubState>,
}

#[derive(Default)]
struct HubState {
    results: Vec<Option<FileStats>>,
    completed: usize,
    finished: bool,
    subscribers: Vec<mpsc::UnboundedSender<StatsEvent>>,
}

impl StatsHub {
    fn lock(&self) -> std::sync::MutexGuard<'_, HubState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn start(&self, total: usize) {
        let mut state = self.lock();
        state.results = vec![None; total];
        if total == 0 {
            state.finish();
        }
    }

    fn subscribe(&self) -> mpsc::UnboundedReceiver<StatsEvent> {
        let (tx, rx) = mpsc::unbounded_channel();
        let mut state = self.lock();
        let known: Vec<_> = state
            .results
            .iter()
            .enumerate()
            .filter_map(|(index, stats)| stats.clone().map(|stats| (index, stats)))
            .collect();
        if !known.is_empty() {
            let _ = tx.send(StatsEvent::Batch(known));
        }
        if state.finished {
            let _ = tx.send(StatsEvent::Finished);
        } else {
            state.subscribers.push(tx);
        }
        rx
    }

    fn get(&self, index: usize) -> Option<FileStats> {
        self.lock().results.get(index).cloned().flatten()
    }

    /// Records a batch from a worker and forwards it.
    fn publish(&self, batch: Vec<(usize, FileStats)>) {
        if batch.is_empty() {
            return;
        }
        let mut state = self.lock();
        for (index, stats) in &batch {
            if let Some(slot) = state.results.get_mut(*index)
                && slot.replace(stats.clone()).is_none()
            {
                state.completed += 1;
            }
        }
        let event = StatsEvent::Batch(batch);
        state
            .subscribers
            .retain(|subscriber| subscriber.send(event.clone()).is_ok());
        if state.completed == state.results.len() {
            state.finish();
        }
    }

    /// Replaces one entry's counts after the pool finished (Load anyway).
    fn update(&self, index: usize, stats: FileStats) {
        let mut state = self.lock();
        let Some(slot) = state.results.get_mut(index) else {
            return;
        };
        if slot.as_ref() == Some(&stats) {
            return;
        }
        let was_empty = slot.replace(stats.clone()).is_none();
        if was_empty {
            state.completed += 1;
        }
        let event = StatsEvent::Batch(vec![(index, stats)]);
        state
            .subscribers
            .retain(|subscriber| subscriber.send(event.clone()).is_ok());
        if was_empty && state.completed == state.results.len() {
            state.finish();
        }
    }
}

impl HubState {
    fn finish(&mut self) {
        if self.finished {
            return;
        }
        self.finished = true;
        for subscriber in self.subscribers.drain(..) {
            let _ = subscriber.send(StatsEvent::Finished);
        }
    }
}

/// Starts the worker threads that compute every entry's +/- counts in
/// file-list order. They stop when `cancel` fires (session dropped).
fn spawn_stats_pool(
    repo: Repo,
    list: Arc<FileList>,
    hub: Arc<StatsHub>,
    options: DiffOptions,
    cancel: CancelToken,
) {
    hub.start(list.len());
    if list.is_empty() {
        return;
    }
    let next = Arc::new(AtomicUsize::new(0));
    let options = Arc::new(options);
    let workers = options.worker_count().min(list.len());
    for worker in 0..workers {
        let (repo, list, hub, options, cancel, next) = (
            repo.clone(),
            list.clone(),
            hub.clone(),
            options.clone(),
            cancel.clone(),
            next.clone(),
        );
        let spawned = std::thread::Builder::new()
            .name(format!("argus-diff-stats-{worker}"))
            .spawn(move || stats_worker(&repo, &list, &hub, &options, &cancel, &next));
        if let Err(err) = spawned {
            tracing::warn!(%err, "could not start a diff stats worker");
        }
    }
}

fn stats_worker(
    repo: &Repo,
    list: &FileList,
    hub: &StatsHub,
    options: &DiffOptions,
    cancel: &CancelToken,
    next: &AtomicUsize,
) {
    let reader = repo.object_reader();
    let mut batch = Vec::new();
    let mut flushed = Instant::now();
    loop {
        if cancel.is_cancelled() {
            return;
        }
        let index = next.fetch_add(1, Ordering::Relaxed);
        let Some(entry) = list.entries.get(index) else {
            break;
        };
        let stats = match file_diff::compute_stats(&reader, entry, options) {
            Ok(stats) => stats,
            Err(DiffError::Cancelled) => return,
            Err(err) => {
                tracing::warn!(path = %entry.path, %err, "could not compute diff stats");
                FileStats::Failed(err.to_string())
            }
        };
        batch.push((index, stats));
        if batch.len() >= STATS_FLUSH_BATCH || flushed.elapsed() >= STATS_FLUSH_INTERVAL {
            hub.publish(std::mem::take(&mut batch));
            flushed = Instant::now();
        }
    }
    hub.publish(batch);
}
