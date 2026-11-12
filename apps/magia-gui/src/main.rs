//! Desktop front-end (egui/eframe, OpenGL only). All I/O runs on a small tokio runtime;
//! `update()` only drains channels and draws.

// No console window next to the app on Windows (release builds; debug keeps it for logs).
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

use async_trait::async_trait;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use tokio_util::sync::CancellationToken;
mod clipboard;
mod composer;
mod markdown;
mod resume;
mod theme;

use magia_core::{Agent, AgentEvent, Approver, Config, SlashEntry, slash_catalog};
use magia_providers::Client;
use magia_store::{SessionFile, sessions_dir};
use magia_tools::PermissionMode;
use magia_types::{Image, Message, ToolCall};

enum UiMsg {
    Agent(AgentEvent),
    Models(Result<Vec<String>, String>),
    /// A call to approve, with the reason when `auto` mode flagged it.
    Permission(ToolCall, Option<String>, tokio::sync::oneshot::Sender<bool>),
    Question(Question),
    Catalog(Vec<SlashEntry>),
    /// Project files for the `@` autocomplete.
    Files(Vec<String>),
    /// Images read from the clipboard or from files, with their RGBA previews.
    Attach(Result<Vec<(Image, Option<Thumb>)>, String>),
    /// Earlier messages for the composer's ↑/↓ history (from the folder's latest session).
    History(Vec<String>),
    /// Results of a `/resume` search: (generation, hits).
    Sessions(u64, Vec<magia_store::Hit>),
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
        /// The key argument (command, path…), shown on the collapsed line.
        summary: String,
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
        let _ = self.tx.send(UiMsg::Permission(call.clone(), None, reply));
        self.ctx.request_repaint();
        answer.await.unwrap_or(false)
    }

    async fn approve_because(&self, call: &ToolCall, reason: &str) -> bool {
        let (reply, answer) = tokio::sync::oneshot::channel();
        let _ = self.tx.send(UiMsg::Permission(
            call.clone(),
            Some(reason.to_owned()),
            reply,
        ));
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
    /// Name of the open session (its first message), shown in the window title.
    session_title: String,
    /// Title last sent to the window, so it is only changed when it differs.
    shown_title: String,
    /// The `/resume` picker, while open.
    resume: Option<resume::Picker>,
    running: bool,
    cancel: CancellationToken,
    pending: Option<(ToolCall, Option<String>, tokio::sync::oneshot::Sender<bool>)>,
    /// Open question from the model and the user's typed answer.
    question: Option<(Question, String)>,
    /// `/` autocomplete: everything available (refreshed off the UI thread when the popup opens).
    catalog: Vec<SlashEntry>,
    /// Project files for `@` (loaded when the list opens).
    files: Vec<String>,
    models: Vec<String>,
    status: String,
    tokens: (u64, u64),
    /// Cached rendered height per item (0 = not measured yet); drives the virtualized list.
    heights: Vec<f32>,
    last_width: f32,
    /// Created lazily (it starts a background runtime): only when the window is unfocused.
    notifier: Option<fastframe_notifications::Client>,
    todos: Arc<std::sync::Mutex<Vec<magia_tools::Todo>>>,
}

