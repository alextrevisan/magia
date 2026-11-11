use crate::ext::{SkillTool, discover_skills, expand_mentions};
use crate::hooks::Hooks;
use anyhow::{Result, bail};
use async_trait::async_trait;
use futures_util::StreamExt;
use futures_util::future::{BoxFuture, FutureExt, join_all};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use tokio::sync::mpsc::{UnboundedSender, unbounded_channel};
use tokio_util::sync::CancellationToken;
use ufrh_providers::Client;
use ufrh_store::SessionFile;
use ufrh_tools::{Decision, Effect, PermissionMode, Registry, RuleSet, Tool, ToolCx, decide};
use ufrh_types::{ChatRequest, Image, Message, StreamEvent, ToolCall, ToolSpec, Usage};

#[derive(Debug, Clone)]
pub enum AgentEvent {
    TextDelta(String),
    ReasoningDelta(String),
    ToolStart {
        id: String,
        name: String,
        input: Value,
    },
    ToolEnd {
        id: String,
        output: String,
        is_error: bool,
    },
    Usage(Usage),
    /// Informational (e.g. "context compacted").
    Notice(String),
    Error(String),
    /// Turn finished (completed, cancelled or failed).
    TurnDone,
}

/// The user side of a turn: approving tool calls and answering the model's questions.
#[async_trait]
pub trait Approver: Send + Sync {
    async fn approve(&self, call: &ToolCall) -> bool;
    /// Ask the user a question (from the `question` tool). `None` = no answer / not interactive.
    async fn ask(&self, _question: &str, _options: &[String]) -> Option<String> {
        None
    }
    /// Whether `ask` can reach a person; the `question` tool is only offered when it can.
    fn interactive(&self) -> bool {
        false
    }
}

pub struct AutoApprove(pub bool);
#[async_trait]
impl Approver for AutoApprove {
    async fn approve(&self, _: &ToolCall) -> bool {
        self.0
    }
}

const MAX_STEPS: usize = 50;
const CHILD_MAX_STEPS: usize = 25;
const SUMMARY_PREFIX: &str = "[Summary of the earlier conversation]";

/// What the user sends in one turn: text plus pasted/attached images.
#[derive(Debug, Clone, Default)]
pub struct UserInput {
    pub text: String,
    pub images: Vec<Image>,
}

impl From<String> for UserInput {
    fn from(text: String) -> Self {
        Self {
            text,
            images: Vec::new(),
        }
    }
}

impl From<&str> for UserInput {
    fn from(text: &str) -> Self {
        text.to_owned().into()
    }
}

pub struct Agent {
    pub client: Client,
    pub model: String,
    pub mode: PermissionMode,
    pub history: Vec<Message>,
    pub usage: Usage,
    pub tools: Registry,
    pub cx: ToolCx,
    pub store: Option<SessionFile>,
    pub system: String,
    pub hooks: Hooks,
    /// OpenCode-style permission rules (last match wins); they refine `mode`.
    pub rules: RuleSet,
    /// Compact when the context nears this many tokens (0 = never).
    pub compact_at_tokens: u64,
    last_input_tokens: u64,
    is_child: bool,
    /// Used as the session id when there is no session file (tests, subagents).
    fallback_session: String,
}

impl Agent {
    pub fn new(client: Client, model: String, mode: PermissionMode, cwd: PathBuf) -> Self {
        let skills = discover_skills(&cwd);
        let mut system = build_system_prompt(&cwd);
        let mut tools = Registry::builtin();
        if !skills.is_empty() {
            system.push_str("\n\n# Skills (load one with the `skill` tool when relevant)\n");
            for s in &skills {
                system.push_str(&format!("- {}: {}\n", s.name, s.description));
            }
            tools.add(Box::new(SkillTool(skills)));
        }
        Self {
            client,
            model,
            mode,
            history: Vec::new(),
            usage: Usage::default(),
            tools,
            cx: ToolCx::new(cwd),
            store: None,
            system,
            hooks: Hooks::default(),
            rules: RuleSet::default(),
            compact_at_tokens: 100_000,
            last_input_tokens: 0,
            is_child: false,
            fallback_session: new_session_id(),
        }
    }

