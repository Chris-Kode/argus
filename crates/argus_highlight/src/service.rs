//! The background highlighter: a small thread pool with a priority queue,
//! cancellable jobs and a per-blob result cache.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU8, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::task::{Context, Poll};
use std::thread;

use argus_git::Sha;
use tokio::sync::oneshot;
use tree_sitter_highlight::Highlighter as TsHighlighter;

use crate::error::HighlightError;
use crate::grammars;
use crate::highlights::{self, Highlights};
use crate::language::Language;

/// Blobs larger than this are not highlighted unless forced.
pub const MAX_HIGHLIGHT_BYTES: usize = 2 * 1024 * 1024;
/// Blobs with more lines than this are not highlighted unless forced.
pub const MAX_HIGHLIGHT_LINES: usize = 50_000;
/// Default memory budget of the result cache.
pub const DEFAULT_CACHE_BYTES: usize = 64 * 1024 * 1024;

/// What a cached result is keyed by: one blob parsed as one language.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct HighlightKey {
    /// The blob's object id; equal content shares one result.
    pub blob: Sha,
    /// The language it was parsed as.
    pub language: Language,
}

impl HighlightKey {
    /// A key for text that is not a git blob, such as a fenced code block in
    /// a comment: the "blob" id is a 128-bit hash of the text (zero-padded
    /// to an object id), so equal snippets share one cached result. A
    /// collision with a real blob id is astronomically unlikely, and would
    /// only show wrong colors. `None` only if the padded hash failed to
    /// parse as an object id.
    pub fn for_text(text: &str, language: Language) -> Option<Self> {
        let hash = xxhash_rust::xxh3::xxh3_128(text.as_bytes());
        let blob = Sha::parse(&format!("{hash:032x}00000000"))?;
        Some(Self { blob, language })
    }
}

/// How urgently a job is wanted. Higher runs first; among equals, the
/// oldest request.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub enum Priority {
    /// Near the viewport; wanted before the user scrolls there.
    Prefetch = 0,
    /// On screen now.
    Visible = 1,
}

impl Priority {
    fn from_u8(value: u8) -> Self {
        if value == Priority::Visible as u8 {
            Priority::Visible
        } else {
            Priority::Prefetch
        }
    }
}

/// The result of a highlight job.
#[derive(Clone, Debug)]
pub enum Outcome {
    /// Highlights of every line.
    Highlighted(Arc<Highlights>),
    /// Above [`MAX_HIGHLIGHT_BYTES`] or [`MAX_HIGHLIGHT_LINES`]: shown as
    /// plain text until requested again with `force`.
    TooLarge {
        /// Size of the text in bytes.
        bytes: usize,
        /// Number of lines.
        lines: usize,
    },
}

/// The text of a request. Anything that can lend a `&str` for the duration
/// of the job (e.g. a handle to an already loaded diff), so the blob is not
/// copied.
pub type SourceText = Box<dyn AsRef<str> + Send + Sync>;

/// A highlighting request.
pub struct HighlightRequest {
    /// Cache key.
    pub key: HighlightKey,
    /// The full blob text. Line splitting must match the caller's.
    pub text: SourceText,
    /// Initial priority; see [`JobControl::set_priority`].
    pub priority: Priority,
    /// Highlight even above the size limits (the user forced it).
    pub force: bool,
}

/// State shared by a queued job, its [`HighlightJob`] and its
/// [`JobControl`]s.
struct JobState {
    seq: u64,
    priority: AtomicU8,
    /// Non-zero once cancelled; tree-sitter polls it while parsing.
    cancelled: AtomicUsize,
    work: Mutex<Option<Work>>,
}

struct Work {
    key: HighlightKey,
    text: SourceText,
    force: bool,
    reply: oneshot::Sender<Result<Outcome, HighlightError>>,
}

impl JobState {
    fn priority(&self) -> Priority {
        Priority::from_u8(self.priority.load(Ordering::Relaxed))
    }

    fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Relaxed) != 0
    }
}

/// A pending highlight. Await it for the [`Outcome`]; drop it to cancel
/// the job (it is skipped if still queued, or stopped mid-parse).
pub struct HighlightJob {
    state: Option<Arc<JobState>>,
    reply: oneshot::Receiver<Result<Outcome, HighlightError>>,
}

impl HighlightJob {
    /// A handle that changes the job's priority while it is queued.
    pub fn control(&self) -> JobControl {
        JobControl {
            state: self.state.clone(),
        }
    }

    fn ready(outcome: Outcome) -> Self {
        let (sender, reply) = oneshot::channel();
        // The receiver is alive right here, so the send cannot fail.
        let _ = sender.send(Ok(outcome));
        Self { state: None, reply }
    }
}

