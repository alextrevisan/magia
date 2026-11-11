//! Wire formats: request building and SSE decoding. Pure functions, unit-testable
//! with recorded fixtures (no network).

use crate::sse::SseEvent;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use ufrh_types::{ChatRequest, Message, StreamEvent, ToolCall, Usage, parse_arguments};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Wire {
    ChatCompletions,
    Responses,
    Messages,
}

/// Decodes the SSE events of one response into normalized [`StreamEvent`]s.
pub trait Decoder: Send {
    fn feed(&mut self, ev: &SseEvent) -> Vec<StreamEvent>;
    /// Called when the byte stream ends; flushes anything still pending.
    fn finish(&mut self) -> Vec<StreamEvent>;
}

pub fn decoder(wire: Wire) -> Box<dyn Decoder> {
    match wire {
        Wire::ChatCompletions => Box::<ChatDecoder>::default(),
        Wire::Responses => Box::<ResponsesDecoder>::default(),
        Wire::Messages => Box::<MessagesDecoder>::default(),
    }
}

pub fn build_body(wire: Wire, req: &ChatRequest) -> Value {
    match wire {
        Wire::ChatCompletions => chat_body(req),
        Wire::Responses => responses_body(req),
        Wire::Messages => messages_body(req),
    }
}

fn args_string(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

// ───────────────────────── Chat Completions ─────────────────────────

fn chat_body(req: &ChatRequest) -> Value {
    let mut msgs = Vec::new();
    if let Some(sys) = &req.system {
        msgs.push(json!({"role": "system", "content": sys}));
    }
    for m in &req.messages {
        match m {
            Message::User { text } => msgs.push(json!({"role": "user", "content": text})),
            Message::Assistant { text, tool_calls } => {
                let mut o = json!({"role": "assistant", "content": if text.is_empty() { Value::Null } else { json!(text) }});
                if !tool_calls.is_empty() {
                    o["tool_calls"] = tool_calls
                        .iter()
                        .map(|c| {
                            json!({"id": c.id, "type": "function",
                            "function": {"name": c.name, "arguments": args_string(&c.arguments)}})
                        })
                        .collect();
                }
                msgs.push(o);
            }
            Message::ToolResult {
                call_id, content, ..
            } => {
                msgs.push(json!({"role": "tool", "tool_call_id": call_id, "content": content}));
            }
        }
    }
    let mut body = json!({
        "model": req.model,
        "messages": msgs,
        "stream": true,
        "stream_options": {"include_usage": true},
    });
    if !req.tools.is_empty() {
        body["tools"] = req
            .tools
            .iter()
            .map(|t| {
                json!({"type": "function", "function": {
                "name": t.name, "description": t.description, "parameters": t.schema}})
            })
            .collect();
    }
    if let Some(m) = req.max_tokens {
        body["max_tokens"] = json!(m);
    }
    body
}

#[derive(Default)]
struct PartialCall {
    id: String,
    name: String,
    args: String,
}

#[derive(Default)]
pub struct ChatDecoder {
    calls: BTreeMap<u64, PartialCall>,
    flushed: bool,
}

impl ChatDecoder {
    fn flush(&mut self) -> Vec<StreamEvent> {
        if self.flushed {
            return vec![];
        }
        self.flushed = true;
        std::mem::take(&mut self.calls)
            .into_values()
            .map(|c| {
                StreamEvent::ToolCall(ToolCall {
                    id: c.id,
                    name: c.name,
                    arguments: parse_arguments(&c.args),
                })
            })
            .collect()
    }
}

impl Decoder for ChatDecoder {
    fn feed(&mut self, ev: &SseEvent) -> Vec<StreamEvent> {
        if ev.data.trim() == "[DONE]" {
            let mut out = self.flush();
            out.push(StreamEvent::Done);
            return out;
        }
        let Ok(v) = serde_json::from_str::<Value>(&ev.data) else {
            return vec![];
        };
        let mut out = Vec::new();
        if let Some(delta) = v.pointer("/choices/0/delta") {
            if let Some(t) = delta.get("content").and_then(Value::as_str)
                && !t.is_empty()
            {
                out.push(StreamEvent::TextDelta(t.to_owned()));
            }
            for key in ["reasoning_content", "reasoning"] {
                if let Some(t) = delta.get(key).and_then(Value::as_str)
                    && !t.is_empty()
                {
                    out.push(StreamEvent::ReasoningDelta(t.to_owned()));
                }
            }
            if let Some(calls) = delta.get("tool_calls").and_then(Value::as_array) {
                for c in calls {
                    let idx = c.get("index").and_then(Value::as_u64).unwrap_or(0);
                    let e = self.calls.entry(idx).or_default();
                    if let Some(id) = c.get("id").and_then(Value::as_str) {
                        e.id = id.to_owned();
                    }
                    if let Some(n) = c.pointer("/function/name").and_then(Value::as_str) {
                        e.name.push_str(n);
                    }
                    if let Some(a) = c.pointer("/function/arguments").and_then(Value::as_str) {
                        e.args.push_str(a);
                    }
                }
            }
        }
        if v.pointer("/choices/0/finish_reason")
            .is_some_and(|f| !f.is_null())
        {
            out.extend(self.flush());
        }
        if let Some(u) = v.get("usage").filter(|u| !u.is_null()) {
            out.push(StreamEvent::Usage(Usage {
                input_tokens: u.get("prompt_tokens").and_then(Value::as_u64).unwrap_or(0),
                output_tokens: u
                    .get("completion_tokens")
                    .and_then(Value::as_u64)
                    .unwrap_or(0),
            }));
        }
        out
    }

    fn finish(&mut self) -> Vec<StreamEvent> {
        let mut out = self.flush();
        out.push(StreamEvent::Done);
        out
    }
}

// ───────────────────────── Responses ─────────────────────────

fn responses_body(req: &ChatRequest) -> Value {
    let mut input = Vec::new();
    for m in &req.messages {
        match m {
            Message::User { text } => input.push(json!({"role": "user", "content": text})),
            Message::Assistant { text, tool_calls } => {
                if !text.is_empty() {
                    input.push(json!({"role": "assistant", "content": text}));
                }
                for c in tool_calls {
                    input.push(json!({"type": "function_call", "call_id": c.id,
                        "name": c.name, "arguments": args_string(&c.arguments)}));
                }
            }
            Message::ToolResult {
                call_id, content, ..
            } => {
                input.push(
                    json!({"type": "function_call_output", "call_id": call_id, "output": content}),
                );
            }
        }
    }
    let mut body = json!({"model": req.model, "input": input, "stream": true, "store": false});
    if let Some(sys) = &req.system {
        body["instructions"] = json!(sys);
    }
    if !req.tools.is_empty() {
        body["tools"] = req
            .tools
            .iter()
            .map(|t| {
                json!({"type": "function", "name": t.name,
                "description": t.description, "parameters": t.schema})
            })
            .collect();
    }
    if let Some(m) = req.max_tokens {
        body["max_output_tokens"] = json!(m);
    }
    body
}

#[derive(Default)]
pub struct ResponsesDecoder {
    done: bool,
}

impl Decoder for ResponsesDecoder {
    fn feed(&mut self, ev: &SseEvent) -> Vec<StreamEvent> {
        let Ok(v) = serde_json::from_str::<Value>(&ev.data) else {
            return vec![];
        };
        let kind = v
            .get("type")
            .and_then(Value::as_str)
            .or(ev.event.as_deref())
            .unwrap_or("");
        let s = |p: &str| v.pointer(p).and_then(Value::as_str).map(str::to_owned);
        match kind {
            "response.output_text.delta" => s("/delta")
                .map(StreamEvent::TextDelta)
                .into_iter()
                .collect(),
            "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
                s("/delta")
                    .map(StreamEvent::ReasoningDelta)
                    .into_iter()
                    .collect()
            }
            "response.output_item.done"
                if v.pointer("/item/type").and_then(Value::as_str) == Some("function_call") =>
            {
                vec![StreamEvent::ToolCall(ToolCall {
                    id: s("/item/call_id")
                        .or_else(|| s("/item/id"))
                        .unwrap_or_default(),
                    name: s("/item/name").unwrap_or_default(),
                    arguments: parse_arguments(&s("/item/arguments").unwrap_or_default()),
                })]
            }
            "response.completed" | "response.incomplete" => {
                self.done = true;
                let mut out = Vec::new();
                if let Some(u) = v.pointer("/response/usage") {
                    out.push(StreamEvent::Usage(Usage {
                        input_tokens: u.get("input_tokens").and_then(Value::as_u64).unwrap_or(0),
                        output_tokens: u.get("output_tokens").and_then(Value::as_u64).unwrap_or(0),
                    }));
                }
                out.push(StreamEvent::Done);
                out
            }
            "response.failed" | "error" => {
                // Surfaced by the caller as an error through an empty Done; keep message in text.
                let msg = s("/response/error/message")
                    .or_else(|| s("/message"))
                    .unwrap_or_default();
                vec![StreamEvent::TextDelta(format!(
                    "\n[provider error: {msg}]\n"
                ))]
            }
            _ => vec![],
        }
    }

    fn finish(&mut self) -> Vec<StreamEvent> {
        if self.done {
            vec![]
        } else {
            vec![StreamEvent::Done]
        }
    }
}

