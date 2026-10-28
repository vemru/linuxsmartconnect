//! xdg-desktop-portal InputCapture backend (GNOME 45+, and any desktop whose portal implements
//! InputCapture). The portal puts a pointer barrier on the chosen edge; when the pointer pushes
//! into it the desktop activates the capture and hands every pointer and keyboard event to us
//! over libei (receiver context, `reis`) instead of the desktop. The [`Core`] decides when to go
//! back; we then `Release` the capture with the PC pointer put just inside the edge.
//!
//! The first start shows the desktop's "allow input capture" prompt. On portal version 2 the
//! session is persistent and the restore token is kept, so later starts don't ask again.

use std::num::NonZero;
use std::os::unix::net::UnixStream;
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use ashpd::desktop::input_capture::{
    ActivatedBarrier, Barrier, Capabilities, CreateSession2Options, CreateSessionOptions, InputCapture, Region, ReleaseOptions,
    StartOptions, Zones,
};
use ashpd::desktop::{PersistMode, Session};
use async_io::{Async, Timer};
use futures_lite::future::or;
use futures_util::StreamExt;
use reis::ei;
use reis::event::{DeviceCapability, EiEvent};

use crate::core::{Action, Core, Edge};

/// Where the portal's restore token is kept (`$XDG_STATE_HOME/smartconnect-linux/`).
fn token_path() -> PathBuf {
    std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state")))
        .unwrap_or_else(std::env::temp_dir)
        .join("smartconnect-linux/crosscontrol-portal.token")
}

/// Saves a portal restore token owner-only (file 0600, app dir 0700). `mode` only applies when the
/// file is created, so an existing token left world-readable by an older version is tightened too.
fn write_token(path: &Path, tok: &str) {
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
        let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
    }
    let saved = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .and_then(|mut f| {
            f.write_all(tok.as_bytes())?;
            f.set_permissions(std::fs::Permissions::from_mode(0o600))
        });
    if let Err(e) = saved {
        eprintln!("portal: could not save restore token to {}: {e}", path.display());
    }
}

/// Creates and starts a capture session: version 2 (persistent, restore token) when the portal
/// has it, else the version 1 call that starts implicitly.
async fn start_session(ic: &InputCapture) -> Result<Session<InputCapture>> {
    let caps = Capabilities::Keyboard | Capabilities::Pointer;
    if ic.version() >= 2 {
        let session = ic.create_session2(CreateSession2Options::default()).await.context("InputCapture.CreateSession2")?;
        let token = std::fs::read_to_string(token_path()).ok().map(|t| t.trim().to_string()).filter(|t| !t.is_empty());
        let opts = StartOptions::default()
            .set_capabilities(caps)
            .set_persist_mode(PersistMode::ExplicitlyRevoked)
            .set_restore_token(token);
        match ic.start(&session, None, opts).await.and_then(|r| r.response()) {
            Ok(resp) => {
                if let Some(t) = resp.restore_token() {
                    write_token(&token_path(), t);
                }
                return Ok(session);
            }
            Err(e) => {
                eprintln!("portal: InputCapture.Start failed ({e}); trying the version 1 session");
                let _ = session.close().await;
            }
        }
    }
    let (session, granted) = ic
        .create_session(None, CreateSessionOptions::default().set_capabilities(caps))
        .await
        .context("InputCapture.CreateSession (was the prompt declined?)")?;
    println!("portal: capture capabilities {granted:?}");
    Ok(session)
}

/// One barrier on the chosen edge of the monitor the phone sits next to (the GTK app saves its
/// rect with the position), else of every zone (monitor). Barriers between two monitors are
/// refused by the desktop, which leaves exactly the outer edge.
fn barriers(zones: &Zones, edge: Edge, span: (f64, f64), anchor: Option<(i32, i32, u32, u32)>) -> Vec<(Barrier, Region)> {
    let regions: Vec<(i32, i32, u32, u32)> =
        zones.regions().iter().map(|r| (r.x_offset(), r.y_offset(), r.width(), r.height())).collect();
    let zones_list = zones.regions();
    barrier_positions(&regions, edge, span, anchor)
        .into_iter()
        .filter_map(|(i, pos)| Some((Barrier::new(NonZero::new(i as u32 + 1)?, pos), zones_list[i])))
        .collect()
}