impl Future for HighlightJob {
    type Output = Result<Outcome, HighlightError>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        Pin::new(&mut self.reply)
            .poll(cx)
            .map(|received| received.unwrap_or(Err(HighlightError::Cancelled)))
    }
}

impl Drop for HighlightJob {
    fn drop(&mut self) {
        if let Some(state) = &self.state {
            state.cancelled.store(1, Ordering::Relaxed);
        }
    }
}

/// Changes the priority of a queued [`HighlightJob`]; does not keep it
/// alive.
#[derive(Clone)]
pub struct JobControl {
    state: Option<Arc<JobState>>,
}

impl JobControl {
    /// Raises or lowers the job; no effect once it started or finished.
    pub fn set_priority(&self, priority: Priority) {
        if let Some(state) = &self.state {
            state.priority.store(priority as u8, Ordering::Relaxed);
        }
    }
}

#[derive(Default)]
struct Queue {
    jobs: Vec<Arc<JobState>>,
    next_seq: u64,
    shutdown: bool,
}

impl Queue {
    /// Removes and returns the most urgent live job; drops cancelled ones.
    fn pop(&mut self) -> Option<Arc<JobState>> {
        self.jobs.retain(|job| !job.is_cancelled());
        let best = self
            .jobs
            .iter()
            .enumerate()
            .max_by_key(|(_, job)| (job.priority(), std::cmp::Reverse(job.seq)))
            .map(|(index, _)| index)?;
        Some(self.jobs.swap_remove(best))
    }
}

struct CacheEntry {
    highlights: Arc<Highlights>,
    bytes: usize,
    last_used: u64,
}

/// Results by key, bounded by memory; least recently used goes first.
struct Cache {
    entries: HashMap<HighlightKey, CacheEntry>,
    bytes: usize,
    budget: usize,
    tick: u64,
}

impl Cache {
    fn get(&mut self, key: &HighlightKey) -> Option<Arc<Highlights>> {
        self.tick += 1;
        let entry = self.entries.get_mut(key)?;
        entry.last_used = self.tick;
        Some(entry.highlights.clone())
    }

    fn insert(&mut self, key: HighlightKey, highlights: Arc<Highlights>) {
        let bytes = highlights.memory_size();
        if bytes > self.budget {
            return;
        }
        self.tick += 1;
        if let Some(old) = self.entries.insert(
            key,
            CacheEntry {
                highlights,
                bytes,
                last_used: self.tick,
            },
        ) {
            self.bytes -= old.bytes;
        }
        self.bytes += bytes;
        while self.bytes > self.budget {
            let Some(victim) = self
                .entries
                .iter()
                .min_by_key(|(_, entry)| entry.last_used)
                .map(|(key, _)| *key)
            else {
                break;
            };
            if let Some(entry) = self.entries.remove(&victim) {
                self.bytes -= entry.bytes;
            }
        }
    }
}

struct Shared {
    queue: Mutex<Queue>,
    available: Condvar,
    cache: Mutex<Cache>,
}

impl Shared {
    fn queue(&self) -> MutexGuard<'_, Queue> {
        self.queue
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn cache(&self) -> MutexGuard<'_, Cache> {
        self.cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// Stops the workers when the last service handle goes away.
struct Owner(Arc<Shared>);

impl Drop for Owner {
    fn drop(&mut self) {
        self.0.queue().shutdown = true;
        self.0.available.notify_all();
    }
}

/// The background syntax highlighter (spec §4, §4.2).
///
/// Cheap to clone; every clone shares the pool and the cache. Requests are
/// served by dedicated worker threads, most urgent first, so the caller's
/// thread never parses. The workers stop when the last clone is dropped.
#[derive(Clone)]
pub struct HighlightService {
    shared: Arc<Shared>,
    _owner: Arc<Owner>,
}

impl HighlightService {
    /// Starts a pool sized to the machine: half the cores, between one and
    /// four threads, so highlighting never starves diffing.
    pub fn new() -> Self {
        let cores = thread::available_parallelism().map_or(2, |cores| cores.get());
        Self::with_threads((cores / 2).clamp(1, 4), DEFAULT_CACHE_BYTES)
    }

