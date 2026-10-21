//! Permission requests Argus answers on its own, so the permission dialog
//! only opens for what is worth a look.
//!
//! A run works in its own worktree, so most of what Claude Code asks about
//! is routine there: building, testing, reading, editing the worktree's
//! files. [`is_safe`] recognizes those calls; the run manager allows them
//! at once (when [`ClaudeSettings::auto_approve_safe`] is on) and sends
//! everything else to the reviewer.
//!
//! The rules are conservative: anything not recognized is asked.
//!
//! - **Never answered:** `AskUserQuestion`, calls on a path outside the
//!   allowed directories (`blocked_path`), sandbox escapes, MCP tools and
//!   any tool not listed below.
//! - **File tools** (`Read`, `Edit`, `Write`, ...) are safe on paths inside
//!   the worktree.
//! - **Read-only and bookkeeping tools** (`WebSearch`, `WebFetch`,
//!   `TodoWrite`, subagents, background-shell output) are safe.
//! - **Bash** commands are split into simple commands (`&&`, `||`, `;`,
//!   `|`); each one must be a known program used in a known way: read-only
//!   tools, file operations whose paths stay inside the worktree, the build,
//!   test, lint and format commands of the usual toolchains, and local,
//!   read-only or worktree-only `git`. Variable expansion, command
//!   substitution, subshells, heredocs, background jobs, `~` and writes
//!   (redirections included) outside the worktree are always asked, and so
//!   are `sudo`, `curl`, `ssh`, `rm` on the worktree itself, `git push`,
//!   `git commit`, `git reset`, installs to the system, publishing, ...
//!
//! Build and test commands run the project's own code (build scripts,
//! package scripts, Makefiles), which the run may have edited: approving
//! them trusts the worktree like the reviewer does when they build it.
//!
//! [`ClaudeSettings::auto_approve_safe`]: crate::settings::ClaudeSettings::auto_approve_safe

use std::path::{Component, Path, PathBuf};

use serde_json::Value;

use crate::permission::{ASK_USER_QUESTION, PermissionRequest};

/// Whether `request`, made by a run working in `worktree`, can be allowed
/// without asking the reviewer.
pub fn is_safe(request: &PermissionRequest, worktree: &Path) -> bool {
    if request.blocked_path.is_some() || request.tool_name == ASK_USER_QUESTION {
        return false;
    }
    let input = &request.input;
    if input
        .get("dangerouslyDisableSandbox")
        .and_then(Value::as_bool)
        == Some(true)
    {
        return false;
    }
    let worktree = normalize(worktree);
    let field = |key: &str| input.get(key).and_then(Value::as_str);
    let inside = |path: &str| contains(&worktree, &worktree, path);
    match request.tool_name.as_str() {
        "Read" | "Glob" | "Grep" | "LS" => field("file_path")
            .or_else(|| field("path"))
            .is_none_or(inside),
        "Edit" | "Write" | "MultiEdit" | "NotebookEdit" => field("file_path")
            .or_else(|| field("notebook_path"))
            .is_some_and(inside),
        "WebSearch" | "WebFetch" | "TodoWrite" | "Task" | "Agent" | "BashOutput" | "TaskOutput"
        | "KillShell" | "KillBash" | "ToolSearch" => true,
        "Bash" => field("command").is_some_and(|command| is_safe_command(command, &worktree)),
        _ => false,
    }
}

/// Whether every simple command of a Bash command line is safe in
/// `worktree` (already normalized).
fn is_safe_command(command: &str, worktree: &Path) -> bool {
    let Some(tokens) = lex(command) else {
        return false;
    };
    let mut cwd = worktree.to_path_buf();
    let mut words: Vec<String> = Vec::new();
    let mut tokens = tokens.into_iter().peekable();
    loop {
        match tokens.next() {
            Some(Token::Word(word)) => words.push(word),
            Some(Token::RedirectOut) => match tokens.next() {
                Some(Token::Word(target))
                    if target == "/dev/null" || contains(worktree, &cwd, &target) => {}
                _ => return false,
            },
            Some(Token::RedirectIn) => match tokens.next() {
                Some(Token::Word(_)) => {}
                _ => return false,
            },
            Some(Token::DupFd) => {}
            Some(Token::Separator) | None => {
                let end = tokens.peek().is_none();
                if !words.is_empty() && !is_safe_simple(&words, worktree, &mut cwd) {
                    return false;
                }
                words.clear();
                if end {
                    return true;
                }
            }
        }
    }
}

