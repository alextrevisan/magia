//! Minimal MCP client: local servers over stdio (newline-delimited JSON-RPC 2.0) and remote
//! ones over Streamable HTTP (one POST per message; the answer is JSON or a short SSE stream).
//! Exposes each server tool as a `magia_tools::Tool` named `mcp__<server>__<tool>`.

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use magia_tools::{Tool, ToolCx, truncate_output};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::{Mutex as AsyncMutex, oneshot};

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct McpServerConfig {
    pub name: String,
    /// Local server: the program to start (empty for a remote one).
    #[serde(default)]
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: HashMap<String, String>,
    /// Remote server: its Streamable HTTP endpoint.
    #[serde(default)]
    pub url: Option<String>,
    /// Sent with every request to a remote server (e.g. `Authorization`).
    #[serde(default)]
    pub headers: HashMap<String, String>,
}

/// Protocol version we ask for; remote servers echo the one they speak.
const PROTOCOL: &str = "2025-06-18";

type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<Value>>>>;

pub struct McpClient {
    name: String,
    transport: Transport,
    next_id: Mutex<u64>,
}

enum Transport {
    Stdio {
        stdin: AsyncMutex<ChildStdin>,
        pending: Pending,
        _child: Child, // kill_on_drop
    },
    Http {
        http: reqwest::Client,
        url: String,
        headers: Vec<(String, String)>,
        /// `Mcp-Session-Id` from the server's answer to `initialize`, sent back afterwards.
        session: Mutex<Option<String>>,
        /// The version the server agreed to, sent as `MCP-Protocol-Version` afterwards.
        protocol: Mutex<Option<String>>,
    },
}

impl McpClient {
    pub async fn connect(cfg: &McpServerConfig) -> Result<Arc<Self>> {
        let transport = match &cfg.url {
            Some(url) => Transport::Http {
                http: reqwest::Client::builder()
                    .user_agent(concat!("magia/", env!("CARGO_PKG_VERSION")))
                    .connect_timeout(Duration::from_secs(20))
                    .build()?,
                url: url.clone(),
                headers: cfg
                    .headers
                    .iter()
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect(),
                session: Mutex::new(None),
                protocol: Mutex::new(None),
            },
            None => Self::spawn(cfg)?,
        };
        let client = Arc::new(Self {
            name: cfg.name.clone(),
            transport,
            next_id: Mutex::new(1),
        });
        let init = client
            .request(
                "initialize",
                json!({"protocolVersion": PROTOCOL, "capabilities": {},
                       "clientInfo": {"name": "magia", "version": env!("CARGO_PKG_VERSION")}}),
            )
            .await?;
        if let Transport::Http { protocol, .. } = &client.transport {
            *protocol.lock().unwrap() = init["protocolVersion"].as_str().map(str::to_owned);
        }
        client
            .notify("notifications/initialized", json!({}))
            .await?;
        Ok(client)
    }

    fn spawn(cfg: &McpServerConfig) -> Result<Transport> {
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
        Ok(Transport::Stdio {
            stdin: AsyncMutex::new(stdin),
            pending,
            _child: child,
        })
    }

    async fn notify(&self, method: &str, params: Value) -> Result<()> {
        let msg = json!({"jsonrpc": "2.0", "method": method, "params": params});
        match &self.transport {
            Transport::Stdio { stdin, .. } => write_line(stdin, &msg).await,
            Transport::Http { .. } => self.post(&msg, None).await.map(|_| ()),
        }
    }

    async fn request(&self, method: &str, params: Value) -> Result<Value> {
        let id = {
            let mut n = self.next_id.lock().unwrap();
            *n += 1;
            *n
        };
        let msg = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        let timeout = Duration::from_secs(120);
        let resp = match &self.transport {
            Transport::Stdio { stdin, pending, .. } => {
                let (tx, rx) = oneshot::channel();
                pending.lock().unwrap().insert(id, tx);
                write_line(stdin, &msg).await?;
                tokio::time::timeout(timeout, rx)
                    .await
                    .with_context(|| format!("MCP `{}` {method}: timeout", self.name))?
                    .map_err(|_| anyhow::anyhow!("MCP server `{}` exited", self.name))?
            }
            Transport::Http { .. } => tokio::time::timeout(timeout, self.post(&msg, Some(id)))
                .await
                .with_context(|| format!("MCP `{}` {method}: timeout", self.name))?
                .with_context(|| format!("MCP `{}` {method}", self.name))?
                .ok_or_else(|| anyhow::anyhow!("MCP `{}` {method}: no response", self.name))?,
        };
        if let Some(err) = resp.get("error") {
            bail!(
                "MCP `{}` {method}: {}",
                self.name,
                err["message"].as_str().unwrap_or("error")
            );
        }
        Ok(resp["result"].clone())
    }

