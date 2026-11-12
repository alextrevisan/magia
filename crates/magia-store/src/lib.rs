//! Session persistence: one append-only JSONL file per session (one message per line) plus a
//! small `<id>.meta.json` sidecar (folder, title, dates, message count) so listing sessions
//! never reads the transcripts. Pure Rust, no database.
//!
//! Layout: `sessions/<folder-name>-<hash>/<id>.jsonl`, one directory per working folder. A session
//! can only be resumed from the folder it belongs to; every session can be listed and searched.
//! Older flat files (`sessions/<id>.jsonl`, no sidecar) have no known folder: they are listed as
//! such and move into the current folder the first time they are resumed.

use anyhow::{Result, bail};
use magia_types::Message;
use serde::{Deserialize, Serialize};
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// `$XDG_DATA_HOME/magia/sessions` (or `~/.local/share/magia/sessions`).
pub fn sessions_dir() -> PathBuf {
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("magia").join("sessions")
}

/// Canonical form of a working folder (symlinks resolved), so `~/p` and `/home/u/p` match.
pub fn canonical(cwd: &Path) -> PathBuf {
    fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf())
}

/// Directory holding the sessions of one working folder: readable name + hash of the full path.
pub fn project_dir(root: &Path, cwd: &Path) -> PathBuf {
    let cwd = canonical(cwd);
    let name: String = cwd
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "root".into())
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '_' || c == '.' {
                c
            } else {
                '_'
            }
        })
        .take(40)
        .collect();
    root.join(format!(
        "{name}-{:08x}",
        fnv1a(cwd.to_string_lossy().as_bytes())
    ))
}