    /// Apply the user configuration (hooks, compaction threshold, sandbox).
    pub fn configure(&mut self, cfg: &crate::Config) {
        self.hooks = Hooks(cfg.hooks.clone());
        self.compact_at_tokens = cfg.compact_at_tokens;
        self.cx.sandbox = cfg.sandbox;
        self.rules = cfg.rules.clone();
        self.add_instructions(&cfg.instructions);
    }

    /// Append extra instruction files (`instructions` in the config; paths or globs relative to
    /// the project, ≤ 64 KB each). URLs are not fetched.
    fn add_instructions(&mut self, entries: &[String]) {
        for e in entries {
            if e.starts_with("http://") || e.starts_with("https://") {
                continue;
            }
            let Ok(glob) = globset::Glob::new(e) else {
                continue;
            };
            let m = glob.compile_matcher();
            let mut files: Vec<PathBuf> = ignore::WalkBuilder::new(&self.cx.cwd)
                .build()
                .flatten()
                .filter(|d| d.file_type().is_some_and(|t| t.is_file()))
                .map(|d| d.into_path())
                .filter(|p| p.strip_prefix(&self.cx.cwd).is_ok_and(|r| m.is_match(r)))
                .collect();
            files.sort();
            for f in files {
                if let Ok(text) = std::fs::read_to_string(&f)
                    && text.len() <= 64 * 1024
                {
                    let rel = f
                        .strip_prefix(&self.cx.cwd)
                        .unwrap_or(&f)
                        .display()
                        .to_string();
                    self.system
                        .push_str(&format!("\n\n# Instructions ({rel})\n{text}"));
                }
            }
        }
    }

    /// Final decision for a call: config rules override the mode, except that `plan` mode
    /// never lets a modifying tool run.
    fn decision(&self, call: &ToolCall, tool: &dyn Tool) -> Decision {
        let base = decide(self.mode, tool);
        if self.mode == PermissionMode::Plan && !tool.read_only() {
            return base;
        }
        match self.rules.check(&call.name, &permission_key(call)) {
            Some(Effect::Allow) => Decision::Allow,
            Some(Effect::Ask) => Decision::Ask,
            Some(Effect::Deny) => Decision::Deny,
            None => base,
        }
    }

    /// Start the configured MCP servers and register their tools. Returns error messages.
    pub async fn connect_mcp(&mut self, cfgs: &[ufrh_mcp::McpServerConfig]) -> Vec<String> {
        let (tools, errors) = ufrh_mcp::connect_all(cfgs).await;
        for t in tools {
            self.tools.add(t);
        }
        errors
    }

    fn push(&mut self, m: Message) {
        if let Some(s) = &self.store {
            let _ = s.append(&m);
        }
        self.history.push(m);
    }

    /// Whether the model is shown `tool` at all: hidden when no call to it could ever run
    /// (plan mode for modifying tools, a catch-all `deny` rule, or a subagent that has nobody
    /// to ask for approval). Calls are still checked again at execution time.
    fn offered(&self, tool: &dyn Tool) -> bool {
        let name = tool.name();
        if self.mode == PermissionMode::Plan && !tool.read_only() {
            return false;
        }
        if self.rules.fully_denied(name) {
            return false;
        }
        if self.is_child {
            // The subagent's approver always says no, so only auto-allowed tools are usable.
            return decide(self.mode, tool) == Decision::Allow || self.rules.may_allow(name);
        }
        true
    }

