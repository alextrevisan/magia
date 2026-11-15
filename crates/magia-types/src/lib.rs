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
        /// Attached images, sent to the model with the text.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        images: Vec<Image>,
    },
    Assistant {
        text: String,
        tool_calls: Vec<ToolCall>,
    },
    ToolResult {
        call_id: String,
        content: String,
        is_error: bool,
        /// Images the tool returned (a screenshot, an image file it read).
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        images: Vec<Image>,
    },
}

/// An image attached to a user message: base64 (standard alphabet, no `data:` prefix).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Image {
    /// `image/png`, `image/jpeg`, `image/gif` or `image/webp`.
    pub media_type: String,
    pub data: String,
}

/// `s` cut to its first `n` characters, with " […]" when something was cut.
pub fn clip(s: &str, n: usize) -> String {
    match s.char_indices().nth(n) {
        None => s.to_owned(),
        Some((i, _)) => format!("{} […]", &s[..i]),
    }
}

/// Media type of encoded image bytes, from their magic bytes.
pub fn sniff_image(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some("image/jpeg")
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some("image/gif")
    } else if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        Some("image/webp")
    } else {
        None
    }
}

impl Image {
    pub fn data_url(&self) -> String {
        format!("data:{};base64,{}", self.media_type, self.data)
    }

    /// Decoded size in bytes (approximate, from the base64 length).
    pub fn byte_len(&self) -> usize {
        self.data.len() / 4 * 3
    }
}

impl Message {
    pub fn user(text: impl Into<String>) -> Self {
        Message::User {
            text: text.into(),
            images: Vec::new(),
        }
    }
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
    /// Stable id of the conversation (same for every request of one session). Sent to
    /// providers that route/cache by session, e.g. OpenCode Go's `x-opencode-session`.
    pub session_id: Option<String>,
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
    /// Token limits, when the provider's model list reports them.
    pub limit: Option<ModelLimit>,
}

/// How many tokens a model takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ModelLimit {
    /// The whole context window (prompt + answer).
    pub context: u64,
    /// Most tokens of prompt, when lower than the window allows.
    pub input: Option<u64>,
    /// Most tokens of one answer.
    pub output: Option<u64>,
}

impl ModelLimit {
    /// Read the limits of one model entry, in the shapes providers use: models.dev / OpenCode
    /// (`limit: {context, input, output}`, also in the config file), OpenRouter
    /// (`context_length`, `top_provider.max_completion_tokens`), LM Studio's `/api/v0/models`
    /// (`loaded_context_length`: what the model was loaded with, often far below its
    /// `max_context_length`) and others (`context_window`). None when no window is given.
    pub fn from_json(m: &Value) -> Option<Self> {
        let n = |p: &str| m.pointer(p).and_then(Value::as_u64).filter(|&n| n > 0);
        // LM Studio: a model not loaded yet gets whatever window it is loaded with later.
        if m.get("state").and_then(Value::as_str) == Some("not-loaded") {
            return None;
        }
        let context = n("/limit/context")
            .or_else(|| n("/loaded_context_length"))
            .or_else(|| n("/context_length"))
            .or_else(|| n("/top_provider/context_length"))
            .or_else(|| n("/context_window"))
            .or_else(|| n("/max_context_length"))?;
        Some(Self {
            context,
            input: n("/limit/input"),
            output: n("/limit/output")
                .or_else(|| n("/top_provider/max_completion_tokens"))
                .or_else(|| n("/max_completion_tokens"))
                .or_else(|| n("/max_output_tokens")),
        })
    }

    /// When to compact: at `percent` of the window (or of the prompt cap, when the provider
    /// gives one). The answer gets the rest; the provider shortens it if the prompt grew past
    /// the estimate within a round.
    pub fn compact_at(&self, percent: u8) -> u64 {
        let p = u64::from(percent.min(100));
        let prompt = self.input.unwrap_or(self.context).min(self.context);
        prompt / 100 * p + prompt % 100 * p / 100
    }
}

/// Parse tool-call arguments streamed as a string; empty means `{}`.
pub fn parse_arguments(raw: &str) -> Value {
    if raw.trim().is_empty() {
        return Value::Object(Default::default());
    }
    serde_json::from_str(raw).unwrap_or_else(|_| Value::String(raw.to_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn limits_are_read_from_each_provider_shape() {
        let zen =
            json!({"id": "a", "limit": {"context": 200000, "input": 160000, "output": 32000}});
        let or = json!({"id": "b", "context_length": 1048576,
                        "top_provider": {"context_length": 1048576, "max_completion_tokens": 262144}});
        let groq = json!({"id": "c", "context_window": 131072});
        assert_eq!(
            ModelLimit::from_json(&zen),
            Some(ModelLimit {
                context: 200_000,
                input: Some(160_000),
                output: Some(32_000)
            })
        );
        assert_eq!(ModelLimit::from_json(&or).unwrap().output, Some(262_144));
        assert_eq!(ModelLimit::from_json(&groq).unwrap().context, 131_072);
        assert_eq!(ModelLimit::from_json(&json!({"id": "d"})), None);
        // LM Studio: the loaded window, not the most the model could take.
        let lms = json!({"id": "q", "state": "loaded", "max_context_length": 262144,
                         "loaded_context_length": 65536});
        assert_eq!(ModelLimit::from_json(&lms).unwrap().context, 65_536);
        let idle = json!({"id": "e", "state": "not-loaded", "max_context_length": 2048});
        assert_eq!(ModelLimit::from_json(&idle), None);
        assert_eq!(
            ModelLimit::from_json(&json!({"limit": {"context": 0}})),
            None
        );
    }

    #[test]
    fn compaction_comes_at_95_percent() {
        let l = |context, input, output| ModelLimit {
            context,
            input,
            output,
        };
        assert_eq!(l(200_000, None, Some(32_000)).compact_at(95), 190_000);
        // The prompt cap is what fills up.
        assert_eq!(
            l(200_000, Some(160_000), Some(32_000)).compact_at(95),
            152_000
        );
        assert_eq!(l(1_000_000, None, Some(128_000)).compact_at(95), 950_000);
        assert_eq!(l(65_536, None, None).compact_at(95), 62_259);
        assert_eq!(l(65_536, None, None).compact_at(80), 52_428);
    }
}
