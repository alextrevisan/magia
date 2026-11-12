//! Primary agents, as in OpenCode's agent switcher: `build` (the default), `plan` (read-only)
//! and one agent per Markdown file in the project root (`AGENTS.md`, `FABLE.md`, …).
//!
//! A file agent adds its text to the system prompt after magia's own; it does not replace it,
//! so tool use and the session details keep working. A leading `---` frontmatter is dropped
//! (its `description` is shown in the picker).

use crate::ext::frontmatter;
use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};

pub const BUILD: &str = "build";
pub const PLAN: &str = "plan";

/// Root files that are documentation, not instructions.
const NOT_AGENTS: &[&str] = &[
    "readme",
    "changelog",
    "changes",
    "history",
    "news",
    "license",
    "licence",
    "copying",
    "notice",
    "authors",
    "contributors",
    "contributing",
    "code_of_conduct",
    "security",
    "support",
];

/// A file bigger than this is almost certainly not meant to go into every request.
const MAX_BYTES: u64 = 2 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileAgent {
    /// File name, which is also the agent's name in the picker (`FABLE.md`).
    pub name: String,
    pub path: PathBuf,
    pub description: String,
    pub bytes: u64,
}

impl FileAgent {
    /// Rough prompt cost (≈ 4 bytes per token), for the picker.
    pub fn approx_tokens(&self) -> u64 {
        self.bytes / 4
    }
}

/// `*.md` files in `cwd` (not in subfolders), minus README/LICENSE/… , sorted by name.
pub fn discover_agents(cwd: &Path) -> Vec<FileAgent> {
    let Ok(rd) = std::fs::read_dir(cwd) else {
        return vec![];
    };
    let mut out: Vec<FileAgent> = rd
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            let stem = name
                .strip_suffix(".md")
                .or_else(|| name.strip_suffix(".MD"))?;
            if stem.is_empty() || NOT_AGENTS.contains(&stem.to_ascii_lowercase().as_str()) {
                return None;
            }
            let meta = e.metadata().ok().filter(|m| m.is_file())?;
            if meta.len() == 0 || meta.len() > MAX_BYTES {
                return None;
            }
            let description = std::fs::read_to_string(e.path())
                .ok()
                .and_then(|t| frontmatter(&t).0.get("description").cloned())
                .unwrap_or_default();
            Some(FileAgent {
                name,
                path: e.path(),
                description,
                bytes: meta.len(),
            })
        })
        .collect();
    out.sort_by_key(|a| a.name.to_lowercase());
    out
}

/// The agent's instructions: the file without its frontmatter.
pub fn load_agent(agent: &FileAgent) -> Result<String> {
    let text = std::fs::read_to_string(&agent.path)
        .with_context(|| format!("reading {}", agent.path.display()))?;
    let body = frontmatter(&text).1.trim();
    if body.is_empty() {
        bail!("{} has no instructions", agent.name);
    }
    Ok(body.to_owned())
}

/// Find a file agent by file name, case-insensitively, with or without `.md`.
pub fn find_agent(cwd: &Path, name: &str) -> Option<FileAgent> {
    let want = name.trim().to_lowercase();
    let want = want.strip_suffix(".md").unwrap_or(&want).to_owned();
    discover_agents(cwd).into_iter().find(|a| {
        a.name
            .to_lowercase()
            .strip_suffix(".md")
            .is_some_and(|s| s == want)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn root_markdown_files_are_agents_except_docs() {
        let d = tempfile::tempdir().unwrap();
        let w = |n: &str, t: &str| std::fs::write(d.path().join(n), t).unwrap();
        w("AGENTS.md", "be brief");
        w(
            "FABLE.md",
            "---\ndescription: Fable persona\n---\nyou are fable",
        );
        w("README.md", "docs");
        w("LICENSE.md", "mit");
        w("empty.md", "");
        w("notes.txt", "no");
        std::fs::create_dir(d.path().join("sub.md")).unwrap();
        std::fs::create_dir(d.path().join("docs")).unwrap();
        std::fs::write(d.path().join("docs/DEEP.md"), "x").unwrap();

        let a = discover_agents(d.path());
        let names: Vec<_> = a.iter().map(|a| a.name.as_str()).collect();
        assert_eq!(names, ["AGENTS.md", "FABLE.md"]);
        assert_eq!(a[1].description, "Fable persona");
        assert_eq!(load_agent(&a[1]).unwrap(), "you are fable");
        assert_eq!(find_agent(d.path(), "fable").unwrap().name, "FABLE.md");
        assert_eq!(find_agent(d.path(), "Fable.md").unwrap().name, "FABLE.md");
        assert!(find_agent(d.path(), "readme").is_none());
    }
}
