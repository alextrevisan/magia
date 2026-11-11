//! Extensions that shape the prompt: skills, custom slash commands, `@file` mentions.

use anyhow::{Result, bail};
use async_trait::async_trait;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use ufrh_tools::{Tool, ToolCx};
use ufrh_types::Image;

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
pub fn expand_command(
    cwd: &Path,
    input: &str,
    configured: &HashMap<String, String>,
) -> Option<String> {
    let rest = input.trim().strip_prefix('/')?;
    let (name, args) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
    if name.is_empty() || name.contains(['/', '.']) {
        return None;
    }
    let args = args.trim();
    let fill = |body: &str| {
        if body.contains("$ARGUMENTS") {
            body.replace("$ARGUMENTS", args)
        } else if args.is_empty() {
            body.to_owned()
        } else {
            format!("{body}\n\n{args}")
        }
    };
    // Files win over commands defined in the JSON config (project-level customization).
    for root in [
        cwd.join(".ufrh/commands"),
        user_config_dir().join("commands"),
    ] {
        if let Ok(text) = std::fs::read_to_string(root.join(format!("{name}.md"))) {
            return Some(fill(frontmatter(&text).1));
        }
    }
    if let Some(t) = configured.get(name) {
        return Some(fill(t));
    }
    // A skill invoked directly by the user: inline its instructions (no tool round-trip).
    let skill = discover_skills(cwd).into_iter().find(|s| s.name == name)?;
    let text = std::fs::read_to_string(skill.dir.join("SKILL.md")).ok()?;
    let body = frontmatter(&text).1;
    let request = if args.is_empty() {
        "Apply this skill to the current task."
    } else {
        args
    };
    Some(format!(
        "Use the skill `{name}` (directory: {}).\n\n<skill>\n{body}\n</skill>\n\n{request}",
        skill.dir.display()
    ))
}

// ───────────── `/` catalog for autocomplete ─────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum SlashKind {
    /// Handled by the front-end itself (/clear, /compact…).
    Builtin,
    /// Prompt template (`.ufrh/commands/*.md` or `command` in the config).
    Command,
    Skill,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SlashEntry {
    pub name: String,
    pub description: String,
    pub kind: SlashKind,
    /// Short hint for arguments, e.g. `[n]`.
    pub args: String,
}

/// First non-empty line of a template, used when a command file has no `description`.
fn first_line(body: &str) -> String {
    let line = body
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("");
    let line = line.trim_start_matches('#').trim();
    if line.chars().count() > 80 {
        format!("{}…", line.chars().take(80).collect::<String>())
    } else {
        line.to_owned()
    }
}

/// Every `/` entry available in `cwd`, without duplicates. Priority on name clashes matches
/// how input is resolved: built-in, then command files (project, user), config commands, skills.
pub fn slash_catalog(
    cwd: &Path,
    builtins: &[(&str, &str, &str)],
    cfg: &crate::Config,
) -> Vec<SlashEntry> {
    let mut out: Vec<SlashEntry> = builtins
        .iter()
        .map(|(n, d, a)| SlashEntry {
            name: (*n).into(),
            description: (*d).into(),
            kind: SlashKind::Builtin,
            args: (*a).into(),
        })
        .collect();
    let push = |out: &mut Vec<SlashEntry>, e: SlashEntry| {
        if !out.iter().any(|x| x.name == e.name) {
            out.push(e);
        }
    };
    for root in [
        cwd.join(".ufrh/commands"),
        user_config_dir().join("commands"),
    ] {
        let Ok(rd) = std::fs::read_dir(&root) else {
            continue;
        };
        let mut files: Vec<PathBuf> = rd
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|x| x == "md"))
            .collect();
        files.sort();
        for f in files {
            let (Some(stem), Ok(text)) = (
                f.file_stem().map(|s| s.to_string_lossy().into_owned()),
                std::fs::read_to_string(&f),
            ) else {
                continue;
            };
            let (fields, body) = frontmatter(&text);
            let description = fields
                .get("description")
                .cloned()
                .unwrap_or_else(|| first_line(body));
            let args = fields.get("argument-hint").cloned().unwrap_or_default();
            push(
                &mut out,
                SlashEntry {
                    name: stem,
                    description,
                    kind: SlashKind::Command,
                    args,
                },
            );
        }
    }
    let mut cfg_cmds: Vec<(&String, &String)> = cfg.commands.iter().collect();
    cfg_cmds.sort();
    for (name, template) in cfg_cmds {
        let description = cfg
            .command_descriptions
            .get(name)
            .cloned()
            .unwrap_or_else(|| first_line(template));
        push(
            &mut out,
            SlashEntry {
                name: name.clone(),
                description,
                kind: SlashKind::Command,
                args: String::new(),
            },
        );
    }
    for s in discover_skills(cwd) {
        push(
            &mut out,
            SlashEntry {
                name: s.name,
                description: s.description,
                kind: SlashKind::Skill,
                args: String::new(),
            },
        );
    }
    out
}

fn is_subsequence(needle: &str, hay: &str) -> bool {
    let mut it = hay.chars();
    needle.chars().all(|c| it.any(|h| h == c))
}

/// Entries matching what was typed after `/`, best first: name prefix, then name substring,
/// then letters in order (`/cmp` → `compact`), then description substring (3+ letters).
/// Case-insensitive.
pub fn filter_slash<'a>(entries: &'a [SlashEntry], query: &str) -> Vec<&'a SlashEntry> {
    let q = query.trim().to_lowercase();
    let mut scored: Vec<(u8, usize, &SlashEntry)> = entries
        .iter()
        .enumerate()
        .filter_map(|(i, e)| {
            let name = e.name.to_lowercase();
            let score = if q.is_empty() || name.starts_with(&q) {
                0
            } else if name.contains(&q) {
                1
            } else if is_subsequence(&q, &name) {
                2
            } else if q.chars().count() >= 3 && e.description.to_lowercase().contains(&q) {
                3
            } else {
                return None;
            };
            Some((score, i, e))
        })
        .collect();
    scored.sort_by_key(|(s, i, _)| (*s, *i));
    scored.into_iter().map(|(_, _, e)| e).collect()
}

