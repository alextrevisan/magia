//! Desktop front-end (egui/eframe, OpenGL only). All I/O runs on a small tokio runtime;
//! `update()` only drains channels and draws.

use async_trait::async_trait;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use tokio_util::sync::CancellationToken;
mod clipboard;
mod composer;
mod resume;

use ufrh_core::{Agent, AgentEvent, Approver, Config, SlashEntry, slash_catalog};
use ufrh_providers::Client;
use ufrh_store::{SessionFile, sessions_dir};
use ufrh_tools::PermissionMode;
use ufrh_types::{Image, Message, ToolCall};

enum UiMsg {
    Agent(AgentEvent),
    Models(Result<Vec<String>, String>),
    Permission(ToolCall, tokio::sync::oneshot::Sender<bool>),
    Question(Question),
    Catalog(Vec<SlashEntry>),
    /// Images read from the clipboard or from files, with their RGBA previews.
    Attach(Result<Vec<(Image, Option<Thumb>)>, String>),
    /// Results of a `/resume` search: (generation, hits).
    Sessions(u64, Vec<ufrh_store::Hit>),
}

/// RGBA preview: width, height, pixels.
type Thumb = (u32, u32, Vec<u8>);

struct Question {
    text: String,
    options: Vec<String>,
    reply: tokio::sync::oneshot::Sender<Option<String>>,
}

enum Item {
    /// Text and a label per attached image.
    User(String, Vec<String>),
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

    async fn ask(&self, question: &str, options: &[String]) -> Option<String> {
        let (reply, answer) = tokio::sync::oneshot::channel();
        let q = Question {
            text: question.to_owned(),
            options: options.to_vec(),
            reply,
        };
        let _ = self.tx.send(UiMsg::Question(q));
        self.ctx.request_repaint();
        answer.await.ok().flatten()
    }

