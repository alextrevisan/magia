//! Choosing the project folder: the system's folder dialog and the folders of earlier
//! sessions. On Linux the dialog goes through the desktop portal (`org.freedesktop.portal.
//! FileChooser`) over zbus, which is pure Rust and already in the tree; elsewhere `rfd` uses
//! the native dialog. Everything here runs off the UI thread.

use std::path::{Path, PathBuf};

/// How many earlier folders the menu offers.
const RECENT: usize = 12;

/// Where standalone chats (outside any project) get their scratch folders.
pub fn chats_root() -> PathBuf {
    let sessions = magia_store::sessions_dir();
    sessions
        .parent()
        .map(|p| p.join("chats"))
        .unwrap_or_else(|| sessions.join("chats"))
}

/// A new, empty folder for a standalone chat.
pub fn new_chat_dir() -> anyhow::Result<PathBuf> {
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let dir = chats_root().join(ms.to_string());
    std::fs::create_dir_all(&dir)?;
    Ok(magia_store::canonical(&dir))
}

/// Ask the user for a folder, starting at `start`. `Ok(None)` = cancelled.
pub async fn pick(start: &Path) -> anyhow::Result<Option<PathBuf>> {
    imp::pick(start).await
}

/// Folders of earlier sessions that still exist, most recent first, without `current`.
pub fn recent(sessions: &Path, current: &Path) -> Vec<PathBuf> {
    let current = magia_store::canonical(current);
    let chats = chats_root();
    let mut out: Vec<PathBuf> = Vec::new();
    for s in magia_store::list(sessions, magia_store::Scope::All) {
        let Some(dir) = s.meta.cwd else { continue };
        if dir != current && !dir.starts_with(&chats) && !out.contains(&dir) && dir.is_dir() {
            out.push(dir);
            if out.len() == RECENT {
                break;
            }
        }
    }
    out
}

/// A path typed by the user (`/open ~/src/app`): `~` is the home folder, relative paths start
/// at `cwd`. Errors when it is not an existing folder.
pub fn resolve(typed: &str, cwd: &Path) -> anyhow::Result<PathBuf> {
    let typed = typed.trim();
    let path = match typed.strip_prefix('~') {
        Some(rest) if rest.is_empty() || rest.starts_with('/') => {
            let home = std::env::var_os("HOME")
                .or_else(|| std::env::var_os("USERPROFILE"))
                .ok_or_else(|| anyhow::anyhow!("no home folder"))?;
            PathBuf::from(home).join(rest.trim_start_matches('/'))
        }
        _ => cwd.join(typed),
    };
    match std::fs::canonicalize(&path) {
        Ok(p) if p.is_dir() => Ok(magia_types::plain_path(p)),
        Ok(_) => anyhow::bail!("not a folder: {}", path.display()),
        Err(e) => anyhow::bail!("{}: {e}", path.display()),
    }
}

/// `file:///home/a/my%20app` → `/home/a/my app`.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) fn path_from_uri(uri: &str) -> Option<PathBuf> {
    let rest = uri.strip_prefix("file://")?;
    // An authority before the path ("file://localhost/…") is allowed and ignored.
    let path = &rest[rest.find('/')?..];
    let bytes = path.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && let Some(b) = path
                .get(i + 1..i + 3)
                .and_then(|h| u8::from_str_radix(h, 16).ok())
        {
            out.push(b);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStringExt;
        Some(PathBuf::from(std::ffi::OsString::from_vec(out)))
    }
    #[cfg(not(unix))]
    {
        String::from_utf8(out).ok().map(PathBuf::from)
    }
}

#[cfg(target_os = "linux")]
mod imp {
    use crate::portal;
    use std::os::unix::ffi::OsStrExt;
    use std::path::{Path, PathBuf};
    use zbus::zvariant::Value;

    pub async fn pick(start: &Path) -> anyhow::Result<Option<PathBuf>> {
        let mut folder = start.as_os_str().as_bytes().to_vec();
        folder.push(0); // the portal wants a NUL-terminated byte string
        let mut options = portal::Options::new();
        options.insert("directory", Value::from(true));
        options.insert("modal", Value::from(true));
        options.insert("current_folder", Value::from(folder));
        let answer = portal::call(
            "org.freedesktop.portal.FileChooser",
            "OpenFile",
            options,
            |o| ("", "Open folder", o),
            None,
        )
        .await?;
        Ok(answer.as_ref().and_then(portal::path_of))
    }
}

#[cfg(not(target_os = "linux"))]
mod imp {
    use std::path::{Path, PathBuf};

    pub async fn pick(start: &Path) -> anyhow::Result<Option<PathBuf>> {
        Ok(rfd::AsyncFileDialog::new()
            .set_title("Open folder")
            .set_directory(start)
            .pick_folder()
            .await
            .map(|h| h.path().to_path_buf()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_uris_become_paths() {
        assert_eq!(
            path_from_uri("file:///home/a/my%20app"),
            Some(PathBuf::from("/home/a/my app"))
        );
        assert_eq!(
            path_from_uri("file://localhost/srv/x"),
            Some(PathBuf::from("/srv/x"))
        );
        assert_eq!(
            path_from_uri("file:///caf%C3%A9"),
            Some(PathBuf::from("/café"))
        );
        assert_eq!(path_from_uri("https://x/y"), None);
    }

    #[test]
    fn typed_paths_resolve_from_the_current_folder() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("f.txt"), "").unwrap();
        let base = magia_store::canonical(dir.path());
        assert_eq!(resolve("sub", &base).unwrap(), base.join("sub"));
        assert_eq!(resolve(" sub/.. ", &base).unwrap(), base);
        assert!(resolve("f.txt", &base).is_err());
        assert!(resolve("missing", &base).is_err());
    }

    #[test]
    fn recent_folders_skip_the_current_one_and_missing_ones() {
        let root = tempfile::tempdir().unwrap();
        let (a, b) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        for dir in [
            a.path(),
            b.path(),
            a.path(),
            Path::new("/nonexistent/magia"),
        ] {
            let s = magia_store::SessionFile::create_for(root.path(), dir).unwrap();
            s.append(&magia_types::Message::user("hi")).unwrap();
            std::thread::sleep(std::time::Duration::from_millis(3));
        }
        let r = recent(root.path(), b.path());
        assert_eq!(r, vec![magia_store::canonical(a.path())]);
    }
}
