//! Cross Control, at the top of the Display tab: the three modes the Windows app offers, side by
//! side, and where the phone sits next to this PC's screen. The display settings follow below them
//! (`display.rs`); `window.rs` puts the parts together.
//!
//! - **Shared mouse and keyboard** runs `smartconnect-crosscontrol` (svc 26).
//! - **Duplicate display** / **Extend display** run a phone-as-display session (svc 202) through the
//!   Display page (`start_mode("mirror" | "extend")`), so its settings apply.
//!
//! Only one mode runs at a time: picking one stops the other. The canvas draws the monitors where
//! the desktop actually has them (`smartconnect-display --list-outputs` carries their logical
//! rects) and the phone next to the monitor it touches, so the picture, Extend's placement and the
//! pointer barrier all agree. Dragging the phone snaps it to a *free* edge of the nearest monitor
//! (one another monitor does not sit against) and stores the monitor, the side and the part of that
//! edge it touches in the shared `crosscontrol.json`. `smartconnect-crosscontrol` reads it at
//! startup (also when the daemon starts it for the phone's tile); Extend and Duplicate read it to
//! pick the monitor the phone is attached to.

use std::cell::{Cell, RefCell};
use std::os::fd::AsRawFd;
use std::path::PathBuf;
use std::rc::{Rc, Weak};
use std::time::Duration;

use adw::prelude::*;
use gtk::{cairo, gdk, glib};
use serde_json::{json, Value};

use crate::display::{DisplayPage, Screen};
use crate::mirror::{Exit, Mirror};

/// The phone's place next to the PC's screens: the monitor it touches, a side of that monitor,
/// and the centre of the phone along that side (0..1 from the top or the left).
#[derive(Clone, Debug, PartialEq)]
pub struct Layout {
    pub edge: Edge,
    pub center: f64,
    /// Connector of the monitor the phone sits next to. `None` = the primary one.
    pub output: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Edge {
    Left,
    Right,
    Top,
    Bottom,
}

impl Edge {
    fn name(self) -> &'static str {
        match self {
            Edge::Left => "left",
            Edge::Right => "right",
            Edge::Top => "top",
            Edge::Bottom => "bottom",
        }
    }

    fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "left" => Edge::Left,
            "right" => Edge::Right,
            "top" => Edge::Top,
            "bottom" => Edge::Bottom,
            _ => return None,
        })
    }

    fn vertical(self) -> bool {
        matches!(self, Edge::Left | Edge::Right)
    }
}

impl Default for Layout {
    fn default() -> Self {
        Self {
            edge: Edge::Right,
            center: 0.5,
            output: None,
        }
    }
}

/// `$XDG_CONFIG_HOME/smartconnect-linux/crosscontrol.json`, shared with `smartconnect-crosscontrol`.
pub fn config_path() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".config")
        })
        .join("smartconnect-linux/crosscontrol.json")
}

/// Drawing: the phone is `PHONE_OF_SCREEN` of the average monitor's height, portrait (a 20:9
/// phone), or on its side while it shows a landscape display session. `GAP` and `MARGIN` are
/// canvas pixels.
const PHONE_OF_SCREEN: f64 = 0.55;
const PHONE_ASPECT: f64 = 9.0 / 20.0;
const GAP: f64 = 7.0;
const MARGIN: f64 = 12.0;

/// One monitor as drawn, in canvas pixels.
#[derive(Clone, Debug, PartialEq)]
struct Drawn {
    name: String,
    description: String,
    primary: bool,
    rect: (f64, f64, f64, f64),
}

/// The real arrangement, fitted into the canvas, with the phone beside the monitor it touches.
#[derive(Clone, Debug)]
struct Geometry {
    screens: Vec<Drawn>,
    /// Phone size: w, h (canvas pixels).
    phone: (f64, f64),
}

impl Geometry {
    /// `screens` are the monitors' logical rects; `layout` says which one the phone is drawn
    /// beside (the fit includes the phone, so the arrangement is never clipped).
    fn new(w: f64, h: f64, screens: &[Screen], layout: &Layout, landscape: bool) -> Self {
        let (w, h) = (w.max(1.0), h.max(1.0));
        // No outputs listed yet, or a helper too old to send rects: one 16:10 screen.
        let fallback = [Screen {
            name: String::new(),
            description: String::new(),
            rect: (0, 0, 1600, 1000),
            primary: true,
        }];
        let list: &[Screen] = if screens.is_empty() {
            &fallback
        } else {
            screens
        };

        let (mut x0, mut y0, mut x1, mut y1) = (f64::MAX, f64::MAX, f64::MIN, f64::MIN);
        let mut tall: f64 = 0.0;
        for s in list {
            let (x, y, sw, sh) = (
                s.rect.0 as f64,
                s.rect.1 as f64,
                s.rect.2 as f64,
                s.rect.3 as f64,
            );
            x0 = x0.min(x);
            y0 = y0.min(y);
            x1 = x1.max(x + sw);
            y1 = y1.max(y + sh);
            tall += sh;
        }
        let mean_h = (tall / list.len() as f64).max(1.0);
        let phone_h = mean_h * PHONE_OF_SCREEN;
        let phone = if landscape {
            (phone_h, phone_h * PHONE_ASPECT)
        } else {
            (phone_h * PHONE_ASPECT, phone_h)
        };
        // Where the phone would be, in the desktop's coordinates, to include it in the fit.
        let anchor = anchor_screen(list, layout)
            .map(|s| s.rect)
            .unwrap_or(list[0].rect);
        let (px, py) = phone_origin(anchor, layout.edge, layout.center, phone);
        let (bx0, by0) = (x0.min(px), y0.min(py));
        let (bx1, by1) = (x1.max(px + phone.0), y1.max(py + phone.1));

        let scale = ((w - 2.0 * MARGIN) / (bx1 - bx0).max(1.0))
            .min((h - 2.0 * MARGIN) / (by1 - by0).max(1.0))
            .max(0.0001);
        let ox = (w - (bx1 - bx0) * scale) / 2.0 - bx0 * scale;
        let oy = (h - (by1 - by0) * scale) / 2.0 - by0 * scale;
        let at = |r: (i32, i32, u32, u32)| {
            (
                ox + r.0 as f64 * scale,
                oy + r.1 as f64 * scale,
                r.2 as f64 * scale,
                r.3 as f64 * scale,
            )
        };
        Geometry {
            screens: list
                .iter()
                .map(|s| Drawn {
                    name: s.name.clone(),
                    description: s.description.clone(),
                    primary: s.primary,
                    rect: at(s.rect),
                })
                .collect(),
            phone: (phone.0 * scale, phone.1 * scale),
        }
    }

