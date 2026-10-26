# Argus guide

Everything Argus does, in detail. For installing and a first review, see the [README](../README.md).

## The diff

Once base and head are picked, the center shows the three-dot diff: every changed
file in one virtualized list, laid out lazily as it scrolls into view. `j`/`k`
jump between files, `n`/`p` between hunks, and `alt-z` toggles soft wrap. Click
or drag the gutter to select lines, and use the "Expand" rows to reveal context.

The diff picks its layout from the room it has: side by side (old lines on the
left, new lines on the right, each column with its own gutter; the left one
selects old lines, the right one new lines) when it is at least 1100 px wide,
unified otherwise, so showing a dock or narrowing the window never cuts every
line. `cmd-shift-d` (`ctrl-shift-d` on Linux) or the status bar's **Split**
switches between the two and keeps that choice, keeping the line at the top of
the viewport in place; **Wrap** next to it toggles soft wrap. The settings
choose the layout (Auto, Side by side or Unified) and whether both columns
scroll horizontally together. The title bar shows how many files changed and
the lines added and removed.

## Hidden files

Hidden files keep noise out of a review: generated code, snapshots, lock
files, tests. Add a pattern in **Settings → Hidden files**, press Enter, and the
matching files leave both the file list and the diff. A pattern without slashes
around it is plain text that must appear in the file name (`.test` hides
`cache.test.ts`; with a `/` in it, it must appear in the path: `vendor/`). A
pattern between slashes is a regular expression searched in the whole path
(`/\.snap$/`, `/^docs/.*\.md$/`). The title bar counts the hidden files; click
it to edit the patterns. Patterns apply to every repository. Hiding never
drops comments (they keep following their code) and a proposal always shows
everything Claude changed.

The file list shows a filter box when it holds more than 15 files; on a shorter
list, `/` brings it up. **Settings → Layout → File list** switches between the
tree and a flat list (`cmd-shift-l`).

## Comments

