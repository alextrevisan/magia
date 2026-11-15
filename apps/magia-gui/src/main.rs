//! Desktop front-end (egui/eframe, OpenGL only). All I/O runs on a small tokio runtime;
//! `update()` only drains channels and draws.

// No console window next to the app on Windows (release builds; debug keeps it for logs).
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

use async_trait::async_trait;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use tokio_util::sync::CancellationToken;
mod activity;
mod clipboard;
mod composer;
mod folder;
mod magic;
mod markdown;
mod model_picker;
#[cfg(target_os = "linux")]
mod portal;
mod resume;
mod screenshot;
mod settings;
mod sidebar;
mod theme;

use magia_core::agents::{self, FileAgent};
use magia_core::{Agent, AgentEvent, Approver, Config, SlashEntry, magia_mode, slash_catalog};
use magia_providers::Client;
use magia_store::{SessionFile, sessions_dir};
use magia_tools::PermissionMode;
use magia_types::{Image, Message, ToolCall};

enum UiMsg {
    Agent(AgentEvent),
    Models(Result<Vec<String>, String>),
    /// Another provider's model list, for the settings window.
    ProviderModels(String, Vec<String>),
    /// A call to approve.
    Permission(Pending),
    Question(Question),
    Catalog(Vec<SlashEntry>),
    /// Root `.md` files that can be picked as the agent.
    Agents(Vec<FileAgent>),
    /// A line for the status bar from a background task.
    Status(String),
    /// Project files for the `@` autocomplete.
    Files(Vec<String>),
    /// Images read from the clipboard or from files, with their RGBA previews.
    Attach(Result<Vec<(Image, Option<Thumb>)>, String>),
    /// Earlier messages for the composer's ↑/↓ history (from the folder's latest session).
    History(Vec<String>),
    /// Results of a `/resume` search: (generation, hits).
    Sessions(u64, Vec<magia_store::Hit>),
    /// Folders of earlier sessions, for the folder menu.
    Recent(Vec<PathBuf>),
    /// Every saved session, for the left bar.
    SessionList(Vec<magia_store::SessionInfo>),
    /// Reference folders of every project, for the left bar.
    References(Vec<(PathBuf, Vec<PathBuf>)>),
    /// The folder dialog's answer when picking a reference folder for (project).
    ReferencePicked(PathBuf, Result<Option<PathBuf>, String>),
    /// The folder dialog's answer (`None` = cancelled).
    Folder(Result<Option<PathBuf>, String>),
    /// A `/btw` answer, by question number: a piece of text, or the end (with any error).
    Btw(u64, BtwUpdate),
}

enum BtwUpdate {
    Text(String),
    Done(Option<String>),
}

/// The open `/btw` question and its answer (never part of the session).
struct Btw {
    n: u64,
    question: String,
    answer: String,
    done: bool,
    error: Option<String>,
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
    /// Magia mode started another round of the same request: "round N · why".
    Round(String),
    /// What compaction wrote in place of the older messages.
    Summary(String),
}

/// A call waiting for the user's yes or no.
struct Pending {
    call: ToolCall,
    /// Why `auto` mode flagged it.
    reason: Option<String>,
    /// Folders outside the project it would reach (then "always" is offered).
    dirs: Vec<PathBuf>,
    reply: tokio::sync::oneshot::Sender<magia_core::FolderAnswer>,
}

struct GuiApprover {
    tx: UnboundedSender<UiMsg>,
    ctx: egui::Context,
}

impl GuiApprover {
    async fn request(
        &self,
        call: &ToolCall,
        reason: Option<String>,
        dirs: Vec<PathBuf>,
    ) -> magia_core::FolderAnswer {
        let (reply, answer) = tokio::sync::oneshot::channel();
        let _ = self.tx.send(UiMsg::Permission(Pending {
            call: call.clone(),
            reason,
            dirs,
            reply,
        }));
        self.ctx.request_repaint();
        answer.await.unwrap_or(magia_core::FolderAnswer::Deny)
    }
}

#[async_trait]
impl Approver for GuiApprover {
    async fn approve(&self, call: &ToolCall) -> bool {
        self.request(call, None, vec![]).await != magia_core::FolderAnswer::Deny
    }

    async fn approve_because(&self, call: &ToolCall, reason: &str) -> bool {
        self.request(call, Some(reason.to_owned()), vec![]).await != magia_core::FolderAnswer::Deny
    }

    async fn approve_folder(&self, call: &ToolCall, dirs: &[PathBuf]) -> magia_core::FolderAnswer {
        self.request(call, None, dirs.to_vec()).await
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
    pending: Option<Pending>,
    /// Open question from the model and the user's typed answer.
    question: Option<(Question, String)>,
    /// `/` autocomplete: everything available (refreshed off the UI thread when the popup opens).
    catalog: Vec<SlashEntry>,
    /// Project files for `@` (loaded when the list opens).
    files: Vec<String>,
    models: Vec<String>,
    model_picker: model_picker::ModelPicker,
    settings: settings::Settings,
    /// Model lists by provider, for the settings window's role pickers.
    provider_models: std::collections::HashMap<String, Vec<String>>,
    /// Messages sent while the agent works; they go out together when the turn ends.
    queue: Vec<(String, Vec<magia_core::Image>)>,
    /// The status line shows a "trying again" notice.
    retrying: bool,
    /// Spins the Magia agent picker's border.
    magic: magic::Spin,
    /// Magia rounds/review and subagents of the current request (side panel).
    activity: activity::Activity,
    status: String,
    tokens: (u64, u64),
    /// Output speed of the last streamed response.
    last_rate: Option<f64>,
    /// Size of the context at the last response (its input + output); 0 = not known yet.
    context: u64,
    /// The model's context window (None = not reported) and the size at which the agent
    /// compacts, as of the last request.
    context_limit: (Option<u64>, u64),
    /// The conversation waiting for "Delete?" to be confirmed: (session file, title).
    delete_confirm: Option<(PathBuf, String)>,
    /// Cached rendered height per item (0 = not measured yet); drives the virtualized list.
    heights: Vec<f32>,
    last_width: f32,
    /// Item to bring into view: `(index, exact)`. First the scroll jumps to its estimated
    /// place (heights of unseen items are guesses), then, once it is drawn, exactly to it.
    scroll_to: Option<(usize, bool)>,
    /// Created lazily (it starts a background runtime): only when the window is unfocused.
    notifier: Option<fastframe_notifications::Client>,
    todos: Arc<std::sync::Mutex<Vec<magia_tools::Todo>>>,
    /// File agents of this folder (refreshed with the `/` catalog and when the picker opens).
    agents: Vec<FileAgent>,
    /// Mode to go back to when leaving the `plan` agent.
    mode_before_plan: PermissionMode,
    /// Folders of earlier sessions (the folder menu).
    recent: Vec<PathBuf>,
    /// The left bar: projects, their conversations and standalone chats.
    nav: sidebar::Sidebar,
    /// The open session's file, to highlight it in the left bar.
    current_session: Option<PathBuf>,
    /// The left bar's list needs reloading (a turn ended, a session opened…).
    nav_dirty: bool,
    /// The agent's request during a turn, for `/btw`.
    side: magia_core::btw::SideSlot,
    btw: Option<Btw>,
    btw_count: u64,
    /// A follow-up typed in the `/btw` window.
    btw_input: String,
}

/// What depends on the project folder: its config layers, agent files and a fresh agent.
struct Loaded {
    agent: Agent,
    cfg: Config,
    file_agents: Vec<FileAgent>,
    status: String,
    mode_before_plan: PermissionMode,
}

impl Loaded {
    /// Reads the folder's config and agent files: call before the first frame or off it.
    fn new(cwd: &std::path::Path) -> Self {
        let (cfg, status) = match Config::load_from(cwd) {
            Ok(c) => {
                let w = c.warnings.join("; ");
                (c, w)
            }
            Err(e) => (Config::default(), format!("config error: {e:#}")),
        };
        let client = Client::new(
            cfg.provider_config(&cfg.provider)
                .or_else(|_| cfg.provider_config("zen"))
                .expect("zen preset"),
        )
        .expect("http client");
        let mut agent = Agent::new(client, cfg.model.clone(), cfg.permission_mode, cwd.into());
        agent.configure(&cfg);
        if screenshot::available() {
            agent.tools.add(Box::new(screenshot::Screenshot));
        }
        agent.store = SessionFile::create_for(&sessions_dir(), cwd).ok();
        let mut status = status;
        let mut cfg = cfg;
        let file_agents = agents::discover_agents(cwd);
        match cfg.agent.as_str() {
            agents::BUILD => {}
            agents::PLAN => agent.mode = PermissionMode::Plan,
            agents::MAGIA => agent.magia = Some(cfg.magia_mode),
            name => match file_agents.iter().find(|a| a.name == name) {
                Some(fa) => match agents::load_agent(fa) {
                    Ok(text) => agent.persona = Some((fa.name.clone(), text)),
                    Err(e) => status = format!("{e:#}"),
                },
                // Last used in another folder: this one has no such file.
                None => cfg.agent = agents::BUILD.into(),
            },
        }
        let mode_before_plan = match cfg.permission_mode {
            PermissionMode::Plan => PermissionMode::Ask,
            m => m,
        };
        if cfg.agent == agents::PLAN {
            cfg.permission_mode = PermissionMode::Plan;
        }
        Self {
            agent,
            cfg,
            file_agents,
            status,
            mode_before_plan,
        }
    }
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
        // `magia-gui <folder>` opens that folder.
        if let Some(dir) = std::env::args().skip(1).find(|a| !a.starts_with("--"))
            && let Err(e) = std::env::set_current_dir(&dir)
        {
            eprintln!("magia-gui: {dir}: {e}");
        }
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let Loaded {
            agent,
            cfg,
            file_agents,
            status,
            mode_before_plan,
        } = Loaded::new(&cwd);
        // The theme picked in the settings (system by default); MAGIA_THEME=light|dark wins.
        cc.egui_ctx.set_theme(settings::preference(
            &std::env::var("MAGIA_THEME").unwrap_or_else(|_| cfg.theme.clone()),
        ));
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
            model_picker: Default::default(),
            settings: Default::default(),
            provider_models: std::collections::HashMap::new(),
            queue: vec![],
            retrying: false,
            magic: Default::default(),
            activity: Default::default(),
            status,
            tokens: (0, 0),
            last_rate: None,
            context: 0,
            context_limit: (None, 0),
            delete_confirm: None,
            heights: vec![],
            last_width: 0.0,
            scroll_to: None,
            notifier: None,
            todos,
            agents: file_agents,
            mode_before_plan,
            recent: vec![],
            nav: Default::default(),
            current_session: None,
            nav_dirty: false,
            side: Default::default(),
            btw: None,
            btw_count: 0,
            btw_input: String::new(),
        };
        app.side = app
            .agent
            .try_lock()
            .map(|a| a.side.clone())
            .unwrap_or_default();
        app.folder_opened(&cc.egui_ctx);
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

