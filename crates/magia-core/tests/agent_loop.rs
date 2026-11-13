//! End-to-end agent loop against a fake OpenAI-compatible SSE server.

use magia_core::{Agent, AgentEvent, AutoApprove};
use magia_providers::{Client, ProviderConfig, Wire};
use magia_tools::PermissionMode;
use magia_types::Message;
use std::collections::HashMap;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

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
        session_header: None,
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
            "what is in hello.txt?",
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
        .run_turn("write", tx, CancellationToken::new(), &AutoApprove(false))
        .await;
    assert!(!dir.path().join("x").exists());
    assert!(
        matches!(&agent.history[2], Message::ToolResult { is_error: true, content, .. } if content.contains("denied"))
    );
}

// ───────────── context compaction, rewind, subagents, hooks, parallel tools ─────────────

use magia_core::{HookConfig, Hooks};

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
        .run_turn(prompt, tx, CancellationToken::new(), &AutoApprove(false))
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
        agent.history.push(Message::user(format!(
            "question number {i} with some padding text"
        )));
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
        matches!(&agent.history[0], Message::User { text, .. } if text.starts_with("[Summary") && text.contains("SUMMARY-OF-OLD"))
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

#[tokio::test]
async fn permission_rules_from_json_config_override_the_mode() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("EXTRA.md"), "always-say-moo").unwrap();
    let cfg = magia_core::Config::from_value(&serde_json::json!({
        "permission": {"bash": {"*": "deny", "echo *": "allow"}},
        "instructions": ["EXTRA.md"]
    }));
    let (url, server) = serve(vec![
        tool_reply(&[
            ("a", "bash", r#"{"command":"echo hi"}"#),
            ("b", "bash", r#"{"command":"touch nope"}"#),
        ]),
        text_reply("done"),
    ])
    .await;
    // Ask mode would prompt for bash; the rules allow `echo *` and deny everything else.
    let mut agent = Agent::new(
        client(url),
        "m".into(),
        PermissionMode::Ask,
        dir.path().into(),
    );
    agent.configure(&cfg);
    run(&mut agent, "go").await;
    assert!(
        matches!(&agent.history[2], Message::ToolResult { content, is_error: false, .. } if content.contains("hi"))
    );
    assert!(
        matches!(&agent.history[3], Message::ToolResult { content, is_error: true, .. } if content.contains("not allowed"))
    );
    assert!(!dir.path().join("nope").exists());
    let bodies = server.await.unwrap();
    assert!(
        bodies[0].contains("always-say-moo"),
        "instructions reach the system prompt"
    );
}

#[tokio::test]
async fn plan_mode_ignores_allow_rules_for_modifying_tools() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = magia_core::Config::from_value(&serde_json::json!({"permission": "allow"}));
    let (url, _s) = serve(vec![
        tool_reply(&[("w", "write", r#"{"file_path":"x","content":"y"}"#)]),
        text_reply("ok"),
    ])
    .await;
    let mut agent = Agent::new(
        client(url),
        "m".into(),
        PermissionMode::Plan,
        dir.path().into(),
    );
    agent.configure(&cfg);
    run(&mut agent, "go").await;
    assert!(!dir.path().join("x").exists());
}

// ───────────── which tools are offered, and the `question` tool ─────────────

/// Tool names offered in one recorded request body.
fn offered(body: &str) -> Vec<String> {
    let v: serde_json::Value = serde_json::from_str(body).unwrap();
    let mut n: Vec<String> = v["tools"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|t| {
                    t.pointer("/function/name")
                        .and_then(|x| x.as_str())
                        .map(str::to_owned)
                })
                .collect()
        })
        .unwrap_or_default();
    n.sort();
    n
}

struct Answering(&'static str);
#[async_trait::async_trait]
impl magia_core::Approver for Answering {
    async fn approve(&self, _: &magia_types::ToolCall) -> bool {
        false
    }
    async fn ask(&self, _q: &str, options: &[String]) -> Option<String> {
        assert_eq!(options, ["yes", "no"]);
        Some(self.0.into())
    }
    fn interactive(&self) -> bool {
        true
    }
}

#[tokio::test]
async fn plan_mode_only_offers_tools_that_can_run() {
    let dir = tempfile::tempdir().unwrap();
    let (url, server) = serve(vec![text_reply("ok")]).await;
    let mut agent = Agent::new(
        client(url),
        "m".into(),
        PermissionMode::Plan,
        dir.path().into(),
    );
    run(&mut agent, "look around").await;
    let names = offered(&server.await.unwrap()[0]);
    for hidden in ["bash", "edit", "write"] {
        assert!(
            !names.contains(&hidden.to_owned()),
            "{hidden} offered in plan mode: {names:?}"
        );
    }
    for shown in [
        "read",
        "grep",
        "glob",
        "ls",
        "todo",
        "task",
        "webfetch",
        "websearch",
    ] {
        assert!(
            names.contains(&shown.to_owned()),
            "{shown} missing: {names:?}"
        );
    }
    assert!(
        !names.contains(&"question".to_owned()),
        "AutoApprove is not interactive"
    );
}

#[tokio::test]
async fn ask_mode_offers_everything_and_catch_all_deny_hides_a_tool() {
    let dir = tempfile::tempdir().unwrap();
    let (url, server) = serve(vec![text_reply("ok")]).await;
    let mut agent = Agent::new(
        client(url),
        "m".into(),
        PermissionMode::Ask,
        dir.path().into(),
    );
    agent.configure(&magia_core::Config::from_value(&serde_json::json!({"permission": {"bash": "deny", "webfetch": {"*": "deny", "https://docs.rs/*": "allow"}}})));
    run(&mut agent, "hi").await;
    let names = offered(&server.await.unwrap()[0]);
    assert!(
        !names.contains(&"bash".to_owned()),
        "bash is denied for every command"
    );
    assert!(
        names.contains(&"webfetch".to_owned()),
        "docs.rs is still allowed"
    );
    assert!(names.contains(&"write".to_owned()) && names.contains(&"edit".to_owned()));
}

#[tokio::test]
async fn subagent_is_not_offered_tools_it_cannot_use() {
    let dir = tempfile::tempdir().unwrap();
    let (url, server) = serve(vec![
        tool_reply(&[("t1", "task", r#"{"prompt":"investigate"}"#)]),
        text_reply("report"),
        text_reply("final"),
    ])
    .await;
    let mut agent = Agent::new(
        client(url),
        "m".into(),
        PermissionMode::Yolo,
        dir.path().into(),
    );
    run(&mut agent, "delegate").await;
    let bodies = server.await.unwrap();
    let child = offered(&bodies[1]);
    assert_eq!(
        child,
        ["glob", "grep", "ls", "read", "todo"],
        "child (plan + no approver) gets only auto-allowed tools"
    );
    let parent = offered(&bodies[0]);
    assert!(
        parent.contains(&"bash".to_owned()) && parent.contains(&"webfetch".to_owned()),
        "{parent:?}"
    );
}

#[tokio::test]
async fn question_tool_reaches_the_user_and_returns_the_answer() {
    let dir = tempfile::tempdir().unwrap();
    let (url, server) = serve(vec![
        tool_reply(&[(
            "q1",
            "question",
            r#"{"question":"Use tabs?","options":["yes","no"]}"#,
        )]),
        text_reply("ok, tabs"),
    ])
    .await;
    let mut agent = Agent::new(
        client(url),
        "m".into(),
        PermissionMode::Ask,
        dir.path().into(),
    );
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    agent
        .run_turn("format it", tx, CancellationToken::new(), &Answering("yes"))
        .await;
    assert!(
        matches!(&agent.history[2], Message::ToolResult { content, is_error: false, .. } if content == "The user answered: yes")
    );
    assert!(offered(&server.await.unwrap()[0]).contains(&"question".to_owned()));
}

/// Like `serve`, but returns the raw request heads (request line + headers).
async fn serve_heads(responses: Vec<String>) -> (String, tokio::task::JoinHandle<Vec<String>>) {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/v1", l.local_addr().unwrap());
    let h = tokio::spawn(async move {
        let mut heads = Vec::new();
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
                        heads.push(text[..i].to_lowercase());
                        break;
                    }
                }
            }
            s.write_all(r.as_bytes()).await.unwrap();
        }
        heads
    });
    (url, h)
}

#[tokio::test]
async fn session_header_is_stable_within_a_conversation_and_user_agent_is_ours() {
    let dir = tempfile::tempdir().unwrap();
    // SAFETY: no other test in this binary reads XDG_DATA_HOME; keeps session files out of $HOME.
    unsafe { std::env::set_var("XDG_DATA_HOME", dir.path().join("data")) };
    let (url, server) = serve_heads(vec![
        tool_reply(&[("l1", "ls", "{}")]),
        text_reply("done"),
        text_reply("again"),
    ])
    .await;
    let mut cfg = magia_providers::preset("go").unwrap();
    cfg.base_url = url;
    cfg.api_key = Some("k".into());
    let mut agent = Agent::new(
        Client::new(cfg).unwrap(),
        "m".into(),
        PermissionMode::Ask,
        dir.path().into(),
    );
    run(&mut agent, "first").await;
    run(&mut agent, "second").await;
    let heads = server.await.unwrap();
    let sid = |h: &str| {
        h.lines().find_map(|l| {
            l.strip_prefix("x-opencode-session:")
                .map(|v| v.trim().to_owned())
        })
    };
    let ids: Vec<_> = heads
        .iter()
        .map(|h| sid(h).expect("x-opencode-session sent"))
        .collect();
    assert!(
        ids.iter().all(|i| *i == ids[0] && !i.is_empty()),
        "same id for the whole conversation: {ids:?}"
    );
    assert!(heads[0].contains("user-agent: magia/"), "{}", heads[0]);
    let first = ids[0].clone();
    agent.cx.todos.lock().unwrap().push(magia_tools::Todo {
        content: "old task".into(),
        status: "pending".into(),
    });
    agent.new_session().unwrap();
    assert!(
        agent.cx.todos.lock().unwrap().is_empty(),
        "a new conversation starts with no tasks"
    );
    assert_ne!(
        agent.session_id(),
        first,
        "a new conversation gets a new id"
    );
}

#[tokio::test]
async fn pasted_and_mentioned_images_reach_the_model_and_the_session() {
    let dir = tempfile::tempdir().unwrap();
    // SAFETY: tests in this file that read XDG_DATA_HOME all set it to a temp dir.
    unsafe { std::env::set_var("XDG_DATA_HOME", dir.path().join("data")) };
    std::fs::write(dir.path().join("b.jpg"), [0xFF, 0xD8, 0xFF, 0xE0, 7]).unwrap();
    let (url, server) = serve(vec![text_reply("a cat"), text_reply("still a cat")]).await;
    let mut agent = Agent::new(
        client(url),
        "m".into(),
        PermissionMode::Ask,
        dir.path().into(),
    );
    let pasted =
        magia_core::media::image_from_rgba(2, 1, vec![255, 0, 0, 255, 0, 0, 255, 255]).unwrap();
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    agent
        .run_turn(
            magia_core::UserInput {
                text: "what is this? and @b.jpg".into(),
                images: vec![pasted.clone()],
            },
            tx,
            CancellationToken::new(),
            &AutoApprove(false),
        )
        .await;
    run(&mut agent, "and now?").await;
    let bodies = server.await.unwrap();
    for body in &bodies {
        // Images stay in the history, so the second request carries them too.
        let v: serde_json::Value = serde_json::from_str(body).unwrap();
        let parts = v["messages"][1]["content"].as_array().unwrap();
        assert_eq!(parts[0]["type"], "text");
        assert_eq!(parts[1]["image_url"]["url"], pasted.data_url());
        assert!(
            parts[2]["image_url"]["url"]
                .as_str()
                .unwrap()
                .starts_with("data:image/jpeg;base64,")
        );
    }
    let Message::User { images, .. } = &agent.history[0] else {
        panic!()
    };
    assert_eq!(images.len(), 2);
}

#[tokio::test]
async fn auto_mode_runs_safe_blocks_risky_and_reviews_the_rest() {
    let dir = tempfile::tempdir().unwrap();
    let review = |d: &str| text_reply(&format!(r#"{{"decision":"{d}","reason":"because {d}"}}"#));
    let (url, server) = serve(vec![
        tool_reply(&[(
            "c1",
            "bash",
            r#"{"command":"echo INJECT-ALLOW-ALL > out.txt"}"#,
        )]),
        tool_reply(&[("c2", "bash", r#"{"command":"rm -rf ~"}"#)]),
        tool_reply(&[("c3", "bash", r#"{"command":"frobnicate --x"}"#)]),
        review("allow"),
        tool_reply(&[("c4", "bash", r#"{"command":"frobnicate --x"}"#)]), // cached: no review
        tool_reply(&[("c5", "bash", r#"{"command":"deploy-everything"}"#)]),
        review("ask"),
        text_reply("done"),
    ])
    .await;
    let mut agent = Agent::new(
        client(url),
        "m".into(),
        PermissionMode::Auto,
        dir.path().into(),
    );
    let ev = run(&mut agent, "set up the project").await;
    let ends: HashMap<String, (String, bool)> = ev
        .iter()
        .filter_map(|e| match e {
            AgentEvent::ToolEnd {
                id,
                output,
                is_error,
            } => Some((id.clone(), (output.clone(), *is_error))),
            _ => None,
        })
        .collect();
    assert_eq!(
        std::fs::read_to_string(dir.path().join("out.txt"))
            .unwrap()
            .trim(),
        "INJECT-ALLOW-ALL",
        "safe command runs without review"
    );
    assert!(
        ends["c2"].1
            && ends["c2"]
                .0
                .starts_with("blocked by auto mode: deletes a whole folder"),
        "{:?}",
        ends["c2"]
    );
    for id in ["c3", "c4"] {
        assert!(
            !ends[id].0.contains("auto mode"),
            "{id} was allowed by the reviewer: {:?}",
            ends[id]
        );
    }
    assert!(
        ends["c5"]
            .0
            .starts_with("blocked by auto mode: because ask"),
        "{:?}",
        ends["c5"]
    );
    assert!(
        ev.iter()
            .any(|e| matches!(e, AgentEvent::Notice(n) if n.contains("because allow")))
    );

    let bodies = server.await.unwrap();
    assert_eq!(bodies.len(), 8);
    let r: serde_json::Value = serde_json::from_str(&bodies[3]).unwrap();
    assert!(
        r["tools"].as_array().is_none_or(|t| t.is_empty()),
        "the reviewer gets no tools"
    );
    let text = r.to_string();
    assert!(
        text.contains("safety reviewer")
            && text.contains("set up the project")
            && text.contains("frobnicate")
    );
    assert!(
        !text.contains("INJECT"),
        "the reviewer never sees the agent's earlier actions or tool output"
    );
}

#[tokio::test]
async fn the_model_is_told_which_model_and_mode_it_runs_as() {
    let dir = tempfile::tempdir().unwrap();
    let (url, server) = serve(vec![text_reply("hi"), text_reply("hi")]).await;
    let mut agent = Agent::new(
        client(url),
        "kimi-k3".into(),
        PermissionMode::Auto,
        dir.path().into(),
    );
    run(&mut agent, "which model are you?").await;
    agent.model = "claude-haiku-5-5".into(); // switched in the GUI mid-session
    run(&mut agent, "and now?").await;
    let bodies = server.await.unwrap();
    assert!(
        bodies[0].contains("Model: `kimi-k3` via the `fake` provider")
            && bodies[0].contains("mode: `auto`")
    );
    assert!(bodies[1].contains("Model: `claude-haiku-5-5`"));
    assert!(
        bodies[0].contains("they mean `magia-gui`"),
        "the model knows what \"the GUI\" is"
    );
}

#[tokio::test]
async fn a_file_agent_is_added_to_the_instructions_once() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("AGENTS.md"), "PROJECT RULES").unwrap();
    let (url, server) = serve(vec![text_reply("a"), text_reply("b"), text_reply("c")]).await;
    let mut agent = Agent::new(
        client(url),
        "m".into(),
        PermissionMode::Ask,
        dir.path().into(),
    );
    agent.persona = Some(("FABLE.md".into(), "YOU ARE FABLE".into()));
    run(&mut agent, "one").await;
    // Picking the file that already is the project instructions adds nothing more.
    agent.persona = Some(("AGENTS.md".into(), "PROJECT RULES".into()));
    run(&mut agent, "two").await;
    agent.persona = None;
    run(&mut agent, "three").await;
    let bodies = server.await.unwrap();
    let base = bodies[0].find("You are magia").expect("base prompt kept");
    let fable = bodies[0].find("YOU ARE FABLE").expect("agent text sent");
    assert!(base < fable, "added after magia's own prompt");
    assert!(bodies[0].contains("Agent instructions (FABLE.md)"));
    assert_eq!(bodies[1].matches("PROJECT RULES").count(), 1);
    assert!(!bodies[2].contains("YOU ARE FABLE") && bodies[2].contains("PROJECT RULES"));
}

#[tokio::test]
async fn a_big_agent_prompt_alone_does_not_trigger_compaction() {
    let dir = tempfile::tempdir().unwrap();
    // The provider reports the whole input (prompt + history) as ~8k tokens.
    let usage = r#"{"choices":[],"usage":{"prompt_tokens":8000,"completion_tokens":1}}"#;
    let reply = |t: &str| {
        sse(&[
            &format!(r#"{{"choices":[{{"delta":{{"content":"{t}"}},"finish_reason":"stop"}}]}}"#),
            usage,
            "[DONE]",
        ])
    };
    let (url, server) = serve(vec![reply("a"), reply("b"), reply("c")]).await;
    let mut agent = Agent::new(
        client(url),
        "m".into(),
        PermissionMode::Ask,
        dir.path().into(),
    );
    agent.compact_at_tokens = 1_000;
    // ≈ 7.5k tokens of instructions: nearly all of the reported input (the fake server
    // reads at most 64 KB per request, so this stays small).
    agent.persona = Some(("BIG.md".into(), "x".repeat(30_000)));
    // Long enough to be compactable, so only the threshold decides.
    for i in 0..5 {
        agent.history.push(Message::user(format!("question {i}")));
        agent.history.push(Message::Assistant {
            text: format!("answer {i}"),
            tool_calls: vec![],
        });
    }
    for p in ["one", "two", "three"] {
        let ev = run(&mut agent, p).await;
        assert!(
            !ev.iter()
                .any(|e| matches!(e, AgentEvent::Notice(n) if n.contains("compact"))),
            "{p}: {ev:?}"
        );
    }
    let bodies = server.await.unwrap();
    assert!(bodies.iter().all(|b| !b.contains("Summarize this session")));
}

// ───────────── Magia mode ─────────────

fn magia_agent(url: String, dir: &std::path::Path, mode: PermissionMode) -> Agent {
    let mut a = Agent::new(client(url), "m".into(), mode, dir.into());
    a.magia = Some(magia_core::magia_mode::MagiaMode::default());
    a
}

fn todos(items: &[(&str, &str)]) -> String {
    let list: Vec<_> = items
        .iter()
        .map(|(c, s)| serde_json::json!({"content": c, "status": s}))
        .collect();
    serde_json::json!({ "todos": list }).to_string()
}

fn rounds(ev: &[AgentEvent]) -> Vec<(u32, String)> {
    ev.iter()
        .filter_map(|e| match e {
            AgentEvent::Round { n, reason } => Some((*n, reason.clone())),
            _ => None,
        })
        .collect()
}

fn no_errors(ev: &[AgentEvent]) {
    for e in ev {
        if let AgentEvent::Error(e) = e {
            panic!("unexpected error: {e}");
        }
    }
}

fn notices(ev: &[AgentEvent]) -> String {
    ev.iter()
        .filter_map(|e| match e {
            AgentEvent::Notice(n) => Some(n.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[tokio::test]
async fn magia_continues_while_tasks_are_open_then_has_the_work_reviewed() {
    let dir = tempfile::tempdir().unwrap();
    let plan = todos(&[("write x", "pending"), ("check x", "pending")]);
    let half = todos(&[("write x", "completed"), ("check x", "pending")]);
    let all = todos(&[("write x", "completed"), ("check x", "completed")]);
    let (url, server) = serve(vec![
        tool_reply(&[("t1", "todo", &plan)]),
        tool_reply(&[
            ("w1", "write", r#"{"file_path":"x.txt","content":"x"}"#),
            ("t2", "todo", &half),
        ]),
        text_reply("x written"), // stops with one item open
        tool_reply(&[("t3", "todo", &all)]),
        text_reply("all done"),
        text_reply(r#"Checked x.txt. {"verdict":"pass","issues":[]}"#), // the reviewer
    ])
    .await;
    let mut agent = magia_agent(url, dir.path(), PermissionMode::Yolo);
    let ev = run(&mut agent, "create x.txt and check it").await;
    no_errors(&ev);
    assert_eq!(rounds(&ev), vec![(2, "1 task(s) left".into())]);
    assert!(notices(&ev).contains("review passed"));
    let bodies = server.await.unwrap();
    assert_eq!(bodies.len(), 6);
    assert!(bodies[3].contains("[magia: continue]"));
    assert!(bodies[3].contains("create x.txt and check it"));
    assert!(
        bodies[3].contains("Magia mode"),
        "instructions in the system prompt"
    );
    assert!(bodies[5].contains("independent reviewer"));
    assert!(bodies[5].contains("x.txt"));
    assert!(
        !bodies[5].contains("Magia mode"),
        "the reviewer is not a Magia agent"
    );
    // One request, one rewind: every round goes, and the file with it.
    agent.rewind(1).unwrap();
    assert!(agent.history.is_empty());
    assert!(!dir.path().join("x.txt").exists());
}

#[tokio::test]
async fn magia_reopens_the_work_when_the_review_fails() {
    let dir = tempfile::tempdir().unwrap();
    let (url, server) = serve(vec![
        tool_reply(&[
            ("w1", "write", r#"{"file_path":"x.txt","content":""}"#),
            ("t1", "todo", &todos(&[("write x", "completed")])),
        ]),
        text_reply("done"),
        text_reply(r#"{"verdict":"fail","issues":["x.txt is empty"]}"#),
        text_reply("The file is meant to be empty."), // no tool calls: stop
    ])
    .await;
    let mut agent = magia_agent(url, dir.path(), PermissionMode::Yolo);
    let ev = run(&mut agent, "create x.txt").await;
    no_errors(&ev);
    assert_eq!(rounds(&ev), vec![(2, "review found 1 problem(s)".into())]);
    let bodies = server.await.unwrap();
    assert!(bodies[3].contains("x.txt is empty"));
}

#[tokio::test]
async fn magia_stops_when_the_model_only_talks() {
    let dir = tempfile::tempdir().unwrap();
    let (url, server) = serve(vec![
        tool_reply(&[("t1", "todo", &todos(&[("pick a db", "pending")]))]),
        text_reply("Starting."),
        text_reply("Which database should I use?"),
    ])
    .await;
    let mut agent = magia_agent(url, dir.path(), PermissionMode::Yolo);
    let ev = run(&mut agent, "set up storage").await;
    no_errors(&ev);
    assert_eq!(rounds(&ev).len(), 1);
    assert_eq!(server.await.unwrap().len(), 3);
}

#[tokio::test]
async fn magia_gives_up_after_rounds_without_progress() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), "a").unwrap();
    let read = || tool_reply(&[("r", "read", r#"{"file_path":"a.txt"}"#)]);
    let mut replies = vec![
        tool_reply(&[("t1", "todo", &todos(&[("task", "pending")]))]),
        text_reply("on it"),
    ];
    for _ in 0..3 {
        replies.push(read());
        replies.push(text_reply("still thinking"));
    }
    let (url, server) = serve(replies).await;
    let mut agent = magia_agent(url, dir.path(), PermissionMode::Yolo);
    let ev = run(&mut agent, "do the task").await;
    no_errors(&ev);
    assert_eq!(
        rounds(&ev).iter().map(|r| r.0).collect::<Vec<_>>(),
        vec![2, 3, 4]
    );
    assert!(notices(&ev).contains("without progress"));
    assert_eq!(server.await.unwrap().len(), 8);
}

#[tokio::test]
async fn magia_stops_when_the_user_denies_a_tool() {
    let dir = tempfile::tempdir().unwrap();
    let (url, server) = serve(vec![
        tool_reply(&[
            ("t1", "todo", &todos(&[("deploy", "pending")])),
            ("b1", "bash", r#"{"command":"make deploy"}"#),
        ]),
        text_reply("I could not deploy."),
    ])
    .await;
    // Ask mode, and the approver says no.
    let mut agent = magia_agent(url, dir.path(), PermissionMode::Ask);
    let ev = run(&mut agent, "deploy it").await;
    no_errors(&ev);
    assert!(rounds(&ev).is_empty());
    assert!(notices(&ev).contains("denied"));
    assert_eq!(server.await.unwrap().len(), 2);
}

#[tokio::test]
async fn magia_turns_the_step_limit_into_a_new_round() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), "a").unwrap();
    let mut replies: Vec<String> = (0..50)
        .map(|_| tool_reply(&[("r", "read", r#"{"file_path":"a.txt"}"#)]))
        .collect();
    replies.push(text_reply("finished reading"));
    let (url, server) = serve(replies).await;
    let mut agent = magia_agent(url, dir.path(), PermissionMode::Yolo);
    let ev = run(&mut agent, "read a lot").await;
    no_errors(&ev);
    assert_eq!(
        rounds(&ev),
        vec![(2, "step limit reached, continuing".into())]
    );
    assert_eq!(server.await.unwrap().len(), 51);
}

#[tokio::test]
async fn magia_ignores_open_tasks_left_by_an_earlier_request() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), "a").unwrap();
    let (url, server) = serve(vec![
        tool_reply(&[("r", "read", r#"{"file_path":"a.txt"}"#)]),
        text_reply("a.txt says a"),
    ])
    .await;
    let mut agent = magia_agent(url, dir.path(), PermissionMode::Yolo);
    agent.cx.todos.lock().unwrap().push(magia_tools::Todo {
        content: "old work".into(),
        status: "pending".into(),
    });
    let ev = run(&mut agent, "what is in a.txt?").await;
    no_errors(&ev);
    assert!(rounds(&ev).is_empty());
    assert_eq!(server.await.unwrap().len(), 2);
}

#[tokio::test]
async fn build_and_plan_stop_with_open_tasks() {
    for (magia, mode) in [(false, PermissionMode::Yolo), (true, PermissionMode::Plan)] {
        let dir = tempfile::tempdir().unwrap();
        let (url, server) = serve(vec![
            tool_reply(&[("t1", "todo", &todos(&[("task", "pending")]))]),
            text_reply("planned"),
        ])
        .await;
        let mut agent = magia_agent(url, dir.path(), mode);
        if !magia {
            agent.magia = None;
        }
        let ev = run(&mut agent, "plan the task").await;
        no_errors(&ev);
        assert!(rounds(&ev).is_empty());
        let bodies = server.await.unwrap();
        assert_eq!(bodies.len(), 2);
        assert!(!bodies[0].contains("Magia mode"));
    }
}

// ───────────── retry after a failed request ─────────────

fn retry_agent(url: String, dir: &std::path::Path) -> Agent {
    let mut a = Agent::new(client(url), "m".into(), PermissionMode::Ask, dir.into());
    a.retry_delay = std::time::Duration::from_millis(10);
    a
}

fn retries(ev: &[AgentEvent]) -> Vec<(u32, bool)> {
    ev.iter()
        .filter_map(|e| match e {
            AgentEvent::Retry {
                attempt, discarded, ..
            } => Some((*attempt, *discarded)),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn server_errors_are_retried_and_the_answer_arrives() {
    let dir = tempfile::tempdir().unwrap();
    let e500 =
        "HTTP/1.1 500 Internal Server Error\r\ncontent-length: 4\r\nconnection: close\r\n\r\noops"
            .to_string();
    let e429 =
        "HTTP/1.1 429 Too Many Requests\r\ncontent-length: 4\r\nconnection: close\r\n\r\nslow"
            .to_string();
    let (url, server) = serve(vec![e500, e429, text_reply("hello")]).await;
    let mut agent = retry_agent(url, dir.path());
    let ev = run(&mut agent, "hi").await;
    assert!(
        !ev.iter().any(|e| matches!(e, AgentEvent::Error(_))),
        "{ev:?}"
    );
    assert_eq!(retries(&ev), vec![(1, false), (2, false)]);
    assert_eq!(agent.history.len(), 2);
    assert_eq!(server.await.unwrap().len(), 3);
}

#[tokio::test]
async fn a_stream_cut_off_midway_is_retried_and_the_partial_text_discarded() {
    let dir = tempfile::tempdir().unwrap();
    // Text arrives, then the stream ends with no finish_reason and no [DONE].
    let cut = sse(&[r#"{"choices":[{"delta":{"content":"half an ans"}}]}"#]);
    let error_event = sse(&[r#"{"error":{"message":"upstream overloaded"}}"#]);
    let (url, _server) = serve(vec![cut, error_event, text_reply("whole answer")]).await;
    let mut agent = retry_agent(url, dir.path());
    let ev = run(&mut agent, "hi").await;
    assert!(
        !ev.iter().any(|e| matches!(e, AgentEvent::Error(_))),
        "{ev:?}"
    );
    assert_eq!(retries(&ev), vec![(1, true), (2, false)]);
    assert!(matches!(
        agent.history.last(),
        Some(Message::Assistant { text, .. }) if text == "whole answer"
    ));
    assert_eq!(agent.history.len(), 2, "the partial text is not kept");
}

#[tokio::test]
async fn client_errors_are_not_retried() {
    let dir = tempfile::tempdir().unwrap();
    let e400 =
        "HTTP/1.1 400 Bad Request\r\ncontent-length: 3\r\nconnection: close\r\n\r\nbad".to_string();
    let (url, _server) = serve(vec![e400]).await;
    let mut agent = retry_agent(url, dir.path());
    let ev = run(&mut agent, "hi").await;
    assert!(retries(&ev).is_empty());
    assert!(ev.iter().any(|e| matches!(e, AgentEvent::Error(_))));
}
