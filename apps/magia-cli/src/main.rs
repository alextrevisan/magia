//! Headless front-end: `magia <request…>` (or `--run`), `--models`, `--sessions`, `--commands`.

use anyhow::{Result, bail};
use async_trait::async_trait;
use magia_core::{Agent, AgentEvent, Approver, Config, agents};
use magia_providers::Client;
use magia_store::{SessionFile, sessions_dir};
use magia_types::ToolCall;
use std::io::Write;
use tokio_util::sync::CancellationToken;

/// `print!`/`println!` that end the program quietly once stdout is closed, as in
/// `magia --sessions | head` (the std versions panic with "Broken pipe").
macro_rules! print {
    ($($t:tt)*) => {{
        use std::io::Write as _;
        if write!(std::io::stdout(), $($t)*).is_err() {
            std::process::exit(0)
        }
    }};
}
macro_rules! println {
    ($($t:tt)*) => {{
        use std::io::Write as _;
        if writeln!(std::io::stdout(), $($t)*).is_err() {
            std::process::exit(0)
        }
    }};
}

const HELP: &str = "\
magia — coding agent harness (headless)

USAGE:
  magia <request…>                 run the request in this folder (same as --run);
                                   no quotes needed: magia explain how the parser works
  magia --run <request…>           the same, explicitly
  magia --models                   list the provider's models (GET /models)
  magia --sessions [TEXT] [--all]  list this folder's saved sessions, or search them for TEXT;
                                   --all includes other folders (searchable, not resumable here)
  magia --commands                 list /commands and skills (e.g. magia /review src)
  magia --help | --version

  In a request, @file.txt adds a file and @shot.png attaches an image.
  With no request, a piped stdin is the request: cat task.md | magia
  Put -- before a request that contains --words: magia -- explain --force in git push.
  Quote it when the shell would expand it (?, *, !, $): magia 'why does * not match?'

OPTIONS (anywhere, --opt value or --opt=value):
  --provider NAME   zen | go | openrouter | <custom from config>   (default: config / zen)
  --model ID        model id                                       (default: config)
  --mode MODE       ask | accept_edits | auto | plan | yolo        (default: config / ask)
                    auto: routine work runs, a reviewer model checks the rest, risky asks
  --agent NAME      build | plan | magia | a .md file in the project root, e.g. FABLE.md: its
                    text is added to the instructions              (default: config / build)
                    magia: keeps working in rounds until its task list is done, then has
                    the changes reviewed
  --resume          continue this folder's most recent session
  --session ID      continue a session of this folder (id from --sessions)
  --all             with --sessions: every folder

Keys: OPENCODE_API_KEY (zen, go), OPENROUTER_API_KEY (openrouter).
Config: magia.json[c] in ~/.config/magia, the project dir or .magia/ (OpenCode-compatible; $MAGIA_CONFIG adds a file)
";

struct StdinApprover;
#[async_trait]
impl Approver for StdinApprover {
    async fn approve(&self, call: &ToolCall) -> bool {
        let call = call.clone();
        tokio::task::spawn_blocking(move || {
            eprint!("\n? run {} {} [y/N] ", call.name, call.arguments);
            let _ = std::io::stderr().flush();
            let mut line = String::new();
            std::io::stdin().read_line(&mut line).is_ok()
                && matches!(line.trim(), "y" | "Y" | "yes")
        })
        .await
        .unwrap_or(false)
    }

    async fn approve_because(&self, call: &ToolCall, reason: &str) -> bool {
        eprint!("\n! auto mode: {reason}");
        self.approve(call).await
    }

    async fn ask(&self, question: &str, options: &[String]) -> Option<String> {
        let (question, options) = (question.to_owned(), options.to_vec());
        tokio::task::spawn_blocking(move || {
            eprintln!("\n? {question}");
            for (i, o) in options.iter().enumerate() {
                eprintln!("  {}. {o}", i + 1);
            }
            eprint!("> ");
            let _ = std::io::stderr().flush();
            let mut line = String::new();
            std::io::stdin().read_line(&mut line).ok()?;
            let line = line.trim();
            // A number picks an option; anything else is a free-form answer.
            let picked = line
                .parse::<usize>()
                .ok()
                .and_then(|n| options.get(n.wrapping_sub(1)).cloned());
            Some(picked.unwrap_or_else(|| line.to_owned())).filter(|a| !a.is_empty())
        })
        .await
        .ok()
        .flatten()
    }

