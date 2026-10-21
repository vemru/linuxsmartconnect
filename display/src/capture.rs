//! Owned by the capture agent. Contract used by `session`:
//!
//! ```ignore
//! pub enum Mode {
//!     /// Record the focused physical output (eDP-1 on the laptop).
//!     Mirror,
//!     /// Turn on a spare output (first "Virtual-*" vkms output unless `output` names one,
//!     /// e.g. an HDMI dummy plug "HDMI-A-1"), give it `width`x`height`@60, place it right of
//!     /// the existing outputs and record it. It is turned back off when the Capture is dropped.
//!     /// `scale` sets the output's niri scale (e.g. 2.0 = 200%) before its geometry is read.
//!     Extend { width: u32, height: u32, output: Option<String>, scale: Option<f64> },
//! }
//! pub struct Capture { pub width: u32, pub height: u32, /* keeps the cast session alive */ }
//! /// Starts screen capture. No dialog on niri (org.gnome.Mutter.ScreenCast by connector);
//! /// Mirror falls back to the xdg-desktop-portal picker (shown once, token persisted).
//! pub fn start(mode: Mode) -> anyhow::Result<Capture>;
//! impl Capture {
//!     /// gst-launch fragment producing system-memory raw video:
//!     /// "pipewiresrc path=<node> … ! vapostproc ! video/x-raw ! videoconvert".
//!     pub fn source_description(&self) -> String;
//!     /// Screen rectangle (in compositor logical px) that the captured stream covers, for input mapping.
//!     pub fn geometry(&self) -> (i32, i32, u32, u32);
//!     /// niri output (connector) name being captured, e.g. "eDP-1" / "Virtual-1".
//!     pub fn output_name(&self) -> &str;
//!     /// Extend: new size (portrait when height > width) without switching the output off;
//!     /// starts a new cast, so re-read source_description()/geometry().
//!     pub fn reconfigure_extend(&mut self, width: u32, height: u32) -> anyhow::Result<()>;
//! }
//! ```
//!
//! Implementation notes:
//! - Primary path: niri implements Mutter's private ScreenCast D-Bus API (v4) itself, so we
//!   call `ScreenCast.CreateSession` → `Session.RecordMonitor(connector)` → `Session.Start` and
//!   wait for `Stream.PipeWireStreamAdded(node_id)`. No picker. The stream lives on the default
//!   PipeWire remote, so `pipewiresrc path=<node>` needs no fd. niri ends the cast when our
//!   D-Bus connection goes away, so `Capture` owns the connection; Drop calls `Session.Stop`.
//! - Fallback (Mirror only): xdg-desktop-portal ScreenCast via ashpd (on niri served by
//!   xdg-desktop-portal-gnome), persist_mode=ExplicitlyRevoked with the restore token in
//!   `$XDG_DATA_HOME/smartconnect-linux/screencast-token`.
//! - Extend uses `niri msg output <name> on|custom-mode|mode|transform|position|off` (temporary
//!   config, never written to the user's config file, which keeps Virtual-1 `off` by default).
//!   Resizing/rotating keeps the output on and only swaps the cast (`reconfigure_extend`).

#![allow(dead_code)]

use std::collections::HashMap;
use std::os::fd::{AsRawFd, OwnedFd};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use zbus::zvariant::{OwnedObjectPath, OwnedValue, Value};

#[derive(Debug, Clone, PartialEq)]
pub enum Mode {
    Mirror,
    Extend { width: u32, height: u32, output: Option<String>, scale: Option<f64> },
}

pub struct Capture {
    pub width: u32,
    pub height: u32,
    output: String,
    geometry: (i32, i32, u32, u32),
    node_id: u32,
    backend: Backend,
    /// Output we switched on for Extend; switched back off on Drop (after the cast stops).
    enabled_output: Option<String>,
    /// Extend: the niri scale we gave the output (kept when it is resized).
    scale: Option<f64>,
    /// Extend: the output is turned with `transform 90` (its connector had no mode of the wanted
    /// orientation); set back to `normal` before it is switched off.
    transformed: bool,
}

enum Backend {
    Mutter {
        // Field order = drop order: proxy before the connection it uses.
        session: zbus::blocking::Proxy<'static>,
        _conn: zbus::blocking::Connection,
    },
    Portal {
        /// PipeWire remote fd from OpenPipeWireRemote; pipewiresrc dups it on connect.
        fd: OwnedFd,
        session: ashpd::desktop::Session<ashpd::desktop::screencast::Screencast>,
    },
}

