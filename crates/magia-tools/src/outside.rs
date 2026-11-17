//! Which folders outside the project a tool call would touch. Reaching outside the project
//! needs the user's permission (except in `yolo`); see `magia-core` for the prompt.
//!
//! File tools name their path directly. For `bash`, the arguments are read with the same
//! conservative parser as `auto` mode; a word counts only when it looks like a path (`/…`,
//! `~/…`, `$HOME/…`, `..`) and names something that exists, so `grep "/api/" src` does not ask.

use crate::auto::{parse, resolve};
use serde_json::Value;
use std::path::{Path, PathBuf};

/// Device files every shell command may use.
const HARMLESS: &[&str] = &[
    "/dev/null",
    "/dev/stdin",
    "/dev/stdout",
    "/dev/stderr",
    "/dev/tty",
    "/dev/zero",
    "/dev/urandom",
    "/dev/random",
];

/// Folders outside `cwd` (and outside every folder in `granted`) that the call would reach:
/// the folder itself for a directory, the parent folder for a file. Sorted, no duplicates.
pub fn folders(tool: &str, args: &Value, cwd: &Path, granted: &[PathBuf]) -> Vec<PathBuf> {
    let str_arg = |k: &str| args.get(k).and_then(Value::as_str);
    let paths: Vec<PathBuf> = match tool {
        "read" | "write" | "edit" => str_arg("file_path")
            .map(|p| vec![resolve(p, cwd)])
            .unwrap_or_default(),
        "ls" | "glob" | "grep" => str_arg("path")
            .map(|p| vec![resolve(p, cwd)])
            .unwrap_or_default(),
        "bash" => str_arg("command")
            .map(|c| shell_paths(c, cwd))
            .unwrap_or_default(),
        _ => vec![],
    };
    let cwd = normal(cwd);
    let mut out: Vec<PathBuf> = paths
        .into_iter()
        // Where it really is: a symlink in the project can point anywhere.
        .map(|p| real(&p))
        .filter(|p| !p.starts_with(&cwd))
        .map(|p| {
            if p.is_dir() {
                p
            } else {
                p.parent().map(Path::to_path_buf).unwrap_or(p)
            }
        })
        .filter(|d| !granted.iter().any(|g| d.starts_with(g)))
        .collect();
    out.sort();
    out.dedup();
    // A folder inside another one on the list is covered by it.
    let all = out.clone();
    out.retain(|d| !all.iter().any(|o| o != d && d.starts_with(o)));
    out
}

/// The real location of `p` (symlinks followed). A path that does not exist yet is its
/// folder's real location plus the name.
fn real(p: &Path) -> PathBuf {
    if let Ok(r) = canonical(p) {
        return r;
    }
    match (p.parent(), p.file_name()) {
        (Some(dir), Some(name)) => canonical(dir)
            .map(|d| d.join(name))
            .unwrap_or_else(|_| p.to_path_buf()),
        _ => p.to_path_buf(),
    }
}

/// `cwd` itself may be a symlink or relative; compare the same way paths are resolved.
fn normal(cwd: &Path) -> PathBuf {
    canonical(cwd).unwrap_or_else(|_| resolve(".", cwd))
}

/// Symlinks resolved, in the plain form (`C:\x`, not `\\?\C:\x`) that the project folder,
/// reference folders and the user's `external_directory` rules are written in.
fn canonical(p: &Path) -> std::io::Result<PathBuf> {
    std::fs::canonicalize(p).map(magia_types::plain_path)
}

