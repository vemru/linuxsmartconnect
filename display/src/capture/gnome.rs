//! GNOME: everything through Mutter's D-Bus APIs, no picker and no extra packages.
//!
//! - Pixels: `org.gnome.Mutter.ScreenCast`, linked to an `org.gnome.Mutter.RemoteDesktop` session
//!   that carries the phone's input (GNOME has no virtual-pointer protocol). Mutter offers
//!   system-memory buffers, so `pipewiresrc ! video/x-raw ! videoconvert` works without VA-API.
//! - Extend: `RecordVirtual` makes a virtual monitor (`Meta-N`). Its mode is whatever size the
//!   consumer negotiates, so the source description pins `video/x-raw,width=W,height=H` after
//!   `pipewiresrc`; the monitor appears when the session's pipeline starts playing. A later
//!   consumer with other caps (cmd 70, rotation) resizes the same monitor in place, so its windows
//!   stay on it. It disappears when the cast stops (also when this process dies).
//! - Layout: once the monitor exists, a short-lived thread places it next to the monitor the user
//!   put the phone by (`desktop::Placement`, else the primary) on the side they chose, restores
//!   every existing monitor to the position it had before Mutter auto-arranged the layout, and
//!   gives the new monitor the nearest scale Mutter supports for its mode, with
//!   `org.gnome.Mutter.DisplayConfig.ApplyMonitorsConfig` method 1 (temporary: nothing is saved to
//!   monitors.xml and no "Keep these settings?" dialog appears).

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use zbus::zvariant::{OwnedValue, Value};

use super::mutter::{Cast, Source};
use super::{Backend, Mode, SHM_CONVERT, nearest};
use crate::desktop::{Output, Placement};
use crate::input::Route;

const DC: &str = "org.gnome.Mutter.DisplayConfig";
const DC_PATH: &str = "/org/gnome/Mutter/DisplayConfig";
/// How long the layout thread waits for the virtual monitor (the phone has to answer first).
const APPEAR_TIMEOUT: Duration = Duration::from_secs(60);

pub struct GnomeCapture {
    cast: Cast,
    /// Stream size in pixels (Extend: the size we ask the virtual monitor to have).
    size: (u32, u32),
    /// Shared with the layout thread, which fills in the virtual monitor's name and real rect.
    state: Arc<Mutex<Shown>>,
    extend: Option<Virtual>,
}

struct Shown {
    output: String,
    geometry: (i32, i32, u32, u32),
    scale: f64,
}

/// A logical monitor as it was *before* the virtual one appeared. Mutter re-arranges the whole
/// layout when a monitor is added, so the state it reports afterwards is not the user's layout;
/// [`place`] restores this snapshot instead of copying the auto-arranged one back.
#[derive(Clone, Debug)]
struct Base {
    x: i32,
    y: i32,
    scale: f64,
    transform: u32,
    primary: bool,
    connectors: Vec<String>,
}

impl Base {
    fn of(lm: &Logical) -> Self {
        Base {
            x: lm.x,
            y: lm.y,
            scale: lm.scale,
            transform: lm.transform,
            primary: lm.primary,
            connectors: lm.connectors.clone(),
        }
    }
}

struct Virtual {
    /// Wanted scale (the nearest one Mutter supports for the mode is used).
    scale: f64,
    /// Connectors that existed before the virtual monitor: the new one is not among them.
    before: HashSet<String>,
    /// The layout to restore (positions, scales, transforms, grouping) when placing the new monitor.
    base: Vec<Base>,
    /// Stops the current layout thread.
    cancel: Arc<AtomicBool>,
}

impl Backend for GnomeCapture {
    fn source_description(&self) -> String {
        let head = format!(
            "pipewiresrc path={} do-timestamp=true keepalive-time=1000",
            self.cast.node_id
        );
        match &self.extend {
            // The caps set the virtual monitor's mode.
            Some(_) => format!(
                "{head} ! video/x-raw,width={},height={} ! videoconvert",
                self.size.0, self.size.1
            ),
            None => format!("{head} ! {SHM_CONVERT}"),
        }
    }

    fn size(&self) -> (u32, u32) {
        self.size
    }

    fn geometry(&self) -> (i32, i32, u32, u32) {
        self.state.lock().unwrap().geometry
    }

    fn output_name(&self) -> String {
        self.state.lock().unwrap().output.clone()
    }

    fn is_extend(&self) -> bool {
        self.extend.is_some()
    }

