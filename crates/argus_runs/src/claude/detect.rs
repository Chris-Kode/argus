//! Detecting the installed `claude` and the flags it supports (spec §6.2
//! implementation note, Q4).
//!
//! [`detect`] resolves the binary, runs `claude --version` and
//! `claude --help` (each with a clean environment and a timeout), and returns
//! the [`ClaudeCapabilities`]. A missing required flag blocks runs; a missing
//! `--effort` only hides the effort picker, and a missing `auto` permission
//! mode makes runs fall back to `acceptEdits` (with every other tool call
//! asked in the permission dialog).
//!
//! `--help` does not list every flag: Claude Code 2.1 accepts
//! `--append-system-prompt-file` and `--max-turns` without listing them. Each
//! needed flag the help does not mention is therefore probed with
//! `claude -p <flag>`, with no prompt and stdin closed: the CLI rejects the
//! invocation before doing any work ("argument missing" or "Input must be
//! provided"), unless it does not know the flag ("unknown option"). Without a
//! prompt no request is ever sent.

use std::collections::BTreeSet;
use std::fmt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use crate::settings::{SettingError, resolve_claude_path};

use super::env::apply_clean_env;

/// How long `claude --version` or `claude --help` may take.
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(20);

/// The thinking-effort flag; optional, unlike [`REQUIRED_FLAGS`].
pub const EFFORT_FLAG: &str = "--effort";

/// The permission mode of review runs: Claude Code approves what is safe on
/// its own and asks Argus for the rest.
pub const AUTO_MODE: &str = "auto";

/// Flags Argus cannot run without: every flag of the review-run argv
/// (spec §6.2) plus `--tools`, `--max-turns`, `--strict-mcp-config` and
/// `--settings` of the commit-message call (spec §6.4).
/// `--permission-prompt-tool` is not listed by `--help`; the probe finds it.
pub const REQUIRED_FLAGS: [&str; 15] = [
    "-p",
    "--model",
    "--session-id",
    "--resume",
    "--input-format",
    "--output-format",
    "--verbose",
    "--permission-mode",
    "--permission-prompt-tool",
    "--disallowedTools",
    "--append-system-prompt-file",
    "--tools",
    "--max-turns",
    "--strict-mcp-config",
    "--settings",
];

/// Why `claude` could not be detected.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum DetectError {
    /// No usable binary: the configured path is invalid, or none was found.
    #[error(transparent)]
    NotFound(#[from] SettingError),
    /// The binary could not be started.
    #[error("cannot run {}: {reason}", path.display())]
    Spawn {
        /// The binary.
        path: PathBuf,
        /// The I/O error text.
        reason: String,
    },
    /// A probe did not finish within [`PROBE_TIMEOUT`].
    #[error("`claude {probe}` did not answer within {}s", PROBE_TIMEOUT.as_secs())]
    TimedOut {
        /// The probe argument (`--version` or `--help`).
        probe: &'static str,
    },
    /// A probe exited with an error.
    #[error("`claude {probe}` failed: {detail}")]
    Failed {
        /// The probe argument (`--version` or `--help`).
        probe: &'static str,
        /// The exit status and the first line of stderr.
        detail: String,
    },
}

/// A parsed `claude --version`, e.g. `2.1.301 (Claude Code)`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ClaudeVersion {
    /// Major version.
    pub major: u64,
    /// Minor version.
    pub minor: u64,
    /// Patch version.
    pub patch: u64,
}

impl ClaudeVersion {
    /// The first `MAJOR.MINOR[.PATCH]` token of `output`.
    pub fn parse(output: &str) -> Option<Self> {
        output.split_whitespace().find_map(|token| {
            let token = token.trim_start_matches('v');
            let core = token
                .split(|c: char| !(c.is_ascii_digit() || c == '.'))
                .next()?;
            let mut parts = core.split('.');
            let major = parts.next()?.parse().ok()?;
            let minor = parts.next()?.parse().ok()?;
            let patch = parts.next().map_or(Some(0), |part| part.parse().ok())?;
            Some(Self {
                major,
                minor,
                patch,
            })
        })
    }
}

impl fmt::Display for ClaudeVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

/// What the installed `claude` is and supports.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClaudeCapabilities {
    path: PathBuf,
    version: Option<ClaudeVersion>,
    flags: BTreeSet<String>,
    auto_mode: bool,
}

impl ClaudeCapabilities {
    /// Capabilities from the output of `claude --version` and
    /// `claude --help`.
    pub fn from_probes(path: PathBuf, version_output: &str, help_output: &str) -> Self {
        // `--permission-mode` lists its choices: `(choices: "acceptEdits",
        // "auto", …)`.
        let auto_mode = help_output.contains(&format!("\"{AUTO_MODE}\""));
        Self {
            path,
            version: ClaudeVersion::parse(version_output),
            flags: parse_help_flags(help_output),
            auto_mode,
        }
    }

