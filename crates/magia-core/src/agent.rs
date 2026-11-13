use crate::ext::{SkillTool, discover_skills, expand_mentions};
use crate::hooks::Hooks;
use crate::magia_mode::{self, Continue, MagiaMode};
use anyhow::{Result, bail};
use async_trait::async_trait;
use futures_util::StreamExt;
use futures_util::future::{BoxFuture, FutureExt, join_all};
use magia_providers::Client;
use magia_store::SessionFile;
use magia_tools::auto::Verdict;
use magia_tools::{Decision, Effect, PermissionMode, Registry, RuleSet, Tool, ToolCx, decide};
use magia_types::{ChatRequest, Image, Message, StreamEvent, ToolCall, ToolSpec, Usage};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::mpsc::{UnboundedSender, unbounded_channel};
use tokio_util::sync::CancellationToken;

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
    /// A request failed and will be repeated after `wait_secs`. When `discarded`, the text
    /// streamed so far in this response is thrown away (the repeat starts over).
    Retry {
        attempt: u32,
        max: u32,
        wait_secs: u64,
        error: String,
        discarded: bool,
    },
    /// Magia mode started another round of the same request (`n` ≥ 2) and why.
    Round {
        n: u32,
        reason: String,
    },
    /// Magia mode's state, for a status panel (rounds also come as `Round`).
    Magia(MagiaPhase),
    /// A subagent (`task` call `parent`, or the reviewer: `parent` = `"review"`) used a tool.
    SubTool {
        parent: String,
        name: String,
        input: Value,
    },
    /// Turn finished (completed, cancelled or failed).
    TurnDone,
}

/// `parent` of the reviewer's `SubTool` events.
pub const REVIEW_PARENT: &str = "review";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MagiaPhase {
    /// Round `round` is running.
    Working { round: u32 },
    /// The reviewer is checking the changed files (`n`-th review of this request).
    Reviewing { n: u32 },
    /// The reviewer found these problems; the next round fixes them.
    ReviewFailed { issues: Vec<String> },
    /// The request ended, and why.
    Finished(String),
}

/// The answer to "may this call reach these folders outside the project?".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FolderAnswer {
    /// This call only.
    Once,
    /// The folders, for the rest of the session (saved with it).
    Always,
    Deny,
}

/// The user side of a turn: approving tool calls and answering the model's questions.
#[async_trait]
pub trait Approver: Send + Sync {
    async fn approve(&self, call: &ToolCall) -> bool;
    /// The call reaches `dirs`, outside the project. Without a richer UI: yes means once.
    async fn approve_folder(&self, call: &ToolCall, dirs: &[PathBuf]) -> FolderAnswer {
        let list: Vec<String> = dirs.iter().map(|d| d.display().to_string()).collect();
        let why = format!("outside the project: {}", list.join(", "));
        if self.approve_because(call, &why).await {
            FolderAnswer::Once
        } else {
            FolderAnswer::Deny
        }
    }
    /// Like `approve`, with the reason `auto` mode flagged the call (shown to the user).
    async fn approve_because(&self, call: &ToolCall, _reason: &str) -> bool {
        self.approve(call).await
    }
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
/// Subagents running at once; more `task` calls in one response wait for a free slot.
pub const MAX_PARALLEL_SUBAGENTS: usize = 3;
const SUMMARY_PREFIX: &str = "[Summary of the earlier conversation]";

/// How a round of the agent loop ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum End {
    /// The model answered without calling tools.
    Finished,
    Cancelled,
    /// The step limit was reached with the model still calling tools.
    StepLimit,
}

enum Streamed {
    Done(String, Vec<ToolCall>),
    Cancelled(String, Vec<ToolCall>),
    Failed {
        error: anyhow::Error,
        text: String,
        /// Text or reasoning already reached the UI (it will be discarded on a retry).
        emitted: bool,
    },
}

/// Pause before repeating a failed request (longer if the provider asks via `retry-after`).
const RETRY_DELAY: std::time::Duration = std::time::Duration::from_secs(5);
const MAX_RETRY_DELAY: std::time::Duration = std::time::Duration::from_secs(60);
const MAX_RETRIES: u32 = 10;

/// The first line of an error, short enough for a status line.
fn short_error(e: &anyhow::Error) -> String {
    let s = format!("{e:#}");
    let line = s.lines().next().unwrap_or("").trim();
    if line.chars().count() > 160 {
        format!("{}…", line.chars().take(160).collect::<String>())
    } else {
        line.to_owned()
    }
}