    fn reconfigure_extend(&mut self, width: u32, height: u32) -> Result<()> {
        let Some(v) = &mut self.extend else {
            bail!("only an extend capture can be resized")
        };
        // Same cast, new caps: the next consumer renegotiates and Mutter resizes the monitor.
        self.size = (width, height);
        v.cancel.store(true, Ordering::Relaxed);
        v.cancel = Arc::new(AtomicBool::new(false));
        {
            let mut s = self.state.lock().unwrap();
            let scale = s.scale;
            s.geometry = (
                s.geometry.0,
                s.geometry.1,
                logical(width, scale),
                logical(height, scale),
            );
        }
        spawn_layout(
            self.size,
            v.scale,
            v.base.clone(),
            v.before.clone(),
            self.state.clone(),
            v.cancel.clone(),
        );
        Ok(())
    }

    fn reposition(&mut self) -> Result<bool> {
        let (w, h) = self.size;
        self.reconfigure_extend(w, h)?;
        Ok(true)
    }

    fn route(&self) -> Route {
        let state = self.state.clone();
        let scale = Arc::new(move || state.lock().unwrap().scale);
        match self
            .cast
            .remote_input(self.size, self.extend.is_some(), scale)
        {
            Some(r) => Route::Mutter(r),
            None => Route::Wayland,
        }
    }
}

impl Drop for GnomeCapture {
    fn drop(&mut self) {
        if let Some(v) = &self.extend {
            v.cancel.store(true, Ordering::Relaxed);
        }
        // `cast` stops on drop; Mutter then removes the virtual monitor and restores the layout.
    }
}

fn logical(px: u32, scale: f64) -> u32 {
    (f64::from(px) / scale.max(0.1)).round() as u32
}

pub fn start(mode: &Mode) -> Result<GnomeCapture> {
    let st = State::read()?;
    match mode {
        Mode::Mirror => {
            // Duplicate the monitor the phone sits next to (the GTK page's position), else the
            // primary one. This is the only output whose pixels matter in mirror mode.
            let anchor = Placement::current().output;
            let connector = anchor
                .as_deref()
                .filter(|name| st.logical_of(name).is_some())
                .filter(|name| st.monitor(name).is_some_and(|m| !m.is_virtual()))
                .map(str::to_owned)
                .or_else(|| st.primary().map(|m| m.connector.clone()))
                .ok_or_else(|| anyhow!("GNOME reports no active monitor"))?;
            record_existing(&st, &connector)
        }
        // A named connector (e.g. a dummy plug, already on): record it as it is.
        Mode::Extend {
            output: Some(name), ..
        } => record_existing(&st, name),
        Mode::Extend {
            width,
            height,
            output: None,
            scale,
        } => {
            let scale = scale.unwrap_or(1.0);
            let before: HashSet<String> = st.monitors.iter().map(|m| m.connector.clone()).collect();
            // Mutter will re-arrange as soon as the virtual monitor appears. Keep what the user's
            // layout is now so `place()` can put every existing monitor back exactly where it was.
            let base: Vec<Base> = st.logical.iter().map(Base::of).collect();
            let size = (logical(*width, scale), logical(*height, scale));
            let (x, y) = st.spot(None, size);
            let cast = Cast::start(Source::Virtual, true)?;
            let state = Arc::new(Mutex::new(Shown {
                output: "virtual monitor".into(),
                geometry: (x, y, size.0, size.1),
                scale,
            }));
            let cancel = Arc::new(AtomicBool::new(false));
            spawn_layout(
                (*width, *height),
                scale,
                base.clone(),
                before.clone(),
                state.clone(),
                cancel.clone(),
            );
            Ok(GnomeCapture {
                cast,
                size: (*width, *height),
                state,
                extend: Some(Virtual {
                    scale,
                    before,
                    base,
                    cancel,
                }),
            })
        }
    }
}

fn record_existing(st: &State, connector: &str) -> Result<GnomeCapture> {
    let mon = st.monitor(connector).ok_or_else(|| {
        let names: Vec<_> = st.monitors.iter().map(|m| m.connector.as_str()).collect();
        anyhow!(
            "monitor {connector:?} is not connected (GNOME sees: {})",
            names.join(", ")
        )
    })?;
    let lm = st
        .logical_of(connector)
        .ok_or_else(|| anyhow!("monitor {connector} is off"))?;
    let size = mon
        .current_px(lm.transform)
        .ok_or_else(|| anyhow!("monitor {connector} has no current mode"))?;
    let cast = Cast::start(Source::Monitor(connector), true)?;
    let state = Arc::new(Mutex::new(Shown {
        output: connector.to_owned(),
        geometry: st.rect(lm),
        scale: lm.scale,
    }));
    Ok(GnomeCapture {
        cast,
        size,
        state,
        extend: None,
    })
}