const PROVIDERS: [&str; 3] = ["zen", "go", "openrouter"];
const MODES: [(PermissionMode, &str); 5] = [
    (PermissionMode::Ask, "ask"),
    (PermissionMode::AcceptEdits, "accept edits"),
    (PermissionMode::Auto, "auto (reviewed)"),
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
        theme::install(&cc.egui_ctx);
        // Follows the system appearance; MAGIA_THEME=light|dark overrides it.
        cc.egui_ctx
            .set_theme(match std::env::var("MAGIA_THEME").as_deref() {
                Ok("light") => egui::ThemePreference::Light,
                Ok("dark") => egui::ThemePreference::Dark,
                _ => egui::ThemePreference::System,
            });
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
            session_title: String::new(),
            shown_title: APP_NAME.to_owned(),
            running: false,
            cancel: CancellationToken::new(),
            pending: None,
            question: None,
            catalog: vec![],
            files: vec![],
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
        // ↑ works from the first frame: earlier messages of this folder's latest session.
        {
            let (tx, ctx, cwd) = (app.tx.clone(), cc.egui_ctx.clone(), app.cwd.clone());
            app.rt.spawn_blocking(move || {
                let latest = magia_store::SessionFile::latest_for(&sessions_dir(), &cwd);
                if let Some(h) = latest.and_then(|s| s.load().ok()) {
                    let _ = tx.send(UiMsg::History(sent_messages(&h)));
                    ctx.request_repaint();
                }
            });
        }
        // `magia-gui --resume`: reopen this folder's most recent session.
        if std::env::args().any(|a| a == "--resume")
            && let Some(latest) = magia_store::SessionFile::latest_for(&sessions_dir(), &app.cwd)
        {
            let path = latest.path().to_path_buf();
            let res = app
                .agent
                .try_lock()
                .map(|mut a| a.resume(&path).map(|_| a.history.clone()));
            match res {
                Ok(Ok(history)) => app.reset_items(&history),
                Ok(Err(e)) => app.status = format!("{e:#}"),
                Err(_) => {}
            }
        }
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
                        magia_core::config_path().display(),
                        magia_providers::default_key_env(&self.cfg.provider)
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
            let app = fastframe_notifications::App::new("dev.magia.app", "magia");
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

    /// List the project's files for `@` on a blocking thread (walks the tree, honours .gitignore).
    fn refresh_files(&self, ctx: &egui::Context) {
        let (cwd, tx, ctx) = (self.cwd.clone(), self.tx.clone(), ctx.clone());
        self.rt.spawn_blocking(move || {
            let _ = tx.send(UiMsg::Files(magia_core::project_files(&cwd, 20_000)));
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
                            let t = magia_core::media::thumbnail(&i, composer::THUMB);
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
                            magia_store::Scope::All
                        } else {
                            magia_store::Scope::Folder(&cwd)
                        };
                        let hits = magia_store::search(&sessions_dir(), scope, &query);
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
        // Resume, rewind and /clear all change which session (or how much of it) is open.
        self.session_title = sent_messages(history)
            .first()
            .map(|t| magia_store::title_of(t))
            .unwrap_or_default();
        self.items = items_from_history(history, &self.cwd);
        self.heights.clear();
        self.composer.load_history(sent_messages(history));
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
        self.composer.remember(&prompt);
        let typed = prompt.clone();
        if prompt.starts_with('/') {
            if self.slash(&prompt, ctx) {
                self.composer.input.clear();
                return;
            }
            if let Some(expanded) =
                magia_core::expand_command(&self.cwd, &prompt, &self.cfg.commands)
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
        if self.session_title.is_empty() {
            self.session_title = magia_store::title_of(&typed);
        }
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
                    magia_core::UserInput {
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
                UiMsg::Files(f) => self.files = f,
                UiMsg::History(h) => self.composer.load_history(h),
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
                    self.notify(ctx, "magia has a question", &body);
                }
                UiMsg::Permission(call, reason, reply) => {
                    let body = format!("run `{}`?", call.name);
                    self.pending = Some((call, reason, reply));
                    self.notify(ctx, "magia needs your approval", &body);
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
                        summary: relative(&tool_summary(&name, &input), &self.cwd),
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
                        self.notify(ctx, "magia", "finished");
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
        let title = window_title(&self.session_title);
        if title != self.shown_title {
            ctx.send_viewport_cmd(egui::ViewportCommand::Title(title.clone()));
            self.shown_title = title;
        }
        if self.running
            && self.pending.is_none()
            && self.question.is_none()
            && !self.composer.input.starts_with('/')
            && ctx.input(|i| i.key_pressed(egui::Key::Escape))
        {
            self.cancel.cancel();
        }

        // Before the panels: the picker takes ↑↓/Enter/Esc ahead of the composer.
        self.resume_picker(&ctx);
        self.top_bar(ui, &ctx);
        self.composer_panel(ui, &ctx);
        self.tasks_panel(ui);
        self.transcript(ui);
        self.question_modal(&ctx);
        self.permission_modal(&ctx);
    }
}

impl App {
    fn top_bar(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let t = *theme::tokens(ui);
        egui::Panel::top("top")
            .frame(
                egui::Frame::new()
                    .fill(t.bg)
                    .inner_margin(egui::Margin::symmetric(14, 8))
                    .stroke(egui::Stroke::new(1.0, t.line)),
            )
            .show(ui, |ui| {
                // One height for every control in the bar (combos, the model field, buttons).
                ui.spacing_mut().interact_size.y = BAR_H;
                ui.horizontal(|ui| {
                    ui.label(
                        egui::RichText::new(APP_NAME)
                            .font(theme::semibold(15.0))
                            .color(t.fg),
                    );
                    let folder = self
                        .cwd
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_else(|| self.cwd.display().to_string());
                    ui.label(
                        egui::RichText::new(folder)
                            .font(theme::mono(theme::SMALL))
                            .color(t.muted),
                    )
                    .on_hover_text(self.cwd.display().to_string());
                    ui.add_space(12.0);

                    ui.add_enabled_ui(!self.running, |ui| {
                        let mut changed = false;
                        egui::ComboBox::from_id_salt("provider")
                            .selected_text(self.cfg.provider.clone())
                            .width(110.0)
                            .show_ui(ui, |ui| {
                                for p in PROVIDERS {
                                    changed |= ui
                                        .selectable_value(&mut self.cfg.provider, p.to_owned(), p)
                                        .changed();
                                }
                            })
                            .response
                            .on_hover_text("Provider");
                        let r = ui.add(
                            egui::TextEdit::singleline(&mut self.cfg.model)
                                .font(theme::mono(theme::MONO))
                                .hint_text("model id")
                                .vertical_align(egui::Align::Center)
                                .min_size(egui::vec2(200.0, BAR_H))
                                .desired_width(200.0),
                        );
                        changed |= r.lost_focus() && r.changed();
                        let models = egui::Button::new("▾").min_size(egui::vec2(BAR_H, BAR_H));
                        egui::containers::menu::MenuButton::from_button(models)
                            .ui(ui, |ui| {
                                ui.set_min_width(260.0);
                                if self.models.is_empty() {
                                    ui.weak("No model list yet: check the provider key");
                                }
                                egui::ScrollArea::vertical()
                                    .max_height(360.0)
                                    .show(ui, |ui| {
                                        for m in self.models.clone() {
                                            let label = egui::RichText::new(&m)
                                                .font(theme::mono(theme::MONO));
                                            if ui
                                                .selectable_label(self.cfg.model == m, label)
                                                .clicked()
                                            {
                                                self.cfg.model = m;
                                                changed = true;
                                                ui.close();
                                            }
                                        }
                                    });
                            })
                            .0
                            .on_hover_text("Models offered by the provider");
                        let (mode_name, mode_color) = mode_style(self.cfg.permission_mode, &t);
                        egui::ComboBox::from_id_salt("mode")
                            .selected_text(
                                egui::RichText::new(mode_name)
                                    .font(theme::semibold(theme::SMALL + 1.0))
                                    .color(mode_color),
                            )
                            .width(130.0)
                            .show_ui(ui, |ui| {
                                for (m, _) in MODES {
                                    let (name, color) = mode_style(m, &t);
                                    let text = egui::RichText::new(name).color(color);
                                    changed |= ui
                                        .selectable_value(&mut self.cfg.permission_mode, m, text)
                                        .on_hover_text(mode_help(m))
                                        .changed();
                                }
                            })
                            .response
                            .on_hover_text(mode_help(self.cfg.permission_mode));
                        if changed {
                            self.apply_settings(ctx);
                        }
                    });

                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if self.tokens != (0, 0) {
                            ui.label(
                                egui::RichText::new(format!(
                                    "{} in · {} out",
                                    theme::count(self.tokens.0),
                                    theme::count(self.tokens.1)
                                ))
                                .font(theme::mono(theme::SMALL))
                                .color(t.muted),
                            )
                            .on_hover_text("Tokens used in this window");
                        }
                    });
                });
            });
    }

    fn composer_panel(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let t = *theme::tokens(ui);
        egui::Panel::bottom("composer")
            .frame(
                egui::Frame::new()
                    .fill(t.bg)
                    .inner_margin(egui::Margin::symmetric(14, 10)),
            )
            .show(ui, |ui| {
                column(ui, |ui| {
                    if !self.status.is_empty() {
                        ui.horizontal_wrapped(|ui| {
                            ui.label(
                                egui::RichText::new("●")
                                    .color(t.warn)
                                    .font(egui::FontId::proportional(9.0)),
                            );
                            ui.label(
                                egui::RichText::new(&self.status)
                                    .color(t.fg)
                                    .size(theme::SMALL + 0.5),
                            );
                        });
                    }
                    if self.running {
                        ui.horizontal(|ui| {
                            ui.label(
                                egui::RichText::new(if self.pending.is_some() {
                                    "waiting for your approval"
                                } else if self.question.is_some() {
                                    "waiting for your answer"
                                } else {
                                    "working…"
                                })
                                .color(t.accent)
                                .size(theme::SMALL + 0.5),
                            );
                        });
                    }
                    // Enter sends, Shift+Enter inserts a newline. The plain-Enter event is removed
                    // before the TextEdit sees it (egui's consume_key would also swallow
                    // Shift+Enter, since it ignores extra Shift).
                    let out = self.composer.show(ui, &self.catalog, &self.files);
                    if out.files_opened {
                        self.refresh_files(ctx);
                    }
                    if out.opened {
                        self.refresh_catalog(ctx); // pick up commands/skills added since last time
                    }
                    if out.paste_image {
                        self.attach_images(ctx, vec![]);
                    }
                    if !out.files.is_empty() {
                        self.attach_images(ctx, out.files);
                    }
                    if out.send {
                        self.send(ctx);
                    }
                    ui.horizontal(|ui| {
                        for (key, what) in [("/", "commands"), ("@", "files"), ("Ctrl+V", "image")]
                        {
                            ui.label(
                                egui::RichText::new(key)
                                    .font(theme::mono(theme::SMALL))
                                    .color(t.fg),
                            );
                            ui.label(egui::RichText::new(what).size(theme::SMALL).color(t.muted));
                            ui.add_space(6.0);
                        }
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if self.running {
                                if theme::secondary(ui, "Stop  Esc", Some(t.danger)).clicked() {
                                    self.cancel.cancel();
                                }
                            } else {
                                let ready = !self.composer.input.trim().is_empty()
                                    || !self.composer.attachments.is_empty();
                                if theme::primary(ui, "Send", ready).clicked() {
                                    self.send(ctx);
                                }
                            }
                        });
                    });
                });
            });
    }

    fn tasks_on_side(&self, ctx: &egui::Context) -> bool {
        ctx.content_rect().width() >= TASKS_SIDE_MIN_WIDTH
    }

    /// Wide windows: the task list lives in a resizable panel at the right of the transcript.
    fn tasks_panel(&mut self, ui: &mut egui::Ui) {
        if !self.tasks_on_side(ui.ctx()) {
            return;
        }
        let todos = self.todos.lock().map(|t| t.clone()).unwrap_or_default();
        if todos.is_empty() {
            return;
        }
        let t = *theme::tokens(ui);
        egui::Panel::right("tasks")
            .resizable(true)
            .default_size(300.0)
            .size_range(220.0..=480.0)
            .frame(
                egui::Frame::new()
                    .fill(t.bg)
                    .inner_margin(egui::Margin::symmetric(14, 12))
                    .stroke(egui::Stroke::new(1.0, t.line)),
            )
            .show(ui, |ui| {
                ui.label(
                    egui::RichText::new(tasks_title(&todos))
                        .font(theme::semibold(theme::SMALL + 1.0))
                        .color(t.fg),
                );
                ui.add_space(6.0);
                egui::ScrollArea::vertical()
                    .id_salt("tasks_side")
                    .auto_shrink(false)
                    .show(ui, |ui| todo_list(ui, &todos));
            });
    }

    fn transcript(&mut self, ui: &mut egui::Ui) {
        let t = *theme::tokens(ui);
        egui::CentralPanel::default()
            .frame(
                egui::Frame::new()
                    .fill(t.bg)
                    .inner_margin(egui::Margin::symmetric(14, 0)),
            )
            .show(ui, |ui| {
                if !self.tasks_on_side(ui.ctx()) {
                    let todos = self.todos.lock().map(|t| t.clone()).unwrap_or_default();
                    if !todos.is_empty() {
                        column(ui, |ui| todo_card(ui, &todos));
                    }
                }
                if self.items.is_empty() {
                    if let Some(cmd) = column(ui, |ui| empty_state(ui, &self.cwd)) {
                        let ctx = ui.ctx().clone();
                        self.slash(cmd, &ctx);
                    }
                    return;
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
                        ui.add_space(12.0);
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
                                let prev = n.checked_sub(1).map(|p| &self.items[p]);
                                column(ui, |ui| render_item(ui, n, it, prev));
                                self.heights[n] = (ui.cursor().min.y - start).max(1.0);
                            } else {
                                skipped += h + spacing;
                            }
                            y += self.heights[n].max(if known { 0.0 } else { h }) + spacing;
                        }
                        if skipped > 0.0 {
                            ui.add_space(skipped);
                        }
                        ui.add_space(12.0);
                    });
            });
    }

    fn question_modal(&mut self, ctx: &egui::Context) {
        let Some((q, typed)) = &mut self.question else {
            return;
        };
        let mut answer: Option<Option<String>> = None;
        egui::Modal::new(egui::Id::new("question")).show(ctx, |ui| {
            let t = *theme::tokens(ui);
            ui.set_width(460.0);
            theme::role(ui, "THE AGENT ASKS", t.accent);
            ui.add_space(2.0);
            ui.add(egui::Label::new(egui::RichText::new(&q.text).size(theme::BODY + 1.0)).wrap());
            ui.add_space(6.0);
            for o in &q.options {
                let b = egui::Button::new(o).min_size(egui::vec2(ui.available_width(), 30.0));
                if ui.add(b).clicked() {
                    answer = Some(Some(o.clone()));
                }
            }
            let r = ui.add(
                egui::TextEdit::singleline(typed)
                    .hint_text("Or type your own answer")
                    .desired_width(f32::INFINITY),
            );
            let enter = r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let ready = !typed.trim().is_empty();
                    if (theme::primary(ui, "Answer", ready).clicked() || enter) && ready {
                        answer = Some(Some(typed.trim().to_owned()));
                    }
                    if theme::secondary(ui, "Skip", None).clicked() {
                        answer = Some(None);
                    }
                });
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

    fn permission_modal(&mut self, ctx: &egui::Context) {
        let Some((call, reason, _)) = &self.pending else {
            return;
        };
        let mut answer = None;
        egui::Modal::new(egui::Id::new("permission")).show(ctx, |ui| {
            let t = *theme::tokens(ui);
            ui.set_width(520.0);
            theme::role(ui, "PERMISSION NEEDED", t.warn);
            ui.add_space(2.0);
            let summary = tool_summary(&call.name, &call.arguments);
            ui.horizontal_wrapped(|ui| {
                ui.label(egui::RichText::new("Allow").size(theme::HEADING));
                ui.label(
                    egui::RichText::new(&call.name)
                        .font(theme::mono(theme::HEADING - 1.0))
                        .color(t.fg),
                );
                ui.label(egui::RichText::new("to run?").size(theme::HEADING));
            });
            if !summary.is_empty() {
                ui.label(
                    egui::RichText::new(&summary)
                        .font(theme::mono(theme::MONO))
                        .color(t.muted),
                );
            }
            if let Some(reason) = reason {
                ui.add_space(2.0);
                ui.label(
                    egui::RichText::new(format!("Auto mode: {reason}"))
                        .size(theme::SMALL)
                        .color(t.warn),
                );
            }
            ui.add_space(4.0);
            let args = serde_json::to_string_pretty(&call.arguments).unwrap_or_default();
            let args: String = args.chars().take(3000).collect();
            egui::ScrollArea::vertical()
                .max_height(260.0)
                .show(ui, |ui| {
                    markdown::code_block(ui, "arguments", &args, "perm")
                });
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.label(
                    egui::RichText::new("Change the mode in the top bar to stop being asked.")
                        .size(theme::SMALL)
                        .color(t.muted),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if theme::primary(ui, "Allow", true).clicked() {
                        answer = Some(true);
                    }
                    if theme::secondary(ui, "Deny", Some(t.danger)).clicked() {
                        answer = Some(false);
                    }
                });
            });
        });
        if let Some(a) = answer
            && let Some((_, _, reply)) = self.pending.take()
        {
            let _ = reply.send(a);
        }
    }
}

