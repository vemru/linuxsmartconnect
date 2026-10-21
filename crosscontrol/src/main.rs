//! smartconnect-crosscontrol: Cross Control (Motorola "Universal Control", svc 26) for Wayland.
//!
//! A 1-px layer-shell strip sits on one edge of an output. Pushing the pointer into it starts a
//! svc 26 session (cmd 1 trigger:1 → phone cmd 2 with width/height), then maps a fullscreen
//! transparent overlay with exclusive keyboard focus, locks the pointer and integrates relative
//! motion into a virtual cursor in phone pixels. Input goes over svc 16 with displayId 0.
//! Pushing back past the shared edge (or Ctrl+Alt+Esc) returns control to the PC.
//! See evidence-2026-10-20/cross-control-spec.md.

use std::io::{BufRead, BufReader, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use smithay_client_toolkit::{
    compositor::{CompositorHandler, CompositorState},
    delegate_compositor, delegate_layer, delegate_output, delegate_registry, delegate_seat, delegate_shm,
    output::{OutputHandler, OutputState},
    registry::{ProvidesRegistryState, RegistryState},
    registry_handlers,
    seat::{Capability, SeatHandler, SeatState},
    shell::{
        WaylandSurface,
        wlr_layer::{Anchor, KeyboardInteractivity, Layer, LayerShell, LayerShellHandler, LayerSurface, LayerSurfaceConfigure},
    },
    shm::{Shm, ShmHandler, slot::SlotPool},
};
use wayland_client::{
    Connection, Dispatch, QueueHandle, WEnum,
    globals::{GlobalList, registry_queue_init},
    protocol::{wl_keyboard, wl_output, wl_pointer, wl_seat, wl_shm, wl_surface},
};
use wayland_protocols::wp::{
    keyboard_shortcuts_inhibit::zv1::client::{
        zwp_keyboard_shortcuts_inhibit_manager_v1::ZwpKeyboardShortcutsInhibitManagerV1,
        zwp_keyboard_shortcuts_inhibitor_v1::ZwpKeyboardShortcutsInhibitorV1,
    },
    pointer_constraints::zv1::client::{
        zwp_locked_pointer_v1::{self, ZwpLockedPointerV1},
        zwp_pointer_constraints_v1::{Lifetime, ZwpPointerConstraintsV1},
    },
    relative_pointer::zv1::client::{
        zwp_relative_pointer_manager_v1::ZwpRelativePointerManagerV1,
        zwp_relative_pointer_v1::{self, ZwpRelativePointerV1},
    },
};

static STOP: AtomicBool = AtomicBool::new(false);

extern "C" fn on_signal(_: libc::c_int) {
    STOP.store(true, Ordering::SeqCst);
}

fn socket_path() -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from).unwrap_or_else(std::env::temp_dir).join("smartconnectd.sock")
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Edge {
    Left,
    Right,
    Top,
    Bottom,
}

struct Opts {
    edge: Edge,
    output: Option<String>,
    gain: f64,
    /// Outward overshoot (phone px) past the shared edge that returns control.
    exit_px: f64,
    in_use_cap: bool,
    /// Started for a phone svc 26 cmd 0 we never saw: request control (cmd 1) right away.
    accept: bool,
}

fn parse_args() -> Result<Opts> {
    let mut o = Opts { edge: Edge::Right, output: None, gain: 1.6, exit_px: 12.0, in_use_cap: true, accept: false };
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--edge" => {
                o.edge = match args.next().as_deref() {
                    Some("left") => Edge::Left,
                    Some("right") => Edge::Right,
                    Some("top") => Edge::Top,
                    Some("bottom") => Edge::Bottom,
                    v => bail!("--edge left|right|top|bottom (got {v:?})"),
                }
            }
            "--output" => o.output = args.next(),
            "--gain" => o.gain = args.next().context("--gain N")?.parse()?,
            "--exit-px" => o.exit_px = args.next().context("--exit-px N")?.parse()?,
            "--no-in-use-cap" => o.in_use_cap = false,
            "--accept" => o.accept = true,
            "-h" | "--help" => {
                println!(
                    "smartconnect-crosscontrol [--edge left|right|top|bottom] [--output NAME] [--gain 1.6] [--exit-px 12] [--no-in-use-cap] [--accept]\n\
                     Push the pointer into the chosen screen edge to control the phone; push back (or Ctrl+Alt+Esc) to return."
                );
                std::process::exit(0);
            }
            other => bail!("unknown argument {other}"),
        }
    }
    Ok(o)
}

