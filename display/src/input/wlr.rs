//! Injection through Wayland `zwlr_virtual_pointer_v1` + `zwp_virtual_keyboard_v1`: niri,
//! Hyprland, sway and most other wlroots-style compositors (not GNOME, not KDE).
//!
//! The keyboard re-uses the seat's current keymap (so the user's layout is left alone); falls back
//! to `xkbcli compile-keymap --layout us`.

use std::os::fd::{AsFd, OwnedFd};
use std::time::Instant;

use anyhow::{Context, Result, anyhow};
use wayland_client::{
    Connection, Dispatch, EventQueue, Proxy, QueueHandle, WEnum,
    globals::{GlobalListContents, registry_queue_init},
    protocol::{
        wl_keyboard::{self, WlKeyboard},
        wl_output::{self, WlOutput},
        wl_pointer::{Axis, AxisSource, ButtonState},
        wl_registry::WlRegistry,
        wl_seat::WlSeat,
    },
};
use wayland_protocols::xdg::xdg_output::zv1::client::{
    zxdg_output_manager_v1::ZxdgOutputManagerV1,
    zxdg_output_v1::{self, ZxdgOutputV1},
};
use wayland_protocols_misc::zwp_virtual_keyboard_v1::client::{
    zwp_virtual_keyboard_manager_v1::ZwpVirtualKeyboardManagerV1,
    zwp_virtual_keyboard_v1::ZwpVirtualKeyboardV1,
};
use wayland_protocols_wlr::virtual_pointer::v1::client::{
    zwlr_virtual_pointer_manager_v1::ZwlrVirtualPointerManagerV1,
    zwlr_virtual_pointer_v1::ZwlrVirtualPointerV1,
};

use super::Sink;

#[derive(Default)]
struct State {
    outputs: Vec<OutputInfo>,
    keymap: Option<(OwnedFd, u32)>,
}

#[derive(Clone, Debug)]
struct OutputInfo {
    wl: WlOutput,
    /// Connector name (wl_output v4 `name`, or xdg-output `name`), e.g. "eDP-1", "Virtual-1".
    name: Option<String>,
    /// Logical geometry from xdg-output (w == 0 until known).
    x: i32,
    y: i32,
    w: i32,
    h: i32,
}

impl OutputInfo {
    fn rect(&self) -> Option<(i32, i32, u32, u32)> {
        (self.w > 0 && self.h > 0).then_some((self.x, self.y, self.w as u32, self.h as u32))
    }
}

pub struct WlrSink {
    conn: Connection,
    queue: EventQueue<State>,
    state: State,
    pointer: ZwlrVirtualPointerV1,
    keyboard: Option<ZwpVirtualKeyboardV1>,
    /// Index into `state.outputs` when the virtual pointer is bound to one output
    /// (`create_virtual_pointer_with_output`); absolute motion is then relative to that output.
    pinned: Option<usize>,
    target: (i32, i32, u32, u32),
    epoch: Instant,
}

