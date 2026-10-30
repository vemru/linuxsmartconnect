//! Which desktop this session runs, and the per-desktop answers the display helper needs that are
//! not about the video itself: the list of outputs and how to focus one.
//!
//! | Desktop  | Detected by                                  | Outputs listed with                |
//! |----------|----------------------------------------------|------------------------------------|
//! | niri     | `$NIRI_SOCKET`, `XDG_CURRENT_DESKTOP=niri`    | `niri msg --json outputs`          |
//! | Hyprland | `$HYPRLAND_INSTANCE_SIGNATURE`, `…=Hyprland` | `hyprctl -j monitors all`          |
//! | sway     | `$SWAYSOCK`, `…=sway`                        | `swaymsg -t get_outputs -r`        |
//! | GNOME    | `…=GNOME` (also `ubuntu:GNOME`, …)           | Mutter `DisplayConfig` over D-Bus  |
//! | KDE      | `…=KDE`                                      | Wayland `wl_output` + `xdg_output` |
//! | other    | anything else on Wayland                     | Wayland `wl_output` + `xdg_output` |
//!
//! `$SMARTCONNECT_DESKTOP` (one of the names in the first column, lower case) overrides detection.

use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Desktop {
    Niri,
    Hyprland,
    Sway,
    Gnome,
    Kde,
    /// Another Wayland compositor: wlr-screencopy and the virtual pointer, no way to add a screen.
    Other,
}

impl Desktop {
    pub fn detect() -> Desktop {
        let var = |k: &str| std::env::var(k).ok().filter(|v| !v.trim().is_empty());
        if let Some(d) = var("SMARTCONNECT_DESKTOP").and_then(|v| Desktop::parse(&v)) {
            return d;
        }
        // The compositor's own socket variable beats XDG_CURRENT_DESKTOP, which a nested or
        // hand-started session may have inherited from the outer one.
        if var("NIRI_SOCKET").is_some() {
            return Desktop::Niri;
        }
        if var("HYPRLAND_INSTANCE_SIGNATURE").is_some() {
            return Desktop::Hyprland;
        }
        if var("SWAYSOCK").is_some() {
            return Desktop::Sway;
        }
        var("XDG_CURRENT_DESKTOP")
            .and_then(|v| v.split(':').find_map(Desktop::parse))
            .unwrap_or(Desktop::Other)
    }

    /// A name from `$SMARTCONNECT_DESKTOP` or one entry of `$XDG_CURRENT_DESKTOP`.
    pub fn parse(s: &str) -> Option<Desktop> {
        match s.trim().to_ascii_lowercase().as_str() {
            "niri" => Some(Desktop::Niri),
            "hyprland" => Some(Desktop::Hyprland),
            "sway" => Some(Desktop::Sway),
            "gnome" | "gnome-classic" => Some(Desktop::Gnome),
            "kde" | "plasma" => Some(Desktop::Kde),
            "other" | "wlroots" => Some(Desktop::Other),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Desktop::Niri => "niri",
            Desktop::Hyprland => "Hyprland",
            Desktop::Sway => "sway",
            Desktop::Gnome => "GNOME",
            Desktop::Kde => "KDE Plasma",
            Desktop::Other => "this desktop",
        }
    }

    /// How Extend gets its screen: the desktop makes a new virtual one, or (niri) an existing spare
    /// output (vkms `Virtual-*` or a dummy plug) is switched on.
    pub fn extend_kind(self) -> &'static str {
        match self {
            Desktop::Niri => "spare-output",
            Desktop::Hyprland | Desktop::Sway | Desktop::Gnome | Desktop::Kde => "virtual",
            Desktop::Other => "none",
        }
    }
}

/// One screen as the desktop reports it.
#[derive(Clone, Debug, PartialEq)]
pub struct Output {
    /// Connector name, e.g. "eDP-1", "HDMI-A-1", "Virtual-1", "HEADLESS-1", "Meta-0".
    pub name: String,
    /// Make and model, or "" when unknown.
    pub description: String,
    /// Logical rect (x, y, w, h) in compositor coordinates; None when the output is off.
    pub logical: Option<(i32, i32, u32, u32)>,
    pub focused: bool,
    /// A virtual or headless output (vkms, Hyprland/sway headless, GNOME/KDE virtual monitor).
    pub is_virtual: bool,
}