Comment on any line, or a range, on either side: hover a line and click the
gutter "+", or select lines (click, shift-click or drag the gutter) and press
`c`. Removed lines are commented on the old side; added and context lines on the
new side (in side-by-side mode, the column you pick decides). A range cannot
cross a hunk header. To comment on a whole file instead (say, "split this into
smaller modules" or "delete this file"), click the comment button in its
header; binary and too-large files take file comments too. The composer opens
under the last line (a file comment's at the top of the file): write Markdown,
save with `cmd-enter` (`ctrl-enter` on Linux), cancel with `esc`; unsaved
drafts are kept while Argus runs. Threads show inline under their lines with a
status chip (Open, Running, Proposed, Resolved, Dismissed), replies and a menu
to edit, delete, dismiss or reopen. `]c` / `[c` jump between threads, `shift-c`
folds them all, and the "Comments" tab of the left dock (`cmd-shift-c`) lists
every thread by file, filterable by status. File-tree rows show whether Claude
is working on a file or has a proposal in it. A thread shows its status chip
only when it says something (Proposed, Resolved, Dismissed); before Claude
answers, the comment itself is the ask, so the thread offers a quiet **Reply**
instead of an input. Closed threads (Resolved after Accept, or Dismissed) leave
the diff and the Comments tab, so both show only what still needs you: the
status bar's **N closed** puts them back inline, and the Resolved and Dismissed
chips list them again. Debug builds add "Advance status (debug)" to the thread menu, to walk
the status chips by hand.

Comments follow their code. Every time the diff is refreshed (branch change,
Fetch, Accept), each comment looks for its lines and their context in the new
diff: unchanged, moved (lines inserted above, or the file renamed), or gone.
A file comment follows its file, through renames, and is gone only when the
file leaves the diff.
A commit that edits the lines around a comment does not lose it: the commented
lines are found again by themselves, and lines edited in place are found
between their unchanged neighbours. When the code is really gone, the comment
becomes **outdated**. Outdated comments are not shown inline. They are listed
in the "Outdated" group of the file tree and the Comments tab, which shows the
original code at its original line numbers. From there, **Re-attach…** scrolls
the diff to where the code was and selects those lines, so Save re-attaches it
there (or pick other lines: gutter "+", `c` on a selection, or a file header's
comment button to re-attach it to a whole file), and **Dismiss**
closes the comment. Outdated comments cannot be run. A comment that finds its
code again (for example after a revert) is no longer outdated. Resolved
comments are moved too, but are never flagged outdated, and neither are
running or proposed ones while their file is still in the diff: they keep
their place, with the run's progress and the proposal's Accept.

## Runs

Run a comment with Claude from its thread: the "Run ▾" split button runs with
the default model and effort (its tooltip names them), and its arrow opens the
Run menu with every model and effort on offer. `r` runs the focused
thread. The last choice becomes the default for the next run and survives a
restart. Run is disabled for outdated comments and while Claude Code is
missing or too old. Each run gets its own worktree, and runs go in parallel.

A run has everything a normal Claude Code session has: every tool, your
settings and `CLAUDE.md`, MCP servers, skills and plugins, and your shell
environment (minus `ANTHROPIC_*`). It works in Claude Code's `auto` permission
mode (`acceptEdits` on a CLI without it). Tool calls the mode still asks
about are answered by Argus when they are routine for a run in its own
worktree: reading, searching and editing the worktree's files, building,
testing, linting and formatting with the usual toolchains (`cargo`, `npm`,
`yarn`, `pnpm`, `go`, `pytest`, `make`, ...), and local `git` that only reads
or stages. Anything else is asked: writes outside the worktree, `sudo`,
`curl`, `ssh`, shell expansions, `rm` on the worktree itself, `git` that
moves branches or talks to a remote, installs to the system, publishing, MCP
tools. **Settings → Claude → Permissions → Ask every time** turns this off.
When Claude needs you, for a risky tool call or a question it asks with
`AskUserQuestion`, the run waits and a dialog opens. Answer a question by picking options or
typing your own. For a tool call, choose **Allow**, **Allow for this run** or
**Deny**, with an optional note telling Claude what to do instead. **Later**
(`esc`) hides the dialog without answering; the run keeps waiting, and the
status bar's "waiting for you" counter brings the dialog back. Whatever the
mode, a run can never `git commit`, `git push`, or edit files in your own
checkout.

## The run panel

The run panel (`cmd-j`, docked right or bottom with `cmd-shift-j`) is hidden
until you want it: threads already show a run's progress and its proposal, and
a thread's *Running…* or Claude's **What Claude did** opens the panel on that
run's log. It lists the repository's active runs first, then the ones waiting
for you: a proposal to review, or a failure on a comment that is still open.
Finished runs (accepted, rejected, dismissed, stopped, empty, or followed by a
newer run of the same comment) fold behind the header's **Show finished**. Each
row shows the comment, `file:line`, status, the elapsed time while it runs and
a Stop button; the model, effort, turns, cost and duration are in the run's
log. Clicking a row jumps to its comment and shows the run's live log: assistant text as
Markdown, tool calls as compact rows (`Edit src/foo.rs (+3 −1)`,
`Bash git diff`) that expand to their input and output, errors in red, and the
final summary. The log follows new output until you scroll up. "View prompt"
shows what Claude was sent, and "Finder"/"Files" and "Terminal" open the run's
worktree. The status bar shows how many runs are active.

## Proposals and follow-ups

When a run finishes with a change, its comment turns Proposed. "View proposal"
(on the thread, or in its menu) opens a proposal tab next to the diff tab. The
tab shows only what Claude changed: the diff from the commit the run started at
to its worktree, including new and deleted files, in the same viewer. Above the
diff are the comment (its location links back to the thread) and Claude's final
summary; the model, effort, turns, cost and duration are in the run's log. Files other than the
commented one are listed first and badged "outside comment scope". A banner
warns when the head branch moved after the run started. `a` accepts the
proposal, `x` rejects it (press it again to confirm), and `r` opens the reply
box. Reject deletes the run's worktree and sets the comment back to Open; the
run stays in history as Rejected. "Open in editor" opens a changed file of the
worktree in `$VISUAL` / `$EDITOR`, or with the system's default application.
The proposal refreshes as soon as the file changes on disk.

A reply on a Proposed comment, in the proposal tab or in the thread, has two
send options. **Send as follow-up run** (the default, also on Enter) starts a
follow-up: it resumes the comment's Claude session (`--resume <session>`) in
the same worktree, so Claude continues from its earlier edits. **Add note
only** just saves the reply. A follow-up uses the Run menu's model and effort.
On a Proposed thread the Run split button reads "Follow up · model · effort",
and its menu picks them. In the proposal tab, the model and effort buttons of
the reply box change the Run menu defaults. The model and effort can change
between follow-ups; the session stays the same. While the
follow-up runs, the comment is Running and the proposal's diff follows its
edits. When it finishes, the comment is Proposed again, with the combined
change of every run since the first one. Claude's final message of each run
joins the thread as a Claude reply labelled with that run's model and effort,
so the thread reads as a conversation. Once a proposal is rejected, its
worktree is gone and follow-ups are no longer offered; start a new run instead.
If Claude cannot find the session to resume, the run fails with a clear message
and a **Run fresh with the thread as context** button. That button starts a new
session whose prompt carries the comment and every reply.

## Settings

The settings sheet (`cmd-,`) groups the Claude run defaults, the diff options,
the appearance and the dock layout, each section with a "Reset to defaults"
button. The Claude section covers:

- the `claude` binary (empty means auto-detect on `PATH`);
- the default model and effort, and the lists the Run menu offers;
- the review rules file;
- the commit-message timeout.

Text fields apply on Enter. An invalid value, such as a Claude path that does
not exist or is not executable, shows an inline error and is not saved. Valid
changes apply to the next run without a restart. On first launch Argus writes
default rules to `~/.argus/review-rules.md`. It never overwrites that file, and
every run appends it to Claude's system prompt. "Open in editor" opens it with
the system's default application.

The Appearance section picks the UI style and the theme. **Zed** uses One Dark
and One Light with rounded controls. **VS Code** uses Dark Modern and Light
Modern with Dark+ / Light+ syntax colors, square controls, an activity bar
(Files, Comments, Settings) and a centered title. Either style follows the
system appearance or stays dark or light.

![Settings](screenshots/settings-light.png)

## Syntax highlighting

Code is syntax-highlighted with tree-sitter, Zed-style, for Rust,
TypeScript/TSX, JavaScript/JSX, Python, Go, Java, Kotlin, Swift, C, C++, C#,
Ruby, PHP, JSON, YAML, TOML, Markdown, HTML, CSS, SQL and Bash (detected from the
file name, extension or shebang; anything else stays plain text). Highlighting
runs on background threads and parses each whole file once, so a block comment
that opens above a hunk is still colored inside it; text is shown plain right
away and recolored when the highlights arrive. Files over 2 MB or 50,000 lines
stay plain unless you click "Highlight" in their header.

![Syntax highlighting in One Light](screenshots/diff-syntax-light.png)
![Syntax highlighting in One Dark](screenshots/diff-syntax-dark.png)
![Side-by-side diff view](screenshots/diff-split-light.png)
![Unified diff view](screenshots/diff-unified-light.png)
![Argus in One Dark](screenshots/shell-dark.png)
![Argus in One Light](screenshots/shell-light.png)

## Where data lives

Everything Argus writes lives in `~/.argus/`, never inside a reviewed repository:

| Path | What |
| --- | --- |
| `~/.argus/argus.db` | SQLite database: repositories, comments, runs, accepts, settings |
| `~/.argus/review-rules.md` | Rules appended to every run's system prompt; edit freely, never overwritten |
| `~/.argus/worktrees/` | One git worktree per run, plus each repository's `_integration` worktree for Accept and Push |
| `~/.argus/logs/argus.log` | Log file |
| `~/.argus/updates/` | An update while it downloads |

Installing an update is the one write outside it: the new `Argus.app` replaces
the old one in its folder.

Set `ARGUS_HOME` to use another directory, e.g. `ARGUS_HOME=/tmp/argus-dev`.
Deleting `~/.argus` resets Argus; your repositories are not affected, though
`git worktree prune` in each one cleans up the worktree entries left behind.

## Keyboard shortcuts

`⌘/` lists every shortcut in the app. On Linux, `⌘` is `ctrl`. Single-key
shortcuts work whenever the focus is not in a text field.

| Keys | Action |
| --- | --- |
| `⌘O` | Open a repository |
| `⌘,` | Settings |
| `⌘/` | Keyboard shortcuts |
| `j` / `k` | Next / previous file |
| `n` / `p` | Next / previous hunk |
| `/` | Filter the files |
| `c` | Comment on the selected line(s) |
| `⌘↩` / `esc` | Save / cancel the comment |
| `]c` / `[c` | Next / previous comment |
| `⇧C` | Collapse / expand all comments |
| `r` | Run the comment with Claude (in a proposal: reply) |
| `a` | Accept the proposal |
| `x` | Reject the proposal (press again to confirm) |
| `⌘B` | Toggle the file tree |
| `⌘⇧C` | Show the files / the comment list |
| `⌘⇧L` | Toggle tree / flat file list |
| `⌘J` | Toggle the run panel |
| `⌘⇧J` | Dock the run panel right / bottom |
| `⌘.` | Focus on the diff (hide both docks, press again to bring them back) |
| `⌘⇧D` | Toggle unified / side-by-side diff |
| `⌥Z` | Toggle soft wrap |
| `⌘K ⌘T` | Cycle theme (system, dark, light) |
| `⌘=` / `⌘-` / `⌘0` | Larger / smaller / reset font size |
| `⌘Q` | Quit |

## Safety rules

These hold for every run, Accept and Push (spec §11):

1. Claude never edits your working checkout: runs happen in worktrees under
   `~/.argus/worktrees/`.
2. Nothing is committed until you click Accept; Reject deletes the worktree.
3. Commits stay local; only Push sends them, after a pull with rebase. Argus never
   force-pushes.
4. Accepts are serialized per repository with compare-and-swap ref updates;
   conflicts open the conflict dialog and never fail silently.
5. Argus never reads or modifies uncommitted changes, and stores no API keys.
