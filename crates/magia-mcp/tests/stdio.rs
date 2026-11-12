use magia_mcp::{McpClient, McpServerConfig};
use magia_tools::ToolCx;
use serde_json::json;

const SERVER: &str = r#"
import sys, json
for line in sys.stdin:
    m = json.loads(line)
    if "id" not in m: continue
    method = m["method"]
    if method == "initialize":
        r = {"protocolVersion": "2024-11-05", "capabilities": {"tools": {}}, "serverInfo": {"name": "fake", "version": "0"}}
    elif method == "tools/list":
        r = {"tools": [{"name": "echo", "description": "echo text", "inputSchema": {"type": "object", "properties": {"text": {"type": "string"}}}}]}
    elif method == "tools/call":
        a = m["params"]["arguments"]
        r = {"content": [{"type": "text", "text": "echo: " + a.get("text", "")}], "isError": a.get("text") == "boom"}
    else:
        r = {}
    print(json.dumps({"jsonrpc": "2.0", "id": m["id"], "result": r}), flush=True)
"#;

#[tokio::test]
async fn discovers_and_calls_tools() {
    if std::process::Command::new("python3")
        .arg("--version")
        .output()
        .is_err()
    {
        return;
    }
    let cfg = McpServerConfig {
        name: "fake".into(),
        command: "python3".into(),
        args: vec!["-c".into(), SERVER.into()],
        env: Default::default(),
    };
    let client = McpClient::connect(&cfg).await.unwrap();
    let tools = client.tools().await.unwrap();
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0].name(), "mcp__fake__echo");
    let cx = ToolCx::new(std::env::temp_dir());
    let out = tools[0].call(json!({"text": "hi"}), &cx).await.unwrap();
    assert_eq!(out, "echo: hi");
    assert!(tools[0].call(json!({"text": "boom"}), &cx).await.is_err());
}
