//! Backend-independent half of Cross Control: the svc 26 session with the phone, the virtual
//! cursor in phone pixels, and input forwarding over svc 16 (displayId 0). A backend (layer-shell
//! strip + pointer lock, or the InputCapture portal) feeds it edge hits, relative motion, buttons,
//! scroll and evdev keys, and carries out the [`Action`]s it returns.

use std::collections::HashSet;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::Daemon;

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Edge {
    Left,
    Right,
    Top,
    Bottom,
}

impl Edge {
    /// Component of a motion delta pointing out of the PC screen through this edge.
    pub fn outward(self, dx: f64, dy: f64) -> f64 {
        match self {
            Edge::Right => dx,
            Edge::Left => -dx,
            Edge::Bottom => dy,
            Edge::Top => -dy,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Backend {
    /// wlr-layer-shell when the compositor has it, else the portal.
    Auto,
    LayerShell,
    Portal,
}

pub struct Opts {
    pub edge: Edge,
    /// Where the phone sits along the edge: (start, end) as fractions from the top or the left.
    /// Only that part of the edge leads to the phone, and it maps onto the phone's whole side.
    pub span: (f64, f64),
    pub output: Option<String>,
    pub gain: f64,
    /// Outward overshoot (phone px) past the shared edge that returns control.
    pub exit_px: f64,
    pub in_use_cap: bool,
    /// Started for a phone svc 26 cmd 0 we never saw: request control (cmd 1) right away.
    pub accept: bool,
    pub backend: Backend,
}

/// What the backend must do after a [`Core`] call.
#[derive(Debug, PartialEq)]
pub enum Action {
    None,
    /// Grab input and call [`Core::enter`].
    Enter,
    /// Call [`Core::leave`] and give input back to the PC.
    Leave(&'static str),
}

#[derive(Debug, PartialEq)]
enum Session {
    Idle,
    Requesting { since: Instant },
    Connected,
}

const E0: i64 = 0x100;
const FLAG_NATIVE: i64 = 0x1000;
const KEYCODE_BACK: i64 = 4;

/// Linux evdev keycode → (set-1 scancode, flags). evdev codes < 89 equal set-1 scancodes; the
/// E0-prefixed keys need the 0x100 flag; media keys go as Android keycodes (flag 0x1000).
fn evdev_to_scancode(code: u32) -> Option<(i64, i64)> {
    let c = code as i64;
    Some(match code {
        1..=88 => (c, 0),
        96 => (0x1C, E0),  // KP Enter
        97 => (0x1D, E0),  // Right Ctrl
        98 => (0x35, E0),  // KP /
        99 => (0x37, E0),  // SysRq / PrintScreen
        100 => (0x38, E0), // Right Alt
        102 => (0x47, E0), // Home
        103 => (0x48, E0), // Up
        104 => (0x49, E0), // PgUp
        105 => (0x4B, E0), // Left
        106 => (0x4D, E0), // Right
        107 => (0x4F, E0), // End
        108 => (0x50, E0), // Down
        109 => (0x51, E0), // PgDn
        110 => (0x52, E0), // Insert
        111 => (0x53, E0), // Delete
        119 => (0x45, 0),  // Pause (approx.)
        125 => (0x5B, E0), // Left Super
        126 => (0x5C, E0), // Right Super
        127 => (0x5D, E0), // Menu
        // Media keys → Android keycodes.
        113 => (164, FLAG_NATIVE), // KEY_MUTE → VOLUME_MUTE
        114 => (25, FLAG_NATIVE),  // VOLUMEDOWN
        115 => (24, FLAG_NATIVE),  // VOLUMEUP
        163 => (87, FLAG_NATIVE),  // NEXTSONG → MEDIA_NEXT
        164 => (85, FLAG_NATIVE),  // PLAYPAUSE
        165 => (88, FLAG_NATIVE),  // PREVIOUSSONG
        166 => (86, FLAG_NATIVE),  // STOPCD → MEDIA_STOP
        158 => (KEYCODE_BACK, FLAG_NATIVE), // KEY_BACK
        _ => return None,
    })
}

pub struct Core {
    pub opts: Opts,
    pub daemon: Daemon,
    session: Session,
    cid: i64,
    phone: (f64, f64),
    pending_enter: bool,
    /// Where the pointer met the edge, 0..1 along it (from the top or the left).
    edge_pos: f64,
    pub captured: bool,
    vx: f64,
    vy: f64,
    overshoot: f64,
    move_dirty: bool,
    last_move: Instant,
    held_keys: HashSet<(i64, i64)>,
    held_evdev: HashSet<u32>,
    held_buttons: HashSet<i64>,
    scroll_acc: f64,
    pub cooldown_until: Instant,
}

impl Core {
    pub fn new(opts: Opts, daemon: Daemon) -> Self {
        Self {
            opts,
            daemon,
            session: Session::Idle,
            cid: (std::process::id() as i64 % 1000) * 100,
            phone: (1080.0, 2400.0),
            pending_enter: false,
            edge_pos: 0.5,
            captured: false,
            vx: 0.0,
            vy: 0.0,
            overshoot: 0.0,
            move_dirty: false,
            last_move: Instant::now(),
            held_keys: HashSet::new(),
            held_evdev: HashSet::new(),
            held_buttons: HashSet::new(),
            scroll_acc: 0.0,
            cooldown_until: Instant::now(),
        }
    }

    /// True when `along` (0..1 on the PC edge) touches the phone.
    pub fn in_span(&self, along: f64) -> bool {
        let (s, e) = self.opts.span;
        along >= s - 0.005 && along <= e + 0.005
    }

    /// True while input should stay grabbed: on the phone, or waiting for it to accept.
    pub fn wants_capture(&self) -> bool {
        self.captured || self.pending_enter
    }

    /// True while something is in flight (poll fast).
    pub fn busy(&self) -> bool {
        self.captured || self.session != Session::Idle
    }

    /// Position along the PC edge (0..1) matching the virtual cursor, for putting the PC pointer
    /// back where it left the phone.
    pub fn edge_fraction(&self) -> f64 {
        let f = match self.opts.edge {
            Edge::Left | Edge::Right => self.vy / self.phone.1,
            Edge::Top | Edge::Bottom => self.vx / self.phone.0,
        };
        let (s, e) = self.opts.span;
        (s + f.clamp(0.0, 1.0) * (e - s)).clamp(0.0, 1.0)
    }

    // ---------------------------------------------------------------- session (svc 26)

    pub fn on_daemon(&mut self, msg: Value) -> Action {
        let svc = msg.get("service").and_then(Value::as_i64).unwrap_or(-1);
        let data = msg.get("data").cloned().unwrap_or(Value::Null);
        if svc != 26 {
            return Action::None;
        }
        let cmd = data.get("cmd").and_then(Value::as_i64).unwrap_or(-1);
        let int = |k: &str| data.get(k).and_then(Value::as_i64).unwrap_or(0);
        println!("svc26 <- {data}");
        match cmd {
            0 => {
                // Phone asks to be controlled: answer with a fresh request (yields geometry).
                self.request_control();
            }
            2 => {
                if int("code") == 1 {
                    let (w, h) = (int("width"), int("height"));
                    self.phone = (w.max(1) as f64, h.max(1) as f64);
                    self.session = Session::Connected;
                    println!("cross control accepted: phone {w}x{h}, supportTouch={}", data.get("supportTouch").unwrap_or(&Value::Null));
                    if self.opts.in_use_cap {
                        // Liveness check after WS reconnect wants PC svc 25 cap["26"] & 8 (INFERRED value).
                        self.daemon.send(25, json!({ "ver": "1", "cmd": 1, "cap": { "26": 9 } }));
                    }
                    if self.pending_enter {
                        self.pending_enter = false;
                        return Action::Enter;
                    }
                } else {
                    eprintln!("cross control DENIED by phone (code {}): allow it on the phone / VDM permission?", int("code"));
                    self.session = Session::Idle;
                    self.pending_enter = false;
                    self.cooldown_until = Instant::now() + Duration::from_secs(3);
                }
            }
            5 => {
                println!("phone ended cross control");
                self.daemon.send(26, json!({ "ver": "1", "cmd": 6, "cid": int("cid") }));
                self.session = Session::Idle;
                self.pending_enter = false;
                return Action::Leave("phone disconnect");
            }
            6 => {
                self.session = Session::Idle;
                self.pending_enter = false;
                return Action::Leave("disconnect ack");
            }
            7 => {
                let (w, h) = (int("width").max(1) as f64, int("height").max(1) as f64);
                // Rescale the virtual cursor into the new geometry.
                self.vx = self.vx / self.phone.0 * w;
                self.vy = self.vy / self.phone.1 * h;
                self.phone = (w, h);
                println!("phone geometry now {w}x{h}");
            }
            _ => {}
        }
        Action::None
    }

    pub fn request_control(&mut self) {
        self.cid += 1;
        self.session = Session::Requesting { since: Instant::now() };
        println!("svc26 -> cmd 1 cid {}", self.cid);
        self.daemon.send(26, json!({ "ver": "1", "cmd": 1, "cid": self.cid, "code": 0, "trigger": 1, "controller": true }));
    }

    /// The pointer reached the PC edge at `along` (0..1 from the top or the left). Outside the
    /// phone's span nothing happens.
    pub fn edge_hit(&mut self, along: f64) -> Action {
        if self.captured || Instant::now() < self.cooldown_until || !self.in_span(along) {
            return Action::None;
        }
        let (s, e) = self.opts.span;
        self.edge_pos = ((along - s) / (e - s).max(1e-6)).clamp(0.0, 1.0);
        match self.session {
            Session::Connected => return Action::Enter,
            Session::Requesting { .. } => self.pending_enter = true,
            Session::Idle => {
                self.pending_enter = true;
                self.request_control();
            }
        }
        Action::None
    }

    // ---------------------------------------------------------------- capture

    /// Puts the virtual cursor just inside the phone's shared edge and shows the phone pointer.
    /// The backend has grabbed (or is about to grab) input.
    pub fn enter(&mut self) {
        if self.captured {
            return;
        }
        let (w, h) = self.phone;
        let p = self.edge_pos;
        (self.vx, self.vy) = match self.opts.edge {
            Edge::Right => (0.0, p * h),
            Edge::Left => (w - 1.0, p * h),
            Edge::Bottom => (p * w, 0.0),
            Edge::Top => (p * w, h - 1.0),
        };
        self.clamp();
        self.overshoot = 0.0;
        self.captured = true;
        println!("→ phone at ({:.0},{:.0})", self.vx, self.vy);
        self.daemon.send(26, json!({ "ver": "1", "cmd": 4, "show": true }));
        self.move_dirty = true;
        self.flush_move(true);
    }

    /// Hides the phone pointer and releases anything held down there. Returns false when control
    /// was already with the PC (the backend then has nothing to undo).
    pub fn leave(&mut self, why: &str) -> bool {
        if !self.captured {
            return false;
        }
        self.captured = false;
        self.flush_move(true);
        self.release_all();
        self.daemon.send(26, json!({ "ver": "1", "cmd": 4, "show": false }));
        self.cooldown_until = Instant::now() + Duration::from_millis(350);
        println!("← PC ({why})");
        true
    }

    pub fn release_all(&mut self) {
        let (x, y) = (self.vx as i64, self.vy as i64);
        for (key, flags) in std::mem::take(&mut self.held_keys) {
            self.daemon.input(3, json!({ "displayId": 0, "flags": flags, "key": key, "down": false, "metaState": 0 }));
        }
        self.held_evdev.clear();
        for b in std::mem::take(&mut self.held_buttons) {
            self.daemon.input(5, json!({ "displayId": 0, "key": b, "x": x, "y": y, "down": false }));
        }
    }

    fn clamp(&mut self) {
        self.vx = self.vx.clamp(0.0, self.phone.0 - 1.0);
        self.vy = self.vy.clamp(0.0, self.phone.1 - 1.0);
    }

    /// Relative pointer motion in PC pixels (ignored unless on the phone).
    pub fn on_relative(&mut self, dx: f64, dy: f64) -> Action {
        if !self.captured {
            return Action::None;
        }
        let (dx, dy) = (dx * self.opts.gain, dy * self.opts.gain);
        let (nx, ny) = (self.vx + dx, self.vy + dy);
        // Overshoot back toward the PC along the shared edge.
        let back = match self.opts.edge {
            Edge::Right => -nx,
            Edge::Left => nx - (self.phone.0 - 1.0),
            Edge::Bottom => -ny,
            Edge::Top => ny - (self.phone.1 - 1.0),
        };
        if back > 0.0 {
            self.overshoot += back;
        } else {
            self.overshoot = 0.0;
        }
        self.vx = nx;
        self.vy = ny;
        self.clamp();
        self.move_dirty = true;
        if self.overshoot > self.opts.exit_px && self.held_buttons.is_empty() {
            return Action::Leave("edge");
        }
        Action::None
    }

    fn flush_move(&mut self, force: bool) {
        if !self.move_dirty || (!force && self.last_move.elapsed() < Duration::from_millis(8)) {
            return;
        }
        self.move_dirty = false;
        self.last_move = Instant::now();
        self.daemon.input(4, json!({ "displayId": 0, "x": self.vx as i64, "y": self.vy as i64 }));
    }

    /// `button` is a Linux `BTN_*` code.
    pub fn on_button(&mut self, button: u32, pressed: bool) {
        if !self.captured {
            return;
        }
        self.flush_move(true);
        let (x, y) = (self.vx as i64, self.vy as i64);
        let key = match button {
            0x110 => 1, // BTN_LEFT
            0x112 => 2, // BTN_MIDDLE
            0x111 => 3, // BTN_RIGHT
            0x113 | 0x116 => {
                // BTN_SIDE / BTN_BACK → Android Back
                if pressed {
                    for down in [true, false] {
                        self.daemon.input(3, json!({ "displayId": 0, "flags": FLAG_NATIVE, "key": KEYCODE_BACK, "down": down, "metaState": 0 }));
                    }
                }
                return;
            }
            _ => return,
        };
        if pressed {
            self.held_buttons.insert(key);
        } else {
            self.held_buttons.remove(&key);
        }
        self.daemon.input(5, json!({ "displayId": 0, "key": key, "x": x, "y": y, "down": pressed }));
    }

    /// `notches` > 0 = wheel down (content scrolls down).
    pub fn on_scroll(&mut self, notches: f64) {
        if !self.captured {
            return;
        }
        self.scroll_acc += notches;
        let n = self.scroll_acc.trunc();
        self.scroll_acc -= n;
        let (x, y) = (self.vx as i64, self.vy as i64);
        for _ in 0..(n.abs() as i64) {
            self.daemon.input(5, json!({ "displayId": 0, "key": 4, "x": x, "y": y, "down": n > 0.0 }));
        }
    }

    /// `code` is a Linux evdev keycode.
    pub fn on_key(&mut self, code: u32, pressed: bool) -> Action {
        if !self.captured {
            return Action::None;
        }
        if pressed {
            self.held_evdev.insert(code);
        } else {
            self.held_evdev.remove(&code);
        }
        // Emergency escape: Ctrl+Alt+Esc.
        let h = &self.held_evdev;
        if pressed && code == 1 && (h.contains(&29) || h.contains(&97)) && (h.contains(&56) || h.contains(&100)) {
            return Action::Leave("Ctrl+Alt+Esc");
        }
        let Some((key, flags)) = evdev_to_scancode(code) else {
            println!("unmapped evdev key {code}");
            return Action::None;
        };
        if pressed {
            self.held_keys.insert((key, flags));
        } else if !self.held_keys.remove(&(key, flags)) {
            return Action::None; // never sent the press (held before capture)
        }
        self.daemon.input(3, json!({ "displayId": 0, "flags": flags, "key": key, "down": pressed, "metaState": 0 }));
        Action::None
    }

    /// Call every few ms: request timeout and coalesced pointer moves.
    pub fn tick(&mut self) {
        if let Session::Requesting { since } = self.session
            && since.elapsed() > Duration::from_secs(4)
        {
            eprintln!("no svc26 cmd 2 reply from the phone within 4 s (is \"26\" advertised in the verify reply? phone connected?)");
            self.session = Session::Idle;
            self.pending_enter = false;
            self.cooldown_until = Instant::now() + Duration::from_secs(2);
        }
        self.flush_move(false);
    }

    /// Ends the svc 26 session (after the backend has left).
    pub fn shutdown(&mut self) {
        if self.session != Session::Idle {
            println!("svc26 -> cmd 5 stop");
            self.daemon.send(26, json!({ "ver": "1", "cmd": 5, "cid": self.cid }));
            if self.opts.in_use_cap {
                self.daemon.send(25, json!({ "ver": "1", "cmd": 1, "cap": { "26": 1 } }));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scancodes() {
        assert_eq!(evdev_to_scancode(30), Some((30, 0))); // A
        assert_eq!(evdev_to_scancode(106), Some((0x4D, E0))); // Right arrow
        assert_eq!(evdev_to_scancode(115), Some((24, FLAG_NATIVE))); // Volume up
        assert_eq!(evdev_to_scancode(240), None);
    }

    #[test]
    fn outward_component() {
        assert_eq!(Edge::Right.outward(3.0, 1.0), 3.0);
        assert_eq!(Edge::Left.outward(3.0, 1.0), -3.0);
        assert_eq!(Edge::Top.outward(0.0, -2.0), 2.0);
    }
}
