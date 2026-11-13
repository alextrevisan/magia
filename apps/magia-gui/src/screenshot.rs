//! `screenshot`: lets the model look at the screen (a game it just started, a UI it changed).
//! Linux only, through the desktop portal (`org.freedesktop.portal.Screenshot`) over zbus,
//! which works on Wayland and X11 without a dialog on GNOME and KDE. Registered by the GUI
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
        "Take a screenshot of the whole screen (every monitor) and look at it. Use it to check \
what a program you started or changed looks like (a game window, a GUI). Start the program \
first (in the background with bash, e.g. `godot --path . &`), then call this with `delay` to \
give it time to open. The picture shows everything on screen, so look for the right window. \
To capture one X11 window or a file instead, save a PNG with bash and open it with `read`."
    }
    fn schema(&self) -> Value {
        json!({"type":"object","properties":{
            "delay":{"type":"integer","description":"seconds to wait before the capture (0-30, default 0)"}}})
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
        let bytes = imp::capture().await?;
        let size = magia_types::sniff_image(&bytes)
            .map(|_| png_size(&bytes))
            .unwrap_or_default();
        let text = match size {
            Some((w, h)) => format!("Screenshot of the screen ({w}×{h} px) attached."),
            None => "Screenshot of the screen attached.".into(),
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
    use futures_util::StreamExt;
    use std::collections::HashMap;
    use zbus::zvariant::{OwnedObjectPath, OwnedValue, Value};

    const DEST: &str = "org.freedesktop.portal.Desktop";

    pub async fn capture() -> Result<Vec<u8>> {
        let conn = zbus::Connection::session()
            .await
            .context("no session bus for screenshots")?;
        // Listen on the predictable Request path before calling, so the answer is not missed.
        let token = format!(
            "magiashot{}",
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
        let mut options: HashMap<&str, Value<'_>> = HashMap::new();
        options.insert("handle_token", Value::from(token.as_str()));
        options.insert("interactive", Value::from(false));
        options.insert("modal", Value::from(false));
        let portal = zbus::Proxy::new(
            &conn,
            DEST,
            "/org/freedesktop/portal/desktop",
            "org.freedesktop.portal.Screenshot",
        )
        .await?;
        let _: OwnedObjectPath = portal
            .call("Screenshot", &("", options))
            .await
            .context("the desktop cannot take screenshots (xdg-desktop-portal)")?;
        let msg = tokio::time::timeout(std::time::Duration::from_secs(60), responses.next())
            .await
            .context("the screenshot took too long (is a permission dialog open?)")?
            .context("the screenshot portal closed without answering")?;
        let (code, results): (u32, HashMap<String, OwnedValue>) = msg.body().deserialize()?;
        match code {
            0 => {}
            1 => bail!("the screenshot was refused or cancelled"),
            _ => bail!("the screenshot failed"),
        }
        let uri: String = results
            .get("uri")
            .context("the portal returned no picture")?
            .try_clone()?
            .try_into()?;
        let path = crate::folder::path_from_uri(&uri).context("unexpected picture location")?;
        let bytes = tokio::fs::read(&path)
            .await
            .with_context(|| format!("reading {}", path.display()))?;
        // The portal saved a new file (GNOME: ~/Pictures/Screenshot-N.png) just for us; the
        // picture is in memory now, so don't leave one file per call behind.
        let _ = tokio::fs::remove_file(&path).await;
        Ok(bytes)
    }
}

#[cfg(not(target_os = "linux"))]
mod imp {
    use super::*;

    pub async fn capture() -> Result<Vec<u8>> {
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
        let out = super::Screenshot
            .call_output(serde_json::json!({}), &cx)
            .await
            .unwrap();
        eprintln!("{} · {} bytes", out.text, out.images[0].len());
        std::fs::write(std::env::var("SHOT").unwrap(), &out.images[0]).unwrap();
    }
}