/// Waits (in a thread) for the virtual monitor to show up at `size`, then places and scales it.
fn spawn_layout(
    size: (u32, u32),
    scale: f64,
    base: Vec<Base>,
    before: HashSet<String>,
    shown: Arc<Mutex<Shown>>,
    cancel: Arc<AtomicBool>,
) {
    std::thread::spawn(move || {
        let deadline = Instant::now() + APPEAR_TIMEOUT;
        while !cancel.load(Ordering::Relaxed) && Instant::now() < deadline {
            let Ok(st) = State::read() else { break };
            let found = st
                .monitors
                .iter()
                .find(|m| !before.contains(&m.connector) && m.current_px(0) == Some(size));
            if let Some(m) = found {
                let name = m.connector.clone();
                match place(&st, &name, &base, scale) {
                    Ok((rect, used)) => {
                        println!("{name}: GNOME virtual monitor at {rect:?}, scale {used}");
                        *shown.lock().unwrap() = Shown {
                            output: name,
                            geometry: rect,
                            scale: used,
                        };
                    }
                    Err(e) => {
                        eprintln!("capture: could not place the virtual monitor {name}: {e:#}");
                        shown.lock().unwrap().output = name;
                    }
                }
                return;
            }
            std::thread::sleep(Duration::from_millis(200));
        }
    });
}

/// Puts `name` where the placement says next to the monitor the user picked, at the supported
/// scale nearest `scale`, and puts every monitor from `base` back exactly where it was before
/// Mutter auto-arranged the layout. Returns the phone's logical rect and the scale used.
fn place(st: &State, name: &str, base: &[Base], scale: f64) -> Result<((i32, i32, u32, u32), f64)> {
    let (logical_monitors, rect, scale) =
        layout_specs(st, name, base, scale, &Placement::current())?;
    let mut props: HashMap<&str, Value> = HashMap::new();
    if st.can_change_layout_mode {
        props.insert("layout-mode", Value::U32(st.layout_mode));
    }
    let conn = zbus::blocking::Connection::session()?;
    let dc = zbus::blocking::Proxy::new(&conn, DC, DC_PATH, DC)?;
    dc.call::<_, _, ()>(
        "ApplyMonitorsConfig",
        &(
            st.serial,
            1u32, /* temporary */
            logical_monitors,
            props,
        ),
    )
    .context("DisplayConfig.ApplyMonitorsConfig")?;
    Ok((rect, scale))
}

/// The layout `place` applies, and the phone's rect. Kept free of D-Bus so a test can check that
/// a layout Mutter re-arranged is put back the way it was.
#[allow(clippy::type_complexity)]
fn layout_specs(
    st: &State,
    name: &str,
    base: &[Base],
    scale: f64,
    placement: &Placement,
) -> Result<(Vec<LayoutSpec>, (i32, i32, u32, u32), f64)> {
    let mon = st.monitor(name).ok_or_else(|| anyhow!("{name} is gone"))?;
    let mode = mon
        .current()
        .ok_or_else(|| anyhow!("{name} has no current mode"))?;
    let scale = nearest(&mode.scales, scale);
    let (w, h) = if st.layout_mode == 2 {
        (mode.width, mode.height)
    } else {
        (logical(mode.width, scale), logical(mode.height, scale))
    };

    // `base` is the layout from before Mutter added the virtual monitor: that is the geometry the
    // phone must be placed against, not the auto-arranged one Mutter reports now.
    let outs: Vec<Output> = base
        .iter()
        .map(|b| {
            let lm = Logical {
                x: b.x,
                y: b.y,
                scale: b.scale,
                transform: b.transform,
                primary: b.primary,
                connectors: b.connectors.clone(),
            };
            let connector = b.connectors.first().cloned().unwrap_or_default();
            Output {
                description: st
                    .monitor(&connector)
                    .map(|m| m.display_name.clone())
                    .unwrap_or_default(),
                logical: Some(st.rect(&lm)),
                focused: b.primary,
                is_virtual: b
                    .connectors
                    .iter()
                    .any(|c| st.monitor(c).is_some_and(Monitor::is_virtual)),
                name: connector,
            }
        })
        .collect();
    let (x, y) = crate::desktop::spot_with(&outs, Some(name), (w, h), placement);

    // Mutter wants the layout to start at 0,0: a screen on the left or on top moves the rest.
    let (dx, dy) = (
        -base.iter().map(|b| b.x).chain([x]).min().unwrap_or(0),
        -base.iter().map(|b| b.y).chain([y]).min().unwrap_or(0),
    );

    let mut logical_monitors: Vec<LayoutSpec> = Vec::new();
    for b in base {
        let monitors = monitor_specs(st, &b.connectors);
        if monitors.is_empty() {
            continue; // unplugged between the snapshot and now
        }
        logical_monitors.push((
            b.x + dx,
            b.y + dy,
            b.scale,
            b.transform,
            b.primary,
            monitors,
        ));
    }
    // A monitor hot-plugged while we waited is not in the snapshot: keep it where Mutter put it.
    let known: HashSet<&str> = base
        .iter()
        .flat_map(|b| b.connectors.iter().map(String::as_str))
        .collect();
    for lm in &st.logical {
        if lm
            .connectors
            .iter()
            .any(|c| c == name || known.contains(c.as_str()))
        {
            continue;
        }
        let monitors = monitor_specs(st, &lm.connectors);
        if !monitors.is_empty() {
            logical_monitors.push((
                lm.x + dx,
                lm.y + dy,
                lm.scale,
                lm.transform,
                lm.primary,
                monitors,
            ));
        }
    }
    logical_monitors.push((
        x + dx,
        y + dy,
        scale,
        0,
        false,
        vec![(name.to_owned(), mode.id.clone(), HashMap::new())],
    ));
    Ok((logical_monitors, (x + dx, y + dy, w, h), scale))
}