impl WlrSink {
    /// `target` = the captured screen area in compositor logical px. Pins the virtual pointer to
    /// the output named `output` (else to the output whose logical rect equals `target`) when
    /// `allow_pin`; unpinned, absolute motion spans the bounding box of all outputs.
    pub fn new(
        target: (i32, i32, u32, u32),
        output: Option<&str>,
        allow_pin: bool,
    ) -> Result<Self> {
        let conn = Connection::connect_to_env().context("connect to Wayland")?;
        let (globals, mut queue) = registry_queue_init::<State>(&conn).context("registry")?;
        let qh = queue.handle();
        let mut state = State::default();

        let seat: WlSeat = globals.bind(&qh, 1..=7, ()).context("wl_seat")?;
        let vp_mgr: ZwlrVirtualPointerManagerV1 = globals
            .bind(&qh, 1..=2, ())
            .context("compositor lacks zwlr_virtual_pointer_manager_v1")?;
        let vk_mgr: Option<ZwpVirtualKeyboardManagerV1> = globals.bind(&qh, 1..=1, ()).ok();
        let xo_mgr: Option<ZxdgOutputManagerV1> = globals.bind(&qh, 1..=3, ()).ok();

        // Enumerate outputs: connector names (wl_output v4) + logical geometry (xdg-output).
        for g in globals.contents().clone_list() {
            if g.interface == "wl_output" {
                let idx = state.outputs.len();
                let wl: WlOutput = globals.registry().bind(g.name, g.version.min(4), &qh, idx);
                if let Some(m) = &xo_mgr {
                    m.get_xdg_output(&wl, &qh, idx);
                }
                state.outputs.push(OutputInfo {
                    wl,
                    name: None,
                    x: 0,
                    y: 0,
                    w: 0,
                    h: 0,
                });
            }
        }
        // Grab the seat keymap for the virtual keyboard.
        let kb = vk_mgr.as_ref().map(|_| seat.get_keyboard(&qh, ()));
        queue.roundtrip(&mut state)?;
        queue.roundtrip(&mut state)?;
        if let Some(kb) = kb
            && kb.version() >= 3
        {
            kb.release();
        }

        let pinned = if allow_pin && vp_mgr.version() >= 2 {
            output
                .and_then(|n| {
                    state
                        .outputs
                        .iter()
                        .position(|o| o.name.as_deref() == Some(n))
                })
                .or_else(|| state.outputs.iter().position(|o| o.rect() == Some(target)))
        } else {
            None
        };
        if let (Some(n), None) = (output, pinned) {
            eprintln!(
                "[input] output {n:?} not found (or no virtual-pointer v2); mapping over the whole layout"
            );
        }
        let pointer = match pinned {
            Some(i) => vp_mgr.create_virtual_pointer_with_output(
                Some(&seat),
                Some(&state.outputs[i].wl),
                &qh,
                (),
            ),
            None => vp_mgr.create_virtual_pointer(Some(&seat), &qh, ()),
        };

        let keyboard = match vk_mgr {
            Some(m) => {
                let (fd, size) = match state.keymap.take() {
                    Some(k) => k,
                    None => us_keymap_fd().context("no seat keymap and xkbcli fallback failed")?,
                };
                let vk = m.create_virtual_keyboard(&seat, &qh, ());
                vk.keymap(1 /* XKB_V1 */, fd.as_fd(), size);
                Some(vk)
            }
            None => {
                eprintln!(
                    "[input] compositor lacks zwp_virtual_keyboard_manager_v1: keyboard disabled"
                );
                None
            }
        };
        conn.flush()?;
        Ok(Self {
            conn,
            queue,
            state,
            pointer,
            keyboard,
            pinned,
            target,
            epoch: Instant::now(),
        })
    }

    /// Absolute-motion space (origin x, origin y, w, h) in global logical px: the pinned output's
    /// current geometry, else the bounding box of all outputs (what niri maps unpinned motion to).
    pub fn space(&self) -> (i32, i32, u32, u32) {
        if let Some(i) = self.pinned {
            return self.state.outputs[i].rect().unwrap_or(self.target);
        }
        layout_bbox(self.state.outputs.iter().filter_map(OutputInfo::rect)).unwrap_or(self.target)
    }

    fn time(&self) -> u32 {
        self.epoch.elapsed().as_millis() as u32
    }
}

impl Sink for WlrSink {
    fn describe(&self) -> String {
        format!(
            "virtual pointer pinned to {:?}, motion space {:?}, keyboard {}",
            self.pinned.and_then(|i| self.state.outputs[i].name.clone()),
            self.space(),
            self.keyboard.is_some()
        )
    }

    fn set_target(&mut self, target: (i32, i32, u32, u32)) {
        self.target = target;
    }

    /// Pick up output geometry changes without blocking.
    fn begin(&mut self) {
        let _ = self.queue.dispatch_pending(&mut self.state);
        if let Some(guard) = self.queue.prepare_read() {
            let _ = guard.read(); // non-blocking; WouldBlock when nothing is queued
        }
        let _ = self.queue.dispatch_pending(&mut self.state);
    }

    fn flush(&mut self) {
        if let Err(e) = self.conn.flush() {
            eprintln!("[input] wayland flush: {e}");
        }
    }

    fn move_abs(&mut self, x: f64, y: f64) {
        let t = self.time();
        let (sx, sy, sw, sh) = self.space();
        let rx = (x - sx as f64).clamp(0.0, sw.saturating_sub(1) as f64);
        let ry = (y - sy as f64).clamp(0.0, sh.saturating_sub(1) as f64);
        // Sub-pixel precision: extents ×16.
        self.pointer.motion_absolute(
            t,
            (rx * 16.0).round() as u32,
            (ry * 16.0).round() as u32,
            sw * 16,
            sh * 16,
        );
        self.pointer.frame();
    }

    fn move_rel(&mut self, dx: f64, dy: f64) {
        self.pointer.motion(self.time(), dx, dy);
        self.pointer.frame();
    }

    fn button(&mut self, b: u32, down: bool) {
        self.pointer.button(
            self.time(),
            b,
            if down {
                ButtonState::Pressed
            } else {
                ButtonState::Released
            },
        );
        self.pointer.frame();
    }

    fn wheel(&mut self, steps: i32) {
        self.pointer.axis_source(AxisSource::Wheel);
        self.pointer.axis_discrete(
            self.time(),
            Axis::VerticalScroll,
            steps as f64 * 15.0,
            steps,
        );
        self.pointer.frame();
    }