    /// The monitor the phone is drawn beside: the saved one, else the primary, else the first.
    fn anchor(&self, l: &Layout) -> Option<&Drawn> {
        l.output
            .as_deref()
            .and_then(|name| self.screens.iter().find(|s| s.name == name))
            .or_else(|| self.screens.iter().find(|s| s.primary))
            .or_else(|| self.screens.first())
    }

    /// Length of `rect`'s edge the phone covers, as a fraction of that edge.
    fn span_len(&self, edge: Edge, rect: (f64, f64, f64, f64)) -> f64 {
        let (_, _, w, h) = rect;
        if edge.vertical() {
            (self.phone.1 / h).min(1.0)
        } else {
            (self.phone.0 / w).min(1.0)
        }
    }

    /// Top-left corner of the phone for a layout, in canvas pixels.
    fn phone_at(&self, l: &Layout) -> (f64, f64) {
        let Some(d) = self.anchor(l) else {
            return (0.0, 0.0);
        };
        let (x, y, w, h) = d.rect;
        let (pw, ph) = self.phone;
        let c = l.center.clamp(0.0, 1.0);
        let along = |origin: f64, len: f64, extent: f64| {
            if extent <= len {
                (origin + c * len - extent / 2.0).clamp(origin, origin + len - extent)
            } else {
                origin + c * len - extent / 2.0
            }
        };
        match l.edge {
            Edge::Right => (x + w + GAP, along(y, h, ph)),
            Edge::Left => (x - GAP - pw, along(y, h, ph)),
            Edge::Top => (along(x, w, pw), y - GAP - ph),
            Edge::Bottom => (along(x, w, pw), y + h + GAP),
        }
    }

    /// `l` with its centre kept so the phone does not run off the monitor's edge (unless it is
    /// longer than that edge, and then it is centred).
    fn clamped(&self, l: &Layout) -> Layout {
        let Some(d) = self.anchor(l) else {
            return l.clone();
        };
        let half = self.span_len(l.edge, d.rect) / 2.0;
        let center = if half >= 0.5 {
            0.5
        } else {
            l.center.clamp(half, 1.0 - half)
        };
        Layout {
            edge: l.edge,
            center,
            output: l.output.clone(),
        }
    }

    /// Whether no other monitor sits against this edge of `idx`: only then does the pointer leave
    /// this PC there (the desktop refuses barriers on edges shared by two monitors).
    fn free(&self, idx: usize, edge: Edge) -> bool {
        let (x, y, w, h) = self.screens[idx].rect;
        let near = |a: f64, b: f64| (a - b).abs() < 2.0;
        !self.screens.iter().enumerate().any(|(i, o)| {
            if i == idx {
                return false;
            }
            let (ox, oy, ow, oh) = o.rect;
            let overlap_y = oy < y + h && y < oy + oh;
            let overlap_x = ox < x + w && x < ox + ow;
            match edge {
                Edge::Right => near(ox, x + w) && overlap_y,
                Edge::Left => near(ox + ow, x) && overlap_y,
                Edge::Top => near(oy + oh, y) && overlap_x,
                Edge::Bottom => near(oy, y + h) && overlap_x,
            }
        })
    }

    /// The layout nearest to a phone whose top-left corner is at `(px, py)`: a free edge of the
    /// monitor the phone was dragged onto, and the part of that edge it is nearest.
    fn snap(&self, px: f64, py: f64, current: &Layout) -> Layout {
        let (pw, ph) = self.phone;
        let (cx, cy) = (px + pw / 2.0, py + ph / 2.0);
        let mut best: Option<(f64, Layout)> = None;
        for (i, d) in self.screens.iter().enumerate() {
            let (x, y, w, h) = d.rect;
            for edge in [Edge::Left, Edge::Right, Edge::Top, Edge::Bottom] {
                if !self.free(i, edge) {
                    continue;
                }
                let center = if edge.vertical() {
                    (cy - y) / h.max(1.0)
                } else {
                    (cx - x) / w.max(1.0)
                };
                let l = self.clamped(&Layout {
                    edge,
                    center,
                    output: Some(d.name.clone()),
                });
                let (qx, qy) = self.phone_at(&l);
                let dist = (qx - px).powi(2) + (qy - py).powi(2);
                if best.as_ref().is_none_or(|(bd, _)| dist < *bd) {
                    best = Some((dist, l));
                }
            }
        }
        if let Some((_, l)) = best {
            return l;
        }
        // Every edge is shared with another monitor (a tiled layout): use the nearest side of the
        // nearest monitor anyway, so the phone still moves.
        let Some((_, d)) = self
            .screens
            .iter()
            .map(|d| {
                let (x, y, w, h) = d.rect;
                ((x + w / 2.0 - cx).powi(2) + (y + h / 2.0 - cy).powi(2), d)
            })
            .filter(|(dist, _)| dist.is_finite())
            .min_by(|a, b| a.0.total_cmp(&b.0))
        else {
            return current.clone();
        };
        let (x, y, w, h) = d.rect;
        let (nx, ny) = (
            (cx - (x + w / 2.0)) / (w / 2.0).max(1.0),
            (cy - (y + h / 2.0)) / (h / 2.0).max(1.0),
        );
        let (edge, center) = if nx.abs() >= ny.abs() {
            (
                if nx >= 0.0 { Edge::Right } else { Edge::Left },
                (cy - y) / h.max(1.0),
            )
        } else {
            (
                if ny >= 0.0 { Edge::Bottom } else { Edge::Top },
                (cx - x) / w.max(1.0),
            )
        };
        self.clamped(&Layout {
            edge,
            center,
            output: Some(d.name.clone()),
        })
    }
}

