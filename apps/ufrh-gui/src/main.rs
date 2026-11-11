//! Desktop front-end (egui/eframe, OpenGL only). All I/O runs on a small tokio runtime;
//! `update()` only drains channels and draws.

use async_trait::async_trait;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use tokio_util::sync::CancellationToken;
use ufrh_core::{Agent, AgentEvent, Approver, Config};
use ufrh_providers::Client;
use ufrh_store::{SessionFile, sessions_dir};
use ufrh_tools::PermissionMode;
use ufrh_types::{Message, ToolCall};

enum UiMsg {
    Agent(AgentEvent),
    Models(Result<Vec<String>, String>),
    Permission(ToolCall, tokio::sync::oneshot::Sender<bool>),
}

enum Item {
    User(String),
    Assistant {
        text: String,
        reasoning: String,
    },
    Tool {
        id: String,
        name: String,
        input: String,
        output: Option<(String, bool)>,
    },
    Error(String),
}

struct GuiApprover {
    tx: UnboundedSender<UiMsg>,
    ctx: egui::Context,
}

#[async_trait]
impl Approver for GuiApprover {
    async fn approve(&self, call: &ToolCall) -> bool {
        let (reply, answer) = tokio::sync::oneshot::channel();
        let _ = self.tx.send(UiMsg::Permission(call.clone(), reply));
        self.ctx.request_repaint();
        answer.await.unwrap_or(false)
    }
}

struct App {
    rt: Arc<tokio::runtime::Runtime>,
    agent: Arc<tokio::sync::Mutex<Agent>>,
    cfg: Config,
    cwd: PathBuf,
    tx: UnboundedSender<UiMsg>,
    rx: UnboundedReceiver<UiMsg>,
    items: Vec<Item>,
    input: String,
    running: bool,
    cancel: CancellationToken,
    pending: Option<(ToolCall, tokio::sync::oneshot::Sender<bool>)>,
    models: Vec<String>,
    status: String,
    tokens: (u64, u64),
    /// Cached rendered height per item (0 = not measured yet); drives the virtualized list.
    heights: Vec<f32>,
    last_width: f32,
    /// Created lazily (it starts a background runtime): only when the window is unfocused.
    notifier: Option<fastframe_notifications::Client>,
    todos: Arc<std::sync::Mutex<Vec<ufrh_tools::Todo>>>,
}

const PROVIDERS: [&str; 3] = ["zen", "go", "openrouter"];
const MODES: [(PermissionMode, &str); 4] = [
    (PermissionMode::Ask, "ask"),
    (PermissionMode::AcceptEdits, "accept edits"),
    (PermissionMode::Plan, "plan (read-only)"),
    (PermissionMode::Yolo, "yolo"),
];

impl App {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        // fastframe: fonts + desktop text rendering, before the first frame.
        let mut fonts = fastframe_fonts::FontSetup::default().definitions();
        let rendering = fastframe_text::detect();
        rendering.apply_to(&mut fonts);
        cc.egui_ctx.set_fonts(fonts);
        cc.egui_ctx
            .all_styles_mut(|s| rendering.apply_to_visuals(&mut s.visuals));

