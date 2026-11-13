//! `screenshot`: lets the model look at the screen (a game it just started, a UI it changed).
//! Linux only: the whole screen through the desktop portal (`org.freedesktop.portal.
//! Screenshot`) over zbus, without a dialog on GNOME and KDE; one window through X11 (x11rb,
//! already in the tree via arboard), which covers XWayland programs such as Godot. Registered by the GUI
//! only, so the CLI stays without zbus; there `bash` can save a picture and `read` shows it.

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use magia_tools::{Tool, ToolCx, ToolOutput};
use serde_json::{Value, json};

pub struct Screenshot;

/// Longest wait the model may ask for before the capture.
const MAX_DELAY: u64 = 30;

#[async_trait]
impl Tool for Screenshot {
    fn name(&self) -> &'static str {
        "screenshot"
    }
    fn description(&self) -> &'static str {
        "Take a screenshot and look at it: check what a program you started or changed looks \
like (a game, a GUI). Start the program first in the background with bash (e.g. \
`godot --path . &`; the bash call returns while it keeps running), then call this with \
`delay` to give it time to open, and `window` set to part of its title to get only that \
window (works for X11/XWayland programs such as Godot; otherwise you get the whole screen). \
Close the program with bash when done (e.g. `pkill -f godot`)."
    }
    fn schema(&self) -> Value {
        json!({"type":"object","properties":{
            "delay":{"type":"integer","description":"seconds to wait before the capture (0-30, default 0)"},
            "window":{"type":"string","description":"part of the window title (case-insensitive); omit for the whole screen"}}})
    }
    fn read_only(&self) -> bool {
        true
    }
    async fn call(&self, input: Value, cx: &ToolCx) -> Result<String> {
        Ok(self.call_output(input, cx).await?.text)
    }
    async fn call_output(&self, input: Value, cx: &ToolCx) -> Result<ToolOutput> {
        let delay = input.get("delay").and_then(Value::as_u64).unwrap_or(0);
        if delay > 0 {
            tokio::select! {
                _ = tokio::time::sleep(std::time::Duration::from_secs(delay.min(MAX_DELAY))) => {}
                _ = cx.cancel.cancelled() => bail!("cancelled"),
            }
        }
        let window = input
            .get("window")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|w| !w.is_empty());
        let mut note = String::new();
        if let Some(w) = window {
            match imp::capture_window(w.to_owned()).await {
                Ok((name, bytes)) => {
                    let size = png_size(&bytes)
                        .map(|(w, h)| format!(" ({w}×{h} px)"))
                        .unwrap_or_default();
                    return Ok(ToolOutput {
                        text: format!("Screenshot of the window \"{name}\"{size} attached."),
                        images: vec![bytes],
                    });
                }
                Err(e) => note = format!(" Only the window could not be captured ({e:#})."),
            }
        }
        let bytes = imp::capture().await?;
        let size = magia_types::sniff_image(&bytes)
            .map(|_| png_size(&bytes))
            .unwrap_or_default();
        let text = match size {
            Some((w, h)) => format!("Screenshot of the whole screen ({w}×{h} px) attached.{note}"),
            None => format!("Screenshot of the whole screen attached.{note}"),
        };
        Ok(ToolOutput {
            text,
            images: vec![bytes],
        })
    }
}

/// Width and height from a PNG header.
fn png_size(b: &[u8]) -> Option<(u32, u32)> {
    if b.len() < 24 || &b[12..16] != b"IHDR" {
        return None;
    }
    let n = |i: usize| u32::from_be_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]]);
    Some((n(16), n(20)))
}

/// Whether this build can take screenshots (the tool is only offered then).
pub fn available() -> bool {
    cfg!(target_os = "linux")
}

#[cfg(target_os = "linux")]
mod imp {
    use super::*;
    use crate::portal;
    use zbus::zvariant::Value;

    /// The whole screen, through the portal (Wayland and X11; no dialog on GNOME/KDE).
    pub async fn capture() -> Result<Vec<u8>> {
        let mut options = portal::Options::new();
        options.insert("interactive", Value::from(false));
        options.insert("modal", Value::from(false));
        let answer = portal::call(
            "org.freedesktop.portal.Screenshot",
            "Screenshot",
            options,
            |o| ("", o),
            Some(std::time::Duration::from_secs(60)),
        )
        .await?
        .context("the screenshot was refused or cancelled")?;
        let path = portal::path_of(&answer).context("the portal returned no picture")?;
        let bytes = tokio::fs::read(&path)
            .await
            .with_context(|| format!("reading {}", path.display()))?;
        // The portal saved a new file (GNOME: ~/Pictures/Screenshot-N.png) just for us; the
        // picture is in memory now, so don't leave one file per call behind.
        let _ = tokio::fs::remove_file(&path).await;
        Ok(bytes)
    }