/// Talks to smartconnectd: one socket for `watch`, one for `send` (replies drained by a thread).
struct Daemon {
    cmd: UnixStream,
    watch: UnixStream,
    buf: Vec<u8>,
}

impl Daemon {
    fn connect() -> Result<Self> {
        let watch = UnixStream::connect(socket_path()).context("is smartconnectd running?")?;
        let cmd = UnixStream::connect(socket_path())?;
        (&watch).write_all(b"{\"cmd\":\"watch\",\"services\":[26,16,25]}\n")?;
        watch.set_nonblocking(true)?;
        let drain = cmd.try_clone()?;
        std::thread::spawn(move || {
            for line in BufReader::new(drain).lines().map_while(Result::ok) {
                if line.contains("\"ok\":false") {
                    eprintln!("daemon: {line}");
                }
            }
        });
        Ok(Self { cmd, watch, buf: Vec::new() })
    }

    fn send(&mut self, service: i64, data: Value) {
        let line = json!({ "cmd": "send", "service": service, "data": data }).to_string() + "\n";
        if let Err(e) = self.cmd.write_all(line.as_bytes()) {
            eprintln!("daemon write failed: {e}");
        }
    }

    fn input(&mut self, cmd: i64, data: Value) {
        self.send(16, json!({ "cmd": cmd, "data": data, "messageId": 0, "instanceId": 0 }));
    }

    /// Reads whatever is available; returns complete JSON lines. `Err` = daemon gone.
    fn poll_lines(&mut self) -> Result<Vec<Value>> {
        let mut tmp = [0u8; 8192];
        loop {
            match self.watch.read(&mut tmp) {
                Ok(0) => bail!("daemon closed the watch socket"),
                Ok(n) => self.buf.extend_from_slice(&tmp[..n]),
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(e) => return Err(e.into()),
            }
        }
        let mut out = Vec::new();
        while let Some(pos) = self.buf.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = self.buf.drain(..=pos).collect();
            if let Ok(v) = serde_json::from_slice::<Value>(&line) {
                out.push(v);
            }
        }
        Ok(out)
    }
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

struct App {
    opts: Opts,
    registry_state: RegistryState,
    seat_state: SeatState,
    output_state: OutputState,
    compositor: CompositorState,
    layer_shell: LayerShell,
    shm: Shm,
    pool: SlotPool,
    constraints: Option<ZwpPointerConstraintsV1>,
    rel_mgr: Option<ZwpRelativePointerManagerV1>,
    inhibit_mgr: Option<ZwpKeyboardShortcutsInhibitManagerV1>,

    seat: Option<wl_seat::WlSeat>,
    pointer: Option<wl_pointer::WlPointer>,
    keyboard: Option<wl_keyboard::WlKeyboard>,
    rel: Option<ZwpRelativePointerV1>,

    output: Option<wl_output::WlOutput>,
    strip: Option<LayerSurface>,
    strip_size: (u32, u32),
    overlay: Option<LayerSurface>,
    overlay_size: (u32, u32),
    lock: Option<ZwpLockedPointerV1>,
    locked: bool,
    inhibitor: Option<ZwpKeyboardShortcutsInhibitorV1>,

