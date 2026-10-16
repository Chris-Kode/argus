//! Small ref reads used to pick sensible defaults: the checked-out branch,
//! the remote's default branch and the configured remotes.

use gix::refs::TargetRef;

use super::{Repo, blocking};
use crate::error::{GitError, Result};

/// Short name of the branch checked out in the working tree the repository
/// was opened from (`feature/x`), or `None` when `HEAD` is detached.
///
/// Only `HEAD` is read; the index and working tree are left alone.
pub async fn current_branch(repo: &Repo) -> Result<Option<String>> {
    let repo = repo.clone();
    blocking(move |_| {
        let name = repo
            .local()
            .head_name()
            .map_err(|err| GitError::gix("reading HEAD", err))?;
        Ok(name.and_then(|name| {
            name.as_bstr()
                .strip_prefix(b"refs/heads/")
                .map(|short| String::from_utf8_lossy(short).into_owned())
        }))
    })
    .await
}

/// Short name of the branch `refs/remotes/<remote>/HEAD` points to
/// (`origin/main`), i.e. the remote's default branch as recorded by
/// `git clone` or `git remote set-head`. `None` when the symbolic ref is
/// missing or not symbolic.
pub async fn remote_default_branch(repo: &Repo, remote: &str) -> Result<Option<String>> {
    let repo = repo.clone();
    let name = format!("refs/remotes/{remote}/HEAD");
    blocking(move |_| {
        let local = repo.local();
        let Some(reference) = local
            .try_find_reference(name.as_str())
            .map_err(|err| GitError::gix("looking up the remote HEAD", err))?
        else {
            return Ok(None);
        };
        Ok(match reference.target() {
            TargetRef::Symbolic(target) => Some(target.shorten().to_string()),
            TargetRef::Object(_) => None,
        })
    })
    .await
}

/// Names of every configured remote, sorted.
pub async fn remote_names(repo: &Repo) -> Result<Vec<String>> {
    let repo = repo.clone();
    blocking(move |_| {
        Ok(repo
            .local()
            .remote_names()
            .into_iter()
            .map(|name| name.to_string())
            .collect())
    })
    .await
}

/// Splits a remote-tracking branch short name (`origin/feature/x`) into its
/// remote (`origin`) and branch (`feature/x`) parts, given the configured
/// remote names. The longest matching remote wins, so a remote called
/// `origin/mirror` is preferred over `origin` for `origin/mirror/main`.
pub fn split_remote_branch<'a>(
    remotes: &[String],
    remote_branch: &'a str,
) -> Option<(&'a str, &'a str)> {
    remotes
        .iter()
        .filter_map(|remote| {
            let rest = remote_branch.strip_prefix(remote.as_str())?;
            let branch = rest.strip_prefix('/')?;
            (!branch.is_empty()).then(|| (&remote_branch[..remote.len()], branch))
        })
        .max_by_key(|(remote, _)| remote.len())
}