    /// Only when stdin is a terminal: under a pipe/CI there is nobody to answer.
    fn interactive(&self) -> bool {
        use std::io::IsTerminal;
        std::io::stdin().is_terminal()
    }
}

fn main() -> Result<()> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .max_blocking_threads(4)
        .enable_all()
        .build()?;
    rt.block_on(real_main())
}

async fn real_main() -> Result<()> {
    let args = parse_args(std::env::args().skip(1))?;
    match args.cmd {
        Cmd::Help => {
            print!("{HELP}");
            return Ok(());
        }
        Cmd::Version => {
            println!("magia {}", env!("CARGO_PKG_VERSION"));
            return Ok(());
        }
        _ => {}
    }
    let mut cfg = Config::load()?;
    if let Some(p) = &args.provider {
        cfg.provider = p.clone();
    }
    if let Some(m) = &args.model {
        cfg.model = m.clone();
    }
    if let Some(m) = &args.mode {
        cfg.permission_mode = serde_json::from_value(serde_json::Value::String(m.clone()))
            .map_err(|_| anyhow::anyhow!("--mode must be ask, accept_edits, auto, plan or yolo"))?;
    }
    let cwd = std::env::current_dir()?;
    let words = args.words.join(" ");
    match args.cmd {
        Cmd::Sessions => {
            sessions(&cwd, &words, args.all);
            return Ok(());
        }
        Cmd::Commands => {
            // Same catalog as the GUI's `/` list, minus the GUI-only built-ins.
            for e in magia_core::slash_catalog(&cwd, &[], &cfg) {
                let kind = match e.kind {
                    magia_core::SlashKind::Skill => "skill",
                    _ => "command",
                };
                let name = if e.args.is_empty() {
                    format!("/{}", e.name)
                } else {
                    format!("/{} {}", e.name, e.args)
                };
                println!("{name:<24} {kind:<8} {}", e.description);
            }
            return Ok(());
        }
        Cmd::Models => {
            let client = Client::new(cfg.provider_config(&cfg.provider)?)?;
            for m in client.list_models().await? {
                println!("{}", m.id);
            }
            return Ok(());
        }
        _ => {}
    }

    // --run (the default): the words are the request; with none, a piped stdin is.
    let mut prompt = words;
    if prompt.trim().is_empty() {
        use std::io::IsTerminal;
        if !std::io::stdin().is_terminal() {
            prompt = read_piped_stdin(std::time::Duration::from_secs(3));
        }
        if prompt.trim().is_empty() {
            print!("{HELP}");
            return Ok(());
        }
    }
    let magia = args
        .agent
        .as_deref()
        .unwrap_or(&cfg.agent)
        .trim()
        .eq_ignore_ascii_case(agents::MAGIA);
    let persona = pick_agent(&mut cfg, args.agent.as_deref(), args.mode.is_some(), &cwd)?;
    let client = Client::new(cfg.provider_config(&cfg.provider)?)?;
    if let Some(expanded) = magia_core::expand_command(&cwd, &prompt, &cfg.commands) {
        prompt = expanded; // custom slash command from .magia/commands
    }
    if cfg.model.is_empty() {
        bail!("no model set: pass --model ID or set `model` in the config (see `magia --models`)");
    }
    if client.config.api_key.is_none() {
        bail!(
            "no API key for `{0}`: set provider.{0}.options.apiKey in {1} or the {2} variable",
            cfg.provider,
            magia_core::config_path().display(),
            magia_providers::default_key_env(&cfg.provider)
        );
    }
    let mut agent = Agent::new(
        client,
        cfg.model.clone(),
        cfg.permission_mode,
        std::env::current_dir()?,
    );
    let dir = sessions_dir();
    let store = if let Some(id) = &args.session {
        let found = magia_store::list(&dir, magia_store::Scope::All)
            .into_iter()
            .find(|s| s.id() == *id)
            .ok_or_else(|| anyhow::anyhow!("no session `{id}` (see `magia --sessions --all`)"))?;
        let s = SessionFile::resume(&dir, &found.path, &cwd)?;
        agent.history = s.load()?;
        Some(s)
    } else if args.resume {
        SessionFile::latest_for(&dir, &cwd).inspect(|s| {
            if let Ok(h) = s.load() {
                agent.history = h;
            }
        })
    } else {
        None
    };
    agent.configure(&cfg);
    agent.persona = persona;
    agent.magia = magia.then_some(cfg.magia_mode);
    for e in agent.connect_mcp(&cfg.mcp).await {
        eprintln!("mcp: {e}");
    }
    agent.store = Some(match store {
        Some(s) => s,
        None => SessionFile::create_for(&dir, &cwd)?,
    });

    let cancel = CancellationToken::new();
    let c2 = cancel.clone();
    tokio::spawn(async move {
        let _ = tokio::signal::ctrl_c().await;
        c2.cancel();
    });
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let printer = tokio::spawn(async move {
        let mut failed = false;
        while let Some(ev) = rx.recv().await {
            match ev {
                AgentEvent::TextDelta(t) => {
                    print!("{t}");
                    let _ = std::io::stdout().flush();
                }
                AgentEvent::ToolStart { name, input, .. } => {
                    eprintln!("\n▸ {name} {input}")
                }
                AgentEvent::ToolEnd {
                    is_error: true,
                    output,
                    ..
                } => {
                    eprintln!("  ✗ {}", output.lines().next().unwrap_or(""))
                }
                AgentEvent::Error(e) => {
                    failed = true;
                    eprintln!("\nerror: {e}")
                }
                AgentEvent::Notice(n) => eprintln!("\n[{n}]"),
                AgentEvent::Round { n, reason } => eprintln!("\n── round {n} · {reason} ──"),
                AgentEvent::Usage(u) => {
                    eprintln!("\n[tokens in {} / out {}]", u.input_tokens, u.output_tokens)
                }
                AgentEvent::TurnDone => break,
                _ => {}
            }
        }
        failed
    });
    let approver = StdinApprover;
    agent.run_turn(prompt, tx, cancel, &approver).await;
    let failed = printer.await.unwrap_or(true);
    println!();
    if failed {
        std::process::exit(1);
    }
    Ok(())
}