// ───────────────────────── Anthropic Messages ─────────────────────────

fn messages_body(req: &ChatRequest) -> Value {
    let mut msgs: Vec<Value> = Vec::new();
    for m in &req.messages {
        match m {
            Message::User { text } => msgs.push(json!({"role": "user", "content": text})),
            Message::Assistant { text, tool_calls } => {
                let mut blocks = Vec::new();
                if !text.is_empty() {
                    blocks.push(json!({"type": "text", "text": text}));
                }
                for c in tool_calls {
                    let input = if c.arguments.is_object() {
                        c.arguments.clone()
                    } else {
                        json!({})
                    };
                    blocks.push(
                        json!({"type": "tool_use", "id": c.id, "name": c.name, "input": input}),
                    );
                }
                msgs.push(json!({"role": "assistant", "content": blocks}));
            }
            Message::ToolResult {
                call_id,
                content,
                is_error,
            } => {
                let block = json!({"type": "tool_result", "tool_use_id": call_id,
                    "content": content, "is_error": is_error});
                // Consecutive tool results must share one user message.
                if let Some(last) = msgs.last_mut()
                    && last["role"] == "user"
                    && let Some(arr) = last["content"].as_array_mut()
                    && arr.first().is_some_and(|b| b["type"] == "tool_result")
                {
                    arr.push(block);
                    continue;
                }
                msgs.push(json!({"role": "user", "content": [block]}));
            }
        }
    }
    let mut body = json!({
        "model": req.model,
        "max_tokens": req.max_tokens.unwrap_or(8192),
        "messages": msgs,
        "stream": true,
    });
    // Prompt caching: mark the stable prefix (system prompt and the tool list).
    if let Some(sys) = &req.system {
        body["system"] =
            json!([{"type": "text", "text": sys, "cache_control": {"type": "ephemeral"}}]);
    }
    if !req.tools.is_empty() {
        let mut tools: Vec<Value> = req
            .tools
            .iter()
            .map(
                |t| json!({"name": t.name, "description": t.description, "input_schema": t.schema}),
            )
            .collect();
        if let Some(last) = tools.last_mut() {
            last["cache_control"] = json!({"type": "ephemeral"});
        }
        body["tools"] = Value::Array(tools);
    }
    body
}

