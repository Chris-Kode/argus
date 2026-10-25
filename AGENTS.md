# AGENTS.md

Instructions for AI coding agents in this repository. Claude Code reads
[CLAUDE.md](CLAUDE.md), which points here; treat this file as the source of
truth.

## What this is

Argus is a native desktop code-review tool: Rust + GPUI (Zed-style), one binary
that shows a three-dot git diff, lets the user comment on lines, and turns each
comment into a headless `claude -p` run in its own git worktree. Nothing lands
on a branch until the user accepts it.


## Hard rules

### 1. Do not write tests

This project deliberately has no tests. Never add:

- `#[test]` functions, `#[cfg(test)]` modules, or `tests/` directories;
- benches, or test-only fixtures and helpers.

Many issues list unit or integration tests in their acceptance criteria. Skip
those items and say so in your summary. `cargo test --workspace` must pass by
having nothing to run.

### 2. Do not touch the pins

- `gpui` / `gpui_platform` are pinned to one Zed rev in the root `Cargo.toml`.
- `rust-toolchain.toml` pins the Rust toolchain.

Bump them only when a task explicitly asks for it, both together, following the
procedure in [CONTRIBUTING.md](CONTRIBUTING.md).

### 3. Keep the UI thread free

The UI thread never performs git I/O, diff computation, syntax highlighting,
SQLite writes, or process I/O (spec §4.2). Services expose async APIs and push
results into GPUI entities. Every long operation must be cancellable.

### 4. Keep the binary thin

`crates/argus` only bootstraps (logging, tokio runtime, GPUI window). Real code
belongs in the library crates so it can be exercised without a window.

### 5. User data lives in `~/.argus/`

Never write inside a reviewed repository. Honor `ARGUS_HOME` for anything that
resolves a user-data path, and use it in manual checks:

```sh
ARGUS_HOME=/tmp/argus-dev cargo run -p argus
```

## Commands

| Task | Command |
| --- | --- |
| Build | `cargo build --workspace` |
| Run | `cargo run -p argus` |
| Format check | `cargo fmt --check` |
| Format | `cargo fmt` |
| Lint | `cargo clippy --workspace --all-targets -- -D warnings` |
| Tests | `cargo test --workspace` (expected to run 0 tests) |
| Perf fixtures | `cargo xtask fixtures` (into `target/fixtures/`) |
| Perf report | `ARGUS_PERF=1 cargo run --release -- --repo target/fixtures/huge-files --base base --head head --auto-scroll` |

Clippy must be clean with `-D warnings`.

## Crate layout

| Crate | Responsibility |
| --- | --- |
| `crates/argus` | Binary: bootstrap, logging, tokio runtime, window |
| `crates/argus_ui` | GPUI views, theme, `AppContext` global with service handles |
| `crates/argus_git` | `gix` reads/diffs; `git` CLI for worktrees, commit, rebase, push |
| `crates/argus_diff` | Diff model, hunks, line mapping, anchoring |
| `crates/argus_highlight` | Tree-sitter grammars and the background highlighter |
| `crates/argus_runs` | Prompt builder, run manager, `stream-json` parser |
| `crates/argus_store` | SQLite schema, migrations, repositories |
| `crates/argus_update` | Release checks and in-place self-update of `Argus.app` |
| `xtask` | Dev tasks (`cargo xtask fixtures`); not part of the app |

`AppContext` (in `argus_ui`) is the GPUI global holding the git, store and run
service handles plus the tokio runtime handle.

## Safety invariants (spec §11 — must hold in every change)

1. Claude never edits the user's working checkout; runs happen in worktrees
   under `~/.argus/worktrees/`.
2. Nothing is committed until the user clicks Accept; Reject deletes the
   worktree.
3. Commits stay local; only Push sends them, after a pull with rebase. Never
   force-push.
4. Accepts are serialized per repo with compare-and-swap ref updates; conflicts
   open the conflict dialog, never fail silently.
5. Argus never reads or modifies uncommitted changes, and stores no API keys.

## UI copy

Avoid unnecessary secondary labels and descriptions. Let the layout do the
explaining.

- No taglines, subtitles, eyebrow labels or helper text under a control when
  its position, title, icon or placeholder already says what it is.
- Section headers only where the grouping is not already obvious; counts and
  status are shown once, in one place.
- Keep text that tells the user something they cannot see: errors, why an
  action is disabled (in its tooltip), and what an action will do when that is
  not obvious (e.g. a confirm prompt).

## Style

- Rustfmt defaults, edition 2024; keep standard clippy lints clean.
- `anyhow` at binary/app boundaries, `thiserror` for typed errors in libraries;
  avoid `unwrap`/`expect` outside startup-fatal paths.
- Doc comments on public items; inline comments only for the *why*.
- Commit style: imperative subject referencing the issue
  (`#<issue>: <summary>`).
