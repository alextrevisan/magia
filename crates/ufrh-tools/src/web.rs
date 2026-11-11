//! Network tools: `webfetch` (URL → readable text) and `websearch` (DuckDuckGo HTML results).
//! Both are `network()` tools, so they ask for approval outside `yolo` mode.

use crate::{Tool, ToolCx, truncate_output};
use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use serde_json::{Value, json};
use std::sync::OnceLock;
use std::time::Duration;

const MAX_BODY: usize = 2 * 1024 * 1024;
const MAX_OUT: usize = 30_000;

fn http() -> &'static reqwest::Client {
    static C: OnceLock<reqwest::Client> = OnceLock::new();
    C.get_or_init(|| {
        reqwest::Client::builder()
            .user_agent(concat!(
                "Mozilla/5.0 (compatible; ufrh/",
                env!("CARGO_PKG_VERSION"),
                ")"
            ))
            .timeout(Duration::from_secs(30))
            .redirect(reqwest::redirect::Policy::limited(5))
            .pool_max_idle_per_host(1)
            .build()
            .expect("http client")
    })
}

/// Read at most `MAX_BODY` bytes, so a huge page cannot blow up memory.
async fn read_capped(mut resp: reqwest::Response) -> Result<Vec<u8>> {
    let mut buf = Vec::new();
    while let Some(chunk) = resp.chunk().await? {
        let room = MAX_BODY.saturating_sub(buf.len());
        buf.extend_from_slice(&chunk[..chunk.len().min(room)]);
        if room <= chunk.len() {
            break;
        }
    }
    Ok(buf)
}

// ───────────── webfetch ─────────────

pub struct WebFetch;

#[async_trait]
impl Tool for WebFetch {
    fn name(&self) -> &'static str {
        "webfetch"
    }
    fn description(&self) -> &'static str {
        "Fetch an http(s) URL and return its content as plain text (HTML is converted; output truncated)."
    }
    fn schema(&self) -> Value {
        json!({"type":"object","properties":{"url":{"type":"string","description":"http:// or https:// URL"}},"required":["url"]})
    }
    fn read_only(&self) -> bool {
        true
    }
    fn network(&self) -> bool {
        true
    }
    async fn call(&self, input: Value, cx: &ToolCx) -> Result<String> {
        let url = input
            .get("url")
            .and_then(Value::as_str)
            .context("missing string argument `url`")?;
        let parsed = reqwest::Url::parse(url).with_context(|| format!("invalid URL `{url}`"))?;
        if !matches!(parsed.scheme(), "http" | "https") {
            bail!("only http and https URLs are supported");
        }
        let resp = tokio::select! {
            r = http().get(parsed).send() => r.with_context(|| format!("GET {url}"))?,
            _ = cx.cancel.cancelled() => bail!("cancelled"),
        };
        let status = resp.status();
        let final_url = resp.url().to_string();
        let ctype = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_ascii_lowercase();
        let body = read_capped(resp).await?;
        if !status.is_success() {
            bail!("GET {url} → {status}");
        }
        let text = String::from_utf8_lossy(&body);
        let is_html =
            ctype.contains("html") || (ctype.is_empty() && text.trim_start().starts_with('<'));
        let content = if is_html {
            html_to_text(&text)
        } else if ctype.starts_with("text/")
            || ctype.contains("json")
            || ctype.contains("xml")
            || ctype.contains("javascript")
            || ctype.is_empty()
        {
            text.into_owned()
        } else {
            bail!("unsupported content type `{ctype}` (only text, HTML, JSON, XML)");
        };
        let header = if final_url != url {
            format!("[{final_url}]\n\n")
        } else {
            String::new()
        };
        Ok(truncate_output(format!("{header}{content}"), MAX_OUT))
    }
}