/// Lay `add` out in the centred reading column.
/// Centred on the visible area (not on the content, which a wide child could stretch).
fn column<R>(ui: &mut egui::Ui, add: impl FnOnce(&mut egui::Ui) -> R) -> R {
    let clip = ui.clip_rect().intersect(ui.max_rect());
    let full = clip.width().min(ui.available_width());
    let w = full.min(theme::COLUMN);
    let left = clip.left() + (full - w) / 2.0;
    let rect = egui::Rect::from_min_size(egui::pos2(left, ui.cursor().min.y), egui::vec2(w, 0.0));
    ui.scope_builder(
        egui::UiBuilder::new()
            .max_rect(rect.with_max_y(f32::INFINITY))
            .layout(egui::Layout::top_down(egui::Align::Min)),
        add,
    )
    .inner
}

const APP_NAME: &str = "Magia";

/// `Magia - <session>`, or just `Magia` before the first message.
fn window_title(session: &str) -> String {
    const MAX: usize = 60;
    if session.is_empty() {
        return APP_NAME.to_owned();
    }
    let name = if session.chars().count() > MAX {
        format!(
            "{}…",
            session.chars().take(MAX).collect::<String>().trim_end()
        )
    } else {
        session.to_owned()
    };
    format!("{APP_NAME} - {name}")
}