/// Which zone (monitor) gets a barrier and where, as (zone index, (x1, y1, x2, y2)). The anchor is
/// the rect the GTK page saved for the monitor the phone touches: only that monitor's edge leads
/// to the phone. No anchor, or one that no zone matches (the layout changed): every zone, as before.
fn barrier_positions(
    regions: &[(i32, i32, u32, u32)],
    edge: Edge,
    (s, e): (f64, f64),
    anchor: Option<(i32, i32, u32, u32)>,
) -> Vec<(usize, (i32, i32, i32, i32))> {
    let matching: Vec<usize> =
        anchor.map(|rect| regions.iter().enumerate().filter(|(_, r)| **r == rect).map(|(i, _)| i).collect()).unwrap_or_default();
    if anchor.is_some() && matching.is_empty() {
        eprintln!("portal: the saved monitor position is not among the portal's zones (did the layout change?); arming every monitor edge");
    }
    let chosen: Vec<usize> = if matching.is_empty() { (0..regions.len()).collect() } else { matching };
    chosen
        .into_iter()
        .filter_map(|i| {
            let (x, y, w, h) = regions[i];
            let (x, y, w, h) = (x, y, w as i32, h as i32);
            // Only the part of the edge the phone touches. Always at least one pixel, every pixel
            // of a full span, and never past the monitor's corner (a hand-edited file can say 1.0).
            let seg = |start: f64, end: f64, size: i32| {
                let a = ((start * size as f64).round() as i32).clamp(0, size - 1);
                let b = ((end * size as f64).round() as i32 - 1).clamp(a, size - 1);
                (a, b)
            };
            let (a, b) = seg(s, e, if edge.vertical() { h } else { w });
            let pos = match edge {
                Edge::Left => (x, y + a, x, y + b),
                Edge::Right => (x + w, y + a, x + w, y + b),
                Edge::Top => (x + a, y, x + b, y),
                Edge::Bottom => (x + a, y + h, x + b, y + h),
            };
            Some((i, pos))
        })
        .collect()
}

/// Sets the barriers and returns the zones that got one.
async fn arm(
    ic: &InputCapture,
    session: &Session<InputCapture>,
    edge: Edge,
    span: (f64, f64),
    anchor: Option<(i32, i32, u32, u32)>,
) -> Result<Vec<(u32, Region)>> {
    let zones = ic.zones(session, Default::default()).await?.response().context("InputCapture.GetZones")?;
    let wanted = barriers(&zones, edge, span, anchor);
    let list: Vec<Barrier> = wanted.iter().map(|(b, _)| Barrier::new(b.barrier_id(), b.position())).collect();
    let resp = ic
        .set_pointer_barriers(session, &list, zones.zone_set(), Default::default())
        .await?
        .response()
        .context("InputCapture.SetPointerBarriers")?;
    let failed: Vec<u32> = resp.failed_barriers().iter().map(|id| id.get()).collect();
    let armed: Vec<(u32, Region)> =
        wanted.iter().map(|(b, r)| (b.barrier_id().get(), *r)).filter(|(id, _)| !failed.contains(id)).collect();
    if armed.is_empty() {
        bail!("the desktop accepted no pointer barrier on the {edge:?} edge (an edge shared with another monitor is refused)");
    }
    println!("portal: barrier on the {edge:?} edge of {} of {} monitor(s)", armed.len(), zones.regions().len());
    Ok(armed)
}

/// 0..1 along the edge of `r` for a desktop-coordinate cursor position.
fn along(edge: Edge, r: Region, (cx, cy): (f32, f32)) -> f64 {
    let f = match edge {
        Edge::Left | Edge::Right => (cy as f64 - r.y_offset() as f64) / r.height().max(1) as f64,
        Edge::Top | Edge::Bottom => (cx as f64 - r.x_offset() as f64) / r.width().max(1) as f64,
    };
    f.clamp(0.0, 1.0)
}