/// A lexed piece of a command line.
#[derive(Debug, PartialEq, Eq)]
enum Token {
    /// A word, with its quotes removed.
    Word(String),
    /// `>`, `>>`, `&>`, `2>`, ...: the next word is written to.
    RedirectOut,
    /// `<`: the next word is read.
    RedirectIn,
    /// `2>&1`, `>&2`, ...: no file involved.
    DupFd,
    /// `&&`, `||`, `;`, `|` or a newline.
    Separator,
}

/// Splits a command line into tokens. `None` for anything this does not
/// follow: expansions (`$`, backticks), subshells, process substitution,
/// heredocs, background jobs and unbalanced quotes.
fn lex(command: &str) -> Option<Vec<Token>> {
    let mut tokens = Vec::new();
    let mut word = String::new();
    // A word was started, possibly empty (`''`).
    let mut in_word = false;
    let mut chars = command.chars().peekable();
    let end_word = |tokens: &mut Vec<Token>, word: &mut String, in_word: &mut bool| {
        if *in_word {
            tokens.push(Token::Word(std::mem::take(word)));
            *in_word = false;
        }
    };
    while let Some(ch) = chars.next() {
        match ch {
            ' ' | '\t' => end_word(&mut tokens, &mut word, &mut in_word),
            '\n' | ';' => {
                end_word(&mut tokens, &mut word, &mut in_word);
                tokens.push(Token::Separator);
            }
            '#' if !in_word => {
                // A comment runs to the end of the line.
                while chars.next_if(|&next| next != '\n').is_some() {}
            }
            '\'' => {
                in_word = true;
                loop {
                    match chars.next()? {
                        '\'' => break,
                        other => word.push(other),
                    }
                }
            }
            '"' => {
                in_word = true;
                loop {
                    match chars.next()? {
                        '"' => break,
                        // `$?`, the last exit status, says nothing about paths.
                        '$' if chars.next_if_eq(&'?').is_some() => word.push_str("$?"),
                        '$' | '`' => return None,
                        '\\' => match chars.next()? {
                            escaped @ ('"' | '\\' | '$' | '`') => word.push(escaped),
                            '\n' => {}
                            other => {
                                word.push('\\');
                                word.push(other);
                            }
                        },
                        other => word.push(other),
                    }
                }
            }
            '\\' => match chars.next()? {
                '\n' => {}
                other => {
                    in_word = true;
                    word.push(other);
                }
            },
            '$' if chars.next_if_eq(&'?').is_some() => {
                in_word = true;
                word.push_str("$?");
            }
            '$' | '`' | '(' | ')' => return None,
            '&' => {
                end_word(&mut tokens, &mut word, &mut in_word);
                if chars.next_if_eq(&'&').is_some() {
                    tokens.push(Token::Separator);
                } else if chars.next_if_eq(&'>').is_some() {
                    chars.next_if_eq(&'>');
                    tokens.push(Token::RedirectOut);
                } else {
                    return None;
                }
            }
            '|' => {
                end_word(&mut tokens, &mut word, &mut in_word);
                if chars.next_if_eq(&'&').is_some() {
                    return None;
                }
                chars.next_if_eq(&'|');
                tokens.push(Token::Separator);
            }
            '>' | '<' => {
                // `2>`: the digits before the operator name a descriptor.
                if in_word && !word.is_empty() && word.chars().all(|c| c.is_ascii_digit()) {
                    word.clear();
                    in_word = false;
                }
                end_word(&mut tokens, &mut word, &mut in_word);
                if chars.next_if_eq(&'(').is_some() {
                    return None;
                }
                if ch == '<' {
                    if chars.next_if(|&next| next == '<' || next == '>').is_some() {
                        return None;
                    }
                    tokens.push(Token::RedirectIn);
                    continue;
                }
                chars.next_if(|&next| next == '>' || next == '|');
                if chars.next_if_eq(&'&').is_some() {
                    // `>&2`, `2>&1`, `>&-`.
                    let mut target = false;
                    while chars
                        .next_if(|next| next.is_ascii_digit() || *next == '-')
                        .is_some()
                    {
                        target = true;
                    }
                    if !target {
                        return None;
                    }
                    tokens.push(Token::DupFd);
                } else {
                    tokens.push(Token::RedirectOut);
                }
            }
            other => {
                in_word = true;
                word.push(other);
            }
        }
    }
    end_word(&mut tokens, &mut word, &mut in_word);
    Some(tokens)
}

