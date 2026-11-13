//! The Streamable HTTP transport against a fake server on localhost.

use magia_mcp::{McpServerConfig, connect_all};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// (lower-cased request head, JSON body) of every request.
type Seen = Arc<Mutex<Vec<(String, Value)>>>;

async fn fake_server() -> (String, Seen) {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/mcp", l.local_addr().unwrap());
    let seen: Seen = Default::default();
    let s2 = seen.clone();
    tokio::spawn(async move {
        loop {
            let (mut sock, _) = l.accept().await.unwrap();
            let mut buf = Vec::new();
            let mut chunk = [0u8; 8192];
            let (head, body) = loop {
                let n = sock.read(&mut chunk).await.unwrap();
                buf.extend_from_slice(&chunk[..n]);
                let text = String::from_utf8_lossy(&buf).to_string();
                if let Some(i) = text.find("\r\n\r\n") {
                    let head = text[..i].to_lowercase();
                    let len: usize = head
                        .lines()
                        .find_map(|l| l.strip_prefix("content-length:"))
                        .map(|v| v.trim().parse().unwrap())
                        .unwrap_or(0);
                    if buf.len() >= i + 4 + len {
                        break (head, text[i + 4..i + 4 + len].to_string());
                    }
                }
            };
            let msg: Value = serde_json::from_str(&body).unwrap();
            s2.lock().unwrap().push((head, msg.clone()));
            let id = msg["id"].clone();
            let reply = match msg["method"].as_str().unwrap() {
                "initialize" => json_reply(
                    json!({"jsonrpc":"2.0","id":id,"result":{"protocolVersion":"2025-03-26","capabilities":{}}}),
                    "mcp-session-id: s-123\r\n",
                ),
                "notifications/initialized" => {
                    "HTTP/1.1 202 Accepted\r\ncontent-length: 0\r\nconnection: close\r\n\r\n".into()
                }
                // An SSE answer with a server notification before the response.
                "tools/list" => {
                    let events = format!(
                        "event: message\ndata: {}\n\nevent: message\ndata: {}\n\n",
                        json!({"jsonrpc":"2.0","method":"notifications/message","params":{}}),
                        json!({"jsonrpc":"2.0","id":id,"result":{"tools":[
                            {"name":"echo","description":"Echo","inputSchema":{"type":"object"}}]}})
                    );
                    format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{events}",
                        events.len()
                    )
                }
                "tools/call" => json_reply(
                    json!({"jsonrpc":"2.0","id":id,"result":{"content":[
                        {"type":"text","text": format!("echo: {}", msg["params"]["arguments"]["text"].as_str().unwrap())}]}}),
                    "",
                ),
                m => panic!("unexpected {m}"),
            };
            sock.write_all(reply.as_bytes()).await.unwrap();
        }
    });
    (url, seen)
}

fn json_reply(v: Value, extra: &str) -> String {
    let body = v.to_string();
    format!(
        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\n{extra}content-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    )
}

#[tokio::test]
async fn remote_server_tools_work_over_streamable_http() {
    let (url, seen) = fake_server().await;
    let cfg = McpServerConfig {
        name: "web".into(),
        url: Some(url),
        headers: [("Authorization".to_owned(), "Bearer t0k".to_owned())].into(),
        ..Default::default()
    };
    let (tools, errors) = connect_all(&[cfg]).await;
    assert!(errors.is_empty(), "{errors:?}");
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0].name(), "mcp__web__echo");
    let cx = magia_tools::ToolCx::new(std::env::temp_dir());
    let out = tools[0].call(json!({"text": "hi"}), &cx).await.unwrap();
    assert_eq!(out, "echo: hi");

    let seen = seen.lock().unwrap();
    let methods: Vec<&str> = seen
        .iter()
        .map(|(_, m)| m["method"].as_str().unwrap())
        .collect();
    assert_eq!(
        methods,
        [
            "initialize",
            "notifications/initialized",
            "tools/list",
            "tools/call"
        ]
    );
    for (head, _) in seen.iter() {
        assert!(head.contains("authorization: bearer t0k"), "{head}");
        assert!(head.contains("accept: application/json, text/event-stream"));
    }
    assert!(!seen[0].0.contains("mcp-session-id"));
    for (head, _) in &seen[1..] {
        assert!(head.contains("mcp-session-id: s-123"), "{head}");
        assert!(head.contains("mcp-protocol-version: 2025-03-26"), "{head}");
    }
}

#[tokio::test]
async fn an_unauthorized_remote_server_explains_what_to_do() {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/mcp", l.local_addr().unwrap());
    tokio::spawn(async move {
        let (mut sock, _) = l.accept().await.unwrap();
        let mut b = [0u8; 4096];
        let _ = sock.read(&mut b).await;
        sock.write_all(
            b"HTTP/1.1 401 Unauthorized\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
        )
        .await
        .unwrap();
    });
    let cfg = McpServerConfig {
        name: "web".into(),
        url: Some(url),
        ..Default::default()
    };
    let (_, errors) = connect_all(&[cfg]).await;
    assert!(errors[0].contains("headers"), "{errors:?}");
}