/// Downstream of pipewiresrc. niri offers only DMA-BUF (BGRx + modifiers, no SHM), which
/// `videoconvert` can't take and `glupload` fails to import; VA-API `vapostproc` imports it and
/// hands out system-memory raw video. (Zero-copy alternative for the session: `vapostproc !
/// vah264enc`, i.e. drop the `video/x-raw ! videoconvert` tail.)
const CONVERT: &str = "vapostproc ! video/x-raw ! videoconvert";

impl Capture {
    pub fn source_description(&self) -> String {
        let common = "do-timestamp=true keepalive-time=1000";
        match &self.backend {
            Backend::Mutter { .. } => {
                format!("pipewiresrc path={} {common} ! {CONVERT}", self.node_id)
            }
            Backend::Portal { fd, .. } => format!(
                "pipewiresrc fd={} path={} {common} ! {CONVERT}",
                fd.as_raw_fd(),
                self.node_id
            ),
        }
    }

    pub fn geometry(&self) -> (i32, i32, u32, u32) {
        self.geometry
    }

    pub fn output_name(&self) -> &str {
        &self.output
    }

    pub fn node_id(&self) -> u32 {
        self.node_id
    }

    /// Extend only: whether `reconfigure_extend` can resize this capture's output in place.
    pub fn is_extend(&self) -> bool {
        self.enabled_output.is_some()
    }

    /// Extend only: switches the output to `width`x`height` WITHOUT turning it off (so its windows
    /// stay on it and the injected pointer stays pinned to it) and records it again: the old cast
    /// is stopped and a new PipeWire node replaces it (re-read `source_description()`). Used for
    /// the phone's resolution menu (cmd 70) and rotation (cmd 7): `height > width` = portrait.
    /// On error the output is still on and still owned by this Capture (drop it to switch it off).
    pub fn reconfigure_extend(&mut self, width: u32, height: u32) -> Result<()> {
        self.reconfigure(width, height, false)
    }

    /// `force_transform` (tests): rotate with `transform 90` even if a portrait custom mode works.
    fn reconfigure(&mut self, width: u32, height: u32, force_transform: bool) -> Result<()> {
        let name = self.enabled_output.clone().ok_or_else(|| anyhow!("only an extend capture can be resized"))?;
        // Stop the current cast first; niri would otherwise keep rendering it at the new size.
        if let Backend::Mutter { session, .. } = &self.backend {
            let _ = session.call::<_, _, ()>("Stop", &());
        }
        let pos = (self.geometry.0, self.geometry.1);
        let applied = apply_extend_mode(&name, width, height, pos, self.scale, self.transformed, force_transform);
        let (out, transformed) = match applied {
            Ok(r) => r,
            Err(e) => {
                self.transformed = true; // unknown now: make Drop put `normal` back
                return Err(e);
            }
        };
        self.transformed = transformed;
        let mut fresh = start_mutter(&out).with_context(|| format!("recording {name} at {width}x{height}"))?;
        // `fresh` takes the old (stopped) session with it; it never switched an output on, so
        // dropping it leaves ours alone.
        std::mem::swap(&mut self.backend, &mut fresh.backend);
        self.node_id = fresh.node_id;
        self.width = fresh.width;
        self.height = fresh.height;
        self.geometry = fresh.geometry;
        Ok(())
    }

    /// PipeWire remote fd (portal fallback only).
    pub fn fd(&self) -> Option<i32> {
        match &self.backend {
            Backend::Portal { fd, .. } => Some(fd.as_raw_fd()),
            Backend::Mutter { .. } => None,
        }
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        match &self.backend {
            Backend::Mutter { session, .. } => {
                let _ = session.call::<_, _, ()>("Stop", &());
            }
            Backend::Portal { session, .. } => {
                let _ = pollster::block_on(session.close());
            }
        }
        if let Some(name) = &self.enabled_output {
            switch_off(name, self.transformed);
        }
    }
}

/// Turns an extend output back off (and upright first, so the next session starts unrotated).
fn switch_off(name: &str, transformed: bool) {
    if transformed {
        let _ = niri_msg(&["output", name, "transform", "normal"]);
    }
    if let Err(e) = niri_msg(&["output", name, "off"]) {
        eprintln!("capture: failed to turn {name} back off: {e:#}");
    }
}

