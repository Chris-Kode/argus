//! The `stream-json` output of `claude -p --output-format stream-json
//! --verbose` (spec §6.3).
//!
//! Every stdout line is one JSON object. [`parse_line`] turns it into a
//! typed [`StreamEvent`]; anything Argus does not model (new event types,
//! system notices, fields that changed shape) becomes
//! [`StreamEvent::Unknown`] with the raw value, so a newer Claude Code never
//! breaks a run. Only a line that is not JSON at all is an error, and the run
//! manager logs and skips it.
//!
//! The shapes follow Claude Code 2.1:
//!
//! ```text
//! {"type":"system","subtype":"init","session_id":…,"model":…,"cwd":…,"tools":[…],…}
//! {"type":"assistant","message":{"content":[{"type":"text","text":…},
//!     {"type":"tool_use","id":…,"name":"Edit","input":{…}}],…},…}
//! {"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":…,
//!     "content":…,"is_error":false}],…},…}
//! {"type":"result","subtype":"success","is_error":false,"num_turns":3,
//!     "total_cost_usd":0.04,"duration_ms":12000,"result":"…",…}
//! ```

use serde::Deserialize;
use serde_json::Value;

/// One parsed line of a run's stdout.
#[derive(Clone, Debug, PartialEq)]
pub enum StreamEvent {
    /// `system/init`: the session started.
    System(Init),
    /// An assistant message: text, tool calls, thinking.
    Assistant(AssistantMessage),
    /// A user message; in a `-p` run these carry tool results.
    User(UserMessage),
    /// The final `result` line.
    Result(RunResult),
    /// Any other line, kept as is.
    Unknown(Value),
}

/// The `system/init` event.
#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct Init {
    /// Claude session UUID.
    pub session_id: Option<String>,
    /// Model actually used.
    pub model: Option<String>,
    /// Working directory of the run.
    pub cwd: Option<String>,
    /// Tools available to the session.
    pub tools: Vec<String>,
    /// Permission mode in effect.
    #[serde(rename = "permissionMode")]
    pub permission_mode: Option<String>,
    /// Version of Claude Code.
    pub claude_code_version: Option<String>,
}

/// The content blocks of an assistant message.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct AssistantMessage {
    /// Blocks in message order.
    pub content: Vec<AssistantContent>,
}

impl AssistantMessage {
    /// The text blocks joined by blank lines, or `None` without text.
    pub fn text(&self) -> Option<String> {
        let texts: Vec<&str> = self
            .content
            .iter()
            .filter_map(|block| match block {
                AssistantContent::Text(text) if !text.trim().is_empty() => Some(text.as_str()),
                _ => None,
            })
            .collect();
        (!texts.is_empty()).then(|| texts.join("\n\n"))
    }
}

/// One block of an assistant message.
#[derive(Clone, Debug, PartialEq)]
pub enum AssistantContent {
    /// Visible text.
    Text(String),
    /// A tool call.
    ToolUse {
        /// Tool call id, matched by the [`ToolResult`].
        id: String,
        /// Tool name (`Read`, `Edit`, `Bash`, ...).
        name: String,
        /// Tool input, as sent by the model.
        input: Value,
    },
    /// Extended thinking text.
    Thinking(String),
    /// A block of another type.
    Other(Value),
}

/// The content blocks of a user message.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct UserMessage {
    /// Blocks in message order.
    pub content: Vec<UserContent>,
}

/// One block of a user message.
#[derive(Clone, Debug, PartialEq)]
pub enum UserContent {
    /// The result of a tool call.
    ToolResult(ToolResult),
    /// Plain text.
    Text(String),
    /// A block of another type.
    Other(Value),
}

/// The result of a tool call.
#[derive(Clone, Debug, PartialEq)]
pub struct ToolResult {
    /// The [`AssistantContent::ToolUse`] id it answers.
    pub tool_use_id: String,
    /// The result text; text blocks of a structured result are joined.
    pub content: String,
    /// Whether the tool failed.
    pub is_error: bool,
}

/// The final `result` event.
#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct RunResult {
    /// `success`, `error_max_turns`, `error_during_execution`, ...
    pub subtype: String,
    /// Whether the run ended in an error.
    pub is_error: bool,
    /// Number of turns.
    pub num_turns: Option<u32>,
    /// Total cost in USD.
    pub total_cost_usd: Option<f64>,
    /// Duration reported by Claude.
    pub duration_ms: Option<i64>,
    /// Claude's final message (or the error text).
    pub result: Option<String>,
    /// Claude session UUID.
    pub session_id: Option<String>,
}

