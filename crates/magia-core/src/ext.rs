//! Extensions that shape the prompt: skills, custom slash commands, `@file` mentions.

use anyhow::{Result, bail};
use async_trait::async_trait;
use magia_tools::{Tool, ToolCx};
use magia_types::Image;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

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
    /// The id, from the path (`release/SKILL.md` or `release.md` → `release`), as OpenCode.
    pub name: String,
    pub description: String,
    /// Shown after the name in the `/` list (`argument-hint`).
    pub args: String,
    /// The folder the skill's files sit in.
    pub dir: PathBuf,
    /// Its instructions: `SKILL.md`, or a flat `<id>.md` at a source's root.
    pub file: PathBuf,
    /// Offered to the model in the system prompt: it has a description and does not set
    /// `disable-model-invocation: true` (it can still be run as `/name`).
    pub for_model: bool,
}

/// `cwd` and its parents up to the project root (the nearest folder with `.git`), nearest
/// first. Without a repository, only `cwd`.
fn project_dirs(cwd: &Path) -> Vec<PathBuf> {
    let up: Vec<PathBuf> = cwd.ancestors().map(Path::to_path_buf).collect();
    match up.iter().position(|d| d.join(".git").exists()) {
        Some(root) => up[..=root].to_vec(),
        None => vec![cwd.to_path_buf()],
    }
}

fn home_dir() -> Option<PathBuf> {
    // Unit tests must not pick up the skills installed on the machine running them.
    if cfg!(test) {
        return None;
    }
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
}

/// Where definitions live, highest priority first: magia's own folders, then OpenCode's
/// order (project `.opencode`, global OpenCode, then the compatibility folders `extra`, each
/// project nearest-first before the global one). `kind` is `skills` or `commands`; OpenCode
/// still reads the singular `skill`/`command` too.
fn sources(cwd: &Path, kind: &str, compat: &[&str]) -> Vec<PathBuf> {
    let single = kind.trim_end_matches('s');
    let dirs = project_dirs(cwd);
    let home = home_dir();
    let mut out: Vec<PathBuf> = dirs.iter().map(|d| d.join(".magia").join(kind)).collect();
    out.push(user_config_dir().join(kind));
    for d in &dirs {
        out.push(d.join(".opencode").join(kind));
        out.push(d.join(".opencode").join(single));
    }
    if let Some(h) = &home {
        out.push(h.join(".config/opencode").join(kind));
        out.push(h.join(".config/opencode").join(single));
    }
    for c in compat {
        out.extend(dirs.iter().map(|d| d.join(c).join(kind)));
        if let Some(h) = &home {
            out.push(h.join(c).join(kind));
        }
    }
    out.dedup();
    out
}

/// Hidden entries and `node_modules` are not searched.
fn skipped(name: &std::ffi::OsStr) -> bool {
    let n = name.to_string_lossy();
    n.starts_with('.') || n == "node_modules"
}

/// The skills of one source: flat `.md` files at its root, and folders holding `SKILL.md`
/// at any depth up to six (a skill folder is not searched further).
fn skills_in(root: &Path, out: &mut Vec<(String, PathBuf, PathBuf)>) {
    fn walk(dir: &Path, depth: usize, out: &mut Vec<(String, PathBuf, PathBuf)>) {
        let Ok(rd) = std::fs::read_dir(dir) else {
            return;
        };
        let mut entries: Vec<_> = rd.flatten().filter(|e| !skipped(&e.file_name())).collect();
        entries.sort_by_key(|e| e.file_name());
        for e in entries {
            let p = e.path();
            if p.is_dir() {
                let skill = p.join("SKILL.md");
                if skill.is_file() {
                    out.push((e.file_name().to_string_lossy().into_owned(), p, skill));
                } else if depth < 6 {
                    walk(&p, depth + 1, out);
                }
            } else if depth == 0
                && p.extension().is_some_and(|x| x == "md")
                && let Some(stem) = p.file_stem()
            {
                out.push((stem.to_string_lossy().into_owned(), dir.to_path_buf(), p));
            }
        }
    }
    walk(root, 0, out);
}

