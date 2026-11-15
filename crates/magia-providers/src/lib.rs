//! Provider layer. Zen, Go and OpenRouter are *configurations* (base URL + auth +
//! wire format per model), not separate implementations.

mod sse;
mod wire;

pub use wire::Wire;

use anyhow::{Context, Result, bail};
use futures_util::{Stream, StreamExt, stream::BoxStream};
use magia_types::{ChatRequest, ModelInfo, ModelLimit, StreamEvent};
use std::collections::HashMap;
use std::time::Duration;

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
    /// Header carrying `ChatRequest::session_id`, for providers that require one.
    pub session_header: Option<String>,
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
            vec![("X-Title".into(), "magia".into())],
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
        // OpenCode Go rejects requests without it (`MissingSessionID`); Zen uses it for caching.
        session_header: matches!(name, "zen" | "go").then(|| "x-opencode-session".to_owned()),
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
            // Go requires a client-specific user agent (not a generic HTTP-library one).
            .user_agent(concat!("magia/", env!("CARGO_PKG_VERSION")))
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
        let base = self.config.base_url.trim_end_matches('/');
        let mut models = parse_models(&self.get_json(&format!("{base}/models")).await?);
        // LM Studio's OpenAI-style list has no windows; its own API on the same server has
        // them. Only asked when the first list said nothing about any model.
        if !models.is_empty()
            && models.iter().all(|m| m.limit.is_none())
            && let Some(root) = base.strip_suffix("/v1")
        {
            let limits = match self.get_json(&format!("{root}/api/v1/models")).await {
                Ok(v) => lmstudio_windows(&v),
                Err(_) => match self.get_json(&format!("{root}/api/v0/models")).await {
                    Ok(v) => parse_models(&v),
                    Err(_) => vec![],
                },
            };
            for m in &mut models {
                m.limit = limits.iter().find(|l| l.id == m.id).and_then(|l| l.limit);
            }
        }
        Ok(models)
    }

    async fn get_json(&self, url: &str) -> Result<serde_json::Value> {
        let resp = self
            .request(reqwest::Method::GET, url, Auth::Bearer)
            .send()
            .await?;
        let status = resp.status();
        let text = resp.text().await?;
        if !status.is_success() {
            bail!("GET {url} → {status}: {}", truncate(&text, 500));
        }
        serde_json::from_str(&text).context("invalid /models JSON")
    }

    /// Start a streaming completion (one attempt; the caller decides whether to retry, see
    /// [`retry_after`]). A non-2xx answer is an [`HttpError`]; an error event inside the stream
    /// is a [`StreamError`], and so is a stream that ends before the response is complete.
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

        let mut rb = self.request(reqwest::Method::POST, &url, auth).json(&body);
        if let (Some(h), Some(id)) = (&self.config.session_header, &req.session_id) {
            rb = rb.header(h, id);
        }
        let resp = rb.send().await.with_context(|| format!("POST {url}"))?;
        let status = resp.status();
        if !status.is_success() {
            let retry_after = resp
                .headers()
                .get("retry-after")
                .and_then(|h| h.to_str().ok())
                .and_then(|s| s.parse::<u64>().ok());
            let body = resp.text().await.unwrap_or_default();
            return Err(HttpError {
                url,
                status,
                quota: is_quota_error(&body),
                body: truncate(&body, 800),
                retry_after,
            }
            .into());
        }
        Ok(decode_stream(resp.bytes_stream(), wire))
    }
}

/// LM Studio's `/api/v1/models`: the context each loaded model was loaded with (its KV cache
/// is shared by the parallel slots, so one request can use all of it). Models not loaded have
/// none yet.
fn lmstudio_windows(v: &serde_json::Value) -> Vec<ModelInfo> {
    let mut out = Vec::new();
    for inst in v["models"]
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|m| m["loaded_instances"].as_array().into_iter().flatten())
    {
        let c = &inst["config"];
        let (Some(id), Some(ctx)) = (inst["id"].as_str(), c["context_length"].as_u64()) else {
            continue;
        };
        out.push(ModelInfo {
            id: id.to_owned(),
            limit: Some(ModelLimit {
                context: ctx,
                input: None,
                output: None,
            }),
        });
    }
    out
}

