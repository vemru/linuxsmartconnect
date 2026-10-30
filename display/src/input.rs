//! Phone input (data-channel packets) → this PC. Contract used by `session`:
//!
//! ```ignore
//! /// The captured screen, and how input reaches it on this desktop (from `Capture::input_target`).
//! pub struct Target { pub rect: (i32, i32, u32, u32), pub output: String, pub route: Route }
//! pub struct Injector { … }
//! impl Injector {
//!     pub fn open(target: &Target) -> anyhow::Result<Self>;
//!     /// The captured area moved/resized (cmd 70 resize, rotation); same output and route.
//!     pub fn set_target(&mut self, target: &Target);
//!     /// Called with each binary data-channel message from the phone.
//!     pub fn handle_packet(&mut self, bytes: &[u8]);
//! }
//! ```
//! `Injector` is `Send` (move it into the data-channel callback thread, behind a `Mutex` if shared).
//!
//! Wire format (spec B.4, checked against the APK: `z68`, `j68`, `i68`, `x29`, `py1`). All LE,
//! header `u32 type @0` (0 touch, 1 keyboard, 2 mouse, 3 pen), `u32 0 @4`:
//! * TOUCH (128 B): `i32 x @8, y @12` (phone view px), `i32 resX @16, resY @20`, `u16 flags @24`
//!   (1 down, 3 move, 5 up/cancel; only ACTION_DOWN/UP/MOVE/CANCEL are sent, never POINTER_DOWN/UP),
//!   `u16 pointerId @26`, `i32 timeMs @28`, `i32 count @32`, then count (≤7) × `{i32 x, i32 y, u16 flags, u16 id}` @36.
//! * MOUSE (48 B): `f32 dx @8, dy @12` (relative), `i32 resX @16, resY @20`, `i32 flags @24`
//!   (1 move, 2/4 L down/up, 8/16 R, 32/64 M, 2048 wheel), `f32 wheel @28` (Android scroll axis:
//!   +1 = up; phone touchpad sends ±1), `i64 timeMs @32`.
//! * KEYBOARD (32 B): `i32 androidKeyCode @8`, `i32 flags @16` (0 down, 2 up, 32768 = toggle
//!   (CapsLock, sent once with no down/up)).
//! * PEN (56 B): touch-like x/y/res, `u16 flags @24` (1 down, 2 move, 4 up, 32 cancel, 8 hover, 16 hover exit),
//!   `f32 pressure @28, i32 tilt @32, i32 orientation @36, i64 time @40`.
//!
//! Injection, per desktop (`Route`):
//! * `Wayland`: `zwlr_virtual_pointer_v1` + `zwp_virtual_keyboard_v1` (niri, Hyprland, sway, …),
//!   in `wlr.rs`.
//! * `Mutter`: the RemoteDesktop session linked to GNOME's screen cast (`mutter.rs`); positions
//!   are in stream pixels, so no output geometry is needed.
//! * `Kwin`: KDE's `org_kde_kwin_fake_input` (`kwin.rs`), in global logical coordinates.
//!
//! Touch gestures: 1 finger tap = left click, 1 finger move (> threshold) = left drag,
//! hold ≥ 600 ms without moving = right click, ≥ 2 fingers = scroll (finger source).

mod kwin;
mod mutter;
mod wlr;

pub use mutter::RemoteInput;

/// What to tell the user when KWin does not offer its screencast or fake-input protocol to us.
pub fn kwin_trust_hint() -> &'static str {
    kwin::TRUST_HINT
}

use anyhow::Result;

