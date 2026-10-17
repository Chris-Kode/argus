//! Conflicted states for resolve runs (spec §6.6, §11).
//!
//! When Accept or the pull before Push conflicts, the integration worktree
//! is reset right away, so nothing half-applied is left behind. A resolve
//! run needs that conflicted state back, in its own worktree, with the
//! conflict markers in the files:
//!
//! - [`WorktreeManager::apply_keeping_conflicts`]: `git apply --3way` of an
//!   Accept's patch, leaving the markers in place;
//! - [`WorktreeManager::rebase_until_conflict`]: `git rebase <upstream>`,
//!   stopped at the first local commit that conflicts;
//! - [`WorktreeManager::continue_rebase`]: `git add -A` + `git rebase
//!   --continue` once a stopped commit is resolved, stopping again at the
//!   next conflicting commit;
//! - [`WorktreeManager::conflict_markers`]: the markers left in a worktree
//!   (`git diff --check` + a scan of the conflicted files), which block
//!   the Accept of a resolution.
//!
//! Every function runs inside an Argus worktree, behind
//! [`WorktreeManager::guard`]; the user's checkout is never touched.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::cli::GitCli;
use crate::error::{GitError, Result};
use crate::push::StepLog;
use crate::read::Sha;
use crate::worktrees::{Worktree, WorktreeManager, io_error, unmerged_paths};

/// Files larger than this are not scanned for conflict markers (in bytes);
/// `git diff --check` still covers their added lines.
const SCAN_MAX_BYTES: u64 = 8 * 1024 * 1024;

/// Config pinned for every rebase in an Argus worktree: nothing reaches
/// outside the worktree or waits for an editor.
const REBASE_CONFIG: [&str; 8] = [
    "-c",
    "rebase.updateRefs=false",
    "-c",
    "rebase.autoStash=false",
    "-c",
    "rebase.autoSquash=false",
    "-c",
    "core.editor=true",
];

/// How [`WorktreeManager::apply_keeping_conflicts`] went.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConflictedApply {
    /// The patch conflicts: the worktree holds it with conflict markers in
    /// these files (relative to the repository root).
    Conflicted {
        /// The conflicted paths.
        files: Vec<PathBuf>,
    },
    /// The patch applies cleanly now (the head moved since the conflict);
    /// it is staged in the worktree.
    Clean,
}

/// A rebase stopped at a conflicting commit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RebaseStop {
    /// The worktree's `HEAD`: the commits replayed so far, on top of the
    /// upstream. The resolution is diffed against it.
    pub head: Sha,
    /// The local commit being replayed (`REBASE_HEAD`).
    pub commit: Sha,
    /// That commit's subject line.
    pub subject: String,
    /// The conflicted paths, relative to the repository root.
    pub files: Vec<PathBuf>,
}

/// How a rebase step in a resolve worktree went.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RebaseStep {
    /// Every commit is replayed; the worktree's detached `HEAD` is this
    /// commit.
    Finished(Sha),
    /// A commit conflicts; the rebase waits in the worktree.
    Stopped(RebaseStop),
    /// The rebase failed for another reason (e.g. commit signing); the log
    /// has git's output. The worktree is left as it is.
    Failed,
}

/// A conflict marker left in a file.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct ConflictMarker {
    /// The file, relative to the repository root.
    pub path: PathBuf,
    /// The 1-based line of the marker.
    pub line: u32,
}

impl std::fmt::Display for ConflictMarker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}", self.path.display(), self.line)
    }
}