/// The monitor `l` names, else the primary one, else the first.
fn anchor_screen<'a>(screens: &'a [Screen], l: &Layout) -> Option<&'a Screen> {
    l.output
        .as_deref()
        .and_then(|name| screens.iter().find(|s| s.name == name))
        .or_else(|| screens.iter().find(|s| s.primary))
        .or_else(|| screens.first())
}

/// Where a `phone`-sized screen goes beside `anchor` (desktop coordinates; the phone touches the
/// monitor's edge). Mirrors `Geometry::phone_at`, which is the same in canvas pixels.
fn phone_origin(
    anchor: (i32, i32, u32, u32),
    edge: Edge,
    center: f64,
    phone: (f64, f64),
) -> (f64, f64) {
    let (x, y, w, h) = (
        anchor.0 as f64,
        anchor.1 as f64,
        anchor.2 as f64,
        anchor.3 as f64,
    );
    let c = center.clamp(0.0, 1.0);
    let along = |origin: f64, len: f64, extent: f64| {
        if extent <= len {
            (origin + c * len - extent / 2.0).clamp(origin, origin + len - extent)
        } else {
            origin + c * len - extent / 2.0
        }
    };
    match edge {
        Edge::Right => (x + w, along(y, h, phone.1)),
        Edge::Left => (x - phone.0, along(y, h, phone.1)),
        Edge::Top => (along(x, w, phone.0), y - phone.1),
        Edge::Bottom => (along(x, w, phone.0), y + h),
    }
}

/// Reads `crosscontrol.json`, if there is one.
fn load_layout() -> Option<Layout> {
    let v: Value = serde_json::from_str(&std::fs::read_to_string(config_path()).ok()?).ok()?;
    parse_layout(&v)
}

/// The shared file's format: `edge`, `start`, `end`, and (new) `output` = the monitor the phone
/// touches; `rect` is written for the portal and ignored here (it is the output's, not a place).
fn parse_layout(v: &Value) -> Option<Layout> {
    let edge = Edge::parse(v.get("edge")?.as_str()?)?;
    let start = v.get("start").and_then(Value::as_f64).unwrap_or(0.0);
    let end = v.get("end").and_then(Value::as_f64).unwrap_or(1.0);
    (0.0..=1.0).contains(&start).then_some(())?;
    (start < end && end <= 1.0).then_some(())?;
    let output = v
        .get("output")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned);
    Some(Layout {
        edge,
        center: (start + end) / 2.0,
        output,
    })
}

/// What goes in the shared file: the side, the part of that edge the phone covers, the monitor it
/// touches and that monitor's rect (the portal has no output names, so the rect is how
/// `smartconnect-crosscontrol` finds the right barrier segment).
fn layout_json(l: &Layout, start: f64, end: f64, rect: Option<(i32, i32, u32, u32)>) -> Value {
    let round = |f: f64| (f * 1000.0).round() / 1000.0;
    let mut v = json!({ "edge": l.edge.name(), "start": round(start), "end": round(end) });
    let obj = v.as_object_mut().expect("object");
    if let Some(name) = &l.output {
        obj.insert("output".into(), json!(name));
    }
    if let Some((x, y, w, h)) = rect {
        obj.insert("rect".into(), json!([x, y, w, h]));
    }
    v
}

fn save_layout(
    l: &Layout,
    start: f64,
    end: f64,
    rect: Option<(i32, i32, u32, u32)>,
) -> std::io::Result<()> {
    let path = config_path();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let text = serde_json::to_string_pretty(&layout_json(l, start, end, rect))?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, text + "\n")?;
    std::fs::rename(tmp, path)
}

fn crosscontrol_lock_path() -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("smartconnect-crosscontrol.lock")
}

/// The pid of a running `smartconnect-crosscontrol` (ours, the daemon's or a terminal's), from its
/// lock file: `Some` only while the lock is held.
fn running_crosscontrol() -> Option<libc::pid_t> {
    let path = crosscontrol_lock_path();
    let file = std::fs::File::open(&path).ok()?;
    // SAFETY: flock(2) on a file we own for the duration of the call.
    let free = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_SH | libc::LOCK_NB) } == 0;
    if free {
        return None; // dropped below, which releases our probe lock
    }
    std::fs::read_to_string(&path).ok()?.trim().parse().ok()
}

/// Which mode is running right now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    Shared,
    Duplicate,
    Extend,
}

#[allow(clippy::type_complexity)]
pub struct CrossControlPage {
    toasts: adw::ToastOverlay,
    display: Rc<DisplayPage>,
    cards: Vec<(Mode, gtk::ToggleButton, gtk::Image)>,
    status: gtk::Label,
    canvas: gtk::DrawingArea,
    position: gtk::Label,
    cross: RefCell<Mirror>,
    layout: RefCell<Layout>,
    /// The monitors as last reported by the display helper (real arrangement).
    screens: RefCell<Vec<Screen>>,
    /// The phone is drawn on its side: a display session runs with the phone held landscape.
    landscape: Cell<bool>,
    /// The phone's top-left corner while it is being dragged, and where in the phone it was grabbed.
    drag: Cell<Option<((f64, f64), (f64, f64))>>,
    /// The canvas as it was when the drag started: snapping must not shift under the pointer.
    drag_geometry: RefCell<Option<Geometry>>,
    /// Our client was stopped to apply a new position: start it again once it has exited.
    restart_after_exit: Cell<bool>,
    connected: Cell<bool>,
    syncing: Cell<bool>,
    polling: Cell<bool>,
}