    /// Background work after a folder is (re)opened: models, MCP servers, `/` catalog, the
    /// composer history of the folder's latest session and the recent-folders menu.
    fn folder_opened(&mut self, ctx: &egui::Context) {
        self.refresh_models(ctx);
        if self.cfg.agent == agents::MAGIA {
            self.magic.start(ctx);
        }
        self.start_mcp(ctx);
        self.refresh_catalog(ctx);
        self.refresh_sessions(ctx);
        let (tx, ctx, cwd) = (self.tx.clone(), ctx.clone(), self.cwd.clone());
        self.rt.spawn_blocking(move || {
            // ↑ works from the first frame: earlier messages of this folder's latest session.
            let latest = magia_store::SessionFile::latest_for(&sessions_dir(), &cwd);
            if let Some(h) = latest.and_then(|s| s.load().ok()) {
                let _ = tx.send(UiMsg::History(sent_messages(&h)));
            }
            let _ = tx.send(UiMsg::Recent(folder::recent(&sessions_dir(), &cwd)));
            ctx.request_repaint();
        });
    }

    /// Continue the saved session at `path`, switching to its folder first when it belongs to
    /// another one. `false` (with the reason in the status line) when it cannot be opened.
    fn resume_path(&mut self, path: &std::path::Path, ctx: &egui::Context) -> bool {
        // The sidecar is a few hundred bytes: read here, like the picker's own selection.
        if let Some(dir) = SessionFile::open(path).meta().cwd
            && dir.is_dir()
            && dir != magia_store::canonical(&self.cwd)
        {
            self.open_folder(dir, ctx);
        }
        let Ok(mut a) = self.agent.try_lock() else {
            self.status = "busy — wait for the current turn to finish".into();
            return false;
        };
        match a.resume(path) {
            Ok(n) => {
                let history = a.history.clone();
                drop(a);
                self.reset_items(&history);
                self.status = format!("resumed ({n} messages)");
                ctx.memory_mut(|m| m.request_focus(composer::Composer::id()));
                true
            }
            Err(e) => {
                self.status = format!("{e:#}");
                false
            }
        }
    }

    /// Reload the left bar's list (reads the session sidecars, off the UI thread) and note
    /// which session is open.
    fn refresh_sessions(&mut self, ctx: &egui::Context) {
        if let Ok(a) = self.agent.try_lock() {
            self.current_session = a.store.as_ref().map(|s| s.path().to_path_buf());
        }
        let (tx, ctx) = (self.tx.clone(), ctx.clone());
        self.rt.spawn_blocking(move || {
            let list = magia_store::list(&sessions_dir(), magia_store::Scope::All);
            let _ = tx.send(UiMsg::SessionList(list));
            let _ = tx.send(UiMsg::References(magia_core::projects::all_references()));
            ctx.request_repaint();
        });
    }

    /// The left bar, when open. Switching is refused while a request runs.
    fn nav_panel(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        if !self.cfg.sidebar {
            return;
        }
        let t = *theme::tokens(ui);
        let root = folder::chats_root();
        let act = egui::Panel::left("nav")
            .resizable(true)
            .default_size(250.0)
            .size_range(190.0..=420.0)
            .frame(
                egui::Frame::new()
                    .fill(t.bg)
                    .inner_margin(egui::Margin::symmetric(10, 10))
                    .stroke(egui::Stroke::new(1.0, t.line)),
            )
            .show(ui, |ui| {
                self.nav
                    .show(ui, &self.cwd, self.current_session.as_deref(), &root)
            })
            .inner;
        let Some(act) = act else { return };
        if self.running && !matches!(act, sidebar::Action::Search) {
            self.status = "busy — stop or wait for the current request first".into();
            return;
        }
        match act {
            sidebar::Action::NewChat => match folder::new_chat_dir() {
                Ok(dir) => self.open_folder(dir, ctx),
                Err(e) => self.status = format!("{e:#}"),
            },
            sidebar::Action::NewIn(dir) => {
                if magia_store::canonical(&dir) == magia_store::canonical(&self.cwd) {
                    self.slash("/clear", ctx);
                } else {
                    self.open_folder(dir, ctx);
                }
            }
            sidebar::Action::Open { path } => {
                self.resume_path(&path, ctx);
            }
            sidebar::Action::Delete { path, title } => self.delete_confirm = Some((path, title)),
            sidebar::Action::AddProject => self.pick_folder(ctx),
            sidebar::Action::AddReference(project) => {
                let (tx, ctx, start) = (self.tx.clone(), ctx.clone(), project.clone());
                self.rt.spawn(async move {
                    let r = folder::pick(&start).await.map_err(|e| format!("{e:#}"));
                    let _ = tx.send(UiMsg::ReferencePicked(project, r));
                    ctx.request_repaint();
                });
            }
            sidebar::Action::RemoveReference(project, dir) => {
                let mut refs = self.nav.references_of(&project);
                refs.retain(|r| *r != dir);
                self.save_references(project, refs);
                self.status = format!("stopped using {} as a reference", dir.display());
            }
            sidebar::Action::Search => {
                let current = self.current_session.clone();
                self.resume = Some(resume::Picker::new(&self.cwd, "", current));
            }
        }
    }

    /// Add `dir` to `project`'s reference folders (unless it is the project or inside it).
    fn add_reference(&mut self, project: PathBuf, refs: &mut Vec<PathBuf>, dir: PathBuf) {
        let dir = magia_store::canonical(&dir);
        let proj = magia_store::canonical(&project);
        if dir.starts_with(&proj) {
            self.status = "that folder is already part of the project".into();
            return;
        }
        if refs.contains(&dir) {
            self.status = format!("{} is already a reference folder", dir.display());
            return;
        }
        refs.push(dir.clone());
        self.save_references(project, refs.clone());
        self.status = format!(
            "{} is a reference folder of {}: read freely, writing there asks",
            tilde(&dir).display(),
            sidebar::name_of(&proj)
        );
    }

    /// Save a project's reference folders (off the UI thread) and give them to the open
    /// agent when it is that project. Not while a request runs: the agent is busy.
    fn save_references(&mut self, project: PathBuf, refs: Vec<PathBuf>) {
        if magia_store::canonical(&project) == magia_store::canonical(&self.cwd) {
            match self.agent.try_lock() {
                Ok(mut a) => a.reference_dirs = refs.clone(),
                Err(_) => {
                    self.status = "busy — change reference folders when the request ends".into();
                    return;
                }
            }
        }
        self.nav.set_project_references(&project, refs.clone());
        let tx = self.tx.clone();
        self.rt.spawn_blocking(move || {
            if let Err(e) = magia_core::projects::set_references(&project, &refs) {
                let _ = tx.send(UiMsg::Status(format!(
                    "could not save reference folders: {e:#}"
                )));
            }
        });
    }

    /// Show or hide the left bar (remembered for the next start).
    fn toggle_nav(&mut self) {
        self.cfg.sidebar = !self.cfg.sidebar;
        let cfg = self.cfg.clone();
        self.rt.spawn_blocking(move || {
            let _ = cfg.save_state();
        });
    }

