//! Minimal MCP client over stdio (newline-delimited JSON-RPC 2.0).
//! Exposes each server tool as a `ufrh_tools::Tool` named `mcp__<server>__<tool>`.
//! Streamable-HTTP servers are not supported.

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::{Mutex as AsyncMutex, oneshot};
use ufrh_tools::{Tool, ToolCx, truncate_output};

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct McpServerConfig {
    pub name: String,
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: HashMap<String, String>,
}

type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<Value>>>>;

pub struct McpClient {
    name: String,
    stdin: AsyncMutex<ChildStdin>,
    pending: Pending,
    next_id: Mutex<u64>,
    _child: Child, // kill_on_drop
}

impl McpClient {
    pub async fn connect(cfg: &McpServerConfig) -> Result<Arc<Self>> {
        let mut child = Command::new(&cfg.command)
            .args(&cfg.args)
            .envs(&cfg.env)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .with_context(|| format!("starting MCP server `{}` ({})", cfg.name, cfg.command))?;
        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let pending: Pending = Default::default();
        let p2 = pending.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let Ok(v) = serde_json::from_str::<Value>(&line) else {
                    continue;
                };
                // Only responses (id + result/error); server requests/notifications are ignored.
                if let (Some(id), true) = (
                    v.get("id").and_then(Value::as_u64),
                    v.get("method").is_none(),
                ) && let Some(tx) = p2.lock().unwrap().remove(&id)
                {
                    let _ = tx.send(v);
                }
            }
            p2.lock().unwrap().clear(); // wake waiters: the server died
        });
        let client = Arc::new(Self {
            name: cfg.name.clone(),
            stdin: AsyncMutex::new(stdin),
            pending,
            next_id: Mutex::new(1),
            _child: child,
        });
        client
            .request(
                "initialize",
                json!({"protocolVersion": "2024-11-05", "capabilities": {},
                       "clientInfo": {"name": "ufrh", "version": env!("CARGO_PKG_VERSION")}}),
            )
            .await?;
        client
            .notify("notifications/initialized", json!({}))
            .await?;
        Ok(client)
    }

    async fn send(&self, v: Value) -> Result<()> {
        let mut s = self.stdin.lock().await;
        s.write_all(format!("{v}\n").as_bytes()).await?;
        s.flush().await?;
        Ok(())
    }

    async fn notify(&self, method: &str, params: Value) -> Result<()> {
        self.send(json!({"jsonrpc": "2.0", "method": method, "params": params}))
            .await
    }

    async fn request(&self, method: &str, params: Value) -> Result<Value> {
        let id = {
            let mut n = self.next_id.lock().unwrap();
            *n += 1;
            *n
        };
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(id, tx);
        self.send(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))
            .await?;
        let resp = tokio::time::timeout(Duration::from_secs(120), rx)
            .await
            .with_context(|| format!("MCP `{}` {method}: timeout", self.name))?
            .map_err(|_| anyhow::anyhow!("MCP server `{}` exited", self.name))?;
        if let Some(err) = resp.get("error") {
            bail!(
                "MCP `{}` {method}: {}",
                self.name,
                err["message"].as_str().unwrap_or("error")
            );
        }
        Ok(resp["result"].clone())
    }

    /// Discover the server's tools as ready-to-register `Tool`s.
    pub async fn tools(self: &Arc<Self>) -> Result<Vec<Box<dyn Tool>>> {
        let r = self.request("tools/list", json!({})).await?;
        let mut out: Vec<Box<dyn Tool>> = Vec::new();
        for t in r["tools"].as_array().cloned().unwrap_or_default() {
            let Some(tool_name) = t["name"].as_str() else {
                continue;
            };
            // Tool names are `'static` by trait contract; the set is small and lives for the process.
            let leak = |s: String| -> &'static str { Box::leak(s.into_boxed_str()) };
            out.push(Box::new(McpTool {
                client: self.clone(),
                remote: tool_name.to_owned(),
                full: leak(format!("mcp__{}__{}", self.name, tool_name)),
                description: leak(t["description"].as_str().unwrap_or("").to_owned()),
                schema: t
                    .get("inputSchema")
                    .cloned()
                    .unwrap_or_else(|| json!({"type": "object"})),
            }));
        }
        Ok(out)
    }
}

struct McpTool {
    client: Arc<McpClient>,
    remote: String,
    full: &'static str,
    description: &'static str,
    schema: Value,
}

#[async_trait]
impl Tool for McpTool {
    fn name(&self) -> &'static str {
        self.full
    }
    fn description(&self) -> &'static str {
        self.description
    }
    fn schema(&self) -> Value {
        self.schema.clone()
    }
    /// External code: always ask.
    fn read_only(&self) -> bool {
        false
    }
    async fn call(&self, input: Value, _cx: &ToolCx) -> Result<String> {
        let r = self
            .client
            .request(
                "tools/call",
                json!({"name": self.remote, "arguments": input}),
            )
            .await?;
        let text: String = r["content"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|c| c["text"].as_str())
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .unwrap_or_default();
        let text = truncate_output(text, 30_000);
        if r["isError"].as_bool().unwrap_or(false) {
            bail!("{text}");
        }
        Ok(text)
    }
}

/// Connect to every configured server; failures are returned alongside the tools.
pub async fn connect_all(cfgs: &[McpServerConfig]) -> (Vec<Box<dyn Tool>>, Vec<String>) {
    let (mut tools, mut errors) = (Vec::new(), Vec::new());
    for c in cfgs {
        match McpClient::connect(c).await {
            Ok(cl) => match cl.tools().await {
                Ok(t) => tools.extend(t),
                Err(e) => errors.push(format!("{e:#}")),
            },
            Err(e) => errors.push(format!("{e:#}")),
        }
    }
    (tools, errors)
}