impl Output {
    /// A laptop's own panel.
    pub fn is_internal(&self) -> bool {
        ["eDP", "LVDS", "DSI"]
            .iter()
            .any(|p| self.name.starts_with(p))
    }
}

pub fn outputs(desk: Desktop) -> Result<Vec<Output>> {
    let mut list = match desk {
        Desktop::Niri => crate::capture::niri::outputs()?,
        Desktop::Hyprland => crate::capture::hyprland::outputs()?
            .iter()
            .map(crate::capture::hyprland::Monitor::output)
            .collect(),
        Desktop::Sway => crate::capture::sway::outputs()?,
        Desktop::Gnome => crate::capture::gnome::outputs()?,
        Desktop::Kde | Desktop::Other => wl_outputs()?,
    };
    list.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(list)
}

/// The output to share in Duplicate mode: the focused one if it is real and on, else the first
/// real enabled one, else any enabled one. `exclude` skips the output being set up for Extend.
pub fn primary<'a>(outs: &'a [Output], exclude: Option<&str>) -> Option<&'a Output> {
    let usable = |o: &&Output| o.logical.is_some() && Some(o.name.as_str()) != exclude;
    outs.iter()
        .filter(usable)
        .find(|o| o.focused && !o.is_virtual)
        .or_else(|| {
            outs.iter()
                .filter(usable)
                .find(|o| !o.is_virtual && o.logical.is_some_and(|l| (l.0, l.1) == (0, 0)))
        })
        .or_else(|| outs.iter().filter(usable).find(|o| !o.is_virtual))
        .or_else(|| outs.iter().find(usable))
}

// ---------------------------------------------------------------------------------------------
// Where the phone's screen goes

/// A side of the primary screen.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Edge {
    Left,
    Right,
    Top,
    Bottom,
}

impl Edge {
    pub fn parse(s: &str) -> Option<Edge> {
        match s.trim().to_ascii_lowercase().as_str() {
            "left" => Some(Edge::Left),
            "right" => Some(Edge::Right),
            "top" => Some(Edge::Top),
            "bottom" => Some(Edge::Bottom),
            _ => None,
        }
    }
}

/// Where the phone sits next to the PC's screens. The GTK Cross Control page saves it (the user
/// drags the phone around the real monitor arrangement) to
/// `$XDG_CONFIG_HOME/smartconnect-linux/crosscontrol.json` as
/// `{"edge":"left|right|top|bottom","start":0.15,"end":0.85,"output":"HDMI-1"}`; `start`/`end` are
/// fractions along that monitor's edge, from its top (left/right) or its left (top/bottom), and
/// `output` is the monitor the phone touches (missing: the primary). Extend centres the phone's
/// screen on `(start + end) / 2` of that edge. Missing or invalid: right of the primary, centred.
#[derive(Clone, Debug, PartialEq)]
pub struct Placement {
    pub edge: Edge,
    pub start: f64,
    pub end: f64,
    /// Connector the phone sits next to. `None` = the primary/focused output.
    pub output: Option<String>,
}

impl Default for Placement {
    fn default() -> Self {
        Placement {
            edge: Edge::Right,
            start: 0.0,
            end: 1.0,
            output: None,
        }
    }
}

/// `--side`: overrides the file's edge (set once at startup).
static SIDE: std::sync::OnceLock<Edge> = std::sync::OnceLock::new();

pub fn set_side(edge: Edge) {
    let _ = SIDE.set(edge);
}

pub fn placement_path() -> std::path::PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(std::path::PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| {
            std::path::PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".config")
        })
        .join("smartconnect-linux")
        .join("crosscontrol.json")
}

/// The placement file's modification time (a running Extend session follows changes).
pub fn placement_stamp() -> Option<std::time::SystemTime> {
    std::fs::metadata(placement_path())
        .and_then(|m| m.modified())
        .ok()
}

