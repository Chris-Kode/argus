//! Prompt builder for comment, follow-up and resolve runs (spec §6.1, §6.6).
//!
//! The templates live here, in code, and are versioned by
//! [`PROMPT_VERSION`]; every rendered prompt starts with a marker naming the
//! version and the run kind, so a prompt stored in `runs.prompt` can always
//! be traced back to the template that produced it.
//!
//! The prompt is handed to `claude -p` as a single process argument (never
//! through a shell), and user text (comment bodies, replies, conflicted
//! file contents) is embedded verbatim inside Markdown code fences that are
//! always longer than any backtick run in the text, so nothing has to be
//! escaped and nothing can break out of its block.
//!
//! - [`comment_prompt`]: a run for one or more comments ([`CommentContext`]);
//!   the MVP passes exactly one, a numbered list is rendered for more
//!   (batch-ready, spec §3.2).
//! - [`followup_prompt`]: a reply resuming the same Claude session.
//! - [`fresh_followup_prompt`]: a follow-up in a new session, when the
//!   comment's session cannot be resumed; it carries the comment and the
//!   whole thread ([`ThreadMessage`]) instead.
//! - [`resolve_prompt`]: a conflict-resolution run ([`ResolveContext`]);
//!   [`resolve_followup_prompt`]: a reply to the resolution of a pull before
//!   push, which has no comment.

mod excerpt;

use std::fmt::Write as _;

use argus_diff::review::Comment;
use argus_diff::{Side, TextDiff};
use argus_store::{ReplyAuthor, RunKind};

pub use excerpt::{
    DiffExcerpt, EXTRA_CONTEXT_LINES, ExcerptTrim, MAX_EXCERPT_CHARS, TRIMMED_CONTEXT_LINES,
};

/// Version of the prompt templates. Bump it whenever the rendered text
/// changes in a way that matters for auditing or comparing runs.
pub const PROMPT_VERSION: u32 = 2;

/// Scope rule of comment and follow-up runs (spec §6.1).
pub const SCOPE_RULE: &str = "Change only what the comment asks. Touch other files only if needed, and say why in your final message.";

/// Output rule of every run (spec §6.1).
pub const SUMMARY_RULE: &str = "End with a short summary of what you changed.";

/// Rule of resolve runs (spec §6.6).
pub const RESOLVE_RULE: &str =
    "Resolve the conflict markers keeping the intent of both sides; change nothing else.";

/// Keeps Claude's edits uncommitted: Argus diffs the worktree against the
/// start SHA and commits only on Accept.
const NO_COMMIT_RULE: &str =
    "Do not commit, stage, stash or switch branches; leave your edits in the working directory.";

/// Errors of the prompt builder.
#[derive(Debug, thiserror::Error)]
pub enum PromptError {
    /// [`comment_prompt`] was called without any comment.
    #[error("a comment run needs at least one comment")]
    NoComments,
}

/// A rendered prompt, ready for `claude -p` and for `runs.prompt`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Prompt {
    /// The kind of run the prompt is for.
    pub kind: RunKind,
    /// [`PROMPT_VERSION`] at render time.
    pub version: u32,
    /// The prompt text, passed verbatim as one argument.
    pub text: String,
}

/// The repository and branches a run works on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RepoContext {
    /// Display name of the repository.
    pub repo_name: String,
    /// Base branch of the review.
    pub base_branch: String,
    /// Head branch of the review (the one the change lands on).
    pub head_branch: String,
    /// Head SHA the run's worktree is detached at.
    pub start_sha: String,
}

/// One comment as the prompt needs it: where it points, the diff around
/// it and its text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommentContext {
    /// Path on the new side (the merge-base path for deleted files).
    pub file_path: String,
    /// Path on the old side, when the file was renamed.
    pub old_file_path: Option<String>,
    /// Side the line numbers refer to.
    pub side: Side,
    /// First commented line, 1-based.
    pub start_line: u32,
    /// Last commented line, 1-based, inclusive.
    pub end_line: u32,
    /// The diff around the commented lines.
    pub excerpt: DiffExcerpt,
    /// The comment's Markdown body, verbatim.
    pub body: String,
}

impl CommentContext {
    /// Builds the context of `comment` from the current line diff of its
    /// file. Falls back to the snippet saved with the comment when there is
    /// no line diff (binary or too-large file) or the lines are gone.
    ///
    /// This walks the whole file diff: call it off the UI thread.
    pub fn from_comment(comment: &Comment, diff: Option<&TextDiff>) -> Self {
        let anchor = &comment.anchor;
        let excerpt = diff
            .and_then(|diff| DiffExcerpt::from_diff(diff, anchor.side, anchor.lines()))
            .unwrap_or_else(|| {
                DiffExcerpt::from_snippet(
                    &anchor.snippet,
                    anchor.snippet_start_line(),
                    anchor.side,
                    anchor.lines(),
                )
            });
        Self {
            file_path: anchor.file_path.clone(),
            old_file_path: anchor.old_file_path.clone(),
            side: anchor.side,
            start_line: anchor.start_line,
            end_line: anchor.end_line,
            excerpt,
            body: comment.body_md.clone(),
        }
    }
}

