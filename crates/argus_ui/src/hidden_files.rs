//! Hidden-file patterns: files the reviewer never wants to see in the main
//! diff or the file tree (generated code, snapshots, tests…).
//!
//! A pattern is one of:
//!
//! - **text**, matched literally: without a `/` it must appear in the file
//!   name (`.test` hides `cache.test.ts`); with a `/` it must appear in the
//!   path (`vendor/` hides everything under a `vendor` directory);
//! - **`/regex/`**, a regular expression searched in the whole path
//!   (`/\.snap$/`, `/^docs/.*\.md$/`).
//!
//! Patterns are global and persisted in the UI settings. Hiding is a view
//! filter only: the diff session keeps every file, so comments on a hidden
//! file still re-anchor, and a run's proposal always shows everything it
//! changed.

use argus_diff::FileList;
use regex::Regex;

/// One compiled pattern.
#[derive(Clone, Debug)]
enum Rule {
    /// Text that must appear in the file name.
    Name(String),
    /// Text that must appear in the path.
    Path(String),
    /// A regular expression searched in the path.
    Regex(Regex),
}

impl Rule {
    fn parse(pattern: &str) -> Result<Self, String> {
        let pattern = pattern.trim();
        if pattern.is_empty() {
            return Err("Type a file name or a /regex/.".to_owned());
        }
        if let Some(body) = pattern
            .strip_prefix('/')
            .and_then(|rest| rest.strip_suffix('/'))
        {
            if body.is_empty() {
                return Err("The regular expression between the slashes is empty.".to_owned());
            }
            return Regex::new(body)
                .map(Rule::Regex)
                .map_err(|error| format!("Invalid regular expression: {error}"));
        }
        if pattern.contains('/') {
            Ok(Rule::Path(pattern.to_owned()))
        } else {
            Ok(Rule::Name(pattern.to_owned()))
        }
    }

    fn hides(&self, path: &str) -> bool {
        match self {
            Rule::Name(text) => file_name(path).contains(text.as_str()),
            Rule::Path(text) => path.contains(text.as_str()),
            Rule::Regex(regex) => regex.is_match(path),
        }
    }
}

/// The compiled hidden-file patterns.
#[derive(Clone, Debug, Default)]
pub struct HiddenFiles {
    rules: Vec<Rule>,
}

impl HiddenFiles {
    /// Compiles `patterns`, skipping the invalid ones (the settings refuse
    /// them, so only a hand-edited record has any).
    pub fn new(patterns: &[String]) -> Self {
        Self {
            rules: patterns
                .iter()
                .filter_map(|pattern| Rule::parse(pattern).ok())
                .collect(),
        }
    }

    /// Whether no pattern is set.
    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }

    /// Whether `path` (repository-relative) is hidden.
    pub fn hides(&self, path: &str) -> bool {
        self.rules.iter().any(|rule| rule.hides(path))
    }

    /// Which entries of `list` are hidden, by file-list index. Walks every
    /// path, so callers run it on the tokio runtime.
    pub fn mask(&self, list: &FileList) -> Vec<bool> {
        if self.is_empty() {
            return vec![false; list.len()];
        }
        list.entries()
            .iter()
            .map(|entry| self.hides(&entry.path))
            .collect()
    }
}

/// Checks a pattern typed in the settings; the error is shown as is.
pub fn validate(pattern: &str) -> Result<(), String> {
    Rule::parse(pattern).map(|_| ())
}

/// The last segment of `path`.
fn file_name(path: &str) -> &str {
    path.rsplit_once('/').map_or(path, |(_, name)| name)
}