    /// Which of our surfaces has pointer focus: 0 none, 1 strip, 2 overlay.
    focus: u8,
    pointer_serial: u32,
    /// Position along the edge where the pointer entered the strip (surface coords).
    strip_pos: (f64, f64),
    /// Outward push accumulated while sitting in the strip (re-entry without leaving).
    knock: f64,
    knock_pending: bool,
    cooldown_until: Instant,

    daemon: Daemon,
    session: Session,
    cid: i64,
    phone: (f64, f64),
    pending_enter: bool,

    captured: bool,
    vx: f64,
    vy: f64,
    overshoot: f64,
    move_dirty: bool,
    last_move: Instant,
    held_keys: std::collections::HashSet<(i64, i64)>,
    held_evdev: std::collections::HashSet<u32>,
    held_buttons: std::collections::HashSet<i64>,
    scroll_acc: f64,
    saw_value120: bool,
    exit: bool,
}

impl App {
    fn create_strip(&mut self, qh: &QueueHandle<Self>) {
        let surface = self.compositor.create_surface(qh);
        let layer = self.layer_shell.create_layer_surface(qh, surface, Layer::Overlay, Some("smartconnect-crosscontrol-edge"), self.output.as_ref());
        let (anchor, size) = match self.opts.edge {
            Edge::Right => (Anchor::RIGHT | Anchor::TOP | Anchor::BOTTOM, (1, 0)),
            Edge::Left => (Anchor::LEFT | Anchor::TOP | Anchor::BOTTOM, (1, 0)),
            Edge::Top => (Anchor::TOP | Anchor::LEFT | Anchor::RIGHT, (0, 1)),
            Edge::Bottom => (Anchor::BOTTOM | Anchor::LEFT | Anchor::RIGHT, (0, 1)),
        };
        layer.set_anchor(anchor);
        layer.set_size(size.0, size.1);
        layer.set_exclusive_zone(-1);
        layer.set_keyboard_interactivity(KeyboardInteractivity::None);
        layer.commit();
        self.strip = Some(layer);
    }

    fn attach_transparent(&mut self, which: u8, w: u32, h: u32) {
        let (w, h) = (w.max(1), h.max(1));
        let Ok((buffer, canvas)) = self.pool.create_buffer(w as i32, h as i32, w as i32 * 4, wl_shm::Format::Argb8888) else {
            eprintln!("shm buffer {w}x{h} failed");
            return;
        };
        canvas.fill(0);
        let layer = if which == 1 { self.strip.as_ref() } else { self.overlay.as_ref() };
        if let Some(layer) = layer {
            layer.wl_surface().damage_buffer(0, 0, w as i32, h as i32);
            let _ = buffer.attach_to(layer.wl_surface());
            layer.commit();
        }
    }

    // ---------------------------------------------------------------- session (svc 26)