    /// Starts a pool of `threads` workers whose cache holds up to
    /// `cache_bytes` of results.
    pub fn with_threads(threads: usize, cache_bytes: usize) -> Self {
        let shared = Arc::new(Shared {
            queue: Mutex::new(Queue::default()),
            available: Condvar::new(),
            cache: Mutex::new(Cache {
                entries: HashMap::new(),
                bytes: 0,
                budget: cache_bytes,
                tick: 0,
            }),
        });
        for index in 0..threads.max(1) {
            let shared = shared.clone();
            let spawned = thread::Builder::new()
                .name(format!("argus-highlight-{index}"))
                .spawn(move || worker(&shared));
            if let Err(error) = spawned {
                tracing::error!(%error, "failed to start a highlight worker");
            }
        }
        // Compile every grammar's queries up front (tens of ms for the big
        // ones), so the first file of a language is not delayed by it.
        let warm_up = thread::Builder::new()
            .name("argus-highlight-warmup".to_owned())
            .spawn(|| {
                for language in Language::ALL {
                    grammars::config(language);
                }
            });
        if let Err(error) = warm_up {
            tracing::warn!(%error, "failed to start the grammar warm-up");
        }
        Self {
            _owner: Arc::new(Owner(shared.clone())),
            shared,
        }
    }

    /// The cached result for `key`, if any. A map lookup; safe on the UI
    /// thread, so a file coming back into view repaints highlighted at once.
    pub fn cached(&self, key: &HighlightKey) -> Option<Arc<Highlights>> {
        self.shared.cache().get(key)
    }

    /// Queues a request. Resolves at once from the cache when possible.
    pub fn request(&self, request: HighlightRequest) -> HighlightJob {
        if let Some(highlights) = self.cached(&request.key) {
            return HighlightJob::ready(Outcome::Highlighted(highlights));
        }
        let (sender, reply) = oneshot::channel();
        let mut queue = self.shared.queue();
        let state = Arc::new(JobState {
            seq: queue.next_seq,
            priority: AtomicU8::new(request.priority as u8),
            cancelled: AtomicUsize::new(0),
            work: Mutex::new(Some(Work {
                key: request.key,
                text: request.text,
                force: request.force,
                reply: sender,
            })),
        });
        queue.next_seq += 1;
        queue.jobs.push(state.clone());
        drop(queue);
        self.shared.available.notify_one();
        HighlightJob {
            state: Some(state),
            reply,
        }
    }
}

impl Default for HighlightService {
    fn default() -> Self {
        Self::new()
    }
}

fn worker(shared: &Shared) {
    // One parser per thread, reused across jobs.
    let mut highlighter = TsHighlighter::new();
    loop {
        let job = {
            let mut queue = shared.queue();
            loop {
                if queue.shutdown {
                    return;
                }
                if let Some(job) = queue.pop() {
                    break job;
                }
                queue = shared
                    .available
                    .wait(queue)
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
            }
        };
        let work = job
            .work
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
        let Some(work) = work else {
            continue;
        };
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            run(shared, &mut highlighter, &job, &work)
        }))
        .unwrap_or_else(|_| {
            // Keep the worker alive; the parser state is unknown, so start
            // from a fresh one.
            highlighter = TsHighlighter::new();
            tracing::error!(language = work.key.language.name(), "highlighting panicked");
            Err(HighlightError::Failed(
                "the highlighter panicked".to_owned(),
            ))
        });
        if matches!(result, Err(HighlightError::Cancelled)) {
            // Nobody waits for a cancelled job.
            continue;
        }
        let _ = work.reply.send(result);
    }
}

fn run(
    shared: &Shared,
    highlighter: &mut TsHighlighter,
    job: &JobState,
    work: &Work,
) -> Result<Outcome, HighlightError> {
    if job.is_cancelled() {
        return Err(HighlightError::Cancelled);
    }
    // A duplicate request may have finished while this one was queued.
    if let Some(highlights) = shared.cache().get(&work.key) {
        return Ok(Outcome::Highlighted(highlights));
    }
    let text = (*work.text).as_ref();
    let bytes = text.len();
    let lines = highlights::line_count(text);
    if !work.force && (bytes > MAX_HIGHLIGHT_BYTES || lines > MAX_HIGHLIGHT_LINES) {
        return Ok(Outcome::TooLarge { bytes, lines });
    }
    let started = std::time::Instant::now();
    let computed = highlights::compute(highlighter, work.key.language, text, &job.cancelled);
    // A cancelled parse leaves the parser mid-document.
    if computed.is_err() {
        highlighter.parser.reset();
    }
    let highlights = Arc::new(computed?);
    tracing::debug!(
        language = work.key.language.name(),
        bytes,
        lines,
        elapsed_ms = started.elapsed().as_secs_f64() * 1000.0,
        "highlighted a blob"
    );
    shared.cache().insert(work.key, highlights.clone());
    Ok(Outcome::Highlighted(highlights))
}
