//! End-to-end agent loop against a fake OpenAI-compatible SSE server.

use std::collections::HashMap;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;
use ufrh_core::{Agent, AgentEvent, AutoApprove};
use ufrh_providers::{Client, ProviderConfig, Wire};
use ufrh_tools::PermissionMode;
use ufrh_types::Message;

fn sse(chunks: &[&str]) -> String {
    let body: String = chunks.iter().map(|c| format!("data: {c}\n\n")).collect();
    format!(
        "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    )
}

/// Serves the given raw HTTP responses, one per connection, and records request bodies.
async fn serve(responses: Vec<String>) -> (String, tokio::task::JoinHandle<Vec<String>>) {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/v1", l.local_addr().unwrap());
    let h = tokio::spawn(async move {
        let mut bodies = Vec::new();
        for r in responses {
            let (mut s, _) = l.accept().await.unwrap();
            let mut buf = vec![0u8; 65536];
            let mut n = 0;
            loop {
                n += s.read(&mut buf[n..]).await.unwrap();
                let text = String::from_utf8_lossy(&buf[..n]).to_string();
                if let Some(i) = text.find("\r\n\r\n") {
                    let len: usize = text[..i]
                        .lines()
                        .find_map(|l| {
                            l.to_lowercase()
                                .strip_prefix("content-length:")
                                .map(|v| v.trim().parse().unwrap())
                        })
                        .unwrap_or(0);
                    if n >= i + 4 + len {
                        bodies.push(text[i + 4..].to_string());
                        break;
                    }
                }
            }
            s.write_all(r.as_bytes()).await.unwrap();
        }
        bodies
    });
    (url, h)
}

fn client(base: String) -> Client {
    Client::new(ProviderConfig {
        name: "fake".into(),
        base_url: base,
        api_key: Some("k".into()),
        extra_headers: vec![],
        wire_overrides: HashMap::new(),
        default_wire: |_| Wire::ChatCompletions,
    })
    .unwrap()
}

#[tokio::test]
async fn tool_call_then_final_answer() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("hello.txt"), "secret-42\n").unwrap();
    let r1 = sse(&[
        r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"c1","function":{"name":"read","arguments":"{\"file_path\":\"hello.txt\"}"}}]}}]}"#,
        r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#,
        "[DONE]",
    ]);
    let r2 = sse(&[
        r#"{"choices":[{"delta":{"content":"The file says secret-42."}}]}"#,
        r#"{"choices":[{"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":9,"completion_tokens":4}}"#,
        "[DONE]",
    ]);
    let (url, server) = serve(vec![r1, r2]).await;

    let mut agent = Agent::new(
        client(url),
        "m".into(),
        PermissionMode::Ask,
        dir.path().into(),
    );
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    agent
        .run_turn(
            "what is in hello.txt?".into(),
            tx,
            CancellationToken::new(),
            &AutoApprove(false),
        )
        .await;

    let mut text = String::new();
    let mut tool_ok = false;
    while let Ok(ev) = rx.try_recv() {
        match ev {
            AgentEvent::TextDelta(t) => text.push_str(&t),
            AgentEvent::ToolEnd {
                output, is_error, ..
            } => tool_ok = !is_error && output.contains("secret-42"),
            AgentEvent::Error(e) => panic!("{e}"),
            _ => {}
        }
    }
    assert!(tool_ok, "read tool should run without approval");
    assert_eq!(text, "The file says secret-42.");
    assert_eq!(agent.usage.input_tokens, 9);
    // user, assistant(call), tool result, assistant(final)
    assert_eq!(agent.history.len(), 4);
    assert!(matches!(
        &agent.history[2],
        Message::ToolResult {
            is_error: false,
            ..
        }
    ));

    let bodies = server.await.unwrap();
    assert!(bodies[1].contains("\"role\":\"tool\"") && bodies[1].contains("secret-42"));
}

#[tokio::test]
async fn denied_write_is_reported_to_model() {
    let dir = tempfile::tempdir().unwrap();
    let r1 = sse(&[
        r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"w1","function":{"name":"write","arguments":"{\"file_path\":\"x\",\"content\":\"y\"}"}}]},"finish_reason":"tool_calls"}]}"#,
        "[DONE]",
    ]);
    let r2 = sse(&[
        r#"{"choices":[{"delta":{"content":"ok"},"finish_reason":"stop"}]}"#,
        "[DONE]",
    ]);
    let (url, _server) = serve(vec![r1, r2]).await;
    let mut agent = Agent::new(
        client(url),
        "m".into(),
        PermissionMode::Ask,
        dir.path().into(),
    );
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    agent
        .run_turn(
            "write".into(),
            tx,
            CancellationToken::new(),
            &AutoApprove(false),
        )
        .await;
    assert!(!dir.path().join("x").exists());
    assert!(
        matches!(&agent.history[2], Message::ToolResult { is_error: true, content, .. } if content.contains("denied"))
    );
}

// ───────────── context compaction, rewind, subagents, hooks, parallel tools ─────────────

use ufrh_core::{HookConfig, Hooks};

fn text_reply(t: &str) -> String {
    sse(&[
        &format!(
            r#"{{"choices":[{{"delta":{{"content":{}}},"finish_reason":"stop"}}]}}"#,
            serde_json::to_string(t).unwrap()
        ),
        "[DONE]",
    ])
}

