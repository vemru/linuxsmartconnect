//! System-tray item (freedesktop/KDE StatusNotifierItem, via the `ksni` crate).
//!
//! The tray runs on its own thread (ksni's blocking API spawns one). Its menu items and a left
//! click can't touch GTK directly from that thread, so they only drop a [`Command`] into an
//! `async_channel`; the GTK main loop drains it (see `window::Ui::listen_tray`). The icon and the
//! Connect/Disconnect menu entries are refreshed from the GTK side through [`Tray::set`].

use ksni::blocking::{Handle, TrayMethods};

/// What a tray menu entry (or a left click) asks the window to do, handled on the GTK main loop.
#[derive(Clone, Debug)]
pub enum Command {
    /// Show the window if hidden, hide it if shown (also the left-click action).
    ToggleWindow,
    Connect,
    Disconnect,
    /// Quit the app *and* stop the daemon (the only thing that does).
    Quit,
    // Webcam controls:
    WebcamToggleMirror,
    WebcamToggleFlipV,
    WebcamSetBrightness(i32),
    WebcamSetContrast(i32),
    WebcamSetQuality(String),
    WebcamSwitchCamera,
    WebcamToggleTorch,
    WebcamStop,
}

/// The coarse state the icon reflects.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    /// `smartconnectd` is not answering its control socket.
    DaemonDown,
    /// The daemon was just launched and the socket hasn't answered yet.
    Starting,
    Disconnected,
    Connected,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct WebcamTrayState {
    pub active: bool,
    pub quality: String,
    pub mirror: bool,
    pub flip_v: bool,
    pub brightness: i32,
    pub contrast: i32,
    pub fps: f64,
    pub size: String,
}

/// The actual StatusNotifierItem. Lives on the ksni thread; the GTK side only reaches it through
/// the [`Handle`] in [`Tray`].
struct Item {
    state: State,
    /// Drives the Show/Hide label.
    window_visible: bool,
    /// "Phone name · 63%" while connected, empty otherwise. Shown in the tooltip and as the first
    /// menu line, the way GSConnect's device menu reads.
    detail: String,
    webcam: Option<WebcamTrayState>,
    tx: async_channel::Sender<Command>,
}

impl Item {
    fn send(&self, cmd: Command) {
        // Unbounded channel: `try_send` never blocks (only fails if the UI is gone), which is what
        // we want from the tray thread.
        let _ = self.tx.try_send(cmd);
    }
}

impl ksni::Tray for Item {
    fn id(&self) -> String {
        "dev.smartconnect.Linux".into()
    }

    fn title(&self) -> String {
        "Smart Connect".into()
    }

    fn icon_name(&self) -> String {
        if self.webcam.as_ref().is_some_and(|w| w.active) {
            return "camera-web-symbolic".into();
        }
        match self.state {
            State::DaemonDown => "network-offline-symbolic",
            State::Starting => "content-loading-symbolic",
            State::Disconnected | State::Connected => "phone-symbolic",
        }
        .into()
    }

    /// A small check badge on the icon when the phone is connected (hosts that don't draw overlays
    /// just ignore it; the tooltip still says the state).
    fn overlay_icon_name(&self) -> String {
        match self.state {
            State::Connected => "emblem-ok-symbolic".into(),
            _ => String::new(),
        }
    }

    fn tool_tip(&self) -> ksni::ToolTip {
        if let Some(w) = self.webcam.as_ref().filter(|w| w.active) {
            let quality_lbl = match w.quality.to_ascii_lowercase().as_str() {
                "low" => "Low (1.5 Mbit/s)",
                "medium" => "Medium (4 Mbit/s)",
                "max" => "Max (15 Mbit/s)",
                _ => "High (8 Mbit/s)",
            };
            return ksni::ToolTip {
                icon_name: "camera-web-symbolic".into(),
                icon_pixmap: Vec::new(),
                title: "Smart Connect — Phone Webcam".into(),
                description: format!(
                    "Camera active: {} · {}\nDevice: {}",
                    w.size, quality_lbl, self.detail
                ),
            };
        }
        let description = match self.state {
            State::DaemonDown => "Daemon not running",
            State::Starting => "Starting…",
            State::Disconnected => "Phone disconnected",
            State::Connected if !self.detail.is_empty() => {
                return {
                    ksni::ToolTip {
                        icon_name: self.icon_name(),
                        icon_pixmap: Vec::new(),
                        title: "Smart Connect".into(),
                        // e.g. "Phone connected — Alex's Phone · 63%"
                        description: format!("Phone connected — {}", self.detail),
                    }
                };
            }
            State::Connected => "Phone connected",
        };
        ksni::ToolTip {
            icon_name: self.icon_name(),
            icon_pixmap: Vec::new(),
            title: "Smart Connect".into(),
            description: description.into(),
        }
    }

