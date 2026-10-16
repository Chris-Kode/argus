//! Filesystem locations for Argus user data.

use std::env;
use std::path::PathBuf;

/// Base directory for all Argus user data: `~/.argus` by default.
///
/// `ARGUS_HOME` overrides it, which is how tests and CI keep runs away from a
/// real home directory. Everything Argus owns — the SQLite database, review
/// rules, worktrees and logs — lives below this path and never inside a
/// reviewed repository (spec §8, §11).
pub fn argus_home() -> PathBuf {
    match env::var_os("ARGUS_HOME") {
        Some(value) if !value.is_empty() => PathBuf::from(value),
        _ => home_dir().join(".argus"),
    }
}

/// Best-effort home directory.
///
/// macOS and Linux set `HOME`; with no home at all, fall back to the current
/// directory so Argus still starts with isolated (if surprising) data.
fn home_dir() -> PathBuf {
    env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}