    fn on_daemon(&mut self, qh: &QueueHandle<Self>, msg: Value) {
        let svc = msg.get("service").and_then(Value::as_i64).unwrap_or(-1);
        let data = msg.get("data").cloned().unwrap_or(Value::Null);
        if svc != 26 {
            return;
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
                        self.enter(qh);
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
                self.leave("phone disconnect");
                self.session = Session::Idle;
            }
            6 => {
                self.leave("disconnect ack");
                self.session = Session::Idle;
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
    }

    fn request_control(&mut self) {
        self.cid += 1;
        self.session = Session::Requesting { since: Instant::now() };
        println!("svc26 -> cmd 1 cid {}", self.cid);
        self.daemon.send(26, json!({ "ver": "1", "cmd": 1, "cid": self.cid, "code": 0, "trigger": 1, "controller": true }));
    }

    /// Pointer reached the edge strip.
    fn edge_hit(&mut self, qh: &QueueHandle<Self>) {
        if self.captured || Instant::now() < self.cooldown_until {
            return;
        }
        match self.session {
            Session::Connected => self.enter(qh),
            Session::Requesting { .. } => self.pending_enter = true,
            Session::Idle => {
                self.pending_enter = true;
                self.request_control();
            }
        }
    }

    // ---------------------------------------------------------------- capture

    fn enter(&mut self, qh: &QueueHandle<Self>) {
        if self.captured {
            return;
        }
        let (sw, sh) = (self.strip_size.0.max(1) as f64, self.strip_size.1.max(1) as f64);
        let (w, h) = self.phone;
        match self.opts.edge {
            Edge::Right => (self.vx, self.vy) = (0.0, self.strip_pos.1 / sh * h),
            Edge::Left => (self.vx, self.vy) = (w - 1.0, self.strip_pos.1 / sh * h),
            Edge::Bottom => (self.vx, self.vy) = (self.strip_pos.0 / sw * w, 0.0),
            Edge::Top => (self.vx, self.vy) = (self.strip_pos.0 / sw * w, h - 1.0),
        }
        self.clamp();
        self.overshoot = 0.0;
        self.captured = true;
        println!("→ phone at ({:.0},{:.0})", self.vx, self.vy);
        self.daemon.send(26, json!({ "ver": "1", "cmd": 4, "show": true }));
        self.move_dirty = true;
        self.flush_move(true);

        // Fullscreen transparent overlay with exclusive keyboard.
        let surface = self.compositor.create_surface(qh);
        let layer = self.layer_shell.create_layer_surface(qh, surface, Layer::Overlay, Some("smartconnect-crosscontrol-grab"), self.output.as_ref());
        layer.set_anchor(Anchor::TOP | Anchor::BOTTOM | Anchor::LEFT | Anchor::RIGHT);
        layer.set_size(0, 0);
        layer.set_exclusive_zone(-1);
        layer.set_keyboard_interactivity(KeyboardInteractivity::Exclusive);
        layer.commit();
        if let (Some(c), Some(p)) = (&self.constraints, &self.pointer) {
            self.lock = Some(c.lock_pointer(layer.wl_surface(), p, None, Lifetime::Persistent, qh, ()));
        } else {
            eprintln!("no pointer-constraints: relying on relative motion only");
        }
        if let (Some(m), Some(seat)) = (&self.inhibit_mgr, &self.seat) {
            self.inhibitor = Some(m.inhibit_shortcuts(layer.wl_surface(), seat, qh, ()));
        }
        self.overlay = Some(layer);
    }

    /// Returns control to the PC (idempotent).
    fn leave(&mut self, why: &str) {
        if !self.captured {
            return;
        }
        self.captured = false;
        self.flush_move(true);
        self.release_all();
        self.daemon.send(26, json!({ "ver": "1", "cmd": 4, "show": false }));
        // Put the PC pointer just inside the shared edge at the matching position.
        if let (Some(lock), true) = (&self.lock, self.overlay_size.0 > 0) {
            let (ow, oh) = (self.overlay_size.0 as f64, self.overlay_size.1 as f64);
            let (fx, fy) = (self.vx / self.phone.0, self.vy / self.phone.1);
            let (x, y) = match self.opts.edge {
                Edge::Right => (ow - 4.0, fy * oh),
                Edge::Left => (3.0, fy * oh),
                Edge::Bottom => (fx * ow, oh - 4.0),
                Edge::Top => (fx * ow, 3.0),
            };
            lock.set_cursor_position_hint(x, y);
            if let Some(o) = &self.overlay {
                o.wl_surface().commit();
            }
        }
        if let Some(l) = self.lock.take() {
            l.destroy();
        }
        if let Some(i) = self.inhibitor.take() {
            i.destroy();
        }
        self.overlay = None; // drops → layer + surface destroyed
        self.overlay_size = (0, 0);
        self.locked = false;
        self.knock = 0.0;
        self.cooldown_until = Instant::now() + Duration::from_millis(350);
        println!("← PC ({why})");
    }

    fn release_all(&mut self) {
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

    fn on_relative(&mut self, dx: f64, dy: f64) {
        if !self.captured {
            // Sitting in the strip after a return: a firm outward push re-enters.
            if self.focus == 1 {
                let out = match self.opts.edge {
                    Edge::Right => dx,
                    Edge::Left => -dx,
                    Edge::Bottom => dy,
                    Edge::Top => -dy,
                };
                self.knock = (self.knock + out).max(0.0);
                if self.knock > 25.0 && Instant::now() >= self.cooldown_until {
                    self.knock = 0.0;
                    self.knock_pending = true;
                }
            }
            return;
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
            self.leave("edge");
        }
    }

    fn flush_move(&mut self, force: bool) {
        if !self.move_dirty || (!force && self.last_move.elapsed() < Duration::from_millis(8)) {
            return;
        }
        self.move_dirty = false;
        self.last_move = Instant::now();
        self.daemon.input(4, json!({ "displayId": 0, "x": self.vx as i64, "y": self.vy as i64 }));
    }

    fn on_button(&mut self, button: u32, pressed: bool) {
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
    fn on_scroll(&mut self, amount: f64) {
        if !self.captured {
            return;
        }
        self.scroll_acc += amount;
        let n = self.scroll_acc.trunc();
        self.scroll_acc -= n;
        let (x, y) = (self.vx as i64, self.vy as i64);
        for _ in 0..(n.abs() as i64) {
            self.daemon.input(5, json!({ "displayId": 0, "key": 4, "x": x, "y": y, "down": n > 0.0 }));
        }
    }

    fn on_key(&mut self, code: u32, pressed: bool) {
        if !self.captured {
            return;
        }
        if pressed {
            self.held_evdev.insert(code);
        } else {
            self.held_evdev.remove(&code);
        }
        // Emergency escape: Ctrl+Alt+Esc.
        let h = &self.held_evdev;
        if pressed && code == 1 && (h.contains(&29) || h.contains(&97)) && (h.contains(&56) || h.contains(&100)) {
            self.leave("Ctrl+Alt+Esc");
            return;
        }
        let Some((key, flags)) = evdev_to_scancode(code) else {
            println!("unmapped evdev key {code}");
            return;
        };
        if pressed {
            self.held_keys.insert((key, flags));
        } else if !self.held_keys.remove(&(key, flags)) {
            return; // never sent the press (held before capture)
        }
        self.daemon.input(3, json!({ "displayId": 0, "flags": flags, "key": key, "down": pressed, "metaState": 0 }));
    }

    fn tick(&mut self, qh: &QueueHandle<Self>) {
        if self.knock_pending {
            self.knock_pending = false;
            self.edge_hit(qh);
        }
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

    fn shutdown(&mut self) {
        self.leave("exit");
        if self.session != Session::Idle {
            println!("svc26 -> cmd 5 stop");
            self.daemon.send(26, json!({ "ver": "1", "cmd": 5, "cid": self.cid }));
            if self.opts.in_use_cap {
                self.daemon.send(25, json!({ "ver": "1", "cmd": 1, "cap": { "26": 1 } }));
            }
        }
    }
}

// ---------------------------------------------------------------- sctk handlers

impl CompositorHandler for App {
    fn scale_factor_changed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: i32) {}
    fn transform_changed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: wl_output::Transform) {}
    fn frame(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: u32) {}
    fn surface_enter(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: &wl_output::WlOutput) {}
    fn surface_leave(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: &wl_output::WlOutput) {}
}

impl OutputHandler for App {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }
    fn new_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn output_destroyed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
}

impl LayerShellHandler for App {
    fn closed(&mut self, _: &Connection, _: &QueueHandle<Self>, layer: &LayerSurface) {
        if self.strip.as_ref() == Some(layer) {
            eprintln!("edge strip closed by compositor");
            self.exit = true;
        } else {
            self.leave("overlay closed");
        }
    }

