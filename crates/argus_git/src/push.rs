//! The git steps of Push (spec §5.4): the head's upstream, a fetch of that
//! one branch, ahead/behind counts and a plain `git push`.
//!
//! Every function runs `git -C <dir>` through [`GitCli`], where `dir` is the
//! repository's `_integration` worktree when Push runs them (see
//! [`WorktreeManager::integration_worktree`](crate::WorktreeManager::integration_worktree)).
//! They only read and move refs and the remote: no working tree or index is
//! touched. The rebase step lives in
//! [`WorktreeManager::rebase`](crate::WorktreeManager::rebase), behind the
//! worktrees guard.
//!
//! Pushes never force: [`push_branch`] builds a plain `git push <remote>
//! refs/heads/<head>:refs/heads/<branch>`, and [`GitCli`] refuses any push
//! argv that forces or deletes ([`crate::cli::check_args`]).

use std::fmt;
use std::path::Path;

use crate::cli::{GitCli, GitOutput};
use crate::error::{GitError, Result};
use crate::read::{Repo, Sha};

/// Where a local branch pushes to and pulls from: `branch.<head>.remote`
/// and the branch part of `branch.<head>.merge`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Upstream {
    /// The remote's name, e.g. `origin`.
    pub remote: String,
    /// The branch on the remote, e.g. `feature` (without `refs/heads/`).
    pub branch: String,
}

impl Upstream {
    /// The remote-tracking ref Argus fetches into:
    /// `refs/remotes/<remote>/<branch>`.
    pub fn tracking_ref(&self) -> String {
        format!("refs/remotes/{}/{}", self.remote, self.branch)
    }
}

impl fmt::Display for Upstream {
    /// `origin/feature`, as git shows it.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.remote, self.branch)
    }
}

/// One git command of a Push and what it printed, for the progress popover.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StepLog {
    /// The command line, `git ...`.
    pub command: String,
    /// Standard output and standard error, trimmed, in that order.
    pub output: String,
    /// Whether git exited with status 0.
    pub success: bool,
}

impl StepLog {
    fn new(args: &[&str], output: &GitOutput) -> Self {
        Self::from_output(format!("git {}", args.join(" ")), output)
    }

    /// The log of `command` from what it printed. Carriage returns of
    /// git's progress meters become line breaks.
    pub(crate) fn from_output(command: String, output: &GitOutput) -> Self {
        let stdout = output.stdout_lossy();
        let text = [stdout.trim(), output.stderr.trim()]
            .into_iter()
            .filter(|part| !part.is_empty())
            .collect::<Vec<_>>()
            .join("\n")
            .replace("\r\n", "\n")
            .replace('\r', "\n");
        Self {
            command,
            output: text,
            success: output.success(),
        }
    }
}

/// How [`fetch_upstream`] went.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FetchOutcome {
    /// The remote-tracking ref now points to the remote's branch tip.
    Fetched(Sha),
    /// The remote has no such branch (never pushed, or deleted there).
    Missing,
    /// git failed (network, authentication, ...); the log has its output.
    Failed,
}

/// How [`push_branch`] went.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PushResult {
    /// The remote branch now points to the local head tip.
    Pushed,
    /// The remote refused a non-fast-forward update: it moved since the
    /// fetch. Fetching (and rebasing) again may succeed.
    Rejected,
    /// Any other failure (network, authentication, a hook declined, ...).
    Failed,
}

/// The head's upstream from the repository config, read fresh with `git
/// config` (an upstream set by `git push -u` a moment ago is seen).
///
/// `None` when the branch has no upstream, or when its upstream is a local
/// branch (`branch.<head>.remote = .`), which cannot be pushed to.
pub async fn upstream_of(repo: &Repo, head: &str) -> Result<Option<Upstream>> {
    validate_name(head, |name| GitError::InvalidBranchName { name })?;
    let cli = GitCli::new(repo.workdir());
    let remote = config_value(&cli, &format!("branch.{head}.remote")).await?;
    let merge = config_value(&cli, &format!("branch.{head}.merge")).await?;
    let (Some(remote), Some(merge)) = (remote, merge) else {
        return Ok(None);
    };
    if remote == "." {
        return Ok(None);
    }
    let Some(branch) = merge.strip_prefix("refs/heads/") else {
        return Ok(None);
    };
    Ok(Some(Upstream {
        remote,
        branch: branch.to_owned(),
    }))
}

/// `git config --get <key>`; `None` when it is not set.
async fn config_value(cli: &GitCli, key: &str) -> Result<Option<String>> {
    let output = cli.run_unchecked(["config", "--get", key]).await?;
    let value = output.stdout_lossy().trim().to_owned();
    Ok((output.success() && !value.is_empty()).then_some(value))
}

/// `git fetch --no-tags <remote> +refs/heads/<branch>:refs/remotes/<remote>/<branch>`:
/// fetches the upstream branch only, and updates its remote-tracking ref
/// whatever the remote's configured refspecs say.
///
/// The `+` only lets the local remote-tracking ref follow a remote that was
/// rewritten; nothing is ever forced on the remote.
pub async fn fetch_upstream(dir: &Path, upstream: &Upstream) -> Result<(StepLog, FetchOutcome)> {
    validate_upstream(upstream)?;
    let refspec = format!(
        "+refs/heads/{}:{}",
        upstream.branch,
        upstream.tracking_ref()
    );
    let args = ["fetch", "--no-tags", upstream.remote.as_str(), &refspec];
    let cli = GitCli::new(dir);
    let output = cli.run_unchecked(args).await?;
    let log = StepLog::new(&args, &output);
    if !output.success() {
        let missing = output.stderr.contains("couldn't find remote ref");
        tracing::info!(remote = %upstream.remote, branch = %upstream.branch, missing, "upstream fetch failed");
        return Ok((
            log,
            if missing {
                FetchOutcome::Missing
            } else {
                FetchOutcome::Failed
            },
        ));
    }
    match ref_tip(dir, &upstream.tracking_ref()).await? {
        Some(tip) => Ok((log, FetchOutcome::Fetched(tip))),
        None => Ok((log, FetchOutcome::Missing)),
    }
}