/// Skills from magia's folders, OpenCode's (`.opencode/skills`, `~/.config/opencode/skills`)
/// and the compatibility folders OpenCode also reads (`.agents/skills`, `.claude/skills`, in
/// the project and its parents up to the repository root, and in the home folder). On the
/// same id the higher source wins, in that order.
pub fn discover_skills(cwd: &Path) -> Vec<Skill> {
    let mut out: Vec<Skill> = Vec::new();
    for root in sources(cwd, "skills", &[".agents", ".claude"]) {
        let mut found = Vec::new();
        skills_in(&root, &mut found);
        for (name, dir, file) in found {
            if out.iter().any(|s| s.name == name) {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(&file) else {
                continue;
            };
            let (f, _) = frontmatter(&text);
            let description = f.get("description").cloned().unwrap_or_default();
            let for_model = !description.is_empty()
                && f.get("disable-model-invocation").map(String::as_str) != Some("true");
            out.push(Skill {
                name,
                description,
                args: f.get("argument-hint").cloned().unwrap_or_default(),
                dir,
                file,
                for_model,
            });
        }
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// Command files (`<name>.md`, a subfolder adds to the name: `team/review.md` →
/// `team/review`), from magia's, OpenCode's and Claude Code's `commands` folders. On the same
/// name the higher source wins.
fn command_files(cwd: &Path) -> Vec<(String, PathBuf)> {
    fn walk(root: &Path, dir: &Path, depth: usize, out: &mut Vec<(String, PathBuf)>) {
        let Ok(rd) = std::fs::read_dir(dir) else {
            return;
        };
        let mut entries: Vec<_> = rd.flatten().filter(|e| !skipped(&e.file_name())).collect();
        entries.sort_by_key(|e| e.file_name());
        for e in entries {
            let p = e.path();
            if p.is_dir() {
                if depth < 6 {
                    walk(root, &p, depth + 1, out);
                }
            } else if p.extension().is_some_and(|x| x == "md")
                && let Ok(rel) = p.with_extension("").strip_prefix(root)
            {
                let name = rel
                    .components()
                    .map(|c| c.as_os_str().to_string_lossy())
                    .collect::<Vec<_>>()
                    .join("/");
                if !out.iter().any(|(n, _)| *n == name) {
                    out.push((name, p));
                }
            }
        }
    }
    let mut out = Vec::new();
    for root in sources(cwd, "commands", &[".claude"]) {
        walk(&root, &root, 0, &mut out);
    }
    out
}

/// A skill's instructions as the model should see them. Skills written for Claude Code use
/// `${CLAUDE_SKILL_DIR}` and `$CLAUDE_PROJECT_DIR`, filled in here, and lines `` !`cmd` `` whose
/// output Claude Code pastes in before the model reads it; magia asks the model to run them
/// first instead (through `bash`, so the usual permissions apply).
fn skill_text(skill: &Skill, body: &str, cwd: &Path) -> String {
    let dir = skill.dir.display().to_string();
    let project = cwd.display().to_string();
    let mut out = String::new();
    for line in body
        .replace("${CLAUDE_SKILL_DIR}", &dir)
        .replace("$CLAUDE_SKILL_DIR", &dir)
        .replace("${CLAUDE_PROJECT_DIR}", &project)
        .replace("$CLAUDE_PROJECT_DIR", &project)
        .lines()
    {
        match line
            .trim()
            .strip_prefix("!`")
            .and_then(|l| l.strip_suffix('`'))
        {
            Some(cmd) => out.push_str(&format!(
                "[Run first with bash and use its output here: `{cmd}`]\n"
            )),
            None => {
                out.push_str(line);
                out.push('\n');
            }
        }
    }
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
    async fn call(&self, input: Value, cx: &ToolCx) -> Result<String> {
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
        let text = tokio::fs::read_to_string(&s.file).await?;
        let (_, body) = frontmatter(&text);
        Ok(format!(
            "Skill directory: {}\n\n{}",
            s.dir.display(),
            skill_text(s, body, &cx.cwd)
        ))
    }
}

/// Expand `/name args` using a command file (`command_files`). `$ARGUMENTS` is replaced
/// by the args; without the placeholder the args are appended.
pub fn expand_command(
    cwd: &Path,
    input: &str,
    configured: &HashMap<String, String>,
) -> Option<String> {
    let rest = input.trim().strip_prefix('/')?;
    let (name, args) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
    if name.is_empty() || name.contains('.') || name.starts_with('/') {
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
    if let Some((_, file)) = command_files(cwd).into_iter().find(|(n, _)| n == name)
        && let Ok(text) = std::fs::read_to_string(file)
    {
        return Some(fill(frontmatter(&text).1));
    }
    if let Some(t) = configured.get(name) {
        return Some(fill(t));
    }
    // A skill invoked directly by the user: inline its instructions (no tool round-trip).
    let skill = discover_skills(cwd).into_iter().find(|s| s.name == name)?;
    let text = std::fs::read_to_string(&skill.file).ok()?;
    let (fields, body) = frontmatter(&text);
    let body = skill_text(&skill, body, cwd);
    let filled = body.contains("$ARGUMENTS");
    let body = body.replace("$ARGUMENTS", args);
    // The text after `/name` is the skill's input, not a task of its own: a small model given
    // a long skill and then a bare request tends to drop the skill and just do the request.
    let request = match (args.is_empty(), filled) {
        (true, _) => "Follow this skill now, step by step.".to_owned(),
        (false, true) => {
            "Follow this skill now, step by step (its arguments are filled in above).".to_owned()
        }
        (false, false) => {
            let hint = fields
                .get("argument-hint")
                .map(|h| format!(" (it expects: {h})"))
                .unwrap_or_default();
            format!(
                "Follow this skill now, step by step. The user's arguments for it{hint}:\n\n\
                 {args}\n\nThey are the skill's input, not a separate task: do what the skill \
                 says with them. If they do not fit what it expects, or what it needs is \
                 missing, say so as the skill instructs instead of doing something else."
            )
        }
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
    /// Prompt template (`.magia/commands/*.md` or `command` in the config).
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
    for (name, f) in command_files(cwd) {
        let Ok(text) = std::fs::read_to_string(&f) else {
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
                name,
                description,
                kind: SlashKind::Command,
                args,
            },
        );
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
                args: s.args,
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

/// Files and folders of the project for the `@` autocomplete: relative paths with `/`,
/// folders ending in `/`, hidden and git-ignored entries skipped, at most `limit` entries.
pub fn project_files(cwd: &Path, limit: usize) -> Vec<String> {
    let mut v = Vec::new();
    for e in ignore::WalkBuilder::new(cwd).build().flatten() {
        if v.len() >= limit {
            break;
        }
        let Ok(rel) = e.path().strip_prefix(cwd) else {
            continue;
        };
        if rel.as_os_str().is_empty() {
            continue;
        }
        let mut s = rel
            .components()
            .map(|c| c.as_os_str().to_string_lossy())
            .collect::<Vec<_>>()
            .join("/");
        if s.contains(char::is_whitespace) {
            continue; // an `@mention` ends at whitespace
        }
        if e.file_type().is_some_and(|t| t.is_dir()) {
            s.push('/');
        }
        v.push(s);
    }
    v.sort();
    v
}

/// Paths matching what was typed after `@`, best first: file name starts with it, path starts
/// with it, path contains it, then letters in order; shorter paths first within each group.
/// Case-insensitive; at most `max` results.
pub fn filter_files<'a>(files: &'a [String], query: &str, max: usize) -> Vec<&'a String> {
    let q = query.to_lowercase();
    let mut scored: Vec<(u8, usize, &String)> = files
        .iter()
        .filter_map(|f| {
            let path = f.to_lowercase();
            let name = path.trim_end_matches('/').rsplit('/').next().unwrap_or("");
            let score = if q.is_empty() || path.starts_with(&q) || name.starts_with(&q) {
                0
            } else if path.contains(&q) {
                1
            } else if is_subsequence(&q, &path) {
                2
            } else {
                return None;
            };
            let depth = f.trim_end_matches('/').matches('/').count();
            Some((score, if q.is_empty() { depth } else { f.len() }, f))
        })
        .collect();
    scored.sort_by(|a, b| (a.0, a.1, a.2).cmp(&(b.0, b.1, b.2)));
    scored.into_iter().take(max).map(|(_, _, f)| f).collect()
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
    #[test]
    fn files_are_listed_relative_and_filtered_by_name_first() {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(d.path().join("src/ui")).unwrap();
        for f in [
            "src/main.rs",
            "src/ui/menu.rs",
            "README.md",
            "domain.rs",
            ".hidden",
        ] {
            std::fs::write(d.path().join(f), "").unwrap();
        }
        let files = super::project_files(d.path(), 100);
        assert_eq!(
            files,
            [
                "README.md",
                "domain.rs",
                "src/",
                "src/main.rs",
                "src/ui/",
                "src/ui/menu.rs"
            ]
        );
        let f = |q| super::filter_files(&files, q, 10);
        assert_eq!(
            f("ma"),
            ["src/main.rs", "domain.rs"],
            "name prefix before substring"
        );
        assert_eq!(f("src/u"), ["src/ui/", "src/ui/menu.rs"]);
        assert_eq!(f("smr"), ["src/main.rs", "src/ui/menu.rs"]);
        assert_eq!(
            f("")[..3],
            ["README.md", "domain.rs", "src/"],
            "top level first"
        );
        assert!(f("zzz").is_empty());
    }

    use super::*;

    #[test]
    fn frontmatter_and_commands() {
        let (f, body) = frontmatter("---\nname: x\ndescription: \"does y\"\n---\nbody here");
        assert_eq!(f["name"], "x");
        assert_eq!(f["description"], "does y");
        assert_eq!(body, "body here");

        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(d.path().join(".magia/commands")).unwrap();
        std::fs::write(
            d.path().join(".magia/commands/fix.md"),
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
        std::fs::create_dir_all(d.path().join(".magia/commands")).unwrap();
        std::fs::write(
            d.path().join(".magia/commands/review.md"),
            "---\ndescription: Review the diff\nargument-hint: [path]\n---\nReview $ARGUMENTS",
        )
        .unwrap();
        std::fs::write(
            d.path().join(".magia/commands/clear.md"),
            "shadowed by the builtin",
        )
        .unwrap();
        std::fs::write(
            d.path().join(".magia/commands/notes.md"),
            "# Summarize notes\nmore",
        )
        .unwrap();
        std::fs::create_dir_all(d.path().join(".magia/skills/deploy")).unwrap();
        std::fs::write(
            d.path().join(".magia/skills/deploy/SKILL.md"),
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
            out.contains("Run make deploy")
                && out.contains("arguments for it")
                && out.contains("\n\nstaging\n\n")
                && out.contains("not a separate task"),
            "{out}"
        );
    }

    #[test]
    fn skills_and_commands_follow_opencode_folders() {
        let d = tempfile::tempdir().unwrap();
        let root = d.path();
        let w = |rel: &str, text: &str| {
            let p = root.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, text).unwrap();
        };
        std::fs::create_dir_all(root.join(".git")).unwrap();
        let sub = root.join("game/level");
        std::fs::create_dir_all(&sub).unwrap();
        // At the repository root, found from a subfolder.
        w(
            ".claude/skills/adopt/SKILL.md",
            "---\ndescription: audit\nargument-hint: [focus]\n---\nx",
        );
        // Grouping folders; the id comes from the folder, not `name`.
        w(
            ".agents/skills/team/release/SKILL.md",
            "---\nname: Release It\ndescription: ship\n---\nx",
        );
        // A flat file at a source's root, OpenCode's singular folder.
        w(
            ".opencode/skill/notes.md",
            "---\ndescription: notes\n---\nx",
        );
        // No description, or hidden: runnable as /name, not offered to the model.
        w(".opencode/skills/quiet/SKILL.md", "x");
        w(
            ".opencode/skills/manual/SKILL.md",
            "---\ndescription: m\ndisable-model-invocation: true\n---\nx",
        );
        // The nearer folder wins over the repository root's.
        w(
            "game/level/.claude/skills/adopt/SKILL.md",
            "---\ndescription: near\n---\nx",
        );
        // Skipped: hidden folders inside a source.
        w(
            ".claude/skills/.cache/old/SKILL.md",
            "---\ndescription: no\n---\nx",
        );
        let s = discover_skills(&sub);
        let get = |n: &str| s.iter().find(|s| s.name == n);
        assert_eq!(get("adopt").unwrap().description, "near");
        assert_eq!(get("release").unwrap().description, "ship");
        assert_eq!(get("notes").unwrap().description, "notes");
        assert!(get("old").is_none());
        assert!(!get("quiet").unwrap().for_model && !get("manual").unwrap().for_model);
        assert!(get("release").unwrap().for_model);

        // Commands: Claude Code's and OpenCode's folders, subfolders in the name.
        w(
            ".claude/commands/fix.md",
            "---\ndescription: fix it\n---\nFix $ARGUMENTS",
        );
        w(".opencode/commands/team/review.md", "Review $ARGUMENTS");
        w(".opencode/command/old.md", "Old one");
        let cfg = crate::Config::default();
        let names: Vec<String> = slash_catalog(&sub, &[], &cfg)
            .into_iter()
            .map(|e| e.name)
            .collect();
        for n in ["fix", "team/review", "old", "adopt", "release"] {
            assert!(names.contains(&n.to_owned()), "{n} in {names:?}");
        }
        let empty = HashMap::new();
        assert_eq!(
            expand_command(&sub, "/fix the bug", &empty).unwrap(),
            "Fix the bug"
        );
        assert_eq!(
            expand_command(&sub, "/team/review src", &empty).unwrap(),
            "Review src"
        );
        assert!(expand_command(&sub, "/../x", &empty).is_none());

        // A skill written for Claude Code: its folder and the commands to run first.
        w(
            ".claude/skills/epics/SKILL.md",
            "---\ndescription: e\n---\n!`bash \"${CLAUDE_SKILL_DIR}/../../hooks/y.sh\" --keys a`\nLayer: $ARGUMENTS",
        );
        let out = expand_command(&sub, "/epics feature", &empty).unwrap();
        let dir = root.join(".claude/skills/epics").display().to_string();
        assert!(
            out.contains(&format!("[Run first with bash and use its output here: `bash \"{dir}/../../hooks/y.sh\" --keys a`]")),
            "{out}"
        );
        assert!(out.contains("Layer: feature"), "{out}");
    }

    #[test]
    fn skills_discovered_and_mentions_inlined() {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(d.path().join(".magia/skills/deploy")).unwrap();
        std::fs::write(
            d.path().join(".magia/skills/deploy/SKILL.md"),
            "---\nname: deploy\ndescription: ship it\n---\nsteps",
        )
        .unwrap();
        let s = discover_skills(d.path());
        assert!(
            s.iter()
                .any(|s| s.name == "deploy" && s.description == "ship it")
        );
        // Installed for Claude Code: found too; magia's own copy of a name wins.
        for (dir, text) in [
            (
                ".claude/skills/adopt",
                "---\nname: adopt\ndescription: \"audit it\"\n---\nx",
            ),
            (
                ".claude/skills/deploy",
                "---\nname: deploy\ndescription: other\n---\nx",
            ),
        ] {
            std::fs::create_dir_all(d.path().join(dir)).unwrap();
            std::fs::write(d.path().join(dir).join("SKILL.md"), text).unwrap();
        }
        let s = discover_skills(d.path());
        assert!(
            s.iter()
                .any(|s| s.name == "adopt" && s.description == "audit it")
        );
        let deploy: Vec<_> = s.iter().filter(|s| s.name == "deploy").collect();
        assert_eq!(
            (deploy.len(), deploy[0].description.as_str()),
            (1, "ship it")
        );

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