fn mode_card(icon: &str, title: &str, subtitle: &str) -> (gtk::ToggleButton, gtk::Image) {
    let image = gtk::Image::builder()
        .icon_name(icon)
        .pixel_size(40)
        .margin_bottom(6)
        .build();
    let title = gtk::Label::builder()
        .label(title)
        .css_classes(["heading"])
        .wrap(true)
        .justify(gtk::Justification::Center)
        .build();
    let sub = gtk::Label::builder()
        .label(subtitle)
        .css_classes(["dim-label", "caption"])
        .wrap(true)
        .justify(gtk::Justification::Center)
        .max_width_chars(18)
        .build();
    let check = gtk::Image::builder()
        .icon_name("object-select-symbolic")
        .css_classes(["accent"])
        .visible(false)
        .build();
    let content = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(4)
        .valign(gtk::Align::Center)
        .build();
    content.append(&image);
    content.append(&title);
    content.append(&sub);
    content.append(&check);
    let button = gtk::ToggleButton::builder()
        .child(&content)
        .css_classes(["card", "sc-mode"])
        .hexpand(true)
        .build();
    (button, check)
}

impl CrossControlPage {
    /// Returns the page, the modes (title, cards, status) and the phone-position group, for the
    /// Display tab to place around the display settings.
    pub fn build(
        toasts: &adw::ToastOverlay,
        display: Rc<DisplayPage>,
    ) -> (Rc<Self>, gtk::Widget, gtk::Widget) {
        let title = gtk::Label::builder()
            .label("Cross Control")
            .xalign(0.0)
            .css_classes(["title-2"])
            .build();
        let intro = gtk::Label::builder()
            .label("Use your phone next to this PC: share this PC's mouse and keyboard with it, or use it as a second display.")
            .xalign(0.0)
            .wrap(true)
            .css_classes(["dim-label"])
            .build();

        let modes = gtk::Box::builder().spacing(12).homogeneous(true).build();
        let mut cards = Vec::new();
        for (mode, icon, name, sub) in [
            (
                Mode::Shared,
                "input-mouse-symbolic",
                "Shared mouse and keyboard",
                "Move the pointer past the screen edge onto the phone",
            ),
            (
                Mode::Duplicate,
                "video-joined-displays-symbolic",
                "Duplicate display",
                "The phone shows this PC's screen",
            ),
            (
                Mode::Extend,
                "video-display-symbolic",
                "Extend display",
                "The phone becomes a second screen",
            ),
        ] {
            let (button, check) = mode_card(icon, name, sub);
            modes.append(&button);
            cards.push((mode, button, check));
        }
        let status = gtk::Label::builder()
            .xalign(0.0)
            .wrap(true)
            .css_classes(["dim-label"])
            .build();

        let canvas = gtk::DrawingArea::builder()
            .content_height(320)
            .hexpand(true)
            .cursor(&gdk::Cursor::from_name("grab", None).expect("cursor"))
            .build();
        let canvas_card = gtk::Box::builder().css_classes(["card"]).build();
        canvas_card.append(&canvas);
        let position = gtk::Label::builder()
            .xalign(0.0)
            .wrap(true)
            .css_classes(["dim-label", "caption"])
            .build();
        let arrange = adw::PreferencesGroup::builder()
            .title("Phone position")
            .description("Drag the phone to a free edge of the monitor it sits next to. Extend display puts the phone's screen there, Duplicate shows that monitor, and shared mouse and keyboard crosses over along the highlighted part of that edge.")
            .build();
        let arrange_box = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(8)
            .build();
        arrange_box.append(&canvas_card);
        arrange_box.append(&position);
        arrange.add(&arrange_box);

        let shortcuts = gtk::Label::builder()
            .label("Push the pointer back out of the phone, or press Ctrl+Alt+Esc, to return to this PC.")
            .xalign(0.0)
            .wrap(true)
            .css_classes(["dim-label", "caption"])
            .build();

        let top = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(12)
            .build();
        top.append(&title);
        top.append(&intro);
        top.append(&modes);
        top.append(&status);
        top.append(&shortcuts);

        let layout = load_layout().unwrap_or_default();
        let page = Rc::new(Self {
            toasts: toasts.clone(),
            screens: RefCell::new(display.screens()),
            display,
            cards,
            status,
            canvas,
            position,
            cross: RefCell::new(Mirror::cross_control()),
            layout: RefCell::new(layout),
            landscape: Cell::new(false),
            drag: Cell::new(None),
            drag_geometry: RefCell::new(None),
            restart_after_exit: Cell::new(false),
            connected: Cell::new(false),
            syncing: Cell::new(false),
            polling: Cell::new(false),
        });
        page.connect();
        page.update_position_label();
        page.sync();
        (page, top.upcast(), arrange.upcast())
    }

