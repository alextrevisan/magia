//! What magia remembers per project folder, outside the user's config files (which are never
//! rewritten): its reference folders. Kept in `~/.local/state/magia/projects.json` as
//! `{ "<project>": { "references": ["<folder>", …] } }`.
//!
//! A reference folder is read without asking; writing there (or running commands on it) asks
//! like any folder outside the project.

use anyhow::Result;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

fn file() -> PathBuf {
    crate::config::state_dir().join("projects.json")
}

fn load_from(file: &Path) -> Value {
    std::fs::read_to_string(file)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_else(|| json!({}))
}

fn key(project: &Path) -> String {
    magia_store::canonical(project).display().to_string()
}

/// Reference folders of `project` (existing or not; the caller decides what to show).
pub fn references(project: &Path) -> Vec<PathBuf> {
    references_in(&file(), project)
}

fn references_in(file: &Path, project: &Path) -> Vec<PathBuf> {
    load_from(file)
        .get(key(project))
        .and_then(|p| p.get("references"))
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(PathBuf::from))
                .collect()
        })
        .unwrap_or_default()
}

/// Every project that has reference folders, with them.
pub fn all_references() -> Vec<(PathBuf, Vec<PathBuf>)> {
    let v = load_from(&file());
    let Some(o) = v.as_object() else {
        return vec![];
    };
    o.iter()
        .filter_map(|(k, p)| {
            let refs: Vec<PathBuf> = p
                .get("references")?
                .as_array()?
                .iter()
                .filter_map(|v| v.as_str().map(PathBuf::from))
                .collect();
            (!refs.is_empty()).then(|| (PathBuf::from(k), refs))
        })
        .collect()
}

/// Replace the reference folders of `project`.
pub fn set_references(project: &Path, refs: &[PathBuf]) -> Result<()> {
    set_in(&file(), project, refs)
}

fn set_in(path: &Path, project: &Path, refs: &[PathBuf]) -> Result<()> {
    let mut v = load_from(path);
    if !v.is_object() {
        v = json!({});
    }
    let k = key(project);
    let o = v.as_object_mut().expect("an object");
    if refs.is_empty() {
        if let Some(p) = o.get_mut(&k).and_then(Value::as_object_mut) {
            p.remove("references");
            if p.is_empty() {
                o.remove(&k);
            }
        }
    } else {
        let list: Vec<String> = refs.iter().map(|r| r.display().to_string()).collect();
        let entry = o.entry(k).or_insert_with(|| json!({}));
        if !entry.is_object() {
            *entry = json!({});
        }
        entry["references"] = json!(list);
    }
    if let Some(d) = path.parent() {
        std::fs::create_dir_all(d)?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(&v)?)?;
    std::fs::rename(tmp, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn references_are_kept_per_project() {
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join("state/projects.json");
        let (a, b) = (d.path().join("a"), d.path().join("b"));
        std::fs::create_dir_all(&a).unwrap();
        assert!(references_in(&f, &a).is_empty());
        set_in(
            &f,
            &a,
            &[PathBuf::from("/ref/one"), PathBuf::from("/ref/two")],
        )
        .unwrap();
        set_in(&f, &b, &[PathBuf::from("/ref/b")]).unwrap();
        assert_eq!(
            references_in(&f, &a),
            vec![PathBuf::from("/ref/one"), PathBuf::from("/ref/two")]
        );
        set_in(&f, &a, &[]).unwrap();
        assert!(references_in(&f, &a).is_empty());
        assert_eq!(references_in(&f, &b), vec![PathBuf::from("/ref/b")]);
        assert!(
            !std::fs::read_to_string(&f).unwrap().contains("/ref/one"),
            "removed from the file"
        );
    }
}
