//! Explanations of a change for the reviewer (the Explain button).
//!
//! An explanation is one `claude -p` call without tools
//! ([`ClaudeCommand::explain`]): Argus gathers what Claude needs (the
//! commits of the diff, the diff itself and, for a line selection, the
//! lines in question) with `git` reads of commits only, and hands it over
//! on stdin. Claude answers in Markdown: what changed, before and after,
//! small steps, plain-text diagrams and what is worth checking.
//!
//! Answers are stored in the `explanations` table, keyed by the merge-base,
//! the head tip and the scope ([`ExplainRequest::key`]): the same diff opens
//! its explanation again at once, and a branch that moves needs a new one.
//!
//! [`RunManager::explain`](crate::RunManager::explain) starts a call and
//! returns an [`ExplainTicket`]; the text arrives as it is written
//! ([`ExplainEvent::Delta`]) when the CLI streams partial messages. Dropping
//! the ticket kills `claude`.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use argus_diff::Side;
use argus_git::GitCli;
use argus_store::{Explanation, ExplanationKey, RepoId, StorageService};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt as _, AsyncReadExt as _, AsyncWriteExt as _, BufReader};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::claude::{ClaudeCapabilities, ClaudeCommand};
use crate::stream::{self, StreamEvent};

/// Largest diff handed to Claude, in bytes; files past it are listed only.
const DIFF_MAX_BYTES: usize = 512 * 1024;
/// Largest diff of one file in a whole-diff explanation, in bytes.
const FILE_DIFF_MAX_BYTES: usize = 64 * 1024;
/// Largest quote of the selected lines, in bytes.
const LINES_MAX_BYTES: usize = 64 * 1024;
/// Largest commit log handed to Claude, in bytes.
const LOG_MAX_BYTES: usize = 16 * 1024;
/// Commits listed at most.
const LOG_MAX_COMMITS: &str = "50";
/// Context lines around the changes of a line selection's file.
const LINES_DIFF_CONTEXT: &str = "-U12";
/// Longest error detail shown, in bytes.
const ERROR_MAX_BYTES: usize = 600;

/// Scope key of a whole-diff explanation.
const DIFF_SCOPE: &str = "diff";

/// What to explain.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ExplainScope {
    /// The whole diff, without the files the reviewer hides.
    Diff {
        /// Paths left out (hidden-file patterns).
        hidden: HashSet<String>,
    },
    /// Lines of one file.
    Lines {
        /// File path in head (the merge-base path for deleted files).
        file: String,
        /// Path in the merge-base when the file was renamed.
        old_file: Option<String>,
        /// Old for removed lines, new otherwise.
        side: Side,
        /// First line, 1-based, on `side`.
        start: u32,
        /// Last line, 1-based and inclusive, on `side`.
        end: u32,
    },
}

impl ExplainScope {
    /// The `explanations.scope` of this scope.
    pub fn key(&self) -> String {
        match self {
            ExplainScope::Diff { .. } => DIFF_SCOPE.to_owned(),
            ExplainScope::Lines {
                file,
                side,
                start,
                end,
                ..
            } => format!("lines:{}:{start}-{end}:{file}", side_name(*side)),
        }
    }
}

/// An explanation to look up or write.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExplainRequest {
    /// The reviewed repository.
    pub repo_id: RepoId,
    /// Its directory; only commits are read from it.
    pub repo_path: PathBuf,
    /// Base branch name, for the prompt.
    pub base: String,
    /// Head branch name, for the prompt.
    pub head: String,
    /// Merge-base SHA: the diff is `merge_base..head_tip`.
    pub merge_base: String,
    /// Head tip SHA.
    pub head_tip: String,
    /// What to explain.
    pub scope: ExplainScope,
}

impl ExplainRequest {
    /// The cache key of the explanation.
    pub fn key(&self) -> ExplanationKey {
        ExplanationKey {
            repo_id: self.repo_id,
            merge_base: self.merge_base.clone(),
            head_tip: self.head_tip.clone(),
            scope: self.scope.key(),
        }
    }
}

/// Progress of an explanation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ExplainEvent {
    /// More of the answer, to append to the text so far.
    Delta(String),
    /// The answer, stored.
    Done(Explanation),
    /// Why there is no explanation.
    Failed(String),
}

/// An explanation being written. Dropping it kills `claude`.
#[derive(Debug)]
pub struct ExplainTicket {
    /// Progress, ending with [`ExplainEvent::Done`] or
    /// [`ExplainEvent::Failed`].
    pub events: mpsc::UnboundedReceiver<ExplainEvent>,
    task: JoinHandle<()>,
}