impl Placement {
    /// The saved placement, with `--side` on top.
    pub fn current() -> Placement {
        let saved = std::fs::read_to_string(placement_path())
            .ok()
            .and_then(|t| serde_json::from_str::<Value>(&t).ok())
            .and_then(|v| Placement::parse(&v));
        let mut p = saved.unwrap_or_default();
        if let Some(edge) = SIDE.get()
            && p.edge != *edge
        {
            // The span belonged to the old edge; keep the monitor the phone sits next to.
            p = Placement {
                edge: *edge,
                start: 0.0,
                end: 1.0,
                output: p.output.clone(),
            };
        }
        p
    }

    pub fn parse(v: &Value) -> Option<Placement> {
        let edge = Edge::parse(v.get("edge")?.as_str()?)?;
        let f = |k: &str| {
            v.get(k)
                .and_then(Value::as_f64)
                .filter(|x| (0.0..=1.0).contains(x))
        };
        let output = v
            .get("output")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned);
        match (f("start"), f("end")) {
            (Some(start), Some(end)) if start <= end => Some(Placement {
                edge,
                start,
                end,
                output,
            }),
            (None, None) => Some(Placement {
                edge,
                output,
                ..Placement::default()
            }),
            _ => None,
        }
    }

    /// Top-left corner for a `size` (logical px) screen on this side of `primary`, centred on the
    /// chosen spot but kept touching the primary along the whole overlap it can have. When another
    /// screen is already there, it goes past that one instead.
    pub fn position(
        &self,
        primary: (i32, i32, u32, u32),
        size: (u32, u32),
        others: &[(i32, i32, u32, u32)],
    ) -> (i32, i32) {
        let (px, py, pw, ph) = (
            f64::from(primary.0),
            f64::from(primary.1),
            f64::from(primary.2),
            f64::from(primary.3),
        );
        let (w, h) = (f64::from(size.0), f64::from(size.1));
        let centre = ((self.start + self.end) / 2.0).clamp(0.0, 1.0);
        let along = |origin: f64, len: f64, extent: f64| {
            let (lo, hi) = if extent <= len {
                (origin, origin + len - extent)
            } else {
                (origin + len - extent, origin)
            };
            (origin + centre * len - extent / 2.0).clamp(lo, hi).round()
        };
        let (mut x, mut y) = match self.edge {
            Edge::Right => (px + pw, along(py, ph, h)),
            Edge::Left => (px - w, along(py, ph, h)),
            Edge::Bottom => (along(px, pw, w), py + ph),
            Edge::Top => (along(px, pw, w), py - h),
        };
        let r = |o: &(i32, i32, u32, u32)| {
            (
                f64::from(o.0),
                f64::from(o.1),
                f64::from(o.0) + f64::from(o.2),
                f64::from(o.1) + f64::from(o.3),
            )
        };
        let overlaps = |x: f64, y: f64| {
            others
                .iter()
                .map(r)
                .any(|(x0, y0, x1, y1)| x < x1 && x + w > x0 && y < y1 && y + h > y0)
        };
        if overlaps(x, y) {
            let all: Vec<_> = others
                .iter()
                .chain(std::iter::once(&primary))
                .map(r)
                .collect();
            match self.edge {
                Edge::Right => x = all.iter().map(|o| o.2).fold(f64::MIN, f64::max),
                Edge::Left => x = all.iter().map(|o| o.0).fold(f64::MAX, f64::min) - w,
                Edge::Bottom => y = all.iter().map(|o| o.3).fold(f64::MIN, f64::max),
                Edge::Top => y = all.iter().map(|o| o.1).fold(f64::MAX, f64::min) - h,
            }
        }
        (x as i32, y as i32)
    }
}

/// Where the phone's screen `name` (logical `size`) goes among `outs`: next to the monitor the
/// saved placement names, else the primary one, as the placement says.
pub fn spot(outs: &[Output], name: Option<&str>, size: (u32, u32)) -> (i32, i32) {
    spot_with(outs, name, size, &Placement::current())
}