impl WorktreeManager {
    /// `git apply --3way --index -` of `patch` in a resolve worktree, keeping
    /// the conflicts: the conflicted files hold `<<<<<<< ours` (the
    /// worktree's commit) / `>>>>>>> theirs` (the patch) markers and stay
    /// unmerged in the worktree's index.
    ///
    /// Fails when the patch is refused without any 3-way merge (e.g. a new
    /// file that already exists): there are no markers to resolve then.
    pub async fn apply_keeping_conflicts(
        &self,
        worktree: &Worktree,
        patch: &[u8],
    ) -> Result<ConflictedApply> {
        self.guard(worktree.path())?;
        let cli = GitCli::new(worktree.path());
        let output = cli
            .run_with_input(["apply", "--3way", "--index", "-"], patch)
            .await?;
        if output.success() {
            return Ok(ConflictedApply::Clean);
        }
        let files = unmerged_paths(&cli).await?;
        if files.is_empty() {
            return Err(GitError::Command {
                command: "git apply --3way --index -".to_owned(),
                code: output.code,
                stderr: output.stderr,
            });
        }
        tracing::info!(
            path = %worktree.path().display(),
            files = files.len(),
            "prepared the conflicted state of an accept"
        );
        Ok(ConflictedApply::Conflicted { files })
    }

    /// `git rebase <onto>` of the worktree's detached `HEAD`, left stopped at
    /// the first conflicting commit (unlike [`WorktreeManager::rebase`],
    /// which aborts). Returns the outcome and the command's log.
    pub async fn rebase_until_conflict(
        &self,
        worktree: &Worktree,
        onto: &Sha,
    ) -> Result<(RebaseStep, StepLog)> {
        self.guard(worktree.path())?;
        let cli = GitCli::new(worktree.path());
        let onto_hex = onto.to_hex();
        let mut args = REBASE_CONFIG.to_vec();
        args.extend(["rebase", onto_hex.as_str()]);
        let output = cli.run_unchecked(&args).await?;
        let log = StepLog::from_output(format!("git rebase {onto_hex}"), &output);
        let step = rebase_step(&cli, output.success()).await?;
        tracing::info!(path = %worktree.path().display(), %onto, step = step_name(&step), "rebase in a resolve worktree");
        Ok((step, log))
    }

    /// Marks the stopped commit resolved and replays the rest: `git add -A`
    /// then `git rebase --continue`. A resolution that leaves the commit
    /// empty drops it, as git does. Stops again at the next conflicting
    /// commit.
    pub async fn continue_rebase(&self, worktree: &Worktree) -> Result<(RebaseStep, StepLog)> {
        self.guard(worktree.path())?;
        let cli = GitCli::new(worktree.path());
        cli.run(["add", "-A"]).await?;
        let mut args = REBASE_CONFIG.to_vec();
        args.extend(["rebase", "--continue"]);
        let output = cli.run_unchecked(&args).await?;
        let log = StepLog::from_output("git add -A && git rebase --continue".to_owned(), &output);
        let step = rebase_step(&cli, output.success()).await?;
        tracing::info!(path = %worktree.path().display(), step = step_name(&step), "continued a rebase");
        Ok((step, log))
    }

    /// The conflict markers left in a worktree: `git diff --cached --check
    /// <start>` after `git add -A` (markers in added lines, `=======`
    /// included), plus a scan of `scan` (the conflicted files) for lines
    /// starting with `<<<<<<<`, `|||||||` or `>>>>>>>`. Whitespace errors
    /// that `--check` also reports are ignored. Sorted, without duplicates.
    pub async fn conflict_markers(
        &self,
        worktree: &Worktree,
        start: &Sha,
        scan: &[PathBuf],
    ) -> Result<Vec<ConflictMarker>> {
        self.guard(worktree.path())?;
        let cli = GitCli::new(worktree.path());
        cli.run(["add", "-A"]).await?;
        let start_hex = start.to_hex();
        let output = cli
            .run_unchecked([
                "-c",
                "core.quotePath=false",
                "diff",
                "--cached",
                "--check",
                "--no-color",
                "--no-ext-diff",
                start_hex.as_str(),
                "--",
            ])
            .await?;
        // Exit 2 reports problems; anything else non-zero is a failure.
        if !output.success() && output.code != Some(2) {
            return Err(GitError::Command {
                command: format!("git diff --cached --check {start_hex}"),
                code: output.code,
                stderr: output.stderr,
            });
        }
        let mut markers = parse_check(&output.stdout_lossy());
        for path in scan {
            let file = worktree.path().join(path);
            // Only regular files inside the worktree; a symlink is not
            // followed out of it.
            let Ok(meta) = tokio::fs::symlink_metadata(&file).await else {
                continue;
            };
            if !meta.is_file() || meta.len() > SCAN_MAX_BYTES {
                continue;
            }
            let bytes = tokio::fs::read(&file)
                .await
                .map_err(|source| io_error(&file, source))?;
            markers.extend(scan_markers(&bytes).into_iter().map(|line| ConflictMarker {
                path: path.clone(),
                line,
            }));
        }
        markers.sort();
        markers.dedup();
        Ok(markers)
    }
}

