//! Reading images from the system clipboard (egui itself only pastes text).
//!
//! Wayland: egui's `smithay-clipboard` only speaks text, so the image is fetched with
//! `wl-paste` (wl-clipboard). X11 (or Wayland without wl-paste, via XWayland): `arboard`, already
//! in the tree through egui-winit. Both run on the tokio runtime, never inside `update()`.

use anyhow::{Context, Result, bail};
use magia_core::media;
use magia_types::Image;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::process::Command;

const TIMEOUT: Duration = Duration::from_secs(3);
/// Preferred order when the clipboard offers several image types.
const IMAGE_TYPES: &[&str] = &["image/png", "image/jpeg", "image/webp", "image/gif"];

/// Images currently on the clipboard: pixel data, or copied image files (file manager).
/// An empty list means the clipboard holds nothing usable.
pub async fn read_images() -> Result<Vec<Image>> {
    if std::env::var_os("WAYLAND_DISPLAY").is_some() {
        match wayland().await {
            Ok(Some(v)) => return Ok(v),
            Ok(None) => {} // wl-paste missing: try X11 through XWayland
            Err(e) => return Err(e),
        }
    }
    tokio::task::spawn_blocking(x11).await?
}

/// `Ok(None)` when `wl-paste` is not installed.
async fn wayland() -> Result<Option<Vec<Image>>> {
    let Some(types) = wl_paste(&["--list-types"]).await? else {
        return Ok(None);
    };
    let types = String::from_utf8_lossy(&types);
    let offered: Vec<&str> = types.lines().map(str::trim).collect();
    if let Some(mt) = IMAGE_TYPES.iter().find(|t| offered.contains(t)) {
        let bytes = wl_paste(&["--no-newline", "--type", mt])
            .await?
            .unwrap_or_default();
        let img = tokio::task::spawn_blocking(move || media::image_from_bytes(bytes)).await??;
        return Ok(Some(vec![img]));
    }
    if offered.contains(&"text/uri-list") {
        let list = wl_paste(&["--no-newline", "--type", "text/uri-list"])
            .await?
            .unwrap_or_default();
        let paths = parse_uri_list(&String::from_utf8_lossy(&list));
        return Ok(Some(load_files(paths).await?));
    }
    Ok(Some(Vec::new()))
}

/// Run `wl-paste` with a timeout. `Ok(None)` if it is not installed; an empty clipboard
/// ("Nothing is copied") is an empty output, not an error.
async fn wl_paste(args: &[&str]) -> Result<Option<Vec<u8>>> {
    let run = Command::new("wl-paste")
        .args(args)
        .kill_on_drop(true)
        .output();
    let out = match tokio::time::timeout(TIMEOUT, run).await {
        Err(_) => bail!("timed out reading the clipboard"),
        Ok(Err(e)) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Ok(r) => r.context("wl-paste")?,
    };
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        if err.contains("Nothing is copied") || err.contains("No suitable type") {
            return Ok(Some(Vec::new()));
        }
        bail!("wl-paste: {}", err.trim());
    }
    Ok(Some(out.stdout))
}

fn x11() -> Result<Vec<Image>> {
    let mut cb = arboard::Clipboard::new().context("clipboard unavailable")?;
    match cb.get_image() {
        Ok(img) => {
            let (w, h) = (img.width as u32, img.height as u32);
            return Ok(vec![media::image_from_rgba(w, h, img.bytes.into_owned())?]);
        }
        Err(arboard::Error::ContentNotAvailable) => {}
        Err(e) => return Err(e.into()),
    }
    match cb.get().file_list() {
        Ok(paths) => paths
            .into_iter()
            .filter(|p| media::is_image_path(p))
            .map(|p| media::load_image_file(&p))
            .collect(),
        Err(_) => Ok(Vec::new()),
    }
}

async fn load_files(paths: Vec<PathBuf>) -> Result<Vec<Image>> {
    tokio::task::spawn_blocking(move || {
        paths
            .iter()
            .filter(|p| media::is_image_path(p))
            .map(|p| media::load_image_file(p))
            .collect()
    })
    .await?
}

/// `text/uri-list` (RFC 2483): one URI per line, `#` comments. Only local `file://` URIs.
pub fn parse_uri_list(s: &str) -> Vec<PathBuf> {
    s.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .filter_map(file_uri_to_path)
        .collect()
}

fn file_uri_to_path(uri: &str) -> Option<PathBuf> {
    let rest = uri.strip_prefix("file://")?;
    // `file:///path` or `file://localhost/path`.
    let path = rest.strip_prefix("localhost").unwrap_or(rest);
    path.starts_with('/')
        .then(|| PathBuf::from(uri_decode(path)))
}

/// `%XX` escapes only (`+` is a literal plus in file URIs, unlike in query strings).
fn uri_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        let hex = |j: usize| b.get(j).and_then(|c| (*c as char).to_digit(16));
        if b[i] == b'%'
            && let (Some(h), Some(l)) = (hex(i + 1), hex(i + 2))
        {
            out.push((h * 16 + l) as u8);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Pasted text that is only image files (paths or `file://` URIs, one per line, as file
/// managers copy them) → those paths. Anything else is ordinary text.
pub fn pasted_image_paths(text: &str) -> Option<Vec<PathBuf>> {
    let mut out = Vec::new();
    for line in text.lines().map(str::trim).filter(|l| !l.is_empty()) {
        let p = file_uri_to_path(line).unwrap_or_else(|| PathBuf::from(line));
        if !(p.is_absolute() && media::is_image_path(&p) && Path::new(&p).is_file()) {
            return None;
        }
        out.push(p);
    }
    (!out.is_empty()).then_some(out)
}

pub async fn load_paths(paths: Vec<PathBuf>) -> Result<Vec<Image>> {
    load_files(paths).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uri_lists_and_pasted_paths() {
        assert_eq!(
            parse_uri_list(
                "# comment\r\nfile:///tmp/a%20b+c.png\r\nfile://localhost/x.jpg\nhttp://e/x.png"
            ),
            [PathBuf::from("/tmp/a b+c.png"), PathBuf::from("/x.jpg")]
        );
        let d = tempfile_dir();
        let img = d.join("shot.png");
        std::fs::write(&img, b"x").unwrap();
        let text = format!("file://{}\n", img.display());
        assert_eq!(pasted_image_paths(&text), Some(vec![img.clone()]));
        assert_eq!(
            pasted_image_paths(&img.display().to_string()),
            Some(vec![img.clone()])
        );
        assert_eq!(pasted_image_paths("just some text"), None);
        assert_eq!(
            pasted_image_paths(&format!("{}\nhello", img.display())),
            None
        );
        assert_eq!(pasted_image_paths("/does/not/exist.png"), None);
        std::fs::remove_dir_all(d).unwrap();
    }

    fn tempfile_dir() -> PathBuf {
        let d = std::env::temp_dir().join(format!("magia-clip-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// Reads the real system clipboard: `cargo test -p magia-gui -- --ignored --nocapture`
    /// after copying an image (e.g. a screenshot).
    #[test]
    #[ignore]
    fn reads_the_system_clipboard() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let images = rt.block_on(read_images()).unwrap();
        for i in &images {
            println!(
                "{} {:?} {} bytes",
                i.media_type,
                media::dimensions(i),
                i.byte_len()
            );
        }
        assert!(!images.is_empty(), "copy an image first");
        let x = x11();
        println!("arboard/X11: {:?}", x.as_ref().map(|v| v.len()));
    }
}