/// [`spot`] with the placement given explicitly (tests, and callers that just read it).
pub fn spot_with(
    outs: &[Output],
    name: Option<&str>,
    size: (u32, u32),
    placement: &Placement,
) -> (i32, i32) {
    let usable = |o: &&Output| o.logical.is_some() && Some(o.name.as_str()) != name;
    // The monitor the phone is attached to wins over the primary, so a drag on the GTK page
    // places the phone next to that monitor and not next to whichever one GNOME calls primary.
    let anchor = placement
        .output
        .as_deref()
        .and_then(|want| outs.iter().find(|o| o.name == want))
        .filter(usable);
    let Some((primary_name, primary)) = anchor
        .map(|o| (o.name.clone(), o.logical.unwrap()))
        .or_else(|| primary(outs, name).and_then(|o| o.logical.map(|l| (o.name.clone(), l))))
    else {
        return (0, 0);
    };
    let others: Vec<_> = outs
        .iter()
        .filter(|o| Some(o.name.as_str()) != name && o.name != primary_name)
        .filter_map(|o| o.logical)
        .collect();
    placement.position(primary, size, &others)
}

/// Logical size of `px` pixels at `scale`.
pub fn logical_size((w, h): (u32, u32), scale: f64) -> (u32, u32) {
    let s = if scale > 0.0 { scale } else { 1.0 };
    (
        (f64::from(w) / s).round() as u32,
        (f64::from(h) / s).round() as u32,
    )
}

/// Moves keyboard focus to output `name`, so a layer-shell keyboard (wvkbd) opens there. Best
/// effort: GNOME and KDE have no command for it (their own keyboards follow the focused window).
pub fn focus_output(desk: Desktop, name: &str) {
    let argv: &[&str] = match desk {
        Desktop::Niri => &["niri", "msg", "action", "focus-monitor", name],
        Desktop::Hyprland => &["hyprctl", "dispatch", "focusmonitor", name],
        Desktop::Sway => &["swaymsg", "focus", "output", name],
        Desktop::Gnome | Desktop::Kde | Desktop::Other => return,
    };
    let _ = std::process::Command::new(argv[0])
        .args(&argv[1..])
        .stdout(std::process::Stdio::null())
        .status();
}

/// `smartconnect-display --list-outputs`: what the GUI shows under Advanced, as one JSON object.
pub fn list_json() -> Value {
    let desk = Desktop::detect();
    let (outputs, error) = match outputs(desk) {
        Ok(list) => (list, None),
        Err(e) => (Vec::new(), Some(format!("{e:#}"))),
    };
    json!({
        "desktop": desk.name(),
        "extend": desk.extend_kind(),
        "error": error,
        "outputs": outputs.iter().map(|o| json!({
            "name": o.name,
            "description": o.description,
            "enabled": o.logical.is_some(),
            "virtual": o.is_virtual,
            "internal": o.is_internal(),
            "focused": o.focused,
            // Logical rect in the desktop's coordinate space (the GTK page draws the real
            // arrangement and saves the monitor the phone sits next to). Null when it is off.
            "x": o.logical.map(|l| l.0),
            "y": o.logical.map(|l| l.1),
            "width": o.logical.map(|l| l.2),
            "height": o.logical.map(|l| l.3),
        })).collect::<Vec<_>>(),
    })
}

/// Runs a desktop's command line tool and returns its stdout.
pub fn run_tool(argv: &[&str]) -> Result<String> {
    let out = std::process::Command::new(argv[0])
        .args(&argv[1..])
        .stdin(std::process::Stdio::null())
        .output()
        .with_context(|| format!("running `{}` (is {} running?)", argv[0], argv[0]))?;
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        let why = [err.trim(), stdout.trim()]
            .into_iter()
            .find(|s| !s.is_empty())
            .unwrap_or("no output");
        bail!("`{}` failed: {why}", argv.join(" "));
    }
    Ok(stdout)
}

// ---------------------------------------------------------------------------------------------
// Plain Wayland output list (KDE, other compositors): wl_output v4 names + xdg-output rects.