    /// Left click: toggle the window.
    fn activate(&mut self, _x: i32, _y: i32) {
        self.send(Command::ToggleWindow);
    }

    fn menu(&self) -> Vec<ksni::MenuItem<Self>> {
        use ksni::menu::*;
        let mut items: Vec<ksni::MenuItem<Self>> = Vec::new();
        // GSConnect-style device line: name and battery, when there is one.
        if matches!(self.state, State::Connected) && !self.detail.is_empty() {
            items.push(
                StandardItem {
                    label: self.detail.clone(),
                    enabled: false,
                    ..Default::default()
                }
                .into(),
            );
            items.push(MenuItem::Separator);
        }

        // --- Active Webcam Controls in System Tray ---
        if let Some(w) = self.webcam.as_ref().filter(|w| w.active) {
            let quality_lbl = match w.quality.to_ascii_lowercase().as_str() {
                "low" => "Low (1.5 Mbit/s)",
                "medium" => "Medium (4 Mbit/s)",
                "max" => "Max (15 Mbit/s)",
                _ => "High (8 Mbit/s)",
            };
            items.push(
                StandardItem {
                    label: format!("● Phone Camera Active ({})", w.size),
                    enabled: false,
                    ..Default::default()
                }
                .into(),
            );

            // 1. Bitrate / Quality SubMenu
            let qualities = [
                ("low", "Low (1.5 Mbit/s)"),
                ("medium", "Medium (4 Mbit/s)"),
                ("high", "High (8 Mbit/s)"),
                ("max", "Max (15 Mbit/s)"),
            ];
            let mut q_items: Vec<ksni::MenuItem<Self>> = Vec::new();
            for (q_id, q_name) in qualities {
                let is_cur = w.quality.eq_ignore_ascii_case(q_id);
                let q_str = q_id.to_string();
                q_items.push(
                    CheckmarkItem {
                        label: q_name.into(),
                        checked: is_cur,
                        activate: Box::new(move |this: &mut Self| {
                            this.send(Command::WebcamSetQuality(q_str.clone()));
                        }),
                        ..Default::default()
                    }
                    .into(),
                );
            }
            items.push(
                SubMenu {
                    label: format!("Bitrate: {quality_lbl}"),
                    submenu: q_items,
                    ..Default::default()
                }
                .into(),
            );

            // 2. Mirror stream (horizontal flip)
            items.push(
                CheckmarkItem {
                    label: "Mirror Stream (Flip Horizontal)".into(),
                    checked: w.mirror,
                    activate: Box::new(|this: &mut Self| this.send(Command::WebcamToggleMirror)),
                    ..Default::default()
                }
                .into(),
            );

            // 3. Flip vertical
            items.push(
                CheckmarkItem {
                    label: "Flip Vertical".into(),
                    checked: w.flip_v,
                    activate: Box::new(|this: &mut Self| this.send(Command::WebcamToggleFlipV)),
                    ..Default::default()
                }
                .into(),
            );

            // 4. Brightness SubMenu
            let b_options = [
                (-40, "-40% (Darker)"),
                (-20, "-20% (Dim)"),
                (0, "Normal (0%)"),
                (20, "+20% (Bright)"),
                (40, "+40% (Brighter)"),
            ];
            let mut b_items: Vec<ksni::MenuItem<Self>> = Vec::new();
            for (b_val, b_name) in b_options {
                b_items.push(
                    CheckmarkItem {
                        label: b_name.into(),
                        checked: w.brightness == b_val,
                        activate: Box::new(move |this: &mut Self| {
                            this.send(Command::WebcamSetBrightness(b_val));
                        }),
                        ..Default::default()
                    }
                    .into(),
                );
            }
            items.push(
                SubMenu {
                    label: format!("Brightness: {:+}%", w.brightness),
                    submenu: b_items,
                    ..Default::default()
                }
                .into(),
            );

            // 5. Contrast SubMenu
            let c_options = [
                (60, "60% (Low)"),
                (80, "80% (Soft)"),
                (100, "Normal (100%)"),
                (120, "120% (Punchy)"),
                (140, "140% (High)"),
            ];
            let mut c_items: Vec<ksni::MenuItem<Self>> = Vec::new();
            for (c_val, c_name) in c_options {
                c_items.push(
                    CheckmarkItem {
                        label: c_name.into(),
                        checked: w.contrast == c_val,
                        activate: Box::new(move |this: &mut Self| {
                            this.send(Command::WebcamSetContrast(c_val));
                        }),
                        ..Default::default()
                    }
                    .into(),
                );
            }
            items.push(
                SubMenu {
                    label: format!("Contrast: {}%", w.contrast),
                    submenu: c_items,
                    ..Default::default()
                }
                .into(),
            );

            // 6. Camera flip & torch
            items.push(
                StandardItem {
                    label: "Switch Camera (Front / Back)".into(),
                    activate: Box::new(|this: &mut Self| this.send(Command::WebcamSwitchCamera)),
                    ..Default::default()
                }
                .into(),
            );
            items.push(
                StandardItem {
                    label: "Toggle Phone Light (Torch)".into(),
                    activate: Box::new(|this: &mut Self| this.send(Command::WebcamToggleTorch)),
                    ..Default::default()
                }
                .into(),
            );
            items.push(
                StandardItem {
                    label: "Stop Webcam".into(),
                    activate: Box::new(|this: &mut Self| this.send(Command::WebcamStop)),
                    ..Default::default()
                }
                .into(),
            );
            items.push(MenuItem::Separator);
        }

        items.extend(vec![
            StandardItem {
                label: if self.window_visible {
                    "Hide Window".into()
                } else {
                    "Show Window".into()
                },
                activate: Box::new(|this: &mut Self| this.send(Command::ToggleWindow)),
                ..Default::default()
            }
            .into(),
            MenuItem::Separator,
            StandardItem {
                label: "Connect".into(),
                enabled: matches!(self.state, State::Disconnected),
                activate: Box::new(|this: &mut Self| this.send(Command::Connect)),
                ..Default::default()
            }
            .into(),
            StandardItem {
                label: "Disconnect".into(),
                enabled: matches!(self.state, State::Connected),
                activate: Box::new(|this: &mut Self| this.send(Command::Disconnect)),
                ..Default::default()
            }
            .into(),
            MenuItem::Separator,
            StandardItem {
                label: "Quit".into(),
                icon_name: "application-exit-symbolic".into(),
                activate: Box::new(|this: &mut Self| this.send(Command::Quit)),
                ..Default::default()
            }
            .into(),
        ]);
        items
    }
}

/// A spawned tray item. Dropping it tears the tray down.
pub struct Tray {
    handle: Handle<Item>,
}

impl Tray {
    /// Spawns the tray on its own thread. Returns the handle and the menu-command receiver, or
    /// `None` when the desktop has no StatusNotifierHost (then the app just runs without a tray).
    pub fn spawn() -> Option<(Tray, async_channel::Receiver<Command>)> {
        let (tx, rx) = async_channel::unbounded();
        let item = Item {
            state: State::Starting,
            window_visible: true,
            detail: String::new(),
            webcam: None,
            tx,
        };
        match item.spawn() {
            Ok(handle) => Some((Tray { handle }, rx)),
            Err(e) => {
                eprintln!("smartconnect-gui: system tray unavailable: {e}");
                None
            }
        }
    }

    /// Pushes the current connection state, window visibility, device line and optional webcam status
    /// to the tray.
    pub fn set(
        &self,
        state: State,
        window_visible: bool,
        detail: String,
        webcam: Option<WebcamTrayState>,
    ) {
        self.handle.update(|item| {
            item.state = state;
            item.window_visible = window_visible;
            item.detail = detail;
            item.webcam = webcam;
        });
    }
}