/// A line that is not a JSON object.
#[derive(Debug, thiserror::Error)]
#[error("malformed stream-json line: {0}")]
pub struct MalformedLine(#[from] serde_json::Error);

impl StreamEvent {
    /// The `run_events.type` of the event: `system/init`, `assistant`,
    /// `user`, `result`, or the raw `type[/subtype]` of an unknown line.
    pub fn event_type(&self) -> String {
        match self {
            Self::System(_) => "system/init".to_owned(),
            Self::Assistant(_) => "assistant".to_owned(),
            Self::User(_) => "user".to_owned(),
            Self::Result(_) => "result".to_owned(),
            Self::Unknown(value) => raw_type(value),
        }
    }
}

/// Parses one stdout line. Lines that are JSON but not a known event (or a
/// known event whose fields do not fit) give [`StreamEvent::Unknown`].
pub fn parse_line(line: &str) -> Result<StreamEvent, MalformedLine> {
    let value: Value = serde_json::from_str(line)?;
    Ok(parse_value(value))
}

/// Types a parsed JSON line.
pub fn parse_value(value: Value) -> StreamEvent {
    let kind = value.get("type").and_then(Value::as_str);
    let subtype = value.get("subtype").and_then(Value::as_str);
    let parsed = match (kind, subtype) {
        (Some("system"), Some("init")) => Init::deserialize(&value).ok().map(StreamEvent::System),
        (Some("assistant"), _) => {
            content_blocks(&value).map(|blocks| StreamEvent::Assistant(assistant(blocks)))
        }
        (Some("user"), _) => {
            message_content(&value).map(|content| StreamEvent::User(user(content)))
        }
        (Some("result"), _) => RunResult::deserialize(&value).ok().map(StreamEvent::Result),
        _ => None,
    };
    parsed.unwrap_or(StreamEvent::Unknown(value))
}

fn raw_type(value: &Value) -> String {
    let kind = value.get("type").and_then(Value::as_str);
    let subtype = value.get("subtype").and_then(Value::as_str);
    match (kind, subtype) {
        (Some(kind), Some(subtype)) => format!("{kind}/{subtype}"),
        (Some(kind), None) => kind.to_owned(),
        (None, _) => "unknown".to_owned(),
    }
}

fn message_content(value: &Value) -> Option<&Value> {
    value.get("message")?.get("content")
}

fn content_blocks(value: &Value) -> Option<&Vec<Value>> {
    message_content(value)?.as_array()
}

fn assistant(blocks: &[Value]) -> AssistantMessage {
    let content = blocks
        .iter()
        .map(|block| {
            let text = |key: &str| block.get(key).and_then(Value::as_str).map(str::to_owned);
            match block.get("type").and_then(Value::as_str) {
                Some("text") => text("text").map(AssistantContent::Text),
                Some("tool_use") => block.get("name").and_then(Value::as_str).map(|name| {
                    AssistantContent::ToolUse {
                        id: text("id").unwrap_or_default(),
                        name: name.to_owned(),
                        input: block.get("input").cloned().unwrap_or(Value::Null),
                    }
                }),
                Some("thinking") => text("thinking").map(AssistantContent::Thinking),
                _ => None,
            }
            .unwrap_or_else(|| AssistantContent::Other(block.clone()))
        })
        .collect();
    AssistantMessage { content }
}

fn user(content: &Value) -> UserMessage {
    let content = match content {
        Value::String(text) => vec![UserContent::Text(text.clone())],
        Value::Array(blocks) => blocks
            .iter()
            .map(|block| match block.get("type").and_then(Value::as_str) {
                Some("tool_result") => UserContent::ToolResult(ToolResult {
                    tool_use_id: block
                        .get("tool_use_id")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned(),
                    content: block.get("content").map(flatten_text).unwrap_or_default(),
                    is_error: block
                        .get("is_error")
                        .and_then(Value::as_bool)
                        .unwrap_or(false),
                }),
                Some("text") => match block.get("text").and_then(Value::as_str) {
                    Some(text) => UserContent::Text(text.to_owned()),
                    None => UserContent::Other(block.clone()),
                },
                _ => UserContent::Other(block.clone()),
            })
            .collect(),
        other => vec![UserContent::Other(other.clone())],
    };
    UserMessage { content }
}

/// A tool result's content is a string or a list of content blocks.
fn flatten_text(content: &Value) -> String {
    match content {
        Value::String(text) => text.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .filter_map(|block| block.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}
