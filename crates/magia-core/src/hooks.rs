//! Hooks: shell commands run on agent events. The event payload is passed as JSON on
//! stdin (plus `MAGIA_EVENT` / `MAGIA_TOOL` env vars). Exit code 2 from a `pre_tool` or
//! `user_prompt` hook blocks the action; stderr becomes the reason shown to the model.
//! Stdout of a `user_prompt` hook is appended to the prompt as extra context.
//!
//! Hooks written for Claude Code work too: the payload also carries `tool_name` (`Bash`,
//! `Read`, `Write`…), `tool_input`, `hook_event_name` and `cwd`, `CLAUDE_PROJECT_DIR` is set,
//! and a matcher may list tools as `Write|Edit` in either spelling.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;
use tokio::io::AsyncWriteExt;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HookConfig {
    /// `pre_tool` | `post_tool` | `user_prompt` | `stop`
    pub event: String,
    /// Tool name to match (`pre_tool`/`post_tool`), or several as `write|edit`; any case, so
    /// Claude Code's `Bash`/`Write` match too. Omit or `*` for all.
    #[serde(default)]
    pub matcher: Option<String>,
    pub command: String,
}

#[derive(Debug, Default)]
pub struct HookOutcome {
    pub blocked: Option<String>,
    pub output: String,
}

#[derive(Debug, Clone, Default)]
pub struct Hooks(pub Vec<HookConfig>);

impl Hooks {
    pub async fn run(
        &self,
        event: &str,
        tool: Option<&str>,
        payload: &Value,
        cwd: &Path,
    ) -> HookOutcome {
        let mut out = HookOutcome::default();
        for h in &self.0 {
            if h.event != event {
                continue;
            }
            if let (Some(m), Some(t)) = (&h.matcher, tool)
                && m != "*"
                && !m.split('|').any(|x| x.trim().eq_ignore_ascii_case(t))
            {
                continue;
            }
            let Ok(mut child) = tokio::process::Command::new(magia_tools::bash_program())
                .arg("-c")
                .arg(&h.command)
                .current_dir(cwd)
                .env("MAGIA_EVENT", event)
                .env("MAGIA_TOOL", tool.unwrap_or(""))
                .env("CLAUDE_PROJECT_DIR", cwd)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .kill_on_drop(true)
                .spawn()
            else {
                continue;
            };
            if let Some(mut si) = child.stdin.take() {
                let _ = si
                    .write_all(
                        claude_payload(event, tool, payload, cwd)
                            .to_string()
                            .as_bytes(),
                    )
                    .await;
            }
            let Ok(Ok(res)) =
                tokio::time::timeout(Duration::from_secs(30), child.wait_with_output()).await
            else {
                continue;
            };
            out.output.push_str(&String::from_utf8_lossy(&res.stdout));
            if res.status.code() == Some(2) {
                let reason = String::from_utf8_lossy(&res.stderr).trim().to_owned();
                out.blocked = Some(if reason.is_empty() {
                    "blocked by hook".into()
                } else {
                    reason
                });
                return out;
            }
        }
        out
    }
}

/// The payload with Claude Code's fields added, for hooks written for it.
fn claude_payload(event: &str, tool: Option<&str>, payload: &Value, cwd: &Path) -> Value {
    let mut v = payload.clone();
    let Some(o) = v.as_object_mut() else {
        return v;
    };
    let name = match event {
        "pre_tool" => "PreToolUse",
        "post_tool" => "PostToolUse",
        "user_prompt" => "UserPromptSubmit",
        "stop" => "Stop",
        other => other,
    };
    o.insert("hook_event_name".into(), name.into());
    o.insert("cwd".into(), cwd.display().to_string().into());
    if let Some(t) = tool {
        // `bash` → `Bash`: Claude Code's names for the same tools.
        let mut c = t.chars();
        let cap: String = c
            .next()
            .map(|f| f.to_uppercase().chain(c).collect())
            .unwrap_or_default();
        o.insert("tool_name".into(), cap.into());
        if let Some(i) = o.get("input").cloned() {
            o.insert("tool_input".into(), i);
        }
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn claude_code_hooks_get_their_fields_and_matchers() {
        let d = tempfile::tempdir().unwrap();
        // Blocks a write as a Claude Code hook would: reads tool_name / tool_input.file_path.
        let script = r#"in=$(cat); case "$in" in *'"tool_name":"Write"'*) ;; *) exit 0;; esac; case "$in" in *'"tool_input":{"file_path":"secret.txt"'*) echo "no: $CLAUDE_PROJECT_DIR" >&2; exit 2;; esac"#;
        let hooks = Hooks(vec![HookConfig {
            event: "pre_tool".into(),
            matcher: Some("Write|Edit".into()),
            command: script.into(),
        }]);
        let out = hooks
            .run(
                "pre_tool",
                Some("write"),
                &json!({"tool": "write", "input": {"file_path": "secret.txt"}}),
                d.path(),
            )
            .await;
        assert_eq!(out.blocked, Some(format!("no: {}", d.path().display())));
        let other = hooks
            .run(
                "pre_tool",
                Some("read"),
                &json!({"tool": "read", "input": {"file_path": "secret.txt"}}),
                d.path(),
            )
            .await;
        assert!(other.blocked.is_none(), "Read is not in the matcher");
    }

    #[tokio::test]
    async fn blocks_on_exit_2_and_matches_tool() {
        let hooks = Hooks(vec![
            HookConfig {
                event: "pre_tool".into(),
                matcher: Some("bash".into()),
                command: "echo no rm >&2; exit 2".into(),
            },
            HookConfig {
                event: "user_prompt".into(),
                matcher: None,
                command: "echo extra-context".into(),
            },
        ]);
        let cwd = std::env::temp_dir();
        let r = hooks.run("pre_tool", Some("bash"), &json!({}), &cwd).await;
        assert_eq!(r.blocked.as_deref(), Some("no rm"));
        assert!(
            hooks
                .run("pre_tool", Some("read"), &json!({}), &cwd)
                .await
                .blocked
                .is_none()
        );
        assert_eq!(
            hooks
                .run("user_prompt", None, &json!({}), &cwd)
                .await
                .output
                .trim(),
            "extra-context"
        );
    }
}