    fn connect(self: &Rc<Self>) {
        for (mode, button, _) in &self.cards {
            let weak = Rc::downgrade(self);
            let mode = *mode;
            button.connect_toggled(move |_| {
                if let Some(page) = weak.upgrade() {
                    if !page.syncing.get() {
                        page.pick(mode);
                    }
                }
            });
        }

        let weak: Weak<Self> = Rc::downgrade(self);
        self.canvas.set_draw_func(move |area, cr, w, h| {
            if let Some(page) = weak.upgrade() {
                page.draw(area, cr, w as f64, h as f64);
            }
        });

        let drag = gtk::GestureDrag::new();
        let weak = Rc::downgrade(self);
        drag.connect_drag_begin(move |_, x, y| {
            let Some(page) = weak.upgrade() else { return };
            // The monitors may have changed since the last poll: draw and snap the current ones.
            page.follow_screens();
            let g = page.geometry();
            let (px, py) = g.phone_at(&page.layout.borrow());
            let (pw, ph) = g.phone;
            if x >= px && x <= px + pw && y >= py && y <= py + ph {
                page.drag.set(Some(((px, py), (x - px, y - py))));
                page.drag_geometry.replace(Some(g));
                page.canvas.set_cursor_from_name(Some("grabbing"));
                page.canvas.queue_draw();
            }
        });
        let weak = Rc::downgrade(self);
        drag.connect_drag_update(move |gesture, dx, dy| {
            let Some(page) = weak.upgrade() else { return };
            let (Some((_, grab)), Some((sx, sy))) = (page.drag.get(), gesture.start_point()) else {
                return;
            };
            page.drag
                .set(Some(((sx + dx - grab.0, sy + dy - grab.1), grab)));
            page.canvas.queue_draw();
        });
        let weak = Rc::downgrade(self);
        drag.connect_drag_end(move |_, _, _| {
            let Some(page) = weak.upgrade() else { return };
            let Some(((px, py), _)) = page.drag.take() else {
                return;
            };
            let g = page.drag_geometry.take().unwrap_or_else(|| page.geometry());
            page.canvas.set_cursor_from_name(Some("grab"));
            let layout = g.snap(px, py, &page.layout.borrow());
            page.set_layout(layout);
        });
        self.canvas.add_controller(drag);

        // Keep the drawing in step with the theme's accent colour.
        let weak = Rc::downgrade(self);
        adw::StyleManager::default().connect_accent_color_rgba_notify(move |_| {
            if let Some(page) = weak.upgrade() {
                page.canvas.queue_draw();
            }
        });

        // Monitors get plugged, unplugged and rearranged: follow the display page's output list
        // (only while the page is on screen; the list itself is refreshed by the Display page).
        let weak = Rc::downgrade(self);
        glib::timeout_add_seconds_local(3, move || {
            let Some(page) = weak.upgrade() else {
                return glib::ControlFlow::Break;
            };
            if page.canvas.is_mapped() {
                page.follow_screens();
            }
            glib::ControlFlow::Continue
        });
    }

    /// Takes the display page's current monitor list; redraws when the arrangement changed.
    fn follow_screens(&self) {
        let now = self.display.screens();
        if now == *self.screens.borrow() {
            return;
        }
        self.screens.replace(now);
        self.update_position_label();
        self.canvas.queue_draw();
    }

    /// The phone link is up: modes can start.
    pub fn set_connected(self: &Rc<Self>, connected: bool) {
        self.connected.set(connected);
        self.sync();
    }

    /// Called when the page is shown or the phone asked for Cross Control: refresh, and poll while
    /// anything runs.
    pub fn refresh(self: &Rc<Self>) {
        self.sync();
        self.poll_soon();
    }

    /// Stops our own Cross Control client when the app quits (a display session keeps running, as
    /// the Display page does).
    pub fn shutdown(&self) {
        self.cross.borrow_mut().stop();
    }

    fn running(&self) -> Option<Mode> {
        let cross = self.cross.borrow();
        if (cross.running() && !cross.stopping())
            || (!cross.running() && running_crosscontrol().is_some())
        {
            return Some(Mode::Shared);
        }
        match self.display.running_mode().as_deref() {
            Some("mirror") => Some(Mode::Duplicate),
            Some("extend") => Some(Mode::Extend),
            _ => None,
        }
    }

    /// Starts `mode`, or stops it when it is the one running; any other running mode is stopped.
    fn pick(self: &Rc<Self>, mode: Mode) {
        let running = self.running();
        if running == Some(mode) {
            match mode {
                Mode::Shared => self.stop_shared(),
                _ => self.display.stop_session(),
            }
            self.after_change();
            return;
        }
        if !self.connected.get() {
            self.toasts
                .add_toast(adw::Toast::new("Connect the phone first"));
            self.after_change();
            return;
        }
        match running {
            Some(Mode::Shared) => self.stop_shared(),
            Some(_) if mode == Mode::Shared => self.display.stop_session(),
            _ => {}
        }
        match mode {
            Mode::Shared => self.start_shared(),
            Mode::Duplicate => self.display.start_mode("mirror"),
            Mode::Extend => self.display.start_mode("extend"),
        }
        self.after_change();
    }

    fn start_shared(&self) {
        // The client reads crosscontrol.json itself; a running one (e.g. the daemon's, for the
        // phone's tile) is taken over.
        if let Err(e) = self.cross.borrow_mut().start(&[]) {
            self.toasts.add_toast(adw::Toast::new(&e));
        }
    }

    fn stop_shared(&self) {
        let mut cross = self.cross.borrow_mut();
        if cross.running() {
            cross.stop();
        } else if let Some(pid) = running_crosscontrol() {
            // Started by the daemon (phone tile) or a terminal: SIGINT tells the phone to stop.
            // SAFETY: plain kill(2) on the pid from the client's own lock file.
            unsafe { libc::kill(pid, libc::SIGINT) };
        }
    }

    fn after_change(self: &Rc<Self>) {
        self.sync();
        self.poll_soon();
    }

    /// Polls every 500 ms while something runs or is winding down, to follow helpers that exit
    /// and sessions started elsewhere.
    fn poll_soon(self: &Rc<Self>) {
        if self.polling.replace(true) {
            return;
        }
        let weak = Rc::downgrade(self);
        glib::timeout_add_local(Duration::from_millis(500), move || {
            let Some(page) = weak.upgrade() else {
                return glib::ControlFlow::Break;
            };
            let exit = page.cross.borrow_mut().poll();
            if let Some(Exit::Failed(msg)) = &exit {
                page.toasts.add_toast(adw::Toast::new(msg));
            }
            if exit.is_some() && page.restart_after_exit.take() {
                page.start_shared();
            }
            page.sync();
            let busy = page.cross.borrow().running() || page.running().is_some();
            if busy {
                glib::ControlFlow::Continue
            } else {
                page.polling.set(false);
                glib::ControlFlow::Break
            }
        });
    }

