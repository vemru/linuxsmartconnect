//! The last connected phone, remembered by the GUI so the Phone page and the device menu can show it
//! while the phone is away, while the daemon is stopped, and after an app restart.
//!
//! The daemon keeps the phone's name, battery and wallpaper only for its own lifetime (a restart
//! starts empty) and saves the wallpaper under `$XDG_RUNTIME_DIR` (cleared at logout). So the GUI
//! keeps its own copy:
//!
//! - `$XDG_STATE_HOME/smartconnect-linux/last-device.json` (default `~/.local/state/…`):
//!   `{"name","model","battery","charging","wifi","wifiSignal","cellSignal","bluetooth",
//!     "lastConnected":<unix seconds>,"wallpaper":"<path of our copy>"}`
//! - `last-wallpaper.<ext>` next to it: a copy of the phone's wallpaper.
//!
//! Unknown or missing fields fall back to defaults, so the file survives other versions.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::daemon::Status;

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct LastDevice {
    pub name: Option<String>,
    pub model: Option<String>,
    pub battery: Option<i64>,
    pub charging: bool,
    pub wifi: bool,
    pub wifi_signal: Option<i64>,
    pub cell_signal: Option<i64>,
    pub bluetooth: bool,
    /// Unix seconds when the phone was last seen connected.
    pub last_connected: Option<u64>,
    /// Absolute path of our copy of the wallpaper.
    pub wallpaper: Option<String>,
}

impl LastDevice {
    /// Whether anything about a phone is known at all.
    pub fn known(&self) -> bool {
        self.name.is_some() || self.last_connected.is_some()
    }

    /// Takes over what a connected status reports (keeps a known name/model if the status lacks one).
    pub fn absorb(&mut self, s: &Status) {
        if let Some(name) = s.name.clone().filter(|n| !n.is_empty()) {
            self.name = Some(name);
        }
        if let Some(model) = s.model.clone().filter(|m| !m.is_empty()) {
            self.model = Some(model);
        }
        self.battery = s.battery.or(self.battery);
        self.charging = s.charging;
        self.wifi = s.wifi;
        self.wifi_signal = s.wifi_signal;
        self.cell_signal = s.cell_signal;
        self.bluetooth = s.bluetooth;
    }

    /// Everything but the timestamp equal (to decide whether a save is worth it).
    pub fn same_fields(&self, other: &Self) -> bool {
        Self { last_connected: None, ..self.clone() } == Self { last_connected: None, ..other.clone() }
    }

    /// The cached wallpaper, if the file is still there.
    pub fn wallpaper_path(&self) -> Option<&str> {
        self.wallpaper.as_deref().filter(|p| Path::new(p).is_file())
    }

    /// "last connected 5 min ago", or `None` when never seen.
    pub fn last_seen(&self) -> Option<String> {
        self.last_connected.map(|t| format!("last connected {}", relative(t, now_secs())))
    }
}

fn state_dir() -> PathBuf {
    std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".local/state"))
        .join("smartconnect-linux")
}

pub fn path() -> PathBuf {
    state_dir().join("last-device.json")
}

pub fn load() -> LastDevice {
    load_from(&path())
}

fn load_from(path: &Path) -> LastDevice {
    std::fs::read_to_string(path).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default()
}

pub fn save(d: &LastDevice) -> std::io::Result<()> {
    save_to(&path(), d)
}

fn save_to(path: &Path, d: &LastDevice) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(d).map_err(std::io::Error::other)? + "\n")?;
    std::fs::rename(tmp, path)
}

/// Copies the daemon's wallpaper file into the state dir as `last-wallpaper.<ext>` and returns the
/// copy's path. Written to a temp file first, so a reader never sees half an image.
pub fn copy_wallpaper(src: &Path) -> Option<String> {
    let ext = src.extension().and_then(|e| e.to_str()).filter(|e| !e.is_empty()).unwrap_or("img");
    let dir = state_dir();
    std::fs::create_dir_all(&dir).ok()?;
    let dest = dir.join(format!("last-wallpaper.{ext}"));
    let tmp = dir.join(format!("last-wallpaper.{ext}.tmp"));
    std::fs::copy(src, &tmp).ok()?;
    std::fs::rename(&tmp, &dest).ok()?;
    Some(dest.display().to_string())
}

pub fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// A short relative time: "just now", "5 min ago", "3 h ago", "yesterday", "4 days ago".
pub fn relative(then: u64, now: u64) -> String {
    let d = now.saturating_sub(then);
    match d {
        0..=59 => "just now".to_owned(),
        60..=3599 => format!("{} min ago", d / 60),
        3600..=86_399 => format!("{} h ago", d / 3600),
        86_400..=172_799 => "yesterday".to_owned(),
        _ => format!("{} days ago", d / 86_400),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_times() {
        assert_eq!(relative(1000, 1030), "just now");
        assert_eq!(relative(1000, 1000 + 5 * 60), "5 min ago");
        assert_eq!(relative(1000, 1000 + 3 * 3600), "3 h ago");
        assert_eq!(relative(1000, 1000 + 30 * 3600), "yesterday");
        assert_eq!(relative(1000, 1000 + 4 * 86_400), "4 days ago");
        // A clock that went backwards is "just now", not a panic.
        assert_eq!(relative(2000, 1000), "just now");
    }

    #[test]
    fn absorb_keeps_known_name_and_round_trips() {
        let mut d = LastDevice { name: Some("Phone A".into()), model: Some("Model X".into()), ..LastDevice::default() };
        let s = Status { connected: true, battery: Some(46), wifi: true, wifi_signal: Some(4), ..Status::default() };
        d.absorb(&s);
        assert_eq!((d.name.as_deref(), d.model.as_deref(), d.battery, d.wifi), (Some("Phone A"), Some("Model X"), Some(46), true));
        let later = LastDevice { last_connected: Some(5), ..d.clone() };
        assert!(d.same_fields(&later), "the timestamp alone is no field change");

        let dir = std::env::temp_dir().join(format!("sc-gui-lastdev-{}", std::process::id()));
        let file = dir.join("last-device.json");
        save_to(&file, &later).unwrap();
        assert_eq!(load_from(&file), later);
        // Missing/garbage file → defaults.
        std::fs::write(&file, "not json").unwrap();
        assert_eq!(load_from(&file), LastDevice::default());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
