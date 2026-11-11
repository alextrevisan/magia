//! Provider layer. Zen, Go and OpenRouter are *configurations* (base URL + auth +
//! wire format per model), not separate implementations.

mod sse;
mod wire;

pub use wire::Wire;

use anyhow::{Context, Result, bail};
use futures_util::{Stream, StreamExt, stream::BoxStream};
use std::collections::HashMap;
use std::time::Duration;
use ufrh_types::{ChatRequest, ModelInfo, StreamEvent};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Auth {
    Bearer,
    XApiKey,
}

/// Static description of a provider.
#[derive(Debug, Clone)]
pub struct ProviderConfig {
    pub name: String,
    /// Includes the `/v1` suffix, e.g. `https://opencode.ai/zen/v1`.
    pub base_url: String,
    pub api_key: Option<String>,
    pub extra_headers: Vec<(String, String)>,
    /// Exact model id → wire override.
    pub wire_overrides: HashMap<String, Wire>,
    pub default_wire: fn(&str) -> Wire,
}

/// Built-in presets. `name` is one of `zen`, `go`, `openrouter`.
pub fn preset(name: &str) -> Option<ProviderConfig> {
    type Preset = (&'static str, fn(&str) -> Wire, Vec<(String, String)>);
    let (base, wire_fn, headers): Preset = match name {
        "zen" => ("https://opencode.ai/zen/v1", zen_wire, vec![]),
        "go" => ("https://opencode.ai/zen/go/v1", go_wire, vec![]),
        "openrouter" => (
            "https://openrouter.ai/api/v1",
            |_| Wire::ChatCompletions,
            vec![("X-Title".into(), "ufrh".into())],
        ),
        _ => return None,
    };
    Some(ProviderConfig {
        name: name.into(),
        base_url: base.into(),
        api_key: None,
        extra_headers: headers,
        wire_overrides: HashMap::new(),
        default_wire: wire_fn,
    })
}

pub fn default_key_env(provider: &str) -> &'static str {
    match provider {
        "openrouter" => "OPENROUTER_API_KEY",
        _ => "OPENCODE_API_KEY",
    }
}

fn zen_wire(model: &str) -> Wire {
    let m = model.rsplit('/').next().unwrap_or(model);
    if m.starts_with("gpt-") {
        Wire::Responses
    } else if m.starts_with("claude-") {
        Wire::Messages
    } else {
        Wire::ChatCompletions
    }
}

fn go_wire(model: &str) -> Wire {
    if model.starts_with("claude-") {
        Wire::Messages
    } else {
        Wire::ChatCompletions
    }
}

impl ProviderConfig {
    pub fn wire_for(&self, model: &str) -> Wire {
        self.wire_overrides
            .get(model)
            .copied()
            .unwrap_or_else(|| (self.default_wire)(model))
    }

    fn url(&self, wire: Wire) -> String {
        let base = self.base_url.trim_end_matches('/');
        match wire {
            Wire::ChatCompletions => format!("{base}/chat/completions"),
            Wire::Responses => format!("{base}/responses"),
            Wire::Messages => {
                format!("{}/v1/messages", base.strip_suffix("/v1").unwrap_or(base))
            }
        }
    }
}

#[derive(Clone)]
pub struct Client {
    http: reqwest::Client,
    pub config: ProviderConfig,
}