    fn sync(&self) {
        let running = self.running();
        self.syncing.set(true);
        for (mode, button, check) in &self.cards {
            let on = running == Some(*mode);
            button.set_active(on);
            check.set_visible(on);
            button.set_sensitive(self.connected.get() || on);
        }
        self.syncing.set(false);
        let text = match (running, self.connected.get()) {
            (Some(Mode::Shared), _) => {
                "Shared mouse and keyboard is on: push the pointer into the highlighted edge."
            }
            (Some(Mode::Duplicate), _) => "The phone is showing this PC's screen.",
            (Some(Mode::Extend), _) => "The phone is a second screen for this PC.",
            (None, true) => "Pick a mode to start. Pick it again to stop.",
            (None, false) => "Connect the phone to use Cross Control.",
        };
        self.status.set_label(text);
        let landscape = self.display.phone_landscape().unwrap_or(false);
        if self.landscape.replace(landscape) != landscape {
            self.canvas.queue_draw();
        }
    }

    // ── Phone position ──────────────────────────────────────────────────────

    fn geometry(&self) -> Geometry {
        Geometry::new(
            self.canvas.width() as f64,
            self.canvas.height() as f64,
            &self.screens.borrow(),
            &self.layout.borrow(),
            self.landscape.get(),
        )
    }

    fn set_layout(self: &Rc<Self>, layout: Layout) {
        let g = self.geometry();
        let half = g
            .anchor(&layout)
            .map(|d| g.span_len(layout.edge, d.rect))
            .unwrap_or(1.0)
            / 2.0;
        // The monitor's rect goes in the file too: the portal has no output names.
        let name = match &layout.output {
            Some(name) => Some(name.clone()),
            None => self
                .screens
                .borrow()
                .iter()
                .find(|s| s.primary)
                .map(|s| s.name.clone()),
        };
        let rect = name.and_then(|n| {
            self.screens
                .borrow()
                .iter()
                .find(|s| s.name == n)
                .map(|s| s.rect)
        });
        if let Err(e) = save_layout(&layout, layout.center - half, layout.center + half, rect) {
            self.toasts.add_toast(adw::Toast::new(&format!(
                "Could not save the phone position: {e}"
            )));
        }
        self.layout.replace(layout);
        self.update_position_label();
        self.canvas.queue_draw();
        self.poll_soon();
        match self.running() {
            // Cross Control only reads the position at startup: restart our client to apply it.
            Some(Mode::Shared) => {
                let mut cross = self.cross.borrow_mut();
                if cross.running() {
                    cross.stop();
                    self.restart_after_exit.set(true);
                } else {
                    drop(cross);
                    self.start_shared(); // takes over the daemon's client
                }
            }
            // Duplicate reads the monitor it mirrors at startup; Extend follows the file by itself.
            Some(Mode::Duplicate) => self.display.restart_session(),
            _ => {}
        }
    }

    /// The monitor the phone is drawn beside, with a monitor name or model for the label.
    fn anchor_name(&self, layout: &Layout) -> (String, bool) {
        let screens = self.screens.borrow();
        let found = layout
            .output
            .as_deref()
            .and_then(|n| screens.iter().find(|s| s.name == n));
        let missing = layout
            .output
            .as_deref()
            .is_some_and(|n| !screens.iter().any(|s| s.name == n));
        let anchor = found
            .or_else(|| screens.iter().find(|s| s.primary))
            .or_else(|| screens.first());
        match anchor {
            Some(s) if !s.description.is_empty() && s.description != s.name => {
                (format!("{} ({})", s.name, s.description), missing)
            }
            Some(s) => (s.name.clone(), missing),
            None => ("this PC".into(), missing),
        }
    }

    fn update_position_label(&self) {
        let layout = self.layout.borrow();
        let side = match layout.edge {
            Edge::Left => "left of",
            Edge::Right => "right of",
            Edge::Top => "above",
            Edge::Bottom => "below",
        };
        let (name, missing) = self.anchor_name(&layout);
        let text = if missing {
            format!("The phone is {side} {name}: the monitor saved for it is not connected, so the primary one is used.")
        } else {
            format!("The phone is {side} {name}.")
        };
        self.position.set_label(&text);
    }

