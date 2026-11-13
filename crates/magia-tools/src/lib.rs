//! Tool trait, built-in tools and the permission engine.

pub mod auto;
mod builtin;
mod checkpoint;
pub mod outside;
mod permissions;
mod web;

pub use checkpoint::Checkpoints;

pub use builtin::builtin_tools;
pub use permissions::{Decision, Effect, PermissionMode, RuleSet, decide};
pub use web::{html_to_text, parse_ddg_results};

use anyhow::Result;

/// The `bash` that runs commands and hooks. On Windows, `Command::new("bash")` looks in
/// System32 before PATH and finds the WSL launcher (`System32\\bash.exe`), which fails without a
/// Linux distribution. So search PATH ourselves, skip that launcher, and fall back to Git for
/// Windows' usual places.
pub fn bash_program() -> PathBuf {
    if !cfg!(windows) {
        return PathBuf::from("bash");
    }
    static FOUND: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    FOUND
        .get_or_init(|| {
            let wsl_launcher = |p: &Path| {
                let s = p.to_string_lossy().to_lowercase();
                s.contains("\\system32\\") || s.contains("\\windowsapps\\")
            };
            let in_path = std::env::var_os("PATH").and_then(|path| {
                std::env::split_paths(&path)
                    .map(|d| d.join("bash.exe"))
                    .find(|p| p.is_file() && !wsl_launcher(p))
            });
            let git = || {
                ["ProgramFiles", "ProgramW6432", "LOCALAPPDATA"]
                    .iter()
                    .filter_map(std::env::var_os)
                    .flat_map(|base| {
                        let base = PathBuf::from(base);
                        [
                            base.join("Git\\bin\\bash.exe"),
                            base.join("Programs\\Git\\bin\\bash.exe"),
                        ]
                    })
                    .find(|p| p.is_file())
            };
            in_path
                .or_else(git)
                .unwrap_or_else(|| PathBuf::from("bash"))
        })
        .clone()
}

/// A relative path as the model sees it: `/`-separated on every OS (`src\\a.rs` → `src/a.rs`).
pub fn slashed(p: &Path) -> String {
    let s = p.to_string_lossy();
    if cfg!(windows) {
        s.replace('\\', "/")
    } else {
        s.into_owned()
    }
}
use async_trait::async_trait;
use magia_types::ToolSpec;
use serde_json::Value;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tokio_util::sync::CancellationToken;

/// Per-session context shared by all tool calls.
#[derive(Clone)]
pub struct ToolCx {
    pub cwd: PathBuf,
    pub cancel: CancellationToken,
    /// Files read in this session; `edit`/`write` on an existing file require it.
    pub read_files: Arc<Mutex<HashSet<PathBuf>>>,
    pub checkpoints: Arc<Checkpoints>,
    pub todos: Arc<Mutex<Vec<Todo>>>,
    /// Run `bash` inside bubblewrap (read-only filesystem except the project dir) when available.
    pub sandbox: bool,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Todo {
    pub content: String,
    /// "pending" | "in_progress" | "completed" | "cancelled"
    pub status: String,
}

impl ToolCx {
    pub fn new(cwd: PathBuf) -> Self {
        let ck = std::env::temp_dir().join(format!(
            "magia-ckpt-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos())
        ));
        Self {
            cwd,
            cancel: CancellationToken::new(),
            read_files: Default::default(),
            checkpoints: Arc::new(Checkpoints::new(ck)),
            todos: Default::default(),
            sandbox: false,
        }
    }

    pub fn resolve(&self, p: &str) -> PathBuf {
        let p = Path::new(p);
        if p.is_absolute() {
            p.to_path_buf()
        } else {
            self.cwd.join(p)
        }
    }
}

#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &'static str;
    fn description(&self) -> &'static str;
    fn schema(&self) -> Value;
    /// Read-only tools never modify the filesystem or run commands.
    fn read_only(&self) -> bool;
    /// Edits files (auto-approved in `AcceptEdits` mode).
    fn is_edit(&self) -> bool {
        false
    }
    /// Sends data off the machine (URL, query). Never auto-approved except in `Yolo`, because a
    /// prompt-injected model could use it to exfiltrate what it has read.
    fn network(&self) -> bool {
        false
    }
    async fn call(&self, input: Value, cx: &ToolCx) -> Result<String>;
}

pub struct Registry {
    tools: Vec<Box<dyn Tool>>,
}

impl Registry {
    pub fn builtin() -> Self {
        Self {
            tools: builtin_tools(),
        }
    }

    pub fn add(&mut self, tool: Box<dyn Tool>) {
        self.tools.push(tool);
    }

    pub fn iter(&self) -> impl Iterator<Item = &dyn Tool> {
        self.tools.iter().map(|b| b.as_ref())
    }

    pub fn get(&self, name: &str) -> Option<&dyn Tool> {
        self.tools
            .iter()
            .find(|t| t.name() == name)
            .map(|b| b.as_ref())
    }

    pub fn specs(&self) -> Vec<ToolSpec> {
        self.tools
            .iter()
            .map(|t| ToolSpec {
                name: t.name().into(),
                description: t.description().into(),
                schema: t.schema(),
            })
            .collect()
    }
}

/// Truncate to `max` bytes on a char boundary, noting how much was dropped.
pub fn truncate_output(s: String, max: usize) -> String {
    if s.len() <= max {
        return s;
    }
    let cut = s.floor_char_boundary(max);
    format!("{}\n…[truncated {} bytes]", &s[..cut], s.len() - cut)
}
