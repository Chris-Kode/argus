//! UI timing mode (`ARGUS_PERF=1`, issue #12): measures the spec §12
//! targets on a real window and prints a report.
//!
//! - **Startup**: when the branches are listed, when the file list is
//!   computed, when the file tree first paints rows, and when the diff view
//!   first paints lines of a loaded file. Times count from the moment the workspace starts opening the
//!   repository (and, in parentheses, from process start).
//! - **Auto-scroll** (`--auto-scroll`): one second after the first diff
//!   paint, the diff view scrolls by [`SCROLL_STEP`] pixels every frame
//!   until its end or [`MAX_SCROLL_FRAMES`]. Each frame records its build
//!   time (the root view's render to the end of the window's paint, CPU
//!   side) and its interval (paint to paint, which includes waiting for
//!   vsync). Then it measures how long the rows on screen take to finish
//!   loading and highlighting after the scroll stops, prints the report
//!   and quits.
//!
//! The probe is a GPUI global that exists only in this mode; every hook is
//! a no-op without it. Hooks that see something painted ([`mark_painted`])
//! only stage the milestone: it is stamped when the frame finishes
//! painting, in the root's [`frame_marker`].

use std::fmt::Write as _;
use std::io::Write as _;
use std::time::{Duration, Instant};

use gpui::{AnyElement, App, Global, WeakEntity, Window, canvas, prelude::*, px};

use crate::diff_view::DiffView;
use crate::jobs::BackgroundJobs;

/// Pixels the diff view scrolls per frame (a fast trackpad flick).
const SCROLL_STEP: f64 = 48.0;
/// Upper bound of the scroll, in frames (10 s at 120 Hz).
const MAX_SCROLL_FRAMES: usize = 1_200;
/// Pause between the first diff paint and the scroll, so the scroll
/// measures scrolling rather than the tail of startup.
const SCROLL_DELAY: Duration = Duration::from_secs(1);
/// How often the settle check polls after the scroll stops.
const SETTLE_POLL: Duration = Duration::from_millis(1);
/// Give up waiting for the rows on screen to settle after this long.
const SETTLE_TIMEOUT: Duration = Duration::from_secs(10);
/// Report whatever was measured if the run has not finished by then.
const WATCHDOG: Duration = Duration::from_secs(180);
/// The frame budget of spec §12 (120 fps would be 8.3 ms).
const FRAME_BUDGET_MS: f64 = 16.0;

/// What the timing mode does, from the command line.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PerfOptions {
    /// Scroll through the diff once it shows, then print the report and
    /// quit.
    pub auto_scroll: bool,
}

/// A startup milestone.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Milestone {
    /// The window painted its first frame.
    FirstFrame,
    /// The branch list of the repository arrived (not painted yet).
    BranchesListed,
    /// The changed-file list arrived in the file tree (not painted yet).
    FileListComputed,
    /// The file tree painted its first rows.
    FileTreePainted,
    /// The diff view painted lines of a loaded file.
    FirstDiffPainted,
}

impl Milestone {
    const ALL: [Milestone; 5] = [
        Milestone::FirstFrame,
        Milestone::BranchesListed,
        Milestone::FileListComputed,
        Milestone::FileTreePainted,
        Milestone::FirstDiffPainted,
    ];

    fn index(self) -> usize {
        self as usize
    }

    fn label(self) -> &'static str {
        match self {
            Milestone::FirstFrame => "window first frame",
            Milestone::BranchesListed => "branches listed",
            Milestone::FileListComputed => "file list computed",
            Milestone::FileTreePainted => "file tree first paint",
            Milestone::FirstDiffPainted => "first diff paint",
        }
    }
}

/// Where the auto-scroll is.
#[derive(Clone, Copy, Debug, PartialEq)]
enum ScrollPhase {
    /// Waiting for the first diff paint (or not requested).
    Idle,
    /// Paused for [`SCROLL_DELAY`].
    Delayed,
    /// Scrolling; frames are being recorded.
    Running,
    /// Stopped at `Instant`; waiting for the rows on screen to settle.
    Settling(Instant),
    /// Reported.
    Done,
}

/// The measurements of one session, as a GPUI global.
pub(crate) struct PerfProbe {
    options: PerfOptions,
    launched: Instant,
    opened: Option<Instant>,
    milestones: [Option<Instant>; 5],
    staged: Vec<Milestone>,
    frame_started: Option<Instant>,
    last_paint: Option<Instant>,
    diff: Option<WeakEntity<DiffView>>,
    files: Option<usize>,
    phase: ScrollPhase,
    scroll_started: Option<Instant>,
    scrolled: f64,
    builds_ms: Vec<f64>,
    intervals_ms: Vec<f64>,
    settled_ms: Option<f64>,
    startup_reported: bool,
}