    fn configure(&mut self, _: &Connection, _: &QueueHandle<Self>, layer: &LayerSurface, cfg: LayerSurfaceConfigure, _: u32) {
        let (w, h) = cfg.new_size;
        if self.strip.as_ref() == Some(layer) {
            self.strip_size = (w, h);
            println!("edge strip {w}x{h} on {:?} edge", self.opts.edge);
            self.attach_transparent(1, w, h);
        } else if self.overlay.as_ref() == Some(layer) {
            self.overlay_size = (w, h);
            self.attach_transparent(2, w, h);
        }
    }
}

impl SeatHandler for App {
    fn seat_state(&mut self) -> &mut SeatState {
        &mut self.seat_state
    }
    fn new_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}
    fn new_capability(&mut self, _: &Connection, qh: &QueueHandle<Self>, seat: wl_seat::WlSeat, cap: Capability) {
        self.seat.get_or_insert_with(|| seat.clone());
        if cap == Capability::Pointer && self.pointer.is_none() {
            let p = seat.get_pointer(qh, ());
            if let Some(m) = &self.rel_mgr {
                self.rel = Some(m.get_relative_pointer(&p, qh, ()));
            }
            self.pointer = Some(p);
        }
        if cap == Capability::Keyboard && self.keyboard.is_none() {
            self.keyboard = Some(seat.get_keyboard(qh, ()));
        }
    }
    fn remove_capability(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat, _: Capability) {}
    fn remove_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}
}

