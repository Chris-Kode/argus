//! Run defaults the user sets once (spec §10, Claude section): the `claude`
//! binary, the default model and effort, the review rules file and the
//! commit-message timeout. The models and efforts on offer are fixed:
//! [`MODELS`] and [`EFFORTS`].
//!
//! [`ClaudeSettings`] is stored as JSON in the `settings` table under
//! [`CLAUDE_SETTINGS_KEY`]. Every field has a serde default and unknown keys
//! are ignored, so records written by older or newer versions keep loading.
//!
//! The validators that touch the filesystem ([`validate_claude_path`],
//! [`validate_review_rules_path`], [`find_on_path`]) block; call them on the
//! tokio runtime, never on the UI thread.

use std::path::{Path, PathBuf};
use std::time::Duration;

use argus_store::SettingsStore;
use serde::{Deserialize, Serialize};

/// Settings key holding the JSON-encoded [`ClaudeSettings`].
pub const CLAUDE_SETTINGS_KEY: &str = "claude";

/// Name of the `claude` executable looked up on `PATH`.
pub const CLAUDE_BINARY: &str = "claude";
/// Initial default model.
pub const DEFAULT_MODEL: &str = "sonnet";
/// Initial default thinking effort.
pub const DEFAULT_EFFORT: &str = "medium";
/// Models offered in the Run menu, as `claude --model` aliases.
pub const MODELS: [&str; 4] = ["fable", "opus", "sonnet", "haiku"];
/// Thinking efforts offered in the Run menu.
pub const EFFORTS: [&str; 5] = ["low", "medium", "high", "xhigh", "max"];
/// File name of the review rules inside the Argus home.
pub const REVIEW_RULES_FILE_NAME: &str = "review-rules.md";
/// Default timeout of the Haiku commit-message call (spec §6.4), in seconds.
pub const DEFAULT_COMMIT_MESSAGE_TIMEOUT_S: u64 = 20;
/// Smallest accepted commit-message timeout, in seconds.
pub const MIN_COMMIT_MESSAGE_TIMEOUT_S: u64 = 1;
/// Largest accepted commit-message timeout, in seconds.
pub const MAX_COMMIT_MESSAGE_TIMEOUT_S: u64 = 300;

/// An invalid settings value, worded for an inline error under its field.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum SettingError {
    /// The path does not exist.
    #[error("{} does not exist", .0.display())]
    NotFound(PathBuf),
    /// The path exists but is not a regular file.
    #[error("{} is not a file", .0.display())]
    NotAFile(PathBuf),
    /// The file exists but cannot be executed.
    #[error("{} is not executable", .0.display())]
    NotExecutable(PathBuf),
    /// The path could not be inspected.
    #[error("cannot read {}: {reason}", path.display())]
    Unreadable {
        /// The inspected path.
        path: PathBuf,
        /// The I/O error text.
        reason: String,
    },
    /// No `claude` executable was found on `PATH` or in the common install
    /// locations.
    #[error(
        "`claude` was not found on PATH or in the usual install locations; install Claude Code or set its path"
    )]
    NotOnPath,
    /// The value is not a whole number.
    #[error("enter a whole number of seconds")]
    NotANumber,
    /// The number is outside the accepted range.
    #[error("enter a value between {min} and {max}")]
    OutOfRange {
        /// Smallest accepted value.
        min: u64,
        /// Largest accepted value.
        max: u64,
    },
}

/// The Claude run defaults.
///
/// Missing fields take their defaults and unknown fields are ignored, so a
/// record from an older or a newer Argus still loads.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ClaudeSettings {
    /// Path of the `claude` binary; `None` auto-detects it on `PATH`. A
    /// leading `~` is expanded when resolving.
    pub claude_path: Option<PathBuf>,
    /// Model preselected in the Run menu; Argus updates it to the last choice.
    pub default_model: String,
    /// Thinking effort preselected in the Run menu.
    pub default_effort: String,
    /// Review rules passed with `--append-system-prompt-file`; `None` uses
    /// `<argus_home>/review-rules.md`. A leading `~` is expanded.
    pub review_rules_path: Option<PathBuf>,
    /// How long the Haiku commit-message call may take before the commit
    /// dialog opens with an empty body (spec §6.4), in seconds.
    pub commit_message_timeout_s: u64,
    /// Allow the permission requests of runs that
    /// [`auto_approve::is_safe`](crate::auto_approve::is_safe) recognizes
    /// as routine, instead of asking in the permission dialog.
    pub auto_approve_safe: bool,
}