    fn all_specs(&self, interactive: bool) -> Vec<ToolSpec> {
        let mut v: Vec<ToolSpec> = self
            .tools
            .iter()
            .filter(|t| self.offered(*t))
            .map(|t| ToolSpec {
                name: t.name().into(),
                description: t.description().into(),
                schema: t.schema(),
            })
            .collect();
        if interactive && !self.is_child && !self.rules.fully_denied("question") {
            v.push(ToolSpec {
                name: "question".into(),
                description: "Ask the user a question and wait for the answer. Use it when the request is ambiguous \
or a decision is genuinely theirs; do not use it for things you can find out yourself."
                    .into(),
                schema: json!({"type":"object","properties":{
                    "question":{"type":"string"},
                    "options":{"type":"array","items":{"type":"string"},"description":"suggested answers (the user may also type their own)"}},
                    "required":["question"]}),
            });
        }
        if !self.is_child && !self.rules.fully_denied("task") {
            v.push(ToolSpec {
                name: "task".into(),
                description: "Delegate a self-contained research question to a read-only subagent with its own context. \
It can read/search the project but not modify it; it returns a concise report."
                    .into(),
                schema: json!({"type":"object","properties":{
                    "description":{"type":"string","description":"3-5 word label"},
                    "prompt":{"type":"string","description":"everything the subagent needs to know"}},
                    "required":["prompt"]}),
            });
        }
        v
    }

    /// Run one user turn: model → tools → model … until the model stops calling tools.
    pub async fn run_turn(
        &mut self,
        prompt: impl Into<UserInput>,
        events: UnboundedSender<AgentEvent>,
        cancel: CancellationToken,
        approver: &dyn Approver,
    ) {
        let UserInput {
            text: prompt,
            mut images,
        } = prompt.into();
        self.cx.cancel = cancel.clone();
        let hook = self
            .hooks
            .run(
                "user_prompt",
                None,
                &json!({"prompt": prompt, "images": images.len()}),
                &self.cx.cwd,
            )
            .await;
        if let Some(reason) = hook.blocked {
            let _ = events.send(AgentEvent::Error(format!(
                "prompt blocked by hook: {reason}"
            )));
            let _ = events.send(AgentEvent::TurnDone);
            return;
        }
        let (mut prompt, mentioned) = expand_mentions(&self.cx, &prompt);
        images.extend(mentioned);
        if !hook.output.trim().is_empty() {
            prompt.push_str(&format!("\n\n[hook output]\n{}", hook.output.trim()));
        }
        self.cx.checkpoints.begin_turn();
        self.push(Message::User {
            text: prompt,
            images,
        });
        if let Err(e) = self.run_loop(&events, &cancel, approver).await {
            let _ = events.send(AgentEvent::Error(format!("{e:#}")));
        }
        self.hooks.run("stop", None, &json!({}), &self.cx.cwd).await;
        let _ = events.send(AgentEvent::TurnDone);
    }