pub fn start(mode: Mode) -> Result<Capture> {
    match mode {
        Mode::Mirror => {
            let outs = niri_outputs()?;
            let out = primary_output(&outs, None)
                .ok_or_else(|| anyhow!("no enabled output to mirror"))?
                .clone();
            match start_mutter(&out) {
                Ok(c) => Ok(c),
                Err(e) => {
                    eprintln!(
                        "capture: Mutter ScreenCast API failed ({e:#}); falling back to the portal"
                    );
                    pollster::block_on(portal::start_mirror())
                }
            }
        }
        Mode::Extend { width, height, output, scale } => {
            let (out, transformed) = enable_extend_output(width, height, output.as_deref(), scale)?;
            let name = out.name.clone();
            match start_mutter(&out) {
                Ok(mut c) => {
                    c.enabled_output = Some(name);
                    c.scale = scale;
                    c.transformed = transformed;
                    Ok(c)
                }
                Err(e) => {
                    switch_off(&name, transformed);
                    Err(e.context(format!("recording extend output {name}")))
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------
// org.gnome.Mutter.ScreenCast (served by niri itself)

const MUTTER_SC: &str = "org.gnome.Mutter.ScreenCast";

fn start_mutter(out: &NiriOutput) -> Result<Capture> {
    let geometry = out.logical.ok_or_else(|| anyhow!("output {} is off", out.name))?;
    let conn = zbus::blocking::Connection::session().context("D-Bus session bus")?;
    let sc = zbus::blocking::Proxy::new(&conn, MUTTER_SC, "/org/gnome/Mutter/ScreenCast", MUTTER_SC)?;
    let session_path: OwnedObjectPath = sc
        .call("CreateSession", &(HashMap::<&str, Value>::new(),))
        .context("Mutter ScreenCast.CreateSession")?;
    let session = zbus::blocking::Proxy::new(
        &conn,
        MUTTER_SC,
        session_path,
        "org.gnome.Mutter.ScreenCast.Session",
    )?;
    let props = HashMap::from([("cursor-mode", Value::U32(1))]); // 1 = embedded
    let stream_path: OwnedObjectPath = session
        .call("RecordMonitor", &(out.name.as_str(), props))
        .with_context(|| format!("Mutter ScreenCast.Session.RecordMonitor({})", out.name))?;
    let stream = zbus::blocking::Proxy::new(
        &conn,
        MUTTER_SC,
        stream_path,
        "org.gnome.Mutter.ScreenCast.Stream",
    )?;
    // Subscribe before Start so the signal can't be missed.
    let mut added = stream.receive_signal("PipeWireStreamAdded")?;
    session.call::<_, _, ()>("Start", &()).context("Mutter ScreenCast.Session.Start")?;
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let id = added.next().and_then(|m| m.body().deserialize::<u32>().ok());
        let _ = tx.send(id);
    });
    let node_id = match rx.recv_timeout(Duration::from_secs(5)) {
        Ok(Some(id)) => id,
        _ => {
            let _ = session.call::<_, _, ()>("Stop", &());
            bail!("no PipeWireStreamAdded from Mutter ScreenCast within 5 s");
        }
    };

    // Stream size in buffer pixels. niri casts at the output's mode size, but its stream
    // `Parameters.size` is the logical size (they differ when the output is scaled), so prefer the mode.
    let params: HashMap<String, OwnedValue> =
        stream.get_property("Parameters").unwrap_or_default();
    let size = params.get("size").and_then(|v| <(i32, i32)>::try_from(v.clone()).ok());
    let (width, height) = match (out.pixel_size(), size) {
        (Some(px), _) => px,
        (None, Some((w, h))) if w > 0 && h > 0 => (w as u32, h as u32),
        _ => (geometry.2, geometry.3),
    };

    Ok(Capture {
        width,
        height,
        output: out.name.clone(),
        geometry,
        node_id,
        backend: Backend::Mutter { session, _conn: conn },
        enabled_output: None,
        scale: None,
        transformed: false,
    })
}

// ---------------------------------------------------------------------------------------------
// niri output handling

#[derive(Debug, Clone)]
struct NiriOutput {
    name: String,
    /// Logical rect (x, y, w, h); None when the output is off.
    logical: Option<(i32, i32, u32, u32)>,
    /// Index into `modes` of the current mode.
    current: Option<usize>,
    /// (width, height, refresh mHz).
    modes: Vec<(u32, u32, u32)>,
    rotated: bool,
    /// niri output scale (1.0 when off).
    scale: f64,
}

impl NiriOutput {
    fn is_virtual(&self) -> bool {
        self.name.starts_with("Virtual-")
    }

    /// Current mode size, rotated like the logical rect.
    fn pixel_size(&self) -> Option<(u32, u32)> {
        let (w, h, _) = *self.modes.get(self.current?)?;
        Some(if self.rotated { (h, w) } else { (w, h) })
    }
}

fn niri_msg(args: &[&str]) -> Result<String> {
    let out = std::process::Command::new("niri")
        .arg("msg")
        .args(args)
        .output()
        .context("running `niri msg` (is niri running?)")?;
    if !out.status.success() {
        bail!(
            "`niri msg {}` failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn niri_outputs() -> Result<Vec<NiriOutput>> {
    parse_outputs(&niri_msg(&["--json", "outputs"])?)
}

fn parse_outputs(json: &str) -> Result<Vec<NiriOutput>> {
    let v: serde_json::Value = serde_json::from_str(json).context("parsing niri outputs")?;
    let map = v.as_object().ok_or_else(|| anyhow!("niri outputs: not an object"))?;
    let mut res: Vec<NiriOutput> = map
        .iter()
        .map(|(name, o)| {
            let logical = o.get("logical").filter(|l| l.is_object()).map(|l| {
                let g = |k: &str| l.get(k).and_then(|x| x.as_i64()).unwrap_or(0);
                (g("x") as i32, g("y") as i32, g("width") as u32, g("height") as u32)
            });
            let rotated = o
                .pointer("/logical/transform")
                .and_then(|t| t.as_str())
                .is_some_and(|t| t.contains("90") || t.contains("270"));
            let modes = o
                .get("modes")
                .and_then(|m| m.as_array())
                .map(|a| {
                    a.iter()
                        .map(|m| {
                            let g = |k: &str| m.get(k).and_then(|x| x.as_u64()).unwrap_or(0) as u32;
                            (g("width"), g("height"), g("refresh_rate"))
                        })
                        .collect()
                })
                .unwrap_or_default();
            let current = o.get("current_mode").and_then(|c| c.as_u64()).map(|c| c as usize);
            let scale = o.pointer("/logical/scale").and_then(|s| s.as_f64()).unwrap_or(1.0);
            NiriOutput { name: name.clone(), logical, current, modes, rotated, scale }
        })
        .collect();
    res.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(res)
}

/// Focused output if it is a real, enabled one; else the first enabled non-virtual output;
/// else any enabled output. `exclude` skips the output being set up for Extend.
fn primary_output<'a>(outs: &'a [NiriOutput], exclude: Option<&str>) -> Option<&'a NiriOutput> {
    let usable = |o: &&NiriOutput| o.logical.is_some() && Some(o.name.as_str()) != exclude;
    let focused = niri_msg(&["--json", "focused-output"])
        .ok()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
        .and_then(|v| v.get("name").and_then(|n| n.as_str()).map(str::to_owned));
    outs.iter()
        .filter(usable)
        .find(|o| !o.is_virtual() && Some(&o.name) == focused.as_ref())
        .or_else(|| outs.iter().filter(usable).find(|o| !o.is_virtual()))
        .or_else(|| outs.iter().find(usable))
}

/// Mode closest to the request: same orientation first, then smallest size difference,
/// then closest to 60 Hz.
fn nearest_mode(modes: &[(u32, u32, u32)], w: u32, h: u32) -> Option<(u32, u32, u32)> {
    let landscape = w >= h;
    modes.iter().copied().min_by_key(|&(mw, mh, r)| {
        let orient = u64::from((mw >= mh) != landscape);
        let diff = (i64::from(mw) - i64::from(w)).unsigned_abs()
            + (i64::from(mh) - i64::from(h)).unsigned_abs();
        let rate = (i64::from(r) - 60_000).unsigned_abs();
        (orient, diff, rate)
    })
}

/// Poll niri until `name` is on with the given pixel size (and position and scale, if given).
fn wait_for_output(
    name: &str,
    size: (u32, u32),
    pos: Option<(i32, i32)>,
    scale: Option<f64>,
    timeout: Duration,
) -> Option<NiriOutput> {
    let deadline = Instant::now() + timeout;
    loop {
        let found = niri_outputs()
            .ok()
            .and_then(|outs| outs.into_iter().find(|o| o.name == name))
            .filter(|o| {
                o.logical.is_some()
                    && o.pixel_size() == Some(size)
                    && pos.is_none_or(|p| o.logical.map(|l| (l.0, l.1)) == Some(p))
                    && scale.is_none_or(|s| (o.scale - s).abs() < 0.02)
            });
        if found.is_some() || Instant::now() >= deadline {
            return found;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Switches the extend output on at `width`x`height` (see `apply_extend_mode`). Returns it and
/// whether it had to be turned with a transform.
fn enable_extend_output(width: u32, height: u32, wanted: Option<&str>, scale: Option<f64>) -> Result<(NiriOutput, bool)> {
    let outs = niri_outputs()?;
    let target = match wanted {
        Some(n) => outs.iter().find(|o| o.name == n).ok_or_else(|| {
            let names: Vec<_> = outs.iter().map(|o| o.name.as_str()).collect();
            anyhow!("output {n:?} is not connected (niri sees: {})", names.join(", "))
        })?,
        None => outs.iter().find(|o| o.is_virtual()).ok_or_else(|| {
            anyhow!(
                "no virtual output for extend mode: run `sudo modprobe vkms` (creates \
                 \"Virtual-1\"; add `vkms` to /etc/modules-load.d/ to keep it), or plug an \
                 HDMI dummy and pass its connector name"
            )
        })?,
    }
    .clone();
    let name = target.name.as_str();

    // Right of everything else, top-aligned with the primary output.
    let x = outs
        .iter()
        .filter(|o| o.name != name)
        .filter_map(|o| o.logical.map(|l| l.0 + l.2 as i32))
        .max()
        .unwrap_or(0);
    let y = primary_output(&outs, Some(name)).and_then(|o| o.logical).map_or(0, |l| l.1);
    let (xs, ys) = (x.to_string(), y.to_string());

    // niri lays out explicitly positioned outputs first and auto-places the rest after them, so
    // positioning only the virtual output would push an unpositioned laptop panel to its right.
    // Pin every other output where it is now (temporary, like the rest of this).
    for o in outs.iter().filter(|o| o.name != name) {
        if let Some((ox, oy, ..)) = o.logical {
            let _ = niri_msg(&["output", &o.name, "position", "set", &ox.to_string(), &oy.to_string()]);
        }
    }

    // `on` first: it overrides an `output "…" { off }` in the config (temporary, until Drop).
    niri_msg(&["output", name, "on"])?;
    // A transform left behind by a killed session would turn the picture: start upright.
    let _ = niri_msg(&["output", name, "transform", "normal"]);
    // The scale changes the logical size, so set it before the geometry is read for input mapping.
    if let Some(s) = scale {
        niri_msg(&["output", name, "scale", &s.to_string()])?;
    }
    niri_msg(&["output", name, "position", "set", &xs, &ys])?;
    match apply_extend_mode(name, width, height, (x, y), scale, false, false) {
        Ok(r) => Ok(r),
        Err(e) => {
            switch_off(name, true);
            Err(e)
        }
    }
}

/// Gives the (on, positioned) output `name` the pixel size `width`x`height` (`height > width` =
/// portrait): a custom mode of exactly that size first (vkms takes any), else the connector's
/// nearest listed mode, turned with `transform 90` when no listed mode has the wanted
/// orientation. `pos` is where the output sits (unchanged). Returns the output once niri reports
/// the new size and whether it is transformed (`was_transformed`: it is now, undo that first).
fn apply_extend_mode(
    name: &str,
    width: u32,
    height: u32,
    pos: (i32, i32),
    scale: Option<f64>,
    was_transformed: bool,
    force_transform: bool,
) -> Result<(NiriOutput, bool)> {
    let portrait = height > width;
    let set_transform = |rotate: bool| niri_msg(&["output", name, "transform", if rotate { "90" } else { "normal" }]);
    if force_transform {
        // Tests: a landscape custom mode turned to portrait by the compositor.
        let (lw, lh) = (width.max(height), width.min(height));
        niri_msg(&["output", name, "custom-mode", &format!("{lw}x{lh}@60")])?;
        set_transform(portrait)?;
        return match wait_for_output(name, (width, height), Some(pos), scale, Duration::from_secs(3)) {
            Some(o) => Ok((o, portrait)),
            None => bail!("{name} did not come up as {width}x{height} with a transform"),
        };
    }
    if was_transformed {
        set_transform(false)?;
    }
    if niri_msg(&["output", name, "custom-mode", &format!("{width}x{height}@60")]).is_ok()
        && let Some(o) = wait_for_output(name, (width, height), Some(pos), scale, Duration::from_secs(2))
    {
        return Ok((o, false));
    }

    // Custom mode refused (real connectors often are picky): use the nearest listed mode. A
    // connector without a mode of the wanted orientation (EDIDs rarely list portrait modes) gets
    // the nearest one of the other orientation, turned by 90°.
    let modes = niri_outputs()?.into_iter().find(|o| o.name == name).map(|o| o.modes).unwrap_or_default();
    let (mw, mh, mr, rotate) = pick_fallback_mode(&modes, width, height)
        .ok_or_else(|| anyhow!("{name}: custom mode {width}x{height} failed and no modes are listed"))?;
    let turned = if rotate { ", turned by 90°" } else { "" };
    eprintln!("capture: {name}: custom mode {width}x{height} not applied; using {mw}x{mh}{turned}");
    let (xs, ys) = (pos.0.to_string(), pos.1.to_string());
    let set = niri_msg(&["output", name, "mode", &format!("{mw}x{mh}@{:.3}", f64::from(mr) / 1000.0)])
        .and_then(|_| set_transform(rotate))
        .and_then(|_| niri_msg(&["output", name, "position", "set", &xs, &ys]));
    let size = if rotate { (mh, mw) } else { (mw, mh) };
    match set.ok().and_then(|_| wait_for_output(name, size, None, scale, Duration::from_secs(3))) {
        Some(o) => Ok((o, rotate)),
        None => bail!("{name} did not come up with mode {mw}x{mh}{turned}"),
    }
}

/// The listed mode to use when a custom `width`x`height` is refused: (w, h, refresh mHz, turn by
/// 90°). The nearest mode of the same orientation, else the nearest of the other, turned.
fn pick_fallback_mode(modes: &[(u32, u32, u32)], width: u32, height: u32) -> Option<(u32, u32, u32, bool)> {
    let portrait = height > width;
    let same = modes.iter().any(|&(mw, mh, _)| (mh > mw) == portrait);
    let (rw, rh, rotate) = if same || modes.is_empty() { (width, height, false) } else { (height, width, true) };
    nearest_mode(modes, rw, rh).map(|(mw, mh, mr)| (mw, mh, mr, rotate))
}

// ---------------------------------------------------------------------------------------------
// xdg-desktop-portal fallback (Mirror only)

mod portal {
    use super::*;
    use ashpd::desktop::{
        PersistMode,
        screencast::{CursorMode, Screencast, SelectSourcesOptions, SourceType},
    };

    fn token_path() -> PathBuf {
        let base = std::env::var_os("XDG_DATA_HOME")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".local/share")
            });
        base.join("smartconnect-linux/screencast-token")
    }

    pub(super) async fn start_mirror() -> Result<Capture> {
        let proxy = Screencast::new()
            .await
            .context("connecting to xdg-desktop-portal ScreenCast")?;
        let tpath = token_path();
        let saved = std::fs::read_to_string(&tpath)
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        match try_start(&proxy, saved.as_deref()).await {
            Ok(c) => Ok(c),
            // A stale/revoked token can make the portal fail; retry once with the picker.
            Err(e) if saved.is_some() => {
                eprintln!("capture: restore token failed ({e:#}); asking again");
                let _ = std::fs::remove_file(&tpath);
                try_start(&proxy, None).await
            }
            Err(e) => Err(e),
        }
    }

    async fn try_start(proxy: &Screencast, token: Option<&str>) -> Result<Capture> {
        let session = proxy
            .create_session(Default::default())
            .await
            .context("ScreenCast.CreateSession")?;
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

        let stream = streams
            .streams()
            .first()
            .ok_or_else(|| anyhow!("portal returned no streams"))?;
        let node_id = stream.pipe_wire_node_id();
        let fd = proxy
            .open_pipe_wire_remote(&session, Default::default())
            .await
            .context("ScreenCast.OpenPipeWireRemote")?;

        let outs = niri_outputs().unwrap_or_default();
        let out = match_output(&outs, stream.position(), stream.size());
        let (output, geometry) = match out {
            Some(o) => (o.name.clone(), o.logical.unwrap_or_default()),
            None => {
                let (w, h) = stream.size().ok_or_else(|| anyhow!("unknown stream geometry"))?;
                let (x, y) = stream.position().unwrap_or((0, 0));
                (String::new(), (x, y, w as u32, h as u32))
            }
        };
        let (width, height) = match stream.size() {
            Some((w, h)) if w > 0 && h > 0 => (w as u32, h as u32),
            _ => out.and_then(|o| o.pixel_size()).unwrap_or((geometry.2, geometry.3)),
        };
        Ok(Capture {
            width,
            height,
            output,
            geometry,
            node_id,
            backend: Backend::Portal { fd, session },
            enabled_output: None,
            scale: None,
            transformed: false,
        })
    }
}

/// Output matching a portal stream's position/size (size may be logical or pixel).
fn match_output<'a>(
    outs: &'a [NiriOutput],
    pos: Option<(i32, i32)>,
    size: Option<(i32, i32)>,
) -> Option<&'a NiriOutput> {
    let on: Vec<&NiriOutput> = outs.iter().filter(|o| o.logical.is_some()).collect();
    let size_ok = |o: &NiriOutput, (w, h): (i32, i32)| {
        let s = Some((w as u32, h as u32));
        o.logical.map(|l| (l.2, l.3)) == s || o.pixel_size() == s
    };
    if let Some(p) = pos {
        if let Some(o) = on.iter().find(|o| {
            o.logical.map(|l| (l.0, l.1)) == Some(p) && size.is_none_or(|s| size_ok(o, s))
        }) {
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

    const OUTS: &str = r#"{
      "eDP-1":{"name":"eDP-1","modes":[{"width":1920,"height":1080,"refresh_rate":60003,"is_preferred":true}],
               "current_mode":0,"logical":{"x":0,"y":0,"width":1920,"height":1080,"scale":1.0,"transform":"Normal"}},
      "Virtual-1":{"name":"Virtual-1","modes":[{"width":1024,"height":768,"refresh_rate":60004,"is_preferred":true},
               {"width":2560,"height":1440,"refresh_rate":59951,"is_preferred":false},
               {"width":2560,"height":1440,"refresh_rate":60000,"is_preferred":false},
               {"width":1080,"height":1920,"refresh_rate":60000,"is_preferred":false}],
               "current_mode":null,"logical":null}
    }"#;

    #[test]
    fn parse_and_match() {
        let outs = parse_outputs(OUTS).unwrap();
        assert_eq!(outs.len(), 2);
        let v = outs.iter().find(|o| o.is_virtual()).unwrap();
        assert!(v.logical.is_none() && v.current.is_none());
        assert_eq!(match_output(&outs, None, Some((1920, 1080))).unwrap().name, "eDP-1");
        assert_eq!(match_output(&outs, Some((0, 0)), None).unwrap().name, "eDP-1");
        assert_eq!(nearest_mode(&v.modes, 2712, 1220), Some((2560, 1440, 60000)));
        assert_eq!(nearest_mode(&v.modes, 1220, 2712), Some((1080, 1920, 60000)));
    }

    #[test]
    fn fallback_mode_turns_when_no_portrait_mode_is_listed() {
        let with_portrait = [(1024, 768, 60004), (2560, 1440, 60000), (1080, 1920, 60000)];
        assert_eq!(pick_fallback_mode(&with_portrait, 1220, 2712), Some((1080, 1920, 60000, false)));
        let hdmi = [(1920, 1080, 60000), (1280, 720, 60000), (1920, 1080, 50000)];
        // Portrait wanted, only landscape modes: the nearest to the turned size, rotated.
        assert_eq!(pick_fallback_mode(&hdmi, 1080, 1920), Some((1920, 1080, 60000, true)));
        assert_eq!(pick_fallback_mode(&hdmi, 720, 1280), Some((1280, 720, 60000, true)));
        assert_eq!(pick_fallback_mode(&hdmi, 1280, 720), Some((1280, 720, 60000, false)));
        assert_eq!(pick_fallback_mode(&[], 1280, 720), None);
    }

    /// Runs `<source> ! fakesink` in gst-launch-1.0 for 3 s and returns the buffer count.
    fn count_frames(cap: &Capture) -> usize {
        run_pipeline(cap).0
    }

    /// Buffers seen in 3 s, and the caps pipewiresrc negotiated (the real buffer size).
    fn run_pipeline(cap: &Capture) -> (usize, String) {
        use std::os::unix::process::CommandExt;
        let desc = format!("{} ! fakesink silent=false", cap.source_description());
        let mut cmd = std::process::Command::new("gst-launch-1.0");
        cmd.arg("-v").args(desc.split_whitespace()).stdout(std::process::Stdio::piped());
        if let Some(fd) = cap.fd() {
            // The portal fd is CLOEXEC; let the child inherit it under the same number.
            unsafe {
                cmd.pre_exec(move || {
                    let fl = libc::fcntl(fd, libc::F_GETFD);
                    libc::fcntl(fd, libc::F_SETFD, fl & !libc::FD_CLOEXEC);
                    Ok(())
                });
            }
        }
        let mut child = cmd.spawn().expect("spawn gst-launch-1.0");
        std::thread::sleep(Duration::from_secs(3));
        let _ = child.kill();
        let out = child.wait_with_output().unwrap();
        let text = String::from_utf8_lossy(&out.stdout);
        let caps = text
            .lines()
            .find(|l| l.contains("GstPipeWireSrc") && l.contains("caps = "))
            .unwrap_or_default()
            .to_owned();
        (text.lines().filter(|l| l.contains("chain")).count(), caps)
    }

    fn describe(cap: &Capture) {
        eprintln!(
            "capture {} {}x{} node={} geometry={:?}\n  src: {}",
            cap.output_name(),
            cap.width,
            cap.height,
            cap.node_id,
            cap.geometry(),
            cap.source_description()
        );
    }

    /// Live: cargo test --release capture_smoke -- --ignored --nocapture --test-threads=1
    #[test]
    #[ignore]
    fn capture_smoke_mirror() {
        let cap = start(Mode::Mirror).expect("start mirror capture");
        describe(&cap);
        let n = count_frames(&cap);
        eprintln!("mirror frames in 3 s: {n}");
        assert!(n > 0);
    }

    /// Live: turns the vkms output on at 2712x1220 and 200 %, records it, turns it off again.
    #[test]
    #[ignore]
    fn capture_smoke_extend() {
        let cap = start(Mode::Extend { width: 2712, height: 1220, output: None, scale: Some(2.0) })
            .expect("start extend capture");
        describe(&cap);
        assert_eq!((cap.width, cap.height), (2712, 1220));
        assert_eq!((cap.geometry().2, cap.geometry().3), (1356, 610), "logical size at scale 2");
        let (n, caps) = run_pipeline(&cap);
        eprintln!("negotiated: {caps}");
        assert!(caps.contains("width=(int)2712") && caps.contains("height=(int)1220"), "full-resolution buffers");
        let name = cap.output_name().to_string();
        drop(cap);
        let off = niri_outputs().unwrap().into_iter().find(|o| o.name == name).unwrap();
        eprintln!("extend frames in 3 s: {n}; {name} off after drop: {}", off.logical.is_none());
        assert!(off.logical.is_none());
        assert!(n > 0);
    }

    /// Live: Virtual-1 on at 1920x1080 and 200 %, turned portrait IN PLACE (custom mode, then the
    /// `transform 90` fallback), back to landscape, then off. The output is never switched off in
    /// between, so windows on it stay there.
    #[test]
    #[ignore]
    fn capture_smoke_extend_rotate() {
        let mut cap = start(Mode::Extend { width: 1920, height: 1080, output: None, scale: Some(2.0) })
            .expect("start extend capture");
        let name = cap.output_name().to_string();
        // (PipeWire reuses freed node ids, so a new cast may get the old id.)
        let check = |cap: &Capture, what: &str, (w, h): (u32, u32)| {
            describe(cap);
            let (n, caps) = run_pipeline(cap);
            eprintln!("{what}: {n} frames in 3 s; negotiated: {caps}");
            assert_eq!((cap.width, cap.height), (w, h), "{what}: capture size");
            assert_eq!((cap.geometry().2, cap.geometry().3), (w / 2, h / 2), "{what}: logical size at 200 %");
            assert!(caps.contains(&format!("width=(int){w}")) && caps.contains(&format!("height=(int){h}")), "{what}: buffers");
            assert!(n > 0, "{what}: no frames");
        };
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            cap.reconfigure_extend(1080, 1920).expect("portrait (custom mode)");
            assert!(!cap.transformed);
            check(&cap, "portrait custom mode", (1080, 1920));
            cap.reconfigure(1080, 1920, true).expect("portrait (transform)");
            assert!(cap.transformed);
            check(&cap, "portrait via transform 90", (1080, 1920));
            cap.reconfigure_extend(1920, 1080).expect("back to landscape");
            assert!(!cap.transformed);
            check(&cap, "landscape again", (1920, 1080));
        }));
        drop(cap); // always switch the output off, even after a failed assertion
        let off = niri_outputs().unwrap().into_iter().find(|o| o.name == name).unwrap();
        eprintln!("{name} off after drop: {}", off.logical.is_none());
        assert!(off.logical.is_none());
        if let Err(p) = result {
            std::panic::resume_unwind(p);
        }
    }

    /// Live: forces the portal fallback path (no picker after the first approval).
    #[test]
    #[ignore]
    fn capture_smoke_portal() {
        let cap = pollster::block_on(portal::start_mirror()).expect("portal capture");
        describe(&cap);
        let n = count_frames(&cap);
        eprintln!("portal frames in 3 s: {n}");
        assert!(n > 0);
    }
}
