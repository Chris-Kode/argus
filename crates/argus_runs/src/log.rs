//! The live log of a run, as the run panel shows it (spec §6.3).
//!
//! [`RunLog`] folds the parsed [`StreamEvent`]s of one run into a flat list
//! of [`LogEntry`] rows: assistant text (Markdown), thinking, compact tool
//! calls (`Read src/foo.rs`, `Edit src/foo.rs (+3 −1)`, `Bash git diff`)
//! whose result is attached when it arrives, errors and the final summary.
//! It has no UI dependency, so a stored log of thousands of events can be
//! rebuilt on the tokio runtime and handed to the view in one piece.

use std::collections::HashMap;
use std::ops::Range;

use argus_store::RunEvent;
use serde_json::Value;

use crate::stream::{AssistantContent, RunResult, StreamEvent, UserContent, parse_line};

/// Longest tool label, in characters, before it is cut with `…`.
const MAX_LABEL_CHARS: usize = 120;

/// Longest tool input or output kept for the expanded view, in bytes.
/// Larger payloads (a big `Read` result) are cut; the full text stays in
/// `run_events`.
const MAX_DETAIL_BYTES: usize = 64 * 1024;

/// One row of a run's log.
#[derive(Clone, Debug, PartialEq)]
pub enum LogEntry {
    /// The session started (`system/init`).
    Started {
        /// The model Claude Code actually uses.
        model: Option<String>,
        /// Version of Claude Code.
        version: Option<String>,
    },
    /// Assistant text, in Markdown.
    Text(String),
    /// Extended thinking text.
    Thinking(String),
    /// A tool call, with its result once it arrived.
    Tool(ToolCall),
    /// Plain text sent as the user (rare in `-p` runs).
    UserText(String),
    /// An error reported in the stream.
    Error(String),
    /// The final `result` line.
    Summary {
        /// Claude's final message.
        text: Option<String>,
        /// Number of turns.
        turns: Option<u32>,
        /// Total cost in USD.
        cost_usd: Option<f64>,
        /// Duration reported by Claude.
        duration_ms: Option<i64>,
    },
    /// An event Argus does not model, by `type[/subtype]`, with its JSON.
    Other {
        /// The raw event type.
        kind: String,
        /// The raw JSON, pretty-printed.
        json: String,
    },
}

/// A tool call row.
#[derive(Clone, Debug, PartialEq)]
pub struct ToolCall {
    /// Tool call id, matched by its result.
    pub id: String,
    /// Tool name (`Read`, `Edit`, `Bash`, ...).
    pub name: String,
    /// The compact one-line label.
    pub label: String,
    /// The input, pretty-printed.
    pub input: String,
    /// The result text, once it arrived.
    pub output: Option<String>,
    /// Whether the tool reported an error.
    pub is_error: bool,
}

/// Which rows a [`RunLog::push`] touched, for views that cache row heights.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LogChange {
    /// Rows appended at the end.
    pub appended: Range<usize>,
    /// Earlier rows that changed in place (a tool call got its result).
    pub updated: Vec<usize>,
}

impl LogChange {
    /// Whether nothing changed.
    pub fn is_empty(&self) -> bool {
        self.appended.is_empty() && self.updated.is_empty()
    }
}

/// The folded log of one run.
#[derive(Clone, Debug, Default)]
pub struct RunLog {
    entries: Vec<LogEntry>,
    /// Row of each tool call, by id.
    tools: HashMap<String, usize>,
    /// Working directory of the run; tool paths are shown relative to it.
    cwd: Option<String>,
    /// Last applied `seq`; older or repeated events are ignored.
    last_seq: Option<i64>,
    /// Assistant messages so far, the live turn estimate.
    assistant_messages: u32,
    /// The final result, once it arrived.
    result: Option<RunResult>,
}

impl RunLog {
    /// An empty log.
    pub fn new() -> Self {
        Self::default()
    }

    /// Rebuilds a log from the stored events of a run. Lines that no longer
    /// parse are skipped. Blocking work for long runs: call it on the tokio
    /// runtime.
    pub fn from_stored(events: &[RunEvent]) -> Self {
        let mut log = Self::new();
        for event in events {
            if let Ok(parsed) = parse_line(&event.json) {
                log.push(event.seq, &parsed);
            }
        }
        log
    }