/// The `(connector, mode, properties)` tuples ApplyMonitorsConfig takes, for `connectors`.
fn monitor_specs(
    st: &State,
    connectors: &[String],
) -> Vec<(String, String, HashMap<String, Value<'static>>)> {
    connectors
        .iter()
        .filter_map(|c| {
            let m = st.monitor(c)?;
            Some((c.clone(), m.current()?.id.clone(), m.apply_props()))
        })
        .collect()
}

/// For `desktop::outputs`.
pub fn outputs() -> Result<Vec<Output>> {
    let st = State::read()?;
    Ok(st
        .monitors
        .iter()
        .map(|m| {
            let lm = st.logical_of(&m.connector);
            Output {
                name: m.connector.clone(),
                description: if m.is_virtual() {
                    String::new()
                } else {
                    m.display_name.clone()
                },
                logical: lm.map(|lm| st.rect(lm)),
                focused: lm.is_some_and(|lm| lm.primary),
                is_virtual: m.is_virtual(),
            }
        })
        .collect())
}

// ---------------------------------------------------------------------------------------------
// DisplayConfig.GetCurrentState

#[derive(Debug, Clone)]
struct ModeInfo {
    id: String,
    width: u32,
    height: u32,
    scales: Vec<f64>,
    current: bool,
}

#[derive(Debug, Clone)]
struct Monitor {
    connector: String,
    vendor: String,
    display_name: String,
    modes: Vec<ModeInfo>,
    props: HashMap<String, OwnedValue>,
}

impl Monitor {
    fn is_virtual(&self) -> bool {
        self.connector.starts_with("Meta-") || self.vendor == "MetaVendor"
    }

    fn current(&self) -> Option<&ModeInfo> {
        self.modes.iter().find(|m| m.current)
    }

    /// Current mode in pixels, turned by the logical monitor's transform (1, 3, 5, 7 = 90°/270°).
    fn current_px(&self, transform: u32) -> Option<(u32, u32)> {
        let m = self.current()?;
        Some(if transform % 2 == 1 {
            (m.height, m.width)
        } else {
            (m.width, m.height)
        })
    }

    /// The per-monitor properties ApplyMonitorsConfig takes, copied from the current state so
    /// that re-applying a monitor changes nothing about it.
    fn apply_props(&self) -> HashMap<String, Value<'static>> {
        let mut p = HashMap::new();
        for key in ["color-mode", "rgb-range"] {
            if let Some(v) = self.props.get(key).and_then(|v| u32::try_from(v).ok()) {
                p.insert(key.to_owned(), Value::U32(v));
            }
        }
        if self
            .props
            .get("is-underscanning")
            .and_then(|v| bool::try_from(v).ok())
            == Some(true)
        {
            p.insert("underscanning".to_owned(), Value::Bool(true));
        }
        p
    }
}

#[derive(Debug, Clone)]
struct Logical {
    x: i32,
    y: i32,
    scale: f64,
    transform: u32,
    primary: bool,
    connectors: Vec<String>,
}

#[derive(Debug)]
struct State {
    serial: u32,
    monitors: Vec<Monitor>,
    logical: Vec<Logical>,
    /// 1 = logical (positions in logical px), 2 = physical.
    layout_mode: u32,
    can_change_layout_mode: bool,
}