fn tool_reply(calls: &[(&str, &str, &str)]) -> String {
    let chunks: Vec<String> = calls
        .iter()
        .enumerate()
        .map(|(i, (id, name, args))| {
            format!(
                r#"{{"choices":[{{"delta":{{"tool_calls":[{{"index":{i},"id":"{id}","function":{{"name":"{name}","arguments":{}}}}}]}}}}]}}"#,
                serde_json::to_string(args).unwrap()
            )
        })
        .collect();
    let mut all: Vec<&str> = chunks.iter().map(String::as_str).collect();
    all.push(r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#);
    all.push("[DONE]");
    sse(&all)
}

async fn run(agent: &mut Agent, prompt: &str) -> Vec<AgentEvent> {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    agent
        .run_turn(
            prompt.into(),
            tx,
            CancellationToken::new(),
            &AutoApprove(false),
        )
        .await;
    let mut v = Vec::new();
    while let Ok(e) = rx.try_recv() {
        v.push(e);
    }
    v
}

#[tokio::test]
async fn auto_compaction_summarizes_old_history_and_keeps_tail() {
    let dir = tempfile::tempdir().unwrap();
    let (url, server) = serve(vec![text_reply("SUMMARY-OF-OLD"), text_reply("done")]).await;
    let mut agent = Agent::new(
        client(url),
        "m".into(),
        PermissionMode::Ask,
        dir.path().into(),
    );
    agent.compact_at_tokens = 10;
    for i in 0..5 {
        agent.history.push(Message::User {
            text: format!("question number {i} with some padding text"),
        });
        agent.history.push(Message::Assistant {
            text: format!("answer number {i} with some padding text"),
            tool_calls: vec![],
        });
    }
    let ev = run(&mut agent, "latest question").await;
    assert!(
        ev.iter()
            .any(|e| matches!(e, AgentEvent::Notice(n) if n.contains("compacted")))
    );
    assert!(
        matches!(&agent.history[0], Message::User { text } if text.starts_with("[Summary") && text.contains("SUMMARY-OF-OLD"))
    );
    assert!(
        agent.history.len() < 10,
        "history shrank: {}",
        agent.history.len()
    );
    assert!(
        matches!(agent.history.last(), Some(Message::Assistant { text, .. }) if text == "done")
    );
    let bodies = server.await.unwrap();
    assert!(bodies[0].contains("Summarize this session transcript"));
    assert!(bodies[1].contains("SUMMARY-OF-OLD"));
}

#[tokio::test]
async fn rewind_restores_files_and_drops_turn() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("keep.txt"), "original").unwrap();
    let (url, _s) = serve(vec![
        tool_reply(&[("w1", "write", r#"{"file_path":"new.txt","content":"x"}"#)]),
        text_reply("wrote"),
    ])
    .await;
    let mut agent = Agent::new(
        client(url),
        "m".into(),
        PermissionMode::Yolo,
        dir.path().into(),
    );
    run(&mut agent, "create new.txt").await;
    assert!(dir.path().join("new.txt").exists());
    agent.rewind(1).unwrap();
    assert!(
        !dir.path().join("new.txt").exists(),
        "created file removed by rewind"
    );
    assert!(agent.history.is_empty());
    assert!(agent.rewind(1).is_err());
}

#[tokio::test]
async fn subagent_runs_read_only_with_fresh_context() {
    let dir = tempfile::tempdir().unwrap();
    let (url, server) = serve(vec![
        tool_reply(&[("t1", "task", r#"{"prompt":"find the answer"}"#)]),
        text_reply("report: the answer is 42"),
        text_reply("final: 42"),
    ])
    .await;
    let mut agent = Agent::new(
        client(url),
        "m".into(),
        PermissionMode::Ask,
        dir.path().into(),
    );
    run(&mut agent, "delegate").await;
    assert!(
        matches!(&agent.history[2], Message::ToolResult { content, is_error: false, .. } if content.contains("the answer is 42"))
    );
    let bodies = server.await.unwrap();
    assert!(
        bodies[0].contains(r#""name":"task""#),
        "parent offers `task`"
    );
    assert!(
        !bodies[1].contains(r#""name":"task""#),
        "child has no `task` (no recursion)"
    );
    assert!(
        bodies[1].contains("find the answer") && !bodies[1].contains("delegate"),
        "child context is fresh"
    );
}

#[tokio::test]
async fn pre_tool_hook_blocks_execution() {
    let dir = tempfile::tempdir().unwrap();
    let (url, _s) = serve(vec![
        tool_reply(&[("b1", "bash", r#"{"command":"touch should-not-exist"}"#)]),
        text_reply("ok"),
    ])
    .await;
    let mut agent = Agent::new(
        client(url),
        "m".into(),
        PermissionMode::Yolo,
        dir.path().into(),
    );
    agent.hooks = Hooks(vec![HookConfig {
        event: "pre_tool".into(),
        matcher: Some("bash".into()),
        command: "echo policy says no >&2; exit 2".into(),
    }]);
    run(&mut agent, "go").await;
    assert!(!dir.path().join("should-not-exist").exists());
    assert!(
        matches!(&agent.history[2], Message::ToolResult { is_error: true, content, .. } if content.contains("policy says no"))
    );
}

#[tokio::test]
async fn independent_reads_run_in_parallel_and_keep_order() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), "AAA").unwrap();
    std::fs::write(dir.path().join("b.txt"), "BBB").unwrap();
    let (url, _s) = serve(vec![
        tool_reply(&[
            ("r1", "read", r#"{"file_path":"a.txt"}"#),
            ("r2", "read", r#"{"file_path":"b.txt"}"#),
        ]),
        text_reply("both read"),
    ])
    .await;
    let mut agent = Agent::new(
        client(url),
        "m".into(),
        PermissionMode::Ask,
        dir.path().into(),
    );
    run(&mut agent, "read both").await;
    assert!(
        matches!(&agent.history[2], Message::ToolResult { call_id, content, .. } if call_id == "r1" && content.contains("AAA"))
    );
    assert!(
        matches!(&agent.history[3], Message::ToolResult { call_id, content, .. } if call_id == "r2" && content.contains("BBB"))
    );
}