    /// The rows, in stream order.
    pub fn entries(&self) -> &[LogEntry] {
        &self.entries
    }

    /// Number of rows.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the log has no rows.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The `seq` of the last applied event.
    pub fn last_seq(&self) -> Option<i64> {
        self.last_seq
    }

    /// Turns: Claude's count once the run reported its result, otherwise
    /// the assistant messages so far.
    pub fn turns(&self) -> u32 {
        self.result
            .as_ref()
            .and_then(|result| result.num_turns)
            .unwrap_or(self.assistant_messages)
    }

    /// Total cost, once the run reported it.
    pub fn cost_usd(&self) -> Option<f64> {
        self.result
            .as_ref()
            .and_then(|result| result.total_cost_usd)
    }

    /// Applies event `seq` of the run. An event at or before the last
    /// applied `seq` is ignored, so a replay from storage and the live
    /// stream can overlap.
    pub fn push(&mut self, seq: i64, event: &StreamEvent) -> LogChange {
        if self.last_seq.is_some_and(|last| seq <= last) {
            return LogChange::default();
        }
        self.last_seq = Some(seq);
        let start = self.entries.len();
        let mut updated = Vec::new();
        match event {
            StreamEvent::System(init) => {
                self.cwd = init.cwd.clone();
                self.entries.push(LogEntry::Started {
                    model: init.model.clone(),
                    version: init.claude_code_version.clone(),
                });
            }
            StreamEvent::Assistant(message) => {
                self.assistant_messages += 1;
                for block in &message.content {
                    match block {
                        AssistantContent::Text(text) if !text.trim().is_empty() => {
                            self.entries.push(LogEntry::Text(text.clone()));
                        }
                        AssistantContent::Text(_) => {}
                        AssistantContent::Thinking(text) if !text.trim().is_empty() => {
                            self.entries.push(LogEntry::Thinking(text.clone()));
                        }
                        AssistantContent::Thinking(_) => {}
                        AssistantContent::ToolUse { id, name, input } => {
                            let label = tool_label(name, input, self.cwd.as_deref());
                            self.tools.insert(id.clone(), self.entries.len());
                            self.entries.push(LogEntry::Tool(ToolCall {
                                id: id.clone(),
                                name: name.clone(),
                                label,
                                input: pretty(input),
                                output: None,
                                is_error: false,
                            }));
                        }
                        AssistantContent::Other(_) => {}
                    }
                }
            }
            StreamEvent::User(message) => {
                for block in &message.content {
                    match block {
                        UserContent::ToolResult(result) => {
                            let row = self.tools.get(&result.tool_use_id).copied();
                            match row.and_then(|row| self.entries.get_mut(row).map(|e| (row, e))) {
                                Some((row, LogEntry::Tool(call))) => {
                                    call.output = Some(truncate(&result.content));
                                    call.is_error = result.is_error;
                                    if row < start {
                                        updated.push(row);
                                    }
                                }
                                _ if result.is_error => {
                                    self.entries.push(LogEntry::Error(result.content.clone()));
                                }
                                _ => {}
                            }
                        }
                        UserContent::Text(text) if !text.trim().is_empty() => {
                            self.entries.push(LogEntry::UserText(text.clone()));
                        }
                        UserContent::Text(_) | UserContent::Other(_) => {}
                    }
                }
            }
            StreamEvent::Result(result) => {
                let text = result.result.clone().filter(|text| !text.trim().is_empty());
                if result.is_error {
                    let what = match result.subtype.as_str() {
                        "" | "success" => "Claude reported an error".to_owned(),
                        subtype => format!("Claude reported an error ({subtype})"),
                    };
                    self.entries.push(LogEntry::Error(match &text {
                        Some(text) => format!("{what}: {text}"),
                        None => what,
                    }));
                }
                self.entries.push(LogEntry::Summary {
                    text: if result.is_error { None } else { text },
                    turns: result.num_turns,
                    cost_usd: result.total_cost_usd,
                    duration_ms: result.duration_ms,
                });
                self.result = Some(result.clone());
            }
            StreamEvent::Unknown(value) => {
                self.entries.push(LogEntry::Other {
                    kind: event.event_type(),
                    json: pretty(value),
                });
            }
        }
        LogChange {
            appended: start..self.entries.len(),
            updated,
        }
    }
}

