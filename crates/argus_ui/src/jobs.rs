//! Background job state shown in the status bar.

use gpui::{App, BorrowAppContext, Global};

/// What is running in the background, as a GPUI global.
///
/// Services update it through [`BackgroundJobs::update`] when diffing,
/// highlighting, fetching or runs start and stop; the status bar observes it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BackgroundJobs {
    /// A diff is being computed.
    pub diffing: bool,
    /// Syntax highlighting is in progress.
    pub highlighting: bool,
    /// A `git fetch` is running.
    pub fetching: bool,
    /// Number of active Claude runs.
    pub active_runs: usize,
}

impl Global for BackgroundJobs {}

impl BackgroundJobs {
    /// Whether anything is in progress.
    pub fn is_busy(&self) -> bool {
        self.diffing || self.highlighting || self.fetching || self.active_runs > 0
    }

    /// Short labels for the work in progress, in display order.
    pub fn labels(&self) -> Vec<String> {
        let mut labels = Vec::new();
        if self.diffing {
            labels.push("Diffing…".to_owned());
        }
        if self.highlighting {
            labels.push("Highlighting…".to_owned());
        }
        if self.fetching {
            labels.push("Fetching…".to_owned());
        }
        match self.active_runs {
            0 => {}
            1 => labels.push("1 run active".to_owned()),
            runs => labels.push(format!("{runs} runs active")),
        }
        labels
    }

    /// Mutates the global and notifies its observers.
    pub fn update(cx: &mut App, f: impl FnOnce(&mut BackgroundJobs)) {
        cx.update_global::<BackgroundJobs, _>(|jobs, _| f(jobs));
    }
}