/// Whether one simple command is safe. `cwd` follows `cd`.
fn is_safe_simple(words: &[String], worktree: &Path, cwd: &mut PathBuf) -> bool {
    // `~` expands to the home directory, wherever it is in a word.
    if words
        .iter()
        .any(|word| word.starts_with('~') || word.contains("=~"))
    {
        return false;
    }
    // Leading `NAME=value` assignments only set the command's environment.
    let start = words
        .iter()
        .position(|word| !is_assignment(word))
        .unwrap_or(words.len());
    let words = &words[start..];
    let Some((program, args)) = words.split_first() else {
        return true;
    };
    let inside = |path: &str| path == "/dev/null" || contains(worktree, cwd, path);
    // Every argument that names a path must stay in the worktree.
    let writes_inside = || {
        args.iter()
            .filter(|arg| !arg.starts_with('-'))
            .all(|arg| inside(arg))
    };
    let has = |flag: &str| args.iter().any(|arg| arg == flag);
    let has_any = |flags: &[&str]| args.iter().any(|arg| flags.contains(&arg.as_str()));
    let subcommand = || args.iter().find(|arg| !arg.starts_with(['-', '+']));
    match program.as_str() {
        "cd" => match args {
            [dir] if contains(worktree, cwd, dir) => {
                *cwd = normalize(&cwd.join(dir));
                true
            }
            _ => false,
        },
        "time" | "nice" => is_safe_simple(args, worktree, cwd),
        "timeout" => {
            // `timeout [flags] DURATION command...`
            match args.iter().position(|arg| !arg.starts_with('-')) {
                Some(duration) => is_safe_simple(&args[duration + 1..], worktree, cwd),
                None => false,
            }
        }
        "ls" | "cat" | "head" | "tail" | "wc" | "grep" | "egrep" | "fgrep" | "rg" | "ag"
        | "sort" | "uniq" | "cut" | "tr" | "diff" | "cmp" | "file" | "stat" | "pwd" | "echo"
        | "printf" | "which" | "type" | "tree" | "du" | "df" | "jq" | "true" | "false" | "test"
        | "[" | "basename" | "dirname" | "realpath" | "readlink" | "nl" | "column" | "comm"
        | "fold" | "date" | "sleep" | "seq" | "xxd" | "hexdump" | "od" | "strings" | "md5sum"
        | "sha256sum" | "shasum" => {
            // `sort -o`, `uniq IN OUT` and `tree -o` write files.
            !(program == "sort" && args.iter().any(|arg| arg.starts_with("-o")))
                && !(program == "uniq" && args.iter().filter(|a| !a.starts_with('-')).count() > 1)
                && !(program == "tree" && has("-o"))
        }
        "mkdir" | "touch" | "cp" | "mv" | "ln" | "chmod" | "tee" | "patch" | "rmdir" => {
            writes_inside()
        }
        "rm" => {
            writes_inside()
                && args.iter().filter(|arg| !arg.starts_with('-')).all(|arg| {
                    // Not the worktree itself, its `.git` link or a glob.
                    let path = normalize(&cwd.join(arg));
                    path != worktree && path != worktree.join(".git") && !arg.contains('*')
                })
        }
        "find" => !has_any(&[
            "-delete", "-exec", "-execdir", "-ok", "-okdir", "-fprint", "-fprint0", "-fprintf",
            "-fls",
        ]),
        // Only line printing: `sed -n '10,40p' file`.
        "sed" => {
            has("-n")
                && !args
                    .iter()
                    .any(|arg| arg.starts_with("-i") || arg == "--in-place")
                && args
                    .iter()
                    .filter(|arg| !arg.starts_with('-'))
                    .take(1)
                    .all(|script| {
                        script
                            .chars()
                            .all(|c| c.is_ascii_digit() || matches!(c, ',' | '$' | 'p' | ';' | ' '))
                    })
        }
        "git" => is_safe_git(args),
        "cargo" => {
            subcommand().is_some_and(|sub| {
                [
                    "build",
                    "b",
                    "check",
                    "c",
                    "test",
                    "t",
                    "clippy",
                    "fmt",
                    "run",
                    "r",
                    "doc",
                    "bench",
                    "tree",
                    "metadata",
                    "clean",
                    "update",
                    "fix",
                    "nextest",
                    "expand",
                    "add",
                    "remove",
                    "search",
                    "version",
                    "help",
                    "locate-project",
                    "pkgid",
                    "verify-project",
                ]
                .contains(&sub.as_str())
            }) || has_any(&["--version", "-V"])
        }
        "rustc" | "rustfmt" | "gofmt" | "make" | "cmake" | "ctest" | "ninja" | "meson"
        | "pytest" | "ruff" | "black" | "isort" | "flake8" | "pylint" | "mypy" | "pyright"
        | "tsc" | "eslint" | "prettier" | "biome" | "jest" | "vitest" | "mocha" | "gradle"
        | "./gradlew" | "mvn" | "./mvnw" | "clang-format" | "shellcheck" => {
            args.iter().all(|arg| !arg.starts_with('/') || inside(arg))
        }
        "go" => {
            subcommand().is_some_and(|sub| {
                [
                    "build", "test", "vet", "fmt", "run", "mod", "list", "env", "version", "doc",
                    "generate", "get", "help", "tool",
                ]
                .contains(&sub.as_str())
            }) && !has("-w")
        }
        "npm" => {
            !has_any(&["-g", "--global"])
                && subcommand().is_some_and(|sub| {
                    [
                        "test",
                        "t",
                        "tst",
                        "run",
                        "run-script",
                        "install",
                        "i",
                        "ci",
                        "ls",
                        "list",
                        "outdated",
                        "why",
                        "explain",
                        "audit",
                        "fund",
                        "view",
                        "help",
                        "prune",
                        "dedupe",
                        "update",
                        "uninstall",
                        "rebuild",
                        "start",
                        "pack",
                    ]
                    .contains(&sub.as_str())
                })
        }
        "yarn" | "pnpm" | "bun" => {
            !has_any(&["-g", "--global"])
                && subcommand().is_none_or(|sub| {
                    ![
                        "publish",
                        "login",
                        "logout",
                        "adduser",
                        "global",
                        "link",
                        "unlink",
                        "version",
                        "dlx",
                        "exec",
                        "x",
                        "create",
                        "init",
                        "config",
                        "set",
                        "token",
                        "owner",
                        "deprecate",
                        "unpublish",
                        "dist-tag",
                        "team",
                        "access",
                        "npm",
                        "self-update",
                        "policies",
                        "plugin",
                        "upgrade-interactive",
                    ]
                    .contains(&sub.as_str())
                })
        }
        "deno" => subcommand().is_some_and(|sub| {
            [
                "test", "fmt", "lint", "check", "task", "bench", "doc", "info",
            ]
            .contains(&sub.as_str())
        }),
        "dotnet" => subcommand().is_some_and(|sub| {
            ["build", "test", "restore", "format", "run", "clean", "list"].contains(&sub.as_str())
        }),
        "swift" => subcommand().is_some_and(|sub| ["build", "test", "run"].contains(&sub.as_str())),
        "python" | "python3" => is_safe_python(args, &inside),
        "node" => {
            !has_any(&[
                "-e",
                "--eval",
                "-p",
                "--print",
                "-r",
                "--require",
                "--import",
            ]) && args
                .iter()
                .find(|arg| !arg.starts_with('-'))
                .is_none_or(|script| inside(script))
        }
        // A program of the worktree, e.g. `./target/debug/app`.
        path if path.starts_with("./") || path.starts_with('/') => inside(path),
        _ => false,
    }
}

