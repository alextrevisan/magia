//! Provider-agnostic message and event types. No I/O.

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "snake_case")]
pub enum Message {
    User {
        text: String,
    },
    Assistant {
        text: String,
        tool_calls: Vec<ToolCall>,
    },
    ToolResult {
        call_id: String,
        content: String,
        is_error: bool,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    /// JSON Schema for the arguments.
    pub schema: Value,
}

#[derive(Debug, Clone, Default)]
pub struct ChatRequest {
    pub model: String,
    pub system: Option<String>,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolSpec>,
    pub max_tokens: Option<u32>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
}

impl Usage {
    pub fn add(&mut self, other: Usage) {
        self.input_tokens += other.input_tokens;
        self.output_tokens += other.output_tokens;
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum StreamEvent {
    TextDelta(String),
    ReasoningDelta(String),
    /// A fully assembled tool call (arguments parsed).
    ToolCall(ToolCall),
    Usage(Usage),
    Done,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ModelInfo {
    pub id: String,
}

/// Parse tool-call arguments streamed as a string; empty means `{}`.
pub fn parse_arguments(raw: &str) -> Value {
    if raw.trim().is_empty() {
        return Value::Object(Default::default());
    }
    serde_json::from_str(raw).unwrap_or_else(|_| Value::String(raw.to_owned()))
}