    fn interactive(&self) -> bool {
        true
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
    composer: composer::Composer,
    /// The `/resume` picker, while open.
    resume: Option<resume::Picker>,
    running: bool,
    cancel: CancellationToken,
    pending: Option<(ToolCall, tokio::sync::oneshot::Sender<bool>)>,
    /// Open question from the model and the user's typed answer.
    question: Option<(Question, String)>,
    /// `/` autocomplete: everything available (refreshed off the UI thread when the popup opens).
    catalog: Vec<SlashEntry>,
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
            Ok(c) => {
                let w = c.warnings.join("; ");
                (c, w)
            }
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
        agent.store = SessionFile::create_for(&sessions_dir(), &cwd).ok();
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
            composer: composer::Composer::default(),
            resume: None,
            running: false,
            cancel: CancellationToken::new(),
            pending: None,
            question: None,
            catalog: vec![],
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
        app.refresh_catalog(&cc.egui_ctx);
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
                    format!(
                        "no API key for `{0}`: set provider.{0}.options.apiKey in {1} or {2}",
                        self.cfg.provider,
                        ufrh_core::config_path().display(),
                        ufrh_providers::default_key_env(&self.cfg.provider)
                    )
                } else {
                    String::new()
                };
                let _ = self.cfg.save_state();
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
        let declared = self.cfg.declared_models(&self.cfg.provider);
        self.rt.spawn(async move {
            // Models declared in the JSON config are always offered, even if `/models` fails.
            let r = match client.list_models().await {
                Ok(m) => {
                    let mut ids: Vec<String> = m.into_iter().map(|m| m.id).collect();
                    for d in declared {
                        if !ids.contains(&d) {
                            ids.push(d);
                        }
                    }
                    Ok(ids)
                }
                Err(_) if !declared.is_empty() => Ok(declared),
                Err(e) => Err(format!("{e:#}")),
            };
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

    /// Rebuild the `/` catalog on a blocking thread (it reads command files and skills).
    fn refresh_catalog(&self, ctx: &egui::Context) {
        let (cwd, cfg, tx, ctx) = (
            self.cwd.clone(),
            self.cfg.clone(),
            self.tx.clone(),
            ctx.clone(),
        );
        self.rt.spawn_blocking(move || {
            let _ = tx.send(UiMsg::Catalog(slash_catalog(&cwd, BUILTINS, &cfg)));
            ctx.request_repaint();
        });
    }

    /// Read clipboard images (`files` empty) or load image files, off the UI thread.
    fn attach_images(&self, ctx: &egui::Context, files: Vec<PathBuf>) {
        let (tx, ctx) = (self.tx.clone(), ctx.clone());
        self.rt.spawn(async move {
            let res = if files.is_empty() {
                clipboard::read_images().await
            } else {
                clipboard::load_paths(files).await
            };
            let res = match res {
                Ok(images) => tokio::task::spawn_blocking(move || {
                    images
                        .into_iter()
                        .map(|i| {
                            let t = ufrh_core::media::thumbnail(&i, composer::THUMB);
                            (i, t)
                        })
                        .collect()
                })
                .await
                .map_err(|e| e.to_string()),
                Err(e) => Err(format!("{e:#}")),
            };
            let _ = tx.send(UiMsg::Attach(res));
            ctx.request_repaint();
        });
    }

    /// Draw the `/resume` picker and carry out what it asks for.
    fn resume_picker(&mut self, ctx: &egui::Context) {
        let Some(p) = &mut self.resume else { return };
        for action in p.show(ctx) {
            match action {
                resume::Action::Search {
                    generation,
                    query,
                    all,
                } => {
                    let (tx, ctx, cwd) = (self.tx.clone(), ctx.clone(), self.cwd.clone());
                    self.rt.spawn_blocking(move || {
                        let scope = if all {
                            ufrh_store::Scope::All
                        } else {
                            ufrh_store::Scope::Folder(&cwd)
                        };
                        let hits = ufrh_store::search(&sessions_dir(), scope, &query);
                        let _ = tx.send(UiMsg::Sessions(generation, hits));
                        ctx.request_repaint();
                    });
                }
                resume::Action::Open(path) => {
                    let Ok(mut a) = self.agent.try_lock() else {
                        self.status = "busy — wait for the current turn to finish".into();
                        continue;
                    };
                    match a.resume(&path) {
                        Ok(n) => {
                            let history = a.history.clone();
                            drop(a);
                            self.reset_items(&history);
                            self.status = format!("resumed ({n} messages)");
                            self.resume = None;
                            ctx.memory_mut(|m| m.request_focus(composer::Composer::id()));
                            return;
                        }
                        Err(e) => self.status = format!("{e:#}"),
                    }
                }
                resume::Action::Close => {
                    self.resume = None;
                    ctx.memory_mut(|m| m.request_focus(composer::Composer::id()));
                    return;
                }
            }
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
                let names: Vec<String> = self
                    .catalog
                    .iter()
                    .map(|e| format!("/{}", e.name))
                    .collect();
                self.status = format!("{} — type / to browse", names.join(" "));
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
            "resume" => {
                let Ok(a) = self.agent.try_lock() else {
                    self.status = busy.into();
                    return true;
                };
                let current = a.store.as_ref().map(|s| s.path().to_path_buf());
                drop(a);
                self.resume = Some(resume::Picker::new(&self.cwd, arg.unwrap_or(""), current));
            }
            "clear" | "rewind" | "fork" => {
                let Ok(mut a) = self.agent.try_lock() else {
                    self.status = busy.into();
                    return true;
                };
                let res: anyhow::Result<String> = match cmd {
                    "clear" => a.new_session().map(|_| "new session".into()),
                    "rewind" => a
                        .rewind(arg.and_then(|n| n.parse().ok()).unwrap_or(1))
                        .map(|f| format!("rewound; {} file(s) restored", f.len())),
                    _ => a.fork().map(|p| format!("forked into {}", p.display())),
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
        let mut prompt = self.composer.input.trim().to_owned();
        if (prompt.is_empty() && self.composer.attachments.is_empty()) || self.running {
            return;
        }
        if prompt.starts_with('/') {
            if self.slash(&prompt, ctx) {
                self.composer.input.clear();
                return;
            }
            if let Some(expanded) =
                ufrh_core::expand_command(&self.cwd, &prompt, &self.cfg.commands)
            {
                prompt = expanded;
            } else {
                // `/word` with no such command is a typo, not a message; `/etc/hosts …` is a path.
                let first = prompt.split_whitespace().next().unwrap_or("");
                if !first[1..].contains('/') {
                    self.status = format!("unknown command `{first}` — type / to see the list");
                    return;
                }
            }
        }
        if self.cfg.model.trim().is_empty() {
            self.status = "pick or type a model first".into();
            return;
        }
        self.composer.input.clear();
        let images = self.composer.take_images();
        self.items.push(Item::User(
            prompt.clone(),
            images.iter().map(composer::image_label).collect(),
        ));
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
                .run_turn(
                    ufrh_core::UserInput {
                        text: prompt,
                        images,
                    },
                    ev_tx,
                    cancel,
                    &approver,
                )
                .await;
            let _ = fwd.await;
        });
    }

    fn drain(&mut self, ctx: &egui::Context) {
        while let Ok(msg) = self.rx.try_recv() {
            match msg {
                UiMsg::Models(Ok(m)) => self.models = m,
                UiMsg::Models(Err(e)) => self.status = format!("models: {e}"),
                UiMsg::Catalog(c) => self.catalog = c,
                UiMsg::Sessions(generation, hits) => {
                    if let Some(p) = &mut self.resume {
                        p.set_results(generation, hits);
                    }
                }
                UiMsg::Attach(Ok(v)) if v.is_empty() => {
                    self.status = "the clipboard has no image".into();
                }
                UiMsg::Attach(Ok(v)) => {
                    for (img, thumb) in v {
                        self.composer.attach(ctx, img, thumb);
                    }
                    self.status.clear();
                }
                UiMsg::Attach(Err(e)) => self.status = format!("could not attach the image: {e}"),
                UiMsg::Question(q) => {
                    let body = q.text.chars().take(120).collect::<String>();
                    self.question = Some((q, String::new()));
                    self.notify(ctx, "ufrh has a question", &body);
                }
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
                        self.question = None;
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
        if self.running
            && !self.composer.input.starts_with('/')
            && ctx.input(|i| i.key_pressed(egui::Key::Escape))
        {
            self.cancel.cancel();
        }

        // Before the panels: the picker takes ↑↓/Enter/Esc ahead of the composer.
        self.resume_picker(&ctx);

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
            // Enter sends, Shift+Enter inserts a newline. The plain-Enter event is removed before
            // the TextEdit sees it (egui's consume_key would also swallow Shift+Enter, since it
            // ignores extra Shift), and the TextEdit's own newline key becomes Shift+Enter.
            let out = self.composer.show(ui, &self.catalog);
            if out.opened {
                self.refresh_catalog(&ctx); // pick up commands/skills added since last time
            }
            if out.paste_image {
                self.attach_images(&ctx, vec![]);
            }
            if !out.files.is_empty() {
                self.attach_images(&ctx, out.files);
            }
            if out.send {
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

        if let Some((q, typed)) = &mut self.question {
            let mut answer: Option<Option<String>> = None;
            egui::Window::new("Question from the agent")
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                .show(&ctx, |ui| {
                    ui.label(&q.text);
                    ui.add_space(4.0);
                    for o in &q.options {
                        if ui.button(o).clicked() {
                            answer = Some(Some(o.clone()));
                        }
                    }
                    let r =
                        ui.add(egui::TextEdit::singleline(typed).hint_text("or type an answer"));
                    let enter = r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                    ui.horizontal(|ui| {
                        if (ui
                            .add_enabled(!typed.trim().is_empty(), egui::Button::new("Answer"))
                            .clicked()
                            || enter)
                            && !typed.trim().is_empty()
                        {
                            answer = Some(Some(typed.trim().to_owned()));
                        }
                        if ui.button("Skip").clicked() {
                            answer = Some(None);
                        }
                    });
                });
            if let Some(a) = answer
                && let Some((q, _)) = self.question.take()
            {
                if let Some(text) = &a {
                    self.items
                        .push(Item::User(format!("(answer) {text}"), vec![]));
                }
                let _ = q.reply.send(a);
            }
        }

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

/// Built-in commands handled by `App::slash` (name, description, argument hint).
const BUILTINS: &[(&str, &str, &str)] = &[
    ("help", "List the available commands", ""),
    ("clear", "Start a new session", ""),
    ("compact", "Summarize older history to free context", ""),
    (
        "rewind",
        "Undo the last turn(s): messages and file edits",
        "[n]",
    ),
    ("fork", "Continue in a copy of this session", ""),
    (
        "resume",
        "Pick a saved session of this folder (search every folder)",
        "[search]",
    ),
];

fn render_item(ui: &mut egui::Ui, n: usize, it: &Item) {
    match it {
        Item::User(t, images) => {
            ui.add_space(6.0);
            ui.strong("you");
            for l in images {
                ui.weak(format!("[image {l}]"));
            }
            if !t.is_empty() {
                ui.label(t);
            }
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
            Message::User { text, images } => items.push(Item::User(
                text.clone(),
                images.iter().map(composer::image_label).collect(),
            )),
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