/// Where a rebase command left the worktree: finished on success, stopped
/// when paths are unmerged, failed otherwise.
async fn rebase_step(cli: &GitCli, success: bool) -> Result<RebaseStep> {
    if success {
        return Ok(RebaseStep::Finished(rev_parse(cli, "HEAD").await?));
    }
    let files = unmerged_paths(cli).await?;
    if files.is_empty() {
        return Ok(RebaseStep::Failed);
    }
    let head = rev_parse(cli, "HEAD").await?;
    let commit = rev_parse(cli, "REBASE_HEAD").await?;
    let commit_hex = commit.to_hex();
    let subject = cli
        .run(["log", "-1", "--format=%s", commit_hex.as_str()])
        .await?
        .stdout_lossy()
        .trim()
        .to_owned();
    Ok(RebaseStep::Stopped(RebaseStop {
        head,
        commit,
        subject,
        files,
    }))
}

async fn rev_parse(cli: &GitCli, name: &str) -> Result<Sha> {
    let spec = format!("{name}^{{commit}}");
    let output = cli.run(["rev-parse", "--verify", spec.as_str()]).await?;
    let hex = output.stdout_lossy();
    Sha::parse(hex.trim()).ok_or_else(|| GitError::Parse {
        command: format!("git rev-parse --verify {spec}"),
        message: format!("not an object id: `{}`", hex.trim()),
    })
}

fn step_name(step: &RebaseStep) -> &'static str {
    match step {
        RebaseStep::Finished(_) => "finished",
        RebaseStep::Stopped(_) => "stopped",
        RebaseStep::Failed => "failed",
    }
}

/// The `<path>:<line>: leftover conflict marker` lines of `git diff
/// --check`.
fn parse_check(output: &str) -> Vec<ConflictMarker> {
    output
        .lines()
        .filter(|line| line.ends_with(": leftover conflict marker"))
        .filter_map(|line| {
            let location = line.strip_suffix(": leftover conflict marker")?;
            let (path, number) = location.rsplit_once(':')?;
            Some(ConflictMarker {
                path: PathBuf::from(path),
                line: number.parse().ok()?,
            })
        })
        .collect()
}

/// The 1-based lines of `bytes` that start a conflict marker: seven `<`,
/// `|` or `>` followed by a space or the end of the line. Binary content
/// (a NUL byte) has none.
fn scan_markers(bytes: &[u8]) -> Vec<u32> {
    if bytes.contains(&0) {
        return Vec::new();
    }
    let mut lines = Vec::new();
    for (index, line) in bytes.split(|&b| b == b'\n').enumerate() {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        let marker = b"<|>".iter().any(|&c| {
            line.len() >= 7
                && line[..7].iter().all(|&b| b == c)
                && line.get(7).is_none_or(|&b| b == b' ')
        });
        if marker {
            lines.push(u32::try_from(index + 1).unwrap_or(u32::MAX));
        }
    }
    lines
}

/// Whether `path` is a plain relative path that stays inside whatever
/// directory it is joined to: no root, no `..`, no `.`.
pub fn is_relative_inside(path: &Path) -> bool {
    path.is_relative()
        && path
            .components()
            .all(|component| matches!(component, std::path::Component::Normal(_)))
}