    fn draw(&self, area: &gtk::DrawingArea, cr: &cairo::Context, w: f64, h: f64) {
        let g = Geometry::new(
            w,
            h,
            &self.screens.borrow(),
            &self.layout.borrow(),
            self.landscape.get(),
        );
        let fg = area.color();
        let accent = adw::StyleManager::default().accent_color_rgba();
        let set = |c: &gdk::RGBA, alpha: f32| {
            cr.set_source_rgba(
                c.red() as f64,
                c.green() as f64,
                c.blue() as f64,
                (c.alpha() * alpha) as f64,
            )
        };
        let layout = g.clamped(&self.layout.borrow());

        // This PC's monitors, where they really are.
        for d in &g.screens {
            let (x, y, sw, sh) = d.rect;
            let anchor = g.anchor(&layout).is_some_and(|a| a.name == d.name);
            rounded(cr, x, y, sw, sh, 6.0);
            set(&fg, 0.08);
            let _ = cr.fill_preserve();
            set(
                if anchor { &accent } else { &fg },
                if anchor { 0.9 } else { 0.4 },
            );
            cr.set_line_width(if anchor { 2.5 } else { 1.5 });
            let _ = cr.stroke();
            if sw > 64.0 && sh > 34.0 {
                set(&fg, 0.7);
                let name = if d.name.is_empty() {
                    "This PC"
                } else {
                    &d.name
                };
                label(cr, name, x + sw / 2.0, y + sh / 2.0 - 7.0, 12.0);
                let sub = match (&d.description, d.primary) {
                    (d, true) if !d.is_empty() => format!("{d} · primary"),
                    (d, false) if !d.is_empty() => d.clone(),
                    _ if d.primary => "primary".to_owned(),
                    _ => String::new(),
                };
                if !sub.is_empty() {
                    set(&fg, 0.5);
                    label(cr, &sub, x + sw / 2.0, y + sh / 2.0 + 8.0, 10.0);
                }
            }
            // The primary monitor's corner, so it is visible even when the label is not drawn.
            if d.primary && (sw <= 64.0 || sh <= 34.0) {
                set(&fg, 0.6);
                cr.arc(x + 5.0, y + 5.0, 2.0, 0.0, 2.0 * std::f64::consts::PI);
                let _ = cr.fill();
            }
        }

        // Where the pointer crosses over: the part of the anchor monitor's edge the phone touches.
        if self.drag.get().is_none() {
            let (x, y, sw, sh) = g
                .anchor(&layout)
                .map(|d| d.rect)
                .unwrap_or((0.0, 0.0, 0.0, 0.0));
            let half = g.span_len(layout.edge, (x, y, sw, sh)) / 2.0;
            let (a, b) = (layout.center - half, layout.center + half);
            let (x1, y1, x2, y2) = match layout.edge {
                Edge::Right => (x + sw, y + a * sh, x + sw, y + b * sh),
                Edge::Left => (x, y + a * sh, x, y + b * sh),
                Edge::Top => (x + a * sw, y, x + b * sw, y),
                Edge::Bottom => (x + a * sw, y + sh, x + b * sw, y + sh),
            };
            set(&accent, 1.0);
            cr.set_line_width(4.0);
            cr.set_line_cap(cairo::LineCap::Round);
            cr.move_to(x1, y1);
            cr.line_to(x2, y2);
            let _ = cr.stroke();
        }

        // The phone (where it is being dragged, or its saved place).
        let (px, py) = self
            .drag
            .get()
            .map(|(p, _)| p)
            .unwrap_or_else(|| g.phone_at(&layout));
        let (pw, ph) = g.phone;
        rounded(cr, px, py, pw, ph, 7.0);
        set(&accent, 0.22);
        let _ = cr.fill_preserve();
        set(&accent, 1.0);
        cr.set_line_width(2.0);
        let _ = cr.stroke();
        // Speaker slot, so it reads as a phone.
        set(&accent, 0.8);
        rounded(cr, px + pw * 0.35, py + 5.0, pw * 0.3, 3.0, 1.5);
        let _ = cr.fill();
        set(&fg, 0.7);
        label(cr, "Phone", px + pw / 2.0, py + ph / 2.0, 11.0);
    }
}

fn rounded(cr: &cairo::Context, x: f64, y: f64, w: f64, h: f64, r: f64) {
    let r = r.min(w / 2.0).min(h / 2.0);
    cr.new_sub_path();
    cr.arc(x + w - r, y + r, r, -std::f64::consts::FRAC_PI_2, 0.0);
    cr.arc(x + w - r, y + h - r, r, 0.0, std::f64::consts::FRAC_PI_2);
    cr.arc(
        x + r,
        y + h - r,
        r,
        std::f64::consts::FRAC_PI_2,
        std::f64::consts::PI,
    );
    cr.arc(
        x + r,
        y + r,
        r,
        std::f64::consts::PI,
        1.5 * std::f64::consts::PI,
    );
    cr.close_path();
}