impl Drop for ExplainTicket {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Everything an explanation call needs.
pub(crate) struct ExplainJob {
    pub store: StorageService,
    pub scratch: PathBuf,
    pub capabilities: ClaudeCapabilities,
    pub request: ExplainRequest,
    pub model: String,
    pub effort: String,
}

/// Starts `job` on the current tokio runtime.
pub(crate) fn start(job: ExplainJob) -> ExplainTicket {
    let (events, receiver) = mpsc::unbounded_channel();
    let task = tokio::spawn(async move {
        let outcome = match explain(&job, &events).await {
            Ok(explanation) => ExplainEvent::Done(explanation),
            Err(error) => {
                tracing::info!(%error, "explanation failed");
                ExplainEvent::Failed(error)
            }
        };
        let _ = events.send(outcome);
    });
    ExplainTicket {
        events: receiver,
        task,
    }
}

/// The stored explanation of `request`, if any.
pub(crate) async fn cached(
    store: &StorageService,
    request: &ExplainRequest,
) -> argus_store::Result<Option<Explanation>> {
    store.explanations().get(request.key()).await
}

async fn explain(
    job: &ExplainJob,
    events: &mpsc::UnboundedSender<ExplainEvent>,
) -> Result<Explanation, String> {
    let request = &job.request;
    let git = GitCli::new(&request.repo_path);
    let context = context(&git, request).await?;
    tokio::fs::create_dir_all(&job.scratch)
        .await
        .map_err(|error| format!("cannot create {}: {error}", job.scratch.display()))?;
    let effort = Some(job.effort.as_str()).filter(|effort| !effort.trim().is_empty());
    let command = ClaudeCommand::explain(context, prompt(request), &job.model, effort)
        .with_cwd(&job.scratch)
        .adapt_to(&job.capabilities);
    let text = run_claude(&command, job.capabilities.path(), events).await?;
    let text = text.trim().to_owned();
    if text.is_empty() {
        return Err("Claude returned an empty explanation".to_owned());
    }
    job.store
        .explanations()
        .put(request.key(), text, job.model.clone(), job.effort.clone())
        .await
        .map_err(|error| format!("cannot save the explanation: {error}"))
}

/// The instructions, passed as the prompt argument; the material is on
/// stdin.
fn prompt(request: &ExplainRequest) -> String {
    let (subject, before_after) = match &request.scope {
        ExplainScope::Diff { .. } => (
            format!(
                "Standard input holds a change under review: the commits and the diff of \
                 branch `{}` against `{}`. Explain the whole change.",
                request.head, request.base
            ),
            "## Before\nHow the code worked before the change, in plain words.\n\n\
             ## After\nHow it works now, and what is different for its users or callers.",
        ),
        ExplainScope::Lines { file, .. } => (
            format!(
                "Standard input holds a change under review (branch `{}` against `{}`): \
                 the lines the reviewer selected in `{file}`, the diff of that file and the \
                 commits. Explain those lines and their part in the change; mention the \
                 rest of the file only where it helps to understand them.",
                request.head, request.base
            ),
            "## Before\nWhat these lines (or the code they replace) did before the change. If \
             they are new, say what was there instead, or that nothing was.\n\n\
             ## After\nWhat they do now. If they did not change, say how the change around \
             them affects them.",
        ),
    };
    format!(
        "You help a code reviewer understand a change quickly, as a patient senior colleague \
         would explain it to someone new to this code. {subject}\n\n\
         Answer in Markdown with exactly these sections, in this order:\n\n\
         ## In short\nTwo or three sentences: what changes and why it matters.\n\n\
         {before_after}\n\n\
         ## Step by step\nA numbered list of baby steps that walk through the change in the \
         order a reader should follow it. One small idea per step; name the files and \
         functions involved.\n\n\
         ## Diagram\nOne or two plain-text diagrams, each in a ```text block, made of boxes \
         and arrows and at most 72 characters wide, showing the flow or the structure before \
         and after.\n\n\
         ## Worth checking\nA short bulleted list of risks, edge cases or open questions for \
         the review. Write \"Nothing stands out.\" when there are none.\n\n\
         Rules: use plain, friendly words and short paragraphs; explain any jargon the first \
         time it appears; put identifiers in `inline code`; quote at most a few lines of \
         code. Do not use tables, HTML or Mermaid: they are not rendered. Do not repeat the \
         diff back. Answer only with the explanation."
    )
}

/// The material on stdin: commits, the selected lines, then the diff.
async fn context(git: &GitCli, request: &ExplainRequest) -> Result<Vec<u8>, String> {
    let range = format!("{}..{}", request.merge_base, request.head_tip);
    let mut out = String::new();
    out.push_str(&format!(
        "Base branch: {} (merge-base {})\nHead branch: {} (tip {})\n\n",
        request.base,
        short(&request.merge_base),
        request.head,
        short(&request.head_tip)
    ));

    let log = git
        .run([
            "log",
            "--no-color",
            "--reverse",
            "--max-count",
            LOG_MAX_COMMITS,
            "--format=* %s%n%b",
            &range,
        ])
        .await
        .map_err(|error| error.to_string())?;
    let log = String::from_utf8_lossy(&log.stdout);
    out.push_str("===== COMMITS (oldest first) =====\n");
    out.push_str(capped(log.trim(), LOG_MAX_BYTES));
    out.push_str("\n\n");

    let diff = match &request.scope {
        ExplainScope::Diff { hidden } => {
            let diff = git_diff(git, request, None, None).await?;
            limit_diff(&diff, hidden)
        }
        ExplainScope::Lines {
            file,
            old_file,
            side,
            start,
            end,
        } => {
            let (sha, path) = match side {
                Side::New => (&request.head_tip, file),
                Side::Old => (&request.merge_base, old_file.as_ref().unwrap_or(file)),
            };
            let lines = selected_lines(git, sha, path, *start, *end).await?;
            out.push_str(&format!(
                "===== SELECTED LINES: {path}, lines {start}-{end} of the {} version =====\n",
                match side {
                    Side::New => "new (head)",
                    Side::Old => "old (merge-base)",
                }
            ));
            out.push_str(&lines);
            out.push_str("\n\n");
            let paths: Vec<&str> = std::iter::once(file.as_str())
                .chain(old_file.as_deref().filter(|old| old != file))
                .collect();
            let diff = git_diff(git, request, Some(&paths), Some(LINES_DIFF_CONTEXT)).await?;
            capped(&diff, DIFF_MAX_BYTES).to_owned()
        }
    };
    out.push_str("===== DIFF =====\n");
    out.push_str(&diff);
    Ok(out.into_bytes())
}

/// `git diff merge_base head_tip`, without external diff or text
/// conversion drivers, with renames.
async fn git_diff(
    git: &GitCli,
    request: &ExplainRequest,
    paths: Option<&[&str]>,
    context: Option<&str>,
) -> Result<String, String> {
    let mut args: Vec<&str> = vec!["diff", "--no-color", "--no-ext-diff", "--no-textconv", "-M"];
    args.extend(context);
    args.extend([request.merge_base.as_str(), request.head_tip.as_str()]);
    if let Some(paths) = paths {
        args.push("--");
        args.extend(paths);
    }
    let output = git.run(args).await.map_err(|error| error.to_string())?;
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Lines `start..=end` of `path` at `sha`, numbered.
async fn selected_lines(
    git: &GitCli,
    sha: &str,
    path: &str,
    start: u32,
    end: u32,
) -> Result<String, String> {
    let output = git
        .run(["show", &format!("{sha}:{path}")])
        .await
        .map_err(|error| error.to_string())?;
    let text = String::from_utf8_lossy(&output.stdout);
    let mut lines = String::new();
    for (index, line) in text.lines().enumerate() {
        let number = index as u32 + 1;
        if number < start {
            continue;
        }
        if number > end || lines.len() > LINES_MAX_BYTES {
            break;
        }
        lines.push_str(&format!("{number:>5} | {line}\n"));
    }
    Ok(lines)
}

/// The diff without the `hidden` files, each file cut at
/// [`FILE_DIFF_MAX_BYTES`] and the whole at [`DIFF_MAX_BYTES`]; files cut
/// off entirely are listed at the end.
fn limit_diff(diff: &str, hidden: &HashSet<String>) -> String {
    let mut out = String::new();
    let mut left_out = Vec::new();
    for section in file_sections(diff) {
        let path = section_path(section);
        if path.is_some_and(|path| hidden.contains(path)) {
            continue;
        }
        if out.len() >= DIFF_MAX_BYTES {
            left_out.push(path.unwrap_or("?").to_owned());
            continue;
        }
        let room = (DIFF_MAX_BYTES - out.len()).min(FILE_DIFF_MAX_BYTES);
        let kept = capped(section, room);
        out.push_str(kept);
        if kept.len() < section.len() {
            out.push_str("\n[diff of this file truncated]\n");
        }
    }
    if !left_out.is_empty() {
        out.push_str("\n[diff too long; these files changed too]\n");
        for path in left_out {
            out.push_str(&path);
            out.push('\n');
        }
    }
    out
}

/// The `diff --git` sections of a diff.
fn file_sections(diff: &str) -> Vec<&str> {
    let mut starts: Vec<usize> = Vec::new();
    let mut offset = 0;
    for line in diff.split_inclusive('\n') {
        if line.starts_with("diff --git ") {
            starts.push(offset);
        }
        offset += line.len();
    }
    if starts.first() != Some(&0) {
        starts.insert(0, 0);
    }
    starts
        .iter()
        .zip(starts.iter().skip(1).chain(std::iter::once(&diff.len())))
        .map(|(&start, &end)| &diff[start..end])
        .filter(|section| !section.is_empty())
        .collect()
}

/// The head path of a `diff --git a/<old> b/<new>` section, from its
/// `+++ b/` line, or its `---` line for a deleted file.
fn section_path(section: &str) -> Option<&str> {
    let mut old = None;
    for line in section.lines().take(12) {
        if let Some(path) = line.strip_prefix("+++ b/") {
            return Some(path);
        }
        if let Some(path) = line.strip_prefix("--- a/") {
            old = Some(path);
        }
        if let Some(path) = line.strip_prefix("rename to ") {
            return Some(path);
        }
    }
    old
}

/// Runs the explanation command, forwarding streamed text to `events`,
/// and returns Claude's answer. Dropping the future kills `claude`.
async fn run_claude(
    command: &ClaudeCommand,
    program: &Path,
    events: &mpsc::UnboundedSender<ExplainEvent>,
) -> Result<String, String> {
    let mut child = command
        .to_command(program)
        .spawn()
        .map_err(|error| format!("cannot start {}: {error}", program.display()))?;
    let input = command.stdin();
    let mut stdin = child.stdin.take();
    let stdout = child.stdout.take();
    let mut stderr = child.stderr.take();
    let write = async move {
        if let Some(stdin) = stdin.as_mut() {
            // Claude may exit before reading everything; its exit status
            // tells what went wrong.
            let _ = stdin.write_all(&input).await;
        }
        drop(stdin);
    };
    let read_err = async move {
        let mut buffer = Vec::new();
        if let Some(stderr) = stderr.as_mut() {
            let _ = stderr.read_to_end(&mut buffer).await;
        }
        buffer
    };
    let read_out = async move {
        let mut streamed = String::new();
        let mut answer = None;
        let mut result = None;
        let Some(stdout) = stdout else {
            return (answer, result);
        };
        let mut lines = BufReader::new(stdout).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            if line.trim().is_empty() {
                continue;
            }
            match stream::parse_line(&line) {
                Ok(StreamEvent::Unknown(value)) => {
                    if let Some(delta) = text_delta(&value, !streamed.is_empty()) {
                        streamed.push_str(&delta);
                        let _ = events.send(ExplainEvent::Delta(delta));
                    }
                }
                Ok(StreamEvent::Assistant(message)) => answer = message.text().or(answer),
                Ok(StreamEvent::Result(last)) => result = Some(last),
                Ok(_) => {}
                Err(error) => tracing::debug!(%error, "skipping a malformed explanation line"),
            }
        }
        (answer, result)
    };
    let ((), (answer, result), err) = tokio::join!(write, read_out, read_err);
    let status = child.wait().await.map_err(|error| error.to_string())?;
    match (result, answer) {
        (Some(result), answer) if status.success() && !result.is_error => result
            .result
            .filter(|text| !text.trim().is_empty())
            .or(answer)
            .ok_or_else(|| "Claude returned no explanation".to_owned()),
        // The turn limit ends the call after Claude answered; the answer
        // stands.
        (Some(result), Some(answer)) if result.subtype == "error_max_turns" => Ok(answer),
        (result, _) => {
            let err = String::from_utf8_lossy(&err);
            let detail = [
                err.trim(),
                result
                    .as_ref()
                    .and_then(|r| r.result.as_deref())
                    .unwrap_or(""),
                result.as_ref().map_or("", |r| r.subtype.as_str()),
            ]
            .into_iter()
            .find(|text| !text.is_empty())
            .unwrap_or("no output");
            Err(format!("{status}: {}", capped(detail, ERROR_MAX_BYTES)))
        }
    }
}

/// The answer text a `stream_event` line adds: a `text_delta`, or the
/// blank line before a new text block when some text came before.
fn text_delta(value: &Value, has_text: bool) -> Option<String> {
    if value.get("type").and_then(Value::as_str) != Some("stream_event") {
        return None;
    }
    let event = value.get("event")?;
    match event.get("type").and_then(Value::as_str)? {
        "content_block_delta" => {
            let delta = event.get("delta")?;
            if delta.get("type").and_then(Value::as_str) != Some("text_delta") {
                return None;
            }
            let text = delta.get("text").and_then(Value::as_str)?;
            (!text.is_empty()).then(|| text.to_owned())
        }
        "content_block_start" => {
            let block = event.get("content_block")?;
            (has_text && block.get("type").and_then(Value::as_str) == Some("text"))
                .then(|| "\n\n".to_owned())
        }
        _ => None,
    }
}

fn side_name(side: Side) -> &'static str {
    match side {
        Side::Old => "old",
        Side::New => "new",
    }
}

fn short(sha: &str) -> &str {
    sha.get(..12).unwrap_or(sha)
}

/// The longest prefix of `text` of at most `max` bytes, on a char boundary.
fn capped(text: &str, max: usize) -> &str {
    if text.len() <= max {
        return text;
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}