    /// `/btw`: answer a side question from the conversation so far, while a turn may run.
    /// Nothing is added to the session and no tool runs.
    fn ask_btw(&mut self, question: String, ctx: &egui::Context) {
        if self.cfg.model.trim().is_empty() {
            self.status = "pick or type a model first".into();
            return;
        }
        let side = if self.running {
            self.side.lock().ok().and_then(|s| s.clone())
        } else {
            // Idle: what the next request would send (in memory, no I/O).
            self.agent
                .try_lock()
                .ok()
                .map(|a| Arc::new(magia_core::btw::SideContext::of(&a, true)))
        };
        let Some(side) = side else {
            self.status = "busy — ask again in a moment".into();
            return;
        };
        self.btw_count += 1;
        let n = self.btw_count;
        self.btw = Some(Btw {
            n,
            question: question.clone(),
            answer: String::new(),
            done: false,
            error: None,
        });
        let (tx, ctx) = (self.tx.clone(), ctx.clone());
        self.rt.spawn(async move {
            let r = magia_core::btw::answer(&side, &question, |piece| {
                let _ = tx.send(UiMsg::Btw(n, BtwUpdate::Text(piece.to_owned())));
                ctx.request_repaint();
            })
            .await;
            let _ = tx.send(UiMsg::Btw(
                n,
                BtwUpdate::Done(r.err().map(|e| format!("{e:#}"))),
            ));
            ctx.request_repaint();
        });
    }