/// Height of the controls in the top bar.
const BAR_H: f32 = 28.0;

fn mode_style(m: PermissionMode, t: &theme::Tokens) -> (&'static str, egui::Color32) {
    match m {
        PermissionMode::Ask => ("Ask first", t.fg),
        PermissionMode::AcceptEdits => ("Accept edits", t.warn),
        PermissionMode::Plan => ("Plan · read-only", t.accent),
        PermissionMode::Auto => ("Auto · reviewed", t.ok),
        PermissionMode::Yolo => ("Yolo · no prompts", t.danger),
    }
}

fn mode_help(m: PermissionMode) -> &'static str {
    match m {
        PermissionMode::Ask => "Asks before every edit, command and web access",
        PermissionMode::AcceptEdits => {
            "Edits files without asking; still asks for commands and web access"
        }
        PermissionMode::Plan => "Reads and plans only: no edits or commands",
        PermissionMode::Auto => {
            "Runs routine work on its own; a reviewer checks the rest and asks you when something looks risky"
        }
        PermissionMode::Yolo => "Runs everything without asking",
    }
}

/// First screen of a session: where we are and how to start.
fn empty_state(ui: &mut egui::Ui, cwd: &std::path::Path) -> Option<&'static str> {
    let t = *theme::tokens(ui);
    let mut cmd = None;
    ui.add_space((ui.available_height() * 0.18).clamp(16.0, 120.0));
    ui.label(
        egui::RichText::new("What are we working on?")
            .font(theme::semibold(22.0))
            .color(t.fg),
    );
    ui.label(
        egui::RichText::new(cwd.display().to_string())
            .font(theme::mono(theme::MONO))
            .color(t.muted),
    );
    ui.add_space(18.0);
    egui::Grid::new("hints")
        .num_columns(2)
        .spacing([18.0, 10.0])
        .show(ui, |ui| {
            for (key, what) in [
                ("/", "Commands and skills, filtered as you type"),
                (
                    "@path",
                    "Add a file to the message, or an image such as @shot.png",
                ),
                (
                    "Ctrl+V",
                    "Paste a screenshot; you can also drop image files here",
                ),
                ("Shift+Enter", "New line (Enter sends)"),
            ] {
                ui.label(
                    egui::RichText::new(key)
                        .font(theme::mono(theme::MONO))
                        .color(t.accent),
                );
                ui.label(egui::RichText::new(what).color(t.fg));
                ui.end_row();
            }
        });
    ui.add_space(18.0);
    ui.horizontal(|ui| {
        if theme::secondary(ui, "Resume a session…", None).clicked() {
            cmd = Some("/resume");
        }
        if theme::secondary(ui, "Commands", None).clicked() {
            cmd = Some("/help");
        }
    });
    cmd
}