impl Client {
    pub fn new(config: ProviderConfig) -> Result<Self> {
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(20))
            .pool_idle_timeout(Duration::from_secs(30))
            .pool_max_idle_per_host(1)
            .build()?;
        Ok(Self { http, config })
    }

    fn request(&self, method: reqwest::Method, url: &str, auth: Auth) -> reqwest::RequestBuilder {
        let mut rb = self.http.request(method, url);
        if let Some(key) = &self.config.api_key {
            rb = match auth {
                Auth::Bearer => rb.bearer_auth(key),
                Auth::XApiKey => rb
                    .header("x-api-key", key)
                    .header("anthropic-version", "2023-06-01"),
            };
        }
        for (k, v) in &self.config.extra_headers {
            rb = rb.header(k, v);
        }
        rb
    }

    pub async fn list_models(&self) -> Result<Vec<ModelInfo>> {
        let url = format!("{}/models", self.config.base_url.trim_end_matches('/'));
        let resp = self
            .request(reqwest::Method::GET, &url, Auth::Bearer)
            .send()
            .await?;
        let status = resp.status();
        let text = resp.text().await?;
        if !status.is_success() {
            bail!("GET {url} → {status}: {}", truncate(&text, 500));
        }
        let v: serde_json::Value = serde_json::from_str(&text).context("invalid /models JSON")?;
        let mut ids: Vec<ModelInfo> = v["data"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|m| m["id"].as_str())
                    .map(|id| ModelInfo { id: id.to_owned() })
                    .collect()
            })
            .unwrap_or_default();
        ids.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(ids)
    }

    /// Start a streaming completion. Retries transient failures (429/5xx/connect)
    /// before the first byte, honouring `retry-after`.
    pub async fn stream(
        &self,
        req: &ChatRequest,
    ) -> Result<BoxStream<'static, Result<StreamEvent>>> {
        let wire = self.config.wire_for(&req.model);
        let url = self.config.url(wire);
        let body = wire::build_body(wire, req);
        let auth = if wire == Wire::Messages {
            Auth::XApiKey
        } else {
            Auth::Bearer
        };

        let mut attempt = 0u32;
        let resp = loop {
            let sent = self
                .request(reqwest::Method::POST, &url, auth)
                .json(&body)
                .send()
                .await;
            match sent {
                Ok(r) if r.status().is_success() => break r,
                Ok(r) => {
                    let status = r.status();
                    let retry_after = r
                        .headers()
                        .get("retry-after")
                        .and_then(|h| h.to_str().ok())
                        .and_then(|s| s.parse::<u64>().ok());
                    let transient = status.as_u16() == 429 || status.is_server_error();
                    if transient && attempt < 3 {
                        let wait = retry_after.unwrap_or(1 << attempt).min(30);
                        tokio::time::sleep(Duration::from_secs(wait)).await;
                        attempt += 1;
                        continue;
                    }
                    let text = r.text().await.unwrap_or_default();
                    bail!("{} → {status}: {}", url, truncate(&text, 800));
                }
                Err(e) if (e.is_connect() || e.is_timeout()) && attempt < 3 => {
                    tokio::time::sleep(Duration::from_secs(1 << attempt)).await;
                    attempt += 1;
                }
                Err(e) => return Err(e).with_context(|| format!("POST {url}")),
            }
        };
        Ok(decode_stream(resp.bytes_stream(), wire))
    }
}

fn decode_stream<S>(bytes: S, wire: Wire) -> BoxStream<'static, Result<StreamEvent>>
where
    S: Stream<Item = reqwest::Result<bytes::Bytes>> + Send + 'static,
{
    struct State<S> {
        bytes: std::pin::Pin<Box<S>>,
        parser: sse::SseParser,
        dec: Box<dyn wire::Decoder>,
        pending: std::collections::VecDeque<Result<StreamEvent>>,
        ended: bool,
    }
    let st = State {
        bytes: Box::pin(bytes),
        parser: sse::SseParser::default(),
        dec: wire::decoder(wire),
        pending: Default::default(),
        ended: false,
    };
    futures_util::stream::unfold(st, |mut st| async move {
        loop {
            if let Some(ev) = st.pending.pop_front() {
                return Some((ev, st));
            }
            if st.ended {
                return None;
            }
            match st.bytes.next().await {
                Some(Ok(chunk)) => {
                    for ev in st.parser.push(&chunk) {
                        st.pending.extend(st.dec.feed(&ev).into_iter().map(Ok));
                    }
                }
                Some(Err(e)) => {
                    st.ended = true;
                    st.pending.push_back(Err(e.into()));
                }
                None => {
                    st.ended = true;
                    st.pending.extend(st.dec.finish().into_iter().map(Ok));
                }
            }
        }
    })
    .boxed()
}

fn truncate(s: &str, n: usize) -> String {
    if s.len() <= n {
        s.to_owned()
    } else {
        format!("{}…", &s[..s.floor_char_boundary(n)])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_selection_and_urls() {
        let zen = preset("zen").unwrap();
        assert_eq!(zen.wire_for("gpt-5"), Wire::Responses);
        assert_eq!(zen.wire_for("claude-sonnet-4"), Wire::Messages);
        assert_eq!(zen.wire_for("kimi-k2"), Wire::ChatCompletions);
        assert_eq!(
            zen.url(Wire::Messages),
            "https://opencode.ai/zen/v1/messages"
        );
        assert_eq!(
            zen.url(Wire::ChatCompletions),
            "https://opencode.ai/zen/v1/chat/completions"
        );
        let go = preset("go").unwrap();
        assert_eq!(
            go.url(Wire::Messages),
            "https://opencode.ai/zen/go/v1/messages"
        );
        let or = preset("openrouter").unwrap();
        assert_eq!(
            or.wire_for("anthropic/claude-sonnet-4"),
            Wire::ChatCompletions
        );
    }
}