// ---------------------------------------------------------------------------------------------
// Decoding
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TouchPoint {
    pub x: i32,
    pub y: i32,
    pub flags: u16,
    pub id: u16,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Packet {
    Touch {
        x: i32,
        y: i32,
        res_x: i32,
        res_y: i32,
        flags: u16,
        pointer_id: u16,
        time_ms: i32,
        points: Vec<TouchPoint>,
    },
    Mouse {
        dx: f32,
        dy: f32,
        res_x: i32,
        res_y: i32,
        flags: i32,
        wheel: f32,
        time_ms: i64,
    },
    Key {
        android_code: i32,
        flags: i32,
    },
    Pen {
        x: i32,
        y: i32,
        res_x: i32,
        res_y: i32,
        flags: u16,
        pressure: f32,
        time_ms: i64,
    },
}

fn u16_at(b: &[u8], o: usize) -> Option<u16> {
    b.get(o..o + 2).map(|s| u16::from_le_bytes([s[0], s[1]]))
}
fn i32_at(b: &[u8], o: usize) -> Option<i32> {
    b.get(o..o + 4)
        .map(|s| i32::from_le_bytes(s.try_into().unwrap()))
}
fn f32_at(b: &[u8], o: usize) -> Option<f32> {
    b.get(o..o + 4)
        .map(|s| f32::from_le_bytes(s.try_into().unwrap()))
}
fn i64_at(b: &[u8], o: usize) -> Option<i64> {
    b.get(o..o + 8)
        .map(|s| i64::from_le_bytes(s.try_into().unwrap()))
}

pub fn decode(b: &[u8]) -> Option<Packet> {
    match i32_at(b, 0)? {
        0 => {
            let count = i32_at(b, 32).unwrap_or(0).clamp(0, 7) as usize;
            let mut points = Vec::with_capacity(count);
            for i in 0..count {
                let o = 36 + i * 12;
                let (Some(x), Some(y), Some(flags), Some(id)) = (
                    i32_at(b, o),
                    i32_at(b, o + 4),
                    u16_at(b, o + 8),
                    u16_at(b, o + 10),
                ) else {
                    break;
                };
                points.push(TouchPoint { x, y, flags, id });
            }
            Some(Packet::Touch {
                x: i32_at(b, 8)?,
                y: i32_at(b, 12)?,
                res_x: i32_at(b, 16)?,
                res_y: i32_at(b, 20)?,
                flags: u16_at(b, 24)?,
                pointer_id: u16_at(b, 26)?,
                time_ms: i32_at(b, 28).unwrap_or(0),
                points,
            })
        }
        1 => Some(Packet::Key {
            android_code: i32_at(b, 8)?,
            flags: i32_at(b, 16)?,
        }),
        2 => Some(Packet::Mouse {
            dx: f32_at(b, 8)?,
            dy: f32_at(b, 12)?,
            res_x: i32_at(b, 16)?,
            res_y: i32_at(b, 20)?,
            flags: i32_at(b, 24)?,
            wheel: f32_at(b, 28).unwrap_or(0.0),
            time_ms: i64_at(b, 32).unwrap_or(0),
        }),
        3 => Some(Packet::Pen {
            x: i32_at(b, 8)?,
            y: i32_at(b, 12)?,
            res_x: i32_at(b, 16)?,
            res_y: i32_at(b, 20)?,
            flags: u16_at(b, 24)?,
            pressure: f32_at(b, 28).unwrap_or(0.0),
            time_ms: i64_at(b, 40).unwrap_or(0),
        }),
        _ => None,
    }
}

// ---------------------------------------------------------------------------------------------
// Android keycode → Linux evdev keycode
// ---------------------------------------------------------------------------------------------

pub fn android_to_evdev(code: i32) -> Option<u32> {
    // A..Z (Android 29..54) in alphabetical order → evdev positions.
    const LETTERS: [u32; 26] = [
        30, 48, 46, 32, 18, 33, 34, 35, 23, 36, 37, 38, 50, 49, 24, 25, 16, 19, 31, 20, 22, 47, 17,
        45, 21, 44,
    ];
    Some(match code {
        7 => 11,                         // 0
        8..=16 => (code - 8 + 2) as u32, // 1..9
        29..=54 => LETTERS[(code - 29) as usize],
        4 => 1,                                // BACK → Esc
        19 => 103,                             // DPAD_UP
        20 => 108,                             // DPAD_DOWN
        21 => 105,                             // DPAD_LEFT
        22 => 106,                             // DPAD_RIGHT
        23 => 28,                              // DPAD_CENTER → Enter
        24 => 115,                             // VOLUME_UP
        25 => 114,                             // VOLUME_DOWN
        55 => 51,                              // COMMA
        56 => 52,                              // PERIOD
        57 => 56,                              // ALT_LEFT
        58 => 100,                             // ALT_RIGHT
        59 => 42,                              // SHIFT_LEFT
        60 => 54,                              // SHIFT_RIGHT
        61 => 15,                              // TAB
        62 => 57,                              // SPACE
        66 => 28,                              // ENTER
        67 => 14,                              // DEL → Backspace
        68 => 41,                              // GRAVE
        69 => 12,                              // MINUS
        70 => 13,                              // EQUALS
        71 => 26,                              // LEFT_BRACKET
        72 => 27,                              // RIGHT_BRACKET
        73 => 43,                              // BACKSLASH
        74 => 39,                              // SEMICOLON
        75 => 40,                              // APOSTROPHE
        76 => 53,                              // SLASH
        82 => 127,                             // MENU → Compose/Menu
        85 => 164,                             // MEDIA_PLAY_PAUSE
        86 => 166,                             // MEDIA_STOP
        87 => 163,                             // MEDIA_NEXT
        88 => 165,                             // MEDIA_PREVIOUS
        92 => 104,                             // PAGE_UP
        93 => 109,                             // PAGE_DOWN
        111 => 1,                              // ESCAPE
        112 => 111,                            // FORWARD_DEL
        113 => 29,                             // CTRL_LEFT
        114 => 97,                             // CTRL_RIGHT
        115 => 58,                             // CAPS_LOCK
        116 => 70,                             // SCROLL_LOCK
        117 => 125,                            // META_LEFT
        118 => 126,                            // META_RIGHT
        120 => 99,                             // SYSRQ
        121 => 119,                            // BREAK → Pause
        122 => 102,                            // MOVE_HOME
        123 => 107,                            // MOVE_END
        124 => 110,                            // INSERT
        131..=140 => (code - 131 + 59) as u32, // F1..F10
        141 => 87,                             // F11
        142 => 88,                             // F12
        143 => 69,                             // NUM_LOCK
        144 => 82,                             // NUMPAD_0
        145 => 79,
        146 => 80,
        147 => 81,
        148 => 75,
        149 => 76,
        150 => 77,
        151 => 71,
        152 => 72,
        153 => 73,  // NUMPAD_9
        154 => 98,  // NUMPAD_DIVIDE
        155 => 55,  // NUMPAD_MULTIPLY
        156 => 74,  // NUMPAD_SUBTRACT
        157 => 78,  // NUMPAD_ADD
        158 => 83,  // NUMPAD_DOT
        159 => 121, // NUMPAD_COMMA
        160 => 96,  // NUMPAD_ENTER
        161 => 117, // NUMPAD_EQUALS
        164 => 113, // VOLUME_MUTE
        _ => return None,
    })
}

// xkb real-modifier masks (Shift, Lock, Control, Mod1, …, Mod4) as used by every stock keymap.
fn modifier_mask(evdev: u32) -> u32 {
    match evdev {
        42 | 54 => 1,    // Shift
        29 | 97 => 4,    // Control
        56 | 100 => 8,   // Mod1 (Alt)
        125 | 126 => 64, // Mod4 (Super)
        _ => 0,
    }
}

// ---------------------------------------------------------------------------------------------
// Packet → abstract actions (pure, unit-tested)
// ---------------------------------------------------------------------------------------------

pub const BTN_LEFT: u32 = 0x110;
pub const BTN_RIGHT: u32 = 0x111;
pub const BTN_MIDDLE: u32 = 0x112;

/// Relative mouse gain on top of the phone's ×0.5 / ×0.55 scaling.
const MOUSE_GAIN: f64 = 1.0;
/// Phone-px a single finger must travel before a touch turns into a drag.
const DRAG_THRESHOLD: f64 = 12.0;
/// Hold time (ms) after which a stationary single-finger tap becomes a right click.
const LONG_PRESS_MS: i64 = 600;

#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    /// Absolute position in compositor-global logical px.
    MoveAbs(f64, f64),
    MoveRel(f64, f64),
    Button(u32, bool),
    /// Wheel: discrete steps, +1 = scroll down (wl_pointer convention).
    Wheel(i32),
    /// Finger scroll: continuous logical-px value (+ = down), and `None` to stop.
    FingerScroll(Option<(f64, f64)>),
    Key(u32, bool),
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum TouchMode {
    Idle,
    Pending { sx: f64, sy: f64, t0: i64 },
    Drag,
    Scroll { cx: f64, cy: f64 },
}

pub struct Translator {
    target: (i32, i32, u32, u32),
    touch: TouchMode,
    pen_down: bool,
    mods: u32,
    locked: u32,
}

impl Translator {
    pub fn new(target: (i32, i32, u32, u32)) -> Self {
        Self {
            target,
            touch: TouchMode::Idle,
            pen_down: false,
            mods: 0,
            locked: 0,
        }
    }

    fn map(&self, x: i32, y: i32, rx: i32, ry: i32) -> (f64, f64) {
        let (tx, ty, tw, th) = self.target;
        let fx = if rx > 0 {
            (x as f64 / rx as f64).clamp(0.0, 1.0)
        } else {
            0.5
        };
        let fy = if ry > 0 {
            (y as f64 / ry as f64).clamp(0.0, 1.0)
        } else {
            0.5
        };
        // Keep strictly inside the rect (the right/bottom edge belongs to the neighbour).
        (
            tx as f64 + (fx * tw as f64).min(tw as f64 - 1.0),
            ty as f64 + (fy * th as f64).min(th as f64 - 1.0),
        )
    }

    /// Scale from phone view px to logical px (for scroll distances).
    fn scale(&self, rx: i32) -> f64 {
        if rx > 0 {
            self.target.2 as f64 / rx as f64
        } else {
            1.0
        }
    }

    /// Returns the current modifier state (depressed, locked) after a key packet.
    pub fn modifiers(&self) -> (u32, u32) {
        (self.mods, self.locked)
    }

    pub fn translate(&mut self, p: &Packet) -> Vec<Action> {
        let mut out = Vec::new();
        match *p {
            Packet::Touch {
                x,
                y,
                res_x,
                res_y,
                flags,
                time_ms,
                ref points,
                ..
            } => {
                let n = points.len().max(1);
                let t = time_ms as i64;
                let (px, py) = self.map(x, y, res_x, res_y);
                let centroid = || {
                    if points.is_empty() {
                        (x as f64, y as f64)
                    } else {
                        let k = points.len() as f64;
                        (
                            points.iter().map(|p| p.x as f64).sum::<f64>() / k,
                            points.iter().map(|p| p.y as f64).sum::<f64>() / k,
                        )
                    }
                };
                match flags {
                    1 => {
                        // DOWN (always the first finger).
                        if self.touch == TouchMode::Drag {
                            out.push(Action::Button(BTN_LEFT, false));
                        }
                        out.push(Action::MoveAbs(px, py));
                        self.touch = if n >= 2 {
                            let (cx, cy) = centroid();
                            TouchMode::Scroll { cx, cy }
                        } else {
                            TouchMode::Pending {
                                sx: x as f64,
                                sy: y as f64,
                                t0: t,
                            }
                        };
                    }
                    3 => {
                        if n >= 2 {
                            let (cx, cy) = centroid();
                            match self.touch {
                                TouchMode::Scroll { cx: lx, cy: ly } => {
                                    let s = self.scale(res_x);
                                    let (dx, dy) = ((lx - cx) * s, (ly - cy) * s); // natural: fingers up → content down
                                    if dx != 0.0 || dy != 0.0 {
                                        out.push(Action::FingerScroll(Some((dx, dy))));
                                    }
                                }
                                TouchMode::Drag => out.push(Action::Button(BTN_LEFT, false)),
                                _ => {}
                            }
                            self.touch = TouchMode::Scroll { cx, cy };
                        } else {
                            match self.touch {
                                TouchMode::Pending { sx, sy, .. } => {
                                    let d =
                                        ((x as f64 - sx).powi(2) + (y as f64 - sy).powi(2)).sqrt();
                                    if d > DRAG_THRESHOLD {
                                        out.push(Action::Button(BTN_LEFT, true));
                                        out.push(Action::MoveAbs(px, py));
                                        self.touch = TouchMode::Drag;
                                    }
                                }
                                TouchMode::Drag | TouchMode::Idle => {
                                    out.push(Action::MoveAbs(px, py))
                                }
                                TouchMode::Scroll { .. } => {} // a finger lifted mid-scroll: stay scrolling
                            }
                        }
                    }
                    5 => {
                        match self.touch {
                            TouchMode::Pending { t0, .. } => {
                                let btn = if t - t0 >= LONG_PRESS_MS {
                                    BTN_RIGHT
                                } else {
                                    BTN_LEFT
                                };
                                out.push(Action::MoveAbs(px, py));
                                out.push(Action::Button(btn, true));
                                out.push(Action::Button(btn, false));
                            }
                            TouchMode::Drag => {
                                out.push(Action::MoveAbs(px, py));
                                out.push(Action::Button(BTN_LEFT, false));
                            }
                            TouchMode::Scroll { .. } => out.push(Action::FingerScroll(None)),
                            TouchMode::Idle => {}
                        }
                        self.touch = TouchMode::Idle;
                    }
                    _ => {}
                }
            }
            Packet::Pen {
                x,
                y,
                res_x,
                res_y,
                flags,
                ..
            } => {
                let (px, py) = self.map(x, y, res_x, res_y);
                match flags {
                    1 => {
                        out.push(Action::MoveAbs(px, py));
                        if !self.pen_down {
                            out.push(Action::Button(BTN_LEFT, true));
                        }
                        self.pen_down = true;
                    }
                    2 | 8 => out.push(Action::MoveAbs(px, py)),
                    4 | 32 => {
                        out.push(Action::MoveAbs(px, py));
                        if self.pen_down {
                            out.push(Action::Button(BTN_LEFT, false));
                        }
                        self.pen_down = false;
                    }
                    _ => {}
                }
            }
            Packet::Mouse {
                dx,
                dy,
                flags,
                wheel,
                ..
            } => {
                if dx != 0.0 || dy != 0.0 {
                    out.push(Action::MoveRel(
                        dx as f64 * MOUSE_GAIN,
                        dy as f64 * MOUSE_GAIN,
                    ));
                }
                for (bit, btn, down) in [
                    (2, BTN_LEFT, true),
                    (4, BTN_LEFT, false),
                    (8, BTN_RIGHT, true),
                    (16, BTN_RIGHT, false),
                    (32, BTN_MIDDLE, true),
                    (64, BTN_MIDDLE, false),
                ] {
                    if flags & bit != 0 {
                        out.push(Action::Button(btn, down));
                    }
                }
                if flags & 2048 != 0 && wheel != 0.0 {
                    // Android: +1 = wheel up; wl_pointer: + = down.
                    let steps = (-wheel).round() as i32;
                    let steps = if steps == 0 {
                        -(wheel.signum() as i32)
                    } else {
                        steps
                    };
                    out.push(Action::Wheel(steps));
                }
            }
            Packet::Key {
                android_code,
                flags,
            } => {
                let Some(code) = android_to_evdev(android_code) else {
                    return out;
                };
                let toggle = |s: &mut Self, out: &mut Vec<Action>| {
                    out.push(Action::Key(code, true));
                    out.push(Action::Key(code, false));
                    if code == 58 {
                        s.locked ^= 2;
                    }
                };
                if flags & 32768 != 0 {
                    toggle(self, &mut out);
                } else if code == 58 {
                    // The phone only sends CapsLock as a toggle; ignore any plain down/up.
                } else {
                    let down = flags & 2 == 0;
                    let m = modifier_mask(code);
                    if down {
                        self.mods |= m
                    } else {
                        self.mods &= !m
                    }
                    out.push(Action::Key(code, down));
                }
            }
        }
        out
    }
}

// ---------------------------------------------------------------------------------------------
// Injection
// ---------------------------------------------------------------------------------------------

/// How phone input reaches this desktop.
#[derive(Clone)]
pub enum Route {
    /// wlroots virtual pointer + virtual keyboard (niri, Hyprland, sway, other wlroots compositors).
    Wayland,
    /// GNOME: Mutter's RemoteDesktop session linked to the screen cast.
    Mutter(RemoteInput),
    /// KDE Plasma: `org_kde_kwin_fake_input`.
    Kwin,
}

/// The captured screen and how input reaches it.
#[derive(Clone)]
pub struct Target {
    /// Captured area in compositor logical px.
    pub rect: (i32, i32, u32, u32),
    /// Connector name of the captured output ("" when unknown).
    pub output: String,
    pub route: Route,
}

impl Target {
    /// The rectangle the translator maps the phone into: stream pixels for Mutter (its absolute
    /// motion is relative to the stream), global logical px otherwise.
    fn space(&self) -> (i32, i32, u32, u32) {
        match &self.route {
            Route::Mutter(r) => (0, 0, r.size.0, r.size.1),
            Route::Wayland | Route::Kwin => self.rect,
        }
    }
}

/// Where translated actions go. Coordinates are in `Target::space()`.
trait Sink: Send {
    fn describe(&self) -> String;
    fn set_target(&mut self, _rect: (i32, i32, u32, u32)) {}
    /// Before a packet: pick up compositor events without blocking.
    fn begin(&mut self) {}
    /// After a packet.
    fn flush(&mut self) {}
    fn move_abs(&mut self, x: f64, y: f64);
    fn move_rel(&mut self, dx: f64, dy: f64);
    fn button(&mut self, button: u32, down: bool);
    /// Discrete wheel steps, +1 = down.
    fn wheel(&mut self, steps: i32);
    /// Finger scroll in the translator's px (+ = down); `None` ends it.
    fn finger_scroll(&mut self, delta: Option<(f64, f64)>);
    /// `mods` = (depressed, locked) after this key; `modifier` = the key changes them. Returns
    /// whether the key was sent (false: no keyboard on this route).
    fn key(&mut self, code: u32, down: bool, mods: (u32, u32), modifier: bool) -> bool;
    /// Last call before the sink is dropped (buttons and keys are already released).
    fn finish(&mut self, _locked: u32) {}
}

pub struct Injector {
    tr: Translator,
    sink: Box<dyn Sink>,
    held_keys: Vec<u32>,
    held_buttons: Vec<u32>,
    scrolling: bool,
}

impl Injector {
    pub fn open(target: &Target) -> Result<Self> {
        let sink: Box<dyn Sink> = match &target.route {
            Route::Wayland => {
                let output = Some(target.output.as_str()).filter(|o| !o.is_empty());
                Box::new(wlr::WlrSink::new(target.rect, output, true)?)
            }
            Route::Mutter(r) => Box::new(mutter::MutterSink::new(r.clone())),
            Route::Kwin => Box::new(kwin::KwinSink::new()?),
        };
        Ok(Self::with_sink(target.space(), sink))
    }

    fn with_sink(space: (i32, i32, u32, u32), sink: Box<dyn Sink>) -> Self {
        let mut me = Self {
            tr: Translator::new(space),
            sink,
            held_keys: Vec::new(),
            held_buttons: Vec::new(),
            scrolling: false,
        };
        eprintln!("[input] target {space:?}, {}", me.sink.describe());
        me.sink.flush();
        me
    }

    /// The captured area moved or was resized (same output and route).
    pub fn set_target(&mut self, target: &Target) {
        if let Route::Mutter(r) = &target.route {
            self.sink = Box::new(mutter::MutterSink::new(r.clone()));
        }
        self.tr.target = target.space();
        self.sink.set_target(target.rect);
    }

    /// Called with each binary data-channel message from the phone.
    pub fn handle_packet(&mut self, bytes: &[u8]) {
        let Some(p) = decode(bytes) else {
            eprintln!("[input] undecodable packet ({} B)", bytes.len());
            return;
        };
        self.sink.begin();
        for a in self.tr.translate(&p) {
            self.apply(a);
        }
        self.sink.flush();
    }

    /// Absolute move to (x, y) in the target space. (Used by the smoke test; no buttons involved.)
    #[cfg(test)]
    pub fn move_abs(&mut self, x: f64, y: f64) {
        self.sink.begin();
        self.apply(Action::MoveAbs(x, y));
        self.sink.flush();
    }

    /// Relative move by (dx, dy) logical px. (Used by the smoke test; no buttons involved.)
    #[cfg(test)]
    pub fn move_rel(&mut self, dx: f64, dy: f64) {
        self.apply(Action::MoveRel(dx, dy));
        self.sink.flush();
    }

    fn apply(&mut self, a: Action) {
        match a {
            Action::MoveAbs(x, y) => self.sink.move_abs(x, y),
            Action::MoveRel(dx, dy) => self.sink.move_rel(dx, dy),
            Action::Button(b, down) => {
                self.sink.button(b, down);
                if down {
                    self.held_buttons.push(b);
                } else {
                    self.held_buttons.retain(|&x| x != b);
                }
            }
            Action::Wheel(steps) => self.sink.wheel(steps),
            Action::FingerScroll(Some(d)) => {
                self.scrolling = true;
                self.sink.finger_scroll(Some(d));
            }
            Action::FingerScroll(None) => {
                if self.scrolling {
                    self.sink.finger_scroll(None);
                }
                self.scrolling = false;
            }
            Action::Key(code, down) => {
                let modifier = modifier_mask(code) != 0 || code == 58;
                if self.sink.key(code, down, self.tr.modifiers(), modifier) {
                    if down {
                        self.held_keys.push(code);
                    } else {
                        self.held_keys.retain(|&k| k != code);
                    }
                }
            }
        }
    }
}

impl Drop for Injector {
    fn drop(&mut self) {
        // Never leave a button or key stuck when the session dies.
        for b in std::mem::take(&mut self.held_buttons) {
            self.sink.button(b, false);
        }
        for k in std::mem::take(&mut self.held_keys) {
            self.sink.key(k, false, (0, self.tr.locked), false);
        }
        self.sink.finish(self.tr.locked);
    }
}

// ---------------------------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn touch(x: i32, y: i32, flags: u16, t: i32, pts: &[(i32, i32)]) -> Vec<u8> {
        let mut b = vec![0u8; 128];
        b[0..4].copy_from_slice(&0i32.to_le_bytes());
        b[8..12].copy_from_slice(&x.to_le_bytes());
        b[12..16].copy_from_slice(&y.to_le_bytes());
        b[16..20].copy_from_slice(&1000i32.to_le_bytes());
        b[20..24].copy_from_slice(&500i32.to_le_bytes());
        b[24..26].copy_from_slice(&flags.to_le_bytes());
        b[28..32].copy_from_slice(&t.to_le_bytes());
        b[32..36].copy_from_slice(&(pts.len() as i32).to_le_bytes());
        for (i, (px, py)) in pts.iter().enumerate() {
            let o = 36 + i * 12;
            b[o..o + 4].copy_from_slice(&px.to_le_bytes());
            b[o + 4..o + 8].copy_from_slice(&py.to_le_bytes());
            b[o + 8..o + 10].copy_from_slice(&flags.to_le_bytes());
            b[o + 10..o + 12].copy_from_slice(&(i as u16).to_le_bytes());
        }
        b
    }

    fn mouse(dx: f32, dy: f32, flags: i32, wheel: f32) -> Vec<u8> {
        let mut b = vec![0u8; 48];
        b[0..4].copy_from_slice(&2i32.to_le_bytes());
        b[8..12].copy_from_slice(&dx.to_le_bytes());
        b[12..16].copy_from_slice(&dy.to_le_bytes());
        b[16..20].copy_from_slice(&1000i32.to_le_bytes());
        b[20..24].copy_from_slice(&500i32.to_le_bytes());
        b[24..28].copy_from_slice(&flags.to_le_bytes());
        b[28..32].copy_from_slice(&wheel.to_le_bytes());
        b[32..40].copy_from_slice(&1234i64.to_le_bytes());
        b
    }

    fn key(code: i32, flags: i32) -> Vec<u8> {
        let mut b = vec![0u8; 32];
        b[0..4].copy_from_slice(&1i32.to_le_bytes());
        b[8..12].copy_from_slice(&code.to_le_bytes());
        b[16..20].copy_from_slice(&flags.to_le_bytes());
        b
    }

    const T: (i32, i32, u32, u32) = (1920, 0, 2000, 1000);

    fn run(tr: &mut Translator, b: &[u8]) -> Vec<Action> {
        tr.translate(&decode(b).unwrap())
    }

    #[test]
    fn decode_touch() {
        let p = decode(&touch(500, 250, 1, 77, &[(500, 250)])).unwrap();
        assert_eq!(
            p,
            Packet::Touch {
                x: 500,
                y: 250,
                res_x: 1000,
                res_y: 500,
                flags: 1,
                pointer_id: 0,
                time_ms: 77,
                points: vec![TouchPoint {
                    x: 500,
                    y: 250,
                    flags: 1,
                    id: 0
                }]
            }
        );
    }

    #[test]
    fn decode_mouse_key_pen() {
        assert_eq!(
            decode(&mouse(1.5, -2.0, 1, 0.0)).unwrap(),
            Packet::Mouse {
                dx: 1.5,
                dy: -2.0,
                res_x: 1000,
                res_y: 500,
                flags: 1,
                wheel: 0.0,
                time_ms: 1234
            }
        );
        assert_eq!(
            decode(&key(113, 0)).unwrap(),
            Packet::Key {
                android_code: 113,
                flags: 0
            }
        );
        let mut pen = vec![0u8; 56];
        pen[0] = 3;
        pen[8..12].copy_from_slice(&10i32.to_le_bytes());
        pen[16..20].copy_from_slice(&100i32.to_le_bytes());
        pen[20..24].copy_from_slice(&100i32.to_le_bytes());
        pen[24..26].copy_from_slice(&2u16.to_le_bytes());
        pen[28..32].copy_from_slice(&0.5f32.to_le_bytes());
        assert!(
            matches!(decode(&pen).unwrap(), Packet::Pen { x: 10, flags: 2, pressure, .. } if pressure == 0.5)
        );
        assert_eq!(decode(&[9, 0, 0, 0]), None);
        assert_eq!(decode(&[0, 0]), None);
    }

    #[test]
    fn tap_is_left_click_mapped_into_target() {
        let mut tr = Translator::new(T);
        assert_eq!(
            run(&mut tr, &touch(500, 250, 1, 0, &[(500, 250)])),
            vec![Action::MoveAbs(2920.0, 500.0)]
        );
        assert_eq!(run(&mut tr, &touch(502, 251, 3, 30, &[(502, 251)])), vec![]);
        assert_eq!(
            run(&mut tr, &touch(502, 251, 5, 80, &[(502, 251)])),
            vec![
                Action::MoveAbs(2924.0, 502.0),
                Action::Button(BTN_LEFT, true),
                Action::Button(BTN_LEFT, false)
            ]
        );
    }

    #[test]
    fn long_press_is_right_click() {
        let mut tr = Translator::new(T);
        run(&mut tr, &touch(100, 100, 1, 0, &[(100, 100)]));
        let a = run(&mut tr, &touch(100, 100, 5, 700, &[(100, 100)]));
        assert_eq!(a[1], Action::Button(BTN_RIGHT, true));
    }

    #[test]
    fn drag() {
        let mut tr = Translator::new(T);
        run(&mut tr, &touch(100, 100, 1, 0, &[(100, 100)]));
        let a = run(&mut tr, &touch(150, 100, 3, 20, &[(150, 100)]));
        assert_eq!(
            a,
            vec![
                Action::Button(BTN_LEFT, true),
                Action::MoveAbs(2220.0, 200.0)
            ]
        );
        assert_eq!(
            run(&mut tr, &touch(200, 100, 3, 40, &[(200, 100)])),
            vec![Action::MoveAbs(2320.0, 200.0)]
        );
        assert_eq!(
            run(&mut tr, &touch(200, 100, 5, 60, &[(200, 100)])),
            vec![
                Action::MoveAbs(2320.0, 200.0),
                Action::Button(BTN_LEFT, false)
            ]
        );
    }

    #[test]
    fn two_finger_scroll() {
        let mut tr = Translator::new(T);
        run(&mut tr, &touch(100, 300, 1, 0, &[(100, 300)]));
        // Second finger appears on a MOVE (POINTER_DOWN is never sent).
        assert_eq!(
            run(&mut tr, &touch(100, 300, 3, 10, &[(100, 300), (200, 300)])),
            vec![]
        );
        // Fingers move up by 10 phone px → content scrolls down (+20 logical px, scale 2).
        assert_eq!(
            run(&mut tr, &touch(100, 290, 3, 20, &[(100, 290), (200, 290)])),
            vec![Action::FingerScroll(Some((0.0, 20.0)))]
        );
        assert_eq!(
            run(&mut tr, &touch(100, 290, 5, 30, &[(100, 290)])),
            vec![Action::FingerScroll(None)]
        );
    }

    #[test]
    fn mouse_buttons_wheel() {
        let mut tr = Translator::new(T);
        assert_eq!(
            run(&mut tr, &mouse(3.0, -1.0, 1, 0.0)),
            vec![Action::MoveRel(3.0, -1.0)]
        );
        assert_eq!(
            run(&mut tr, &mouse(0.0, 0.0, 2, 0.0)),
            vec![Action::Button(BTN_LEFT, true)]
        );
        assert_eq!(
            run(&mut tr, &mouse(0.0, 0.0, 16, 0.0)),
            vec![Action::Button(BTN_RIGHT, false)]
        );
        assert_eq!(
            run(&mut tr, &mouse(0.0, 0.0, 2048, 1.0)),
            vec![Action::Wheel(-1)]
        );
        assert_eq!(
            run(&mut tr, &mouse(0.0, 0.0, 2048, -0.3)),
            vec![Action::Wheel(1)]
        );
    }

    #[test]
    fn keys_ctrl_c_and_capslock() {
        let mut tr = Translator::new(T);
        assert_eq!(run(&mut tr, &key(113, 0)), vec![Action::Key(29, true)]);
        assert_eq!(tr.modifiers(), (4, 0));
        assert_eq!(run(&mut tr, &key(31, 0)), vec![Action::Key(46, true)]); // C
        assert_eq!(run(&mut tr, &key(31, 2)), vec![Action::Key(46, false)]);
        assert_eq!(run(&mut tr, &key(113, 2)), vec![Action::Key(29, false)]);
        assert_eq!(tr.modifiers(), (0, 0));
        assert_eq!(
            run(&mut tr, &key(115, 32768)),
            vec![Action::Key(58, true), Action::Key(58, false)]
        );
        assert_eq!(tr.modifiers(), (0, 2));
        assert_eq!(run(&mut tr, &key(9999, 0)), vec![]);
    }

    #[test]
    fn keymap_table() {
        assert_eq!(android_to_evdev(29), Some(30)); // A
        assert_eq!(android_to_evdev(54), Some(44)); // Z
        assert_eq!(android_to_evdev(50), Some(47)); // V
        assert_eq!(android_to_evdev(7), Some(11)); // 0
        assert_eq!(android_to_evdev(16), Some(10)); // 9
        assert_eq!(android_to_evdev(131), Some(59)); // F1
        assert_eq!(android_to_evdev(142), Some(88)); // F12
        assert_eq!(android_to_evdev(67), Some(14)); // Backspace
    }

    #[test]
    fn extend_output_mapping() {
        // eDP-1 at 0,0 1920x1080 + vkms Virtual-1 at 1920,0 1024x768 (extend target).
        assert_eq!(
            wlr::layout_bbox([(0, 0, 1920, 1080), (1920, 0, 1024, 768)].into_iter()),
            Some((0, 0, 2944, 1080))
        );
        let mut tr = Translator::new((1920, 0, 1024, 768));
        // Phone view 2048x1536: centre → centre of Virtual-1 in global coords.
        let mut b = touch(1024, 768, 1, 0, &[(1024, 768)]);
        b[16..20].copy_from_slice(&2048i32.to_le_bytes());
        b[20..24].copy_from_slice(&1536i32.to_le_bytes());
        assert_eq!(run(&mut tr, &b), vec![Action::MoveAbs(2432.0, 384.0)]);
        // Bottom-right edge stays inside the output.
        let mut b = touch(2048, 1536, 3, 0, &[(2048, 1536)]);
        b[16..20].copy_from_slice(&2048i32.to_le_bytes());
        b[20..24].copy_from_slice(&1536i32.to_le_bytes());
        tr.touch = TouchMode::Drag;
        assert_eq!(run(&mut tr, &b), vec![Action::MoveAbs(2943.0, 767.0)]);
    }

    #[test]
    fn injector_is_send() {
        fn assert_send<T: Send>() {}
        assert_send::<Injector>();
    }

    /// Live, any desktop: the phone's touch lands where it should on the extended screen. Only a
    /// touch DOWN is sent, which moves the pointer without clicking. Hyprland: checked against
    /// `hyprctl cursorpos`; everywhere: frames (cursor drawn in) are saved to
    /// `$TMPDIR/smartconnect-input-frames/` to look at; the cursor tip should be at 29.5 % / 41 %.
    /// `cargo test --release -p smartconnect-display input_smoke -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn input_smoke_extend() {
        use gst::prelude::*;
        use std::time::Duration;
        let cap = crate::capture::start(crate::capture::Mode::Extend {
            width: 2712,
            height: 1220,
            output: None,
            scale: Some(2.0),
        })
        .unwrap();
        let dir = std::env::temp_dir().join("smartconnect-input-frames");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        gst::init().unwrap();
        let desc = format!(
            "{} ! videoconvert ! pngenc ! multifilesink max-files=3 location={}/f%05d.png",
            cap.source_description(),
            dir.display()
        );
        let pipeline = gst::parse::launch(&desc).unwrap();
        cap.bind(&pipeline);
        pipeline.set_state(gst::State::Playing).unwrap();
        // GNOME: the monitor appears and is placed. INPUT_SMOKE_WAIT=secs leaves time to open a
        // pointer probe window on it.
        let wait = std::env::var("INPUT_SMOKE_WAIT")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(2);
        std::thread::sleep(Duration::from_secs(wait));
        let target = cap.input_target();
        let mut inj = Injector::open(&target).unwrap();
        // 295/1000, 205/500 of the phone view.
        inj.handle_packet(&touch(295, 205, 1, 0, &[(295, 205)]));
        std::thread::sleep(Duration::from_millis(1500));
        let (x, y, w, h) = cap.geometry();
        let want = (x as f64 + 0.295 * w as f64, y as f64 + 0.41 * h as f64);
        eprintln!(
            "{} {:?}: expect the pointer at {want:?} (frames in {})",
            cap.output_name(),
            cap.geometry(),
            dir.display()
        );
        if cap.desktop() == crate::desktop::Desktop::Hyprland {
            let out = std::process::Command::new("hyprctl")
                .arg("cursorpos")
                .output()
                .unwrap();
            let pos: Vec<f64> = String::from_utf8_lossy(&out.stdout)
                .split(',')
                .filter_map(|v| v.trim().parse().ok())
                .collect();
            eprintln!("hyprctl cursorpos: {pos:?}");
            assert!(
                (pos[0] - want.0).abs() <= 2.0 && (pos[1] - want.1).abs() <= 2.0,
                "pointer at {pos:?}, want {want:?}"
            );
        }
        drop(inj);
        pipeline.set_state(gst::State::Null).unwrap();
    }

    /// Safe live smoke test: small pointer moves on the focused output, ending where it started.
    /// No clicks, no keys. `cargo test --release smoke -- --ignored --nocapture`.
    /// With `CURSOR_LOCATOR=/path/cursor_pos.py` (grim-based sprite finder) it also verifies the
    /// absolute mapping, pinned (to the output) and unpinned (layout bounding box).
    #[test]
    #[ignore]
    fn smoke_move_pointer() {
        use std::{process::Command, thread::sleep, time::Duration};
        let out = Command::new("niri")
            .args(["msg", "--json", "focused-output"])
            .output()
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
        let name = v["name"].as_str().unwrap().to_string();
        let l = &v["logical"];
        let target = (
            l["x"].as_i64().unwrap() as i32,
            l["y"].as_i64().unwrap() as i32,
            l["width"].as_u64().unwrap() as u32,
            l["height"].as_u64().unwrap() as u32,
        );
        let locator = std::env::var("CURSOR_LOCATOR").ok();
        // Global position of the cursor sprite's top-left (≈ hotspot + constant offset).
        let locate = || -> Option<(i32, i32)> {
            let o = Command::new("python3")
                .args(["-I", locator.as_deref()?, &name])
                .output()
                .ok()?;
            let t = String::from_utf8_lossy(&o.stdout);
            let n: Vec<i32> = t
                .split_whitespace()
                .filter_map(|w| w.parse().ok())
                .collect();
            (n.len() == 5).then(|| (n[0] + target.0, n[1] + target.1))
        };
        let pause = || sleep(Duration::from_millis(250));
        for pin in [true, false] {
            let mut inj = if pin {
                Injector::open(&Target {
                    rect: target,
                    output: name.clone(),
                    route: Route::Wayland,
                })
                .unwrap()
            } else {
                Injector::with_sink(
                    target,
                    Box::new(wlr::WlrSink::new(target, None, false).unwrap()),
                )
            };
            eprintln!(
                "--- pinned={pin} output={name} target={target:?}: {}",
                inj.sink.describe()
            );
            inj.move_rel(12.0, 0.0);
            pause();
            let p1 = locate();
            inj.move_rel(-12.0, 0.0);
            pause();
            let p0 = locate();
            eprintln!("relative: after +12 {p1:?}, back {p0:?}");
            let (Some(p0), Some(p1)) = (p0, p1) else {
                continue;
            };
            if p1 != (p0.0 + 12, p0.1) {
                eprintln!(
                    "locator not trustworthy here (cursor at an edge?); skipping absolute check"
                );
                continue;
            }
            // Absolute: go +15,+10 from the sprite position, derive the sprite/hotspot offset, return exactly.
            inj.move_abs((p0.0 + 15) as f64, (p0.1 + 10) as f64);
            pause();
            let c = locate().expect("cursor after absolute move");
            let e = (c.0 - p0.0 - 15, c.1 - p0.1 - 10);
            inj.move_abs((p0.0 - e.0) as f64, (p0.1 - e.1) as f64);
            pause();
            let back = locate();
            eprintln!(
                "absolute: moved sprite to {c:?} (hotspot offset {e:?}), back at {back:?} (start {p0:?})"
            );
            // e = sprite top-left minus hotspot: (0,0)-ish for an arrow, (-5,-10) for an I-beam.
            assert!(
                e.0.abs() <= 32 && e.1.abs() <= 32,
                "absolute mapping off by {e:?}"
            );
            assert_eq!(back, Some(p0), "did not return to start");
            assert!(inj.held_buttons.is_empty() && inj.held_keys.is_empty());
        }
    }
}