impl Default for ClaudeSettings {
    fn default() -> Self {
        Self {
            claude_path: None,
            default_model: DEFAULT_MODEL.to_owned(),
            default_effort: DEFAULT_EFFORT.to_owned(),
            review_rules_path: None,
            commit_message_timeout_s: DEFAULT_COMMIT_MESSAGE_TIMEOUT_S,
            auto_approve_safe: true,
        }
    }
}

impl ClaudeSettings {
    /// Loads the settings, falling back to defaults when they are missing or
    /// unreadable. Must run on the tokio runtime.
    pub async fn load(store: &SettingsStore) -> Self {
        match store.get::<ClaudeSettings>(CLAUDE_SETTINGS_KEY).await {
            Ok(Some(settings)) => settings.sanitized(),
            Ok(None) => Self::default(),
            Err(error) => {
                tracing::warn!(%error, "failed to load Claude settings; using defaults");
                Self::default()
            }
        }
    }

    /// Stores the settings. Must run on the tokio runtime.
    pub async fn save(&self, store: &SettingsStore) -> argus_store::Result<()> {
        store.set(CLAUDE_SETTINGS_KEY, self).await
    }

    /// Repairs values a hand-edited, corrupted or older record could make
    /// invalid: a default model or effort that is no longer offered falls
    /// back to [`DEFAULT_MODEL`] / [`DEFAULT_EFFORT`], and the timeout is
    /// clamped.
    pub fn sanitized(mut self) -> Self {
        if !MODELS.contains(&self.default_model.as_str()) {
            self.default_model = DEFAULT_MODEL.to_owned();
        }
        if !EFFORTS.contains(&self.default_effort.as_str()) {
            self.default_effort = DEFAULT_EFFORT.to_owned();
        }
        self.claude_path = self.claude_path.filter(|path| !path.as_os_str().is_empty());
        self.review_rules_path = self
            .review_rules_path
            .filter(|path| !path.as_os_str().is_empty());
        self.commit_message_timeout_s = self
            .commit_message_timeout_s
            .clamp(MIN_COMMIT_MESSAGE_TIMEOUT_S, MAX_COMMIT_MESSAGE_TIMEOUT_S);
        self
    }

    /// The review rules file: the configured path, or
    /// `<argus_home>/review-rules.md`.
    pub fn review_rules_path(&self, argus_home: &Path) -> PathBuf {
        match &self.review_rules_path {
            Some(path) => expand_home(path),
            None => default_review_rules_path(argus_home),
        }
    }

    /// The commit-message timeout as a [`Duration`].
    pub fn commit_message_timeout(&self) -> Duration {
        Duration::from_secs(self.commit_message_timeout_s)
    }

    /// The `claude` binary to run; see [`resolve_claude_path`]. Blocking.
    pub fn resolve_claude(&self) -> Result<PathBuf, SettingError> {
        resolve_claude_path(self.claude_path.as_deref())
    }
}

/// Where installers put `claude` when it is not on the app's `PATH` (a GUI
/// app started from the Finder sees only the system `PATH`). A leading `~`
/// is expanded.
pub const COMMON_CLAUDE_PATHS: [&str; 7] = [
    "~/.claude/local/claude",
    "~/.local/bin/claude",
    "/opt/homebrew/bin/claude",
    "/usr/local/bin/claude",
    "~/.npm-global/bin/claude",
    "~/.bun/bin/claude",
    "~/.volta/bin/claude",
];

/// The `claude` binary to run: the configured path when set (it must be an
/// executable file; there is no fallback), else the first `claude` on
/// `PATH`, else the first of [`COMMON_CLAUDE_PATHS`] that is executable.
/// Blocking.
pub fn resolve_claude_path(configured: Option<&Path>) -> Result<PathBuf, SettingError> {
    if let Some(path) = configured {
        let path = expand_home(path);
        check_executable(&path)?;
        return Ok(path);
    }
    find_on_path(CLAUDE_BINARY)
        .or_else(|| {
            COMMON_CLAUDE_PATHS
                .iter()
                .map(|path| expand_home(Path::new(path)))
                .find(|path| path.is_absolute() && check_executable(path).is_ok())
        })
        .ok_or(SettingError::NotOnPath)
}