/// `git` used locally: reading, and staging or restoring the worktree's own
/// files. Branches, history rewrites, remotes and configuration are asked.
fn is_safe_git(args: &[String]) -> bool {
    // `-C` moves elsewhere, `-c` can set an alias or a pager to any command.
    let Some(index) = args.iter().position(|arg| arg != "--no-pager") else {
        return true;
    };
    let (sub, rest) = (args[index].as_str(), &args[index + 1..]);
    let has_any = |flags: &[&str]| rest.iter().any(|arg| flags.contains(&arg.as_str()));
    match sub {
        "status" | "diff" | "log" | "show" | "blame" | "grep" | "ls-files" | "ls-tree"
        | "rev-parse" | "rev-list" | "describe" | "shortlog" | "cat-file" | "merge-base"
        | "name-rev" | "show-ref" | "for-each-ref" | "check-ignore" | "help" | "version"
        | "add" | "restore" | "rm" | "mv" | "apply" | "diff-tree" | "diff-files" | "diff-index"
        | "whatchanged" | "range-diff" | "count-objects" => {
            !rest.iter().any(|arg| arg.starts_with("--output"))
        }
        // Listing only.
        "branch" | "tag" | "remote" | "worktree" | "stash" => match sub {
            "branch" => {
                rest.iter().all(|arg| arg.starts_with('-'))
                    && !has_any(&[
                        "-d",
                        "-D",
                        "-m",
                        "-M",
                        "-c",
                        "-C",
                        "-f",
                        "-u",
                        "--delete",
                        "--move",
                        "--copy",
                        "--force",
                        "--set-upstream-to",
                        "--unset-upstream",
                        "--edit-description",
                    ])
            }
            "tag" => rest.is_empty() || has_any(&["-l", "--list"]),
            "remote" => rest.iter().all(|arg| arg == "-v" || arg == "--verbose"),
            "worktree" | "stash" => matches!(
                rest.first().map(String::as_str),
                Some("list") | Some("show")
            ),
            _ => false,
        },
        "reflog" => rest.is_empty() || rest[0] == "show",
        _ => false,
    }
}