impl ShmHandler for App {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm
    }
}

impl ProvidesRegistryState for App {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }
    registry_handlers![OutputState, SeatState];
}

delegate_compositor!(App);
delegate_output!(App);
delegate_shm!(App);
delegate_seat!(App);
delegate_layer!(App);
delegate_registry!(App);

// ---------------------------------------------------------------- raw protocol dispatch

impl Dispatch<wl_pointer::WlPointer, ()> for App {
    fn event(app: &mut Self, ptr: &wl_pointer::WlPointer, ev: wl_pointer::Event, _: &(), _: &Connection, qh: &QueueHandle<Self>) {
        use wl_pointer::Event as E;
        match ev {
            E::Enter { serial, surface, surface_x, surface_y } => {
                app.pointer_serial = serial;
                if app.overlay.as_ref().is_some_and(|o| o.wl_surface() == &surface) {
                    app.focus = 2;
                    ptr.set_cursor(serial, None, 0, 0); // hide the PC cursor while on the phone
                } else if app.strip.as_ref().is_some_and(|s| s.wl_surface() == &surface) {
                    app.focus = 1;
                    app.strip_pos = (surface_x, surface_y);
                    app.knock = 0.0;
                    if Instant::now() >= app.cooldown_until {
                        app.edge_hit(qh);
                    }
                }
            }
            E::Leave { .. } => app.focus = 0,
            E::Motion { surface_x, surface_y, .. } => {
                if app.focus == 1 {
                    app.strip_pos = (surface_x, surface_y);
                }
            }
            E::Button { button, state, .. } => {
                app.on_button(button, matches!(state, WEnum::Value(wl_pointer::ButtonState::Pressed)));
            }
            E::AxisValue120 { axis: WEnum::Value(wl_pointer::Axis::VerticalScroll), value120 } => {
                app.saw_value120 = true;
                app.on_scroll(value120 as f64 / 120.0);
            }
            E::Axis { axis: WEnum::Value(wl_pointer::Axis::VerticalScroll), value, .. } if !app.saw_value120 => {
                app.on_scroll(value / 15.0);
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_keyboard::WlKeyboard, ()> for App {
    fn event(app: &mut Self, _: &wl_keyboard::WlKeyboard, ev: wl_keyboard::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        use wl_keyboard::Event as E;
        match ev {
            E::Key { key, state, .. } => app.on_key(key, matches!(state, WEnum::Value(wl_keyboard::KeyState::Pressed))),
            E::Leave { .. } => {
                // Keyboard focus lost while on the phone: don't leave keys stuck down there.
                if app.captured {
                    app.release_all();
                }
            }
            _ => {} // keymap fd is closed on drop
        }
    }
}

impl Dispatch<ZwpRelativePointerV1, ()> for App {
    fn event(app: &mut Self, _: &ZwpRelativePointerV1, ev: zwp_relative_pointer_v1::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        if let zwp_relative_pointer_v1::Event::RelativeMotion { dx, dy, .. } = ev {
            app.on_relative(dx, dy);
        }
    }
}

impl Dispatch<ZwpLockedPointerV1, ()> for App {
    fn event(app: &mut Self, _: &ZwpLockedPointerV1, ev: zwp_locked_pointer_v1::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        match ev {
            zwp_locked_pointer_v1::Event::Locked => {
                app.locked = true;
                println!("pointer locked");
            }
            zwp_locked_pointer_v1::Event::Unlocked => {
                app.locked = false;
                println!("pointer unlocked by compositor");
            }
            _ => {}
        }
    }
}

macro_rules! ignore_events {
    ($($t:ty),*) => {$(
        impl Dispatch<$t, ()> for App {
            fn event(_: &mut Self, _: &$t, _: <$t as wayland_client::Proxy>::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
        }
    )*};
}
ignore_events!(ZwpPointerConstraintsV1, ZwpRelativePointerManagerV1, ZwpKeyboardShortcutsInhibitManagerV1, ZwpKeyboardShortcutsInhibitorV1);

fn bind_opt<I, U>(globals: &GlobalList, qh: &QueueHandle<App>) -> Option<I>
where
    I: wayland_client::Proxy + 'static,
    App: Dispatch<I, ()>,
{
    match globals.bind::<I, App, ()>(qh, 1..=1, ()) {
        Ok(p) => Some(p),
        Err(e) => {
            eprintln!("missing {}: {e}", I::interface().name);
            None
        }
    }
}

/// One client at a time: holds an exclusive flock on `$XDG_RUNTIME_DIR/smartconnect-crosscontrol.lock`
/// (smartconnectd checks it before starting one for the phone). A newer instance takes over: it
/// SIGINTs the holder (which tells the phone to stop) and waits for the lock.
fn single_instance() -> Result<std::fs::File> {
    let path = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("smartconnect-crosscontrol.lock");
    let mut f = std::fs::OpenOptions::new().read(true).write(true).create(true).truncate(false).open(&path)?;
    let try_lock = |f: &std::fs::File| unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0;
    if !try_lock(&f) {
        let mut pid = String::new();
        let _ = f.read_to_string(&mut pid);
        if let Ok(pid) = pid.trim().parse::<libc::pid_t>() {
            println!("taking over from running instance (pid {pid})");
            unsafe { libc::kill(pid, libc::SIGINT) };
        }
        let deadline = Instant::now() + Duration::from_secs(3);
        while !try_lock(&f) {
            if Instant::now() > deadline {
                bail!("another smartconnect-crosscontrol is running and didn't stop ({})", path.display());
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
    f.set_len(0)?;
    write!(f, "{}", std::process::id())?;
    Ok(f)
}

fn main() -> Result<()> {
    let opts = parse_args()?;
    let _lock = single_instance()?;
    unsafe {
        libc::signal(libc::SIGINT, on_signal as *const () as usize);
        libc::signal(libc::SIGTERM, on_signal as *const () as usize);
    }
    let daemon = Daemon::connect()?;

    let conn = Connection::connect_to_env().context("no Wayland display")?;
    let (globals, mut queue) = registry_queue_init::<App>(&conn)?;
    let qh = queue.handle();
    let compositor = CompositorState::bind(&globals, &qh)?;
    let layer_shell = LayerShell::bind(&globals, &qh).context("compositor lacks wlr-layer-shell")?;
    let shm = Shm::bind(&globals, &qh)?;
    let pool = SlotPool::new(4096, &shm)?;

    let mut app = App {
        registry_state: RegistryState::new(&globals),
        seat_state: SeatState::new(&globals, &qh),
        output_state: OutputState::new(&globals, &qh),
        constraints: bind_opt::<ZwpPointerConstraintsV1, ()>(&globals, &qh),
        rel_mgr: bind_opt::<ZwpRelativePointerManagerV1, ()>(&globals, &qh),
        inhibit_mgr: bind_opt::<ZwpKeyboardShortcutsInhibitManagerV1, ()>(&globals, &qh),
        opts,
        compositor,
        layer_shell,
        shm,
        pool,
        seat: None,
        pointer: None,
        keyboard: None,
        rel: None,
        output: None,
        strip: None,
        strip_size: (0, 0),
        overlay: None,
        overlay_size: (0, 0),
        lock: None,
        locked: false,
        inhibitor: None,
        focus: 0,
        pointer_serial: 0,
        strip_pos: (0.0, 0.0),
        knock: 0.0,
        knock_pending: false,
        cooldown_until: Instant::now(),
        daemon,
        session: Session::Idle,
        cid: (std::process::id() as i64 % 1000) * 100,
        phone: (1080.0, 2400.0),
        pending_enter: false,
        captured: false,
        vx: 0.0,
        vy: 0.0,
        overshoot: 0.0,
        move_dirty: false,
        last_move: Instant::now(),
        held_keys: Default::default(),
        held_evdev: Default::default(),
        held_buttons: Default::default(),
        scroll_acc: 0.0,
        saw_value120: false,
        exit: false,
    };
    queue.roundtrip(&mut app)?;
    queue.roundtrip(&mut app)?;

    // Pick the output: --output NAME, else the first one.
    let outputs: Vec<_> = app.output_state.outputs().collect();
    let chosen = outputs.iter().find(|o| {
        let info = app.output_state.info(o);
        match (&app.opts.output, info) {
            (Some(want), Some(i)) => i.name.as_deref() == Some(want.as_str()),
            (None, _) => true,
            _ => false,
        }
    });
    if let Some(o) = chosen {
        let name = app.output_state.info(o).and_then(|i| i.name).unwrap_or_default();
        println!("using output {name}");
        app.output = Some(o.clone());
    } else if app.opts.output.is_some() {
        bail!("output {:?} not found", app.opts.output);
    }
    app.create_strip(&qh);
    if app.opts.accept {
        // The phone asked (svc 26 cmd 0) before we were running: answer it now; the edge is then armed.
        app.request_control();
    }
    println!("ready: push the pointer into the {:?} edge; push back or Ctrl+Alt+Esc to return; Ctrl+C quits", app.opts.edge);

    let wl_fd = conn.backend().poll_fd().as_raw_fd();
    let d_fd = app.daemon.watch.as_raw_fd();
    while !app.exit && !STOP.load(Ordering::SeqCst) {
        queue.dispatch_pending(&mut app)?;
        queue.flush()?;
        if let Some(guard) = queue.prepare_read() {
            let mut fds = [
                libc::pollfd { fd: wl_fd, events: libc::POLLIN, revents: 0 },
                libc::pollfd { fd: d_fd, events: libc::POLLIN, revents: 0 },
            ];
            let timeout = if app.captured || app.session != Session::Idle { 8 } else { 200 };
            unsafe { libc::poll(fds.as_mut_ptr(), 2, timeout) };
            if fds[0].revents & libc::POLLIN != 0 {
                let _ = guard.read();
            } else {
                drop(guard);
            }
        }
        queue.dispatch_pending(&mut app)?;
        match app.daemon.poll_lines() {
            Ok(msgs) => {
                for m in msgs {
                    app.on_daemon(&qh, m);
                }
            }
            Err(e) => {
                eprintln!("{e}");
                app.leave("daemon gone");
                break;
            }
        }
        app.tick(&qh);
    }
    println!("exiting");
    app.shutdown();
    let _ = queue.roundtrip(&mut app);
    Ok(())
}