type MonitorSpec = (String, String, String, String);
type ModeTuple = (
    String,
    i32,
    i32,
    f64,
    f64,
    Vec<f64>,
    HashMap<String, OwnedValue>,
);
type MonitorTuple = (MonitorSpec, Vec<ModeTuple>, HashMap<String, OwnedValue>);
type LogicalTuple = (
    i32,
    i32,
    f64,
    u32,
    bool,
    Vec<MonitorSpec>,
    HashMap<String, OwnedValue>,
);
/// One entry of ApplyMonitorsConfig's logical monitor list.
type LayoutSpec = (
    i32,
    i32,
    f64,
    u32,
    bool,
    Vec<(String, String, HashMap<String, Value<'static>>)>,
);

impl State {
    fn read() -> Result<State> {
        let conn = zbus::blocking::Connection::session().context("D-Bus session bus")?;
        let dc = zbus::blocking::Proxy::new(&conn, DC, DC_PATH, DC)?;
        let (serial, monitors, logical, props): (
            u32,
            Vec<MonitorTuple>,
            Vec<LogicalTuple>,
            HashMap<String, OwnedValue>,
        ) = dc
            .call("GetCurrentState", &())
            .context("Mutter DisplayConfig.GetCurrentState (is this GNOME?)")?;
        Ok(State::from_tuples(serial, monitors, logical, &props))
    }

    fn from_tuples(
        serial: u32,
        monitors: Vec<MonitorTuple>,
        logical: Vec<LogicalTuple>,
        props: &HashMap<String, OwnedValue>,
    ) -> State {
        let flag = |v: &HashMap<String, OwnedValue>, k: &str| {
            v.get(k)
                .and_then(|x| bool::try_from(x).ok())
                .unwrap_or(false)
        };
        let monitors = monitors
            .into_iter()
            .map(|((connector, vendor, ..), modes, props)| Monitor {
                display_name: props
                    .get("display-name")
                    .and_then(|v| String::try_from(v.clone()).ok())
                    .unwrap_or_default(),
                modes: modes
                    .into_iter()
                    .map(|(id, w, h, _, _, scales, mp)| ModeInfo {
                        current: flag(&mp, "is-current"),
                        id,
                        width: w.max(0) as u32,
                        height: h.max(0) as u32,
                        scales,
                    })
                    .collect(),
                connector,
                vendor,
                props,
            })
            .collect();
        let logical = logical
            .into_iter()
            .map(|(x, y, scale, transform, primary, mons, _)| Logical {
                x,
                y,
                scale,
                transform,
                primary,
                connectors: mons.into_iter().map(|m| m.0).collect(),
            })
            .collect();
        State {
            serial,
            monitors,
            logical,
            layout_mode: props
                .get("layout-mode")
                .and_then(|v| u32::try_from(v).ok())
                .unwrap_or(1),
            can_change_layout_mode: flag(props, "supports-changing-layout-mode"),
        }
    }

    fn monitor(&self, connector: &str) -> Option<&Monitor> {
        self.monitors.iter().find(|m| m.connector == connector)
    }

    fn logical_of(&self, connector: &str) -> Option<&Logical> {
        self.logical
            .iter()
            .find(|lm| lm.connectors.iter().any(|c| c == connector))
    }

    /// The primary logical monitor's first real monitor.
    fn primary(&self) -> Option<&Monitor> {
        let lm = self
            .logical
            .iter()
            .find(|lm| lm.primary)
            .or(self.logical.first())?;
        lm.connectors
            .iter()
            .filter_map(|c| self.monitor(c))
            .find(|m| !m.is_virtual())
    }

    /// Rect of a logical monitor in layout coordinates.
    fn rect(&self, lm: &Logical) -> (i32, i32, u32, u32) {
        let px = lm
            .connectors
            .first()
            .and_then(|c| self.monitor(c))
            .and_then(|m| m.current_px(lm.transform))
            .unwrap_or((0, 0));
        if self.layout_mode == 2 {
            (lm.x, lm.y, px.0, px.1)
        } else {
            (lm.x, lm.y, logical(px.0, lm.scale), logical(px.1, lm.scale))
        }
    }

    /// Where a new monitor of logical `size` goes (before shifting the layout to 0,0): next to
    /// the monitor the placement names (else the primary), past any other monitor on that side.
    /// `exclude` is the virtual monitor itself.
    fn spot(&self, exclude: Option<&str>, size: (u32, u32)) -> (i32, i32) {
        let outs: Vec<Output> = self
            .logical
            .iter()
            .filter(|lm| !lm.connectors.iter().any(|c| Some(c.as_str()) == exclude))
            .map(|lm| Output {
                name: lm.connectors.first().cloned().unwrap_or_default(),
                description: String::new(),
                logical: Some(self.rect(lm)),
                focused: lm.primary,
                is_virtual: lm
                    .connectors
                    .iter()
                    .any(|c| self.monitor(c).is_some_and(Monitor::is_virtual)),
            })
            .collect();
        crate::desktop::spot(&outs, None, size)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capture::tests::run_pipeline;

    fn mode(id: &str, w: i32, h: i32, scales: &[f64], current: bool) -> ModeTuple {
        let mut p = HashMap::new();
        if current {
            p.insert("is-current".to_owned(), OwnedValue::from(true));
        }
        (id.into(), w, h, 60.0, 1.0, scales.to_vec(), p)
    }

    fn spec(c: &str) -> MonitorSpec {
        (c.into(), "V".into(), "P".into(), "S".into())
    }

    /// The laptop at 1.25 plus a virtual monitor Mutter put at x=1536 (unscaled).
    fn sample() -> State {
        let monitors = vec![
            (
                spec("eDP-1"),
                vec![mode("1920x1080@60", 1920, 1080, &[1.0, 1.25, 2.0], true)],
                HashMap::new(),
            ),
            (
                (
                    "Meta-0".into(),
                    "MetaVendor".into(),
                    "Virtual remote monitor".into(),
                    "0x1".into(),
                ),
                vec![mode("2712x1220@60", 2712, 1220, &[1.0, 1.333, 2.0], true)],
                HashMap::new(),
            ),
        ];
        let logical = vec![
            (0, 0, 1.25, 0, true, vec![spec("eDP-1")], HashMap::new()),
            (1536, 0, 1.0, 0, false, vec![spec("Meta-0")], HashMap::new()),
        ];
        let props = HashMap::from([("layout-mode".to_owned(), OwnedValue::from(1u32))]);
        State::from_tuples(7, monitors, logical, &props)
    }

    #[test]
    fn reads_the_layout() {
        let st = sample();
        assert_eq!(st.primary().unwrap().connector, "eDP-1");
        assert!(st.monitor("Meta-0").unwrap().is_virtual());
        // 1920 px at 125 % = 1536 logical px wide.
        assert_eq!(st.rect(st.logical_of("eDP-1").unwrap()), (0, 0, 1536, 864));
        // (Which side depends on the user's crosscontrol.json; right is the default.)
        let (x, _) = st.spot(Some("Meta-0"), (960, 540));
        assert!(x == 1536 || x == -960, "{x}");
        assert_eq!(
            nearest(
                &st.monitor("Meta-0").unwrap().current().unwrap().scales,
                1.5
            ),
            1.333
        );
    }

    /// Mutter auto-arranges when the virtual monitor appears (here the laptop panel jumps from
    /// below the external monitor to its right). Placing the phone must put the real layout back
    /// and attach the phone to the monitor the placement names.
    #[test]
    fn placing_the_phone_restores_the_real_layout() {
        let monitors = vec![
            (
                spec("HDMI-1"),
                vec![mode("2560x1440@60", 2560, 1440, &[1.0], true)],
                HashMap::new(),
            ),
            (
                spec("eDP-1"),
                vec![mode("1920x1080@60", 1920, 1080, &[1.0, 1.25], true)],
                HashMap::new(),
            ),
            (
                (
                    "Meta-0".into(),
                    "MetaVendor".into(),
                    "Virtual remote monitor".into(),
                    "0x1".into(),
                ),
                vec![mode("862x1920@60", 862, 1920, &[1.0, 2.0], true)],
                HashMap::new(),
            ),
        ];
        // After Mutter's auto-arrange: eDP-1 moved to the right of HDMI-1, the phone past it.
        let logical = vec![
            (0, 0, 1.0, 0, true, vec![spec("HDMI-1")], HashMap::new()),
            (2560, 0, 1.0, 0, false, vec![spec("eDP-1")], HashMap::new()),
            (
                4480,
                187,
                2.0,
                0,
                false,
                vec![spec("Meta-0")],
                HashMap::new(),
            ),
        ];
        let props = HashMap::from([("layout-mode".to_owned(), OwnedValue::from(1u32))]);
        let st = State::from_tuples(9, monitors, logical, &props);
        let base = vec![
            Base {
                x: 0,
                y: 0,
                scale: 1.0,
                transform: 0,
                primary: true,
                connectors: vec!["HDMI-1".into()],
            },
            Base {
                x: 250,
                y: 1440,
                scale: 1.0,
                transform: 0,
                primary: false,
                connectors: vec!["eDP-1".into()],
            },
        ];
        let placement = Placement {
            edge: crate::desktop::Edge::Right,
            start: 0.4,
            end: 0.6,
            output: Some("HDMI-1".into()),
        };
        let (specs, rect, scale) = layout_specs(&st, "Meta-0", &base, 2.0, &placement).unwrap();
        assert_eq!(scale, 2.0);
        // 862x1920 at 200% = 431x960, centred on the 1440 px tall HDMI-1's right edge.
        assert_eq!(rect, (2560, 240, 431, 960));
        let pos = |c: &str| {
            specs
                .iter()
                .find(|s| s.5.iter().any(|m| m.0 == c))
                .map(|s| (s.0, s.1))
                .unwrap()
        };
        assert_eq!(pos("HDMI-1"), (0, 0));
        assert_eq!(
            pos("eDP-1"),
            (250, 1440),
            "the laptop panel is back below the external monitor"
        );
        assert_eq!(
            pos("Meta-0"),
            (2560, 240),
            "the phone is beside the monitor it was attached to"
        );
    }

    /// Live (GNOME): cargo test --release -p smartconnect-display gnome_smoke -- --ignored --nocapture --test-threads=1
    /// A virtual monitor at 2712x1220 / 200 %, placed right of the others, resized in place to
    /// portrait, then gone after drop.
    #[test]
    #[ignore]
    fn gnome_smoke_extend() {
        let mut cap = start(&Mode::Extend {
            width: 2712,
            height: 1220,
            output: None,
            scale: Some(2.0),
        })
        .expect("start");
        let (n, caps) = run_pipeline(&cap.source_description(), None);
        eprintln!("landscape: {n} frames, {caps}");
        assert!(n > 0 && caps.contains("width=(int)2712"));
        std::thread::sleep(Duration::from_millis(500));
        let st = State::read().unwrap();
        let name = cap.output_name();
        let lm = st.logical_of(&name).expect("virtual monitor in the layout");
        eprintln!("{name}: {:?} scale {}", st.rect(lm), lm.scale);
        assert_eq!((lm.scale, st.rect(lm).2), (2.0, 1356));
        cap.reconfigure_extend(1220, 2712).unwrap();
        let (n, caps) = run_pipeline(&cap.source_description(), None);
        eprintln!("portrait: {n} frames, {caps}");
        assert!(caps.contains("width=(int)1220") && caps.contains("height=(int)2712"));
        let st = State::read().unwrap();
        assert_eq!(
            st.monitor(&name).and_then(|m| m.current_px(0)),
            Some((1220, 2712)),
            "same monitor, resized"
        );
        drop(cap);
        std::thread::sleep(Duration::from_millis(500));
        assert!(
            State::read().unwrap().monitor(&name).is_none(),
            "virtual monitor gone after drop"
        );
    }

    /// Live (GNOME): the virtual monitor follows the placement file (a temporary
    /// `XDG_CONFIG_HOME`): left of the laptop, then moved below it while it runs.
    #[test]
    #[ignore]
    fn gnome_smoke_placement() {
        let dir =
            std::env::temp_dir().join(format!("smartconnect-placement-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("smartconnect-linux")).unwrap();
        // SAFETY: live tests run with --test-threads=1.
        unsafe { std::env::set_var("XDG_CONFIG_HOME", &dir) };
        let file = dir.join("smartconnect-linux/crosscontrol.json");
        let st_before = State::read().expect("current state");
        let target = if st_before.monitor("eDP-1").is_some() {
            "eDP-1".to_string()
        } else {
            st_before
                .primary()
                .expect("a primary monitor")
                .connector
                .clone()
        };
        std::fs::write(
            &file,
            format!(r#"{{"edge":"left","start":0.5,"end":1.0,"output":"{target}"}}"#),
        )
        .unwrap();
        let mut cap = start(&Mode::Extend {
            width: 1920,
            height: 1080,
            output: None,
            scale: Some(2.0),
        })
        .expect("start");
        run_pipeline(&cap.source_description(), None);
        let st = State::read().unwrap();
        let name = cap.output_name();
        let rect = |st: &State, c: &str| st.rect(st.logical_of(c).unwrap());
        let (ours, laptop) = (rect(&st, &name), rect(&st, &target));
        eprintln!("left: {name} {ours:?}, {target} {laptop:?}");
        assert_eq!(
            ours.0 + ours.2 as i32,
            laptop.0,
            "touching the target's left edge"
        );
        std::fs::write(
            &file,
            format!(r#"{{"edge":"bottom","start":0.0,"end":0.2,"output":"{target}"}}"#),
        )
        .unwrap();
        assert!(cap.reposition().unwrap());
        std::thread::sleep(Duration::from_millis(800));
        let st = State::read().unwrap();
        let (ours, laptop) = (rect(&st, &name), rect(&st, &target));
        eprintln!("bottom: {name} {ours:?}, {target} {laptop:?}");
        assert_eq!(
            (ours.1, ours.0),
            (laptop.1 + laptop.3 as i32, laptop.0),
            "below, at the left end"
        );
        drop(cap);
        let _ = std::fs::remove_dir_all(&dir);
        std::thread::sleep(Duration::from_millis(500));
        let st = State::read().unwrap();
        eprintln!("after: {target} {:?}", rect(&st, &target));
    }

    /// Live (GNOME): adding the virtual monitor must not move the real ones. Captures the layout,
    /// starts Extend (the phone anchored to the primary monitor), and checks that every existing
    /// monitor is exactly where it was, with the phone beside its anchor and touching nothing else.
    #[test]
    #[ignore]
    fn gnome_smoke_restores_the_layout() {
        let before = State::read().expect("current state");
        let base: Vec<(String, (i32, i32, u32, u32))> = before
            .logical
            .iter()
            .flat_map(|lm| lm.connectors.iter().map(|c| (c.clone(), before.rect(lm))))
            .collect();
        let primary = before
            .primary()
            .expect("a primary monitor")
            .connector
            .clone();

        let dir = std::env::temp_dir().join(format!("smartconnect-layout-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("smartconnect-linux")).unwrap();
        // SAFETY: live tests run with --test-threads=1.
        unsafe { std::env::set_var("XDG_CONFIG_HOME", &dir) };
        std::fs::write(
            dir.join("smartconnect-linux/crosscontrol.json"),
            format!(r#"{{"edge":"right","start":0.4,"end":0.6,"output":"{primary}"}}"#),
        )
        .unwrap();

        let cap = start(&Mode::Extend {
            width: 862,
            height: 1920,
            output: None,
            scale: Some(2.0),
        })
        .expect("start");
        let (n, caps) = run_pipeline(&cap.source_description(), None);
        eprintln!("{n} frames, {caps}");
        assert!(
            n > 0,
            "the virtual monitor only appears once a consumer negotiates its size"
        );
        std::thread::sleep(Duration::from_millis(800));

        let during = State::read().unwrap();
        let name = cap.output_name();
        for (c, want) in &base {
            assert_eq!(
                during.logical_of(c).map(|lm| during.rect(lm)),
                Some(*want),
                "{c} must stay where the user put it"
            );
        }
        let ours = during.rect(
            during
                .logical_of(&name)
                .expect("the virtual monitor is in the layout"),
        );
        let (px, py, pw, ph) = base.iter().find(|(c, _)| *c == primary).unwrap().1;
        eprintln!(
            "{name} {ours:?} beside {primary} {:?}, others {base:?}",
            (px, py, pw, ph)
        );
        assert_eq!(ours.0, px + pw as i32, "touching {primary}'s right edge");
        assert!(
            ours.1 >= py && ours.1 + ours.3 as i32 <= py + ph as i32,
            "{ours:?} is within {primary}'s height"
        );
        for (c, r) in &base {
            let overlap = ours.0 < r.0 + r.2 as i32
                && r.0 < ours.0 + ours.2 as i32
                && ours.1 < r.1 + r.3 as i32
                && r.1 < ours.1 + ours.3 as i32;
            assert!(!overlap, "{name} {ours:?} overlaps {c} {r:?}");
        }

        drop(cap);
        std::thread::sleep(Duration::from_millis(600));
        let after = State::read().unwrap();
        assert!(
            after.monitor(&name).is_none(),
            "the virtual monitor is gone"
        );
        for (c, want) in &base {
            assert_eq!(
                after.logical_of(c).map(|lm| after.rect(lm)),
                Some(*want),
                "{c} after the session"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Live (GNOME): duplicate the primary monitor.
    #[test]
    #[ignore]
    fn gnome_smoke_mirror() {
        let cap = start(&Mode::Mirror).expect("start");
        let (n, caps) = run_pipeline(&cap.source_description(), None);
        eprintln!(
            "{} {:?}: {n} frames, {caps}",
            cap.output_name(),
            cap.geometry()
        );
        assert!(n > 0);
    }
}