/// The request from a piped stdin. Waits up to `first` for the first bytes (or EOF), then
/// reads to the end without a limit. A stdin that is open but silent (an IDE task, a wrapper
/// that never closes it) gives up instead of hanging.
fn read_piped_stdin(first: std::time::Duration) -> String {
    use std::io::Read;
    let (started, wait) = std::sync::mpsc::channel();
    let reader = std::thread::spawn(move || {
        let mut stdin = std::io::stdin().lock();
        let mut buf = vec![0u8; 8192];
        let n = stdin.read(&mut buf).unwrap_or(0);
        buf.truncate(n);
        let _ = started.send(());
        if n > 0 {
            let _ = stdin.read_to_end(&mut buf);
        }
        String::from_utf8_lossy(&buf).into_owned()
    });
    match wait.recv_timeout(first) {
        Ok(()) => reader.join().unwrap_or_default(),
        Err(_) => String::new(), // silent stdin: leave the reader blocked and show the help
    }
}

fn sessions(cwd: &std::path::Path, query: &str, all: bool) {
    let scope = if all {
        magia_store::Scope::All
    } else {
        magia_store::Scope::Folder(cwd)
    };
    let hits = magia_store::search(&sessions_dir(), scope, query);
    if hits.is_empty() {
        eprintln!("no sessions found");
    }
    for h in hits {
        let s = &h.session;
        let place = if s.resumable_in(cwd) {
            String::new()
        } else {
            let owner = s.meta.cwd.as_deref().map(|p| p.display().to_string());
            format!("   [{}]", owner.unwrap_or_else(|| "folder unknown".into()))
        };
        println!(
            "{:<16} {:>9} {:>4} msgs  {}{place}",
            s.id(),
            magia_store::age(s.meta.updated),
            s.meta.messages,
            s.meta.title
        );
        for snip in &h.snippets {
            println!("{:<16} {}", "", snip);
        }
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
enum Cmd {
    #[default]
    Run,
    Models,
    Sessions,
    Commands,
    Help,
    Version,
}

impl Cmd {
    fn flag(self) -> &'static str {
        match self {
            Cmd::Run => "--run",
            Cmd::Models => "--models",
            Cmd::Sessions => "--sessions",
            Cmd::Commands => "--commands",
            Cmd::Help => "--help",
            Cmd::Version => "--version",
        }
    }
}

/// Resolve the agent (`--agent`, else the config/GUI choice). `plan` switches to the Plan mode
/// unless only the saved choice asks for it and `--mode` says otherwise; a file agent returns
/// its instructions. An unknown `--agent` is an error; an unknown saved one (picked in another
/// folder) falls back to `build`.
fn pick_agent(
    cfg: &mut Config,
    flag: Option<&str>,
    mode_flag: bool,
    cwd: &std::path::Path,
) -> Result<Option<(String, String)>> {
    let name = flag.unwrap_or(&cfg.agent).trim().to_owned();
    match name.to_ascii_lowercase().as_str() {
        agents::BUILD | agents::MAGIA => return Ok(None),
        agents::PLAN => {
            if flag.is_some() || !mode_flag {
                cfg.permission_mode = magia_tools::PermissionMode::Plan;
            }
            return Ok(None);
        }
        _ => {}
    }
    match agents::find_agent(cwd, &name) {
        Some(fa) => Ok(Some((fa.name.clone(), agents::load_agent(&fa)?))),
        None if flag.is_some() => {
            let mut names = vec![
                agents::BUILD.to_owned(),
                agents::PLAN.to_owned(),
                agents::MAGIA.to_owned(),
            ];
            names.extend(agents::discover_agents(cwd).into_iter().map(|a| a.name));
            bail!("no agent `{name}` here; available: {}", names.join(", "))
        }
        None => Ok(None),
    }
}

#[derive(Debug, Default, PartialEq, Eq)]
struct Args {
    cmd: Cmd,
    /// Everything that is not an option: the request (or the `--sessions` search).
    words: Vec<String>,
    provider: Option<String>,
    model: Option<String>,
    mode: Option<String>,
    agent: Option<String>,
    resume: bool,
    session: Option<String>,
    all: bool,
}

/// Options can go anywhere (`--opt value` or `--opt=value`); `--` ends them, so the rest is
/// taken as words even when it starts with `--`. Single-dash words stay part of the request.
fn parse_args(args: impl IntoIterator<Item = String>) -> Result<Args> {
    let mut a = Args::default();
    let mut cmd: Option<Cmd> = None;
    let mut it = args.into_iter();
    while let Some(arg) = it.next() {
        let (key, inline) = match arg.split_once('=') {
            Some((k, v)) if k.starts_with("--") => (k.to_owned(), Some(v.to_owned())),
            _ => (arg.clone(), None),
        };
        let action = match key.as_str() {
            "--" => {
                a.words.extend(it.by_ref());
                break;
            }
            "--run" => Some(Cmd::Run),
            "--models" => Some(Cmd::Models),
            "--sessions" => Some(Cmd::Sessions),
            "--commands" => Some(Cmd::Commands),
            "-h" | "--help" => Some(Cmd::Help),
            "-V" | "--version" => Some(Cmd::Version),
            "--resume" => {
                a.resume = true;
                None
            }
            "--all" => {
                a.all = true;
                None
            }
            "--provider" | "--model" | "--mode" | "--agent" | "--session" => {
                let v = match inline {
                    Some(v) => v,
                    None => it
                        .next()
                        .ok_or_else(|| anyhow::anyhow!("{key} needs a value"))?,
                };
                let slot = match key.as_str() {
                    "--provider" => &mut a.provider,
                    "--model" => &mut a.model,
                    "--mode" => &mut a.mode,
                    "--agent" => &mut a.agent,
                    _ => &mut a.session,
                };
                *slot = Some(v);
                None
            }
            k if k.starts_with("--") => bail!("unknown option {k} (see magia --help)"),
            _ => {
                a.words.push(arg);
                None
            }
        };
        if let Some(c) = action {
            match cmd {
                Some(prev) if prev != c && !matches!(c, Cmd::Help) => {
                    bail!("{} and {} cannot be used together", prev.flag(), c.flag())
                }
                _ => cmd = Some(if cmd == Some(Cmd::Help) { Cmd::Help } else { c }),
            }
        }
    }
    a.cmd = cmd.unwrap_or_default();
    Ok(a)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(s: &[&str]) -> Result<Args> {
        parse_args(s.iter().map(|s| s.to_string()))
    }

    #[test]
    fn agent_flag_picks_a_root_file_or_plan() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(
            d.path().join("FABLE.md"),
            "---\ndescription: x\n---\nbe fable",
        )
        .unwrap();
        let mut cfg = Config::default();
        let p = pick_agent(&mut cfg, Some("fable"), false, d.path()).unwrap();
        assert_eq!(p, Some(("FABLE.md".into(), "be fable".into())));
        let err = pick_agent(&mut cfg, Some("nope"), false, d.path()).unwrap_err();
        assert!(err.to_string().contains("FABLE.md"), "{err}");
        // A saved choice from another folder is ignored.
        cfg.agent = "OTHER.md".into();
        assert_eq!(pick_agent(&mut cfg, None, false, d.path()).unwrap(), None);
        // plan switches the mode, unless --mode was given for a saved plan choice.
        cfg.agent = "plan".into();
        cfg.permission_mode = magia_tools::PermissionMode::Yolo;
        pick_agent(&mut cfg, None, true, d.path()).unwrap();
        assert_eq!(cfg.permission_mode, magia_tools::PermissionMode::Yolo);
        pick_agent(&mut cfg, None, false, d.path()).unwrap();
        assert_eq!(cfg.permission_mode, magia_tools::PermissionMode::Plan);
        assert_eq!(
            parse(&["--agent=plan", "x"]).unwrap().agent.as_deref(),
            Some("plan")
        );
    }

    #[test]
    fn plain_words_are_a_request() {
        let a = parse(&[
            "verifique",
            "esta",
            "pasta",
            "e",
            "organize",
            "um",
            "README.md",
            "pra",
            "mim",
        ])
        .unwrap();
        assert_eq!(a.cmd, Cmd::Run);
        assert_eq!(
            a.words.join(" "),
            "verifique esta pasta e organize um README.md pra mim"
        );
        let b = parse(&["--run", "fix", "the", "tests"]).unwrap();
        assert_eq!(
            (b.cmd, b.words.join(" ")),
            (Cmd::Run, "fix the tests".into())
        );
        // One quoted argument works the same.
        assert_eq!(
            parse(&["explain main.rs"]).unwrap().words,
            ["explain main.rs"]
        );
    }

    #[test]
    fn options_anywhere_with_both_value_forms() {
        let a = parse(&[
            "--provider",
            "go",
            "corrija",
            "--model=kimi-k3",
            "o",
            "bug",
            "--mode",
            "plan",
            "--resume",
        ])
        .unwrap();
        assert_eq!(a.words.join(" "), "corrija o bug");
        assert_eq!(a.provider.as_deref(), Some("go"));
        assert_eq!(a.model.as_deref(), Some("kimi-k3"));
        assert_eq!(a.mode.as_deref(), Some("plan"));
        assert!(a.resume);
        assert!(
            parse(&["--model"])
                .unwrap_err()
                .to_string()
                .contains("needs a value")
        );
    }

    #[test]
    fn double_dash_ends_options_and_single_dash_words_stay() {
        let a = parse(&["use", "-O2", "--", "and", "--force", "push"]).unwrap();
        assert_eq!(a.words.join(" "), "use -O2 and --force push");
        assert!(
            parse(&["--frobnicate"])
                .unwrap_err()
                .to_string()
                .contains("unknown option")
        );
    }

    #[test]
    fn actions_are_flags() {
        assert_eq!(
            parse(&["--models", "--provider", "go"]).unwrap().cmd,
            Cmd::Models
        );
        let s = parse(&["--sessions", "parser", "--all"]).unwrap();
        assert_eq!(
            (s.cmd, s.words.join(" "), s.all),
            (Cmd::Sessions, "parser".into(), true)
        );
        assert_eq!(parse(&["--commands"]).unwrap().cmd, Cmd::Commands);
        assert_eq!(parse(&["--models", "--help"]).unwrap().cmd, Cmd::Help);
        assert_eq!(parse(&["-V"]).unwrap().cmd, Cmd::Version);
        assert_eq!(parse(&[]).unwrap(), Args::default());
        let e = parse(&["--models", "--sessions"]).unwrap_err().to_string();
        assert!(e.contains("cannot be used together"), "{e}");
    }
}