/// Small HTML → text converter: drops script/style/etc., turns block elements into line
/// breaks, list items into `- `, decodes entities and collapses whitespace.
pub fn html_to_text(html: &str) -> String {
    // `head` is not skipped: its <title> is kept; style/script inside it are still dropped.
    const SKIP: [&str; 6] = ["script", "style", "noscript", "svg", "template", "iframe"];
    const BLOCK: [&str; 22] = [
        "p",
        "div",
        "br",
        "tr",
        "section",
        "article",
        "header",
        "footer",
        "nav",
        "main",
        "aside",
        "ul",
        "ol",
        "table",
        "pre",
        "blockquote",
        "h1",
        "h2",
        "h3",
        "h4",
        "h5",
        "h6",
    ];
    let mut out = String::with_capacity(html.len() / 3);
    let mut rest = html;
    let mut title = String::new();
    while let Some(lt) = rest.find('<') {
        out.push_str(&decode_entities(&rest[..lt]));
        let tail = &rest[lt..];
        if let Some(comment) = tail.strip_prefix("<!--") {
            rest = comment.find("-->").map_or("", |e| &comment[e + 3..]);
            continue;
        }
        let Some(gt) = tail.find('>') else { break };
        let tag = &tail[1..gt];
        let closing = tag.starts_with('/');
        let name: String = tag
            .trim_start_matches('/')
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric())
            .collect::<String>()
            .to_ascii_lowercase();
        rest = &tail[gt + 1..];
        if !closing && name == "title" {
            if let Some(end) = rest.to_ascii_lowercase().find("</title") {
                title = decode_entities(rest[..end].trim());
                rest = &rest[end..];
            }
            continue;
        }
        if !closing && SKIP.contains(&name.as_str()) && !tag.ends_with('/') {
            let close = format!("</{name}");
            rest = rest
                .to_ascii_lowercase()
                .find(&close)
                .map_or("", |e| &rest[e..]);
            continue;
        }
        if name == "li" && !closing {
            out.push_str("\n- ");
        } else if BLOCK.contains(&name.as_str()) {
            out.push('\n');
        } else if matches!(name.as_str(), "td" | "th") {
            out.push_str(" | ");
        }
    }
    out.push_str(&decode_entities(rest));

    // Collapse spaces inside lines and runs of blank lines.
    let mut clean = String::new();
    let mut blank = 0;
    for line in out.lines() {
        let line = line.split_whitespace().collect::<Vec<_>>().join(" ");
        if line.is_empty() {
            blank += 1;
            if blank > 1 {
                continue;
            }
        } else {
            blank = 0;
        }
        clean.push_str(&line);
        clean.push('\n');
    }
    let clean = clean.trim().to_owned();
    if title.is_empty() {
        clean
    } else {
        format!("# {title}\n\n{clean}")
    }
}