/// Below this window width the task list is a short card above the transcript instead of a
/// side panel, so the reading column keeps its width.
const TASKS_SIDE_MIN_WIDTH: f32 = 1000.0;

fn tasks_title(todos: &[magia_tools::Todo]) -> String {
    let done = todos.iter().filter(|t| t.status == "completed").count();
    format!("Tasks  {done}/{}", todos.len())
}

fn todo_list(ui: &mut egui::Ui, todos: &[magia_tools::Todo]) {
    let t = *theme::tokens(ui);
    for todo in todos {
        let (mark, color, text_color) = match todo.status.as_str() {
            "completed" => ("✓", t.ok, t.muted),
            "in_progress" => ("▸", t.accent, t.fg),
            _ => ("○", t.muted, t.fg),
        };
        ui.horizontal_top(|ui| {
            ui.label(egui::RichText::new(mark).color(color));
            let mut text = egui::RichText::new(&todo.content)
                .color(text_color)
                .size(theme::SMALL + 0.5);
            if todo.status == "completed" {
                text = text.strikethrough();
            }
            ui.add(egui::Label::new(text).wrap());
        });
    }
}

/// Narrow windows: a collapsible card with a few lines visible; the rest scrolls.
fn todo_card(ui: &mut egui::Ui, todos: &[magia_tools::Todo]) {
    let t = *theme::tokens(ui);
    egui::Frame::new()
        .fill(t.surface)
        .stroke(egui::Stroke::new(1.0, t.line))
        .corner_radius(egui::CornerRadius::same(8))
        .inner_margin(egui::Margin::symmetric(12, 8))
        .outer_margin(egui::Margin::symmetric(0, 8))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            egui::CollapsingHeader::new(
                egui::RichText::new(tasks_title(todos)).font(theme::semibold(theme::SMALL + 1.0)),
            )
            .id_salt("todos")
            .default_open(true)
            .show(ui, |ui| {
                egui::ScrollArea::vertical()
                    .id_salt("todos_scroll")
                    .max_height(120.0)
                    .show(ui, |ui| todo_list(ui, todos));
            });
        });
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

