//! Choosing the project folder: the system's folder dialog and the folders of earlier
//! sessions. On Linux the dialog goes through the desktop portal (`org.freedesktop.portal.
//! FileChooser`) over zbus, which is pure Rust and already in the tree; elsewhere `rfd` uses
//! the native dialog. Everything here runs off the UI thread.

use std::path::{Path, PathBuf};

/// How many earlier folders the menu offers.
const RECENT: usize = 12;

/// Ask the user for a folder, starting at `start`. `Ok(None)` = cancelled.
pub async fn pick(start: &Path) -> anyhow::Result<Option<PathBuf>> {
    imp::pick(start).await
}

/// Folders of earlier sessions that still exist, most recent first, without `current`.
pub fn recent(sessions: &Path, current: &Path) -> Vec<PathBuf> {
    let current = magia_store::canonical(current);
    let mut out: Vec<PathBuf> = Vec::new();
    for s in magia_store::list(sessions, magia_store::Scope::All) {
        let Some(dir) = s.meta.cwd else { continue };
        if dir != current && !out.contains(&dir) && dir.is_dir() {
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
        Ok(p) if p.is_dir() => Ok(p),
        Ok(_) => anyhow::bail!("not a folder: {}", path.display()),
        Err(e) => anyhow::bail!("{}: {e}", path.display()),
    }
}

/// `file:///home/a/my%20app` → `/home/a/my app`.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn path_from_uri(uri: &str) -> Option<PathBuf> {
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
    use anyhow::{Context, bail};
    use futures_util::StreamExt;
    use std::collections::HashMap;
    use std::os::unix::ffi::OsStrExt;
    use std::path::{Path, PathBuf};
    use zbus::zvariant::{OwnedObjectPath, OwnedValue, Value};

    const DEST: &str = "org.freedesktop.portal.Desktop";

    pub async fn pick(start: &Path) -> anyhow::Result<Option<PathBuf>> {
        let conn = zbus::Connection::session()
            .await
            .context("no session bus for the folder dialog")?;
        // The portal answers on a Request object whose path we can predict from our bus name
        // and a token; listening there before the call means the answer cannot be missed.
        let token = format!(
            "magia{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos() as u64)
                .unwrap_or(0)
        );
        let sender = conn
            .unique_name()
            .context("no bus name")?
            .trim_start_matches(':')
            .replace('.', "_");
        let handle = format!("/org/freedesktop/portal/desktop/request/{sender}/{token}");
        let request = zbus::Proxy::new(
            &conn,
            DEST,
            handle.as_str(),
            "org.freedesktop.portal.Request",
        )
        .await?;
        let mut responses = request.receive_signal("Response").await?;

        let mut folder = start.as_os_str().as_bytes().to_vec();
        folder.push(0); // the portal wants a NUL-terminated byte string
        let mut options: HashMap<&str, Value<'_>> = HashMap::new();
        options.insert("handle_token", Value::from(token.as_str()));
        options.insert("directory", Value::from(true));
        options.insert("modal", Value::from(true));
        options.insert("current_folder", Value::from(folder));
        let chooser = zbus::Proxy::new(
            &conn,
            DEST,
            "/org/freedesktop/portal/desktop",
            "org.freedesktop.portal.FileChooser",
        )
        .await?;
        let _: OwnedObjectPath = chooser
            .call("OpenFile", &("", "Open folder", options))
            .await
            .context("the desktop has no folder dialog (xdg-desktop-portal)")?;

        let msg = responses
            .next()
            .await
            .context("the folder dialog closed without answering")?;
        let (code, results): (u32, HashMap<String, OwnedValue>) = msg.body().deserialize()?;
        match code {
            0 => {}
            1 => return Ok(None), // cancelled
            _ => bail!("the folder dialog failed"),
        }
        let uris: Vec<String> = match results.get("uris") {
            Some(v) => Vec::<String>::try_from(v.try_clone()?)?,
            None => vec![],
        };
        Ok(uris.first().and_then(|u| super::path_from_uri(u)))
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
        let base = std::fs::canonicalize(dir.path()).unwrap();
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
