//! KDE Plasma: input through `org_kde_kwin_fake_input` (KWin has no virtual-pointer protocol).
//!
//! KWin only offers this interface to programs it trusts: a `.desktop` file whose `Exec` is the
//! absolute path of this binary and whose `X-KDE-Wayland-Interfaces` lists it
//! (`packaging/smartconnect-display.desktop`, installed by `packaging/install.sh`). Absolute motion
//! is in global logical coordinates; buttons and keys are evdev codes; KWin keeps the modifiers.

use anyhow::{Context, Result};
use wayland_client::{
    Connection, Dispatch, EventQueue, QueueHandle,
    globals::{GlobalListContents, registry_queue_init},
    protocol::wl_registry::WlRegistry,
};
use wayland_protocols_plasma::fake_input::client::org_kde_kwin_fake_input::OrgKdeKwinFakeInput;

use super::Sink;

/// What to tell the user when KWin does not offer an interface to us.
pub const TRUST_HINT: &str = "KDE Plasma only offers screen casting and input injection to programs listed in a \
     .desktop file with X-KDE-Wayland-Interfaces: run packaging/install.sh (it installs \
     smartconnect-display.desktop with this binary's absolute path), then log out and back in";

struct State;

pub struct KwinSink {
    conn: Connection,
    _queue: EventQueue<State>,
    fake: OrgKdeKwinFakeInput,
}

impl KwinSink {
    pub fn new() -> Result<Self> {
        let conn = Connection::connect_to_env().context("connect to Wayland")?;
        let (globals, queue) = registry_queue_init::<State>(&conn).context("registry")?;
        let qh = queue.handle();
        let fake: OrgKdeKwinFakeInput = globals
            .bind(&qh, 1..=5, ())
            .with_context(|| format!("KWin did not offer org_kde_kwin_fake_input. {TRUST_HINT}"))?;
        fake.authenticate("Smart Connect".into(), "Touch, mouse and keyboard from the phone showing this screen".into());
        conn.flush()?;
        Ok(Self { conn, _queue: queue, fake })
    }
}

impl Sink for KwinSink {
    fn describe(&self) -> String {
        format!("KWin fake input v{}", wayland_client::Proxy::version(&self.fake))
    }

    fn flush(&mut self) {
        if let Err(e) = self.conn.flush() {
            eprintln!("[input] wayland flush: {e}");
        }
    }

    fn move_abs(&mut self, x: f64, y: f64) {
        self.fake.pointer_motion_absolute(x, y);
    }

    fn move_rel(&mut self, dx: f64, dy: f64) {
        self.fake.pointer_motion(dx, dy);
    }

    fn button(&mut self, button: u32, down: bool) {
        self.fake.button(button, down as u32);
    }

    fn wheel(&mut self, steps: i32) {
        // Axis 0 = vertical, in wl_pointer units (15 per notch, + = down).
        self.fake.axis(0, steps as f64 * 15.0);
    }

    fn finger_scroll(&mut self, delta: Option<(f64, f64)>) {
        // No axis-stop request in this protocol: the end of a scroll sends nothing.
        if let Some((dx, dy)) = delta {
            if dy != 0.0 {
                self.fake.axis(0, dy);
            }
            if dx != 0.0 {
                self.fake.axis(1, dx);
            }
        }
    }

    fn key(&mut self, code: u32, down: bool, _mods: (u32, u32), _modifier: bool) -> bool {
        if wayland_client::Proxy::version(&self.fake) < 4 {
            return false;
        }
        self.fake.keyboard_key(code, down as u32);
        true
    }

    fn finish(&mut self, _locked: u32) {
        if wayland_client::Proxy::version(&self.fake) >= 5 {
            self.fake.destroy();
        }
        let _ = self.conn.flush();
    }
}

impl Dispatch<WlRegistry, GlobalListContents> for State {
    fn event(_: &mut Self, _: &WlRegistry, _: wayland_client::protocol::wl_registry::Event, _: &GlobalListContents, _: &Connection, _: &QueueHandle<Self>) {}
}

impl Dispatch<OrgKdeKwinFakeInput, ()> for State {
    fn event(_: &mut Self, _: &OrgKdeKwinFakeInput, _: <OrgKdeKwinFakeInput as wayland_client::Proxy>::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}