        let rt = Arc::new(
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .max_blocking_threads(4)
                .enable_all()
                .build()
                .expect("tokio runtime"),
        );
        let (cfg, status) = match Config::load() {
            Ok(c) => (c, String::new()),
            Err(e) => (Config::default(), format!("config error: {e:#}")),
        };
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let client = Client::new(
            cfg.provider_config(&cfg.provider)
                .or_else(|_| cfg.provider_config("zen"))
                .expect("zen preset"),
        )
        .expect("http client");
        let mut agent = Agent::new(client, cfg.model.clone(), cfg.permission_mode, cwd.clone());
        agent.configure(&cfg);
        agent.store = SessionFile::create_in(&sessions_dir()).ok();
        let todos = agent.cx.todos.clone();
        let (tx, rx) = unbounded_channel();
        let mut app = Self {
            rt,
            agent: Arc::new(tokio::sync::Mutex::new(agent)),
            cfg,
            cwd,
            tx,
            rx,
            items: vec![],
            input: String::new(),
            running: false,
            cancel: CancellationToken::new(),
            pending: None,
            models: vec![],
            status,
            tokens: (0, 0),
            heights: vec![],
            last_width: 0.0,
            notifier: None,
            todos,
        };
        app.refresh_models(&cc.egui_ctx);
        app.start_mcp(&cc.egui_ctx);
        app
    }

    /// Rebuild the client from the current provider/model/mode settings (only while idle).
    fn apply_settings(&mut self, ctx: &egui::Context) {
        match self
            .cfg
            .provider_config(&self.cfg.provider)
            .and_then(Client::new)
        {
            Ok(client) => {
                let missing_key = client.config.api_key.is_none();
                if let Ok(mut a) = self.agent.try_lock() {
                    a.client = client;
                    a.model = self.cfg.model.clone();
                    a.mode = self.cfg.permission_mode;
                }
                self.status = if missing_key {
                    format!("no API key for `{}` in the environment", self.cfg.provider)
                } else {
                    String::new()
                };
                let _ = self.cfg.save();
                self.refresh_models(ctx);
            }
            Err(e) => self.status = format!("{e:#}"),
        }
    }

    fn refresh_models(&mut self, ctx: &egui::Context) {
        let Ok(a) = self.agent.try_lock() else { return };
        let client = a.client.clone();
        drop(a);
        let (tx, ctx) = (self.tx.clone(), ctx.clone());
        self.rt.spawn(async move {
            let r = client
                .list_models()
                .await
                .map(|m| m.into_iter().map(|m| m.id).collect())
                .map_err(|e| format!("{e:#}"));
            let _ = tx.send(UiMsg::Models(r));
            ctx.request_repaint();
        });
    }

    /// Desktop notification when the user is elsewhere (turn finished / approval needed).
    fn notify(&mut self, ctx: &egui::Context, title: &str, body: &str) {
        if ctx.input(|i| i.viewport().focused.unwrap_or(true)) {
            return;
        }
        if self.notifier.is_none() {
            let app = fastframe_notifications::App::new("dev.ufrh.app", "ufrh");
            self.notifier = fastframe_notifications::Client::new(app).ok();
        }
        if let Some(n) = &self.notifier {
            let _ = n.show(fastframe_notifications::Notification::new(title).body(body));
        }
    }

    fn start_mcp(&self, ctx: &egui::Context) {
        if self.cfg.mcp.is_empty() {
            return;
        }
        let (agent, cfgs, tx, ctx) = (
            self.agent.clone(),
            self.cfg.mcp.clone(),
            self.tx.clone(),
            ctx.clone(),
        );
        self.rt.spawn(async move {
            let errors = agent.lock().await.connect_mcp(&cfgs).await;
            for e in errors {
                let _ = tx.send(UiMsg::Agent(AgentEvent::Notice(format!("mcp: {e}"))));
            }
            ctx.request_repaint();
        });
    }

    fn reset_items(&mut self, history: &[Message]) {
        self.items = items_from_history(history);
        self.heights.clear();
    }

    /// Built-in slash commands. Returns true when the input was handled.
    fn slash(&mut self, input: &str, ctx: &egui::Context) -> bool {
        let mut parts = input.split_whitespace();
        let Some(cmd) = parts.next().and_then(|c| c.strip_prefix('/')) else {
            return false;
        };
        let arg = parts.next();
        let busy = "agent busy; try again in a moment";
        match cmd {
            "help" => {
                self.status = "/clear /compact /rewind [n] /fork /resume — plus custom commands in .ufrh/commands/*.md".into();
            }
            "compact" => {
                let (agent, tx, ctx) = (self.agent.clone(), self.tx.clone(), ctx.clone());
                self.running = true;
                self.rt.spawn(async move {
                    let msg = match agent.lock().await.compact().await {
                        Ok(Some((a, b))) => format!("context compacted: {a} → {b} messages"),
                        Ok(None) => "nothing to compact yet".into(),
                        Err(e) => format!("compaction failed: {e:#}"),
                    };
                    let _ = tx.send(UiMsg::Agent(AgentEvent::Notice(msg)));
                    let _ = tx.send(UiMsg::Agent(AgentEvent::TurnDone));
                    ctx.request_repaint();
                });
            }
            "clear" | "rewind" | "fork" | "resume" => {
                let Ok(mut a) = self.agent.try_lock() else {
                    self.status = busy.into();
                    return true;
                };
                let res: anyhow::Result<String> = match cmd {
                    "clear" => {
                        a.history.clear();
                        a.store = SessionFile::create_in(&sessions_dir()).ok();
                        Ok("new session".into())
                    }
                    "rewind" => a
                        .rewind(arg.and_then(|n| n.parse().ok()).unwrap_or(1))
                        .map(|f| format!("rewound; {} file(s) restored", f.len())),
                    "fork" => a.fork().map(|p| format!("forked into {}", p.display())),
                    _ => match SessionFile::latest_in(&sessions_dir()) {
                        Some(sf) => sf.load().map(|h| {
                            let n = h.len();
                            a.history = h;
                            a.store = Some(sf);
                            format!("resumed ({n} messages)")
                        }),
                        None => Err(anyhow::anyhow!("no saved session")),
                    },
                };
                let history = a.history.clone();
                drop(a);
                match res {
                    Ok(m) => {
                        if cmd != "fork" {
                            self.reset_items(&history);
                        }
                        self.status = m;
                    }
                    Err(e) => self.status = format!("{e:#}"),
                }
            }
            _ => return false,
        }
        true
    }

    fn send(&mut self, ctx: &egui::Context) {
        let mut prompt = self.input.trim().to_owned();
        if prompt.is_empty() || self.running {
            return;
        }
        if prompt.starts_with('/') {
            if self.slash(&prompt, ctx) {
                self.input.clear();
                return;
            }
            if let Some(expanded) = ufrh_core::expand_command(&self.cwd, &prompt) {
                prompt = expanded;
            }
        }
        if self.cfg.model.trim().is_empty() {
            self.status = "pick or type a model first".into();
            return;
        }
        self.input.clear();
        self.items.push(Item::User(prompt.clone()));
        self.running = true;
        self.status.clear();
        self.cancel = CancellationToken::new();
        let (cancel, agent, ui_tx, ctx) = (
            self.cancel.clone(),
            self.agent.clone(),
            self.tx.clone(),
            ctx.clone(),
        );
        let approver = GuiApprover {
            tx: ui_tx.clone(),
            ctx: ctx.clone(),
        };
        self.rt.spawn(async move {
            let (ev_tx, mut ev_rx) = unbounded_channel();
            let fwd = {
                let (ui_tx, ctx) = (ui_tx.clone(), ctx.clone());
                tokio::spawn(async move {
                    while let Some(ev) = ev_rx.recv().await {
                        let _ = ui_tx.send(UiMsg::Agent(ev));
                        ctx.request_repaint();
                    }
                })
            };
            agent
                .lock()
                .await
                .run_turn(prompt, ev_tx, cancel, &approver)
                .await;
            let _ = fwd.await;
        });
    }

    fn drain(&mut self, ctx: &egui::Context) {
        while let Ok(msg) = self.rx.try_recv() {
            match msg {
                UiMsg::Models(Ok(m)) => self.models = m,
                UiMsg::Models(Err(e)) => self.status = format!("models: {e}"),
                UiMsg::Permission(call, reply) => {
                    let body = format!("run `{}`?", call.name);
                    self.pending = Some((call, reply));
                    self.notify(ctx, "ufrh needs your approval", &body);
                }
                UiMsg::Agent(ev) => match ev {
                    AgentEvent::TextDelta(t) => match self.items.last_mut() {
                        Some(Item::Assistant { text, .. }) => text.push_str(&t),
                        _ => self.items.push(Item::Assistant {
                            text: t,
                            reasoning: String::new(),
                        }),
                    },
                    AgentEvent::ReasoningDelta(t) => match self.items.last_mut() {
                        Some(Item::Assistant { reasoning, .. }) => reasoning.push_str(&t),
                        _ => self.items.push(Item::Assistant {
                            text: String::new(),
                            reasoning: t,
                        }),
                    },
                    AgentEvent::ToolStart { id, name, input } => self.items.push(Item::Tool {
                        id,
                        name,
                        input: input.to_string(),
                        output: None,
                    }),
                    AgentEvent::ToolEnd {
                        id,
                        output,
                        is_error,
                    } => {
                        for it in self.items.iter_mut().rev() {
                            if let Item::Tool {
                                id: i, output: o, ..
                            } = it
                                && *i == id
                            {
                                *o = Some((output, is_error));
                                break;
                            }
                        }
                    }
                    AgentEvent::Usage(u) => {
                        self.tokens.0 += u.input_tokens;
                        self.tokens.1 += u.output_tokens;
                    }
                    AgentEvent::Notice(n) => self.status = n,
                    AgentEvent::Error(e) => self.items.push(Item::Error(e)),
                    AgentEvent::TurnDone => {
                        self.running = false;
                        self.pending = None;
                        self.notify(ctx, "ufrh", "finished");
                    }
                },
            }
        }
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.drain(&ctx);
        if self.running && ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            self.cancel.cancel();
        }

        egui::Panel::top("top").show(ui, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.add_enabled_ui(!self.running, |ui| {
                    let mut changed = false;
                    egui::ComboBox::from_id_salt("provider")
                        .selected_text(self.cfg.provider.clone())
                        .show_ui(ui, |ui| {
                            for p in PROVIDERS {
                                changed |= ui
                                    .selectable_value(&mut self.cfg.provider, p.to_owned(), p)
                                    .changed();
                            }
                        });
                    ui.label("model");
                    let r = ui
                        .add(egui::TextEdit::singleline(&mut self.cfg.model).desired_width(220.0));
                    changed |= r.lost_focus() && r.changed();
                    egui::ComboBox::from_id_salt("models")
                        .selected_text("▾")
                        .show_ui(ui, |ui| {
                            egui::ScrollArea::vertical()
                                .max_height(300.0)
                                .show(ui, |ui| {
                                    for m in self.models.clone() {
                                        if ui.selectable_label(self.cfg.model == m, &m).clicked() {
                                            self.cfg.model = m;
                                            changed = true;
                                        }
                                    }
                                });
                        });
                    let mode_name = MODES
                        .iter()
                        .find(|m| m.0 == self.cfg.permission_mode)
                        .map_or("", |m| m.1);
                    egui::ComboBox::from_id_salt("mode")
                        .selected_text(mode_name)
                        .show_ui(ui, |ui| {
                            for (m, name) in MODES {
                                changed |= ui
                                    .selectable_value(&mut self.cfg.permission_mode, m, name)
                                    .changed();
                            }
                        });
                    if changed {
                        self.apply_settings(&ctx);
                    }
                });
            });
        });

        egui::Panel::bottom("composer").show(ui, |ui| {
            if !self.status.is_empty() {
                ui.colored_label(egui::Color32::from_rgb(220, 150, 60), &self.status);
            }
            ui.horizontal(|ui| {
                if ui
                    .add_enabled(!self.running, egui::Button::new("Send"))
                    .clicked()
                {
                    self.send(&ctx);
                }
                if self.running && ui.button("Stop (Esc)").clicked() {
                    self.cancel.cancel();
                }
                ui.weak(format!(
                    "{} · tokens in {} / out {}",
                    self.cwd.display(),
                    self.tokens.0,
                    self.tokens.1
                ));
            });
            let resp = ui.add_sized(
                [ui.available_width(), 64.0],
                egui::TextEdit::multiline(&mut self.input).hint_text("Ctrl+Enter to send"),
            );
            let ctrl_enter = resp.has_focus()
                && ctx.input(|i| i.modifiers.command && i.key_pressed(egui::Key::Enter));
            if ctrl_enter {
                self.send(&ctx);
            }
        });

        egui::CentralPanel::default().show(ui, |ui| {
            let todos = self.todos.lock().map(|t| t.clone()).unwrap_or_default();
            if !todos.is_empty() {
                let done = todos.iter().filter(|t| t.status == "completed").count();
                egui::CollapsingHeader::new(format!("Tasks ({done}/{})", todos.len()))
                    .default_open(true)
                    .show(ui, |ui| {
                        for t in &todos {
                            let mark = match t.status.as_str() {
                                "completed" => "☑",
                                "in_progress" => "▶",
                                _ => "☐",
                            };
                            ui.label(format!("{mark} {}", t.content));
                        }
                    });
            }
            // Virtualized: only items near the viewport are laid out; the rest are skipped
            // using cached heights, so long sessions cost little CPU/RAM.
            egui::ScrollArea::vertical()
                .auto_shrink(false)
                .stick_to_bottom(true)
                .show_viewport(ui, |ui, viewport| {
                    let width = ui.available_width();
                    if (width - self.last_width).abs() > 1.0 {
                        self.heights.iter_mut().for_each(|h| *h = 0.0);
                        self.last_width = width;
                    }
                    self.heights.resize(self.items.len(), 0.0);
                    let spacing = ui.spacing().item_spacing.y;
                    let (top, bottom) = (viewport.min.y - 500.0, viewport.max.y + 500.0);
                    let (mut y, mut skipped) = (0.0f32, 0.0f32);
                    for (n, it) in self.items.iter().enumerate() {
                        let known = self.heights[n] > 0.0;
                        let h = if known { self.heights[n] } else { 80.0 };
                        let visible = if known {
                            y + h >= top && y <= bottom
                        } else {
                            y <= bottom
                        };
                        if visible {
                            if skipped > 0.0 {
                                ui.add_space(skipped);
                                skipped = 0.0;
                            }
                            let start = ui.cursor().min.y;
                            render_item(ui, n, it);
                            self.heights[n] = (ui.cursor().min.y - start).max(1.0);
                        } else {
                            skipped += h + spacing;
                        }
                        y += self.heights[n].max(if known { 0.0 } else { h }) + spacing;
                    }
                    if skipped > 0.0 {
                        ui.add_space(skipped);
                    }
                });
        });

        if let Some((call, _)) = &self.pending {
            let mut answer = None;
            egui::Window::new("Permission required")
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                .show(&ctx, |ui| {
                    ui.label(format!("Run `{}`?", call.name));
                    ui.monospace(
                        call.arguments
                            .to_string()
                            .chars()
                            .take(1500)
                            .collect::<String>(),
                    );
                    ui.horizontal(|ui| {
                        if ui.button("Allow").clicked() {
                            answer = Some(true);
                        }
                        if ui.button("Deny").clicked() {
                            answer = Some(false);
                        }
                    });
                });
            if let Some(a) = answer
                && let Some((_, reply)) = self.pending.take()
            {
                let _ = reply.send(a);
            }
        }
    }
}