    /// The `/btw` answer in a small window of its own, apart from the session. Closing it
    /// (or Esc) drops the answer; a question typed there replaces it.
    fn btw_window(&mut self, ctx: &egui::Context) {
        if self.btw.is_none() {
            return;
        }
        let id = egui::ViewportId::from_hash_of("btw");
        let builder = egui::ViewportBuilder::default()
            .with_title(format!("{APP_NAME} — btw"))
            .with_inner_size([480.0, 360.0])
            .with_min_inner_size([320.0, 200.0]);
        let mut ask = None;
        let mut close = false;
        ctx.show_viewport_immediate(id, builder, |ctx, class| {
            let mut body = |ui: &mut egui::Ui, this: &mut Self| {
                let Some(b) = &this.btw else { return };
                let t = *theme::tokens(ui);
                ui.horizontal_wrapped(|ui| {
                    ui.label(
                        egui::RichText::new("btw")
                            .font(theme::semibold(theme::SMALL))
                            .color(t.accent),
                    );
                    ui.label(
                        egui::RichText::new(&b.question)
                            .color(t.fg)
                            .size(theme::SMALL + 1.0),
                    );
                });
                ui.separator();
                let input_h = 40.0;
                egui::ScrollArea::vertical()
                    .id_salt("btw")
                    .max_height((ui.available_height() - input_h).max(60.0))
                    .auto_shrink([false, false])
                    .stick_to_bottom(true)
                    .show(ui, |ui| {
                        if !b.answer.is_empty() {
                            markdown::show(ui, &b.answer, usize::MAX - b.n as usize);
                        } else if !b.done {
                            ui.label(egui::RichText::new("thinking…").color(t.muted));
                        }
                        if let Some(e) = &b.error {
                            ui.label(egui::RichText::new(e).color(t.danger).size(theme::SMALL));
                        }
                    });
                ui.add_space(4.0);
                let edit = ui.add(
                    egui::TextEdit::singleline(&mut this.btw_input)
                        .hint_text("Ask another side question (Enter) · Esc closes · not saved")
                        .desired_width(f32::INFINITY),
                );
                if edit.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                    let q = std::mem::take(&mut this.btw_input);
                    if !q.trim().is_empty() {
                        ask = Some(q);
                    }
                    edit.request_focus();
                }
            };
            if class == egui::ViewportClass::EmbeddedWindow {
                // No separate windows on this platform: a floating window inside the main one.
                let mut open = true;
                egui::Window::new("btw")
                    .open(&mut open)
                    .default_size([460.0, 320.0])
                    .show(ctx, |ui| body(ui, self));
                close |= !open;
            } else {
                let t = theme::tokens_of(ctx);
                egui::CentralPanel::default()
                    .frame(
                        egui::Frame::new()
                            .fill(t.bg)
                            .inner_margin(egui::Margin::same(12)),
                    )
                    .show(ctx, |ui| body(ui, self));
                close |= ctx
                    .input(|i| i.viewport().close_requested() || i.key_pressed(egui::Key::Escape));
            }
        });
        if close {
            self.btw = None;
            self.btw_input.clear();
        } else if let Some(q) = ask {
            self.ask_btw(q, ctx);
        }
    }

    /// Show the system folder dialog; the answer comes back as `UiMsg::Folder`.
    fn pick_folder(&mut self, ctx: &egui::Context) {
        let (tx, ctx, start) = (self.tx.clone(), ctx.clone(), self.cwd.clone());
        self.rt.spawn(async move {
            let r = folder::pick(&start).await.map_err(|e| format!("{e:#}"));
            let _ = tx.send(UiMsg::Folder(r));
            ctx.request_repaint();
        });
    }

    /// Make `dir` the project: a new session there, with that folder's config and agents.
    fn open_folder(&mut self, dir: PathBuf, ctx: &egui::Context) {
        if self.running {
            self.status = "busy — stop or wait for the current request first".into();
            return;
        }
        if magia_store::canonical(&dir) == magia_store::canonical(&self.cwd) {
            return;
        }
        if let Err(e) = std::env::set_current_dir(&dir) {
            self.status = format!("{}: {e}", dir.display());
            return;
        }
        // A few small files (config layers, agent list): the same work the first frame waits for.
        let l = Loaded::new(&dir);
        self.todos = l.agent.cx.todos.clone();
        self.side = l.agent.side.clone();
        self.btw = None;
        self.agent = Arc::new(tokio::sync::Mutex::new(l.agent)); // the old one (and its MCP servers) goes
        self.cfg = l.cfg;
        self.agents = l.file_agents;
        self.mode_before_plan = l.mode_before_plan;
        self.cwd = dir;
        self.queue.clear();
        self.files.clear();
        self.composer = composer::Composer::default(); // the old folder's history goes too
        self.tokens = (0, 0);
        self.last_rate = None;
        self.reset_items(&[]);
        self.status = if l.status.is_empty() {
            format!("opened {}", self.cwd.display())
        } else {
            l.status
        };
        self.folder_opened(ctx);
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
                    a.magia = (self.cfg.agent == agents::MAGIA).then_some(self.cfg.magia_mode);
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

    /// The built-in providers plus any defined under `"provider"` in the config JSON.
    fn provider_list(&self) -> Vec<String> {
        let mut list: Vec<String> = PROVIDERS.iter().map(|p| p.to_string()).collect();
        for p in self.cfg.providers.keys() {
            if !list.contains(p) {
                list.push(p.clone());
            }
        }
        list
    }

    /// Open the settings window. A provider whose model list came back empty (offline, bad
    /// key) is asked again.
    fn open_settings(&mut self) {
        self.provider_models.retain(|_, m| !m.is_empty());
        self.settings.open(&self.cfg);
    }

    /// "Delete this conversation?"; on yes the files go off the UI thread. The open
    /// conversation is first replaced by a new one, so nothing writes to the deleted file.
    fn delete_dialog(&mut self, ctx: &egui::Context) {
        let Some((path, title)) = &self.delete_confirm else {
            return;
        };
        let mut yes = false;
        let mut close = false;
        let modal = egui::Modal::new(egui::Id::new("delete-session")).show(ctx, |ui| {
            let t = *theme::tokens(ui);
            ui.set_width(420.0);
            ui.label(egui::RichText::new("Delete this conversation?").size(theme::HEADING));
            ui.add_space(4.0);
            ui.label(egui::RichText::new(title).color(t.fg));
            ui.label(
                egui::RichText::new("It cannot be undone.")
                    .size(theme::SMALL)
                    .color(t.muted),
            );
            ui.add_space(12.0);
            ui.horizontal(|ui| {
                yes = theme::secondary(ui, "Delete", Some(t.danger)).clicked();
                close = theme::secondary(ui, "Cancel", None).clicked();
            });
        });
        if !yes {
            if close || modal.should_close() {
                self.delete_confirm = None;
            }
            return;
        }
        let path = path.clone();
        if self.running {
            self.status = "busy — stop or wait for the current request first".into();
            return;
        }
        if self.current_session.as_deref() == Some(path.as_path()) {
            let Ok(mut a) = self.agent.try_lock() else {
                self.status = "busy — try again in a moment".into();
                return;
            };
            let started = a.new_session();
            let history = a.history.clone();
            self.current_session = a.store.as_ref().map(|s| s.path().to_path_buf());
            drop(a);
            if let Err(e) = started {
                self.status = format!("{e:#}");
                return;
            }
            self.reset_items(&history);
        }
        self.delete_confirm = None;
        let keep = self.cwd.clone();
        let (tx, ctx) = (self.tx.clone(), ctx.clone());
        self.rt.spawn_blocking(move || {
            let sf = SessionFile::open(&path);
            let dir = sf.meta().cwd;
            let status = match sf.delete() {
                Ok(()) => {
                    // A standalone chat's scratch folder goes too, when nothing was left in it.
                    if let Some(d) = dir.filter(|d| {
                        d.starts_with(folder::chats_root())
                            && magia_store::canonical(d) != magia_store::canonical(&keep)
                    }) {
                        let _ = std::fs::remove_dir(d); // fails (and stays) when not empty
                    }
                    "conversation deleted".to_owned()
                }
                Err(e) => format!("could not delete {}: {e:#}", path.display()),
            };
            let _ = tx.send(UiMsg::Status(status));
            let list = magia_store::list(&sessions_dir(), magia_store::Scope::All);
            let _ = tx.send(UiMsg::SessionList(list));
            ctx.request_repaint();
        });
    }

    /// Draw the settings window and carry out what it asks.
    fn settings_window(&mut self, ctx: &egui::Context) {
        let providers = self.provider_list();
        let actions = self
            .settings
            .ui(ctx, &self.cfg, &providers, &self.provider_models);
        for a in actions {
            match a {
                settings::Action::Models(p) => self.fetch_provider_models(p, ctx),
                settings::Action::Save(d) => {
                    self.cfg.theme = d.theme.clone();
                    self.cfg.magia_mode.max_rounds = d.max_rounds;
                    self.cfg.magia_mode.review = d.review;
                    self.cfg.role_models = d.role_models(&self.cfg);
                    self.cfg.compact_percent = d.compact_percent;
                    self.cfg.magia_from_state = true;
                    ctx.set_theme(settings::preference(&d.theme));
                    // A running request holds the agent: the settings go in when it ends.
                    let (agent, cfg) = (self.agent.clone(), self.cfg.clone());
                    self.rt.spawn(async move {
                        let mut a = agent.lock().await;
                        a.set_roles(&cfg);
                        a.magia = (cfg.agent == agents::MAGIA).then_some(cfg.magia_mode);
                        a.compact_percent = cfg.compact_percent;
                    });
                    if self.running {
                        self.status = "settings saved; they apply when this request ends".into();
                    }
                    let _ = self.cfg.save_state();
                }
            }
        }
    }

    /// Fetch a provider's model list once (an empty list marks it as asked).
    fn fetch_provider_models(&mut self, provider: String, ctx: &egui::Context) {
        self.provider_models.insert(provider.clone(), Vec::new());
        let Ok(client) = self.cfg.provider_config(&provider).and_then(Client::new) else {
            return;
        };
        let declared = self.cfg.declared_models(&provider);
        let (tx, ctx) = (self.tx.clone(), ctx.clone());
        self.rt.spawn(async move {
            let mut ids: Vec<String> = client
                .list_models()
                .await
                .map(|m| m.into_iter().map(|m| m.id).collect())
                .unwrap_or_default();
            for d in declared {
                if !ids.contains(&d) {
                    ids.push(d);
                }
            }
            let _ = tx.send(UiMsg::ProviderModels(provider, ids));
            ctx.request_repaint();
        });
    }

    /// Switch the primary agent. `plan` also switches the mode; a file agent's text is read off
    /// the UI thread and handed to the agent.
    fn select_agent(&mut self, name: String, ctx: &egui::Context) {
        if name == agents::PLAN {
            if self.cfg.permission_mode != PermissionMode::Plan {
                self.mode_before_plan = self.cfg.permission_mode;
            }
            self.cfg.permission_mode = PermissionMode::Plan;
        } else if self.cfg.permission_mode == PermissionMode::Plan
            && (self.cfg.agent == agents::PLAN || name == agents::MAGIA)
        {
            // Magia has nothing to do in a mode where nothing can change.
            self.cfg.permission_mode = self.mode_before_plan;
        }
        let file = self.agents.iter().find(|a| a.name == name).cloned();
        if name == agents::MAGIA {
            self.magic.start(ctx);
        }
        self.cfg.agent = name;
        // First: it takes the agent lock with `try_lock`, which the task below would hold.
        self.apply_settings(ctx);
        let (agent, tx, ctx2) = (self.agent.clone(), self.tx.clone(), ctx.clone());
        self.rt.spawn(async move {
            let persona = match file {
                None => Ok(None),
                Some(fa) => tokio::task::spawn_blocking(move || {
                    agents::load_agent(&fa).map(|text| Some((fa.name, text)))
                })
                .await
                .unwrap_or_else(|e| Err(anyhow::anyhow!("{e}"))),
            };
            match persona {
                Ok(p) => agent.lock().await.persona = p,
                Err(e) => {
                    let _ = tx.send(UiMsg::Status(format!("{e:#}")));
                    ctx2.request_repaint();
                }
            }
        });
    }

    fn refresh_models(&mut self, ctx: &egui::Context) {
        let Ok(a) = self.agent.try_lock() else { return };
        let client = a.client.clone();
        drop(a);
        let (tx, ctx, agent) = (self.tx.clone(), ctx.clone(), self.agent.clone());
        let declared = self.cfg.declared_models(&self.cfg.provider);
        self.rt.spawn(async move {
            // Models declared in the JSON config are always offered, even if `/models` fails.
            let mut listed = Vec::new();
            let r = match client.list_models().await {
                Ok(m) => {
                    listed = m.clone();
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
            // The agent learns the windows from this list (waits for a running turn to end).
            if !listed.is_empty() {
                agent.lock().await.add_limits(&client.config.name, &listed);
            }
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
            let _ = tx.send(UiMsg::Agents(agents::discover_agents(&cwd)));
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
                    if self.running {
                        self.status = "busy — wait for the current turn to finish".into();
                        continue;
                    }
                    if self.resume_path(&path, ctx) {
                        self.resume = None;
                        ctx.memory_mut(|m| m.request_focus(composer::Composer::id()));
                        return;
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
        self.activity.clear();
        self.nav_dirty = true;
        self.context = 0;
        self.last_rate = None;
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
            "settings" => self.open_settings(),
            "compact" => {
                let (agent, tx, ctx) = (self.agent.clone(), self.tx.clone(), ctx.clone());
                self.running = true;
                self.rt.spawn(async move {
                    let mut agent = agent.lock().await;
                    let ev = match agent.compact().await {
                        Ok(Some((before, _))) => AgentEvent::Compacted {
                            before,
                            history: agent.history.clone(),
                        },
                        Ok(None) => AgentEvent::Notice("nothing to compact yet".into()),
                        Err(e) => AgentEvent::Notice(format!("compaction failed: {e:#}")),
                    };
                    drop(agent);
                    let _ = tx.send(UiMsg::Agent(ev));
                    let _ = tx.send(UiMsg::Agent(AgentEvent::TurnDone));
                    ctx.request_repaint();
                });
            }
            "reference" => {
                let rest = input.trim_start()["/reference".len()..].trim();
                let project = self.cwd.clone();
                let mut refs = self.nav.references_of(&project);
                if let Some(path) = rest.strip_prefix("add ") {
                    match folder::resolve(path, &self.cwd) {
                        Ok(dir) => self.add_reference(project, &mut refs, dir),
                        Err(e) => self.status = format!("{e:#}"),
                    }
                } else if let Some(path) = rest.strip_prefix("remove ") {
                    let target = folder::resolve(path, &self.cwd)
                        .unwrap_or_else(|_| PathBuf::from(path.trim()));
                    let before = refs.len();
                    refs.retain(|r| *r != target);
                    if refs.len() == before {
                        self.status = format!("{} is not a reference folder", target.display());
                    } else {
                        self.save_references(project, refs);
                        self.status = format!("stopped using {} as a reference", target.display());
                    }
                } else if rest == "add" || rest.is_empty() && refs.is_empty() {
                    let (tx, ctx2, start) = (self.tx.clone(), ctx.clone(), project.clone());
                    self.rt.spawn(async move {
                        let r = folder::pick(&start).await.map_err(|e| format!("{e:#}"));
                        let _ = tx.send(UiMsg::ReferencePicked(project, r));
                        ctx2.request_repaint();
                    });
                } else {
                    let list: Vec<String> = refs
                        .iter()
                        .map(|r| tilde(r).display().to_string())
                        .collect();
                    self.status = format!(
                        "reference folders: {} · /reference add <path> · /reference remove <path>",
                        list.join(", ")
                    );
                }
            }
            "permissions" => {
                let Ok(mut a) = self.agent.try_lock() else {
                    self.status = busy.into();
                    return true;
                };
                if arg == Some("reset") {
                    self.status = match a.reset_folder_permissions() {
                        Ok(()) => "folders outside the project will be asked about again".into(),
                        Err(e) => format!("{e:#}"),
                    };
                } else {
                    let (allowed, blocked) = a.folder_permissions();
                    let list = |v: &[PathBuf]| {
                        if v.is_empty() {
                            "none".to_owned()
                        } else {
                            v.iter()
                                .map(|d| tilde(d).display().to_string())
                                .collect::<Vec<_>>()
                                .join(", ")
                        }
                    };
                    self.status = format!(
                        "this session — allowed: {} · blocked: {} · /permissions reset forgets them",
                        list(&allowed),
                        list(&blocked)
                    );
                }
            }
            "btw" => {
                let question = input.trim_start()["/btw".len()..].trim();
                if question.is_empty() {
                    self.status =
                        "/btw <question>: ask something on the side, without stopping the work"
                            .into();
                } else {
                    self.ask_btw(question.to_owned(), ctx);
                }
            }
            "open" => {
                // The whole rest of the line: paths may have spaces.
                let typed = input.trim_start()["/open".len()..].trim();
                if typed.is_empty() {
                    self.pick_folder(ctx);
                } else {
                    match folder::resolve(typed, &self.cwd) {
                        Ok(dir) => self.open_folder(dir, ctx),
                        Err(e) => self.status = format!("{e:#}"),
                    }
                }
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
        if prompt.is_empty() && self.composer.attachments.is_empty() {
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
        let images = self.composer.take_images();
        if self.running {
            // Sent (with anything else queued) as one message when the turn ends.
            self.queue.push((prompt, images));
            return;
        }
        if self.session_title.is_empty() {
            self.session_title = magia_store::title_of(&typed);
        }
        self.start_turn(prompt, images, ctx);
    }

    /// Everything queued while the agent worked, as one message: texts in order, separated by
    /// a blank line, with all their images.
    fn take_queue(&mut self) -> Option<(String, Vec<magia_core::Image>)> {
        merge_queue(std::mem::take(&mut self.queue))
    }

    /// Stop the turn. Queued messages go back into the box instead of being sent.
    fn stop(&mut self, ctx: &egui::Context) {
        self.cancel.cancel();
        if let Some((text, images)) = self.take_queue() {
            let typed = std::mem::take(&mut self.composer.input);
            self.composer.input = [text, typed]
                .into_iter()
                .filter(|t| !t.trim().is_empty())
                .collect::<Vec<_>>()
                .join("\n\n");
            for img in images {
                self.composer.attach(ctx, img, None);
            }
        }
    }

    fn start_turn(&mut self, prompt: String, images: Vec<magia_core::Image>, ctx: &egui::Context) {
        self.items.push(Item::User(
            prompt.clone(),
            images.iter().map(composer::image_label).collect(),
        ));
        self.running = true;
        self.status.clear();
        self.activity.clear();
        if self.cfg.agent == agents::MAGIA && self.cfg.permission_mode != PermissionMode::Plan {
            self.magic.start(ctx);
        }
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
            // The repeat is answering: the "trying again" line is no longer news.
            if self.retrying
                && matches!(
                    msg,
                    UiMsg::Agent(AgentEvent::TextDelta(_) | AgentEvent::ToolStart { .. })
                )
            {
                self.retrying = false;
                self.status.clear();
            }
            match msg {
                UiMsg::Models(Ok(m)) => {
                    self.provider_models
                        .insert(self.cfg.provider.clone(), m.clone());
                    self.models = m;
                }
                UiMsg::ProviderModels(p, m) => {
                    self.provider_models.insert(p, m);
                }
                UiMsg::Models(Err(e)) => self.status = format!("models: {e}"),
                UiMsg::Catalog(c) => self.catalog = c,
                UiMsg::Agents(a) => self.agents = a,
                UiMsg::Status(s) => self.status = s,
                UiMsg::Files(f) => self.files = f,
                UiMsg::History(h) => self.composer.load_history(h),
                UiMsg::Recent(r) => self.recent = r,
                UiMsg::SessionList(list) => self.nav.set_sessions(list),
                UiMsg::References(r) => self.nav.set_references(r),
                UiMsg::ReferencePicked(project, Ok(Some(dir))) => {
                    let mut refs = self.nav.references_of(&project);
                    self.add_reference(project, &mut refs, dir);
                }
                UiMsg::ReferencePicked(_, Ok(None)) => {}
                UiMsg::ReferencePicked(_, Err(e)) => {
                    self.status = format!("{e}; type /reference add <path> instead");
                }
                UiMsg::Btw(n, update) => {
                    if let Some(b) = self.btw.as_mut().filter(|b| b.n == n) {
                        match update {
                            BtwUpdate::Text(piece) => b.answer.push_str(&piece),
                            BtwUpdate::Done(error) => {
                                b.done = true;
                                b.error = error;
                            }
                        }
                    }
                }
                UiMsg::Folder(Ok(Some(dir))) => self.open_folder(dir, ctx),
                UiMsg::Folder(Ok(None)) => {}
                UiMsg::Folder(Err(e)) => {
                    self.status = format!("{e}; type /open <path> instead");
                }
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
                UiMsg::Permission(p) => {
                    let body = if p.dirs.is_empty() {
                        format!("run `{}`?", p.call.name)
                    } else {
                        format!("`{}` wants to reach outside the project", p.call.name)
                    };
                    self.pending = Some(p);
                    self.notify(ctx, "magia needs your approval", &body);
                }
                UiMsg::Agent(ev) => {
                    let cwd = &self.cwd;
                    self.activity
                        .on_event(&ev, |n, i| relative(&tool_summary(n, i), cwd));
                    match ev {
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
                        AgentEvent::Usage {
                            input,
                            output,
                            tokens_per_sec,
                        } => {
                            self.tokens.0 += input;
                            self.tokens.1 += output;
                            self.context = input + output;
                            if let Some(r) = tokens_per_sec {
                                self.last_rate = Some(r);
                            }
                        }
                        AgentEvent::Context {
                            tokens,
                            window,
                            compact_at,
                        } => {
                            self.context = tokens;
                            self.context_limit = (window, compact_at);
                        }
                        AgentEvent::Notice(n) => self.status = n,
                        AgentEvent::Compacted { before, history } => {
                            // Show what the model sees now: the summary, then what was kept.
                            self.status =
                                format!("context compacted: {before} → {} messages", history.len());
                            self.items = items_from_history(&history, &self.cwd);
                            self.heights.clear();
                            // The context shrank; the next response reports its new size.
                            self.context = 0;
                        }
                        AgentEvent::Error(e) => self.items.push(Item::Error(e)),
                        AgentEvent::Magia(_)
                        | AgentEvent::SubTool { .. }
                        | AgentEvent::SubToolEnd { .. } => {}
                        AgentEvent::Round { n, reason } => {
                            self.items
                                .push(Item::Round(format!("round {n} · {reason}")));
                        }
                        AgentEvent::TurnDone => {
                            self.nav_dirty = true; // a new conversation or a new title
                            self.running = false;
                            self.pending = None;
                            self.question = None;
                            if let Some((text, images)) = self.take_queue() {
                                self.start_turn(text, images, ctx);
                            } else {
                                self.notify(ctx, "magia", "finished");
                            }
                        }
                        AgentEvent::Retry {
                            attempt,
                            max,
                            wait_secs,
                            error,
                            discarded,
                        } => {
                            if discarded
                                && matches!(self.items.last(), Some(Item::Assistant { .. }))
                            {
                                self.items.pop();
                                self.heights.truncate(self.items.len());
                            }
                            self.status = format!(
                                "request failed ({error}); trying again in {wait_secs}s ({attempt}/{max})"
                            );
                            self.retrying = true;
                        }
                    }
                }
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
            self.stop(&ctx);
        }

        // Before the panels: the picker takes ↑↓/Enter/Esc ahead of the composer.
        self.resume_picker(&ctx);
        self.settings_window(&ctx);
        self.delete_dialog(&ctx);
        self.btw_window(&ctx);
        if ctx.input_mut(|i| i.consume_key(egui::Modifiers::COMMAND, egui::Key::B)) {
            self.toggle_nav();
        }
        if std::mem::take(&mut self.nav_dirty) {
            self.refresh_sessions(&ctx);
        }
        // Before the top bar, so it runs the full height of the window.
        self.nav_panel(ui, &ctx);
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
                    let hint = if self.cfg.sidebar {
                        "Hide projects and chats (Ctrl+B)"
                    } else {
                        "Show projects and chats (Ctrl+B)"
                    };
                    if ui
                        .add(
                            egui::Button::new(egui::RichText::new("☰").size(16.0).color(t.muted))
                                .frame(false),
                        )
                        .on_hover_text(hint)
                        .clicked()
                    {
                        self.toggle_nav();
                    }
                    if ui
                        .add(
                            egui::Button::new(egui::RichText::new("⚙").size(16.0).color(t.muted))
                                .frame(false),
                        )
                        .on_hover_text("Settings: theme and Magia mode (/settings)")
                        .clicked()
                    {
                        self.open_settings();
                    }
                    ui.label(
                        egui::RichText::new(APP_NAME)
                            .font(theme::semibold(15.0))
                            .color(t.fg),
                    );
                    let folder = if self.cwd.starts_with(folder::chats_root()) {
                        "chat".to_owned() // a standalone chat's scratch folder
                    } else {
                        sidebar::name_of(&self.cwd)
                    };
                    if let Some(dir) = self.folder_menu(ui, folder) {
                        match dir {
                            Some(dir) => self.open_folder(dir, ctx),
                            None => self.pick_folder(ctx),
                        }
                    }
                    ui.add_space(12.0);

                    ui.add_enabled_ui(!self.running, |ui| {
                        let mut changed = false;
                        let providers = self.provider_list();
                        egui::ComboBox::from_id_salt("provider")
                            .selected_text(self.cfg.provider.clone())
                            .width(110.0)
                            .show_ui(ui, |ui| {
                                for p in &providers {
                                    changed |= ui.selectable_value(
                                        &mut self.cfg.provider,
                                        p.to_owned(),
                                        p.as_str(),
                                    )
                                    .changed();
                                }
                            })
                            .response
                            .on_hover_text("Provider: the built-ins plus any defined under \"provider\" in the config");
                        if let Some(m) =
                            self.model_picker
                                .show(ui, &self.cfg.model, &self.models, 230.0)
                            && m != self.cfg.model
                        {
                            self.cfg.model = m;
                            changed = true;
                        }
                        let mut picked = None;
                        let opened = egui::ComboBox::from_id_salt("agent")
                            .selected_text(
                                egui::RichText::new(agent_label(&self.cfg.agent))
                                    .font(theme::semibold(theme::SMALL + 1.0)),
                            )
                            .width(130.0)
                            .show_ui(ui, |ui| {
                                ui.set_min_width(220.0);
                                for (name, help) in [
                                    (agents::BUILD, "magia's own instructions"),
                                    (agents::PLAN, "Read-only: switches the mode to Plan"),
                                    (
                                        agents::MAGIA,
                                        "Keeps working until the task list is done, then has \
                                         the changes reviewed",
                                    ),
                                ] {
                                    if ui
                                        .selectable_label(self.cfg.agent == name, agent_label(name))
                                        .on_hover_text(help)
                                        .clicked()
                                    {
                                        picked = Some(name.to_owned());
                                    }
                                }
                                if !self.agents.is_empty() {
                                    ui.separator();
                                }
                                for a in &self.agents {
                                    let label =
                                        egui::RichText::new(&a.name).font(theme::mono(theme::MONO));
                                    if ui
                                        .selectable_label(self.cfg.agent == a.name, label)
                                        .on_hover_text(file_agent_help(a))
                                        .clicked()
                                    {
                                        picked = Some(a.name.clone());
                                    }
                                }
                            })
                            .response
                            .on_hover_text(
                                "Agent: Build, Plan, Magia, or a .md file from the project root \
                                 whose text is added to the instructions",
                            );
                        if self.cfg.agent == agents::MAGIA {
                            self.magic.paint(ui, opened.rect, &t.magic);
                        }
                        let opened = opened.clicked();
                        if opened {
                            self.refresh_catalog(ctx); // pick up .md files added since
                        }
                        if let Some(name) = picked {
                            if name != self.cfg.agent {
                                self.select_agent(name, ctx);
                            } else if name == agents::MAGIA {
                                self.magic.start(ctx); // picked again: cast it again
                            }
                        }
                        let before = self.cfg.permission_mode;
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
                        if self.cfg.permission_mode != before {
                            // The mode picker and the `plan` agent stay in step.
                            if before == PermissionMode::Plan && self.cfg.agent == agents::PLAN {
                                self.cfg.agent = agents::BUILD.into();
                            } else if self.cfg.permission_mode == PermissionMode::Plan {
                                self.mode_before_plan = before;
                                if self.cfg.agent == agents::BUILD
                                    || self.cfg.agent == agents::MAGIA
                                {
                                    self.cfg.agent = agents::PLAN.into();
                                }
                            }
                        }
                        if changed {
                            self.apply_settings(ctx);
                        }
                    });

                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if self.tokens != (0, 0) {
                            let rate = self.last_rate.map(|r| format!(" · {:.0} tok/s", r));
                            ui.label(
                                egui::RichText::new(format!(
                                    "{} in · {} out{}",
                                    theme::count(self.tokens.0),
                                    theme::count(self.tokens.1),
                                    rate.unwrap_or_default()
                                ))
                                .font(theme::mono(theme::SMALL))
                                .color(t.muted),
                            )
                            .on_hover_text("Tokens used in this window · speed of the last response");
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
                            ui.label(
                                egui::RichText::new("Enter queues a message for when it finishes")
                                    .color(t.muted)
                                    .size(theme::SMALL),
                            );
                        });
                    }
                    let mut unqueue = None;
                    for (i, (text, images)) in self.queue.iter().enumerate() {
                        ui.horizontal(|ui| {
                            if ui
                                .small_button("×")
                                .on_hover_text("Remove from the queue")
                                .clicked()
                            {
                                unqueue = Some(i);
                            }
                            ui.label(
                                egui::RichText::new("queued")
                                    .color(t.muted)
                                    .size(theme::SMALL),
                            );
                            ui.add(
                                egui::Label::new(
                                    egui::RichText::new(queued_label(text, images.len()))
                                        .color(t.fg)
                                        .size(theme::SMALL + 0.5),
                                )
                                .truncate(),
                            );
                        });
                    }
                    if let Some(i) = unqueue {
                        self.queue.remove(i);
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
                        if self.context > 0 {
                            let (window, at) = self.context_limit;
                            let (text, hover) = context_label(self.context, window, at);
                            // Warn from 80% of the compaction point on.
                            let color = if at > 0 && self.context * 5 >= at * 4 {
                                t.warn
                            } else {
                                t.muted
                            };
                            ui.label(
                                egui::RichText::new(text)
                                    .font(theme::mono(theme::SMALL))
                                    .color(color),
                            )
                            .on_hover_text(hover);
                        }
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if self.running {
                                if theme::secondary(ui, "Stop  Esc", Some(t.danger)).clicked() {
                                    self.stop(ctx);
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

    /// The folder name in the top bar, as a menu: earlier folders and "Open folder…".
    /// `Some(Some(dir))` = open `dir`, `Some(None)` = show the dialog.
    fn folder_menu(&mut self, ui: &mut egui::Ui, name: String) -> Option<Option<PathBuf>> {
        let t = *theme::tokens(ui);
        let mut picked = None;
        let button = egui::Button::new(
            egui::RichText::new(format!("{name} ▾"))
                .font(theme::mono(theme::SMALL))
                .color(t.muted),
        )
        .frame(false);
        let running = self.running;
        let r = egui::containers::menu::MenuButton::from_button(button).ui(ui, |ui| {
            ui.set_min_width(260.0);
            if ui
                .add_enabled(!running, egui::Button::new("Open folder…"))
                .clicked()
            {
                picked = Some(None);
            }
            if !self.recent.is_empty() {
                ui.separator();
                ui.label(
                    egui::RichText::new("Recent")
                        .size(theme::SMALL)
                        .color(t.muted),
                );
                for dir in &self.recent {
                    let name = dir
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_else(|| dir.display().to_string());
                    let parent = dir
                        .parent()
                        .map(|p| tilde(p).display().to_string())
                        .unwrap_or_default();
                    let label = egui::RichText::new(format!("{name}   {parent}"));
                    if ui
                        .add_enabled(!running, egui::Button::new(label))
                        .on_hover_text(dir.display().to_string())
                        .clicked()
                    {
                        picked = Some(Some(dir.clone()));
                    }
                }
            }
        });
        r.0.on_hover_text(format!(
            "{}\nclick to open another folder",
            self.cwd.display()
        ));
        if running && picked.is_some() {
            self.status = "busy — stop or wait for the current request first".into();
            return None;
        }
        picked
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
        if todos.is_empty() && self.activity.is_empty() {
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
                egui::ScrollArea::vertical()
                    .id_salt("tasks_side")
                    .auto_shrink(false)
                    .show(ui, |ui| {
                        let mut jump = activity::show(ui, &self.activity);
                        if !todos.is_empty() {
                            ui.label(
                                egui::RichText::new(tasks_title(&todos))
                                    .font(theme::semibold(theme::SMALL + 1.0))
                                    .color(t.fg),
                            );
                            ui.add_space(6.0);
                            jump = jump.or(todo_list(ui, &todos));
                        }
                        if let Some(j) = jump {
                            self.jump(j);
                        }
                    });
            });
    }

    /// Scroll the transcript to where a round or a task began.
    fn jump(&mut self, j: activity::Jump) {
        let found = match &j {
            activity::Jump::Round(n) => round_start(&self.items, *n),
            activity::Jump::Task(content) => task_start(&self.items, content),
        };
        match found {
            Some(i) => self.scroll_to = Some((i, false)),
            None => self.status = "that point is no longer in the conversation".into(),
        }
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
                    if (!todos.is_empty() || !self.activity.is_empty())
                        && let Some(j) = column(ui, |ui| todo_card(ui, &todos, &self.activity))
                    {
                        self.jump(j);
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
                let mut area = egui::ScrollArea::vertical()
                    .auto_shrink(false)
                    .stick_to_bottom(true);
                if let Some((target, false)) = self.scroll_to {
                    let spacing = ui.spacing().item_spacing.y;
                    let y: f32 = 12.0
                        + self
                            .heights
                            .iter()
                            .take(target)
                            .map(|h| if *h > 0.0 { *h } else { 80.0 } + spacing)
                            .sum::<f32>()
                        + (target.saturating_sub(self.heights.len())) as f32 * (80.0 + spacing);
                    area = area.vertical_scroll_offset(y);
                    self.scroll_to = Some((target, true));
                    ui.ctx().request_repaint();
                }
                area.show_viewport(ui, |ui, viewport| {
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
                            if self.scroll_to == Some((n, true)) {
                                let r = egui::Rect::from_x_y_ranges(
                                    ui.max_rect().x_range(),
                                    start..=start + self.heights[n],
                                );
                                ui.scroll_to_rect(r, Some(egui::Align::TOP));
                                self.scroll_to = None;
                            }
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
            ui.set_width(560.0_f32.min(ctx.content_rect().width() - 48.0));
            theme::role(ui, "THE AGENT ASKS", t.accent);
            ui.add_space(2.0);
            // A long question (or long options) scrolls; the answer field and buttons stay put.
            let max = (ctx.content_rect().height() * 0.6).max(160.0);
            egui::ScrollArea::vertical()
                // Per question, so the next step starts at its top, not where this one was.
                .id_salt(("question-text", &q.text))
                .max_height(max)
                .auto_shrink([false, true])
                .show(ui, |ui| {
                    markdown::show(ui, &q.text, 0x9e57);
                    ui.add_space(6.0);
                    for o in &q.options {
                        let b = egui::Button::new(egui::RichText::new(o))
                            .wrap()
                            .min_size(egui::vec2(ui.available_width(), 30.0));
                        if ui.add(b).clicked() {
                            answer = Some(Some(o.clone()));
                        }
                    }
                });
            ui.add_space(6.0);
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
        let Some(Pending {
            call, reason, dirs, ..
        }) = &self.pending
        else {
            return;
        };
        use magia_core::FolderAnswer;
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
                ui.label(
                    egui::RichText::new(if dirs.is_empty() {
                        "to run?"
                    } else {
                        "to reach outside the project?"
                    })
                    .size(theme::HEADING),
                );
            });
            for d in dirs {
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new("▸").color(t.warn));
                    ui.label(
                        egui::RichText::new(tilde(d).display().to_string())
                            .font(theme::mono(theme::MONO))
                            .color(t.fg),
                    )
                    .on_hover_text(d.display().to_string());
                });
            }
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
            // Only what the summary line above does not already say.
            if let Some((lang, body)) = permission_details(&call.name, &call.arguments) {
                ui.add_space(4.0);
                egui::ScrollArea::vertical()
                    .max_height(260.0)
                    .show(ui, |ui| markdown::code_block(ui, &lang, &body, "perm"));
            }
            ui.add_space(6.0);
            ui.label(
                egui::RichText::new(if dirs.is_empty() {
                    "Change the mode in the top bar to stop being asked."
                } else {
                    "\"Always\" (allow or deny) lasts for this session, kept on /resume."
                })
                .size(theme::SMALL)
                .color(t.muted),
            );
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if dirs.is_empty() {
                        if theme::primary(ui, "Allow", true).clicked() {
                            answer = Some(FolderAnswer::Once);
                        }
                    } else {
                        let always = if dirs.len() == 1 {
                            "Always allow this folder"
                        } else {
                            "Always allow these folders"
                        };
                        if theme::primary(ui, always, true).clicked() {
                            answer = Some(FolderAnswer::Always);
                        }
                        if theme::secondary(ui, "Allow once", None).clicked() {
                            answer = Some(FolderAnswer::Once);
                        }
                    }
                    if theme::secondary(ui, "Deny", Some(t.danger)).clicked() {
                        answer = Some(FolderAnswer::Deny);
                    }
                    if !dirs.is_empty()
                        && theme::secondary(ui, "Always deny", Some(t.danger))
                            .on_hover_text("Refuse these folders for the rest of this session")
                            .clicked()
                    {
                        answer = Some(FolderAnswer::DenyAlways);
                    }
                });
            });
        });
        if let Some(a) = answer
            && let Some(p) = self.pending.take()
        {
            let _ = p.reply.send(a);
        }
    }
}