#[derive(Default)]
pub struct MessagesDecoder {
    blocks: BTreeMap<u64, PartialCall>,
    usage: Usage,
    done: bool,
}

impl Decoder for MessagesDecoder {
    fn feed(&mut self, ev: &SseEvent) -> Vec<StreamEvent> {
        let Ok(v) = serde_json::from_str::<Value>(&ev.data) else {
            return vec![];
        };
        let kind = v
            .get("type")
            .and_then(Value::as_str)
            .or(ev.event.as_deref())
            .unwrap_or("");
        let idx = v.get("index").and_then(Value::as_u64).unwrap_or(0);
        match kind {
            "message_start" => {
                if let Some(n) = v
                    .pointer("/message/usage/input_tokens")
                    .and_then(Value::as_u64)
                {
                    self.usage.input_tokens = n;
                }
                vec![]
            }
            "content_block_start" => {
                if v.pointer("/content_block/type").and_then(Value::as_str) == Some("tool_use") {
                    let c = self.blocks.entry(idx).or_default();
                    c.id = v
                        .pointer("/content_block/id")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .into();
                    c.name = v
                        .pointer("/content_block/name")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .into();
                }
                vec![]
            }
            "content_block_delta" => match v.pointer("/delta/type").and_then(Value::as_str) {
                Some("text_delta") => v
                    .pointer("/delta/text")
                    .and_then(Value::as_str)
                    .map(|t| StreamEvent::TextDelta(t.to_owned()))
                    .into_iter()
                    .collect(),
                Some("thinking_delta") => v
                    .pointer("/delta/thinking")
                    .and_then(Value::as_str)
                    .map(|t| StreamEvent::ReasoningDelta(t.to_owned()))
                    .into_iter()
                    .collect(),
                Some("input_json_delta") => {
                    if let Some(p) = v.pointer("/delta/partial_json").and_then(Value::as_str) {
                        self.blocks.entry(idx).or_default().args.push_str(p);
                    }
                    vec![]
                }
                _ => vec![],
            },
            "content_block_stop" => self
                .blocks
                .remove(&idx)
                .map(|c| {
                    StreamEvent::ToolCall(ToolCall {
                        id: c.id,
                        name: c.name,
                        arguments: parse_arguments(&c.args),
                    })
                })
                .into_iter()
                .collect(),
            "message_delta" => {
                if let Some(n) = v.pointer("/usage/output_tokens").and_then(Value::as_u64) {
                    self.usage.output_tokens = n;
                }
                vec![]
            }
            "message_stop" => {
                self.done = true;
                vec![StreamEvent::Usage(self.usage), StreamEvent::Done]
            }
            "error" => {
                let msg = v
                    .pointer("/error/message")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                vec![StreamEvent::TextDelta(format!(
                    "\n[provider error: {msg}]\n"
                ))]
            }
            _ => vec![],
        }
    }