fn label(cr: &cairo::Context, text: &str, cx: f64, cy: f64, size: f64) {
    cr.select_font_face("Sans", cairo::FontSlant::Normal, cairo::FontWeight::Normal);
    cr.set_font_size(size);
    if let Ok(ext) = cr.text_extents(text) {
        cr.move_to(
            cx - ext.width() / 2.0 - ext.x_bearing(),
            cy - ext.height() / 2.0 - ext.y_bearing(),
        );
        let _ = cr.show_text(text);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The user's real layout: a 2560×1440 external monitor (primary) with the 1920×1080 laptop
    /// panel below it, offset 250 px.
    fn laptop_below() -> Vec<Screen> {
        vec![
            Screen {
                name: "HDMI-1".into(),
                description: "Microstep 27\"".into(),
                rect: (0, 0, 2560, 1440),
                primary: true,
            },
            Screen {
                name: "eDP-1".into(),
                description: "Built-in display".into(),
                rect: (250, 1440, 1920, 1080),
                primary: false,
            },
        ]
    }

    fn side_by_side() -> Vec<Screen> {
        vec![
            Screen {
                name: "DP-1".into(),
                description: "".into(),
                rect: (0, 0, 1920, 1080),
                primary: true,
            },
            Screen {
                name: "DP-2".into(),
                description: "".into(),
                rect: (1920, 0, 1920, 1080),
                primary: false,
            },
        ]
    }

    fn geo(screens: &[Screen], l: &Layout, landscape: bool) -> Geometry {
        Geometry::new(800.0, 320.0, screens, l, landscape)
    }

    fn right_of(index: usize) -> Layout {
        Layout {
            edge: Edge::Right,
            center: 0.5,
            output: Some(["HDMI-1", "eDP-1"][index].into()),
        }
    }

    #[test]
    fn the_canvas_draws_the_real_arrangement() {
        let l = right_of(0);
        let g = geo(&laptop_below(), &l, false);
        assert_eq!(g.screens.len(), 2);
        let (hx, hy, _, hh) = g.screens[0].rect;
        let (ex, ey, ew, eh) = g.screens[1].rect;
        // HDMI-1 on top, the laptop panel below and to the right (its 250 px offset).
        assert!(
            ey >= hy + hh - 1.0,
            "eDP-1 is below HDMI-1: {ey} >= {}",
            hy + hh
        );
        assert!(ex > hx + 10.0, "eDP-1 is offset to the right: {ex} > {hx}");
        // The whole thing (phone included) fits in the canvas.
        assert!(hx >= 0.0 && ex + ew <= 800.0 && ey + eh <= 320.0);
        assert!(
            (ew / eh - 1920.0 / 1080.0).abs() < 0.02,
            "aspect ratios are kept"
        );
        // The phone keeps its 20:9 portrait shape and is about half a monitor tall.
        assert!((g.phone.1 / g.phone.0 - 20.0 / 9.0).abs() < 0.02);
        assert!(g.phone.1 < eh && g.phone.1 > eh * 0.4);
    }

    #[test]
    fn the_phone_snaps_to_free_edges_of_either_monitor() {
        let screens = laptop_below();
        let g = geo(&screens, &right_of(0), false);
        let (pw, ph) = g.phone;
        // Dropped to the right of the laptop panel: it snaps there, not to the primary monitor.
        let (_, ey, _, eh) = g.screens[1].rect;
        let (ex, ew, _) = (
            g.screens[1].rect.0,
            g.screens[1].rect.2,
            g.screens[1].rect.3,
        );
        let l = g.snap(ex + ew + GAP + 3.0, ey + eh / 2.0 - ph / 2.0, &right_of(0));
        assert_eq!(l.output.as_deref(), Some("eDP-1"));
        assert_eq!(l.edge, Edge::Right);
        assert!((l.center - 0.5).abs() < 0.08, "{}", l.center);
        // Above the external monitor.
        let (hx, hy, hw, _) = g.screens[0].rect;
        let l = g.snap(hx + hw / 2.0 - pw / 2.0, hy - GAP - ph - 3.0, &right_of(0));
        assert_eq!(l.output.as_deref(), Some("HDMI-1"));
        assert_eq!(l.edge, Edge::Top);
        // On its side while a landscape session runs: the phone is drawn and snapped landscape.
        let gl = geo(&screens, &right_of(0), true);
        assert!(gl.phone.0 > gl.phone.1);
        let l = gl.snap(0.0, 0.0, &right_of(0));
        assert!(!l.output.is_none());
    }

    #[test]
    fn an_edge_shared_with_another_monitor_is_not_offered() {
        let screens = side_by_side();
        let g = geo(&screens, &right_of(0), false);
        let (_, _, aw, ah) = g.screens[0].rect;
        let (ax, ay) = (g.screens[0].rect.0, g.screens[0].rect.1);
        // Dropped in the seam between the two monitors.
        let l = g.snap(ax + aw + GAP, ay + ah / 2.0 - g.phone.1 / 2.0, &right_of(0));
        assert!(
            !(l.output.as_deref() == Some("DP-1") && l.edge == Edge::Right),
            "DP-1's right edge is shared with DP-2, the pointer cannot leave the desktop there: {l:?}"
        );
        // DP-2's right edge is the outer one, so it is offered.
        let l = g.snap(
            2.0 * aw + GAP + 3.0,
            ay + ah / 2.0 - g.phone.1 / 2.0,
            &right_of(0),
        );
        assert_eq!(l.output.as_deref(), Some("DP-2"));
        assert_eq!(l.edge, Edge::Right);
    }

    #[test]
    fn a_saved_position_keeps_its_monitor() {
        let v: Value =
            serde_json::from_str(r#"{"edge":"left","start":0.4,"end":0.6,"output":"eDP-1"}"#)
                .unwrap();
        let l = parse_layout(&v).unwrap();
        assert_eq!((l.edge, l.output.as_deref()), (Edge::Left, Some("eDP-1")));
        assert!((l.center - 0.5).abs() < 1e-9);
        // An older file has no monitor: the primary one is used.
        let v: Value = serde_json::from_str(r#"{"edge":"right","start":0.0,"end":1.0}"#).unwrap();
        assert_eq!(
            parse_layout(&v).unwrap(),
            Layout {
                edge: Edge::Right,
                center: 0.5,
                output: None
            }
        );
        // A monitor that is gone is kept, so the label can say so.
        let v: Value =
            serde_json::from_str(r#"{"edge":"right","start":0.0,"end":1.0,"output":"DP-9"}"#)
                .unwrap();
        assert_eq!(parse_layout(&v).unwrap().output.as_deref(), Some("DP-9"));
        assert_eq!(parse_layout(&json!({"edge": "sideways"})), None);

        // What is written: the side, the span, the monitor and its rect for the portal.
        let l = Layout {
            edge: Edge::Left,
            center: 0.5,
            output: Some("eDP-1".into()),
        };
        let v = layout_json(&l, 0.21, 0.79, Some((250, 1440, 1920, 1080)));
        assert_eq!(v["edge"], "left");
        assert_eq!(v["start"], 0.21);
        assert_eq!(v["output"], "eDP-1");
        assert_eq!(v["rect"], json!([250, 1440, 1920, 1080]));
    }

    #[test]
    fn an_unknown_monitor_falls_back_to_the_primary() {
        let screens = laptop_below();
        let g = geo(&screens, &right_of(0), false);
        let gone = Layout {
            edge: Edge::Bottom,
            center: 0.5,
            output: Some("DP-9".into()),
        };
        let (px, py) = g.phone_at(&gone);
        let (hx, hy, hw, hh) = g.screens[0].rect;
        assert!(
            (px + g.phone.0 / 2.0 - (hx + hw / 2.0)).abs() < 2.0,
            "centred under HDMI-1"
        );
        assert!((py - (hy + hh + GAP)).abs() < 0.01);
    }
}
