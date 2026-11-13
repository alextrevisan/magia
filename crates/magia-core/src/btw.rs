//! `/btw`: a side question about the conversation, answered while the agent may be working.
//! The answer uses the conversation as context but is never added to it, and no tool runs.
//!
//! During a turn the agent is locked, so each model request it makes is also published in a
//! shared slot ([`SideSlot`]); a side question reuses that request (the same `Arc`, no copy).

use crate::agent::Agent;
use anyhow::Result;
use futures_util::StreamExt;
use magia_providers::Client;
use magia_types::{ChatRequest, Message, StreamEvent};
use std::sync::{Arc, Mutex};

/// The agent's latest request and the client it went to; `None` while no turn runs.
pub type SideSlot = Arc<Mutex<Option<Arc<SideContext>>>>;

pub struct SideContext {
    pub client: Client,
    pub req: ChatRequest,
}

const NOTE: &str = "[side question: the user asks this while you keep working on the main task. \
Answer briefly from what this conversation already shows. Do not call any tool: none will run, \
and this exchange is not kept in the conversation.]";

impl SideContext {
    /// The context of an idle agent (the same request its next step would send).
    pub fn of(agent: &Agent, interactive: bool) -> Self {
        Self {
            client: agent.client.clone(),
            req: agent.request(interactive),
        }
    }

    /// The request for `question`. The tool list stays (a history with tool calls needs it,
    /// and an unchanged prefix keeps the provider's prompt cache); calls are ignored.
    pub fn question(&self, question: &str) -> ChatRequest {
        let mut req = self.req.clone();
        req.messages
            .push(Message::user(format!("{NOTE}\n\n{}", question.trim())));
        req.max_tokens = Some(2048);
        req
    }
}

/// Stream the answer; `on_text` gets each new piece of text.
pub async fn answer(
    ctx: &SideContext,
    question: &str,
    mut on_text: impl FnMut(&str),
) -> Result<String> {
    let req = ctx.question(question);
    let mut stream = ctx.client.stream(&req).await?;
    let mut text = String::new();
    while let Some(ev) = stream.next().await {
        match ev? {
            StreamEvent::TextDelta(t) => {
                on_text(&t);
                text.push_str(&t);
            }
            StreamEvent::Done => break,
            _ => {} // reasoning, usage, and tool calls (which never run)
        }
    }
    if text.trim().is_empty() {
        anyhow::bail!("the model gave no answer");
    }
    Ok(text)
}