/// Extra detail for the permission dialog, as (language, text), when the one-line summary
/// does not show everything: a write's content, an edit's change, an unknown tool's input.
fn permission_details(name: &str, args: &serde_json::Value) -> Option<(String, String)> {
    let get = |k: &str| args.get(k).and_then(|v| v.as_str()).unwrap_or("");
    let clip = |s: &str| s.chars().take(3000).collect::<String>();
    match name {
        "read" | "ls" | "glob" | "grep" | "bash" | "webfetch" | "websearch" | "task" => None,
        "write" => {
            let lang = std::path::Path::new(get("file_path"))
                .extension()
                .map(|e| e.to_string_lossy().into_owned())
                .unwrap_or_default();
            Some((lang, clip(get("content"))))
        }
        "edit" => {
            let mut diff = String::new();
            for l in get("old_string").lines() {
                diff.push_str(&format!("- {l}\n"));
            }
            for l in get("new_string").lines() {
                diff.push_str(&format!("+ {l}\n"));
            }
            Some(("diff".into(), clip(diff.trim_end())))
        }
        _ if args.as_object().is_none_or(|o| o.is_empty()) => None,
        _ => Some((
            "json".into(),
            clip(&serde_json::to_string_pretty(args).unwrap_or_default()),
        )),
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

fn agent_label(name: &str) -> &str {
    match name {
        agents::BUILD => "Build",
        agents::PLAN => "Plan",
        agents::MAGIA => "Magia",
        file => file,
    }
}

fn file_agent_help(a: &FileAgent) -> String {
    let size = format!(
        "Adds {} to every request (≈ {} tokens)",
        a.name,
        theme::count(a.approx_tokens())
    );
    if a.description.is_empty() {
        size
    } else {
        format!("{}\n{size}", a.description)
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
        if theme::secondary(ui, "Open folder…", None).clicked() {
            cmd = Some("/open");
        }
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

/// One line for a queued message: its first line, plus how many images it carries.
fn queued_label(text: &str, images: usize) -> String {
    let first = text
        .lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
        .trim();
    match images {
        0 => first.to_owned(),
        1 => format!("{first}  + 1 image"),
        n => format!("{first}  + {n} images"),
    }
}

/// `/home/a/src` → `~/src`, for short labels.
fn tilde(p: &std::path::Path) -> PathBuf {
    match std::env::var_os("HOME").map(PathBuf::from) {
        Some(home) => match p.strip_prefix(&home) {
            Ok(rest) => PathBuf::from("~").join(rest),
            Err(_) => p.to_path_buf(),
        },
        None => p.to_path_buf(),
    }
}

fn tasks_title(todos: &[magia_tools::Todo]) -> String {
    let done = todos
        .iter()
        .filter(|t| matches!(t.status.as_str(), "completed" | "cancelled"))
        .count();
    format!("Tasks  {done}/{}", todos.len())
}

/// The task list; returns the task the user clicked (to see where it began).
fn todo_list(ui: &mut egui::Ui, todos: &[magia_tools::Todo]) -> Option<activity::Jump> {
    let t = *theme::tokens(ui);
    let mut jump = None;
    for todo in todos {
        let (mark, color, text_color) = match todo.status.as_str() {
            "completed" => ("✓", t.ok, t.muted),
            "cancelled" => ("×", t.muted, t.muted),
            "in_progress" => ("▸", t.accent, t.fg),
            _ => ("○", t.muted, t.fg),
        };
        ui.horizontal_top(|ui| {
            ui.label(egui::RichText::new(mark).color(color));
            let mut text = egui::RichText::new(&todo.content)
                .color(text_color)
                .size(theme::SMALL + 0.5);
            if matches!(todo.status.as_str(), "completed" | "cancelled") {
                text = text.strikethrough();
            }
            let r = ui
                .add(egui::Label::new(text).wrap().sense(egui::Sense::click()))
                .on_hover_cursor(egui::CursorIcon::PointingHand)
                .on_hover_text("Show where this task began");
            if r.clicked() {
                jump = Some(activity::Jump::Task(todo.content.clone()));
            }
        });
    }
    jump
}

/// Where the latest request began (its user message; answers to questions don't count).
fn request_start(items: &[Item]) -> Option<usize> {
    items
        .iter()
        .rposition(|i| matches!(i, Item::User(t, _) if !t.starts_with("(answer) ")))
}

/// Round 1 is the request itself; later rounds are their divider after it.
fn round_start(items: &[Item], n: u32) -> Option<usize> {
    let from = request_start(items)?;
    if n <= 1 {
        return Some(from);
    }
    let prefix = format!("round {n} ");
    (from..items.len()).find(|&i| matches!(&items[i], Item::Round(l) if l.starts_with(&prefix)))
}

/// The `todo` call that first set this task in progress, or else first listed it.
fn task_start(items: &[Item], content: &str) -> Option<usize> {
    let lists = items.iter().enumerate().filter_map(|(i, it)| match it {
        Item::Tool { name, input, .. } if name == "todo" => {
            let v: serde_json::Value = serde_json::from_str(input).ok()?;
            let todos = v.get("todos")?.as_array()?.clone();
            Some((i, todos))
        }
        _ => None,
    });
    let mut first = None;
    for (i, todos) in lists {
        for t in &todos {
            if t.get("content").and_then(|c| c.as_str()) == Some(content) {
                if t.get("status").and_then(|s| s.as_str()) == Some("in_progress") {
                    return Some(i);
                }
                first.get_or_insert(i);
            }
        }
    }
    first
}

/// Narrow windows: a collapsible card with a few lines visible; the rest scrolls.
fn todo_card(
    ui: &mut egui::Ui,
    todos: &[magia_tools::Todo],
    act: &activity::Activity,
) -> Option<activity::Jump> {
    let t = *theme::tokens(ui);
    let mut jump = None;
    egui::Frame::new()
        .fill(t.surface)
        .stroke(egui::Stroke::new(1.0, t.line))
        .corner_radius(egui::CornerRadius::same(8))
        .inner_margin(egui::Margin::symmetric(12, 8))
        .outer_margin(egui::Margin::symmetric(0, 8))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            egui::CollapsingHeader::new(
                egui::RichText::new(if todos.is_empty() {
                    "Activity".to_owned()
                } else {
                    tasks_title(todos)
                })
                .font(theme::semibold(theme::SMALL + 1.0)),
            )
            .id_salt("todos")
            .default_open(true)
            .show(ui, |ui| {
                egui::ScrollArea::vertical()
                    .id_salt("todos_scroll")
                    .max_height(160.0)
                    .show(ui, |ui| {
                        jump = activity::show(ui, act);
                        jump = jump.take().or(todo_list(ui, todos));
                    });
            });
        });
    jump
}

/// Built-in commands handled by `App::slash` (name, description, argument hint).
const BUILTINS: &[(&str, &str, &str)] = &[
    ("help", "List the available commands", ""),
    ("clear", "Start a new session", ""),
    ("compact", "Summarize older history to free context", ""),
    ("settings", "Theme and Magia mode settings", ""),
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
    (
        "open",
        "Work in another folder (no path: the folder dialog)",
        "[path]",
    ),
    (
        "reference",
        "Reference folders of this project: read freely, writing there asks (no argument: list, or pick one)",
        "[add|remove <path>]",
    ),
    (
        "permissions",
        "Folders outside the project this session allowed or blocked (`reset` forgets them)",
        "[reset]",
    ),
    (
        "btw",
        "Ask a side question; answered from the conversation without stopping the work or saving it",
        "<question>",
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
        Item::Summary(text) => {
            ui.add_space(10.0);
            let head = egui::RichText::new("Summary of the earlier conversation")
                .size(theme::SMALL)
                .color(t.accent);
            egui::CollapsingHeader::new(head)
                .id_salt(("summary", n))
                // Open: collapsed, the one-line header was easy to miss above the kept messages.
                .default_open(true)
                .show(ui, |ui| {
                    egui::Frame::new()
                        .stroke(egui::Stroke::new(1.0, t.line))
                        .corner_radius(egui::CornerRadius::same(8))
                        .inner_margin(egui::Margin::symmetric(14, 10))
                        .show(ui, |ui| {
                            ui.set_width(ui.available_width());
                            markdown::show(ui, text, n);
                        });
                });
            ui.add_space(4.0);
        }
        Item::Round(label) => {
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                let text = egui::RichText::new(label)
                    .size(theme::SMALL)
                    .color(t.accent);
                let w = ui.available_width();
                let galley_w = ui
                    .painter()
                    .layout_no_wrap(
                        label.clone(),
                        egui::FontId::proportional(theme::SMALL),
                        t.accent,
                    )
                    .size()
                    .x;
                let side = ((w - galley_w - 16.0) / 2.0).max(8.0);
                let line = |ui: &mut egui::Ui| {
                    let (r, _) =
                        ui.allocate_exact_size(egui::vec2(side, 1.0), egui::Sense::hover());
                    ui.painter()
                        .hline(r.x_range(), r.center().y, egui::Stroke::new(1.0, t.line));
                };
                line(ui);
                ui.label(text);
                line(ui);
            });
            ui.add_space(4.0);
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
            Message::User { text, .. }
                if magia_core::split_summary(text).is_none() && !magia_mode::is_continuation(m) =>
            {
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
/// The context counter under the composer: `context 45k / 200k` (or `context 45k` when the
/// window is unknown) and its hover text with the compaction point.
fn context_label(used: u64, window: Option<u64>, compact_at: u64) -> (String, String) {
    let c = theme::count;
    let text = match window {
        Some(w) => format!("context {} / {}", c(used), c(w)),
        None => format!("context {}", c(used)),
    };
    let mut hover = format!("Tokens in the current context (input + output): {used}");
    match window {
        Some(w) => hover.push_str(&format!("\nModel's context window: {w}")),
        None => hover.push_str("\nThe provider does not report this model's context window"),
    }
    if compact_at > 0 {
        hover.push_str(&format!(
            "\nCompacts at about {} ({}% used)",
            c(compact_at),
            (used * 100 / compact_at).min(999)
        ));
    }
    (text, hover)
}

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
        "task" => get("description")
            .or_else(|| get("prompt"))
            .map(|d| match get("agent") {
                Some(role) => format!("{role} · {d}"),
                None => d,
            }),
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
    let mut round = 1;
    for m in history {
        match m {
            Message::User { .. } if magia_mode::is_continuation(m) => {
                round += 1;
                items.push(Item::Round(format!("round {round} · continued")));
            }
            Message::User { text, images } if magia_core::split_summary(text).is_some() => {
                let (summary, rest) = magia_core::split_summary(text).unwrap_or_default();
                round = 1;
                items.push(Item::Summary(summary.to_owned()));
                if !rest.is_empty() || !images.is_empty() {
                    items.push(Item::User(
                        rest.to_owned(),
                        images.iter().map(composer::image_label).collect(),
                    ));
                }
            }
            Message::User { text, images } => {
                round = 1;
                items.push(Item::User(
                    text.clone(),
                    images.iter().map(composer::image_label).collect(),
                ));
            }
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
                ..
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

/// Queued messages become one request: texts joined by a blank line, images in order.
fn merge_queue(
    queue: Vec<(String, Vec<magia_core::Image>)>,
) -> Option<(String, Vec<magia_core::Image>)> {
    if queue.is_empty() {
        return None;
    }
    let (texts, images): (Vec<String>, Vec<Vec<magia_core::Image>>) = queue.into_iter().unzip();
    let text = texts
        .into_iter()
        .filter(|t| !t.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n");
    Some((text, images.into_iter().flatten().collect()))
}

#[cfg(test)]
mod permission_tests {
    use super::permission_details;
    use serde_json::json;

    #[test]
    fn details_only_add_what_the_summary_lacks() {
        assert!(permission_details("read", &json!({"file_path": "/x"})).is_none());
        assert!(permission_details("bash", &json!({"command": "ls"})).is_none());
        let (lang, body) = permission_details(
            "write",
            &json!({"file_path": "a.rs", "content": "fn main() {}"}),
        )
        .unwrap();
        assert_eq!((lang.as_str(), body.as_str()), ("rs", "fn main() {}"));
        let (lang, body) = permission_details(
            "edit",
            &json!({"file_path": "a", "old_string": "x = 1", "new_string": "x = 2"}),
        )
        .unwrap();
        assert_eq!((lang.as_str(), body.as_str()), ("diff", "- x = 1\n+ x = 2"));
        assert_eq!(
            permission_details("mcp__gh__issue", &json!({"n": 1}))
                .unwrap()
                .0,
            "json"
        );
        assert!(permission_details("mcp__gh__ping", &json!({})).is_none());
    }
}

#[cfg(test)]
mod queue_tests {
    use super::merge_queue;

    #[test]
    fn queued_messages_go_out_as_one() {
        assert!(merge_queue(vec![]).is_none());
        let (t, i) = merge_queue(vec![
            ("first".into(), vec![]),
            (String::new(), vec![]),
            ("second".into(), vec![]),
        ])
        .unwrap();
        assert_eq!(t, "first\n\nsecond");
        assert!(i.is_empty());
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
            Message::user(format!("{}\nkeep going", magia_mode::CONTINUE_PREFIX)),
        ];
        assert_eq!(sent_messages(&h), ["look at @a.txt", "next"]);
    }

    #[test]
    fn rounds_and_tasks_lead_to_where_they_began() {
        let todo = |id: &str, status: &str| Item::Tool {
            id: id.into(),
            name: "todo".into(),
            summary: String::new(),
            input: format!(r#"{{"todos":[{{"content":"write docs","status":"{status}"}}]}}"#),
            output: None,
        };
        let items = vec![
            Item::User("old request".into(), vec![]),
            Item::Round("round 2 · continued".into()),
            Item::User("new request".into(), vec![]), // 2
            todo("a", "pending"),                     // 3
            Item::User("(answer) yes".into(), vec![]),
            Item::Round("round 2 · 1 task open".into()), // 5
            todo("b", "in_progress"),                    // 6
        ];
        assert_eq!(round_start(&items, 1), Some(2));
        assert_eq!(round_start(&items, 2), Some(5));
        assert_eq!(round_start(&items, 3), None);
        assert_eq!(task_start(&items, "write docs"), Some(6));
        assert_eq!(task_start(&items[..5], "write docs"), Some(3));
        assert_eq!(task_start(&items, "other"), None);
    }

    #[test]
    fn a_compacted_history_shows_the_summary_then_what_was_kept() {
        let h = vec![
            Message::user(
                "[Summary of the earlier conversation]\nwe fixed X\n[End of summary]\n\nnow do Y",
            ),
            Message::Assistant {
                text: "ok".into(),
                tool_calls: vec![],
            },
        ];
        let items = items_from_history(&h, std::path::Path::new("/p"));
        assert!(matches!(&items[0], Item::Summary(s) if s == "we fixed X"));
        assert!(matches!(&items[1], Item::User(t, _) if t == "now do Y"));
        assert!(matches!(&items[2], Item::Assistant { text, .. } if text == "ok"));
        // Older sessions have no end marker: all of it is the summary.
        let old = [Message::user("[Summary of the earlier conversation]\nlots")];
        let items = items_from_history(&old, std::path::Path::new("/p"));
        assert!(matches!(&items[..], [Item::Summary(s)] if s == "lots"));
    }

    #[test]
    fn magia_continuations_come_back_as_round_dividers() {
        let cont = || Message::user(format!("{}\nkeep going", magia_mode::CONTINUE_PREFIX));
        let h = vec![
            Message::user("build it"),
            cont(),
            cont(),
            Message::user("thanks"),
            cont(),
        ];
        let labels: Vec<String> = items_from_history(&h, std::path::Path::new("/p"))
            .into_iter()
            .map(|i| match i {
                Item::User(t, _) => t,
                Item::Round(r) => r,
                _ => "?".into(),
            })
            .collect();
        assert_eq!(
            labels,
            [
                "build it",
                "round 2 · continued",
                "round 3 · continued",
                "thanks",
                "round 2 · continued"
            ]
        );
    }
}