pub fn wl_outputs() -> Result<Vec<Output>> {
    use wayland_client::{Connection, globals::registry_queue_init};
    let conn = Connection::connect_to_env().context("connecting to Wayland")?;
    let (globals, mut queue) =
        registry_queue_init::<wl::State>(&conn).context("Wayland registry")?;
    let qh = queue.handle();
    let mut state = wl::State::default();
    let xo: Option<wl::XdgOutputManager> = globals.bind(&qh, 1..=3, ()).ok();
    for g in globals
        .contents()
        .clone_list()
        .into_iter()
        .filter(|g| g.interface == "wl_output")
    {
        let idx = state.0.len();
        let out: wl::WlOutput = globals.registry().bind(g.name, g.version.min(4), &qh, idx);
        if let Some(m) = &xo {
            m.get_xdg_output(&out, &qh, idx);
        }
        state.0.push(wl::Info::default());
    }
    queue.roundtrip(&mut state)?;
    queue.roundtrip(&mut state)?;
    let list: Vec<Output> = state
        .0
        .into_iter()
        .enumerate()
        .map(|(i, o)| {
            let name = o.name.unwrap_or_else(|| format!("output-{i}"));
            let is_virtual = ["Virtual", "HEADLESS", "Meta-"]
                .iter()
                .any(|p| name.starts_with(p));
            Output {
                description: o.description.unwrap_or_default(),
                logical: (o.w > 0 && o.h > 0).then_some((o.x, o.y, o.w as u32, o.h as u32)),
                focused: false,
                is_virtual,
                name,
            }
        })
        .collect();
    if list.is_empty() {
        return Err(anyhow!("the compositor lists no outputs"));
    }
    Ok(list)
}

mod wl {
    pub use wayland_client::protocol::wl_output::{self, WlOutput};
    use wayland_client::{
        Connection, Dispatch, QueueHandle, globals::GlobalListContents,
        protocol::wl_registry::WlRegistry,
    };
    pub use wayland_protocols::xdg::xdg_output::zv1::client::zxdg_output_manager_v1::ZxdgOutputManagerV1 as XdgOutputManager;
    use wayland_protocols::xdg::xdg_output::zv1::client::zxdg_output_v1::{self, ZxdgOutputV1};

    #[derive(Default)]
    pub struct Info {
        pub name: Option<String>,
        pub description: Option<String>,
        pub x: i32,
        pub y: i32,
        pub w: i32,
        pub h: i32,
    }

    #[derive(Default)]
    pub struct State(pub Vec<Info>);