    async fn run_loop(
        &mut self,
        events: &UnboundedSender<AgentEvent>,
        cancel: &CancellationToken,
        approver: &dyn Approver,
    ) -> Result<()> {
        let max_steps = if self.is_child {
            CHILD_MAX_STEPS
        } else {
            MAX_STEPS
        };
        for _ in 0..max_steps {
            self.maybe_compact(events).await;
            let req = ChatRequest {
                model: self.model.clone(),
                system: Some(self.system.clone()),
                messages: self.history.clone(),
                tools: self.all_specs(approver.interactive()),
                max_tokens: None,
                session_id: Some(self.session_id()),
            };
            let mut stream = tokio::select! {
                s = self.client.stream(&req) => s?,
                _ = cancel.cancelled() => return self.cancelled(String::new(), vec![]),
            };
            let mut text = String::new();
            let mut calls: Vec<ToolCall> = Vec::new();
            loop {
                let ev = tokio::select! {
                    ev = stream.next() => ev,
                    _ = cancel.cancelled() => return self.cancelled(text, calls),
                };
                match ev {
                    None => break,
                    Some(Err(e)) => {
                        self.push(Message::Assistant {
                            text,
                            tool_calls: calls,
                        });
                        return Err(e);
                    }
                    Some(Ok(StreamEvent::TextDelta(t))) => {
                        text.push_str(&t);
                        let _ = events.send(AgentEvent::TextDelta(t));
                    }
                    Some(Ok(StreamEvent::ReasoningDelta(t))) => {
                        let _ = events.send(AgentEvent::ReasoningDelta(t));
                    }
                    Some(Ok(StreamEvent::ToolCall(c))) => calls.push(c),
                    Some(Ok(StreamEvent::Usage(u))) => {
                        self.usage.add(u);
                        self.last_input_tokens = u.input_tokens;
                        let _ = events.send(AgentEvent::Usage(u));
                    }
                    Some(Ok(StreamEvent::Done)) => break,
                }
            }
            drop(stream);
            self.push(Message::Assistant {
                text,
                tool_calls: calls.clone(),
            });
            if calls.is_empty() {
                return Ok(());
            }

            // Independent, pre-approved calls (reads, greps, subagents…) run in parallel.
            let parallel = calls.len() > 1 && calls.iter().all(|c| self.auto_allowed(c));
            let results: Vec<(String, bool)> = if parallel {
                join_all(calls.iter().map(|c| self.exec_tool(c, events, approver))).await
            } else {
                let mut r = Vec::new();
                for c in &calls {
                    if cancel.is_cancelled() {
                        r.push(("cancelled by user".to_string(), true));
                        continue;
                    }
                    r.push(self.exec_tool(c, events, approver).await);
                }
                r
            };
            for (call, (output, is_error)) in calls.into_iter().zip(results) {
                let _ = events.send(AgentEvent::ToolEnd {
                    id: call.id.clone(),
                    output: output.clone(),
                    is_error,
                });
                self.push(Message::ToolResult {
                    call_id: call.id,
                    content: output,
                    is_error,
                });
            }
            if cancel.is_cancelled() {
                return Ok(());
            }
        }
        bail!("stopped after {max_steps} steps without finishing")
    }

    fn auto_allowed(&self, call: &ToolCall) -> bool {
        if call.name == "question" {
            return false; // interactive: never batched with other calls
        }
        if call.name == "task" {
            return !self.is_child;
        }
        self.tools
            .get(&call.name)
            .is_some_and(|t| self.decision(call, t) == Decision::Allow)
    }

    /// Keep the history valid after cancellation: every tool call needs a result.
    fn cancelled(&mut self, text: String, calls: Vec<ToolCall>) -> Result<()> {
        let ids: Vec<String> = calls.iter().map(|c| c.id.clone()).collect();
        if !text.is_empty() || !calls.is_empty() {
            self.push(Message::Assistant {
                text,
                tool_calls: calls,
            });
        }
        for id in ids {
            self.push(Message::ToolResult {
                call_id: id,
                content: "cancelled by user".into(),
                is_error: true,
            });
        }
        Ok(())
    }