    /// One window by (part of) its title, through X11: programs on XWayland (Godot's
    /// default, most games) or an X11 session. Err names the windows there are.
    pub async fn capture_window(title: String) -> Result<(String, Vec<u8>)> {
        tokio::task::spawn_blocking(move || x11::capture(&title)).await?
    }

    mod x11 {
        use anyhow::{Context, Result, bail};
        use x11rb::connection::Connection;
        use x11rb::protocol::xproto::{AtomEnum, ConnectionExt, ImageFormat, MapState, Window};

        pub fn capture(title: &str) -> Result<(String, Vec<u8>)> {
            let (conn, screen) =
                x11rb::connect(None).context("no X11 display (the program may be Wayland-only)")?;
            let root = conn.setup().roots[screen].root;
            let atom = |name: &str| -> Result<u32> {
                Ok(conn.intern_atom(false, name.as_bytes())?.reply()?.atom)
            };
            let (clients, net_name, utf8) = (
                atom("_NET_CLIENT_LIST")?,
                atom("_NET_WM_NAME")?,
                atom("UTF8_STRING")?,
            );
            let list = conn
                .get_property(false, root, clients, AtomEnum::WINDOW, 0, 4096)?
                .reply()?;
            let windows: Vec<Window> = list.value32().map(|v| v.collect()).unwrap_or_default();
            let name_of = |w: Window| -> String {
                let get = |prop: u32, ty: u32| {
                    conn.get_property(false, w, prop, ty, 0, 1024)
                        .ok()
                        .and_then(|c| c.reply().ok())
                        .map(|r| String::from_utf8_lossy(&r.value).into_owned())
                        .filter(|s| !s.is_empty())
                };
                get(net_name, utf8)
                    .or_else(|| get(AtomEnum::WM_NAME.into(), AtomEnum::STRING.into()))
                    .unwrap_or_default()
            };
            let want = title.to_lowercase();
            let mut names = Vec::new();
            let mut found = None;
            for w in windows {
                let viewable = conn
                    .get_window_attributes(w)
                    .ok()
                    .and_then(|c| c.reply().ok())
                    .is_some_and(|a| a.map_state == MapState::VIEWABLE);
                let name = name_of(w);
                if !viewable || name.is_empty() {
                    continue;
                }
                if found.is_none() && name.to_lowercase().contains(&want) {
                    found = Some((w, name.clone()));
                }
                names.push(name);
            }
            let Some((w, name)) = found else {
                bail!(
                    "no X11 window titled like \"{title}\"; X11 windows open: {}",
                    if names.is_empty() {
                        "none".into()
                    } else {
                        names.join(" · ")
                    }
                );
            };
            let g = conn.get_geometry(w)?.reply()?;
            let img = conn
                .get_image(ImageFormat::Z_PIXMAP, w, 0, 0, g.width, g.height, !0)?
                .reply()?;
            let (width, height) = (u32::from(g.width), u32::from(g.height));
            let px = (width * height) as usize;
            if img.data.len() < px * 4 {
                bail!("unsupported pixel format (depth {})", img.depth);
            }
            // 24/32-bit TrueColor on little-endian X servers: B, G, R, X per pixel.
            let mut rgba = Vec::with_capacity(px * 4);
            for p in img.data.as_chunks::<4>().0.iter().take(px) {
                rgba.extend_from_slice(&[p[2], p[1], p[0], 255]);
            }
            let png = magia_core::media::png_from_rgba(width, height, rgba)?;
            Ok((name, png))
        }
    }
}

#[cfg(not(target_os = "linux"))]
mod imp {
    use super::*;

    pub async fn capture() -> Result<Vec<u8>> {
        bail!("screenshots are only supported on Linux for now")
    }

    pub async fn capture_window(_title: String) -> Result<(String, Vec<u8>)> {
        bail!("screenshots are only supported on Linux for now")
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn png_size_reads_the_header() {
        let mut b = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR".to_vec();
        b.extend(1920u32.to_be_bytes());
        b.extend(1080u32.to_be_bytes());
        assert_eq!(super::png_size(&b), Some((1920, 1080)));
        assert_eq!(super::png_size(b"nope"), None);
    }

    #[tokio::test]
    #[ignore = "takes a real screenshot"]
    async fn live_capture() {
        use magia_tools::Tool;
        let cx = magia_tools::ToolCx::new(std::env::temp_dir());
        let args = match std::env::var("WINDOW") {
            Ok(w) => serde_json::json!({ "window": w }),
            Err(_) => serde_json::json!({}),
        };
        let out = super::Screenshot.call_output(args, &cx).await.unwrap();
        eprintln!("{} · {} bytes", out.text, out.images[0].len());
        std::fs::write(std::env::var("SHOT").unwrap(), &out.images[0]).unwrap();
    }
}