/// The commit `name` (a full ref) points to, `None` when it does not exist.
pub async fn ref_tip(dir: &Path, name: &str) -> Result<Option<Sha>> {
    let spec = format!("{name}^{{commit}}");
    let output = GitCli::new(dir)
        .run_unchecked(["rev-parse", "--verify", "--quiet", spec.as_str()])
        .await?;
    if !output.success() {
        return Ok(None);
    }
    let hex = output.stdout_lossy();
    Sha::parse(hex.trim())
        .map(Some)
        .ok_or_else(|| GitError::Parse {
            command: format!("git rev-parse --verify {spec}"),
            message: format!("not an object id: `{}`", hex.trim()),
        })
}

/// Commits `local` has that `upstream` lacks (ahead) and the reverse
/// (behind): `git rev-list --left-right --count <local>...<upstream>`.
pub async fn ahead_behind(dir: &Path, local: &Sha, upstream: &Sha) -> Result<(u32, u32)> {
    if local == upstream {
        return Ok((0, 0));
    }
    let range = format!("{}...{}", local.to_hex(), upstream.to_hex());
    let args = ["rev-list", "--left-right", "--count", range.as_str()];
    let output = GitCli::new(dir).run(args).await?;
    let text = output.stdout_lossy();
    let parse_error = || GitError::Parse {
        command: format!("git {}", args.join(" ")),
        message: format!("expected two counts, got `{}`", text.trim()),
    };
    let mut counts = text.split_whitespace().map(str::parse::<u32>);
    match (counts.next(), counts.next()) {
        (Some(Ok(ahead)), Some(Ok(behind))) => Ok((ahead, behind)),
        _ => Err(parse_error()),
    }
}

/// Number of commits reachable from `tip`: what a push to a remote without
/// the branch sends at most.
pub async fn commit_count(dir: &Path, tip: &Sha) -> Result<u32> {
    let hex = tip.to_hex();
    let output = GitCli::new(dir)
        .run(["rev-list", "--count", hex.as_str()])
        .await?;
    let text = output.stdout_lossy();
    text.trim().parse().map_err(|_| GitError::Parse {
        command: format!("git rev-list --count {hex}"),
        message: format!("not a count: `{}`", text.trim()),
    })
}

/// `git push [--set-upstream] <remote> refs/heads/<head>:refs/heads/<branch>`.
///
/// No `--force`, no `--force-with-lease`: a remote that moved rejects the
/// push ([`PushResult::Rejected`]) and the caller fetches again. With
/// `set_upstream`, git also records `<remote>/<branch>` as the head's
/// upstream (`branch.<head>.remote` / `.merge`).
pub async fn push_branch(
    dir: &Path,
    upstream: &Upstream,
    head: &str,
    set_upstream: bool,
) -> Result<(StepLog, PushResult)> {
    validate_upstream(upstream)?;
    validate_name(head, |name| GitError::InvalidBranchName { name })?;
    let head = head.strip_prefix("refs/heads/").unwrap_or(head);
    let refspec = format!("refs/heads/{head}:refs/heads/{}", upstream.branch);
    let mut args = vec!["push", "--porcelain"];
    if set_upstream {
        args.push("--set-upstream");
    }
    args.extend([upstream.remote.as_str(), refspec.as_str()]);
    let output = GitCli::new(dir).run_unchecked(&args).await?;
    let log = StepLog::new(&args, &output);
    if output.success() {
        tracing::info!(remote = %upstream.remote, branch = %upstream.branch, "pushed");
        return Ok((log, PushResult::Pushed));
    }
    // `--porcelain` reports a refused ref as `!<TAB>src:dst<TAB>[rejected]
    // (fetch first)` or `(non-fast-forward)`; `[remote rejected]` (a hook)
    // is not a race and is not retried.
    let text = format!("{}\n{}", output.stdout_lossy(), output.stderr);
    let rejected = text.lines().any(|line| {
        line.contains("[rejected]")
            && (line.contains("fetch first")
                || line.contains("non-fast-forward")
                || line.contains("stale info"))
    });
    tracing::info!(remote = %upstream.remote, branch = %upstream.branch, rejected, "push failed");
    Ok((
        log,
        if rejected {
            PushResult::Rejected
        } else {
            PushResult::Failed
        },
    ))
}

fn validate_upstream(upstream: &Upstream) -> Result<()> {
    validate_name(&upstream.remote, |name| GitError::InvalidRemote { name })?;
    validate_name(&upstream.branch, |name| GitError::InvalidBranchName {
        name,
    })
}

/// Refuses names git would misread as options or that break a refspec.
fn validate_name(name: &str, error: impl FnOnce(String) -> GitError) -> Result<()> {
    if name.is_empty()
        || name.starts_with('-')
        || name.starts_with('+')
        || name.contains(char::is_whitespace)
        || name.contains(':')
    {
        return Err(error(name.to_owned()));
    }
    Ok(())
}