struct RoundEnd {
    end: End,
    /// Tool calls the model made in the round.
    calls: usize,
    /// Successful `write`/`edit` calls in the round.
    edits: usize,
}

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
    /// Free subagent slots (see `MAX_PARALLEL_SUBAGENTS`).
    subagents: tokio::sync::Semaphore,
    /// The request being sent during a turn, for `/btw` questions (see `crate::btw`).
    pub side: crate::btw::SideSlot,
    /// Folders outside the project allowed for this session (shared with subagents).
    pub allowed_dirs: Arc<std::sync::Mutex<Vec<PathBuf>>>,
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
    /// `auto` mode reviewer model (None = the session's model).
    pub auto_model: Option<String>,
    /// Instructions of the selected file agent (`crate::agents`): (file name, text). They are
    /// added after the base prompt; the subagent (`task`) does not get them.
    pub persona: Option<(String, String)>,
    /// The Magia agent is selected (`crate::magia_mode`): the request runs in rounds until the
    /// task list is closed. Ignored in plan mode and in subagents.
    pub magia: Option<MagiaMode>,
    /// Pause before repeating a failed request (5 s; the provider's `retry-after` can raise it).
    pub retry_delay: std::time::Duration,
    /// The root file already added as project instructions (`AGENTS.md`/`CLAUDE.md`), so picking
    /// it as the agent does not add it twice.
    project_instructions: Option<&'static str>,
    /// `auto` mode: calls the reviewer already allowed in this session (tool + key).
    reviewed: std::sync::Mutex<std::collections::HashSet<String>>,
    /// The user said no to a tool call in the current round (Magia mode stops then).
    denied: AtomicBool,
    last_input_tokens: u64,
    is_child: bool,
    /// Used as the session id when there is no session file (tests, subagents).
    fallback_session: String,
}