/// The git operation that hit the conflict a resolve run fixes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConflictOperation {
    /// Accepting a comment's proposal onto the head branch.
    Accept {
        /// How to name the comment, e.g. its first line.
        comment_label: String,
    },
    /// Rebasing the head branch's local commits onto its upstream before a
    /// push.
    Rebase {
        /// The upstream ref, e.g. `origin/feature`.
        upstream: String,
        /// The local commit being replayed when the rebase stopped
        /// (`<short-sha> <subject>`), when known.
        stopped_at: Option<String>,
    },
}

/// A file left with conflict markers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConflictedFile {
    /// Path relative to the worktree root.
    pub path: String,
    /// The file content, conflict markers included.
    pub content: String,
}

/// Input of a resolve run (spec §6.6).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolveContext {
    /// What failed.
    pub operation: ConflictOperation,
    /// The conflicted files, with their markers.
    pub conflicted_files: Vec<ConflictedFile>,
    /// The comment whose change conflicted, if any.
    pub original_comment: Option<CommentContext>,
}

/// Renders the prompt of a comment run. One comment gives the MVP prompt;
/// more give a numbered list, each comment with its own location, diff and
/// text.
pub fn comment_prompt(
    repo: &RepoContext,
    comments: &[CommentContext],
) -> Result<Prompt, PromptError> {
    let mut out = String::new();
    marker(&mut out, RunKind::Comment);
    repo_intro(&mut out, repo);
    match comments {
        [] => return Err(PromptError::NoComments),
        [comment] => {
            out.push_str(
                "A reviewer left a comment on this branch. Your task is to apply it as a code \
                 change in this working directory.\n",
            );
            comment_section(&mut out, comment, "##");
        }
        many => {
            let _ = writeln!(
                out,
                "A reviewer left {} comments on this branch. Your task is to apply each of them \
                 as a code change in this working directory. They are numbered; address every \
                 one, in order.",
                many.len()
            );
            for (index, comment) in many.iter().enumerate() {
                let _ = write!(out, "\n## Comment {} of {}\n", index + 1, many.len());
                comment_section(&mut out, comment, "###");
            }
        }
    }
    let scope_rule = if comments.len() > 1 {
        "Change only what the comments ask. Touch other files only if needed, and say why in your final message."
    } else {
        SCOPE_RULE
    };
    let summary_rule = if comments.len() > 1 {
        "End with a short summary of what you changed, one numbered item per comment."
    } else {
        SUMMARY_RULE
    };
    rules(&mut out, &[scope_rule, NO_COMMIT_RULE, summary_rule]);
    Ok(Prompt {
        kind: RunKind::Comment,
        version: PROMPT_VERSION,
        text: out,
    })
}

/// Renders the prompt of a follow-up run: the reviewer's reply to the
/// previous proposal, which is already applied in the worktree. The run
/// resumes the same Claude session, so the original comment is not
/// repeated.
pub fn followup_prompt(reply: &str) -> Prompt {
    let mut out = String::new();
    marker(&mut out, RunKind::Followup);
    out.push_str(
        "Your previous change is already applied in this working directory. The reviewer \
         read it and replied on the same comment. Your task is to update the change as the \
         reply asks, starting from the current state of the files.\n",
    );
    out.push_str("\n## Reply (verbatim)\n\n");
    fenced(&mut out, "markdown", reply);
    rules(&mut out, &[SCOPE_RULE, NO_COMMIT_RULE, SUMMARY_RULE]);
    Prompt {
        kind: RunKind::Followup,
        version: PROMPT_VERSION,
        text: out,
    }
}

/// Renders the prompt of a follow-up on a conflict resolution that belongs
/// to no comment (a pull-before-push rebase): the reviewer's reply to the
/// resolution already in the worktree. The run resumes the resolve run's
/// session.
pub fn resolve_followup_prompt(reply: &str) -> Prompt {
    let mut out = String::new();
    marker(&mut out, RunKind::Followup);
    out.push_str(
        "Your conflict resolution is already applied in this working directory. The reviewer \
         read it and replied. Your task is to update the resolution as the reply asks, \
         starting from the current state of the files.\n",
    );
    out.push_str("\n## Reply (verbatim)\n\n");
    fenced(&mut out, "markdown", reply);
    rules(
        &mut out,
        &[
            RESOLVE_RULE,
            "Leave no conflict marker in any file.",
            NO_COMMIT_RULE,
            SUMMARY_RULE,
        ],
    );
    Prompt {
        kind: RunKind::Followup,
        version: PROMPT_VERSION,
        text: out,
    }
}