    /// Streamable HTTP: POST one message. For a request (`id`), returns the response with that
    /// id, read from a JSON body or from the SSE stream the server answers with.
    async fn post(&self, msg: &Value, id: Option<u64>) -> Result<Option<Value>> {
        let Transport::Http {
            http,
            url,
            headers,
            session,
            protocol,
        } = &self.transport
        else {
            unreachable!("post is only used over HTTP")
        };
        let mut req = http
            .post(url)
            .header("accept", "application/json, text/event-stream")
            .json(msg);
        for (k, v) in headers {
            req = req.header(k, v);
        }
        if let Some(s) = session.lock().unwrap().clone() {
            req = req.header("mcp-session-id", s);
        }
        if let Some(p) = protocol.lock().unwrap().clone() {
            req = req.header("mcp-protocol-version", p);
        }
        let mut resp = req.send().await?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            let hint = match status.as_u16() {
                401 | 403 => {
                    " (the server wants credentials: add them under `headers`, e.g. \"Authorization\": \"Bearer {env:TOKEN}\")"
                }
                404 if session.lock().unwrap().is_some() => " (the server ended the session)",
                _ => "",
            };
            bail!(
                "HTTP {status}{hint}: {}",
                body.trim().chars().take(300).collect::<String>()
            );
        }
        if let Some(s) = resp
            .headers()
            .get("mcp-session-id")
            .and_then(|v| v.to_str().ok())
        {
            *session.lock().unwrap() = Some(s.to_owned());
        }
        let Some(id) = id else {
            return Ok(None); // a notification: 202 Accepted, nothing to read
        };
        let sse = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|c| c.starts_with("text/event-stream"));
        if !sse {
            let v: Value = resp.json().await.context("reading the JSON response")?;
            return Ok(find_response(v, id));
        }
        // Server-sent events: requests and notifications from the server may come first.
        let mut parser = SseLines::default();
        while let Some(chunk) = resp.chunk().await? {
            for data in parser.push(&chunk) {
                if let Some(v) = serde_json::from_str::<Value>(&data)
                    .ok()
                    .and_then(|v| find_response(v, id))
                {
                    return Ok(Some(v));
                }
            }
        }
        Ok(None)
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

async fn write_line(stdin: &AsyncMutex<ChildStdin>, v: &Value) -> Result<()> {
    let mut s = stdin.lock().await;
    s.write_all(format!("{v}\n").as_bytes()).await?;
    s.flush().await?;
    Ok(())
}

/// The response to request `id` in one JSON-RPC message or batch.
fn find_response(v: Value, id: u64) -> Option<Value> {
    match v {
        Value::Array(a) => a.into_iter().find_map(|m| find_response(m, id)),
        m if m.get("id").and_then(Value::as_u64) == Some(id) && m.get("method").is_none() => {
            Some(m)
        }
        _ => None,
    }
}

/// Just enough SSE: the `data:` lines of each event, joined; other fields are ignored.
#[derive(Default)]
struct SseLines {
    buf: Vec<u8>,
    data: Vec<String>,
}

impl SseLines {
    fn push(&mut self, bytes: &[u8]) -> Vec<String> {
        self.buf.extend_from_slice(bytes);
        let mut out = Vec::new();
        while let Some(i) = self.buf.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = self.buf.drain(..=i).collect();
            let line = String::from_utf8_lossy(&line);
            let line = line.trim_end_matches(['\n', '\r']);
            if line.is_empty() {
                if !self.data.is_empty() {
                    out.push(std::mem::take(&mut self.data).join("\n"));
                }
            } else if let Some(d) = line.strip_prefix("data:") {
                self.data.push(d.strip_prefix(' ').unwrap_or(d).to_owned());
            }
        }
        out
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sse_events_split_across_chunks() {
        let mut p = SseLines::default();
        assert!(p.push(b"event: message\ndata: {\"a\"").is_empty());
        assert_eq!(
            p.push(b":1}\r\n\r\ndata: x\n"),
            vec![r#"{"a":1}"#.to_owned()]
        );
        assert_eq!(p.push(b"data: y\n\n"), vec!["x\ny".to_owned()]);
    }

    #[test]
    fn the_matching_response_is_found_in_batches() {
        let batch = json!([
            {"jsonrpc": "2.0", "method": "notifications/progress"},
            {"jsonrpc": "2.0", "id": 7, "method": "ping"},
            {"jsonrpc": "2.0", "id": 7, "result": {"ok": true}}
        ]);
        assert_eq!(find_response(batch, 7).unwrap()["result"]["ok"], true);
        assert!(find_response(json!({"id": 3, "result": {}}), 4).is_none());
    }
}