/// One transcript entry. Only the user's messages sit on a card; replies are text on the page
/// and tool calls are one-line log entries that expand to their input and output.
fn render_item(ui: &mut egui::Ui, n: usize, it: &Item, prev: Option<&Item>) {
    let t = *theme::tokens(ui);
    match it {
        Item::User(text, images) => {
            ui.add_space(10.0);
            egui::Frame::new()
                .fill(t.surface)
                .stroke(egui::Stroke::new(1.0, t.line))
                .corner_radius(egui::CornerRadius::same(8))
                .inner_margin(egui::Margin::symmetric(14, 10))
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    theme::role(ui, "You", t.muted);
                    if !images.is_empty() {
                        ui.horizontal_wrapped(|ui| {
                            for l in images {
                                theme::chip(ui, &format!("Image · {l}"), t.accent);
                            }
                        });
                    }
                    if !text.is_empty() {
                        ui.add(
                            egui::Label::new(egui::RichText::new(text).size(theme::BODY)).wrap(),
                        );
                    }
                });
        }
        Item::Assistant { text, reasoning } => {
            // Consecutive steps of one reply share a single label.
            if !matches!(prev, Some(Item::Assistant { .. } | Item::Tool { .. })) {
                ui.add_space(12.0);
                theme::role(ui, "Assistant", t.accent);
            } else {
                ui.add_space(4.0);
            }
            if !reasoning.is_empty() {
                egui::CollapsingHeader::new(
                    egui::RichText::new("Thinking")
                        .size(theme::SMALL + 0.5)
                        .color(t.muted),
                )
                .id_salt(("reasoning", n))
                .show(ui, |ui| {
                    ui.add(
                        egui::Label::new(
                            egui::RichText::new(reasoning)
                                .size(theme::SMALL + 0.5)
                                .color(t.muted)
                                .italics(),
                        )
                        .wrap(),
                    );
                });
            }
            if !text.is_empty() {
                markdown::show(ui, text, n);
            }
        }
        Item::Tool {
            id,
            name,
            summary,
            input,
            output,
        } => {
            if !matches!(prev, Some(Item::Assistant { .. } | Item::Tool { .. })) {
                ui.add_space(12.0);
                theme::role(ui, "Assistant", t.accent);
            }
            let (mark, color) = match output {
                None => ("…", t.accent),
                Some((_, true)) => ("✗", t.danger),
                Some(_) => ("✓", t.ok),
            };
            let mut job = egui::text::LayoutJob::default();
            let fmt = |font: egui::FontId, color| egui::TextFormat {
                font_id: font,
                color,
                ..Default::default()
            };
            job.append(mark, 0.0, fmt(theme::mono(theme::MONO), color));
            job.append(name, 8.0, fmt(theme::mono(theme::MONO), t.fg));
            if !summary.is_empty() {
                job.append(summary, 10.0, fmt(theme::mono(theme::MONO), t.muted));
            }
            // One line, cut with an ellipsis at the column edge.
            job.wrap = egui::text::TextWrapping {
                max_width: ui.available_width() - 24.0,
                max_rows: 1,
                break_anywhere: true,
                overflow_character: Some('…'),
            };
            egui::CollapsingHeader::new(job)
                .id_salt(id)
                .icon(|_, _, _| {}) // the status mark leads the line instead of a triangle
                .show(ui, |ui| {
                    let pretty = serde_json::from_str::<serde_json::Value>(input)
                        .ok()
                        .and_then(|v| serde_json::to_string_pretty(&v).ok())
                        .unwrap_or_else(|| input.clone());
                    markdown::code_block(ui, "input", &pretty, (id, "in"));
                    if let Some((o, err)) = output {
                        let body: String = o.chars().take(4000).collect();
                        markdown::code_block(
                            ui,
                            if *err { "error" } else { "output" },
                            &body,
                            (id, "out"),
                        );
                    }
                });
        }
        Item::Error(e) => {
            ui.add_space(8.0);
            egui::Frame::new()
                .fill(theme::mix(t.bg, t.danger, 0.10))
                .stroke(egui::Stroke::new(1.0, theme::mix(t.line, t.danger, 0.5)))
                .corner_radius(egui::CornerRadius::same(8))
                .inner_margin(egui::Margin::symmetric(14, 10))
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    theme::role(ui, "The request failed", t.danger);
                    ui.add(
                        egui::Label::new(
                            egui::RichText::new(e)
                                .font(theme::mono(theme::MONO))
                                .color(t.fg),
                        )
                        .wrap(),
                    );
                });
        }
    }
}

