//! GNOME: input through Mutter's RemoteDesktop session (`org.gnome.Mutter.RemoteDesktop`), the one
//! the screen cast is linked to (see `capture/mutter.rs`). GNOME has no virtual-pointer protocol.
//!
//! Absolute motion is relative to the cast stream, so the monitor's position in the layout does not
//! matter here. Mutter reads it differently per stream (measured on GNOME 51 with a pointer probe
//! window): a monitor stream (`RecordMonitor`, Duplicate) takes stream pixels and divides them by
//! the monitor's scale; a virtual stream (`RecordVirtual`, Extend) takes logical px. The translator
//! always maps into stream pixels, and the sink divides by the virtual monitor's current scale.
//! Keys are evdev codes; Mutter keeps the modifier state.
//! Calls are sent without waiting for replies, in order, on the cast's D-Bus connection.

use super::Sink;

/// `NotifyPointerAxis` flags (Mutter `MetaRemoteDesktopNotifyAxisFlags`).
const AXIS_FINISH: u32 = 1 << 0;
const AXIS_SOURCE_FINGER: u32 = 1 << 2;

/// Handle on a started RemoteDesktop session and the stream it drives.
#[derive(Clone)]
pub struct RemoteInput {
    /// `org.gnome.Mutter.RemoteDesktop.Session` proxy.
    pub session: zbus::blocking::Proxy<'static>,
    /// Object path of the linked ScreenCast stream.
    pub stream: String,
    /// Stream size in pixels (the space the translator maps the phone into).
    pub size: (u32, u32),
    /// A virtual stream: absolute positions are logical px, not stream pixels.
    pub virtual_stream: bool,
    /// The recorded monitor's current scale (stream px per logical px). Read on every event: GNOME
    /// rescales a new virtual monitor only after the stream has started.
    pub scale: std::sync::Arc<dyn Fn() -> f64 + Send + Sync>,
}

impl RemoteInput {
    fn scale(&self) -> f64 {
        (self.scale)().max(0.1)
    }
}

pub struct MutterSink {
    r: RemoteInput,
}

impl MutterSink {
    pub fn new(r: RemoteInput) -> Self {
        Self { r }
    }

    fn call<B>(&self, method: &str, body: &B)
    where
        B: serde::ser::Serialize + zbus::zvariant::DynamicType,
    {
        if let Err(e) = self.r.session.call_noreply(method, body) {
            eprintln!("[input] RemoteDesktop.{method}: {e}");
        }
    }
}

impl Sink for MutterSink {
    fn describe(&self) -> String {
        format!(
            "GNOME RemoteDesktop on {} ({}x{} px)",
            self.r.stream, self.r.size.0, self.r.size.1
        )
    }

    fn move_abs(&mut self, x: f64, y: f64) {
        let (x, y) = if self.r.virtual_stream {
            (x / self.r.scale(), y / self.r.scale())
        } else {
            (x, y)
        };
        self.call(
            "NotifyPointerMotionAbsolute",
            &(self.r.stream.as_str(), x, y),
        );
    }

    fn move_rel(&mut self, dx: f64, dy: f64) {
        self.call("NotifyPointerMotionRelative", &(dx, dy));
    }

    fn button(&mut self, button: u32, down: bool) {
        self.call("NotifyPointerButton", &(button as i32, down));
    }

    fn wheel(&mut self, steps: i32) {
        // Axis 0 = vertical; positive steps scroll down, like wl_pointer.
        self.call("NotifyPointerAxisDiscrete", &(0u32, steps));
    }

    fn finger_scroll(&mut self, delta: Option<(f64, f64)>) {
        let s = self.r.scale();
        match delta {
            Some((dx, dy)) => self.call("NotifyPointerAxis", &(dx / s, dy / s, AXIS_SOURCE_FINGER)),
            None => self.call(
                "NotifyPointerAxis",
                &(0.0f64, 0.0f64, AXIS_FINISH | AXIS_SOURCE_FINGER),
            ),
        }
    }

    fn key(&mut self, code: u32, down: bool, _mods: (u32, u32), _modifier: bool) -> bool {
        self.call("NotifyKeyboardKeycode", &(code, down));
        true
    }
}
