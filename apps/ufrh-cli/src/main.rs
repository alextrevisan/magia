//! Headless front-end: `ufrh run "prompt"`, `ufrh models`.

use anyhow::{Result, bail};
use async_trait::async_trait;
use std::io::Write;
use tokio_util::sync::CancellationToken;
use ufrh_core::{Agent, AgentEvent, Approver, Config};
use ufrh_providers::Client;
use ufrh_store::{SessionFile, sessions_dir};
use ufrh_types::ToolCall;

const HELP: &str = "\
ufrh — coding agent harness (headless)

USAGE:
  ufrh run <prompt> [options]     run one turn (tools included) and print the result
                                  @file.txt inlines a file, @shot.png attaches an image
  ufrh models [--provider NAME]   list models from the provider's /models endpoint
  ufrh commands                   list /commands and skills usable in `run` (e.g. ufrh run '/review src')
  ufrh sessions [TEXT] [--all]    list this folder's saved sessions, or search them for TEXT;
                                  --all includes other folders (searchable, not resumable here)

OPTIONS:
  --provider NAME   zen | go | openrouter | <custom from config>   (default: config / zen)
  --model ID        model id                                       (default: config)
  --mode MODE       ask | accept_edits | plan | yolo               (default: config / ask)
  --resume          continue this folder's most recent session
  --session ID      continue a session of this folder (id from `ufrh sessions`)

Keys: OPENCODE_API_KEY (zen, go), OPENROUTER_API_KEY (openrouter).
Config: ufrh.json[c] in ~/.config/ufrh, the project dir or .ufrh/ (OpenCode-compatible; $UFRH_CONFIG adds a file)
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
    let mut args = std::env::args().skip(1);
    let cmd = args.next().unwrap_or_default();
    if matches!(cmd.as_str(), "" | "-h" | "--help" | "help") {
        print!("{HELP}");
        return Ok(());
    }
    let mut cfg = Config::load()?;
    let mut prompt = None;
    let mut resume = false;
    let mut session: Option<String> = None;
    let mut all = false;
    let rest: Vec<String> = args.collect();
    let mut it = rest.into_iter();
    while let Some(a) = it.next() {
        let mut val = |name: &str| {
            it.next()
                .ok_or_else(|| anyhow::anyhow!("{name} needs a value"))
        };
        match a.as_str() {
            "--provider" => cfg.provider = val("--provider")?,
            "--model" => cfg.model = val("--model")?,
            "--mode" => {
                cfg.permission_mode =
                    serde_json::from_value(serde_json::Value::String(val("--mode")?))
                        .map_err(|_| anyhow::anyhow!("mode must be ask|accept_edits|plan|yolo"))?
            }
            "--resume" => resume = true,
            "--session" => session = Some(val("--session")?),
            "--all" => all = true,
            "-h" | "--help" => {
                print!("{HELP}");
                return Ok(());
            }
            s if s.starts_with("--") => bail!("unknown option {s}"),
            _ => prompt = Some(a),
        }
    }
    let cwd = std::env::current_dir()?;
    if cmd == "sessions" {
        let scope = if all {
            ufrh_store::Scope::All
        } else {
            ufrh_store::Scope::Folder(&cwd)
        };
        let hits = ufrh_store::search(&sessions_dir(), scope, prompt.as_deref().unwrap_or(""));
        if hits.is_empty() {
            eprintln!("no sessions found");
        }
        for h in hits {
            let s = &h.session;
            let place = if s.resumable_in(&cwd) {
                String::new()
            } else {
                format!(
                    "   [{}]",
                    s.meta
                        .cwd
                        .as_deref()
                        .map(|p| p.display().to_string())
                        .unwrap_or_default()
                )
            };
            println!(
                "{:<16} {:>9} {:>4} msgs  {}{place}",
                s.id(),
                ufrh_store::age(s.meta.updated),
                s.meta.messages,
                s.meta.title
            );
            for snip in &h.snippets {
                println!("{:<16} {}", "", snip);
            }
        }
        return Ok(());
    }
    if cmd == "commands" {
        // Same catalog as the GUI's `/` list, minus the GUI-only built-ins.
        for e in ufrh_core::slash_catalog(&std::env::current_dir()?, &[], &cfg) {
            let kind = match e.kind {
                ufrh_core::SlashKind::Skill => "skill",
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
    let client = Client::new(cfg.provider_config(&cfg.provider)?)?;
    match cmd.as_str() {
        "models" => {
            for m in client.list_models().await? {
                println!("{}", m.id);
            }
            Ok(())
        }
        "run" => {
            let Some(mut prompt) = prompt else {
                bail!("missing prompt")
            };
            if let Some(expanded) =
                ufrh_core::expand_command(&std::env::current_dir()?, &prompt, &cfg.commands)
            {
                prompt = expanded; // custom slash command from .ufrh/commands
            }
            if cfg.model.is_empty() {
                bail!(
                    "no model set: pass --model ID or set `model` in the config (see `ufrh models`)"
                );
            }
            if client.config.api_key.is_none() {
                bail!(
                    "no API key for `{0}`: set provider.{0}.options.apiKey in {1} or the {2} variable",
                    cfg.provider,
                    ufrh_core::config_path().display(),
                    ufrh_providers::default_key_env(&cfg.provider)
                );
            }
            let mut agent = Agent::new(
                client,
                cfg.model.clone(),
                cfg.permission_mode,
                std::env::current_dir()?,
            );
            let dir = sessions_dir();
            let store = if let Some(id) = &session {
                let found = ufrh_store::list(&dir, ufrh_store::Scope::All)
                    .into_iter()
                    .find(|s| s.id() == *id)
                    .ok_or_else(|| {
                        anyhow::anyhow!("no session `{id}` (see `ufrh sessions --all`)")
                    })?;
                let s = SessionFile::resume(&dir, &found.path, &cwd)?;
                agent.history = s.load()?;
                Some(s)
            } else if resume {
                SessionFile::latest_for(&dir, &cwd).inspect(|s| {
                    if let Ok(h) = s.load() {
                        agent.history = h;
                    }
                })
            } else {
                None
            };
            agent.configure(&cfg);
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
        _ => {
            print!("{HELP}");
            bail!("unknown command `{cmd}`")
        }
    }
}