/// Inline the content of `@path` mentions (files ≤ 64 KB) and mark them as read, so the
/// model can edit them without an extra `read` round-trip.
/// Inline `@path` mentions: text files (≤ 64 KiB) are appended to the prompt, image files
/// (`.png`, `.jpg`, …) are returned as attachments.
pub fn expand_mentions(cx: &ToolCx, prompt: &str) -> (String, Vec<Image>) {
    let mut extra = String::new();
    let mut images = Vec::new();
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
        if !meta.is_file() {
            continue;
        }
        if crate::media::is_image_path(&path) {
            if let Ok(img) = crate::media::load_image_file(&path) {
                seen.push(p);
                images.push(img);
            }
            continue;
        }
        if meta.len() > 64 * 1024 {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        seen.push(p);
        cx.read_files.lock().unwrap().insert(path);
        extra.push_str(&format!("\n\n<file path=\"{p}\">\n{text}\n</file>"));
    }
    (format!("{prompt}{extra}"), images)
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
        let cfg: HashMap<String, String> =
            [("rev".to_owned(), "Review $ARGUMENTS".to_owned())].into();
        assert_eq!(
            expand_command(d.path(), "/fix the bug", &cfg).unwrap(),
            "Fix: the bug now"
        );
        assert_eq!(
            expand_command(d.path(), "/rev src/", &cfg).unwrap(),
            "Review src/"
        );
        assert!(expand_command(d.path(), "/nope", &cfg).is_none());
        assert!(expand_command(d.path(), "/../etc", &cfg).is_none());
    }

    #[test]
    fn catalog_merges_sources_without_duplicates_and_filters() {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(d.path().join(".ufrh/commands")).unwrap();
        std::fs::write(
            d.path().join(".ufrh/commands/review.md"),
            "---\ndescription: Review the diff\nargument-hint: [path]\n---\nReview $ARGUMENTS",
        )
        .unwrap();
        std::fs::write(
            d.path().join(".ufrh/commands/clear.md"),
            "shadowed by the builtin",
        )
        .unwrap();
        std::fs::write(
            d.path().join(".ufrh/commands/notes.md"),
            "# Summarize notes\nmore",
        )
        .unwrap();
        std::fs::create_dir_all(d.path().join(".ufrh/skills/deploy")).unwrap();
        std::fs::write(
            d.path().join(".ufrh/skills/deploy/SKILL.md"),
            "---\nname: deploy\ndescription: Ship to prod\n---\nRun make deploy",
        )
        .unwrap();
        let mut cfg = crate::Config::default();
        cfg.commands
            .insert("release".into(), "Cut a release".into());
        cfg.commands
            .insert("review".into(), "shadowed by the file".into());
        let builtins = [
            ("clear", "New session", ""),
            ("compact", "Summarize history", ""),
        ];

        let cat = slash_catalog(d.path(), &builtins, &cfg);
        let names: Vec<&str> = cat.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(
            names,
            ["clear", "compact", "notes", "review", "release", "deploy"]
        );
        let review = cat.iter().find(|e| e.name == "review").unwrap();
        assert_eq!(
            (review.description.as_str(), review.args.as_str()),
            ("Review the diff", "[path]")
        );
        assert_eq!(
            cat.iter().find(|e| e.name == "notes").unwrap().description,
            "Summarize notes"
        );
        assert_eq!(
            cat.iter().find(|e| e.name == "deploy").unwrap().kind,
            SlashKind::Skill
        );

        let names = |q: &str| {
            filter_slash(&cat, q)
                .iter()
                .map(|e| e.name.clone())
                .collect::<Vec<_>>()
        };
        assert_eq!(names("").len(), 6);
        assert_eq!(names("c"), ["clear", "compact"]);
        assert_eq!(
            names("RE"),
            ["review", "release"],
            "case-insensitive prefix"
        );
        assert_eq!(names("ar"), ["clear"], "substring");
        assert_eq!(names("cmp"), ["compact"], "letters in order");
        assert_eq!(names("prod"), ["deploy"], "description match");
        assert!(names("zzz").is_empty());

        // A skill typed as a command inlines its instructions.
        let out = expand_command(d.path(), "/deploy staging", &cfg.commands).unwrap();
        assert!(
            out.contains("Run make deploy") && out.ends_with("staging"),
            "{out}"
        );
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
        let (out, imgs) = expand_mentions(&cx, "look at @a.txt, and @missing.txt");
        assert!(out.contains("content-a") && !out.contains("missing.txt\">"));
        assert!(imgs.is_empty());
        let png = b"\x89PNG\r\n\x1a\n-not-really";
        std::fs::write(d.path().join("shot.png"), png).unwrap(); // invalid PNG: skipped
        assert!(expand_mentions(&cx, "see @shot.png").1.is_empty());
        std::fs::write(d.path().join("pic.jpg"), [0xFF, 0xD8, 0xFF, 0xE0, 0]).unwrap();
        let (out, imgs) = expand_mentions(&cx, "see @pic.jpg");
        assert_eq!(out, "see @pic.jpg");
        assert_eq!(imgs[0].media_type, "image/jpeg");
        assert!(
            cx.read_files
                .lock()
                .unwrap()
                .contains(&d.path().join("a.txt"))
        );
    }
}