fn render_item(ui: &mut egui::Ui, n: usize, it: &Item) {
    match it {
        Item::User(t) => {
            ui.add_space(6.0);
            ui.strong("you");
            ui.label(t);
        }
        Item::Assistant { text, reasoning } => {
            ui.add_space(6.0);
            ui.strong("assistant");
            if !reasoning.is_empty() {
                egui::CollapsingHeader::new("reasoning")
                    .id_salt(("reasoning", n))
                    .show(ui, |ui| {
                        ui.weak(reasoning);
                    });
            }
            ui.label(text);
        }
        Item::Tool {
            id,
            name,
            input,
            output,
        } => {
            let title = match output {
                None => format!("⏳ {name}"),
                Some((_, true)) => format!("✗ {name}"),
                Some(_) => format!("✓ {name}"),
            };
            egui::CollapsingHeader::new(title)
                .id_salt(id)
                .show(ui, |ui| {
                    ui.monospace(input);
                    if let Some((o, _)) = output {
                        ui.separator();
                        ui.monospace(o.chars().take(4000).collect::<String>());
                    }
                });
        }
        Item::Error(e) => {
            ui.colored_label(egui::Color32::LIGHT_RED, e);
        }
    }
}

/// Rebuild the visible transcript from a saved history (resume / rewind / clear).
fn items_from_history(history: &[Message]) -> Vec<Item> {
    let mut items = Vec::new();
    for m in history {
        match m {
            Message::User { text } => items.push(Item::User(text.clone())),
            Message::Assistant { text, tool_calls } => {
                if !text.is_empty() {
                    items.push(Item::Assistant {
                        text: text.clone(),
                        reasoning: String::new(),
                    });
                }
                for c in tool_calls {
                    items.push(Item::Tool {
                        id: c.id.clone(),
                        name: c.name.clone(),
                        input: c.arguments.to_string(),
                        output: None,
                    });
                }
            }
            Message::ToolResult {
                call_id,
                content,
                is_error,
            } => {
                for it in items.iter_mut().rev() {
                    if let Item::Tool { id, output, .. } = it
                        && id == call_id
                    {
                        *output = Some((content.clone(), *is_error));
                        break;
                    }
                }
            }
        }
    }
    items
}

fn main() {
    let opts = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([900.0, 700.0])
            .with_title("ufrh"),
        ..Default::default()
    };
    let result = fastframe_graphics::Graphics::new("ufrh").run_native(
        move || opts.clone(),
        || Box::new(|cc| Ok(Box::new(App::new(cc)))),
    );
    if let Err(e) = result {
        eprintln!("ufrh: {e}");
        std::process::exit(1);
    }
}
