//! Mirror fallback on any desktop: xdg-desktop-portal ScreenCast via ashpd, with
//! persist_mode=ExplicitlyRevoked and the restore token in
//! `$XDG_DATA_HOME/smartconnect-linux/screencast-token`, so the picker appears once. A stale token
//! is deleted and the picker is shown again. The stream lives on the portal's restricted PipeWire
//! remote, so the source description carries `fd=<OpenPipeWireRemote fd>` (pipewiresrc dups it).

use std::os::fd::{AsRawFd, OwnedFd};
use std::path::PathBuf;

use anyhow::{Context, Result, anyhow, bail};
use ashpd::desktop::{
    PersistMode,
    screencast::{CursorMode, Screencast, SelectSourcesOptions, SourceType},
};

use super::{Backend, DMABUF_CONVERT, SHM_CONVERT};
use crate::desktop::{self, Desktop, Output};
use crate::input::Route;

pub struct PortalCast {
    node_id: u32,
    size: (u32, u32),
    geometry: (i32, i32, u32, u32),
    output: String,
    convert: &'static str,
    route: Route,
    /// PipeWire remote fd from OpenPipeWireRemote; pipewiresrc dups it on connect.
    fd: OwnedFd,
    session: ashpd::desktop::Session<Screencast>,
}

impl Backend for PortalCast {
    fn source_description(&self) -> String {
        format!(
            "pipewiresrc fd={} path={} do-timestamp=true keepalive-time=1000 ! {}",
            self.fd.as_raw_fd(),
            self.node_id,
            self.convert
        )
    }

    fn size(&self) -> (u32, u32) {
        self.size
    }

    fn geometry(&self) -> (i32, i32, u32, u32) {
        self.geometry
    }

    fn output_name(&self) -> String {
        self.output.clone()
    }

    fn is_extend(&self) -> bool {
        false
    }

    fn reconfigure_extend(&mut self, _: u32, _: u32) -> Result<()> {
        bail!("a portal capture cannot be resized")
    }

    fn route(&self) -> Route {
        self.route.clone()
    }
}

impl Drop for PortalCast {
    fn drop(&mut self) {
        let _ = pollster::block_on(self.session.close());
    }
}

impl PortalCast {
    /// PipeWire remote fd (tests run gst-launch with it).
    #[cfg(test)]
    pub fn fd(&self) -> i32 {
        self.fd.as_raw_fd()
    }
}

fn token_path() -> PathBuf {
    let base = std::env::var_os("XDG_DATA_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".local/share"));
    base.join("smartconnect-linux/screencast-token")
}

pub async fn start_mirror(desk: Desktop) -> Result<PortalCast> {
    let proxy = Screencast::new().await.context("connecting to xdg-desktop-portal ScreenCast")?;
    let tpath = token_path();
    let saved = std::fs::read_to_string(&tpath).ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
    match try_start(&proxy, saved.as_deref(), desk).await {
        Ok(c) => Ok(c),
        // A stale/revoked token can make the portal fail; retry once with the picker.
        Err(e) if saved.is_some() => {
            eprintln!("capture: restore token failed ({e:#}); asking again");
            let _ = std::fs::remove_file(&tpath);
            try_start(&proxy, None, desk).await
        }
        Err(e) => Err(e),
    }
}

async fn try_start(proxy: &Screencast, token: Option<&str>, desk: Desktop) -> Result<PortalCast> {
    let session = proxy.create_session(Default::default()).await.context("ScreenCast.CreateSession")?;
    proxy
        .select_sources(
            &session,
            SelectSourcesOptions::default()
                .set_cursor_mode(CursorMode::Embedded)
                .set_sources(ashpd::enumflags2::BitFlags::from(SourceType::Monitor))
                .set_multiple(false)
                .set_persist_mode(PersistMode::ExplicitlyRevoked)
                .set_restore_token(token),
        )
        .await
        .context("ScreenCast.SelectSources")?
        .response()
        .context("ScreenCast.SelectSources response")?;
    let streams = proxy
        .start(&session, None, Default::default())
        .await
        .context("ScreenCast.Start")?
        .response()
        .context("ScreenCast.Start (picker cancelled?)")?;

    if let Some(tok) = streams.restore_token() {
        let p = token_path();
        if let Some(dir) = p.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Err(e) = std::fs::write(&p, tok) {
            eprintln!("capture: could not save restore token to {}: {e}", p.display());
        }
    }

    let stream = streams.streams().first().ok_or_else(|| anyhow!("portal returned no streams"))?;
    let node_id = stream.pipe_wire_node_id();
    let fd = proxy
        .open_pipe_wire_remote(&session, Default::default())
        .await
        .context("ScreenCast.OpenPipeWireRemote")?;

    let outs = desktop::outputs(desk).unwrap_or_default();
    let out = match_output(&outs, stream.position(), stream.size());
    let (output, geometry) = match out {
        Some(o) => (o.name.clone(), o.logical.unwrap_or_default()),
        None => {
            let (w, h) = stream.size().ok_or_else(|| anyhow!("unknown stream geometry"))?;
            let (x, y) = stream.position().unwrap_or((0, 0));
            (String::new(), (x, y, w as u32, h as u32))
        }
    };
    let size = match stream.size() {
        Some((w, h)) if w > 0 && h > 0 => (w as u32, h as u32),
        _ => (geometry.2, geometry.3),
    };
    Ok(PortalCast {
        node_id,
        size,
        geometry,
        output,
        // niri's portal backend relays niri's cast, which offers only DMA-BUF.
        convert: if desk == Desktop::Niri { DMABUF_CONVERT } else { SHM_CONVERT },
        route: match desk {
            Desktop::Kde => Route::Kwin,
            _ => Route::Wayland,
        },
        fd,
        session,
    })
}

/// Output matching a portal stream's position/size (the portal reports the logical size).
pub fn match_output<'a>(outs: &'a [Output], pos: Option<(i32, i32)>, size: Option<(i32, i32)>) -> Option<&'a Output> {
    let on: Vec<&Output> = outs.iter().filter(|o| o.logical.is_some()).collect();
    let size_ok = |o: &Output, (w, h): (i32, i32)| o.logical.map(|l| (l.2, l.3)) == Some((w as u32, h as u32));
    if let Some(p) = pos {
        if let Some(o) = on.iter().find(|o| o.logical.map(|l| (l.0, l.1)) == Some(p) && size.is_none_or(|s| size_ok(o, s))) {
            return Some(o);
        }
    }
    if let Some(s) = size {
        if let Some(o) = on.iter().find(|o| size_ok(o, s)) {
            return Some(o);
        }
    }
    (on.len() == 1).then(|| on[0])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_portal_streams_to_outputs() {
        let o = |name: &str, logical| Output { name: name.into(), description: String::new(), logical, focused: false, is_virtual: false };
        let outs = [o("eDP-1", Some((0, 0, 1920, 1080))), o("HDMI-A-1", Some((1920, 0, 1280, 720))), o("DP-1", None)];
        assert_eq!(match_output(&outs, None, Some((1920, 1080))).unwrap().name, "eDP-1");
        assert_eq!(match_output(&outs, Some((1920, 0)), None).unwrap().name, "HDMI-A-1");
        assert!(match_output(&outs, None, Some((800, 600))).is_none());
    }
}
