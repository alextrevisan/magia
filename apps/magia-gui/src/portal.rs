//! One request to the desktop portal (`org.freedesktop.portal.*`) over zbus: the folder
//! dialog and screenshots both go through here. The portal answers on a `Request` object whose
//! path is predictable from our bus name and a token; we listen there before calling, so the
//! answer cannot be missed.

use anyhow::{Context, Result, bail};
use futures_util::StreamExt;
use std::collections::HashMap;
use std::time::Duration;
use zbus::zvariant::{DynamicType, OwnedObjectPath, OwnedValue, Value};

const DEST: &str = "org.freedesktop.portal.Desktop";

/// The portal's options for one call (`handle_token` is added here).
pub type Options = HashMap<&'static str, Value<'static>>;

/// Call `interface.method` with the body `make(options)` builds and wait for the answer.
/// `Ok(None)` = the user cancelled. `wait` limits the wait (None: as long as it takes, for
/// dialogs the user is using).
pub async fn call<B>(
    interface: &str,
    method: &str,
    mut options: Options,
    make: impl FnOnce(Options) -> B,
    wait: Option<Duration>,
) -> Result<Option<HashMap<String, OwnedValue>>>
where
    B: zbus::export::serde::Serialize + DynamicType,
{
    let conn = zbus::Connection::session()
        .await
        .context("no session bus for the desktop portal")?;
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
    options.insert("handle_token", Value::from(token));
    let portal =
        zbus::Proxy::new(&conn, DEST, "/org/freedesktop/portal/desktop", interface).await?;
    let _: OwnedObjectPath = portal
        .call(method, &make(options))
        .await
        .with_context(|| format!("the desktop has no {interface} (xdg-desktop-portal)"))?;
    let next = responses.next();
    let msg = match wait {
        Some(d) => tokio::time::timeout(d, next)
            .await
            .context("the desktop took too long to answer (is a dialog open?)")?,
        None => next.await,
    }
    .context("the portal closed without answering")?;
    let (code, results): (u32, HashMap<String, OwnedValue>) = msg.body().deserialize()?;
    match code {
        0 => Ok(Some(results)),
        1 => Ok(None),
        _ => bail!("the desktop portal call failed"),
    }
}

/// The first `uri` (or `uris[0]`) of an answer, as a local path.
pub fn path_of(results: &HashMap<String, OwnedValue>) -> Option<std::path::PathBuf> {
    let uri: String = match results.get("uri") {
        Some(v) => v.try_clone().ok()?.try_into().ok()?,
        None => {
            let uris: Vec<String> = results.get("uris")?.try_clone().ok()?.try_into().ok()?;
            uris.into_iter().next()?
        }
    };
    crate::folder::path_from_uri(&uri)
}