/// One message of a comment thread, oldest first, for a follow-up that
/// cannot resume the thread's session.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ThreadMessage {
    /// Who wrote it.
    pub author: ReplyAuthor,
    /// The Markdown text, verbatim.
    pub body: String,
}

/// Renders the prompt of a follow-up started in a new session because the
/// comment's session could not be resumed: the comment, the thread so far
/// and the reviewer's latest reply as the task. The previous change is
/// still applied in the worktree.
///
/// The task is the last reviewer message of `thread`; without one, the
/// comment itself.
pub fn fresh_followup_prompt(
    repo: &RepoContext,
    comment: &CommentContext,
    thread: &[ThreadMessage],
) -> Prompt {
    let mut out = String::new();
    marker(&mut out, RunKind::Followup);
    repo_intro(&mut out, repo);
    out.push_str(
        "A reviewer left a comment on this branch, and an earlier Claude session made a change \
         for it. That change is already applied in this working directory, but the session \
         itself is no longer available, so the comment and the conversation so far are \
         repeated below.\n",
    );
    comment_section(&mut out, comment, "##");
    let task = thread
        .iter()
        .rposition(|message| message.author == ReplyAuthor::User);
    if !thread.is_empty() {
        out.push_str("\n## Conversation so far (oldest first)\n");
        for (index, message) in thread.iter().enumerate() {
            let who = match message.author {
                ReplyAuthor::User => "Reviewer",
                ReplyAuthor::Claude => "Claude (earlier session)",
            };
            let latest = if Some(index) == task {
                ", latest reply"
            } else {
                ""
            };
            let _ = write!(out, "\n### {}. {who}{latest}\n\n", index + 1);
            fenced(&mut out, "markdown", &message.body);
        }
    }
    match task {
        Some(index) => {
            let _ = writeln!(
                out,
                "\nYour task is to update the change as the reviewer's latest reply (message {}) \
                 asks, starting from the current state of the files.",
                index + 1
            );
        }
        None => out.push_str(
            "\nYour task is to finish applying the comment, starting from the current state of \
             the files.\n",
        ),
    }
    rules(&mut out, &[SCOPE_RULE, NO_COMMIT_RULE, SUMMARY_RULE]);
    Prompt {
        kind: RunKind::Followup,
        version: PROMPT_VERSION,
        text: out,
    }
}

/// Renders the prompt of a resolve run: the conflicted files with their
/// markers, the operation that failed and the original comment, if any.
pub fn resolve_prompt(repo: &RepoContext, context: &ResolveContext) -> Prompt {
    let mut out = String::new();
    marker(&mut out, RunKind::Resolve);
    repo_intro(&mut out, repo);
    let head = code(&repo.head_branch);
    match &context.operation {
        ConflictOperation::Accept { comment_label } => {
            let _ = writeln!(
                out,
                "Applying the accepted change for the review comment {} onto the tip of {head} \
                 hit conflicts. In the conflict markers, the first side (`<<<<<<<` to \
                 `=======`) is the current code of {head}; the second side (`=======` to \
                 `>>>>>>>`) is the change made for the comment.",
                quoted(comment_label),
            );
        }
        ConflictOperation::Rebase {
            upstream,
            stopped_at,
        } => {
            let _ = writeln!(
                out,
                "Rebasing the local commits of {head} onto its upstream {} (a pull before \
                 push) hit conflicts. In the conflict markers, the first side (`<<<<<<<` to \
                 `=======`) is the upstream code; the second side (`=======` to `>>>>>>>`) is \
                 the local commit being replayed.",
                code(upstream),
            );
            if let Some(commit) = stopped_at {
                let _ = writeln!(out, "The rebase stopped at local commit {}.", code(commit));
            }
        }
    }
    out.push_str(
        "Your task is to resolve these conflicts in this working directory, so the result \
         keeps what both sides meant to do.\n",
    );
    if let Some(comment) = &context.original_comment {
        out.push_str("\n## Original comment\n\n");
        out.push_str("The conflicting change was made for this review comment.\n");
        comment_section(&mut out, comment, "###");
    }
    let _ = write!(
        out,
        "\n## Conflicted files ({})\n",
        context.conflicted_files.len()
    );
    for file in &context.conflicted_files {
        let _ = write!(out, "\n### {}\n\n", code(&file.path));
        fenced(&mut out, "text", &file.content);
    }
    rules(
        &mut out,
        &[
            RESOLVE_RULE,
            "Remove every conflict marker from the files listed above.",
            NO_COMMIT_RULE,
            SUMMARY_RULE,
        ],
    );
    Prompt {
        kind: RunKind::Resolve,
        version: PROMPT_VERSION,
        text: out,
    }
}