/// What the user typed in a saved history, oldest first (no compaction summaries).
fn sent_messages(history: &[Message]) -> Vec<String> {
    history
        .iter()
        .filter_map(|m| match m {
            Message::User { text, .. } if !text.starts_with("[Summary") => {
                // `@file` contents and hook output are appended after the typed text.
                let typed = text.split("\n\n<file path=").next().unwrap_or(text);
                let typed = typed.split("\n\n[hook output]").next().unwrap_or(typed);
                Some(typed.trim().to_owned())
            }
            _ => None,
        })
        .collect()
}

/// Paths inside the project are shown relative to it.
fn relative(s: &str, cwd: &std::path::Path) -> String {
    let base = format!("{}/", cwd.display());
    s.replace(&base, "")
}

/// The argument that says what a tool call does: the command, the path, the query…
fn tool_summary(name: &str, args: &serde_json::Value) -> String {
    let get = |k: &str| args.get(k).and_then(|v| v.as_str()).map(str::to_owned);
    let s = match name {
        "bash" => get("command").map(|c| format!("$ {c}")),
        "read" | "write" | "edit" => get("file_path"),
        "ls" => get("path"),
        "glob" | "grep" => get("pattern").map(|p| match get("path") {
            Some(dir) => format!("{p}  in {dir}"),
            None => p,
        }),
        "webfetch" => get("url"),
        "websearch" => get("query"),
        "task" => get("description").or_else(|| get("prompt")),
        "todo" => Some("update the task list".into()),
        _ => None,
    };
    s.or_else(|| {
        args.as_object()?
            .values()
            .find_map(|v| v.as_str().map(str::to_owned))
    })
    .map(|s| s.split_whitespace().collect::<Vec<_>>().join(" "))
    .unwrap_or_default()
}