impl Global for PerfProbe {}

/// Turns the timing mode on. `launched` is the process start.
pub fn install(options: PerfOptions, launched: Instant, cx: &mut App) {
    cx.set_global(PerfProbe {
        options,
        launched,
        opened: None,
        milestones: [None; 5],
        staged: Vec::new(),
        frame_started: None,
        last_paint: None,
        diff: None,
        files: None,
        phase: ScrollPhase::Idle,
        scroll_started: None,
        scrolled: 0.0,
        builds_ms: Vec::new(),
        intervals_ms: Vec::new(),
        settled_ms: None,
        startup_reported: false,
    });
    tracing::info!(?options, "perf timing mode on");
    if options.auto_scroll {
        cx.spawn(async move |cx| {
            cx.background_executor().timer(WATCHDOG).await;
            cx.update(|cx| {
                let done = cx
                    .try_global::<PerfProbe>()
                    .is_none_or(|probe| probe.phase == ScrollPhase::Done);
                if !done {
                    tracing::warn!("perf run timed out; reporting what was measured");
                    finish(cx);
                }
            });
        })
        .detach();
    }
}

/// Whether the timing mode is on.
pub(crate) fn enabled(cx: &App) -> bool {
    cx.has_global::<PerfProbe>()
}

/// The workspace starts opening a repository: the startup clock's zero.
pub(crate) fn repo_opening(cx: &mut App) {
    if let Some(probe) = probe_mut(cx) {
        probe.opened.get_or_insert_with(Instant::now);
    }
}

/// Records `milestone` now (data arrival, nothing painted).
pub(crate) fn mark(milestone: Milestone, cx: &mut App) {
    if let Some(probe) = probe_mut(cx) {
        probe.milestones[milestone.index()].get_or_insert_with(Instant::now);
    }
}

/// Stages `milestone`: it is stamped when the current frame ends painting.
pub(crate) fn mark_painted(milestone: Milestone, cx: &mut App) {
    if let Some(probe) = probe_mut(cx)
        && probe.milestones[milestone.index()].is_none()
        && !probe.staged.contains(&milestone)
    {
        probe.staged.push(milestone);
    }
}

/// The file tree received a list of `files` files.
pub(crate) fn file_list_computed(files: usize, cx: &mut App) {
    if let Some(probe) = probe_mut(cx) {
        probe.files.get_or_insert(files);
    }
    mark(Milestone::FileListComputed, cx);
}

/// The main diff view to scroll: the first one opened.
pub(crate) fn watch_diff(view: WeakEntity<DiffView>, cx: &mut App) {
    if let Some(probe) = probe_mut(cx)
        && probe.diff.is_none()
    {
        probe.diff = Some(view);
    }
}

/// The root view starts rendering a frame.
pub(crate) fn frame_started(cx: &mut App) {
    if let Some(probe) = probe_mut(cx) {
        probe.frame_started = Some(Instant::now());
    }
}

/// An invisible element, last in the root, whose paint ends the frame's
/// measurements. `None` when the timing mode is off.
pub(crate) fn frame_marker(cx: &App) -> Option<AnyElement> {
    enabled(cx).then(|| {
        canvas(|_, _, _| {}, |_, _, window, cx| frame_painted(window, cx))
            .absolute()
            .top_0()
            .left_0()
            .size(px(1.0))
            .into_any_element()
    })
}

fn probe_mut(cx: &mut App) -> Option<&mut PerfProbe> {
    if cx.has_global::<PerfProbe>() {
        Some(cx.global_mut::<PerfProbe>())
    } else {
        None
    }
}

/// End of a frame's paint: stamps the staged milestones and records the
/// frame while scrolling.
fn frame_painted(window: &mut Window, cx: &mut App) {
    let now = Instant::now();
    let Some(probe) = probe_mut(cx) else {
        return;
    };
    probe.milestones[Milestone::FirstFrame.index()].get_or_insert(now);
    let mut first_diff = false;
    for milestone in std::mem::take(&mut probe.staged) {
        let slot = &mut probe.milestones[milestone.index()];
        if slot.is_none() {
            *slot = Some(now);
            first_diff |= milestone == Milestone::FirstDiffPainted;
        }
    }
    if probe.phase == ScrollPhase::Running {
        if let Some(started) = probe.frame_started {
            probe.builds_ms.push(millis(now - started));
        }
        if let Some(last) = probe.last_paint {
            probe.intervals_ms.push(millis(now - last));
        }
    }
    probe.last_paint = Some(now);
    probe.frame_started = None;

    if first_diff {
        if probe.options.auto_scroll {
            probe.phase = ScrollPhase::Delayed;
            schedule_scroll(window, cx);
        } else {
            report_startup(cx);
        }
    }
}