/// `<argus_home>/review-rules.md`.
pub fn default_review_rules_path(argus_home: &Path) -> PathBuf {
    argus_home.join(REVIEW_RULES_FILE_NAME)
}

/// Parses the commit-message timeout field.
pub fn parse_commit_message_timeout(input: &str) -> Result<u64, SettingError> {
    let seconds: u64 = input.trim().parse().map_err(|_| SettingError::NotANumber)?;
    if (MIN_COMMIT_MESSAGE_TIMEOUT_S..=MAX_COMMIT_MESSAGE_TIMEOUT_S).contains(&seconds) {
        Ok(seconds)
    } else {
        Err(SettingError::OutOfRange {
            min: MIN_COMMIT_MESSAGE_TIMEOUT_S,
            max: MAX_COMMIT_MESSAGE_TIMEOUT_S,
        })
    }
}

/// Validates the Claude path field. Empty means auto-detect (`Ok(None)`);
/// otherwise the path must be an executable file. Blocking.
pub fn validate_claude_path(input: &str) -> Result<Option<PathBuf>, SettingError> {
    let input = input.trim();
    if input.is_empty() {
        return Ok(None);
    }
    let path = PathBuf::from(input);
    check_executable(&expand_home(&path))?;
    Ok(Some(path))
}

/// Validates the review rules path field. Empty means the default file
/// (`Ok(None)`); otherwise the path must be an existing file. Blocking.
pub fn validate_review_rules_path(input: &str) -> Result<Option<PathBuf>, SettingError> {
    let input = input.trim();
    if input.is_empty() {
        return Ok(None);
    }
    let path = PathBuf::from(input);
    check_file(&expand_home(&path))?;
    Ok(Some(path))
}

/// The first executable file named `name` in the `PATH` directories.
/// Blocking.
pub fn find_on_path(name: &str) -> Option<PathBuf> {
    let paths = std::env::var_os("PATH")?;
    std::env::split_paths(&paths)
        .filter(|dir| !dir.as_os_str().is_empty())
        .flat_map(|dir| candidates(&dir, name))
        .find(|candidate| check_executable(candidate).is_ok())
}

#[cfg(windows)]
fn candidates(dir: &Path, name: &str) -> Vec<PathBuf> {
    ["exe", "cmd", "bat"]
        .iter()
        .map(|extension| dir.join(name).with_extension(extension))
        .collect()
}

#[cfg(not(windows))]
fn candidates(dir: &Path, name: &str) -> Vec<PathBuf> {
    vec![dir.join(name)]
}

/// Expands a leading `~` to `$HOME`.
pub fn expand_home(path: &Path) -> PathBuf {
    let Ok(rest) = path.strip_prefix("~") else {
        return path.to_path_buf();
    };
    match std::env::var_os("HOME") {
        Some(home) if !home.is_empty() => PathBuf::from(home).join(rest),
        _ => path.to_path_buf(),
    }
}

fn check_file(path: &Path) -> Result<std::fs::Metadata, SettingError> {
    match std::fs::metadata(path) {
        Ok(metadata) if metadata.is_file() => Ok(metadata),
        Ok(_) => Err(SettingError::NotAFile(path.to_path_buf())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            Err(SettingError::NotFound(path.to_path_buf()))
        }
        Err(error) => Err(SettingError::Unreadable {
            path: path.to_path_buf(),
            reason: error.to_string(),
        }),
    }
}

fn check_executable(path: &Path) -> Result<(), SettingError> {
    let metadata = check_file(path)?;
    if is_executable(&metadata) {
        Ok(())
    } else {
        Err(SettingError::NotExecutable(path.to_path_buf()))
    }
}

#[cfg(unix)]
fn is_executable(metadata: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt as _;
    metadata.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
fn is_executable(_: &std::fs::Metadata) -> bool {
    true
}
