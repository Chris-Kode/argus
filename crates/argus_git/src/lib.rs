//! Git access for Argus: `gix` for reads and diffs, the `git` CLI for writes.
//!
//! Filled in by issue #4 (read side: open repo, list branches, merge-base,
//! ahead/behind, fetch) and issue #17 (write side: worktrees, commit, rebase,
//! push). Nothing here ever reads or modifies a user's working checkout.

/// Handle to the git service.
///
/// Placeholder until issue #4 lands: it will own the per-repo read cache and
/// expose async, cancellable calls.
#[derive(Clone, Debug, Default)]
pub struct GitService;

impl GitService {
    /// Creates the service handle.
    pub fn new() -> Self {
        Self
    }
}