    /// The binary to run.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The parsed version, when `claude --version` printed one.
    pub fn version(&self) -> Option<ClaudeVersion> {
        self.version
    }

    /// The version for messages, e.g. `2.1.301`.
    pub fn version_label(&self) -> String {
        self.version
            .map_or_else(|| "(unknown version)".to_owned(), |v| v.to_string())
    }

    /// Whether `--help` mentions `flag`.
    pub fn supports(&self, flag: &str) -> bool {
        self.flags.contains(flag)
    }

    /// Whether `--effort` is supported; when not, the Run menu hides the
    /// effort picker.
    pub fn supports_effort(&self) -> bool {
        self.supports(EFFORT_FLAG)
    }

    /// Whether `--permission-mode` offers [`AUTO_MODE`].
    pub fn supports_auto_mode(&self) -> bool {
        self.auto_mode
    }

    /// The [`REQUIRED_FLAGS`] the CLI lacks.
    pub fn missing_required(&self) -> Vec<&'static str> {
        REQUIRED_FLAGS
            .iter()
            .copied()
            .filter(|flag| !self.supports(flag))
            .collect()
    }

    /// Why runs are disabled, e.g. `Claude Code 2.0.1 is missing
    /// --permission-prompt-tool; please update`; `None` when they can start.
    pub fn run_blocker(&self) -> Option<String> {
        let missing = self.missing_required();
        (!missing.is_empty()).then(|| {
            format!(
                "Claude Code {} is missing {}; please update",
                self.version_label(),
                missing.join(", ")
            )
        })
    }

    /// A degradation runs live with: a missing `--effort` or `auto` mode.
    pub fn warning(&self) -> Option<String> {
        let mut gaps = Vec::new();
        if !self.supports_effort() {
            gaps.push(format!(
                "does not support {EFFORT_FLAG}; runs use its default effort"
            ));
        }
        if !self.supports_auto_mode() {
            gaps.push(format!(
                "has no `{AUTO_MODE}` permission mode; runs ask you before anything but edits"
            ));
        }
        (!gaps.is_empty()).then(|| {
            format!(
                "Claude Code {} {}",
                self.version_label(),
                gaps.join(", and ")
            )
        })
    }
}

/// The state of `claude` detection, published by the
/// [`RunManager`](crate::RunManager).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum ClaudeStatus {
    /// Detection has not finished yet.
    #[default]
    Detecting,
    /// No usable `claude`.
    Unavailable(DetectError),
    /// `claude` was found and probed.
    Detected(ClaudeCapabilities),
}

impl ClaudeStatus {
    /// The capabilities, once detected.
    pub fn capabilities(&self) -> Option<&ClaudeCapabilities> {
        match self {
            Self::Detected(capabilities) => Some(capabilities),
            _ => None,
        }
    }

    /// Why Run is disabled, or `None` when runs can start.
    pub fn run_blocker(&self) -> Option<String> {
        match self {
            Self::Detecting => Some("Checking the Claude Code installation…".to_owned()),
            Self::Unavailable(error) => Some(format!("Claude Code is unavailable: {error}")),
            Self::Detected(capabilities) => capabilities.run_blocker(),
        }
    }

    /// A warning to show while runs stay enabled.
    pub fn warning(&self) -> Option<String> {
        match self {
            Self::Detected(capabilities) if capabilities.run_blocker().is_none() => {
                capabilities.warning()
            }
            _ => None,
        }
    }

    /// Whether the Run menu offers the effort picker.
    pub fn supports_effort(&self) -> bool {
        self.capabilities()
            .is_some_and(ClaudeCapabilities::supports_effort)
    }
}