    async fn exec_tool(
        &self,
        call: &ToolCall,
        events: &UnboundedSender<AgentEvent>,
        approver: &dyn Approver,
    ) -> (String, bool) {
        let _ = events.send(AgentEvent::ToolStart {
            id: call.id.clone(),
            name: call.name.clone(),
            input: call.arguments.clone(),
        });
        let pre = self
            .hooks
            .run(
                "pre_tool",
                Some(&call.name),
                &json!({"tool": call.name, "input": call.arguments}),
                &self.cx.cwd,
            )
            .await;
        if let Some(reason) = pre.blocked {
            return (format!("blocked by hook: {reason}"), true);
        }

        let result = if call.name == "question" && !self.is_child {
            let question = call
                .arguments
                .get("question")
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim()
                .to_owned();
            let options: Vec<String> = call
                .arguments
                .get("options")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(|o| o.as_str().map(str::to_owned))
                        .collect()
                })
                .unwrap_or_default();
            if question.is_empty() {
                Err(anyhow::anyhow!("missing `question`"))
            } else if self.rules.check("question", &question) == Some(Effect::Deny) {
                Err(anyhow::anyhow!(
                    "asking the user is disabled by the permission rules"
                ))
            } else {
                match approver.ask(&question, &options).await {
                    Some(a) if !a.trim().is_empty() => {
                        Ok(format!("The user answered: {}", a.trim()))
                    }
                    _ => Err(anyhow::anyhow!(
                        "the user did not answer; continue with your best judgment and say what you assumed"
                    )),
                }
            }
        } else if call.name == "task" && !self.is_child {
            let prompt = call
                .arguments
                .get("prompt")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned();
            if prompt.is_empty() {
                Err(anyhow::anyhow!("missing `prompt`"))
            } else {
                Ok(run_subagent(
                    self.client.clone(),
                    self.model.clone(),
                    self.cx.cwd.clone(),
                    prompt,
                    self.cx.cancel.clone(),
                )
                .await)
            }
        } else {
            let Some(tool) = self.tools.get(&call.name) else {
                return (format!("unknown tool `{}`", call.name), true);
            };
            match self.decision(call, tool) {
                Decision::Deny => {
                    return (
                        format!("`{}` is not allowed in {:?} mode", call.name, self.mode),
                        true,
                    );
                }
                Decision::Ask if !approver.approve(call).await => {
                    return ("the user denied this tool call".into(), true);
                }
                _ => {}
            }
            tool.call(call.arguments.clone(), &self.cx).await
        };
        let (out, is_err) = match result {
            Ok(o) => (o, false),
            Err(e) => (format!("{e:#}"), true),
        };
        self.hooks
            .run(
                "post_tool",
                Some(&call.name),
                &json!({"tool": call.name, "is_error": is_err}),
                &self.cx.cwd,
            )
            .await;
        (out, is_err)
    }

    // ───────────── context compaction ─────────────

    async fn maybe_compact(&mut self, events: &UnboundedSender<AgentEvent>) {
        if self.compact_at_tokens == 0 {
            return;
        }
        // The API's reported input size is the truth; the byte estimate covers the first request.
        let size = estimate_tokens(&self.history).max(self.last_input_tokens);
        if size <= self.compact_at_tokens {
            return;
        }
        match self.compact().await {
            Ok(Some((before, after))) => {
                let _ = events.send(AgentEvent::Notice(format!(
                    "context compacted: {before} → {after} messages"
                )));
            }
            Ok(None) => {}
            Err(e) => {
                let _ = events.send(AgentEvent::Notice(format!("compaction failed: {e:#}")));
            }
        }
    }

    /// Replace the older part of the history with a model-written summary, keeping the most
    /// recent messages verbatim. Returns `(messages before, messages after)`.
    pub async fn compact(&mut self) -> Result<Option<(usize, usize)>> {
        let len = self.history.len();
        // Cut so that no tool result is separated from its call.
        let mut cut = len.saturating_sub(6);
        while cut > 0 && matches!(self.history[cut], Message::ToolResult { .. }) {
            cut -= 1;
        }
        if cut == 0 {
            return Ok(None);
        }
        let summary = self
            .summarize(render_transcript(&self.history[..cut]))
            .await?;
        let mut tail = self.history.split_off(cut);
        let mut head = format!("{SUMMARY_PREFIX}\n{summary}");
        let mut images = Vec::new();
        if matches!(tail.first(), Some(Message::User { .. }))
            && let Message::User { text, images: i } = tail.remove(0)
        {
            head.push_str(&format!("\n\n{text}")); // keep roles alternating
            images = i;
        }
        let mut new = vec![Message::User { text: head, images }];
        new.append(&mut tail);
        self.history = new;
        self.last_input_tokens = 0;
        if let Some(s) = &self.store {
            s.rewrite(&self.history)?;
        }
        Ok(Some((len, self.history.len())))
    }

    async fn summarize(&self, transcript: String) -> Result<String> {
        let req = ChatRequest {
            model: self.model.clone(),
            system: Some(
                "You compress the history of a coding-agent session. Write a dense summary in the same language as the user: \
goals, decisions, files read/changed (with paths), commands run and their outcomes, open problems and next steps. \
Do not call tools."
                    .into(),
            ),
            messages: vec![Message::user(format!("Summarize this session transcript:\n\n{transcript}"))],
            tools: vec![],
            max_tokens: Some(2048),
            session_id: Some(self.session_id()),
        };
        let mut stream = self.client.stream(&req).await?;
        let mut out = String::new();
        while let Some(ev) = stream.next().await {
            if let StreamEvent::TextDelta(t) = ev? {
                out.push_str(&t);
            }
        }
        if out.trim().is_empty() {
            bail!("the model returned an empty summary");
        }
        Ok(out)
    }

    // ───────────── rewind / fork ─────────────

    /// Undo the last `n` user turns: restore files changed by `write`/`edit` and drop the messages.
    /// Returns the restored paths.
    pub fn rewind(&mut self, n: usize) -> Result<Vec<PathBuf>> {
        let turns: Vec<usize> = self
            .history
            .iter()
            .enumerate()
            .filter(
                |(_, m)| matches!(m, Message::User { text, .. } if !text.starts_with(SUMMARY_PREFIX)),
            )
            .map(|(i, _)| i)
            .collect();
        if n == 0 || n > turns.len() {
            bail!("can rewind between 1 and {} turns", turns.len());
        }
        let start = turns[turns.len() - n];
        self.history.truncate(start);
        let restored = self
            .cx
            .checkpoints
            .rewind_to(self.cx.checkpoints.turn() + 1 - n.min(self.cx.checkpoints.turn()));
        if let Some(s) = &self.store {
            s.rewrite(&self.history)?;
        }
        Ok(restored)
    }

    /// Continue in a new session file that starts as a copy of the current history; the
    /// original file stays as the branch point.
    /// Stable id of this conversation: derived from the session file, so it survives resume and
    /// changes on fork / a new session.
    pub fn session_id(&self) -> String {
        match self.store.as_ref().and_then(|s| s.path().file_stem()) {
            Some(stem) => format!("ufrh-{}", stem.to_string_lossy()),
            None => self.fallback_session.clone(),
        }
    }

    /// Start a fresh conversation (new history, new session file, new session id).
    pub fn new_session(&mut self) -> Result<()> {
        self.history.clear();
        self.last_input_tokens = 0;
        self.store = Some(SessionFile::create_for(
            &ufrh_store::sessions_dir(),
            &self.cx.cwd,
        )?);
        Ok(())
    }

    /// Continue a saved session (only one that belongs to this working folder).
    pub fn resume(&mut self, path: &Path) -> Result<usize> {
        let sf = SessionFile::resume(&ufrh_store::sessions_dir(), path, &self.cx.cwd)?;
        self.history = sf.load()?;
        self.last_input_tokens = 0;
        self.store = Some(sf);
        Ok(self.history.len())
    }

    pub fn fork(&mut self) -> Result<PathBuf> {
        let new = SessionFile::create_for(&ufrh_store::sessions_dir(), &self.cx.cwd)?;
        for m in &self.history {
            new.append(m)?;
        }
        let path = new.path().to_path_buf();
        self.store = Some(new);
        Ok(path)
    }
}