/// A `/models` answer (`{"data": [{"id": …}, …]}`) → models sorted by id, with their limits.
fn parse_models(v: &serde_json::Value) -> Vec<ModelInfo> {
    let mut ids: Vec<ModelInfo> = v["data"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|m| {
                    Some(ModelInfo {
                        id: m["id"].as_str()?.to_owned(),
                        limit: ModelLimit::from_json(m),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    ids.sort_by(|a, b| a.id.cmp(&b.id));
    ids
}

/// The provider answered with a non-2xx status.
#[derive(Debug)]
pub struct HttpError {
    pub url: String,
    pub status: reqwest::StatusCode,
    pub body: String,
    /// `retry-after` header, in seconds.
    pub retry_after: Option<u64>,
    /// The body says the plan/credit is used up (a 429 that waiting will not clear).
    pub quota: bool,
}

impl std::fmt::Display for HttpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} → {}: {}", self.url, self.status, self.body)
    }
}

impl std::error::Error for HttpError {}

/// The provider reported an error inside the stream, or the stream ended early.
#[derive(Debug)]
pub struct StreamError(pub String);

impl std::fmt::Display for StreamError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for StreamError {}

/// Whether a failed request is worth repeating, and the least time to wait first. Network,
/// decode and in-stream errors, timeouts, 408/409/425/429 and 5xx are; a bad request, missing
/// or wrong key, an exhausted quota or a context that does not fit are not.
pub fn retry_after(e: &anyhow::Error) -> Option<Duration> {
    if let Some(h) = e.downcast_ref::<HttpError>() {
        let s = h.status.as_u16();
        let transient = matches!(s, 408 | 409 | 425 | 429) || h.status.is_server_error();
        return (transient && !h.quota && !too_long(&h.body))
            .then(|| Duration::from_secs(h.retry_after.unwrap_or(0)));
    }
    if let Some(StreamError(m)) = e.downcast_ref::<StreamError>() {
        return (!is_quota_error(m) && !too_long(m)).then_some(Duration::ZERO);
    }
    Some(Duration::ZERO)
}

/// The request itself is too big; repeating it gives the same answer.
fn too_long(body: &str) -> bool {
    let b = body.to_ascii_lowercase();
    TOO_LONG.iter().any(|k| b.contains(k))
}

const TOO_LONG: [&str; 7] = [
    "context length",
    "context_length",
    "context size",
    "context window",
    "too long",
    "maximum context",
    "too many tokens",
];

/// The provider refused the request as bigger than the model's context. `Some(Some(n))`
/// when the message names the window: the number after the phrase that matched (llama.cpp /
/// LM Studio: "exceeds the available context size (8192 tokens)"; OpenAI: "maximum context
/// length is 128000 tokens").
pub fn context_overflow(e: &anyhow::Error) -> Option<Option<u64>> {
    let msg = match (
        e.downcast_ref::<HttpError>(),
        e.downcast_ref::<StreamError>(),
    ) {
        (Some(h), _) => h.body.clone(),
        (_, Some(StreamError(m))) => m.clone(),
        _ => return None,
    };
    let lower = msg.to_ascii_lowercase();
    let at = TOO_LONG
        .iter()
        .find_map(|k| lower.find(k).map(|i| i + k.len()))?;
    let digits: String = lower[at..]
        .chars()
        .skip_while(|c| !c.is_ascii_digit())
        .take_while(char::is_ascii_digit)
        .collect();
    Some(digits.parse().ok().filter(|&n| n >= 512))
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
                        if let Some(msg) = st.dec.error() {
                            st.ended = true;
                            st.pending
                                .push_back(Err(
                                    StreamError(format!("provider error: {msg}")).into()
                                ));
                            break;
                        }
                    }
                }
                Some(Err(e)) => {
                    st.ended = true;
                    st.pending.push_back(Err(e.into()));
                }
                None => {
                    st.ended = true;
                    if st.dec.complete() {
                        st.pending.extend(st.dec.finish().into_iter().map(Ok));
                    } else {
                        st.pending.push_back(Err(StreamError(
                            "the response stream ended before it was complete".into(),
                        )
                        .into()));
                    }
                }
            }
        }
    })
    .boxed()
}

/// Error bodies that mean "out of credit / limit reached" rather than "slow down".
fn is_quota_error(body: &str) -> bool {
    let b = body.to_ascii_lowercase();
    [
        "usagelimit",
        "insufficient_quota",
        "quota exceeded",
        "insufficient credits",
        "out of credits",
        "limit reached",
    ]
    .iter()
    .any(|k| b.contains(k))
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
    fn lmstudio_window_is_the_loaded_context() {
        let v = serde_json::json!({"models": [
            {"key": "a", "loaded_instances": [{"id": "a", "config": {"context_length": 65536, "parallel": 4}}]},
            {"key": "b", "loaded_instances": []},
            {"key": "c", "loaded_instances": [{"id": "c", "config": {"context_length": 8192}}]}
        ]});
        let w: Vec<_> = lmstudio_windows(&v)
            .into_iter()
            .map(|m| (m.id, m.limit.unwrap().context))
            .collect();
        // `parallel` does not split it: a 41k conversation runs on 131072 with 4 slots.
        assert_eq!(w, [("a".to_owned(), 65_536), ("c".to_owned(), 8_192)]);
    }

    #[test]
    fn a_context_overflow_is_not_retried_and_names_the_window() {
        let http = |body: &str| -> anyhow::Error {
            HttpError {
                url: "u".into(),
                status: reqwest::StatusCode::BAD_REQUEST,
                body: body.into(),
                retry_after: None,
                quota: false,
            }
            .into()
        };
        let lms = http(
            r#"{"error":{"code":400,"message":"request (11671 tokens) exceeds the available context size (8192 tokens), try increasing it"}}"#,
        );
        assert_eq!(context_overflow(&lms), Some(Some(8192)));
        let oai = http(
            "This model's maximum context length is 128000 tokens. However, you requested 130000",
        );
        assert_eq!(context_overflow(&oai), Some(Some(128_000)));
        assert_eq!(context_overflow(&http("prompt is too long")), Some(None));
        assert_eq!(context_overflow(&http("bad key")), None);
        // LM Studio sends it inside the stream; it used to be retried 10 times.
        let in_stream: anyhow::Error = StreamError(
            "provider error: Engine protocol predict request returned 400: request (11671 tokens) exceeds the available context size (8192 tokens)".into(),
        )
        .into();
        assert_eq!(retry_after(&in_stream), None);
        assert_eq!(context_overflow(&in_stream), Some(Some(8192)));
    }

    #[test]
    fn quota_errors_are_not_retried() {
        assert!(is_quota_error(
            r#"{"error":{"type":"FreeUsageLimitError","message":"Free usage limit reached."}}"#
        ));
        assert!(is_quota_error(r#"{"error":{"code":"insufficient_quota"}}"#));
        assert!(!is_quota_error(
            r#"{"error":{"message":"Rate limit, slow down"}}"#
        ));
    }

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
