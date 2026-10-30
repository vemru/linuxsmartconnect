//! wlr-layer-shell backend (niri, sway, Hyprland, KDE): a 1-px layer-shell strip on one edge of
//! an output. Pushing the pointer into it starts the phone session; entering maps a fullscreen
//! transparent overlay with exclusive keyboard focus, locks the pointer and feeds relative motion
//! to the [`Core`]. Pushing back past the shared edge (or Ctrl+Alt+Esc) returns control.

use std::os::fd::AsRawFd;
use std::sync::atomic::Ordering;
use std::time::Instant;

use anyhow::{Context, Result, bail};
use smithay_client_toolkit::{
    compositor::{CompositorHandler, CompositorState},
    delegate_compositor, delegate_layer, delegate_output, delegate_registry, delegate_seat,
    delegate_shm,
    output::{OutputHandler, OutputState},
    registry::{ProvidesRegistryState, RegistryState},
    registry_handlers,
    seat::{Capability, SeatHandler, SeatState},
    shell::{
        WaylandSurface,
        wlr_layer::{
            Anchor, KeyboardInteractivity, Layer, LayerShell, LayerShellHandler, LayerSurface,
            LayerSurfaceConfigure,
        },
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

use crate::core::{Action, Core, Edge};

/// True when the compositor at `$WAYLAND_DISPLAY` offers wlr-layer-shell.
pub fn available() -> bool {
    let Ok(conn) = Connection::connect_to_env() else {
        return false;
    };
    let Ok((globals, _queue)) = registry_queue_init::<App>(&conn) else {
        return false;
    };
    globals
        .contents()
        .with_list(|list| list.iter().any(|g| g.interface == "zwlr_layer_shell_v1"))
}

struct App {
    core: Core,
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
    saw_value120: bool,
    exit: bool,
}

impl App {
    fn create_strip(&mut self, qh: &QueueHandle<Self>) {
        let surface = self.compositor.create_surface(qh);
        let layer = self.layer_shell.create_layer_surface(
            qh,
            surface,
            Layer::Overlay,
            Some("smartconnect-crosscontrol-edge"),
            self.output.as_ref(),
        );
        let (anchor, size) = match self.core.opts.edge {
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
        let Ok((buffer, canvas)) =
            self.pool
                .create_buffer(w as i32, h as i32, w as i32 * 4, wl_shm::Format::Argb8888)
        else {
            eprintln!("shm buffer {w}x{h} failed");
            return;
        };
        canvas.fill(0);
        let layer = if which == 1 {
            self.strip.as_ref()
        } else {
            self.overlay.as_ref()
        };
        if let Some(layer) = layer {
            layer.wl_surface().damage_buffer(0, 0, w as i32, h as i32);
            let _ = buffer.attach_to(layer.wl_surface());
            layer.commit();
        }
    }

    fn act(&mut self, qh: &QueueHandle<Self>, action: Action) {
        match action {
            Action::None => {}
            Action::Enter => self.enter(qh),
            Action::Leave(why) => self.leave(why),
        }
    }

    /// Position of the pointer along the edge strip, 0..1 from the top or the left.
    fn along(&self) -> f64 {
        let (sw, sh) = (
            self.strip_size.0.max(1) as f64,
            self.strip_size.1.max(1) as f64,
        );
        match self.core.opts.edge {
            Edge::Left | Edge::Right => self.strip_pos.1 / sh,
            Edge::Top | Edge::Bottom => self.strip_pos.0 / sw,
        }
    }

    /// Pointer reached the edge strip.
    fn edge_hit(&mut self, qh: &QueueHandle<Self>) {
        let action = self.core.edge_hit(self.along());
        self.act(qh, action);
    }

    fn enter(&mut self, qh: &QueueHandle<Self>) {
        if self.core.captured {
            return;
        }
        self.core.enter();

        // Fullscreen transparent overlay with exclusive keyboard.
        let surface = self.compositor.create_surface(qh);
        let layer = self.layer_shell.create_layer_surface(
            qh,
            surface,
            Layer::Overlay,
            Some("smartconnect-crosscontrol-grab"),
            self.output.as_ref(),
        );
        layer.set_anchor(Anchor::TOP | Anchor::BOTTOM | Anchor::LEFT | Anchor::RIGHT);
        layer.set_size(0, 0);
        layer.set_exclusive_zone(-1);
        layer.set_keyboard_interactivity(KeyboardInteractivity::Exclusive);
        layer.commit();
        if let (Some(c), Some(p)) = (&self.constraints, &self.pointer) {
            self.lock =
                Some(c.lock_pointer(layer.wl_surface(), p, None, Lifetime::Persistent, qh, ()));
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
        if !self.core.leave(why) {
            return;
        }
        // Put the PC pointer just inside the shared edge at the matching position.
        if let (Some(lock), true) = (&self.lock, self.overlay_size.0 > 0) {
            let (ow, oh) = (self.overlay_size.0 as f64, self.overlay_size.1 as f64);
            let f = self.core.edge_fraction();
            let (x, y) = match self.core.opts.edge {
                Edge::Right => (ow - 4.0, f * oh),
                Edge::Left => (3.0, f * oh),
                Edge::Bottom => (f * ow, oh - 4.0),
                Edge::Top => (f * ow, 3.0),
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
    }

    fn on_relative(&mut self, qh: &QueueHandle<Self>, dx: f64, dy: f64) {
        if !self.core.captured {
            // Sitting in the strip after a return: a firm outward push re-enters.
            if self.focus == 1 {
                self.knock = (self.knock + self.core.opts.edge.outward(dx, dy)).max(0.0);
                if self.knock > 25.0 && Instant::now() >= self.core.cooldown_until {
                    self.knock = 0.0;
                    self.knock_pending = true;
                }
            }
            return;
        }
        let action = self.core.on_relative(dx, dy);
        self.act(qh, action);
    }

    fn tick(&mut self, qh: &QueueHandle<Self>) {
        if self.knock_pending {
            self.knock_pending = false;
            self.edge_hit(qh);
        }
        self.core.tick();
    }
}

// ---------------------------------------------------------------- sctk handlers

impl CompositorHandler for App {
    fn scale_factor_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: i32,
    ) {
    }
    fn transform_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: wl_output::Transform,
    ) {
    }
    fn frame(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: u32) {}
    fn surface_enter(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: &wl_output::WlOutput,
    ) {
    }
    fn surface_leave(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: &wl_output::WlOutput,
    ) {
    }
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

    fn configure(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        layer: &LayerSurface,
        cfg: LayerSurfaceConfigure,
        _: u32,
    ) {
        let (w, h) = cfg.new_size;
        if self.strip.as_ref() == Some(layer) {
            self.strip_size = (w, h);
            println!("edge strip {w}x{h} on {:?} edge", self.core.opts.edge);
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
    fn new_capability(
        &mut self,
        _: &Connection,
        qh: &QueueHandle<Self>,
        seat: wl_seat::WlSeat,
        cap: Capability,
    ) {
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
    fn remove_capability(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: wl_seat::WlSeat,
        _: Capability,
    ) {
    }
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
    fn event(
        app: &mut Self,
        ptr: &wl_pointer::WlPointer,
        ev: wl_pointer::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        use wl_pointer::Event as E;
        match ev {
            E::Enter {
                serial,
                surface,
                surface_x,
                surface_y,
            } => {
                app.pointer_serial = serial;
                if app
                    .overlay
                    .as_ref()
                    .is_some_and(|o| o.wl_surface() == &surface)
                {
                    app.focus = 2;
                    ptr.set_cursor(serial, None, 0, 0); // hide the PC cursor while on the phone
                } else if app
                    .strip
                    .as_ref()
                    .is_some_and(|s| s.wl_surface() == &surface)
                {
                    app.focus = 1;
                    app.strip_pos = (surface_x, surface_y);
                    app.knock = 0.0;
                    if Instant::now() >= app.core.cooldown_until {
                        app.edge_hit(qh);
                    }
                }
            }
            E::Leave { .. } => app.focus = 0,
            E::Motion {
                surface_x,
                surface_y,
                ..
            } => {
                if app.focus == 1 {
                    // Sliding along the edge into the phone's part of it counts as arriving.
                    let was_in = app.core.in_span(app.along());
                    app.strip_pos = (surface_x, surface_y);
                    if !was_in
                        && app.core.in_span(app.along())
                        && Instant::now() >= app.core.cooldown_until
                    {
                        app.edge_hit(qh);
                    }
                }
            }
            E::Button { button, state, .. } => {
                app.core.on_button(
                    button,
                    matches!(state, WEnum::Value(wl_pointer::ButtonState::Pressed)),
                );
            }
            E::AxisValue120 {
                axis: WEnum::Value(wl_pointer::Axis::VerticalScroll),
                value120,
            } => {
                app.saw_value120 = true;
                app.core.on_scroll(value120 as f64 / 120.0);
            }
            E::Axis {
                axis: WEnum::Value(wl_pointer::Axis::VerticalScroll),
                value,
                ..
            } if !app.saw_value120 => {
                app.core.on_scroll(value / 15.0);
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_keyboard::WlKeyboard, ()> for App {
    fn event(
        app: &mut Self,
        _: &wl_keyboard::WlKeyboard,
        ev: wl_keyboard::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        use wl_keyboard::Event as E;
        match ev {
            E::Key { key, state, .. } => {
                let action = app.core.on_key(key, matches!(state, WEnum::Value(wl_keyboard::KeyState::Pressed)));
                app.act(qh, action);
            }
            E::Leave { .. }
                // Keyboard focus lost while on the phone: don't leave keys stuck down there.
                if app.core.captured => {
                    app.core.release_all();
                }
            _ => {} // keymap fd is closed on drop
        }
    }
}

impl Dispatch<ZwpRelativePointerV1, ()> for App {
    fn event(
        app: &mut Self,
        _: &ZwpRelativePointerV1,
        ev: zwp_relative_pointer_v1::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let zwp_relative_pointer_v1::Event::RelativeMotion { dx, dy, .. } = ev {
            app.on_relative(qh, dx, dy);
        }
    }
}

impl Dispatch<ZwpLockedPointerV1, ()> for App {
    fn event(
        app: &mut Self,
        _: &ZwpLockedPointerV1,
        ev: zwp_locked_pointer_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
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
ignore_events!(
    ZwpPointerConstraintsV1,
    ZwpRelativePointerManagerV1,
    ZwpKeyboardShortcutsInhibitManagerV1,
    ZwpKeyboardShortcutsInhibitorV1
);

fn bind_opt<I>(globals: &GlobalList, qh: &QueueHandle<App>) -> Option<I>
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

pub fn run(core: Core) -> Result<()> {
    let conn = Connection::connect_to_env().context("no Wayland display")?;
    let (globals, mut queue) = registry_queue_init::<App>(&conn)?;
    let qh = queue.handle();
    let compositor = CompositorState::bind(&globals, &qh)?;
    let layer_shell = LayerShell::bind(&globals, &qh)
        .context("compositor lacks wlr-layer-shell (try --backend portal)")?;
    let shm = Shm::bind(&globals, &qh)?;
    let pool = SlotPool::new(4096, &shm)?;

    let mut app = App {
        core,
        registry_state: RegistryState::new(&globals),
        seat_state: SeatState::new(&globals, &qh),
        output_state: OutputState::new(&globals, &qh),
        constraints: bind_opt::<ZwpPointerConstraintsV1>(&globals, &qh),
        rel_mgr: bind_opt::<ZwpRelativePointerManagerV1>(&globals, &qh),
        inhibit_mgr: bind_opt::<ZwpKeyboardShortcutsInhibitManagerV1>(&globals, &qh),
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
        saw_value120: false,
        exit: false,
    };
    queue.roundtrip(&mut app)?;
    queue.roundtrip(&mut app)?;

    // Pick the output: --output NAME, else the first one.
    let outputs: Vec<_> = app.output_state.outputs().collect();
    let chosen = outputs.iter().find(|o| {
        let info = app.output_state.info(o);
        match (&app.core.opts.output, info) {
            (Some(want), Some(i)) => i.name.as_deref() == Some(want.as_str()),
            (None, _) => true,
            _ => false,
        }
    });
    if let Some(o) = chosen {
        let name = app
            .output_state
            .info(o)
            .and_then(|i| i.name)
            .unwrap_or_default();
        println!("using output {name}");
        app.output = Some(o.clone());
    } else if app.core.opts.output.is_some() {
        bail!("output {:?} not found", app.core.opts.output);
    }
    app.create_strip(&qh);
    if app.core.opts.accept {
        // The phone asked (svc 26 cmd 0) before we were running: answer it now; the edge is then armed.
        app.core.request_control();
    }
    println!(
        "ready: push the pointer into the {:?} edge; push back or Ctrl+Alt+Esc to return; Ctrl+C quits",
        app.core.opts.edge
    );

    let wl_fd = conn.backend().poll_fd().as_raw_fd();
    let d_fd = app.core.daemon.watch.as_raw_fd();
    while !app.exit && !crate::STOP.load(Ordering::SeqCst) {
        queue.dispatch_pending(&mut app)?;
        queue.flush()?;
        if let Some(guard) = queue.prepare_read() {
            let mut fds = [
                libc::pollfd {
                    fd: wl_fd,
                    events: libc::POLLIN,
                    revents: 0,
                },
                libc::pollfd {
                    fd: d_fd,
                    events: libc::POLLIN,
                    revents: 0,
                },
            ];
            let timeout = if app.core.busy() { 8 } else { 200 };
            unsafe { libc::poll(fds.as_mut_ptr(), 2, timeout) };
            if fds[0].revents & libc::POLLIN != 0 {
                let _ = guard.read();
            } else {
                drop(guard);
            }
        }
        queue.dispatch_pending(&mut app)?;
        match app.core.daemon.poll_lines() {
            Ok(msgs) => {
                for m in msgs {
                    let action = app.core.on_daemon(m);
                    app.act(&qh, action);
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
    app.leave("exit");
    app.core.shutdown();
    let _ = queue.roundtrip(&mut app);
    Ok(())
}
