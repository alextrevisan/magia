//! Session persistence: one append-only JSONL file per session (one message per line).
//! Pure Rust, no database.

use anyhow::Result;
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use ufrh_types::Message;

/// `$XDG_DATA_HOME/ufrh/sessions` (or `~/.local/share/ufrh/sessions`).
pub fn sessions_dir() -> PathBuf {
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("ufrh").join("sessions")
}

pub struct SessionFile {
    path: PathBuf,
}

impl SessionFile {
    pub fn create_in(dir: &Path) -> Result<Self> {
        fs::create_dir_all(dir)?;
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let mut path = dir.join(format!("{ts}.jsonl"));
        let mut n = 1;
        while path.exists() {
            path = dir.join(format!("{ts}-{n}.jsonl"));
            n += 1;
        }
        File::create(&path)?;
        Ok(Self { path })
    }

    pub fn open(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn append(&self, m: &Message) -> Result<()> {
        let mut f = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        writeln!(f, "{}", serde_json::to_string(m)?)?;
        Ok(())
    }

    /// Load all messages, skipping unparsable lines (e.g. a torn final write).
    pub fn load(&self) -> Result<Vec<Message>> {
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
            let mut arch_path = self.path.clone().into_os_string();
            arch_path.push(".archive");
            OpenOptions::new()
                .create(true)
                .append(true)
                .open(arch_path)?
                .write_all(&old)?;
        }
        let tmp = self.path.with_extension("jsonl.tmp");
        let mut f = File::create(&tmp)?;
        for m in messages {
            writeln!(f, "{}", serde_json::to_string(m)?)?;
        }
        f.sync_all()?;
        fs::rename(tmp, &self.path)?;
        Ok(())
    }

    /// Most recent session file in `dir`, if any.
    pub fn latest_in(dir: &Path) -> Option<Self> {
        let mut files: Vec<_> = fs::read_dir(dir)
            .ok()?
            .flatten()
            .filter(|e| e.path().extension().is_some_and(|x| x == "jsonl"))
            .filter_map(|e| Some((e.metadata().ok()?.modified().ok()?, e.path())))
            .collect();
        files.sort();
        files.pop().map(|(_, p)| Self::open(p))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_and_torn_line() {
        let d = tempfile::tempdir().unwrap();
        let s = SessionFile::create_in(d.path()).unwrap();
        s.append(&Message::User { text: "hi".into() }).unwrap();
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
        assert!(SessionFile::latest_in(d.path()).is_some());
        s.rewrite(&m[..1]).unwrap();
        assert_eq!(s.load().unwrap().len(), 1);
        assert!(
            d.path()
                .join(format!(
                    "{}.archive",
                    s.path().file_name().unwrap().to_string_lossy()
                ))
                .exists()
        );
    }
}