/// Starts the scroll after [`SCROLL_DELAY`], driven frame by frame.
fn schedule_scroll(window: &mut Window, cx: &mut App) {
    let handle = window.window_handle();
    cx.spawn(async move |cx| {
        cx.background_executor().timer(SCROLL_DELAY).await;
        handle
            .update(cx, |_, window, cx| {
                if let Some(probe) = probe_mut(cx) {
                    probe.phase = ScrollPhase::Running;
                    probe.scroll_started = Some(Instant::now());
                    probe.last_paint = None;
                }
                scroll_step(window, cx);
            })
            .ok();
    })
    .detach();
}

/// One frame of the scroll: moves the diff view and asks for the next
/// frame, or stops at the end.
fn scroll_step(window: &mut Window, cx: &mut App) {
    let Some(probe) = probe_mut(cx) else {
        return;
    };
    if probe.phase != ScrollPhase::Running {
        return;
    }
    let frames = probe.intervals_ms.len();
    let view = probe.diff.as_ref().and_then(WeakEntity::upgrade);
    let moved = match view {
        Some(view) if frames < MAX_SCROLL_FRAMES => {
            view.update(cx, |view, cx| view.scroll_by_pixels(SCROLL_STEP, cx))
        }
        _ => 0.0,
    };
    if moved > 0.0 {
        if let Some(probe) = probe_mut(cx) {
            probe.scrolled += moved;
        }
        window.on_next_frame(scroll_step);
        return;
    }
    let stopped = Instant::now();
    if let Some(probe) = probe_mut(cx) {
        probe.phase = ScrollPhase::Settling(stopped);
    }
    // Repaint once more, so the rows at the final position request their
    // loads and highlights, then poll until they are done.
    window.refresh();
    let handle = window.window_handle();
    cx.spawn(async move |cx| {
        loop {
            cx.background_executor().timer(SETTLE_POLL).await;
            let settled = handle.update(cx, |_, _, cx| {
                let view = cx
                    .try_global::<PerfProbe>()
                    .and_then(|probe| probe.diff.as_ref())
                    .and_then(WeakEntity::upgrade);
                let quiet = view.is_none_or(|view| view.read(cx).visible_settled())
                    && !cx.global::<BackgroundJobs>().highlighting;
                if quiet || stopped.elapsed() >= SETTLE_TIMEOUT {
                    if quiet && let Some(probe) = probe_mut(cx) {
                        probe.settled_ms = Some(millis(stopped.elapsed()));
                    }
                    finish(cx);
                    true
                } else {
                    false
                }
            });
            if settled.unwrap_or(true) {
                break;
            }
        }
    })
    .detach();
}

/// Prints the full report and quits.
fn finish(cx: &mut App) {
    let Some(probe) = probe_mut(cx) else {
        return;
    };
    probe.phase = ScrollPhase::Done;
    let text = render_report(probe, true);
    emit(&text);
    cx.quit();
}

/// Prints the startup part once, when there is no scroll to wait for.
fn report_startup(cx: &mut App) {
    let Some(probe) = probe_mut(cx) else {
        return;
    };
    if std::mem::replace(&mut probe.startup_reported, true) {
        return;
    }
    let text = render_report(probe, false);
    emit(&text);
}

fn emit(text: &str) {
    tracing::info!("perf report\n{text}");
    let mut stdout = std::io::stdout().lock();
    // A closed stdout must not take the app down; the log has the report.
    let _ = stdout.write_all(text.as_bytes());
    let _ = stdout.flush();
}

fn millis(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1_000.0
}