/// Rebuild the visible transcript from a saved history (resume / rewind / clear).
fn items_from_history(history: &[Message], cwd: &std::path::Path) -> Vec<Item> {
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
                        summary: relative(&tool_summary(&c.name, &c.arguments), cwd),
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

/// The app icon (packaging/magia.png), for the title bar and the taskbar.
fn window_icon() -> egui::IconData {
    const PNG: &[u8] = include_bytes!("../../../packaging/magia.png");
    match image::load_from_memory_with_format(PNG, image::ImageFormat::Png) {
        Ok(img) => {
            let img = img.into_rgba8();
            egui::IconData {
                width: img.width(),
                height: img.height(),
                rgba: img.into_raw(),
            }
        }
        Err(_) => egui::IconData::default(),
    }
}

fn main() {
    let opts = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([980.0, 760.0])
            .with_min_inner_size([520.0, 420.0])
            .with_title(APP_NAME)
            // Matches magia.desktop (Wayland picks the icon from it) and the AppImage.
            .with_app_id("magia")
            .with_icon(window_icon()),
        ..Default::default()
    };
    let result = fastframe_graphics::Graphics::new("magia").run_native(
        move || opts.clone(),
        || Box::new(|cc| Ok(Box::new(App::new(cc)))),
    );
    if let Err(e) = result {
        eprintln!("magia: {e}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn window_title_names_the_session() {
        assert_eq!(window_title(""), "Magia");
        assert_eq!(window_title("fix the parser"), "Magia - fix the parser");
        let long = "a".repeat(100);
        let t = window_title(&long);
        assert!(
            t.ends_with('…') && t.chars().count() == "Magia - ".len() + 61,
            "{t}"
        );
    }

    #[test]
    fn sent_messages_are_what_the_user_typed() {
        let h = vec![
            Message::user("[Summary of the earlier conversation]\nlots"),
            Message::user("look at @a.txt\n\n<file path=\"a.txt\">\nbody\n</file>"),
            Message::Assistant {
                text: "ok".into(),
                tool_calls: vec![],
            },
            Message::user("next\n\n[hook output]\nx"),
        ];
        assert_eq!(sent_messages(&h), ["look at @a.txt", "next"]);
    }
}
