//! Extensions that shape the prompt: skills, custom slash commands, `@file` mentions.

use anyhow::{Result, bail};
use async_trait::async_trait;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use ufrh_tools::{Tool, ToolCx};

fn user_config_dir() -> PathBuf {
    crate::config::config_path()
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_default()
}

/// Split `---\nkey: value\n---\nbody` into (fields, body).
pub fn frontmatter(text: &str) -> (HashMap<String, String>, &str) {
    let mut fields = HashMap::new();
    let Some(rest) = text.strip_prefix("---\n") else {
        return (fields, text);
    };
    let Some(end) = rest.find("\n---") else {
        return (fields, text);
    };
    for line in rest[..end].lines() {
        if let Some((k, v)) = line.split_once(':') {
            fields.insert(k.trim().to_owned(), v.trim().trim_matches('"').to_owned());
        }
    }
    let body = rest[end + 4..].trim_start_matches('\n');
    (fields, body)
}

#[derive(Debug, Clone)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub dir: PathBuf,
}

/// `.ufrh/skills/<name>/SKILL.md` in the project, then `~/.config/ufrh/skills/…`.
pub fn discover_skills(cwd: &Path) -> Vec<Skill> {
    let mut out: Vec<Skill> = Vec::new();
    for root in [cwd.join(".ufrh/skills"), user_config_dir().join("skills")] {
        let Ok(rd) = std::fs::read_dir(&root) else {
            continue;
        };
        for e in rd.flatten() {
            let Ok(text) = std::fs::read_to_string(e.path().join("SKILL.md")) else {
                continue;
            };
            let (f, _) = frontmatter(&text);
            let name = f
                .get("name")
                .cloned()
                .unwrap_or_else(|| e.file_name().to_string_lossy().into_owned());
            if out.iter().any(|s| s.name == name) {
                continue; // project skills win over user skills
            }
            out.push(Skill {
                name,
                description: f.get("description").cloned().unwrap_or_default(),
                dir: e.path(),
            });
        }
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// Loads a skill's instructions on demand (only the name+description sit in the prompt).
pub struct SkillTool(pub Vec<Skill>);

#[async_trait]
impl Tool for SkillTool {
    fn name(&self) -> &'static str {
        "skill"
    }
    fn description(&self) -> &'static str {
        "Load the full instructions of a skill by name (see the skill list in the system prompt)."
    }
    fn schema(&self) -> Value {
        json!({"type":"object","properties":{"name":{"type":"string"}},"required":["name"]})
    }
    fn read_only(&self) -> bool {
        true
    }
    async fn call(&self, input: Value, _cx: &ToolCx) -> Result<String> {
        let name = input.get("name").and_then(Value::as_str).unwrap_or("");
        let Some(s) = self.0.iter().find(|s| s.name == name) else {
            bail!(
                "unknown skill `{name}`; available: {}",
                self.0
                    .iter()
                    .map(|s| s.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        };
        let text = tokio::fs::read_to_string(s.dir.join("SKILL.md")).await?;
        let (_, body) = frontmatter(&text);
        Ok(format!("Skill directory: {}\n\n{body}", s.dir.display()))
    }
}

/// Expand `/name args` using `.ufrh/commands/name.md` (or the user dir). `$ARGUMENTS` is replaced
/// by the args; without the placeholder the args are appended.
pub fn expand_command(cwd: &Path, input: &str) -> Option<String> {
    let rest = input.trim().strip_prefix('/')?;
    let (name, args) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
    if name.is_empty() || name.contains(['/', '.']) {
        return None;
    }
    for root in [
        cwd.join(".ufrh/commands"),
        user_config_dir().join("commands"),
    ] {
        if let Ok(text) = std::fs::read_to_string(root.join(format!("{name}.md"))) {
            let (_, body) = frontmatter(&text);
            let args = args.trim();
            return Some(if body.contains("$ARGUMENTS") {
                body.replace("$ARGUMENTS", args)
            } else if args.is_empty() {
                body.to_owned()
            } else {
                format!("{body}\n\n{args}")
            });
        }
    }
    None
}

/// Inline the content of `@path` mentions (files ≤ 64 KB) and mark them as read, so the
/// model can edit them without an extra `read` round-trip.
pub fn expand_mentions(cx: &ToolCx, prompt: &str) -> String {
    let mut extra = String::new();
    let mut seen = Vec::new();
    for tok in prompt.split_whitespace() {
        let Some(p) = tok.strip_prefix('@') else {
            continue;
        };
        let p = p.trim_end_matches([',', '.', ';', ':', ')', '?', '!']);
        if p.is_empty() || seen.contains(&p) {
            continue;
        }
        let path = cx.resolve(p);
        let Ok(meta) = std::fs::metadata(&path) else {
            continue;
        };
        if !meta.is_file() || meta.len() > 64 * 1024 {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        seen.push(p);
        cx.read_files.lock().unwrap().insert(path);
        extra.push_str(&format!("\n\n<file path=\"{p}\">\n{text}\n</file>"));
    }
    format!("{prompt}{extra}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frontmatter_and_commands() {
        let (f, body) = frontmatter("---\nname: x\ndescription: \"does y\"\n---\nbody here");
        assert_eq!(f["name"], "x");
        assert_eq!(f["description"], "does y");
        assert_eq!(body, "body here");

        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(d.path().join(".ufrh/commands")).unwrap();
        std::fs::write(
            d.path().join(".ufrh/commands/fix.md"),
            "Fix: $ARGUMENTS now",
        )
        .unwrap();
        assert_eq!(
            expand_command(d.path(), "/fix the bug").unwrap(),
            "Fix: the bug now"
        );
        assert!(expand_command(d.path(), "/nope").is_none());
        assert!(expand_command(d.path(), "/../etc").is_none());
    }

    #[test]
    fn skills_discovered_and_mentions_inlined() {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(d.path().join(".ufrh/skills/deploy")).unwrap();
        std::fs::write(
            d.path().join(".ufrh/skills/deploy/SKILL.md"),
            "---\nname: deploy\ndescription: ship it\n---\nsteps",
        )
        .unwrap();
        let s = discover_skills(d.path());
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].description, "ship it");

        std::fs::write(d.path().join("a.txt"), "content-a").unwrap();
        let cx = ToolCx::new(d.path().to_path_buf());
        let out = expand_mentions(&cx, "look at @a.txt, and @missing.txt");
        assert!(out.contains("content-a") && !out.contains("missing.txt\">"));
        assert!(
            cx.read_files
                .lock()
                .unwrap()
                .contains(&d.path().join("a.txt"))
        );
    }
}