fn shell_paths(cmd: &str, cwd: &Path) -> Vec<PathBuf> {
    let parsed = parse(cmd);
    let mut out = Vec::new();
    for seg in &parsed.segments {
        // The program itself (`/usr/bin/python`) is not data it reaches.
        let args = seg.words.iter().skip(1);
        let targets = seg.redirects.iter().map(|(_, t)| t);
        for w in args.chain(targets) {
            // `--out=/x/y` → `/x/y`
            let w = match w.split_once('=') {
                Some((k, v)) if k.starts_with('-') => v,
                _ => w.as_str(),
            };
            // Any word with a slash may be a path (`src/../../x`); URLs and regexes fall out
            // below because they name nothing that exists.
            let looks = w.contains('/') || w == "~" || w.starts_with("$HOME") || w == "..";
            if !looks || HARMLESS.contains(&w) {
                continue;
            }
            let p = resolve(w, cwd);
            // Existing paths only (a new file counts by its folder): regexes and URL paths
            // in arguments are not folders.
            if p.exists()
                || p.parent()
                    .is_some_and(|d| d.is_dir() && d != Path::new("/"))
            {
                out.push(p);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn file_tools_outside_the_project_name_their_folder() {
        let project = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        let cwd = canonical(project.path()).unwrap();
        let ext = canonical(other.path()).unwrap();
        std::fs::write(ext.join("notes.txt"), "x").unwrap();
        let read = |p: &Path| json!({"file_path": p.to_str().unwrap()});
        assert!(folders("read", &read(&cwd.join("src/a.rs")), &cwd, &[]).is_empty());
        assert_eq!(
            folders("read", &read(&ext.join("notes.txt")), &cwd, &[]),
            vec![ext.clone()]
        );
        // `..` out of the project counts; granted folders (and their subfolders) do not.
        let up = json!({"path": "../"});
        assert_eq!(
            folders("ls", &up, &cwd, &[]),
            vec![cwd.parent().unwrap().to_path_buf()]
        );
        assert!(
            folders(
                "read",
                &read(&ext.join("notes.txt")),
                &cwd,
                std::slice::from_ref(&ext)
            )
            .is_empty()
        );
        assert!(folders("grep", &json!({"pattern": "x"}), &cwd, &[]).is_empty());
    }

    #[test]
    fn shell_arguments_count_only_when_they_are_real_paths() {
        let project = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        let cwd = canonical(project.path()).unwrap();
        let ext = canonical(other.path()).unwrap();
        let bash = |c: String| folders("bash", &json!({ "command": c }), &cwd, &[]);
        assert!(bash("cargo test 2>/dev/null".into()).is_empty());
        assert!(bash("grep -r \"/api/v1/\" src".into()).is_empty());
        assert!(bash("/usr/bin/env ls".into()).is_empty());
        // As written in a shell: forward slashes (Git Bash takes `C:/x` too, not `C:\x`).
        let e = shell_path(&ext);
        assert_eq!(bash(format!("cat {e}/x.txt")), vec![ext.clone()]);
        assert_eq!(bash(format!("cp a --target={e}")), vec![ext.clone()]);
        assert_eq!(bash(format!("echo hi > {e}/out.log")), vec![ext.clone()]);
        // Climbing out from inside a project folder counts too.
        std::fs::create_dir(cwd.join("src")).unwrap();
        std::fs::write(ext.join("secret"), "x").unwrap();
        let rel = pathdiff(&ext.join("secret"), &cwd.join("src"));
        assert_eq!(bash(format!("cat src/{rel}")), vec![ext.clone()]);
    }

    fn shell_path(p: &Path) -> String {
        p.display().to_string().replace('\\', "/")
    }

    /// `to` relative to `from`, through `..` up to the root (both absolute, same drive).
    fn pathdiff(to: &Path, from: &Path) -> String {
        use std::path::Component::Normal;
        let names = |p: &Path| {
            p.components()
                .filter_map(|c| match c {
                    Normal(n) => Some(n.to_string_lossy().into_owned()),
                    _ => None,
                })
                .collect::<Vec<_>>()
        };
        format!("{}{}", "../".repeat(names(from).len()), names(to).join("/"))
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_in_the_project_counts_where_it_points() {
        let project = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        let cwd = canonical(project.path()).unwrap();
        let ext = canonical(other.path()).unwrap();
        std::fs::write(ext.join("id_key"), "x").unwrap();
        std::os::unix::fs::symlink(&ext, cwd.join("docs")).unwrap();
        let read = json!({"file_path": cwd.join("docs/id_key").to_str().unwrap()});
        assert_eq!(folders("read", &read, &cwd, &[]), vec![ext.clone()]);
        let write = json!({"file_path": cwd.join("docs/new.txt").to_str().unwrap()});
        assert_eq!(folders("write", &write, &cwd, &[]), vec![ext]);
    }
}