/// Desktop position just inside the edge of `r`, `f` along it.
fn inside(edge: Edge, r: Region, f: f64) -> (f64, f64) {
    let (x, y, w, h) = (r.x_offset() as f64, r.y_offset() as f64, r.width() as f64, r.height() as f64);
    match edge {
        Edge::Right => (x + w - 4.0, y + f * h),
        Edge::Left => (x + 3.0, y + f * h),
        Edge::Bottom => (x + f * w, y + h - 4.0),
        Edge::Top => (x + f * w, y + 3.0),
    }
}

pub fn run(core: Core) -> Result<()> {
    // async-io, not tokio: the workspace's other ashpd user (the display helper) uses async-io,
    // and ashpd can't be built with both.
    async_io::block_on(run_async(core))
}

/// One thing that happened, from whichever source was ready first.
enum Ev {
    Activated(ashpd::desktop::input_capture::Activated),
    Deactivated(Option<u32>),
    Disabled,
    ZonesChanged,
    Ei(Option<Result<EiEvent, reis::Error>>),
    Daemon(std::io::Result<()>),
    Tick,
    /// A portal signal stream ended.
    Ended(&'static str),
}

struct Capture {
    /// Activation in progress (the desktop routes input to us).
    activation: Option<u32>,
    /// Zone whose barrier was hit, to place the pointer on release.
    zone: Option<Region>,
}

async fn run_async(mut core: Core) -> Result<()> {
    let edge = core.opts.edge;
    let ic = InputCapture::new().await.context("no InputCapture portal (needs xdg-desktop-portal with a GNOME 45+ or KDE 6.1+ backend)")?;
    let session = start_session(&ic).await?;
    let mut armed = arm(&ic, &session, edge, core.opts.span, core.opts.anchor).await?;

    let fd = ic.connect_to_eis(&session, Default::default()).await.context("InputCapture.ConnectToEIS")?;
    let context = ei::Context::new(UnixStream::from(fd))?;
    let (_connection, mut ei_events) = context
        .handshake_async_io("smartconnect-crosscontrol", ei::handshake::ContextType::Receiver)
        .await
        .context("libei handshake")?;

    let mut activated = ic.receive_activated().await?;
    let mut deactivated = ic.receive_deactivated().await?;
    let mut disabled = ic.receive_disabled().await?;
    let mut zones_changed = ic.receive_zones_changed().await?;
    ic.enable(&session, Default::default()).await.context("InputCapture.Enable")?;

    let watch = Async::new(core.daemon.watch.try_clone()?)?;
    let mut tick = Timer::interval(Duration::from_millis(8));
    let mut cap = Capture { activation: None, zone: None };
    let mut saw_discrete = false;

    if core.opts.accept {
        // The phone asked (svc 26 cmd 0) before we were running: answer it now; the edge is then armed.
        core.request_control();
    }
    println!("ready: push the pointer into the {edge:?} edge; push back or Ctrl+Alt+Esc to return; Ctrl+C quits");

    let result: Result<()> = loop {
        if crate::STOP.load(Ordering::SeqCst) {
            break Ok(());
        }
        let mut action = Action::None;
        let ev = or(
            or(
                or(
                    async { activated.next().await.map_or(Ev::Ended("Activated"), Ev::Activated) },
                    async { deactivated.next().await.map_or(Ev::Ended("Deactivated"), |d| Ev::Deactivated(d.activation_id())) },
                ),
                or(
                    async { disabled.next().await.map_or(Ev::Ended("Disabled"), |_| Ev::Disabled) },
                    async { zones_changed.next().await.map_or(Ev::Ended("ZonesChanged"), |_| Ev::ZonesChanged) },
                ),
            ),
            or(
                or(async { Ev::Ei(ei_events.next().await) }, async { Ev::Daemon(watch.readable().await) }),
                async {
                    tick.next().await;
                    Ev::Tick
                },
            ),
        )
        .await;
        match ev {
            Ev::Activated(a) => {
                let id = match a.barrier_id() {
                    Some(ActivatedBarrier::Barrier(id)) => Some(id.get()),
                    _ => None,
                };
                // The monitor that was hit: by barrier id, else by where the cursor is. Only fall
                // back to "the only armed monitor" — never to the first of several.
                let zone = id
                    .and_then(|id| armed.iter().find(|(b, _)| *b == id).map(|(_, r)| *r))
                    .or_else(|| {
                        let (x, y) = a.cursor_position()?;
                        armed.iter().map(|(_, r)| *r).find(|r| {
                            let (rx, ry) = (r.x_offset() as f32, r.y_offset() as f32);
                            x >= rx && x < rx + r.width() as f32 && y >= ry && y < ry + r.height() as f32
                        })
                    })
                    .or_else(|| (armed.len() == 1).then(|| armed[0].1));
                cap.activation = a.activation_id();
                cap.zone = zone;
                if let Some(r) = zone {
                    let f = match a.cursor_position() {
                        Some(pos) => along(edge, r, pos),
                        None => 0.5,
                    };
                    action = core.edge_hit(f);
                }
            }
            Ev::Deactivated(id) => {
                // The desktop ended the capture itself (e.g. its own release shortcut).
                if id.is_none() || id == cap.activation {
                    cap.activation = None;
                    core.leave("released by the desktop");
                }
            }
            Ev::Disabled => {
                core.leave("capture disabled by the desktop");
                break Err(anyhow::anyhow!("the desktop disabled input capture (permission revoked or session closed)"));
            }
            Ev::ZonesChanged => {
                // Monitors changed: re-arm. The portal needs the session disabled for that.
                core.leave("monitors changed");
                let _ = ic.disable(&session, Default::default()).await;
                match arm(&ic, &session, edge, core.opts.span, core.opts.anchor).await {
                    Ok(a) => armed = a,
                    Err(e) => break Err(e),
                }
                if let Err(e) = ic.enable(&session, Default::default()).await {
                    break Err(anyhow::Error::from(e).context("re-enabling input capture after a monitor change"));
                }
            }
            Ev::Ended(what) => break Err(anyhow::anyhow!("the portal's {what} signal stream ended")),
            Ev::Ei(ev) => {
                let ev = match ev {
                    Some(Ok(ev)) => ev,
                    Some(Err(e)) => break Err(anyhow::Error::from(e).context("libei")),
                    None => break Err(anyhow::anyhow!("libei connection closed")),
                };
                match ev {
                    EiEvent::SeatAdded(s) => {
                        s.seat.bind_capabilities(DeviceCapability::Pointer | DeviceCapability::Keyboard | DeviceCapability::Button | DeviceCapability::Scroll);
                        let _ = context.flush();
                    }
                    EiEvent::PointerMotion(m) => action = core.on_relative(m.dx as f64, m.dy as f64),
                    EiEvent::Button(b) => core.on_button(b.button, b.state == ei::button::ButtonState::Press),
                    EiEvent::ScrollDiscrete(s) => {
                        saw_discrete = true;
                        core.on_scroll(s.discrete_dy as f64 / 120.0);
                    }
                    EiEvent::ScrollDelta(s) if !saw_discrete => core.on_scroll(s.dy as f64 / 15.0),
                    EiEvent::KeyboardKey(k) => action = core.on_key(k.key, k.state == ei::keyboard::KeyState::Press),
                    EiEvent::Disconnected(d) => break Err(anyhow::anyhow!("libei disconnected: {:?}", d.reason)),
                    _ => {}
                }
            }
            Ev::Daemon(ready) => {
                if let Err(e) = ready {
                    break Err(e.into());
                }
                match core.daemon.poll_lines() {
                    Ok(msgs) => {
                        for m in msgs {
                            match core.on_daemon(m) {
                                Action::None => {}
                                a => action = a,
                            }
                        }
                    }
                    Err(e) => {
                        core.leave("daemon gone");
                        break Err(e);
                    }
                }
            }
            Ev::Tick => core.tick(),
        }
        match action {
            Action::None => {}
            Action::Enter => core.enter(),
            Action::Leave(why) => {
                core.leave(why);
            }
        }
        // Keep the desktop's capture in step with the core: give input back whenever the core
        // no longer wants it (left the phone, request denied or timed out, cooldown).
        if let Some(id) = cap.activation
            && !core.wants_capture()
        {
            let pos = cap.zone.map(|r| inside(edge, r, core.edge_fraction()));
            let opts = ReleaseOptions::default().set_activation_id(id).set_cursor_position(pos);
            if let Err(e) = ic.release(&session, opts).await {
                eprintln!("portal: release failed: {e}");
            }
            cap.activation = None;
        }
    };
    println!("exiting");
    core.leave("exit");
    if let Some(id) = cap.activation {
        let _ = ic.release(&session, ReleaseOptions::default().set_activation_id(id)).await;
    }
    core.shutdown();
    let _ = ic.disable(&session, Default::default()).await;
    let _ = session.close().await;
    result
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    fn mode(p: &Path) -> u32 {
        std::fs::metadata(p).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn restore_tokens_are_written_owner_only() {
        let dir = std::env::temp_dir().join(format!("cc-tok-{}", std::process::id()));
        let p = dir.join("smartconnect-linux/crosscontrol-portal.token");
        // A token (and folder) left by an older version: world-readable.
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, "old").unwrap();
        std::fs::set_permissions(p.parent().unwrap(), std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o644)).unwrap();

        write_token(&p, "fresh");
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "fresh");
        assert_eq!(mode(&p), 0o600);
        assert_eq!(mode(p.parent().unwrap()), 0o700);
        std::fs::remove_dir_all(&dir).ok();
    }

    use super::*;

    /// The user's two-monitor layout: HDMI-1 2560×1440 at (0,0), the laptop panel below it.
    fn zones() -> Vec<(i32, i32, u32, u32)> {
        vec![(0, 0, 2560, 1440), (250, 1440, 1920, 1080)]
    }

    #[test]
    fn only_the_monitor_the_phone_touches_gets_a_barrier() {
        let z = zones();
        // The phone sits next to the laptop panel: one barrier, on eDP-1's right edge only.
        let got = barrier_positions(&z, Edge::Right, (0.25, 0.75), Some((250, 1440, 1920, 1080)));
        assert_eq!(got.len(), 1, "the external monitor's right edge must not enter the phone: {got:?}");
        assert_eq!(got[0].0, 1, "the barrier belongs to eDP-1");
        // eDP-1's right edge, over the part of the edge the phone covers (25 %..75 % of 1080 px).
        assert_eq!(got[0].1, (2170, 1710, 2170, 2249));
        // Other edges work the same way.
        let got = barrier_positions(&z, Edge::Bottom, (0.0, 1.0), Some((0, 0, 2560, 1440)));
        assert_eq!(got, vec![(0, (0, 1440, 2559, 1440))]);
        // No anchor (older position file): every monitor, as before.
        assert_eq!(barrier_positions(&z, Edge::Right, (0.0, 1.0), None).len(), 2);
        // The saved monitor is gone / the layout changed: never arm nothing at all.
        assert_eq!(barrier_positions(&z, Edge::Right, (0.0, 1.0), Some((0, 0, 1, 1))).len(), 2);
        // A single monitor: the anchor matches or falls back, and only one barrier is set either way.
        assert_eq!(barrier_positions(&z[..1], Edge::Right, (0.0, 1.0), Some((0, 0, 2560, 1440))).len(), 1);
    }

    #[test]
    fn the_span_covers_the_phone_and_nothing_else() {
        let z = [(0, 0, 2560, 1440)];
        let got = barrier_positions(&z, Edge::Right, (0.5, 0.5), Some((0, 0, 2560, 1440)));
        // A zero-width span still yields a 1-px barrier, never an inverted or empty one.
        assert_eq!(got, vec![(0, (2560, 720, 2560, 720))]);        let got = barrier_positions(&z, Edge::Left, (0.0, 1.0), None);
        assert_eq!(got, vec![(0, (0, 0, 0, 1439))], "the full height, last pixel inclusive");
    }
}