fn marker(out: &mut String, kind: RunKind) {
    let kind = match kind {
        RunKind::Comment => "comment",
        RunKind::Followup => "followup",
        RunKind::Resolve => "resolve",
    };
    let _ = writeln!(out, "<!-- argus prompt v{PROMPT_VERSION} ({kind}) -->\n");
}

fn repo_intro(out: &mut String, repo: &RepoContext) {
    let _ = writeln!(
        out,
        "You are working in a git worktree of the repository {}, detached at commit {}, the \
         tip of the branch {}. The branch is under review against the base branch {}; diffs \
         below compare the merge-base of the two branches (old side) with {} (new side).\n",
        code(&repo.repo_name),
        code(&repo.start_sha),
        code(&repo.head_branch),
        code(&repo.base_branch),
        code(&repo.head_branch),
    );
}

fn comment_section(out: &mut String, comment: &CommentContext, level: &str) {
    let lines = if comment.start_line == comment.end_line {
        format!("line {}", comment.start_line)
    } else {
        format!("lines {}-{}", comment.start_line, comment.end_line)
    };
    let _ = write!(out, "\n{level} Location\n\n");
    let _ = writeln!(out, "- File: {}", code(&comment.file_path));
    if let Some(old) = &comment.old_file_path {
        let _ = writeln!(out, "- Renamed from: {}", code(old));
    }
    match comment.side {
        Side::New => {
            let _ = writeln!(
                out,
                "- Commented: {lines} on the new side, i.e. the code as it is in this working \
                 directory."
            );
        }
        Side::Old => {
            let path = comment
                .old_file_path
                .as_deref()
                .unwrap_or(&comment.file_path);
            let _ = writeln!(
                out,
                "- Commented: {lines} on the old side of {}.\n\nThese lines are code that this \
                 branch removed: they exist at the merge-base but not in this working \
                 directory. The comment is about that removed code (for example, it was \
                 still needed or its removal broke something). Their line numbers refer to \
                 the old file, not to the current one.",
                code(path),
            );
        }
    }

    let _ = write!(out, "\n{level} Diff around the comment\n\n");
    match comment.excerpt.trimmed {
        Some(ExcerptTrim::SnippetOnly) => out.push_str(
            "No line diff is available for this file, so this is the code saved when the \
             comment was written. Rows starting with `>` are the commented lines.\n\n",
        ),
        trimmed => {
            out.push_str(
                "Each row shows the old line number, the new line number, the change (`-` \
                 removed, `+` added, space unchanged) and the code. Rows starting with `>` are \
                 the commented lines.",
            );
            if let Some(ExcerptTrim::Oversized { original_chars }) = trimmed {
                let _ = write!(
                    out,
                    " The full hunk context was too large ({original_chars} characters), so it \
                     was trimmed to the commented lines plus {TRIMMED_CONTEXT_LINES} lines \
                     around them; read the file for more."
                );
            }
            out.push_str("\n\n");
        }
    }
    fenced(out, "diff", &comment.excerpt.text);

    let _ = write!(out, "\n{level} Comment (verbatim)\n\n");
    fenced(out, "markdown", &comment.body);
}

fn rules(out: &mut String, rules: &[&str]) {
    out.push_str("\n## Rules\n\n");
    for rule in rules {
        let _ = writeln!(out, "- {rule}");
    }
}

/// Appends `content` in a fenced block whose fence is longer than any
/// backtick run inside it, so the content needs no escaping.
fn fenced(out: &mut String, info: &str, content: &str) {
    let fence = "`".repeat(longest_backtick_run(content).max(2) + 1);
    let _ = writeln!(out, "{fence}{info}");
    out.push_str(content);
    if !content.ends_with('\n') {
        out.push('\n');
    }
    let _ = writeln!(out, "{fence}");
}

/// Inline code span that survives backticks in `text`.
fn code(text: &str) -> String {
    let ticks = "`".repeat(longest_backtick_run(text) + 1);
    let pad = if text.starts_with('`') || text.ends_with('`') {
        " "
    } else {
        ""
    };
    format!("{ticks}{pad}{text}{pad}{ticks}")
}

fn quoted(text: &str) -> String {
    format!("\"{text}\"")
}

fn longest_backtick_run(text: &str) -> usize {
    text.split(|c| c != '`').map(str::len).max().unwrap_or(0)
}