/// Finds `claude` (the configured path, else `PATH`, else the common install
/// locations) and probes it. Runs on the tokio runtime; the filesystem
/// lookup runs on the blocking pool.
pub async fn detect(configured: Option<PathBuf>) -> Result<ClaudeCapabilities, DetectError> {
    let path = tokio::task::spawn_blocking(move || resolve_claude_path(configured.as_deref()))
        .await
        .map_err(|error| DetectError::Spawn {
            path: PathBuf::from(crate::settings::CLAUDE_BINARY),
            reason: error.to_string(),
        })??;
    let version = probe(&path, "--version").await?;
    let help = probe(&path, "--help").await?;
    let mut capabilities = ClaudeCapabilities::from_probes(path, &version, &help);
    let unlisted: Vec<&'static str> = REQUIRED_FLAGS
        .iter()
        .chain([&EFFORT_FLAG])
        .copied()
        .filter(|flag| !capabilities.supports(flag))
        .collect();
    let mut probes = tokio::task::JoinSet::new();
    for flag in unlisted {
        let path = capabilities.path().to_path_buf();
        probes.spawn(async move { (flag, accepts_flag(&path, flag).await) });
    }
    while let Some(joined) = probes.join_next().await {
        if let Ok((flag, true)) = joined {
            tracing::debug!(flag, "claude accepts a flag its --help does not list");
            capabilities.flags.insert(flag.to_owned());
        }
    }
    tracing::info!(
        path = %capabilities.path().display(),
        version = %capabilities.version_label(),
        missing = ?capabilities.missing_required(),
        effort = capabilities.supports_effort(),
        auto_mode = capabilities.supports_auto_mode(),
        "detected Claude Code"
    );
    Ok(capabilities)
}

/// Whether `claude -p <flag>` (no prompt, stdin closed) fails for any reason
/// other than an unknown option. Without a prompt a real `claude -p` always
/// fails, so a success means the binary ignored its arguments (a wrapper or
/// stub) and proves nothing; it counts as "no", like spawn errors and
/// timeouts.
async fn accepts_flag(path: &Path, flag: &str) -> bool {
    let mut command = tokio::process::Command::new(path);
    command
        .args(["-p", flag])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    apply_clean_env(&mut command, path);
    let Ok(child) = command.spawn() else {
        return false;
    };
    match tokio::time::timeout(PROBE_TIMEOUT, child.wait_with_output()).await {
        Ok(Ok(output)) if !output.status.success() => {
            let unknown = |bytes: &[u8]| String::from_utf8_lossy(bytes).contains("unknown option");
            !unknown(&output.stderr) && !unknown(&output.stdout)
        }
        _ => false,
    }
}

/// Runs `claude <arg>` and returns its stdout.
async fn probe(path: &Path, arg: &'static str) -> Result<String, DetectError> {
    let mut command = tokio::process::Command::new(path);
    command
        .arg(arg)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    apply_clean_env(&mut command, path);
    let child = command.spawn().map_err(|error| DetectError::Spawn {
        path: path.to_path_buf(),
        reason: error.to_string(),
    })?;
    let output = tokio::time::timeout(PROBE_TIMEOUT, child.wait_with_output())
        .await
        .map_err(|_| DetectError::TimedOut { probe: arg })?
        .map_err(|error| DetectError::Spawn {
            path: path.to_path_buf(),
            reason: error.to_string(),
        })?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let first_line = stderr.lines().find(|line| !line.trim().is_empty());
        return Err(DetectError::Failed {
            probe: arg,
            detail: match first_line {
                Some(line) => format!("{} ({})", output.status, line.trim()),
                None => output.status.to_string(),
            },
        });
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Every flag `help` mentions, as `--long` or `-s`.
///
/// Mentions in descriptions count too, and the bracket form
/// `--append-system-prompt[-file]` (from the `--bare` description of Claude
/// Code 2.1) names both spellings. Flags never mentioned are probed by
/// [`detect`].
pub fn parse_help_flags(help: &str) -> BTreeSet<String> {
    let bytes = help.as_bytes();
    let is_name = |b: u8| b.is_ascii_alphanumeric() || b == b'-' || b == b'_';
    let mut flags = BTreeSet::new();
    let mut i = 0;
    while i < bytes.len() {
        let starts_token = bytes[i] == b'-' && (i == 0 || !is_name(bytes[i - 1]));
        if !starts_token {
            i += 1;
            continue;
        }
        let long = bytes.get(i + 1) == Some(&b'-');
        let name_start = if long { i + 2 } else { i + 1 };
        let mut end = name_start;
        while end < bytes.len() && is_name(bytes[end]) {
            end += 1;
        }
        let name = &help[name_start..end];
        let first_is_letter = name.as_bytes().first().is_some_and(u8::is_ascii_alphabetic);
        if long && first_is_letter {
            let flag = format!("--{}", name.trim_end_matches('-'));
            // `--name[-suffix]` also names `--name-suffix`.
            if bytes.get(end) == Some(&b'[')
                && let Some(close) = help[end..].find(']')
            {
                let suffix = &help[end + 1..end + close];
                if !suffix.is_empty() && suffix.bytes().all(is_name) {
                    flags.insert(format!("{flag}{suffix}"));
                }
            }
            flags.insert(flag);
        } else if !long && name.len() == 1 && first_is_letter {
            flags.insert(format!("-{name}"));
        }
        i = end.max(i + 1);
    }
    flags
}