/// The report text.
fn render_report(probe: &PerfProbe, with_scroll: bool) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "\n=== Argus perf report (ARGUS_PERF=1) ===");
    if let Some(files) = probe.files {
        let _ = writeln!(out, "changed files: {files}");
    }
    let origin = probe.opened.unwrap_or(probe.launched);
    let _ = writeln!(
        out,
        "startup, ms after opening the repository (after process start):"
    );
    for milestone in Milestone::ALL {
        let value = match probe.milestones[milestone.index()] {
            Some(at) => format!(
                "{:>9.1} ({:.1})",
                millis(at.saturating_duration_since(origin)),
                millis(at - probe.launched)
            ),
            None => format!("{:>9}", "-"),
        };
        let _ = writeln!(out, "  {:<24}{value}", milestone.label());
    }

    let mut checks = Vec::new();
    let since_open = |milestone: Milestone| {
        probe.milestones[milestone.index()].map(|at| millis(at.saturating_duration_since(origin)))
    };
    if let Some(ms) = since_open(Milestone::FileTreePainted) {
        checks.push((
            "file tree visible < 500 ms",
            ms < 500.0,
            format!("{ms:.1} ms"),
        ));
    }
    // "Open a single file": from the file list being known to its lines
    // on screen, i.e. the hunks computed, laid out and painted.
    if let (Some(list), Some(diff)) = (
        probe.milestones[Milestone::FileListComputed.index()],
        probe.milestones[Milestone::FirstDiffPainted.index()],
    ) {
        let ms = millis(diff.saturating_duration_since(list));
        checks.push((
            "first diff paint < 100 ms (after list)",
            ms < 100.0,
            format!("{ms:.1} ms"),
        ));
    }

    if with_scroll {
        let frames = probe.builds_ms.len();
        let seconds = probe
            .scroll_started
            .zip(match probe.phase {
                ScrollPhase::Settling(at) => Some(at),
                _ => probe.last_paint,
            })
            .map_or(0.0, |(start, end)| {
                end.saturating_duration_since(start).as_secs_f64()
            });
        let _ = writeln!(
            out,
            "auto-scroll: {frames} frames, {:.0} px in {seconds:.2} s ({:.0} fps)",
            probe.scrolled,
            if seconds > 0.0 {
                probe.intervals_ms.len() as f64 / seconds
            } else {
                0.0
            }
        );
        let _ = writeln!(
            out,
            "  {:<34}{:>8}{:>8}{:>8}{:>10}",
            "ms", "p50", "p95", "max", "> 16 ms"
        );
        for (label, samples) in [
            ("frame build (render to paint)", &probe.builds_ms),
            ("frame interval (paint to paint)", &probe.intervals_ms),
        ] {
            let stats = Stats::of(samples);
            let _ = writeln!(
                out,
                "  {label:<34}{:>8.2}{:>8.2}{:>8.2}{:>10}",
                stats.p50, stats.p95, stats.max, stats.over_budget
            );
        }
        if !probe.intervals_ms.is_empty() {
            let max = Stats::of(&probe.intervals_ms).max;
            checks.push((
                "no frame > 16 ms",
                max <= FRAME_BUDGET_MS,
                format!("max {max:.2} ms"),
            ));
        }
        match probe.settled_ms {
            Some(ms) => {
                let _ = writeln!(
                    out,
                    "  rows on screen loaded and highlighted {ms:.1} ms after the scroll stopped"
                );
                checks.push((
                    "highlight of visible region < 50 ms",
                    ms < 50.0,
                    format!("{ms:.1} ms"),
                ));
            }
            None => {
                let _ = writeln!(out, "  rows on screen did not settle after the scroll");
            }
        }
    }

    if let Some(mb) = peak_rss_mb() {
        let _ = writeln!(out, "peak RSS: {mb:.0} MB");
        checks.push((
            "memory < 500 MB (peak RSS)",
            mb < 500.0,
            format!("{mb:.0} MB"),
        ));
    }
    if !checks.is_empty() {
        let _ = writeln!(out, "spec §12 targets:");
        for (target, met, value) in checks {
            let verdict = if met { "ok  " } else { "MISS" };
            let _ = writeln!(out, "  [{verdict}] {target:<40}{value}");
        }
    }
    out
}

/// p50 / p95 / max of a sample, plus how many exceed the frame budget.
struct Stats {
    p50: f64,
    p95: f64,
    max: f64,
    over_budget: usize,
}

impl Stats {
    fn of(samples: &[f64]) -> Self {
        if samples.is_empty() {
            return Self {
                p50: 0.0,
                p95: 0.0,
                max: 0.0,
                over_budget: 0,
            };
        }
        let mut sorted = samples.to_vec();
        sorted.sort_by(f64::total_cmp);
        // Nearest-rank percentiles.
        let rank = |p: f64| {
            let index = ((p * sorted.len() as f64).ceil() as usize).clamp(1, sorted.len());
            sorted[index - 1]
        };
        Self {
            p50: rank(0.50),
            p95: rank(0.95),
            max: sorted[sorted.len() - 1],
            over_budget: sorted.iter().filter(|&&ms| ms > FRAME_BUDGET_MS).count(),
        }
    }
}

/// The process' peak resident set size, in MB.
#[cfg(not(unix))]
fn peak_rss_mb() -> Option<f64> {
    None
}

/// The process' peak resident set size, in MB.
#[cfg(unix)]
fn peak_rss_mb() -> Option<f64> {
    // SAFETY: `getrusage` only writes the zeroed struct it is given.
    let usage = unsafe {
        let mut usage: libc::rusage = std::mem::zeroed();
        if libc::getrusage(libc::RUSAGE_SELF, &mut usage) != 0 {
            return None;
        }
        usage
    };
    let max = usage.ru_maxrss as f64;
    // macOS reports bytes, Linux kibibytes.
    let bytes = if cfg!(target_os = "macos") {
        max
    } else {
        max * 1024.0
    };
    Some(bytes / (1024.0 * 1024.0))
}