fn decode_entities(s: &str) -> String {
    if !s.contains('&') {
        return s.to_owned();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        let tail = &rest[amp..];
        let end = tail[1..]
            .find(|c: char| !(c.is_ascii_alphanumeric() || c == '#'))
            .map(|i| i + 1);
        let decoded = end.filter(|&e| tail[e..].starts_with(';')).and_then(|e| {
            let name = &tail[1..e];
            let ch = match name {
                "amp" => Some('&'),
                "lt" => Some('<'),
                "gt" => Some('>'),
                "quot" => Some('"'),
                "apos" | "#39" => Some('\''),
                "nbsp" => Some(' '),
                "mdash" => Some('—'),
                "ndash" => Some('–'),
                "hellip" => Some('…'),
                "copy" => Some('©'),
                _ => name.strip_prefix('#').and_then(|n| {
                    let code = match n.strip_prefix(['x', 'X']) {
                        Some(h) => u32::from_str_radix(h, 16).ok(),
                        None => n.parse().ok(),
                    };
                    code.and_then(char::from_u32)
                }),
            };
            ch.map(|c| (c, e + 1))
        });
        match decoded {
            Some((c, len)) => {
                out.push(c);
                rest = &tail[len..];
            }
            None => {
                out.push('&');
                rest = &tail[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

// ───────────── websearch ─────────────

pub struct WebSearch;

#[async_trait]
impl Tool for WebSearch {
    fn name(&self) -> &'static str {
        "websearch"
    }
    fn description(&self) -> &'static str {
        "Search the web (DuckDuckGo). Returns titles, URLs and snippets; use `webfetch` to read a result."
    }
    fn schema(&self) -> Value {
        json!({"type":"object","properties":{
            "query":{"type":"string"},
            "max_results":{"type":"integer","description":"default 8, max 20"}},
            "required":["query"]})
    }
    fn read_only(&self) -> bool {
        true
    }
    fn network(&self) -> bool {
        true
    }
    async fn call(&self, input: Value, cx: &ToolCx) -> Result<String> {
        let query = input
            .get("query")
            .and_then(Value::as_str)
            .context("missing string argument `query`")?;
        let max = input
            .get("max_results")
            .and_then(Value::as_u64)
            .unwrap_or(8)
            .clamp(1, 20) as usize;
        let req = http()
            .post("https://html.duckduckgo.com/html/")
            .form(&[("q", query)]);
        let resp = tokio::select! {
            r = req.send() => r.context("searching DuckDuckGo")?,
            _ = cx.cancel.cancelled() => bail!("cancelled"),
        };
        let status = resp.status();
        let body = String::from_utf8_lossy(&read_capped(resp).await?).into_owned();
        if !status.is_success() {
            bail!("DuckDuckGo → {status}");
        }
        let results = parse_ddg_results(&body);
        if results.is_empty() {
            if body.contains("anomaly") || body.contains("challenge") {
                bail!(
                    "DuckDuckGo asked for a captcha (rate limited); try again later or use webfetch on a known URL"
                );
            }
            return Ok(format!("no results for `{query}`"));
        }
        let mut out = String::new();
        for (i, (title, url, snippet)) in results.into_iter().take(max).enumerate() {
            out.push_str(&format!("{}. {title}\n   {url}\n", i + 1));
            if !snippet.is_empty() {
                out.push_str(&format!("   {snippet}\n"));
            }
        }
        Ok(out)
    }
}

/// Extract `(title, url, snippet)` from DuckDuckGo's HTML results page.
pub fn parse_ddg_results(html: &str) -> Vec<(String, String, String)> {
    let mut out: Vec<(String, String, String)> = Vec::new();
    let mut rest = html;
    while let Some(i) = rest.find("class=\"result__a\"") {
        // The <a …> start of this element.
        let start = rest[..i].rfind("<a").unwrap_or(i);
        let tag_end = rest[i..].find('>').map_or(rest.len(), |e| i + e);
        let tag = &rest[start..tag_end];
        let href = attr(tag, "href").unwrap_or_default();
        let close = rest[tag_end..]
            .find("</a>")
            .map_or(rest.len(), |e| tag_end + e);
        let title = html_to_text(&rest[(tag_end + 1).min(close)..close]);
        rest = &rest[close..];
        // Snippet, if it comes before the next result.
        let next = rest.find("class=\"result__a\"").unwrap_or(rest.len());
        let snippet = rest[..next]
            .find("class=\"result__snippet\"")
            .map_or(String::new(), |s| {
                let seg = &rest[s..next];
                let a = seg.find('>').map_or(seg.len(), |e| e + 1);
                let b = seg[a..]
                    .find("</a>")
                    .or_else(|| seg[a..].find("</td>"))
                    .map_or(seg.len(), |e| a + e);
                html_to_text(&seg[a..b])
            });
        let url = resolve_ddg_url(&decode_entities(&href));
        if !url.is_empty()
            && !out.iter().any(|r| r.1 == url)
            && !url.contains("duckduckgo.com/y.js")
        {
            out.push((title, url, snippet));
        }
    }
    out
}

fn attr(tag: &str, name: &str) -> Option<String> {
    let key = format!("{name}=\"");
    let s = tag.find(&key)? + key.len();
    let e = tag[s..].find('"')? + s;
    Some(tag[s..e].to_owned())
}

/// DuckDuckGo wraps results as `//duckduckgo.com/l/?uddg=<percent-encoded URL>&…`.
fn resolve_ddg_url(href: &str) -> String {
    if let Some(q) = href.split_once("uddg=").map(|(_, q)| q) {
        let enc = q.split('&').next().unwrap_or(q);
        return percent_decode(enc);
    }
    if let Some(rest) = href.strip_prefix("//") {
        return format!("https://{rest}");
    }
    href.to_owned()
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let hex = |c: u8| (c as char).to_digit(16).map(|d| d as u8);
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'%' if i + 2 < b.len() => match (hex(b[i + 1]), hex(b[i + 2])) {
                (Some(h), Some(l)) => {
                    out.push(h * 16 + l);
                    i += 3;
                }
                _ => {
                    out.push(b'%');
                    i += 1;
                }
            },
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn html_to_text_keeps_structure_and_drops_scripts() {
        let html = r#"<html><head><title>Doc &amp; Co</title><style>p{}</style></head>
<body><script>alert(1)</script><h1>Hello</h1><p>One&nbsp;two &lt;b&gt; &#233;t&#xE9;</p>
<!-- hidden --><ul><li>a</li><li>b</li></ul><table><tr><td>x</td><td>y</td></tr></table></body></html>"#;
        let t = html_to_text(html);
        assert!(t.starts_with("# Doc & Co"), "{t}");
        assert!(t.contains("Hello") && t.contains("One two <b> été"), "{t}");
        assert!(
            t.contains("- a\n- b"),
            "list items stay on consecutive lines: {t}"
        );
        assert!(
            !t.contains("alert") && !t.contains("hidden") && !t.contains("p{}"),
            "{t}"
        );
    }

    #[test]
    fn parses_duckduckgo_results() {
        let html = r#"
<div class="result"><h2><a rel="nofollow" class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fwww.rust-lang.org%2Flearn&amp;rut=abc">Learn <b>Rust</b></a></h2>
<a class="result__snippet" href="x">The <b>Rust</b> book &amp; more.</a></div>
<div class="result"><h2><a rel="nofollow" class="result__a" href="https://doc.rust-lang.org/">Docs</a></h2></div>
<div class="result"><a class="result__a" href="https://duckduckgo.com/y.js?ad=1">Ad</a></div>"#;
        let r = parse_ddg_results(html);
        assert_eq!(r.len(), 2);
        assert_eq!(
            r[0],
            (
                "Learn Rust".into(),
                "https://www.rust-lang.org/learn".into(),
                "The Rust book & more.".into()
            )
        );
        assert_eq!(r[1].1, "https://doc.rust-lang.org/");
        assert_eq!(r[1].2, "");
    }

    #[test]
    fn percent_decoding() {
        assert_eq!(percent_decode("a%2Fb%20c+d%zz%"), "a/b c d%zz%");
    }
}
