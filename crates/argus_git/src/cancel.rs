//! Cooperative cancellation shared between the UI, services and worker
//! threads.

use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::error::{GitError, Result};

/// A cheap, cloneable cancellation flag.
///
/// Tokens form a tree: a [`child`](Self::child) is cancelled when it or any
/// of its ancestors is cancelled, while cancelling a child leaves its parent
/// untouched. A diff session owns a root token; each job it starts gets a
/// child, so dropping the session stops every job, and dropping one job's
/// future stops only that job.
#[derive(Clone, Default)]
pub struct CancelToken(Arc<Node>);

#[derive(Default)]
struct Node {
    cancelled: AtomicBool,
    parent: Option<CancelToken>,
}

impl CancelToken {
    /// A fresh, uncancelled root token.
    pub fn new() -> Self {
        Self::default()
    }

    /// A token cancelled together with `self`, which can also be cancelled
    /// on its own.
    pub fn child(&self) -> Self {
        Self(Arc::new(Node {
            cancelled: AtomicBool::new(false),
            parent: Some(self.clone()),
        }))
    }

    /// Raises the flag. Work holding this token (or a child) stops at its
    /// next check.
    pub fn cancel(&self) {
        self.0.cancelled.store(true, Ordering::Relaxed);
    }

    /// Whether this token or one of its ancestors was cancelled.
    pub fn is_cancelled(&self) -> bool {
        let mut node = &self.0;
        loop {
            if node.cancelled.load(Ordering::Relaxed) {
                return true;
            }
            match &node.parent {
                Some(parent) => node = &parent.0,
                None => return false,
            }
        }
    }

    /// `Err(GitError::Cancelled)` once cancelled, for use with `?`.
    pub fn check(&self) -> Result<()> {
        if self.is_cancelled() {
            Err(GitError::Cancelled)
        } else {
            Ok(())
        }
    }

    /// A guard that cancels this token when dropped, e.g. when the future
    /// awaiting a background job goes away.
    pub fn drop_guard(self) -> CancelGuard {
        CancelGuard(self)
    }
}

impl fmt::Debug for CancelToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CancelToken")
            .field("cancelled", &self.is_cancelled())
            .finish()
    }
}

/// Cancels its token on drop. See [`CancelToken::drop_guard`].
#[derive(Debug)]
pub struct CancelGuard(CancelToken);

impl CancelGuard {
    /// The guarded token.
    pub fn token(&self) -> &CancelToken {
        &self.0
    }
}

impl Drop for CancelGuard {
    fn drop(&mut self) {
        self.0.cancel();
    }
}