/// The compact label of a tool call, e.g. `Edit src/foo.rs (+3 −1)`.
pub fn tool_label(name: &str, input: &Value, cwd: Option<&str>) -> String {
    let field = |key: &str| input.get(key).and_then(Value::as_str);
    let path = |key: &str| field(key).map(|path| relative(path, cwd));
    let label = match name {
        "Read" | "NotebookRead" => match path("file_path").or_else(|| path("notebook_path")) {
            Some(path) => format!("{name} {path}"),
            None => name.to_owned(),
        },
        "Edit" => {
            let added = line_count(field("new_string"));
            let removed = line_count(field("old_string"));
            format!(
                "Edit {} (+{added} −{removed})",
                path("file_path").unwrap_or_default()
            )
        }
        "MultiEdit" => {
            let (added, removed) = input
                .get("edits")
                .and_then(Value::as_array)
                .map(|edits| {
                    edits.iter().fold((0, 0), |(added, removed), edit| {
                        let text = |key: &str| edit.get(key).and_then(Value::as_str);
                        (
                            added + line_count(text("new_string")),
                            removed + line_count(text("old_string")),
                        )
                    })
                })
                .unwrap_or_default();
            format!(
                "Edit {} (+{added} −{removed})",
                path("file_path").unwrap_or_default()
            )
        }
        "Write" => format!(
            "Write {} (+{})",
            path("file_path").unwrap_or_default(),
            line_count(field("content"))
        ),
        "Bash" => match field("command") {
            Some(command) => format!("Bash {}", command.lines().next().unwrap_or_default()),
            None => "Bash".to_owned(),
        },
        "Grep" => match (field("pattern"), path("path")) {
            (Some(pattern), Some(path)) => format!("Grep {pattern} in {path}"),
            (Some(pattern), None) => format!("Grep {pattern}"),
            _ => "Grep".to_owned(),
        },
        "Glob" => match field("pattern") {
            Some(pattern) => format!("Glob {pattern}"),
            None => "Glob".to_owned(),
        },
        "TodoWrite" => match input.get("todos").and_then(Value::as_array) {
            Some(todos) => format!("TodoWrite ({} items)", todos.len()),
            None => "TodoWrite".to_owned(),
        },
        _ => {
            let detail = [
                "description",
                "url",
                "query",
                "file_path",
                "path",
                "pattern",
            ]
            .iter()
            .find_map(|key| field(key));
            match detail {
                Some(detail) => format!("{name} {}", relative(detail, cwd)),
                None => name.to_owned(),
            }
        }
    };
    cut(label.trim_end(), MAX_LABEL_CHARS)
}

/// `path` relative to the run's working directory when it lies inside.
fn relative(path: &str, cwd: Option<&str>) -> String {
    cwd.map(|cwd| cwd.trim_end_matches('/'))
        .filter(|cwd| !cwd.is_empty())
        .and_then(|cwd| path.strip_prefix(cwd))
        .and_then(|rest| rest.strip_prefix('/'))
        .unwrap_or(path)
        .to_owned()
}

fn line_count(text: Option<&str>) -> usize {
    text.map_or(0, |text| text.lines().count())
}

fn pretty(value: &Value) -> String {
    let text = match value {
        Value::Null => String::new(),
        Value::String(text) => text.clone(),
        other => serde_json::to_string_pretty(other).unwrap_or_else(|_| other.to_string()),
    };
    truncate(&text)
}

/// Cuts a tool payload to [`MAX_DETAIL_BYTES`] on a char boundary.
fn truncate(text: &str) -> String {
    if text.len() <= MAX_DETAIL_BYTES {
        return text.to_owned();
    }
    let mut end = MAX_DETAIL_BYTES;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n… ({} more bytes)", &text[..end], text.len() - end)
}

fn cut(text: &str, max_chars: usize) -> String {
    match text.char_indices().nth(max_chars) {
        Some((end, _)) => format!("{}…", &text[..end]),
        None => text.to_owned(),
    }
}