/// One side of a three-way preview.
#[derive(Clone, Debug, Default)]
pub struct PreviewSide {
    /// Name shown on its conflict marker, e.g. `feature` or `proposal`.
    pub label: String,
    /// The file's content on that side; `None` when the side deleted it
    /// (or never had it).
    pub content: Option<Vec<u8>>,
}

/// The content of `path` (relative to the repository root) at `commit`:
/// `git cat-file blob <commit>:<path>` run in `dir` (any worktree of the
/// repository). `None` when the file does not exist there.
pub async fn blob_at(dir: &Path, commit: &Sha, path: &Path) -> Result<Option<Vec<u8>>> {
    if !is_relative_inside(path) {
        return Ok(None);
    }
    let spec = format!(
        "{}:{}",
        commit.to_hex(),
        path.to_string_lossy().replace('\\', "/")
    );
    let output = GitCli::new(dir)
        .run_unchecked(["cat-file", "blob", spec.as_str()])
        .await?;
    Ok(output.success().then_some(output.stdout))
}

/// `git merge-base <a> <b>`, `None` without a common ancestor.
pub async fn merge_base(dir: &Path, a: &Sha, b: &Sha) -> Result<Option<Sha>> {
    let (a, b) = (a.to_hex(), b.to_hex());
    let output = GitCli::new(dir)
        .run_unchecked(["merge-base", a.as_str(), b.as_str()])
        .await?;
    Ok(output
        .success()
        .then(|| Sha::parse(output.stdout_lossy().trim()))
        .flatten())
}

/// The conflicted content of one file, with markers, as `git merge-file
/// -p` prints it for `ours`, `base` and `theirs`. The three sides are
/// written under `scratch` (a directory of the Argus home, never a
/// repository) and deleted afterwards. When one side deleted the file,
/// the other side's content is shown under a note instead.
pub async fn three_way_preview(
    scratch: &Path,
    ours: &PreviewSide,
    base: &PreviewSide,
    theirs: &PreviewSide,
) -> Result<String> {
    match (&ours.content, &theirs.content) {
        (None, None) => return Ok("[deleted on both sides]\n".to_owned()),
        (None, Some(content)) | (Some(content), None) => {
            let (gone, kept) = if ours.content.is_none() {
                (&ours.label, &theirs.label)
            } else {
                (&theirs.label, &ours.label)
            };
            return Ok(format!(
                "[deleted on `{gone}`, changed on `{kept}`; `{kept}` has:]\n\n{}",
                String::from_utf8_lossy(content)
            ));
        }
        (Some(_), Some(_)) => {}
    }
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let dir = scratch.join(format!(
        "preview-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    tokio::fs::create_dir_all(&dir)
        .await
        .map_err(|source| io_error(&dir, source))?;
    let result = async {
        for (name, side) in [("ours", ours), ("base", base), ("theirs", theirs)] {
            let file = dir.join(name);
            tokio::fs::write(&file, side.content.as_deref().unwrap_or_default())
                .await
                .map_err(|source| io_error(&file, source))?;
        }
        let output = GitCli::new(&dir)
            .run_unchecked([
                "merge-file",
                "-p",
                "-L",
                ours.label.as_str(),
                "-L",
                base.label.as_str(),
                "-L",
                theirs.label.as_str(),
                "ours",
                "base",
                "theirs",
            ])
            .await?;
        // The exit code is the number of conflicts; a negative one (shown
        // as > 127) is an error.
        if output.code.is_none_or(|code| code > 127) {
            return Err(GitError::Command {
                command: "git merge-file -p".to_owned(),
                code: output.code,
                stderr: output.stderr,
            });
        }
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }
    .await;
    if let Err(error) = tokio::fs::remove_dir_all(&dir).await {
        tracing::debug!(dir = %dir.display(), %error, "cannot remove the preview scratch directory");
    }
    result
}
