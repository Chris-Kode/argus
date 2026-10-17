//! Run defaults the user sets once (spec §10, Claude section): the `claude`
//! binary, the model and effort lists with their defaults, the review rules
//! file and the commit-message timeout.
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
/// Initial model list of the Run menu.
pub const DEFAULT_MODELS: [&str; 3] = ["opus", "sonnet", "haiku"];
/// Initial effort list of the Run menu.
pub const DEFAULT_EFFORTS: [&str; 5] = ["low", "medium", "high", "xhigh", "max"];
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
    /// No `claude` executable was found on `PATH`.
    #[error("`claude` was not found on PATH; install Claude Code or set its path")]
    NotOnPath,
    /// A list setting has no entries.
    #[error("enter at least one value")]
    EmptyList,
    /// A list entry contains whitespace, which `claude` would not accept.
    #[error("\"{0}\" must not contain spaces")]
    Whitespace(String),
    /// A list entry appears twice.
    #[error("\"{0}\" is listed twice")]
    Duplicate(String),
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
    /// Models offered in the Run menu.
    pub models: Vec<String>,
    /// Thinking efforts offered in the Run menu.
    pub efforts: Vec<String>,
    /// Review rules passed with `--append-system-prompt-file`; `None` uses
    /// `<argus_home>/review-rules.md`. A leading `~` is expanded.
    pub review_rules_path: Option<PathBuf>,
    /// How long the Haiku commit-message call may take before the commit
    /// dialog opens with an empty body (spec §6.4), in seconds.
    pub commit_message_timeout_s: u64,
}

impl Default for ClaudeSettings {
    fn default() -> Self {
        Self {
            claude_path: None,
            default_model: DEFAULT_MODEL.to_owned(),
            default_effort: DEFAULT_EFFORT.to_owned(),
            models: DEFAULT_MODELS
                .iter()
                .map(|&model| model.to_owned())
                .collect(),
            efforts: DEFAULT_EFFORTS
                .iter()
                .map(|&effort| effort.to_owned())
                .collect(),
            review_rules_path: None,
            commit_message_timeout_s: DEFAULT_COMMIT_MESSAGE_TIMEOUT_S,
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

    /// Repairs values a hand-edited or corrupted record could make invalid:
    /// empty or malformed lists reset, defaults missing from their list
    /// fall back to the first entry, and the timeout is clamped.
    pub fn sanitized(mut self) -> Self {
        let defaults = Self::default();
        if validate_list(&self.models).is_err() {
            self.models = defaults.models;
        }
        if validate_list(&self.efforts).is_err() {
            self.efforts = defaults.efforts;
        }
        self.default_model = pick_default(&self.models, &self.default_model);
        self.default_effort = pick_default(&self.efforts, &self.default_effort);
        self.claude_path = self.claude_path.filter(|path| !path.as_os_str().is_empty());
        self.review_rules_path = self
            .review_rules_path
            .filter(|path| !path.as_os_str().is_empty());
        self.commit_message_timeout_s = self
            .commit_message_timeout_s
            .clamp(MIN_COMMIT_MESSAGE_TIMEOUT_S, MAX_COMMIT_MESSAGE_TIMEOUT_S);
        self
    }

    /// Replaces the model list, keeping the default model when it is still
    /// listed and switching to the first entry otherwise.
    pub fn set_models(&mut self, models: Vec<String>) {
        self.default_model = pick_default(&models, &self.default_model);
        self.models = models;
    }

    /// Replaces the effort list, keeping the default effort when it is still
    /// listed and switching to the first entry otherwise.
    pub fn set_efforts(&mut self, efforts: Vec<String>) {
        self.default_effort = pick_default(&efforts, &self.default_effort);
        self.efforts = efforts;
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

    /// The `claude` binary to run: the configured path when it is a valid
    /// executable, else the first `claude` on `PATH`. Blocking.
    pub fn resolve_claude(&self) -> Result<PathBuf, SettingError> {
        match &self.claude_path {
            Some(path) => {
                let path = expand_home(path);
                check_executable(&path)?;
                Ok(path)
            }
            None => find_on_path(CLAUDE_BINARY).ok_or(SettingError::NotOnPath),
        }
    }
}

/// `<argus_home>/review-rules.md`.
pub fn default_review_rules_path(argus_home: &Path) -> PathBuf {
    argus_home.join(REVIEW_RULES_FILE_NAME)
}

/// Splits a comma- or newline-separated list into trimmed entries and
/// validates them: at least one entry, no inner whitespace, no duplicates.
pub fn parse_list(input: &str) -> Result<Vec<String>, SettingError> {
    let entries: Vec<String> = input
        .split([',', '\n'])
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
        .map(str::to_owned)
        .collect();
    validate_list(&entries)?;
    Ok(entries)
}

/// The list as shown in its text field.
pub fn format_list(entries: &[String]) -> String {
    entries.join(", ")
}

fn validate_list(entries: &[String]) -> Result<(), SettingError> {
    if entries.is_empty() {
        return Err(SettingError::EmptyList);
    }
    for (index, entry) in entries.iter().enumerate() {
        if entry.is_empty() || entry.chars().any(char::is_whitespace) {
            return Err(SettingError::Whitespace(entry.clone()));
        }
        if entries[..index].contains(entry) {
            return Err(SettingError::Duplicate(entry.clone()));
        }
    }
    Ok(())
}

fn pick_default(list: &[String], current: &str) -> String {
    if list.iter().any(|entry| entry == current) {
        current.to_owned()
    } else {
        list.first().cloned().unwrap_or_default()
    }
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
