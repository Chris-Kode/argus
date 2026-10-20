//! Questions a live run asks the reviewer: tool permissions and
//! `AskUserQuestion`.
//!
//! Runs start with `--permission-mode auto --permission-prompt-tool stdio`
//! and read their input as `stream-json`, so Claude Code decides most tool
//! calls on its own and sends the rest to Argus as a `control_request` line
//! on stdout:
//!
//! ```text
//! {"type":"control_request","request_id":…,"request":{"subtype":"can_use_tool",
//!     "tool_name":"Bash","input":{…},"description":…,"permission_suggestions":[…],
//!     "blocked_path":…,"tool_use_id":…}}
//! ```
//!
//! The run waits until the reviewer answers in the permission dialog; the
//! answer goes back on stdin as a `control_response` line
//! ([`PermissionDecision`]). `AskUserQuestion` arrives the same way: its
//! answers are added to the tool input. A `control_cancel_request` (the
//! tool call was abandoned) withdraws a request.

use argus_store::{CommentId, RunId};
use serde_json::{Map, Value, json};

/// The tool whose permission request carries questions for the reviewer.
pub const ASK_USER_QUESTION: &str = "AskUserQuestion";

/// What a run sent on its control channel.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Control {
    /// A tool call waits for the reviewer.
    CanUseTool(Box<PermissionRequest>),
    /// A request of another subtype; answered with an error so the run does
    /// not wait for it.
    Unsupported {
        /// The request id.
        request_id: String,
        /// The request subtype.
        subtype: String,
    },
    /// The run withdrew a request.
    Cancel {
        /// The request id.
        request_id: String,
    },
}

/// A tool call of a live run that waits for the reviewer.
#[derive(Clone, Debug, PartialEq)]
pub struct PermissionRequest {
    /// The run that asks.
    pub run_id: RunId,
    /// Its comment; `None` for a run on no comment.
    pub comment_id: Option<CommentId>,
    /// The control request id the answer refers to.
    pub request_id: String,
    /// The tool (`Bash`, `WebFetch`, `mcp__…`, [`ASK_USER_QUESTION`], ...).
    pub tool_name: String,
    /// The tool name as Claude Code shows it.
    pub display_name: String,
    /// What the call does, in Claude's words.
    pub description: Option<String>,
    /// The tool input.
    pub input: Value,
    /// The path outside the allowed directories the call touches.
    pub blocked_path: Option<String>,
    /// Rules Claude Code offers to remember the approval with; "Allow for
    /// this run" sends them back scoped to the session.
    pub suggestions: Vec<Value>,
    /// The questions of an [`ASK_USER_QUESTION`] call.
    pub questions: Vec<Question>,
}

/// One question of an `AskUserQuestion` call.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Question {
    /// The question text; answers are keyed by it.
    pub question: String,
    /// A short label.
    pub header: String,
    /// The choices.
    pub options: Vec<QuestionOption>,
    /// Whether several options may be picked.
    pub multi_select: bool,
}

/// One choice of a [`Question`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QuestionOption {
    /// The answer text.
    pub label: String,
    /// What picking it means.
    pub description: String,
}

/// The reviewer's answer to a [`PermissionRequest`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PermissionDecision {
    /// Run this call.
    Allow,
    /// Run this call and allow the same kind of call for the rest of the run
    /// (the request's suggestions, scoped to the session).
    AllowForRun,
    /// Refuse the call; `message` tells Claude why or what to do instead.
    Deny {
        /// The reviewer's words, if any.
        message: Option<String>,
    },
    /// The answers of an `AskUserQuestion` call, by question text; several
    /// picks of a multi-select question are joined with `", "`.
    Answer(Vec<(String, String)>),
}

impl PermissionRequest {
    /// Whether this is an `AskUserQuestion` call.
    pub fn is_question(&self) -> bool {
        self.tool_name == ASK_USER_QUESTION && !self.questions.is_empty()
    }

    /// Whether "Allow for this run" applies: Claude Code offered rules.
    pub fn can_allow_for_run(&self) -> bool {
        !self.session_rules().is_empty()
    }

    /// The main argument of the call, for the dialog: the command of `Bash`,
    /// the path of file tools, the URL of `WebFetch`, the query of
    /// `WebSearch`; the whole input as JSON otherwise.
    pub fn input_preview(&self) -> String {
        let field = |key: &str| self.input.get(key).and_then(Value::as_str);
        let main = match self.tool_name.as_str() {
            "Bash" => field("command"),
            "Read" | "Edit" | "Write" | "MultiEdit" | "NotebookEdit" => {
                field("file_path").or_else(|| field("notebook_path"))
            }
            "WebFetch" => field("url"),
            "WebSearch" => field("query"),
            "Glob" | "Grep" => field("pattern"),
            _ => None,
        };
        match main {
            Some(text) => text.to_owned(),
            None => serde_json::to_string_pretty(&self.input).unwrap_or_default(),
        }
    }

