//! Tool trait, built-in tools and the permission engine.

mod builtin;
mod checkpoint;
mod permissions;

pub use checkpoint::Checkpoints;

pub use builtin::builtin_tools;
pub use permissions::{Decision, PermissionMode, decide};

use anyhow::Result;
use async_trait::async_trait;
use serde_json::Value;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tokio_util::sync::CancellationToken;
use ufrh_types::ToolSpec;

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
    /// "pending" | "in_progress" | "completed"
    pub status: String,
}

impl ToolCx {
    pub fn new(cwd: PathBuf) -> Self {
        let ck = std::env::temp_dir().join(format!(
            "ufrh-ckpt-{}-{}",
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