/// The string a permission pattern is matched against (command for `bash`, path for file tools).
fn permission_key(call: &ToolCall) -> String {
    let arg = |k: &str| {
        call.arguments
            .get(k)
            .and_then(Value::as_str)
            .map(str::to_owned)
    };
    match call.name.as_str() {
        "bash" => arg("command"),
        "read" | "write" | "edit" => arg("file_path"),
        "glob" | "grep" => arg("pattern"),
        "ls" => arg("path"),
        "webfetch" => arg("url"),
        "websearch" => arg("query"),
        _ => None,
    }
    .unwrap_or_else(|| "*".into())
}

/// Random-enough id without a `rand` dependency (std's per-process random hasher keys).
fn new_session_id() -> String {
    use std::hash::{BuildHasher, Hasher};
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u128(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos()),
    );
    h.write_u32(std::process::id());
    format!("ufrh-{:016x}", h.finish())
}

fn estimate_tokens(h: &[Message]) -> u64 {
    let bytes: usize = h
        .iter()
        .map(|m| match m {
            // ≈1600 tokens for an image at MAX_EDGE (Anthropic: w·h/750).
            Message::User { text, images } => text.len() + images.len() * 6400,
            Message::Assistant { text, tool_calls } => {
                text.len()
                    + tool_calls
                        .iter()
                        .map(|c| c.arguments.to_string().len() + c.name.len())
                        .sum::<usize>()
            }
            Message::ToolResult { content, .. } => content.len(),
        })
        .sum();
    (bytes / 4) as u64
}

