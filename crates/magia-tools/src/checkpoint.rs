//! File checkpoints: before a turn's first modification of a file we keep a copy, so
//! `rewind` can restore the project to the state before any earlier turn.
//! (Files changed through `bash` are not tracked.)

use std::path::{Path, PathBuf};
use std::sync::Mutex;

struct Entry {
    turn: usize,
    path: PathBuf,
    /// `None` = the file did not exist before (rewind deletes it).
    backup: Option<PathBuf>,
}

pub struct Checkpoints {
    dir: PathBuf,
    state: Mutex<(usize, Vec<Entry>)>,
}

impl Checkpoints {
    pub fn new(dir: PathBuf) -> Self {
        Self {
            dir,
            state: Mutex::new((0, Vec::new())),
        }
    }

    /// Start a new turn (called once per user prompt). Returns its number.
    pub fn begin_turn(&self) -> usize {
        let mut s = self.state.lock().unwrap();
        s.0 += 1;
        s.0
    }

    pub fn turn(&self) -> usize {
        self.state.lock().unwrap().0
    }

    /// Record `path` before it is modified (first time per turn only).
    pub fn snapshot(&self, path: &Path) {
        let mut s = self.state.lock().unwrap();
        let turn = s.0;
        if s.1.iter().any(|e| e.turn == turn && e.path == path) {
            return;
        }
        let backup = if path.is_file() {
            let name = format!("t{turn}-{}", s.1.len());
            let dst = self.dir.join(name);
            std::fs::create_dir_all(&self.dir).ok();
            std::fs::copy(path, &dst).ok().map(|_| dst)
        } else {
            None
        };
        s.1.push(Entry {
            turn,
            path: path.to_path_buf(),
            backup,
        });
    }

    /// Undo every change made in turns `>= turn`. Returns the restored paths.
    pub fn rewind_to(&self, turn: usize) -> Vec<PathBuf> {
        let mut s = self.state.lock().unwrap();
        let mut restored = Vec::new();
        while s.1.last().is_some_and(|e| e.turn >= turn) {
            let e = s.1.pop().unwrap();
            match &e.backup {
                Some(b) => {
                    let _ = std::fs::copy(b, &e.path);
                    let _ = std::fs::remove_file(b);
                }
                None => {
                    let _ = std::fs::remove_file(&e.path);
                }
            }
            restored.push(e.path);
        }
        s.0 = turn.saturating_sub(1);
        restored
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rewind_restores_and_deletes() {
        let d = tempfile::tempdir().unwrap();
        let cp = Checkpoints::new(d.path().join("ck"));
        let f = d.path().join("a.txt");
        std::fs::write(&f, "v0").unwrap();
        cp.begin_turn(); // 1
        cp.snapshot(&f);
        std::fs::write(&f, "v1").unwrap();
        cp.begin_turn(); // 2
        cp.snapshot(&f);
        std::fs::write(&f, "v2").unwrap();
        let g = d.path().join("new.txt");
        cp.snapshot(&g);
        std::fs::write(&g, "x").unwrap();
        cp.rewind_to(2);
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "v1");
        assert!(!g.exists());
        cp.rewind_to(1);
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "v0");
    }
}