fn fnv1a(b: &[u8]) -> u32 {
    b.iter().fold(0x811c9dc5u32, |h, &c| {
        (h ^ c as u32).wrapping_mul(0x01000193)
    })
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionMeta {
    /// Working folder the session belongs to; `None` for sessions saved before folders existed.
    pub cwd: Option<PathBuf>,
    /// Unix seconds.
    pub created: u64,
    pub updated: u64,
    /// First user message, one line, truncated.
    pub title: String,
    pub messages: usize,
}

const TITLE_CHARS: usize = 80;

fn title_of(text: &str) -> String {
    let line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if line.chars().count() > TITLE_CHARS {
        format!("{}…", line.chars().take(TITLE_CHARS).collect::<String>())
    } else {
        line
    }
}

pub struct SessionFile {
    path: PathBuf,
    meta: Mutex<SessionMeta>,
}

impl SessionFile {
    /// A new session for `cwd` under `root` (normally [`sessions_dir`]). Nothing is written
    /// until the first message, so opening the app does not leave empty sessions behind.
    pub fn create_for(root: &Path, cwd: &Path) -> Result<Self> {
        let dir = project_dir(root, cwd);
        fs::create_dir_all(&dir)?;
        let ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        let mut path = dir.join(format!("{ms}.jsonl"));
        let mut n = 1;
        while path.exists() || meta_path(&path).exists() {
            path = dir.join(format!("{ms}-{n}.jsonl"));
            n += 1;
        }
        let t = now();
        Ok(Self {
            path,
            meta: Mutex::new(SessionMeta {
                cwd: Some(canonical(cwd)),
                created: t,
                updated: t,
                ..Default::default()
            }),
        })
    }

    /// Open an existing session file (its sidecar, or metadata derived from the file).
    pub fn open(path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        let meta = read_meta(&path);
        Self {
            path,
            meta: Mutex::new(meta),
        }
    }

    /// Open a session to continue it in `cwd`. Fails if it belongs to another folder; a
    /// session without a folder (saved by an older version) is moved into `cwd`'s folder.
    pub fn resume(root: &Path, path: &Path, cwd: &Path) -> Result<Self> {
        let sf = Self::open(path);
        let here = canonical(cwd);
        let owner = sf.meta().cwd;
        match owner {
            Some(c) if c == here => Ok(sf),
            Some(c) => bail!(
                "this session belongs to {} — open magia in that folder to resume it",
                c.display()
            ),
            None => {
                let dir = project_dir(root, cwd);
                fs::create_dir_all(&dir)?;
                let dest = dir.join(path.file_name().unwrap_or_default());
                fs::rename(path, &dest)?;
                let archive = archive_path(path);
                if archive.exists() {
                    let _ = fs::rename(&archive, archive_path(&dest));
                }
                let moved = Self::open(dest);
                moved.meta.lock().unwrap().cwd = Some(here);
                moved.save_meta()?;
                Ok(moved)
            }
        }
    }

    /// Most recent non-empty session of `cwd`.
    pub fn latest_for(root: &Path, cwd: &Path) -> Option<Self> {
        list(root, Scope::Folder(cwd))
            .into_iter()
            .next()
            .map(|s| Self::open(s.path))
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn meta(&self) -> SessionMeta {
        self.meta.lock().unwrap().clone()
    }

    pub fn append(&self, m: &Message) -> Result<()> {
        let mut f = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        writeln!(f, "{}", serde_json::to_string(m)?)?;
        {
            let mut meta = self.meta.lock().unwrap();
            meta.messages += 1;
            meta.updated = now();
            if meta.title.is_empty()
                && let Message::User { text, .. } = m
            {
                meta.title = title_of(text);
            }
        }
        self.save_meta()
    }

    /// Load all messages, skipping unparsable lines (e.g. a torn final write).
    pub fn load(&self) -> Result<Vec<Message>> {
        if !self.path.exists() {
            return Ok(Vec::new()); // created but nothing written yet
        }
        let f = File::open(&self.path)?;
        Ok(BufReader::new(f)
            .lines()
            .map_while(Result::ok)
            .filter_map(|l| serde_json::from_str(&l).ok())
            .collect())
    }

    /// Replace the file's content (after compaction/rewind). The previous content is
    /// appended to `<file>.archive` so nothing is lost.
    pub fn rewrite(&self, messages: &[Message]) -> Result<()> {
        if let Ok(old) = fs::read(&self.path) {
            OpenOptions::new()
                .create(true)
                .append(true)
                .open(archive_path(&self.path))?
                .write_all(&old)?;
        }
        let tmp = self.path.with_extension("jsonl.tmp");
        let mut f = File::create(&tmp)?;
        for m in messages {
            writeln!(f, "{}", serde_json::to_string(m)?)?;
        }
        f.sync_all()?;
        fs::rename(tmp, &self.path)?;
        {
            let mut meta = self.meta.lock().unwrap();
            meta.messages = messages.len();
            meta.updated = now();
        }
        self.save_meta()
    }

    fn save_meta(&self) -> Result<()> {
        let json = serde_json::to_vec(&*self.meta.lock().unwrap())?;
        let p = meta_path(&self.path);
        let tmp = p.with_extension("json.tmp");
        fs::write(&tmp, json)?;
        fs::rename(tmp, p)?;
        Ok(())
    }
}

fn meta_path(jsonl: &Path) -> PathBuf {
    jsonl.with_extension("meta.json")
}

fn archive_path(jsonl: &Path) -> PathBuf {
    let mut p = jsonl.as_os_str().to_owned();
    p.push(".archive");
    PathBuf::from(p)
}

/// Sidecar metadata, or (old sessions) metadata derived from the transcript itself.
fn read_meta(path: &Path) -> SessionMeta {
    if let Ok(b) = fs::read(meta_path(path))
        && let Ok(m) = serde_json::from_slice(&b)
    {
        return m;
    }
    let mtime = fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let created = path
        .file_stem()
        .and_then(|s| s.to_str())
        .and_then(|s| s.split('-').next())
        .and_then(|s| s.parse::<u64>().ok())
        .map(|t| if t > 100_000_000_000 { t / 1000 } else { t })
        .unwrap_or(mtime);
    let mut meta = SessionMeta {
        cwd: None,
        created,
        updated: mtime,
        ..Default::default()
    };
    if let Ok(f) = File::open(path) {
        for line in BufReader::new(f).lines().map_while(Result::ok) {
            meta.messages += 1;
            if meta.title.is_empty()
                && let Ok(Message::User { text, .. }) = serde_json::from_str(&line)
            {
                meta.title = title_of(&text);
            }
        }
    }
    meta
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionInfo {
    pub path: PathBuf,
    pub meta: SessionMeta,
}

impl SessionInfo {
    /// File stem, used as the session's short id (`magia run --session <id>`).
    pub fn id(&self) -> String {
        self.path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default()
    }

    /// Whether it can be resumed from `cwd` (its own folder, or no recorded folder).
    pub fn resumable_in(&self, cwd: &Path) -> bool {
        self.meta.cwd.as_ref().is_none_or(|c| *c == canonical(cwd))
    }
}

#[derive(Debug, Clone, Copy)]
pub enum Scope<'a> {
    /// Sessions of this working folder only.
    Folder(&'a Path),
    /// Every saved session, all folders.
    All,
}

/// Non-empty sessions, most recently updated first.
pub fn list(root: &Path, scope: Scope) -> Vec<SessionInfo> {
    let dirs: Vec<PathBuf> = match scope {
        Scope::Folder(cwd) => vec![project_dir(root, cwd)],
        Scope::All => {
            let mut v = vec![root.to_path_buf()]; // old flat sessions
            v.extend(
                fs::read_dir(root)
                    .into_iter()
                    .flatten()
                    .flatten()
                    .map(|e| e.path())
                    .filter(|p| p.is_dir()),
            );
            v
        }
    };
    let mut out: Vec<SessionInfo> = dirs
        .iter()
        .flat_map(|d| fs::read_dir(d).into_iter().flatten().flatten())
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "jsonl") && p.is_file())
        .map(|path| SessionInfo {
            meta: read_meta(&path),
            path,
        })
        .filter(|s| s.meta.messages > 0)
        .collect();
    out.sort_by(|a, b| (b.meta.updated, &b.path).cmp(&(a.meta.updated, &a.path)));
    out
}

#[derive(Debug, Clone)]
pub struct Hit {
    pub session: SessionInfo,
    /// Up to three excerpts around the matches.
    pub snippets: Vec<String>,
}

/// Sessions whose title or user/assistant text contains every word of `query`
/// (case-insensitive). An empty query lists everything in scope.
pub fn search(root: &Path, scope: Scope, query: &str) -> Vec<Hit> {
    let terms: Vec<String> = query.split_whitespace().map(str::to_lowercase).collect();
    let sessions = list(root, scope);
    if terms.is_empty() {
        return sessions
            .into_iter()
            .map(|session| Hit {
                session,
                snippets: vec![],
            })
            .collect();
    }
    let mut hits = Vec::new();
    for session in sessions {
        let mut found = vec![false; terms.len()];
        let mut snippets = Vec::new();
        let title = session.meta.title.to_lowercase();
        for (i, t) in terms.iter().enumerate() {
            found[i] = title.contains(t.as_str());
        }
        if let Ok(f) = File::open(&session.path) {
            for line in BufReader::new(f).lines().map_while(Result::ok) {
                let text = match serde_json::from_str::<Message>(&line) {
                    Ok(Message::User { text, .. }) | Ok(Message::Assistant { text, .. }) => text,
                    _ => continue,
                };
                let lower = text.to_lowercase();
                for (i, t) in terms.iter().enumerate() {
                    if let Some(pos) = lower.find(t.as_str()) {
                        if snippets.len() < 3 && (!found[i] || snippets.is_empty()) {
                            snippets.push(excerpt(&text, &lower, pos, t.len()));
                        }
                        found[i] = true;
                    }
                }
                if found.iter().all(|f| *f) && snippets.len() >= 3 {
                    break;
                }
            }
        }
        if found.iter().all(|f| *f) {
            hits.push(Hit { session, snippets });
        }
    }
    hits
}

/// About 60 characters of context on each side of the match, on one line.
fn excerpt(text: &str, lower: &str, pos: usize, len: usize) -> String {
    // Lowercasing can change byte lengths (rare): then cut from the lowercase copy.
    let src = if text.len() == lower.len() {
        text
    } else {
        lower
    };
    let start = src.floor_char_boundary(pos.saturating_sub(60));
    let end = src.ceil_char_boundary((pos + len + 60).min(src.len()));
    let body = src[start..end]
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    format!(
        "{}{body}{}",
        if start > 0 { "…" } else { "" },
        if end < src.len() { "…" } else { "" }
    )
}

/// `5m ago`, `3h ago`, `2d ago`, or `2026-03-01` for anything older than a month.
pub fn age(unix: u64) -> String {
    let d = now().saturating_sub(unix);
    match d {
        0..60 => "just now".into(),
        60..3600 => format!("{}m ago", d / 60),
        3600..86400 => format!("{}h ago", d / 3600),
        86400..2592000 => format!("{}d ago", d / 86400),
        _ => {
            // Days since 1970-01-01 → civil date (Howard Hinnant's algorithm).
            let z = (unix / 86400) as i64 + 719468;
            let era = z.div_euclid(146097);
            let doe = z.rem_euclid(146097);
            let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
            let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
            let mp = (5 * doy + 2) / 153;
            let day = doy - (153 * mp + 2) / 5 + 1;
            let month = if mp < 10 { mp + 3 } else { mp - 9 };
            let year = yoe + era * 400 + (month <= 2) as i64;
            format!("{year:04}-{month:02}-{day:02}")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_and_torn_line() {
        let d = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let s = SessionFile::create_for(d.path(), cwd.path()).unwrap();
        assert!(
            !s.path().exists(),
            "nothing written before the first message"
        );
        assert!(s.load().unwrap().is_empty());
        s.append(&Message::user("hi\n  there")).unwrap();
        s.append(&Message::Assistant {
            text: "yo".into(),
            tool_calls: vec![],
        })
        .unwrap();
        std::fs::OpenOptions::new()
            .append(true)
            .open(s.path())
            .unwrap()
            .write_all(b"{\"role\":")
            .unwrap();
        let m = s.load().unwrap();
        assert_eq!(m.len(), 2);
        let meta = SessionFile::open(s.path()).meta();
        assert_eq!(meta.title, "hi there");
        assert_eq!(meta.messages, 2);
        assert_eq!(meta.cwd, Some(canonical(cwd.path())));
        assert!(SessionFile::latest_for(d.path(), cwd.path()).is_some());
        s.rewrite(&m[..1]).unwrap();
        assert_eq!(s.load().unwrap().len(), 1);
        assert_eq!(SessionFile::open(s.path()).meta().messages, 1);
        assert!(archive_path(s.path()).exists());
    }

    #[test]
    fn sessions_are_split_by_folder_and_only_resume_in_their_own() {
        let root = tempfile::tempdir().unwrap();
        let (a, b) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let sa = SessionFile::create_for(root.path(), a.path()).unwrap();
        sa.append(&Message::user("fix the parser in project A"))
            .unwrap();
        let sb = SessionFile::create_for(root.path(), b.path()).unwrap();
        sb.append(&Message::user("deploy project B")).unwrap();
        sb.append(&Message::Assistant {
            text: "Deployed the Parser service".into(),
            tool_calls: vec![],
        })
        .unwrap();
        // An empty session is never listed.
        let _empty = SessionFile::create_for(root.path(), a.path()).unwrap();

        let in_a = list(root.path(), Scope::Folder(a.path()));
        assert_eq!(in_a.len(), 1);
        assert_eq!(in_a[0].meta.title, "fix the parser in project A");
        assert_eq!(list(root.path(), Scope::All).len(), 2);

        // Search crosses folders; every word must appear somewhere in the session.
        let hits = search(root.path(), Scope::All, "PARSER");
        assert_eq!(hits.len(), 2);
        let hits = search(root.path(), Scope::All, "parser deploy");
        assert_eq!(hits.len(), 1);
        assert!(
            hits[0]
                .snippets
                .iter()
                .any(|s| s.contains("Parser service"))
        );
        assert_eq!(
            search(root.path(), Scope::Folder(a.path()), "deploy").len(),
            0
        );

        // Resume only in the owning folder.
        assert!(!hits[0].session.resumable_in(a.path()));
        let err = SessionFile::resume(root.path(), sb.path(), a.path())
            .err()
            .unwrap();
        assert!(err.to_string().contains("belongs to"), "{err}");
        let ok = SessionFile::resume(root.path(), sb.path(), b.path()).unwrap();
        assert_eq!(ok.load().unwrap().len(), 2);
    }

    #[test]
    fn old_flat_sessions_are_listed_and_adopted_on_resume() {
        let root = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let old = root.path().join("1794411485.jsonl");
        std::fs::write(
            &old,
            format!(
                "{}\n",
                serde_json::to_string(&Message::user("legacy question")).unwrap()
            ),
        )
        .unwrap();
        let all = list(root.path(), Scope::All);
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].meta.cwd, None);
        assert_eq!(all[0].meta.title, "legacy question");
        assert_eq!(all[0].meta.created, 1794411485);
        assert!(all[0].resumable_in(cwd.path()));
        let s = SessionFile::resume(root.path(), &old, cwd.path()).unwrap();
        assert!(!old.exists(), "moved into the folder's directory");
        assert_eq!(s.load().unwrap().len(), 1);
        let mine = list(root.path(), Scope::Folder(cwd.path()));
        assert_eq!(mine.len(), 1);
        assert_eq!(mine[0].meta.cwd, Some(canonical(cwd.path())));
        // Now it belongs to that folder only.
        let other = tempfile::tempdir().unwrap();
        assert!(SessionFile::resume(root.path(), s.path(), other.path()).is_err());
    }

    #[test]
    fn ages() {
        assert_eq!(age(now()), "just now");
        assert_eq!(age(now() - 7200), "2h ago");
        assert_eq!(age(1_700_000_000), "2023-11-14");
    }
}
