//! Branch listing from `refs/heads/*` and `refs/remotes/*`.

use gix::remote::Direction;

use super::{Repo, Sha, blocking};
use crate::error::{GitError, Result};

/// Where a branch lives.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum BranchKind {
    /// `refs/heads/*`.
    Local,
    /// `refs/remotes/*` (remote-tracking).
    Remote,
}

/// One branch and its tip.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Branch {
    /// Short name: `main` for locals, `origin/main` for remotes.
    pub name: String,
    /// Full ref name, e.g. `refs/heads/main`.
    pub full_name: String,
    /// Local or remote-tracking.
    pub kind: BranchKind,
    /// Commit the branch points to (tags are peeled).
    pub tip: Sha,
    /// Committer time of the tip, unix seconds; `None` if the tip is not a
    /// readable commit.
    pub tip_time: Option<i64>,
    /// Short name of the remote-tracking branch configured as upstream
    /// (`origin/main`), locals only.
    pub upstream: Option<String>,
}

/// Lists local and remote-tracking branches, skipping symbolic `*/HEAD`
/// refs. Locals come first, each group sorted by name.
pub async fn branches(repo: &Repo) -> Result<Vec<Branch>> {
    let repo = repo.clone();
    blocking(move |cancel| {
        let repo = repo.local();
        let refs = repo
            .references()
            .map_err(|err| GitError::gix("reading references", err))?;
        let mut out = Vec::new();
        for (kind, iter) in [
            (BranchKind::Local, refs.local_branches()),
            (BranchKind::Remote, refs.remote_branches()),
        ] {
            let iter = iter.map_err(|err| GitError::gix("listing branches", err))?;
            for (index, reference) in iter.enumerate() {
                if index.is_multiple_of(256) {
                    cancel.check()?;
                }
                let mut reference =
                    reference.map_err(|err| GitError::gix("reading a branch ref", err))?;
                // `refs/remotes/origin/HEAD` is a symbolic pointer, not a branch.
                if reference.name().as_bstr().ends_with(b"/HEAD") {
                    continue;
                }
                let full_name = reference.name().as_bstr().to_string();
                let name = reference.name().shorten().to_string();
                let tip_id = match reference.peel_to_id() {
                    Ok(id) => id.detach(),
                    Err(err) => {
                        tracing::warn!(%full_name, error = %format!("{err:#}"), "skipping unresolvable branch");
                        continue;
                    }
                };
                let tip_time = repo
                    .find_commit(tip_id)
                    .ok()
                    .and_then(|commit| commit.time().ok())
                    .map(|time| time.seconds);
                let upstream = match kind {
                    BranchKind::Local => repo
                        .branch_remote_tracking_ref_name(reference.name(), Direction::Fetch)
                        .and_then(Result::ok)
                        .map(|name| name.shorten().to_string()),
                    BranchKind::Remote => None,
                };
                out.push(Branch {
                    name,
                    full_name,
                    kind,
                    tip: Sha::from(tip_id),
                    tip_time,
                    upstream,
                });
            }
        }
        out.sort_by(|a, b| a.kind.cmp(&b.kind).then_with(|| a.name.cmp(&b.name)));
        Ok(out)
    })
    .await
}
