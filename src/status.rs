//! Live phone status for desktop UIs (the Noctalia plugin), built from what the phone already
//! pushes over the control socket: svc 25 device status (`cmd 1` battery/signal/…, `cmd 2`
//! wallpaper) and svc 100 `cmd 1018` unread counts. Published as JSON over `control.rs`.

use std::path::PathBuf;
use std::sync::Mutex;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use serde::Serialize;
use serde_json::Value;
use tokio::sync::broadcast;
use tracing::{info, warn};

#[derive(Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PhoneStatus {
    pub connected: bool,
    pub paired: bool,
    pub name: Option<String>,
    pub model: Option<String>,
    pub battery: Option<i64>,
    pub charging: bool,
    pub wifi: bool,
    pub wifi_signal: Option<i64>,
    pub cell_signal: Option<i64>,
    pub bluetooth: bool,
    pub unread: Option<i64>,
    /// Path of the phone's wallpaper image, saved under the runtime dir.
    pub wallpaper: Option<String>,
    /// Path of the current pairing QR (SVG).
    pub pairing_qr: Option<String>,
    /// This PC's address on the phone's network (the one the phone connected to). Helpers offer
    /// WebRTC candidates on it only.
    pub pc_address: Option<String>,
    /// The phone's address for this session (the WebSocket peer). Helpers that pull from servers
    /// the phone advertises (the fex transfer behind `smartconnect-fex`) connect to it.
    pub phone_address: Option<String>,
    /// Recent phone notifications, newest first (memory only, never written to disk).
    pub notifications: Vec<PhoneNotification>,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PhoneNotification {
    pub key: String,
    pub service: i64,
    pub app: String,
    /// Android package name, e.g. for the per-app mute (`notif_mute`).
    pub pkg: String,
    pub title: String,
    pub body: String,
    /// The app offers a free-text reply action.
    pub replyable: bool,
}

const MAX_NOTIFICATIONS: usize = 30;

pub struct StatusHub {
    state: Mutex<PhoneStatus>,
    wallpaper_md5: Mutex<Option<String>>,
    changes: broadcast::Sender<String>,
}

impl StatusHub {
    pub fn new(paired: bool, pairing_qr: Option<String>) -> Self {
        let state = PhoneStatus { paired, pairing_qr, ..Default::default() };
        Self { state: Mutex::new(state), wallpaper_md5: Mutex::new(None), changes: broadcast::channel(32).0 }
    }

    pub fn snapshot(&self) -> String {
        serde_json::to_string(&*self.state.lock().unwrap()).unwrap_or_default()
    }

    pub fn subscribe(&self) -> broadcast::Receiver<String> {
        self.changes.subscribe()
    }

    pub fn update(&self, f: impl FnOnce(&mut PhoneStatus)) {
        let json = {
            let mut s = self.state.lock().unwrap();
            f(&mut s);
            serde_json::to_string(&*s).unwrap_or_default()
        };
        let _ = self.changes.send(json);
    }

    /// Folds one phone message into the status, if it carries any.
    pub fn apply(&self, service_id: i64, data: &Value) {
        let cmd = data.get("cmd").and_then(Value::as_i64);
        let int = |k: &str| data.get(k).and_then(Value::as_i64);
        let flag = |k: &str| data.get(k).and_then(Value::as_bool).unwrap_or(false);
        let text = |k: &str| data.get(k).and_then(Value::as_str).filter(|s| !s.is_empty()).map(str::to_owned);
        match (service_id, cmd) {
            (25, Some(1)) => self.update(|s| {
                s.battery = int("battery").or(s.battery);
                s.charging = flag("isCharging");
                s.wifi = flag("wifi");
                s.wifi_signal = int("wifiSignal");
                s.cell_signal = int("dataSignal");
                s.bluetooth = flag("bluetooth");
                s.name = text("deviceName").or(s.name.take());
                s.model = text("model").or(s.model.take());
            }),
            (25, Some(2)) => self.save_wallpaper(data),
            (100, Some(1018)) => self.update(|s| s.unread = int("unreadCnt")),
            (crate::notifications::SERVICE_NOTIFY | crate::notifications::SERVICE_NOTIFY_WIDGET, _) => {
                self.apply_notification(service_id, data)
            }
            _ => {}
        }
    }

    fn apply_notification(&self, service_id: i64, n: &Value) {
        let Some(key) = n.get("key").and_then(Value::as_str) else { return };
        match n.get("event").and_then(Value::as_i64) {
            Some(0 | 1) if n.get("type").and_then(Value::as_i64) != Some(7) => {
                let d = n.get("data").cloned().unwrap_or(Value::Null);
                let s = |k: &str| d.get(k).and_then(Value::as_str).unwrap_or_default().to_owned();
                let replyable = n
                    .get("actions")
                    .and_then(Value::as_array)
                    .is_some_and(|a| a.iter().any(|a| a.get("remote_input").and_then(Value::as_bool) == Some(true)));
                let title = [s("conversation_title"), s("title")].into_iter().find(|t| !t.is_empty()).unwrap_or_default();
                let entry = PhoneNotification {
                    key: key.to_owned(),
                    service: service_id,
                    app: s("apk_name"),
                    pkg: s("package_name"),
                    title,
                    body: crate::notifications::body_text(&d),
                    replyable,
                };
                self.update(|st| {
                    st.notifications.retain(|x| x.key != key);
                    st.notifications.insert(0, entry);
                    st.notifications.truncate(MAX_NOTIFICATIONS);
                });
            }
            Some(2) => self.update(|st| st.notifications.retain(|x| x.key != key)),
            _ => {}
        }
    }

    pub fn notification_service(&self, key: &str) -> Option<i64> {
        self.state.lock().unwrap().notifications.iter().find(|n| n.key == key).map(|n| n.service)
    }

    fn save_wallpaper(&self, data: &Value) {
        let (Some(b64), md5) = (data.get("wallpaper").and_then(Value::as_str), data.get("md5").and_then(Value::as_str))
        else {
            return;
        };
        if md5.is_some() && self.wallpaper_md5.lock().unwrap().as_deref() == md5 {
            return;
        }
        let Ok(bytes) = B64.decode(b64.trim()) else {
            warn!("status: wallpaper is not base64");
            return;
        };
        let ext = if bytes.starts_with(&[0x89, b'P', b'N', b'G']) { "png" } else { "jpg" };
        let path = runtime_dir().join(format!("phone-wallpaper.{ext}"));
        if let Err(e) = std::fs::write(&path, &bytes) {
            warn!("status: saving wallpaper: {e}");
            return;
        }
        *self.wallpaper_md5.lock().unwrap() = md5.map(str::to_owned);
        info!(bytes = bytes.len(), path = %path.display(), "status: phone wallpaper saved");
        // A changing query string makes QML Image reload the same path.
        let url = format!("{}?{}", path.display(), md5.unwrap_or("x"));
        self.update(|s| s.wallpaper = Some(url));
    }
}

/// `$XDG_RUNTIME_DIR/smartconnectd/` (private, per-user, cleared on logout).
pub fn runtime_dir() -> PathBuf {
    let base = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
    let dir = base.join("smartconnectd");
    let _ = std::fs::create_dir_all(&dir);
    dir
}
