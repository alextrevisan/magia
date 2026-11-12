//! Hooks: shell commands run on agent events. The event payload is passed as JSON on
//! stdin (plus `MAGIA_EVENT` / `MAGIA_TOOL` env vars). Exit code 2 from a `pre_tool` or
//! `user_prompt` hook blocks the action; stderr becomes the reason shown to the model.
//! Stdout of a `user_prompt` hook is appended to the prompt as extra context.

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
    /// Tool name to match (`pre_tool`/`post_tool`); omit or `*` for all.
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
                && m != t
            {
                continue;
            }
            let Ok(mut child) = tokio::process::Command::new("bash")
                .arg("-c")
                .arg(&h.command)
                .current_dir(cwd)
                .env("MAGIA_EVENT", event)
                .env("MAGIA_TOOL", tool.unwrap_or(""))
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .kill_on_drop(true)
                .spawn()
            else {
                continue;
            };
            if let Some(mut si) = child.stdin.take() {
                let _ = si.write_all(payload.to_string().as_bytes()).await;
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

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