    impl Dispatch<WlRegistry, GlobalListContents> for State {
        fn event(
            _: &mut Self,
            _: &WlRegistry,
            _: wayland_client::protocol::wl_registry::Event,
            _: &GlobalListContents,
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
        }
    }
    impl Dispatch<XdgOutputManager, ()> for State {
        fn event(
            _: &mut Self,
            _: &XdgOutputManager,
            _: <XdgOutputManager as wayland_client::Proxy>::Event,
            _: &(),
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
        }
    }
    impl Dispatch<WlOutput, usize> for State {
        fn event(
            s: &mut Self,
            _: &WlOutput,
            ev: wl_output::Event,
            i: &usize,
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
            match ev {
                wl_output::Event::Name { name } => s.0[*i].name = Some(name),
                wl_output::Event::Description { description } => {
                    s.0[*i].description = Some(description)
                }
                _ => {}
            }
        }
    }
    impl Dispatch<ZxdgOutputV1, usize> for State {
        fn event(
            s: &mut Self,
            _: &ZxdgOutputV1,
            ev: zxdg_output_v1::Event,
            i: &usize,
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
            let o = &mut s.0[*i];
            match ev {
                zxdg_output_v1::Event::LogicalPosition { x, y } => (o.x, o.y) = (x, y),
                zxdg_output_v1::Event::LogicalSize { width, height } => {
                    (o.w, o.h) = (width, height)
                }
                zxdg_output_v1::Event::Name { name } => {
                    o.name.get_or_insert(name);
                }
                _ => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn out(
        name: &str,
        logical: Option<(i32, i32, u32, u32)>,
        focused: bool,
        is_virtual: bool,
    ) -> Output {
        Output {
            name: name.into(),
            description: String::new(),
            logical,
            focused,
            is_virtual,
        }
    }

    #[test]
    fn parses_desktop_names() {
        assert_eq!(Desktop::parse("GNOME"), Some(Desktop::Gnome));
        assert_eq!(Desktop::parse("Hyprland"), Some(Desktop::Hyprland));
        assert_eq!(Desktop::parse("KDE"), Some(Desktop::Kde));
        assert_eq!(Desktop::parse("niri"), Some(Desktop::Niri));
        assert_eq!(Desktop::parse("Unity"), None);
        // XDG_CURRENT_DESKTOP is a colon list; Ubuntu's session is "ubuntu:GNOME".
        assert_eq!(
            "ubuntu:GNOME".split(':').find_map(Desktop::parse),
            Some(Desktop::Gnome)
        );
    }

    #[test]
    fn primary_and_placement() {
        let outs = [
            out("HEADLESS-1", Some((1920, 0, 1356, 610)), true, true),
            out("eDP-1", Some((0, 0, 1920, 1080)), false, false),
            out("HDMI-A-1", None, false, false),
        ];
        // A focused virtual output is never the one to duplicate.
        assert_eq!(primary(&outs, None).unwrap().name, "eDP-1");
        let laptop = (0, 0, 1920, 1080);
        let p = |edge, start, end| Placement {
            edge,
            start,
            end,
            output: None,
        };
        // Right, centred: a 960x540 phone screen sits half-way down the 1080 px edge.
        assert_eq!(
            p(Edge::Right, 0.0, 1.0).position(laptop, (960, 540), &[]),
            (1920, 270)
        );
        // Left, low on the edge: centre 0.9 would hang off the bottom, so it is clamped.
        assert_eq!(
            p(Edge::Left, 0.8, 1.0).position(laptop, (960, 540), &[]),
            (-960, 540)
        );
        assert_eq!(
            p(Edge::Top, 0.0, 0.2).position(laptop, (960, 540), &[]),
            (0, -540)
        );
        assert_eq!(
            p(Edge::Bottom, 0.4, 0.6).position(laptop, (960, 540), &[]),
            (480, 1080)
        );
        // Taller than the edge: it stays touching along the whole edge.
        assert_eq!(
            p(Edge::Right, 0.0, 0.1).position(laptop, (540, 1920), &[]),
            (1920, -840)
        );
        // A monitor already on the right: go past it.
        assert_eq!(
            p(Edge::Right, 0.0, 1.0).position(laptop, (960, 540), &[(1920, 0, 2560, 1440)]),
            (4480, 270)
        );
        // The file format the GTK Cross Control page writes.
        let v: Value =
            serde_json::from_str(r#"{"edge":"left","start":0.221,"end":0.921}"#).unwrap();
        assert_eq!(Placement::parse(&v), Some(p(Edge::Left, 0.221, 0.921)));
        assert_eq!(Placement::parse(&serde_json::json!({"edge":"up"})), None);
        assert_eq!(
            Placement::parse(&serde_json::json!({"edge":"top","start":0.9,"end":0.1})),
            None
        );
    }

    #[test]
    fn the_phone_can_be_anchored_to_any_monitor() {
        let outs = [
            out("HDMI-1", Some((0, 0, 2560, 1440)), true, false),
            out("eDP-1", Some((250, 1440, 1920, 1080)), false, false),
        ];
        // Right of the laptop panel (the phone's screen touches that monitor, not the primary).
        let laptop = Placement {
            edge: Edge::Right,
            start: 0.4,
            end: 0.6,
            output: Some("eDP-1".into()),
        };
        assert_eq!(spot_with(&outs, None, (960, 540), &laptop), (2170, 1710));
        // Parsing keeps the monitor.
        let v: Value =
            serde_json::from_str(r#"{"edge":"right","start":0.4,"end":0.6,"output":"eDP-1"}"#)
                .unwrap();
        assert_eq!(
            Placement::parse(&v).unwrap().output.as_deref(),
            Some("eDP-1")
        );
        // A monitor that is gone falls back to the primary, not to (0, 0).
        let gone = Placement {
            output: Some("DP-9".into()),
            ..laptop.clone()
        };
        assert_eq!(spot_with(&outs, None, (960, 540), &gone), (2560, 450));
        // The anchor is never the output being set up.
        let excluded = Placement {
            output: Some("eDP-1".into()),
            ..laptop
        };
        assert_eq!(
            spot_with(&outs, Some("eDP-1"), (960, 540), &excluded),
            (2560, 450)
        );
    }
}
