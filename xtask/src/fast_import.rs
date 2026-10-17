//! Builds a repository by streaming commits into `git fast-import`.
//!
//! Every commit has a fixed author, committer and timestamp, and git
//! object IDs depend only on content, so the same stream always yields the
//! same SHAs.

use std::io::{BufWriter, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, Command, Stdio};

use anyhow::{Context as _, bail};

/// The identity of every fixture commit.
const IDENTITY: &str = "Argus Fixtures <fixtures@argus.invalid>";
/// Timestamp of the first fixture commit (2023-11-14T22:13:20Z).
pub const EPOCH: u64 = 1_700_000_000;

/// A change inside a commit.
pub enum Change<'a> {
    /// Write a file with this content (`executable` sets mode 100755).
    Write {
        path: &'a str,
        content: &'a [u8],
        executable: bool,
    },
    /// Delete a file.
    Delete(&'a str),
    /// Rename a file, content unchanged.
    Rename { from: &'a str, to: &'a str },
}

/// A `git fast-import` process writing into one repository.
pub struct FastImport {
    child: Child,
    input: BufWriter<ChildStdin>,
    next_mark: u64,
}

/// A `git` command isolated from the user's and the system's config, so
/// settings such as `init.defaultBranch` or the object format cannot change
/// the fixtures.
pub fn git(repo: &Path) -> Command {
    let mut command = Command::new("git");
    command
        .arg("-C")
        .arg(repo)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", null_device())
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .env_remove("GIT_OBJECT_DIRECTORY");
    command
}

fn null_device() -> &'static str {
    if cfg!(windows) { "NUL" } else { "/dev/null" }
}

/// Runs a git command to completion, failing on a non-zero exit.
pub fn run(mut command: Command) -> anyhow::Result<()> {
    let output = command
        .output()
        .with_context(|| format!("failed to run {command:?}"))?;
    if !output.status.success() {
        bail!(
            "{command:?} failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

/// The output of a git command, trimmed.
pub fn read(mut command: Command) -> anyhow::Result<String> {
    let output = command
        .output()
        .with_context(|| format!("failed to run {command:?}"))?;
    if !output.status.success() {
        bail!(
            "{command:?} failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

impl FastImport {
    /// Initializes an empty repository at `repo` and starts the importer.
    pub fn start(repo: &Path) -> anyhow::Result<Self> {
        std::fs::create_dir_all(repo)
            .with_context(|| format!("failed to create {}", repo.display()))?;
        let mut init = git(repo);
        init.args(["init", "--quiet", "--initial-branch=main"]);
        run(init)?;
        let mut command = git(repo);
        command
            .args(["fast-import", "--quiet", "--done"])
            .stdin(Stdio::piped())
            .stdout(Stdio::null());
        let mut child = command.spawn().context("failed to start git fast-import")?;
        let stdin = child.stdin.take().context("git fast-import has no stdin")?;
        Ok(Self {
            child,
            input: BufWriter::with_capacity(1 << 20, stdin),
            next_mark: 1,
        })
    }

    /// Writes a commit on `branch` (created or moved) and returns its mark.
    /// `parent` is the mark of the parent commit; `None` makes a root
    /// commit. `time` is seconds after [`EPOCH`].
    pub fn commit(
        &mut self,
        branch: &str,
        parent: Option<u64>,
        time: u64,
        message: &str,
        changes: &[Change<'_>],
    ) -> anyhow::Result<u64> {
        let mark = self.next_mark;
        self.next_mark += 1;
        let when = EPOCH + time;
        let out = &mut self.input;
        writeln!(out, "commit refs/heads/{branch}")?;
        writeln!(out, "mark :{mark}")?;
        writeln!(out, "author {IDENTITY} {when} +0000")?;
        writeln!(out, "committer {IDENTITY} {when} +0000")?;
        write_data(out, message.as_bytes())?;
        if let Some(parent) = parent {
            writeln!(out, "from :{parent}")?;
        }
        for change in changes {
            match change {
                Change::Write {
                    path,
                    content,
                    executable,
                } => {
                    let mode = if *executable { "100755" } else { "100644" };
                    writeln!(out, "M {mode} inline {path}")?;
                    write_data(out, content)?;
                }
                Change::Delete(path) => writeln!(out, "D {path}")?,
                Change::Rename { from, to } => writeln!(out, "R {from} {to}")?,
            }
        }
        writeln!(out)?;
        Ok(mark)
    }

    /// Points `branch` at the commit `mark`, without a new commit.
    pub fn branch(&mut self, branch: &str, mark: u64) -> anyhow::Result<()> {
        writeln!(self.input, "reset refs/heads/{branch}")?;
        writeln!(self.input, "from :{mark}")?;
        writeln!(self.input)?;
        Ok(())
    }

    /// Ends the stream and waits for the importer.
    pub fn finish(mut self) -> anyhow::Result<()> {
        writeln!(self.input, "done")?;
        self.input.flush()?;
        drop(self.input);
        let status = self.child.wait().context("git fast-import did not exit")?;
        if !status.success() {
            bail!("git fast-import failed with {status}");
        }
        Ok(())
    }
}

/// A `data` command with its exact byte count.
fn write_data(out: &mut impl Write, bytes: &[u8]) -> std::io::Result<()> {
    writeln!(out, "data {}", bytes.len())?;
    out.write_all(bytes)?;
    writeln!(out)
}