/// `python -m <tool>` for the usual test and lint tools, or a script of the
/// worktree; never `-c`.
fn is_safe_python(args: &[String], inside: &impl Fn(&str) -> bool) -> bool {
    match args
        .iter()
        .position(|arg| !arg.starts_with('-') || arg == "-m")
    {
        Some(index) if args[index] == "-m" => args.get(index + 1).is_some_and(|module| {
            [
                "pytest",
                "unittest",
                "mypy",
                "ruff",
                "black",
                "isort",
                "flake8",
                "pylint",
                "compileall",
                "doctest",
                "json.tool",
                "pyflakes",
            ]
            .contains(&module.as_str())
        }),
        Some(index) => {
            !args[..index].iter().any(|arg| arg.starts_with("-c")) && inside(&args[index])
        }
        None => args.iter().all(|arg| arg == "--version" || arg == "-V"),
    }
}

fn is_assignment(word: &str) -> bool {
    word.split_once('=').is_some_and(|(name, _)| {
        !name.is_empty()
            && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
            && !name.starts_with(|c: char| c.is_ascii_digit())
    })
}

/// Whether `path`, relative to `cwd`, is inside `root` (both normalized).
fn contains(root: &Path, cwd: &Path, path: &str) -> bool {
    !path.is_empty() && normalize(&cwd.join(path)).starts_with(root)
}

/// `path` with `.` and `..` resolved lexically.
fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    out
}