impl Agent {
    pub fn new(client: Client, model: String, mode: PermissionMode, cwd: PathBuf) -> Self {
        let skills = discover_skills(&cwd);
        let mut system = build_system_prompt(&cwd);
        let project_instructions = project_instructions_file(&cwd);
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
            auto_model: None,
            persona: None,
            magia: None,
            subagents: tokio::sync::Semaphore::new(MAX_PARALLEL_SUBAGENTS),
            side: Default::default(),
            allowed_dirs: Default::default(),
            retry_delay: RETRY_DELAY,
            project_instructions,
            reviewed: Default::default(),
            denied: AtomicBool::new(false),
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
        self.auto_model = cfg.auto_model.clone();
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
    pub async fn connect_mcp(&mut self, cfgs: &[magia_mcp::McpServerConfig]) -> Vec<String> {
        let (tools, errors) = magia_mcp::connect_all(cfgs).await;
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

    /// The system prompt plus what the model cannot know on its own: which model it is and how
    /// it is being run. Built per request, since provider, model and mode can change mid-session.
    fn system_prompt(&self) -> String {
        let persona = match &self.persona {
            Some((name, text)) if Some(name.as_str()) != self.project_instructions => {
                format!("\n\n# Agent instructions ({name})\n{text}")
            }
            _ => String::new(),
        };
        let persona = if self.magia_active() {
            format!("{persona}\n\n{}", magia_mode::INSTRUCTIONS)
        } else {
            persona
        };
        format!(
            "{}{persona}\n\n# This session\n- You are running in magia, a coding agent. It has two programs: the `magia` CLI and the desktop app `magia-gui`. When the user asks to open \"the GUI\", \"the app\" or magia itself, they mean `magia-gui`: start it detached in the working folder with `{}`, and do not look for other programs.\n- Model: `{}` via the `{}` provider. When asked which model you are, answer with this id.\n- Permission mode: `{}`.\n- Working folder: {}",
            self.system,
            if cfg!(windows) {
                "magia-gui &"
            } else {
                "setsid magia-gui >/dev/null 2>&1 < /dev/null &"
            },
            self.model,
            self.client.config.name,
            serde_json::to_value(self.mode)
                .ok()
                .and_then(|v| v.as_str().map(str::to_owned))
                .unwrap_or_default(),
            self.cx.cwd.display()
        )
    }

    /// Whether this request runs in Magia rounds.
    fn magia_active(&self) -> bool {
        self.magia.is_some() && !self.is_child && self.mode != PermissionMode::Plan
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
                description: format!("Delegate a self-contained research question to a read-only subagent with its own context. \
It can read/search the project but not modify it; it returns a concise report. Several `task` calls in one response \
run in parallel (up to {MAX_PARALLEL_SUBAGENTS} at a time): split independent questions across them."),
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
        let request = prompt.clone();
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
        self.run_rounds(&request, &events, &cancel, approver).await;
        if let Ok(mut s) = self.side.lock() {
            *s = None; // idle again: `/btw` reads the agent itself
        }
        self.hooks.run("stop", None, &json!({}), &self.cx.cwd).await;
        let _ = events.send(AgentEvent::TurnDone);
    }

    /// One user request: a single round normally; in Magia mode, more rounds while the task
    /// list has open items (see `crate::magia_mode`).
    async fn run_rounds(
        &mut self,
        request: &str,
        events: &UnboundedSender<AgentEvent>,
        cancel: &CancellationToken,
        approver: &dyn Approver,
    ) {
        let magia = self.magia.filter(|_| self.magia_active());
        let todos_at_start = self.todos();
        let phase = |p: MagiaPhase| {
            let _ = events.send(AgentEvent::Magia(p));
        };
        if magia.is_some() {
            phase(MagiaPhase::Working { round: 1 });
        }
        let (mut n, mut idle, mut reviews) = (1u32, 0u32, 0u32);
        let reason: String = loop {
            let todos_before = self.todos();
            self.denied.store(false, Ordering::Relaxed);
            let round = match self.run_loop(events, cancel, approver).await {
                Ok(r) => r,
                Err(e) => {
                    let _ = events.send(AgentEvent::Error(format!("{e:#}")));
                    break "the request failed".into();
                }
            };
            let Some(cfg) = magia else {
                if round.end == End::StepLimit {
                    let _ = events.send(AgentEvent::Error(format!(
                        "stopped after {} steps without finishing",
                        self.max_steps()
                    )));
                }
                return;
            };
            if round.end == End::Cancelled {
                break "cancelled".into();
            }
            let todos = self.todos();
            let open = magia_mode::open_tasks(&todos);
            let changed = self.cx.checkpoints.changed_in_turn();
            let progress = todos != todos_before || round.edits > 0;
            idle = if progress { 0 } else { idle + 1 };
            // The list counts only if this request wrote it: open items left over from an
            // earlier request must not hijack an unrelated question.
            let list_is_ours = todos != todos_at_start;
            let notice = |m: &str| {
                let _ = events.send(AgentEvent::Notice(format!("magia: {m}")));
            };
            let why = if self.denied.load(Ordering::Relaxed) {
                notice("stopped because a tool call was denied");
                break "stopped: a tool call was denied".into();
            } else if round.end == End::StepLimit {
                Continue::StepLimit
            } else if round.calls == 0 {
                // only talked: an answer, or something for the user to decide
                break "finished".into();
            } else if open > 0 && list_is_ours {
                Continue::OpenTasks
            } else if cfg.review && reviews < magia_mode::MAX_REVIEWS && !changed.is_empty() {
                reviews += 1;
                notice("reviewing the changes…");
                phase(MagiaPhase::Reviewing { n: reviews });
                match self
                    .review_work(request, &todos, &changed, events, cancel)
                    .await
                {
                    magia_mode::Verdict::Fail(issues) => {
                        phase(MagiaPhase::ReviewFailed {
                            issues: issues.clone(),
                        });
                        Continue::Review(issues)
                    }
                    magia_mode::Verdict::Pass => {
                        notice("review passed");
                        break "finished · review passed".into();
                    }
                    magia_mode::Verdict::Unreadable => {
                        notice("the reviewer gave no clear verdict; finishing");
                        break "finished · reviewer gave no clear verdict".into();
                    }
                }
            } else {
                break "finished".into();
            };
            if cancel.is_cancelled() {
                break "cancelled".into();
            }
            if idle >= magia_mode::MAX_IDLE_ROUNDS && !matches!(why, Continue::Review(_)) {
                notice(&format!("stopped after {idle} rounds without progress"));
                break format!("stopped: {idle} rounds without progress");
            }
            if n >= cfg.max_rounds {
                notice(&format!("stopped after {n} rounds; ask again to continue"));
                break format!("stopped: {n} rounds (the limit)");
            }
            n += 1;
            let _ = events.send(AgentEvent::Round {
                n,
                reason: why.label(open),
            });
            if !matches!(why, Continue::Review(_)) {
                phase(MagiaPhase::Working { round: n });
            }
            self.push(Message::user(magia_mode::continuation(
                request, &todos, &why,
            )));
        };
        if magia.is_some() {
            phase(MagiaPhase::Finished(reason));
        }
    }

    fn todos(&self) -> Vec<magia_tools::Todo> {
        self.cx.todos.lock().map(|t| t.clone()).unwrap_or_default()
    }

    fn max_steps(&self) -> usize {
        if self.is_child {
            CHILD_MAX_STEPS
        } else {
            MAX_STEPS
        }
    }

    /// Ask a read-only reviewer whether the request is really done.
    async fn review_work(
        &self,
        request: &str,
        todos: &[magia_tools::Todo],
        changed: &[PathBuf],
        events: &UnboundedSender<AgentEvent>,
        cancel: &CancellationToken,
    ) -> magia_mode::Verdict {
        let files: Vec<String> = changed
            .iter()
            .map(|p| {
                p.strip_prefix(&self.cx.cwd)
                    .unwrap_or(p)
                    .display()
                    .to_string()
            })
            .collect();
        let claim = self
            .history
            .iter()
            .rev()
            .find_map(|m| match m {
                Message::Assistant { text, .. } if !text.trim().is_empty() => Some(text.as_str()),
                _ => None,
            })
            .unwrap_or("");
        let report = run_subagent(
            self.child_base(),
            magia_mode::review_prompt(request, todos, &files, claim),
            magia_mode::REVIEWER,
            cancel.clone(),
            Some((events.clone(), REVIEW_PARENT.into())),
        )
        .await;
        magia_mode::parse_review(&report)
    }

    async fn run_loop(
        &mut self,
        events: &UnboundedSender<AgentEvent>,
        cancel: &CancellationToken,
        approver: &dyn Approver,
    ) -> Result<RoundEnd> {
        let (mut calls_made, mut edits) = (0usize, 0usize);
        let done = |end: End, calls: usize, edits: usize| Ok(RoundEnd { end, calls, edits });
        for _ in 0..self.max_steps() {
            self.maybe_compact(events).await;
            // Also published for `/btw` questions asked during the turn (same Arc, no copy).
            let side = Arc::new(crate::btw::SideContext {
                client: self.client.clone(),
                req: self.request(approver.interactive()),
            });
            if let Ok(mut s) = self.side.lock() {
                *s = Some(side.clone());
            }
            let req = &side.req;
            // Any failure (network, decode, an error event, a dropped stream, 429/5xx) is
            // retried after a pause, from the start of this response.
            let mut attempt = 0u32;
            let (text, calls) = loop {
                match self.stream_once(req, events, cancel).await {
                    Streamed::Done(text, calls) => break (text, calls),
                    Streamed::Cancelled(text, calls) => {
                        self.cancelled(text, calls);
                        return done(End::Cancelled, calls_made, edits);
                    }
                    Streamed::Failed {
                        error,
                        text,
                        emitted,
                    } => {
                        let Some(min) = magia_providers::retry_after(&error)
                            .filter(|_| attempt < MAX_RETRIES && !cancel.is_cancelled())
                        else {
                            // Keep what was said; unfinished tool calls are dropped (they have
                            // no results and would break the next request).
                            if !text.is_empty() {
                                self.push(Message::Assistant {
                                    text,
                                    tool_calls: vec![],
                                });
                            }
                            return Err(error);
                        };
                        attempt += 1;
                        let wait = min.max(self.retry_delay).min(MAX_RETRY_DELAY);
                        let _ = events.send(AgentEvent::Retry {
                            attempt,
                            max: MAX_RETRIES,
                            wait_secs: wait.as_secs(),
                            error: short_error(&error),
                            discarded: emitted,
                        });
                        tokio::select! {
                            _ = tokio::time::sleep(wait) => {}
                            _ = cancel.cancelled() => return done(End::Cancelled, calls_made, edits),
                        }
                    }
                }
            };
            self.push(Message::Assistant {
                text,
                tool_calls: calls.clone(),
            });
            if calls.is_empty() {
                return done(End::Finished, calls_made, edits);
            }
            calls_made += calls.len();

            // Runs of independent, pre-approved calls (reads, greps, subagents…) run in
            // parallel; a call that needs approval runs alone, in its place in the order. Each
            // call reports its end as soon as it finishes; history keeps the model's order.
            let mut results: Vec<(String, bool)> = Vec::with_capacity(calls.len());
            let mut i = 0;
            while i < calls.len() {
                if cancel.is_cancelled() {
                    let r = ("cancelled by user".to_string(), true);
                    self.report_end(&calls[i], &r, events);
                    results.push(r);
                    i += 1;
                    continue;
                }
                let run = calls[i..]
                    .iter()
                    .take_while(|c| self.auto_allowed(c))
                    .count();
                if run > 1 {
                    let this: &Self = self;
                    let batch = calls[i..i + run].iter().map(|c| async move {
                        let r = this.exec_tool(c, events, approver).await;
                        this.report_end(c, &r, events);
                        r
                    });
                    results.extend(join_all(batch).await);
                    i += run;
                } else {
                    let r = self.exec_tool(&calls[i], events, approver).await;
                    self.report_end(&calls[i], &r, events);
                    results.push(r);
                    i += 1;
                }
            }
            for (call, (output, is_error)) in calls.into_iter().zip(results) {
                if !is_error && self.tools.get(&call.name).is_some_and(|t| t.is_edit()) {
                    edits += 1;
                }
                self.push(Message::ToolResult {
                    call_id: call.id,
                    content: output,
                    is_error,
                });
            }
            if cancel.is_cancelled() {
                return done(End::Cancelled, calls_made, edits);
            }
        }
        done(End::StepLimit, calls_made, edits)
    }

    /// One attempt at streaming a response. Text and reasoning are forwarded as they arrive.
    async fn stream_once(
        &mut self,
        req: &ChatRequest,
        events: &UnboundedSender<AgentEvent>,
        cancel: &CancellationToken,
    ) -> Streamed {
        let mut text = String::new();
        let mut calls: Vec<ToolCall> = Vec::new();
        let mut emitted = false;
        let mut stream = tokio::select! {
            s = self.client.stream(req) => match s {
                Ok(s) => s,
                Err(error) => return Streamed::Failed { error, text, emitted },
            },
            _ = cancel.cancelled() => return Streamed::Cancelled(text, calls),
        };
        loop {
            let ev = tokio::select! {
                ev = stream.next() => ev,
                _ = cancel.cancelled() => return Streamed::Cancelled(text, calls),
            };
            match ev {
                None | Some(Ok(StreamEvent::Done)) => return Streamed::Done(text, calls),
                Some(Err(error)) => {
                    return Streamed::Failed {
                        error,
                        text,
                        emitted,
                    };
                }
                Some(Ok(StreamEvent::TextDelta(t))) => {
                    text.push_str(&t);
                    emitted = true;
                    let _ = events.send(AgentEvent::TextDelta(t));
                }
                Some(Ok(StreamEvent::ReasoningDelta(t))) => {
                    emitted = true;
                    let _ = events.send(AgentEvent::ReasoningDelta(t));
                }
                Some(Ok(StreamEvent::ToolCall(c))) => calls.push(c),
                Some(Ok(StreamEvent::Usage(u))) => {
                    self.usage.add(u);
                    self.last_input_tokens = u.input_tokens;
                    let _ = events.send(AgentEvent::Usage(u));
                }
            }
        }
    }

    /// The next model request: system prompt, the whole history and the tools on offer.
    pub(crate) fn request(&self, interactive: bool) -> ChatRequest {
        ChatRequest {
            model: self.model.clone(),
            system: Some(self.system_prompt()),
            messages: self.history.clone(),
            tools: self.all_specs(interactive),
            max_tokens: None,
            session_id: Some(self.session_id()),
        }
    }

    fn child_base(&self) -> ChildBase {
        ChildBase {
            client: self.client.clone(),
            model: self.model.clone(),
            cwd: self.cx.cwd.clone(),
            allowed_dirs: self.allowed_dirs.clone(),
        }
    }

    fn report_end(
        &self,
        call: &ToolCall,
        r: &(String, bool),
        events: &UnboundedSender<AgentEvent>,
    ) {
        let _ = events.send(AgentEvent::ToolEnd {
            id: call.id.clone(),
            output: r.0.clone(),
            is_error: r.1,
        });
    }

    fn auto_allowed(&self, call: &ToolCall) -> bool {
        if call.name == "question" {
            return false; // interactive: never batched with other calls
        }
        if call.name == "task" {
            return !self.is_child;
        }
        let outside = self.outside_folders(call);
        if !outside.is_empty() && self.outside_rule(&outside) != Effect::Allow {
            return false; // asks first
        }
        self.tools
            .get(&call.name)
            .is_some_and(|t| match self.decision(call, t) {
                Decision::Allow => true,
                Decision::Review => {
                    magia_tools::auto::classify(&call.name, &call.arguments, &self.cx.cwd)
                        == Verdict::Safe
                }
                _ => false,
            })
    }

    /// Folders outside the project that `call` would reach and that are not allowed yet
    /// (nothing in `yolo`).
    fn outside_folders(&self, call: &ToolCall) -> Vec<PathBuf> {
        if self.mode == PermissionMode::Yolo {
            return vec![];
        }
        let granted = self
            .allowed_dirs
            .lock()
            .map(|g| g.clone())
            .unwrap_or_default();
        magia_tools::outside::folders(&call.name, &call.arguments, &self.cx.cwd, &granted)
    }

    /// The `external_directory` rules (OpenCode's key) for these folders: any deny wins, all
    /// allowed passes, anything else asks.
    fn outside_rule(&self, dirs: &[PathBuf]) -> Effect {
        let effects: Vec<Option<Effect>> = dirs
            .iter()
            .map(|d| {
                self.rules
                    .check("external_directory", &d.display().to_string())
            })
            .collect();
        if effects.contains(&Some(Effect::Deny)) {
            Effect::Deny
        } else if effects.iter().all(|e| *e == Some(Effect::Allow)) {
            Effect::Allow
        } else {
            Effect::Ask
        }
    }

    /// Ask before a call reaches outside the project. `Ok(true)` = the user just approved this
    /// call; `Ok(false)` = nothing outside (or allowed by the rules); `Err` = refused.
    async fn outside_check(
        &self,
        call: &ToolCall,
        approver: &dyn Approver,
    ) -> Result<bool, String> {
        let dirs = self.outside_folders(call);
        if dirs.is_empty() {
            return Ok(false);
        }
        let list = dirs
            .iter()
            .map(|d| d.display().to_string())
            .collect::<Vec<_>>()
            .join(", ");
        match self.outside_rule(&dirs) {
            Effect::Allow => return Ok(false),
            Effect::Deny => {
                return Err(format!(
                    "{list} is outside the project and the permission rules deny it. Do not try to reach it another way."
                ));
            }
            Effect::Ask => {}
        }
        if self.is_child {
            return Err(format!(
                "{list} is outside the project; a subagent cannot ask for access. Report that the main agent needs it."
            ));
        }
        match approver.approve_folder(call, &dirs).await {
            FolderAnswer::Once => Ok(true),
            FolderAnswer::Always => {
                let all = {
                    let mut g = self.allowed_dirs.lock().unwrap();
                    g.extend(dirs);
                    g.sort();
                    g.dedup();
                    g.clone()
                };
                if let Some(s) = &self.store {
                    let _ = s.set_allowed_dirs(&all);
                }
                Ok(true)
            }
            FolderAnswer::Deny => {
                self.denied.store(true, Ordering::Relaxed);
                Err(format!(
                    "the user denied access to {list} (outside the project). Do not try to reach it another way."
                ))
            }
        }
    }

    /// `auto` mode: run safe calls, ask about risky ones, let the reviewer settle the rest.
    /// `Err` carries the message returned to the model when the call may not run.
    async fn auto_check(
        &self,
        call: &ToolCall,
        events: &UnboundedSender<AgentEvent>,
        approver: &dyn Approver,
    ) -> Result<(), String> {
        let reason = match magia_tools::auto::classify(&call.name, &call.arguments, &self.cx.cwd) {
            Verdict::Safe => return Ok(()),
            Verdict::Risky(why) => why,
            Verdict::Unsure => {
                let key = format!("{}\u{0}{}", call.name, call.arguments);
                if self.reviewed.lock().is_ok_and(|r| r.contains(&key)) {
                    return Ok(());
                }
                let model = self.auto_model.as_deref().unwrap_or(&self.model);
                let r = crate::review::review(
                    &self.client,
                    model,
                    &crate::review::user_requests(&self.history),
                    call,
                    &self.cx.cwd,
                    self.session_id(),
                )
                .await;
                if r.allow {
                    if let Ok(mut set) = self.reviewed.lock() {
                        set.insert(key);
                    }
                    let _ =
                        events.send(AgentEvent::Notice(format!("auto: allowed — {}", r.reason)));
                    return Ok(());
                }
                r.reason
            }
        };
        if !approver.interactive() {
            return Err(format!(
                "blocked by auto mode: {reason}. Do not retry it another way; tell the user what you wanted to run and why, so they can run it or switch mode."
            ));
        }
        if approver.approve_because(call, &reason).await {
            Ok(())
        } else {
            self.denied.store(true, Ordering::Relaxed);
            Err(format!(
                "the user denied this tool call (auto mode flagged it: {reason})"
            ))
        }
    }

    /// Keep the history valid after cancellation: every tool call needs a result.
    fn cancelled(&mut self, text: String, calls: Vec<ToolCall>) {
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
                // Waits here when MAX_PARALLEL_SUBAGENTS are already running.
                let _slot = self.subagents.acquire().await;
                Ok(run_subagent(
                    self.child_base(),
                    prompt,
                    RESEARCHER,
                    self.cx.cancel.clone(),
                    Some((events.clone(), call.id.clone())),
                )
                .await)
            }
        } else {
            let Some(tool) = self.tools.get(&call.name) else {
                return (format!("unknown tool `{}`", call.name), true);
            };
            let decision = self.decision(call, tool);
            if decision == Decision::Deny {
                return (
                    format!("`{}` is not allowed in {:?} mode", call.name, self.mode),
                    true,
                );
            }
            // Reaching outside the project asks first; a yes there approves the call too. A
            // call `auto` mode finds risky skips this question: its own warning asks instead
            // (once), so "always allow this folder" never waves through `rm -rf ~`.
            let risky = decision == Decision::Review
                && matches!(
                    magia_tools::auto::classify(&call.name, &call.arguments, &self.cx.cwd),
                    Verdict::Risky(_)
                );
            let approved = if risky {
                false
            } else {
                match self.outside_check(call, approver).await {
                    Ok(asked) => asked,
                    Err(msg) => return (msg, true),
                }
            };
            match decision {
                _ if approved => {}
                Decision::Ask if !approver.approve(call).await => {
                    self.denied.store(true, Ordering::Relaxed);
                    return ("the user denied this tool call".into(), true);
                }
                Decision::Review => {
                    if let Err(msg) = self.auto_check(call, events, approver).await {
                        return (msg, true);
                    }
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
        // Only the history can be compacted, so the fixed prompt (which a big file agent can
        // push past the threshold on its own) is taken out of the reported size.
        let fixed = (self.system_prompt().len() / 4) as u64;
        let size = estimate_tokens(&self.history).max(self.last_input_tokens.saturating_sub(fixed));
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
        let once = || async {
            let mut stream = self.client.stream(&req).await?;
            let mut out = String::new();
            while let Some(ev) = stream.next().await {
                if let StreamEvent::TextDelta(t) = ev? {
                    out.push_str(&t);
                }
            }
            anyhow::Ok(out)
        };
        let mut attempt = 0;
        let out = loop {
            match once().await {
                Ok(out) => break out,
                Err(e) => match magia_providers::retry_after(&e) {
                    Some(min) if attempt < 3 => {
                        attempt += 1;
                        tokio::time::sleep(min.max(self.retry_delay).min(MAX_RETRY_DELAY)).await;
                    }
                    _ => return Err(e),
                },
            }
        };
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
            .filter(|(_, m)| {
                matches!(m, Message::User { text, .. } if !text.starts_with(SUMMARY_PREFIX))
                    && !magia_mode::is_continuation(m)
            })
            .map(|(i, _)| i)
            .collect();
        if n == 0 || n > turns.len() {
            bail!("can rewind between 1 and {} turns", turns.len());
        }
        let start = turns[turns.len() - n];
        self.history.truncate(start);
        self.restore_todos(); // the list as it was before those turns
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
            Some(stem) => format!("magia-{}", stem.to_string_lossy()),
            None => self.fallback_session.clone(),
        }
    }

    /// Start a fresh conversation (new history, new session file, new session id).
    pub fn new_session(&mut self) -> Result<()> {
        self.history.clear();
        self.last_input_tokens = 0;
        self.forget_session_state();
        self.store = Some(SessionFile::create_for(
            &magia_store::sessions_dir(),
            &self.cx.cwd,
        )?);
        Ok(())
    }

    /// Continue a saved session (only one that belongs to this working folder).
    pub fn resume(&mut self, path: &Path) -> Result<usize> {
        let sf = SessionFile::resume(&magia_store::sessions_dir(), path, &self.cx.cwd)?;
        self.history = sf.load()?;
        self.last_input_tokens = 0;
        self.forget_session_state();
        self.restore_todos();
        if let Ok(mut g) = self.allowed_dirs.lock() {
            *g = sf.meta().allowed_dirs;
        }
        self.store = Some(sf);
        Ok(self.history.len())
    }

    /// The task list as the history left it: the last `todo` call that succeeded.
    fn restore_todos(&mut self) {
        if let Ok(mut t) = self.cx.todos.lock() {
            *t = todos_in(&self.history);
        }
    }

    /// Per-conversation state that must not leak into the next one: the task list and the
    /// auto-mode reviewer's allows (they were approved against the old user requests).
    fn forget_session_state(&mut self) {
        if let Ok(mut t) = self.cx.todos.lock() {
            t.clear();
        }
        if let Ok(mut r) = self.reviewed.lock() {
            r.clear();
        }
        if let Ok(mut g) = self.allowed_dirs.lock() {
            g.clear();
        }
    }

    pub fn fork(&mut self) -> Result<PathBuf> {
        let new = SessionFile::create_for(&magia_store::sessions_dir(), &self.cx.cwd)?;
        for m in &self.history {
            new.append(m)?;
        }
        let dirs = self
            .allowed_dirs
            .lock()
            .map(|g| g.clone())
            .unwrap_or_default();
        if !dirs.is_empty() {
            new.set_allowed_dirs(&dirs)?;
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
/// The list sent by the last successful `todo` call in `history` (empty if none).
fn todos_in(history: &[Message]) -> Vec<magia_tools::Todo> {
    let ok = |id: &str| {
        history.iter().any(
            |m| matches!(m, Message::ToolResult { call_id, is_error: false, .. } if call_id == id),
        )
    };
    history
        .iter()
        .rev()
        .filter_map(|m| match m {
            Message::Assistant { tool_calls, .. } => Some(tool_calls),
            _ => None,
        })
        .flat_map(|calls| calls.iter().rev())
        .filter(|c| c.name == "todo" && ok(&c.id))
        .find_map(|c| serde_json::from_value(c.arguments.get("todos")?.clone()).ok())
        .unwrap_or_default()
}

fn new_session_id() -> String {
    use std::hash::{BuildHasher, Hasher};
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u128(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos()),
    );
    h.write_u32(std::process::id());
    format!("magia-{:016x}", h.finish())
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

const RESEARCHER: &str = "You are a read-only research subagent. Investigate, then reply with a concise report (no preamble).";

/// A read-only child agent with a fresh context; returns its final report.
/// What a subagent takes from its parent.
struct ChildBase {
    client: Client,
    model: String,
    cwd: PathBuf,
    /// The parent's folders outside the project that the user allowed.
    allowed_dirs: Arc<std::sync::Mutex<Vec<PathBuf>>>,
}

fn run_subagent(
    base: ChildBase,
    prompt: String,
    role: &'static str,
    cancel: CancellationToken,
    // Where to report the child's tool calls live, under which parent id.
    forward: Option<(UnboundedSender<AgentEvent>, String)>,
) -> BoxFuture<'static, String> {
    let ChildBase {
        client,
        model,
        cwd,
        allowed_dirs,
    } = base;
    async move {
        let mut child = Agent::new(client, model, PermissionMode::Plan, cwd);
        child.is_child = true;
        child.allowed_dirs = allowed_dirs;
        child.compact_at_tokens = 0;
        child.system.push_str("\n\n");
        child.system.push_str(role);
        let (tx, mut rx) = unbounded_channel();
        let mut error = None;
        // `tx` is dropped when the turn ends, which ends the receiving side.
        let watch = async {
            while let Some(ev) = rx.recv().await {
                match ev {
                    AgentEvent::Error(e) => error = Some(e),
                    AgentEvent::ToolStart { name, input, .. } => {
                        if let Some((parent_tx, parent)) = &forward {
                            let _ = parent_tx.send(AgentEvent::SubTool {
                                parent: parent.clone(),
                                name,
                                input,
                            });
                        }
                    }
                    _ => {}
                }
            }
        };
        tokio::join!(
            child.run_turn(prompt, tx, cancel, &AutoApprove(false)),
            watch
        );
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
        "You are magia, a coding agent running in the user's terminal/desktop. \
Work in the project directory `{}`. Use the tools to inspect and change files; read a file before editing it. \
Use `todo` to plan multi-step work. Use `websearch`/`webfetch` for documentation you do not know, \
and `question` only when the user must decide. Be concise.",
        cwd.display()
    );
    if let Some(name) = project_instructions_file(cwd)
        && let Ok(text) = std::fs::read_to_string(cwd.join(name))
    {
        s.push_str(&format!("\n\n# Project instructions ({name})\n{text}"));
    }
    s
}

/// `AGENTS.md`, else `CLAUDE.md`, if the project has one.
fn project_instructions_file(cwd: &std::path::Path) -> Option<&'static str> {
    ["AGENTS.md", "CLAUDE.md"]
        .into_iter()
        .find(|n| cwd.join(n).is_file())
}