    fn finish(&mut self) -> Vec<StreamEvent> {
        if self.done {
            vec![]
        } else {
            vec![StreamEvent::Done]
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sse::SseParser;

    fn run(wire: Wire, fixture: &str) -> Vec<StreamEvent> {
        let mut p = SseParser::default();
        let mut d = decoder(wire);
        let mut out = Vec::new();
        for ev in p.push(fixture.as_bytes()) {
            out.extend(d.feed(&ev));
        }
        out.extend(d.finish());
        out
    }

    #[test]
    fn chat_text_and_tool_call() {
        let fx = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"Hi\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"c1\",\"function\":{\"name\":\"read\",\"arguments\":\"{\\\"file_\"}}]}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"path\\\":\\\"a\\\"}\"}}]}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n",
            "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":5}}\n\n",
            "data: [DONE]\n\n",
        );
        let ev = run(Wire::ChatCompletions, fx);
        assert_eq!(ev[0], StreamEvent::TextDelta("Hi".into()));
        assert_eq!(
            ev[1],
            StreamEvent::ToolCall(ToolCall {
                id: "c1".into(),
                name: "read".into(),
                arguments: json!({"file_path": "a"})
            })
        );
        assert!(ev.contains(&StreamEvent::Usage(Usage {
            input_tokens: 10,
            output_tokens: 5
        })));
        assert_eq!(ev.last(), Some(&StreamEvent::Done));
        assert_eq!(
            ev.iter()
                .filter(|e| matches!(e, StreamEvent::ToolCall(_)))
                .count(),
            1
        );
    }

    #[test]
    fn messages_tool_use() {
        let fx = concat!(
            "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":7}}}\n\n",
            "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"tool_use\",\"id\":\"t1\",\"name\":\"bash\"}}\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"command\\\":\\\"ls\\\"}\"}}\n\n",
            "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":1}\n\n",
            "event: message_delta\ndata: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":3}}\n\n",
            "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
        );
        let ev = run(Wire::Messages, fx);
        assert_eq!(
            ev[0],
            StreamEvent::ToolCall(ToolCall {
                id: "t1".into(),
                name: "bash".into(),
                arguments: json!({"command": "ls"})
            })
        );
        assert_eq!(
            ev[1],
            StreamEvent::Usage(Usage {
                input_tokens: 7,
                output_tokens: 3
            })
        );
        assert_eq!(ev[2], StreamEvent::Done);
    }

    #[test]
    fn responses_function_call() {
        let fx = concat!(
            "event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"ok\"}\n\n",
            "data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"function_call\",\"call_id\":\"f1\",\"name\":\"ls\",\"arguments\":\"{}\"}}\n\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":4,\"output_tokens\":2}}}\n\n",
        );
        let ev = run(Wire::Responses, fx);
        assert_eq!(ev[0], StreamEvent::TextDelta("ok".into()));
        assert!(matches!(&ev[1], StreamEvent::ToolCall(c) if c.id == "f1" && c.name == "ls"));
        assert_eq!(ev.last(), Some(&StreamEvent::Done));
    }

    #[test]
    fn messages_body_merges_tool_results() {
        let req = ChatRequest {
            model: "m".into(),
            messages: vec![
                Message::User { text: "go".into() },
                Message::Assistant {
                    text: String::new(),
                    tool_calls: vec![
                        ToolCall {
                            id: "a".into(),
                            name: "x".into(),
                            arguments: json!({}),
                        },
                        ToolCall {
                            id: "b".into(),
                            name: "y".into(),
                            arguments: json!({}),
                        },
                    ],
                },
                Message::ToolResult {
                    call_id: "a".into(),
                    content: "1".into(),
                    is_error: false,
                },
                Message::ToolResult {
                    call_id: "b".into(),
                    content: "2".into(),
                    is_error: false,
                },
            ],
            ..Default::default()
        };
        let body = messages_body(&req);
        let msgs = body["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 3);
        assert_eq!(msgs[2]["content"].as_array().unwrap().len(), 2);
    }
}
