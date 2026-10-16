//! Running service work on the tokio runtime from views.

use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

use gpui::App;
use tokio::task::{JoinError, JoinHandle};

use crate::AppContext;

/// A future running on the tokio runtime, awaited from a GPUI task.
///
/// Dropping it aborts the tokio task. A view that replaces (or drops) the GPUI
/// task awaiting it therefore cancels the work too: git reads notice the
/// dropped future and stop, child processes are killed (spec §4.2).
pub struct RuntimeJob<T> {
    handle: JoinHandle<T>,
}

impl<T> Future for RuntimeJob<T> {
    type Output = Result<T, JoinError>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        Pin::new(&mut self.handle).poll(cx)
    }
}

impl<T> Drop for RuntimeJob<T> {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

/// Spawns `future` on the tokio runtime of the [`AppContext`] global.
pub fn spawn_job<T, F>(cx: &App, future: F) -> RuntimeJob<T>
where
    T: Send + 'static,
    F: Future<Output = T> + Send + 'static,
{
    RuntimeJob {
        handle: cx.global::<AppContext>().runtime.spawn(future),
    }
}

/// Spawns `future` on the tokio runtime and lets it finish even if nobody
/// waits for it; for writes that must not be cut short (e.g. saving the last
/// branch selection).
pub fn spawn_detached<F>(cx: &App, future: F)
where
    F: Future<Output = ()> + Send + 'static,
{
    drop(cx.global::<AppContext>().runtime.spawn(future));
}
