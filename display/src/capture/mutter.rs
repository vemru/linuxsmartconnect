//! `org.gnome.Mutter.ScreenCast` (v4): served by GNOME's Mutter and by niri. No picker, outputs
//! chosen by connector name, and the stream lives on the default PipeWire remote, so
//! `pipewiresrc path=<node>` needs no fd.
//!
//! With `with_input`, the cast session is linked to an `org.gnome.Mutter.RemoteDesktop` session
//! (GNOME only): that session is the one started and stopped, and its `Notify*` calls inject the
//! phone's input relative to the stream (see `input/mutter.rs`).
//!
//! The compositor ends the cast when our D-Bus connection goes away, so `Cast` owns it; Drop
//! calls `Stop`.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use zbus::zvariant::{OwnedObjectPath, OwnedValue, Value};

use crate::input::RemoteInput;

const SC: &str = "org.gnome.Mutter.ScreenCast";
const RD: &str = "org.gnome.Mutter.RemoteDesktop";

pub enum Source<'a> {
    /// An existing monitor, by connector name.
    Monitor(&'a str),
    /// A new virtual monitor (GNOME). It appears once a consumer has negotiated a video size,
    /// which becomes its mode, and goes away when the cast stops.
    Virtual,
}

pub struct Cast {
    pub node_id: u32,
    pub stream_path: OwnedObjectPath,
    /// The stream's `Parameters.size` when the compositor sets it (niri: logical size).
    pub params_size: Option<(i32, i32)>,
    // Field order = drop order: proxies before the connection they use.
    session: zbus::blocking::Proxy<'static>,
    rd: Option<zbus::blocking::Proxy<'static>>,
    _conn: zbus::blocking::Connection,
}

impl Cast {
    pub fn start(source: Source, with_input: bool) -> Result<Cast> {
        let conn = zbus::blocking::Connection::session().context("D-Bus session bus")?;
        let (rd, props) = if with_input {
            let rd_root = zbus::blocking::Proxy::new(&conn, RD, "/org/gnome/Mutter/RemoteDesktop", RD)?;
            let path: OwnedObjectPath = rd_root.call("CreateSession", &()).context("Mutter RemoteDesktop.CreateSession")?;
            let rd = zbus::blocking::Proxy::new(&conn, RD, path, format!("{RD}.Session"))?;
            let id: String = rd.get_property("SessionId").context("RemoteDesktop session id")?;
            (Some(rd), HashMap::from([("remote-desktop-session-id", Value::from(id))]))
        } else {
            (None, HashMap::new())
        };
        let sc = zbus::blocking::Proxy::new(&conn, SC, "/org/gnome/Mutter/ScreenCast", SC)?;
        let session_path: OwnedObjectPath =
            sc.call("CreateSession", &(props,)).context("Mutter ScreenCast.CreateSession")?;
        let session = zbus::blocking::Proxy::new(&conn, SC, session_path, format!("{SC}.Session"))?;
        let rec = HashMap::from([("cursor-mode", Value::U32(1))]); // 1 = embedded
        let stream_path: OwnedObjectPath = match source {
            Source::Monitor(name) => session
                .call("RecordMonitor", &(name, rec))
                .with_context(|| format!("Mutter ScreenCast.Session.RecordMonitor({name})"))?,
            Source::Virtual => session.call("RecordVirtual", &(rec,)).context("Mutter ScreenCast.Session.RecordVirtual")?,
        };
        let stream = zbus::blocking::Proxy::new(&conn, SC, stream_path.clone(), format!("{SC}.Stream"))?;
        // Subscribe before Start so the signal can't be missed.
        let mut added = stream.receive_signal("PipeWireStreamAdded")?;
        let started = match &rd {
            Some(rd) => rd.call::<_, _, ()>("Start", &()).context("Mutter RemoteDesktop.Session.Start"),
            None => session.call::<_, _, ()>("Start", &()).context("Mutter ScreenCast.Session.Start"),
        };
        let mut cast = Cast { node_id: 0, stream_path, params_size: None, session, rd, _conn: conn };
        started?;
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let id = added.next().and_then(|m| m.body().deserialize::<u32>().ok());
            let _ = tx.send(id);
        });
        cast.node_id = match rx.recv_timeout(Duration::from_secs(5)) {
            Ok(Some(id)) => id,
            _ => bail!("no PipeWireStreamAdded from Mutter ScreenCast within 5 s"),
        };
        let params: HashMap<String, OwnedValue> = stream.get_property("Parameters").unwrap_or_default();
        cast.params_size = params.get("size").and_then(|v| <(i32, i32)>::try_from(v.clone()).ok()).filter(|(w, h)| *w > 0 && *h > 0);
        Ok(cast)
    }

    /// The RemoteDesktop handle for input, when the cast was started `with_input`.
    pub fn remote_input(&self, size: (u32, u32), virtual_stream: bool, scale: Arc<dyn Fn() -> f64 + Send + Sync>) -> Option<RemoteInput> {
        let rd = self.rd.clone()?;
        Some(RemoteInput { session: rd, stream: self.stream_path.to_string(), size, virtual_stream, scale })
    }

    pub fn stop(&self) {
        let _ = match &self.rd {
            Some(rd) => rd.call::<_, _, ()>("Stop", &()),
            None => self.session.call::<_, _, ()>("Stop", &()),
        };
    }
}

impl Drop for Cast {
    fn drop(&mut self) {
        self.stop();
    }
}
