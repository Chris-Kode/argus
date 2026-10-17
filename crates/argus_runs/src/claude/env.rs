//! The environment of every `claude` child process.
//!
//! Children start from an empty environment and get only the variables
//! listed here, read one by one from Argus' own environment. Nothing else
//! leaks through: in particular `ANTHROPIC_API_KEY` (and every other
//! `ANTHROPIC_*` variable) is never read, so Claude Code authenticates with
//! the user's own sign-in (spec §11.6).

use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// Variables passed through to `claude`, when Argus has them.
///
/// - `PATH`, `HOME`: required (finding `node`/`git`, `~/.claude`).
/// - user and shell identity: some tools and the macOS keychain lookup use
///   them.
/// - locale and temp dir: correct text handling and scratch files.
/// - XDG directories and `CLAUDE_CONFIG_DIR`: where Claude Code keeps its
///   configuration and sign-in.
/// - proxy and CA variables: corporate networks cannot reach the API
///   without them.
pub const PASSTHROUGH_VARS: [&str; 25] = [
    "PATH",
    "HOME",
    "USER",
    "LOGNAME",
    "SHELL",
    "TMPDIR",
    "LANG",
    "LC_ALL",
    "LC_CTYPE",
    "XDG_CONFIG_HOME",
    "XDG_DATA_HOME",
    "XDG_CACHE_HOME",
    "XDG_STATE_HOME",
    "XDG_RUNTIME_DIR",
    "CLAUDE_CONFIG_DIR",
    "HTTPS_PROXY",
    "https_proxy",
    "HTTP_PROXY",
    "http_proxy",
    "NO_PROXY",
    "no_proxy",
    "ALL_PROXY",
    "NODE_EXTRA_CA_CERTS",
    "SSL_CERT_FILE",
    "SSL_CERT_DIR",
];

/// The child's environment: each [`PASSTHROUGH_VARS`] entry that is set in
/// Argus' environment, with `program`'s directory appended to `PATH` when it
/// is missing there.
///
/// A GUI app started from the Finder gets a minimal `PATH`; a `claude` found
/// in a common install location (e.g. `/opt/homebrew/bin`) may be a Node
/// script that needs its sibling `node`.
pub fn child_env(program: &Path) -> Vec<(&'static str, OsString)> {
    let mut vars: Vec<(&'static str, OsString)> = PASSTHROUGH_VARS
        .iter()
        .filter_map(|&name| std::env::var_os(name).map(|value| (name, value)))
        .collect();
    let Some(dir) = program
        .parent()
        .filter(|dir| dir.is_absolute())
        .map(Path::to_path_buf)
    else {
        return vars;
    };
    match vars.iter_mut().find(|(name, _)| *name == "PATH") {
        Some((_, path)) => {
            let mut dirs: Vec<PathBuf> = std::env::split_paths(path).collect();
            if !dirs.contains(&dir) {
                dirs.push(dir);
                if let Ok(joined) = std::env::join_paths(dirs) {
                    *path = joined;
                }
            }
        }
        None => vars.push(("PATH", dir.into_os_string())),
    }
    vars
}

/// Clears `command`'s environment and sets only [`child_env`] of `program`.
pub fn apply_clean_env(command: &mut tokio::process::Command, program: &Path) {
    command.env_clear();
    command.envs(child_env(program));
}