    /// The suggestions to remember for the rest of the run: rules and
    /// directories, moved to the `session` destination so nothing is written
    /// to a settings file. Mode changes are left out; the run keeps its
    /// permission mode.
    fn session_rules(&self) -> Vec<Value> {
        self.suggestions
            .iter()
            .filter(|suggestion| suggestion.get("type").and_then(Value::as_str) != Some("setMode"))
            .filter_map(|suggestion| {
                let mut suggestion = suggestion.as_object()?.clone();
                suggestion.insert("destination".to_owned(), json!("session"));
                Some(Value::Object(suggestion))
            })
            .collect()
    }

    /// The `control_response` line (without its newline) for `decision`.
    pub(crate) fn response_line(&self, decision: &PermissionDecision) -> String {
        let response = match decision {
            PermissionDecision::Allow => json!({
                "behavior": "allow",
                "updatedInput": self.input,
            }),
            PermissionDecision::AllowForRun => json!({
                "behavior": "allow",
                "updatedInput": self.input,
                "updatedPermissions": self.session_rules(),
            }),
            PermissionDecision::Deny { message } => json!({
                "behavior": "deny",
                "message": message
                    .as_deref()
                    .map(str::trim)
                    .filter(|message| !message.is_empty())
                    .unwrap_or(if self.is_question() {
                        "The reviewer declined to answer in Argus."
                    } else {
                        "The reviewer denied this in Argus."
                    }),
            }),
            PermissionDecision::Answer(answers) => {
                let mut input = self.input.as_object().cloned().unwrap_or_default();
                let answers: Map<String, Value> = answers
                    .iter()
                    .map(|(question, answer)| (question.clone(), json!(answer)))
                    .collect();
                input.insert("answers".to_owned(), Value::Object(answers));
                json!({ "behavior": "allow", "updatedInput": input })
            }
        };
        json!({
            "type": "control_response",
            "response": {
                "subtype": "success",
                "request_id": self.request_id,
                "response": response,
            },
        })
        .to_string()
    }
}

/// The error `control_response` line for a request Argus does not handle.
pub(crate) fn unsupported_line(request_id: &str, subtype: &str) -> String {
    json!({
        "type": "control_response",
        "response": {
            "subtype": "error",
            "request_id": request_id,
            "error": format!("Argus does not handle `{subtype}` requests"),
        },
    })
    .to_string()
}

/// The first `stream-json` input line of a run: the prompt as a user
/// message.
pub(crate) fn user_message_line(prompt: &str) -> String {
    json!({
        "type": "user",
        "message": { "role": "user", "content": prompt },
        "parent_tool_use_id": null,
        "session_id": "",
    })
    .to_string()
}

/// The control message of a stdout line, when it is one.
pub(crate) fn parse_control(
    value: &Value,
    run_id: RunId,
    comment_id: Option<CommentId>,
) -> Option<Control> {
    let text = |value: &Value, key: &str| value.get(key).and_then(Value::as_str).map(str::to_owned);
    match value.get("type").and_then(Value::as_str)? {
        "control_cancel_request" => Some(Control::Cancel {
            request_id: text(value, "request_id")?,
        }),
        "control_request" => {
            let request_id = text(value, "request_id")?;
            let request = value.get("request")?;
            let subtype = text(request, "subtype").unwrap_or_default();
            if subtype != "can_use_tool" {
                return Some(Control::Unsupported {
                    request_id,
                    subtype,
                });
            }
            let tool_name = text(request, "tool_name").unwrap_or_default();
            let input = request.get("input").cloned().unwrap_or(json!({}));
            let questions = if tool_name == ASK_USER_QUESTION {
                parse_questions(&input)
            } else {
                Vec::new()
            };
            Some(Control::CanUseTool(Box::new(PermissionRequest {
                run_id,
                comment_id,
                request_id,
                display_name: text(request, "display_name").unwrap_or_else(|| tool_name.clone()),
                tool_name,
                description: text(request, "description").filter(|text| !text.trim().is_empty()),
                input,
                blocked_path: text(request, "blocked_path"),
                suggestions: request
                    .get("permission_suggestions")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default(),
                questions,
            })))
        }
        _ => None,
    }
}

fn parse_questions(input: &Value) -> Vec<Question> {
    let text = |value: &Value, key: &str| {
        value
            .get(key)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned()
    };
    input
        .get("questions")
        .and_then(Value::as_array)
        .map(|questions| {
            questions
                .iter()
                .filter_map(|question| {
                    let text_of_question = question.get("question")?.as_str()?.to_owned();
                    Some(Question {
                        question: text_of_question,
                        header: text(question, "header"),
                        options: question
                            .get("options")
                            .and_then(Value::as_array)
                            .map(|options| {
                                options
                                    .iter()
                                    .filter_map(|option| {
                                        Some(QuestionOption {
                                            label: option.get("label")?.as_str()?.to_owned(),
                                            description: text(option, "description"),
                                        })
                                    })
                                    .collect()
                            })
                            .unwrap_or_default(),
                        multi_select: question
                            .get("multiSelect")
                            .and_then(Value::as_bool)
                            .unwrap_or(false),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}