fn clip(s: &str, n: usize) -> String {
    if s.len() <= n {
        s.to_owned()
    } else {
        format!("{}…", &s[..s.floor_char_boundary(n)])
    }
}

fn render_transcript(h: &[Message]) -> String {
    let mut out = String::new();
    for m in h {
        match m {
            Message::User { text, images } => {
                out.push_str(&format!("[user] {}\n", clip(text, 4000)));
                if !images.is_empty() {
                    out.push_str(&format!("[user attached {} image(s)]\n", images.len()));
                }
            }
            Message::Assistant { text, tool_calls } => {
                if !text.is_empty() {
                    out.push_str(&format!("[assistant] {}\n", clip(text, 4000)));
                }
                for c in tool_calls {
                    out.push_str(&format!(
                        "[tool call] {} {}\n",
                        c.name,
                        clip(&c.arguments.to_string(), 600)
                    ));
                }
            }
            Message::ToolResult {
                content, is_error, ..
            } => out.push_str(&format!(
                "[tool result{}] {}\n",
                if *is_error { " (error)" } else { "" },
                clip(content, 800)
            )),
        }
    }
    out
}

/// A read-only child agent with a fresh context; returns its final report.
fn run_subagent(
    client: Client,
    model: String,
    cwd: PathBuf,
    prompt: String,
    cancel: CancellationToken,
) -> BoxFuture<'static, String> {
    async move {
        let mut child = Agent::new(client, model, PermissionMode::Plan, cwd);
        child.is_child = true;
        child.compact_at_tokens = 0;
        child.system.push_str("\n\nYou are a read-only research subagent. Investigate, then reply with a concise report (no preamble).");
        let (tx, mut rx) = unbounded_channel();
        child.run_turn(prompt, tx, cancel, &AutoApprove(false)).await;
        let mut error = None;
        while let Ok(ev) = rx.try_recv() {
            if let AgentEvent::Error(e) = ev {
                error = Some(e);
            }
        }
        let report = child
            .history
            .iter()
            .rev()
            .find_map(|m| match m {
                Message::Assistant { text, .. } if !text.trim().is_empty() => Some(text.clone()),
                _ => None,
            })
            .unwrap_or_default();
        match error {
            Some(e) if report.is_empty() => format!("subagent failed: {e}"),
            Some(e) => format!("{report}\n\n[subagent stopped early: {e}]"),
            None if report.is_empty() => "subagent produced no report".into(),
            None => report,
        }
    }
    .boxed()
}

/// System prompt + project instructions (`AGENTS.md`, falling back to `CLAUDE.md`).
pub fn build_system_prompt(cwd: &std::path::Path) -> String {
    let mut s = format!(
        "You are ufrh, a coding agent running in the user's terminal/desktop. \
Work in the project directory `{}`. Use the tools to inspect and change files; read a file before editing it. \
Use `todo` to plan multi-step work. Use `websearch`/`webfetch` for documentation you do not know, \
and `question` only when the user must decide. Be concise.",
        cwd.display()
    );
    for name in ["AGENTS.md", "CLAUDE.md"] {
        if let Ok(text) = std::fs::read_to_string(cwd.join(name)) {
            s.push_str(&format!("\n\n# Project instructions ({name})\n{text}"));
            break;
        }
    }
    s
}