    fn finger_scroll(&mut self, delta: Option<(f64, f64)>) {
        let t = self.time();
        self.pointer.axis_source(AxisSource::Finger);
        match delta {
            Some((dx, dy)) => {
                if dy != 0.0 {
                    self.pointer.axis(t, Axis::VerticalScroll, dy);
                }
                if dx != 0.0 {
                    self.pointer.axis(t, Axis::HorizontalScroll, dx);
                }
            }
            None => {
                self.pointer.axis_stop(t, Axis::VerticalScroll);
                self.pointer.axis_stop(t, Axis::HorizontalScroll);
            }
        }
        self.pointer.frame();
    }

    fn key(
        &mut self,
        code: u32,
        down: bool,
        (depressed, locked): (u32, u32),
        modifier: bool,
    ) -> bool {
        let t = self.time();
        let Some(vk) = &self.keyboard else {
            return false;
        };
        vk.key(t, code, down as u32);
        // A virtual keyboard has to announce the modifier state itself.
        if modifier {
            vk.modifiers(depressed, 0, locked, 0);
        }
        true
    }

    fn finish(&mut self, locked: u32) {
        if let Some(vk) = self.keyboard.take() {
            vk.modifiers(0, 0, locked, 0);
            vk.destroy();
        }
        self.pointer.destroy();
        let _ = self.conn.flush();
    }
}

/// Bounding box of output rects (niri's `global_bounding_rectangle`, used for unpinned absolute motion).
pub fn layout_bbox(
    rects: impl Iterator<Item = (i32, i32, u32, u32)>,
) -> Option<(i32, i32, u32, u32)> {
    let (x0, y0, x1, y1) =
        rects.fold(None, |acc: Option<(i32, i32, i32, i32)>, (x, y, w, h)| {
            let (x1, y1) = (x + w as i32, y + h as i32);
            Some(match acc {
                None => (x, y, x1, y1),
                Some((a, b, c, d)) => (a.min(x), b.min(y), c.max(x1), d.max(y1)),
            })
        })?;
    Some((x0, y0, (x1 - x0) as u32, (y1 - y0) as u32))
}

fn us_keymap_fd() -> Result<(OwnedFd, u32)> {
    let out = std::process::Command::new("xkbcli")
        .args(["compile-keymap", "--layout", "us"])
        .output()?;
    if !out.status.success() || out.stdout.is_empty() {
        return Err(anyhow!("xkbcli compile-keymap failed"));
    }
    let mut data = out.stdout;
    data.push(0);
    let dir = std::env::var_os("XDG_RUNTIME_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let path = dir.join(format!("smartconnect-keymap-{}", std::process::id()));
    std::fs::write(&path, &data)?;
    let f = std::fs::File::open(&path)?;
    let _ = std::fs::remove_file(&path);
    Ok((f.into(), data.len() as u32))
}

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

impl Dispatch<WlKeyboard, ()> for State {
    fn event(
        state: &mut Self,
        _: &WlKeyboard,
        ev: wl_keyboard::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_keyboard::Event::Keymap {
            format: WEnum::Value(wl_keyboard::KeymapFormat::XkbV1),
            fd,
            size,
        } = ev
        {
            state.keymap = Some((fd, size));
        }
    }
}

impl Dispatch<ZxdgOutputV1, usize> for State {
    fn event(
        state: &mut Self,
        _: &ZxdgOutputV1,
        ev: zxdg_output_v1::Event,
        idx: &usize,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let Some(o) = state.outputs.get_mut(*idx) else {
            return;
        };
        match ev {
            zxdg_output_v1::Event::LogicalPosition { x, y } => {
                o.x = x;
                o.y = y;
            }
            zxdg_output_v1::Event::LogicalSize { width, height } => {
                o.w = width;
                o.h = height;
            }
            zxdg_output_v1::Event::Name { name } if o.name.is_none() => o.name = Some(name),
            _ => {}
        }
    }
}

impl Dispatch<WlOutput, usize> for State {
    fn event(
        state: &mut Self,
        _: &WlOutput,
        ev: wl_output::Event,
        idx: &usize,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let (wl_output::Event::Name { name }, Some(o)) = (ev, state.outputs.get_mut(*idx)) {
            o.name = Some(name);
        }
    }
}

macro_rules! ignore_events {
    ($($t:ty),*) => {$(
        impl Dispatch<$t, ()> for State {
            fn event(_: &mut Self, _: &$t, _: <$t as wayland_client::Proxy>::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
        }
    )*};
}
ignore_events!(
    WlSeat,
    ZxdgOutputManagerV1,
    ZwlrVirtualPointerManagerV1,
    ZwlrVirtualPointerV1,
    ZwpVirtualKeyboardManagerV1,
    ZwpVirtualKeyboardV1
);
