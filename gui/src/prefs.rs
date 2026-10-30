//! GUI preferences.
//!
//! - `$XDG_CONFIG_HOME/smartconnect-linux/gui.json` (usually `~/.config/…`): everything the app
//!   remembers, including the resolution chosen per display mode.
//! - `$XDG_CONFIG_HOME/smartconnect-linux/display.json`: the phone-as-display choices shared with
//!   `smartconnect-display` (so a start from the phone uses them) and with the Noctalia plugin.
//!   It is read on top of `gui.json`, so a change made elsewhere wins, and written on every change:
//!   `{"mode":"extend"|"mirror","size":"native"|"requested"|"WxH","displayScale":2.0,"oskHeight":45,
//!     "output":null|"NAME","audio":false,"followRotation":false,"bitrate":8000,"fps":60}`
//!
//! Unknown or missing fields fall back to defaults, so both files survive other versions.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Prefs {
    pub display: DisplayPrefs,
    pub files: FilesPrefs,
    pub apps: AppsPrefs,
    pub webcam: WebcamPrefs,
}

/// Choices for the phone-as-webcam feature.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct WebcamPrefs {
    /// Encoder bitrate quality: "low", "medium", "high", or "max".
    pub quality: String,
    /// Mirror stream (horizontal flip).
    pub mirror: bool,
    /// Flip stream vertically.
    pub flip_vertical: bool,
    /// Brightness adjustment: -1.0 to 1.0 (0.0 is normal).
    pub brightness: f64,
    /// Contrast adjustment: 0.0 to 2.0 (1.0 is normal).
    pub contrast: f64,
}

impl Default for WebcamPrefs {
    fn default() -> Self {
        Self {
            quality: "high".into(),
            mirror: false,
            flip_vertical: false,
            brightness: 0.0,
            contrast: 1.0,
        }
    }
}

/// Choices on the Files page (the phone's storage as a FUSE mount).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct FilesPrefs {
    /// Where to mount the phone's storage. Empty means the default `~/Phone`; a `~/…` path is
    /// expanded against `$HOME`.
    pub mount_dir: String,
    /// Mount the phone's storage automatically once the phone connects.
    pub auto_mount: bool,
    /// Share a PC folder with the phone (`smartconnect-files serve`) while the phone is connected.
    pub share: bool,
    /// The folder shared with the phone. Empty means the default, `~/SmartConnect`; `~/…` is expanded.
    pub share_dir: String,
    /// Also show hidden (dot) files and folders to the phone.
    pub share_hidden: bool,
    /// Refuse deletions from the phone (by default they move the files to the Trash).
    pub share_read_only: bool,
}

/// Remembered choices on the Apps page.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct AppsPrefs {
    /// Packages opened most recently, newest first (for the Recent row). Capped on save.
    pub recent: Vec<String>,
}

impl FilesPrefs {
    /// The folder to mount at: the configured one (with a leading `~/` expanded), else `~/Phone`.
    pub fn mount_path(&self) -> PathBuf {
        let dir = self.mount_dir.trim();
        if dir.is_empty() {
            default_mount_dir()
        } else {
            expand_tilde(dir)
        }
    }

    /// The folder shared with the phone: the configured one (`~/` expanded), else `~/SmartConnect`.
    pub fn share_path(&self) -> PathBuf {
        let dir = self.share_dir.trim();
        if dir.is_empty() {
            home_dir().join("SmartConnect")
        } else {
            expand_tilde(dir)
        }
    }
}

fn home_dir() -> PathBuf {
    PathBuf::from(std::env::var_os("HOME").unwrap_or_default())
}

/// `~/Phone`.
pub fn default_mount_dir() -> PathBuf {
    home_dir().join("Phone")
}

/// Expands a leading `~/` (or a bare `~`) against `$HOME`; other paths are taken as-is.
fn expand_tilde(path: &str) -> PathBuf {
    if path == "~" {
        home_dir()
    } else if let Some(rest) = path.strip_prefix("~/") {
        home_dir().join(rest)
    } else {
        PathBuf::from(path)
    }
}

/// Choices on the Display page (phone as a screen for this PC).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct DisplayPrefs {
    /// `extend` or `mirror` (`smartconnect-display --mode`; `mirror` is shown as Duplicate).
    pub mode: String,
    /// Resolution per mode: `native` (the phone's panel), `phone` (what the phone asks for,
    /// usually 1920x1080) or `custom` (`custom_width` x `custom_height`).
    pub extend_resolution: String,
    pub mirror_resolution: String,
    pub custom_width: u32,
    pub custom_height: u32,
    /// Extend only: scale of the phone's virtual monitor in percent (`--display-scale`).
    pub scale: u32,
    /// Extend only: output to switch on instead of the first `Virtual-*` one (`--output`).
    /// Empty means automatic.
    pub output: String,
    /// Height of the on-screen keyboard the phone's keyboard button opens, in percent of the
    /// shared screen (`--osk-height`).
    pub osk_height: u32,
    /// Experimental: send the PC's audio to the phone.
    pub audio: bool,
    /// Experimental: turn the phone's monitor when the phone is turned.
    pub follow_rotation: bool,
    /// Stream quality: video bitrate in kbit/s (`--bitrate`).
    pub bitrate: u32,
    /// Most frames per second sent (`--fps`).
    pub fps: u32,
}

/// Range offered for a custom stream size (either orientation).
pub const MIN_SIDE: u32 = 360;
pub const MAX_SIDE: u32 = 3840;
/// `--osk-height` range and default (percent of the screen).
pub const OSK_MIN: u32 = 15;
pub const OSK_MAX: u32 = 80;
pub const OSK_DEFAULT: u32 = 45;
/// `--bitrate` range and default (kbit/s) and `--fps` range and default.
pub const BITRATE_MIN: u32 = 500;
pub const BITRATE_MAX: u32 = 50_000;
pub const BITRATE_DEFAULT: u32 = 8000;
pub const FPS_MIN: u32 = 10;
pub const FPS_MAX: u32 = 60;
pub const FPS_DEFAULT: u32 = 60;

impl Default for DisplayPrefs {
    fn default() -> Self {
        Self {
            mode: "extend".into(),
            extend_resolution: "native".into(),
            mirror_resolution: "phone".into(),
            custom_width: 1920,
            custom_height: 1080,
            scale: 200,
            output: String::new(),
            osk_height: OSK_DEFAULT,
            audio: false,
            follow_rotation: false,
            bitrate: BITRATE_DEFAULT,
            fps: FPS_DEFAULT,
        }
    }
}

impl DisplayPrefs {
    /// Replaces values an older or hand-edited file may hold that the page cannot show.
    fn sanitized(mut self) -> Self {
        let d = Self::default();
        if !matches!(self.mode.as_str(), "extend" | "mirror") {
            self.mode = d.mode;
        }
        for (res, dflt) in [
            (&mut self.extend_resolution, d.extend_resolution),
            (&mut self.mirror_resolution, d.mirror_resolution),
        ] {
            if !matches!(res.as_str(), "native" | "phone" | "custom") {
                *res = dflt;
            }
        }
        self.custom_width = self.custom_width.clamp(MIN_SIDE, MAX_SIDE);
        self.custom_height = self.custom_height.clamp(MIN_SIDE, MAX_SIDE);
        // smartconnect-display accepts 50–400 %.
        if !(50..=400).contains(&self.scale) {
            self.scale = d.scale;
        }
        self.output = self.output.trim().to_owned();
        if !(OSK_MIN..=OSK_MAX).contains(&self.osk_height) {
            self.osk_height = d.osk_height;
        }
        if !(BITRATE_MIN..=BITRATE_MAX).contains(&self.bitrate) {
            self.bitrate = d.bitrate;
        }
        if !(FPS_MIN..=FPS_MAX).contains(&self.fps) {
            self.fps = d.fps;
        }
        self
    }

    /// The resolution choice of the current mode.
    pub fn resolution(&self) -> &str {
        if self.mode == "mirror" {
            &self.mirror_resolution
        } else {
            &self.extend_resolution
        }
    }

    fn resolution_mut(&mut self) -> &mut String {
        if self.mode == "mirror" {
            &mut self.mirror_resolution
        } else {
            &mut self.extend_resolution
        }
    }

    /// The shared `display.json` form.
    fn shared(&self) -> Value {
        let size = match self.resolution() {
            "native" => "native".to_owned(),
            "custom" => format!("{}x{}", self.custom_width, self.custom_height),
            _ => "requested".to_owned(),
        };
        json!({
            "mode": self.mode,
            "size": size,
            "displayScale": f64::from(self.scale) / 100.0,
            "oskHeight": self.osk_height,
            "output": if self.output.is_empty() { Value::Null } else { Value::from(self.output.clone()) },
            "audio": self.audio,
            "followRotation": self.follow_rotation,
            "bitrate": self.bitrate,
            "fps": self.fps,
        })
    }

    /// Takes over every valid field of a `display.json` written by any front-end.
    fn apply_shared(&mut self, v: &Value) {
        if let Some(mode) = v
            .get("mode")
            .and_then(Value::as_str)
            .filter(|m| matches!(*m, "extend" | "mirror"))
        {
            self.mode = mode.to_owned();
        }
        match v.get("size").and_then(Value::as_str) {
            Some("native") => *self.resolution_mut() = "native".into(),
            Some("requested") => *self.resolution_mut() = "phone".into(),
            Some(wxh) => {
                if let Some((w, h)) = wxh
                    .split_once('x')
                    .and_then(|(w, h)| Some((w.trim().parse().ok()?, h.trim().parse().ok()?)))
                {
                    *self.resolution_mut() = "custom".into();
                    (self.custom_width, self.custom_height) = (w, h);
                }
            }
            None => {}
        }
        if let Some(s) = v
            .get("displayScale")
            .and_then(Value::as_f64)
            .filter(|s| s.is_finite())
        {
            self.scale = (s * 100.0).round() as u32;
        }
        if let Some(h) = v.get("oskHeight").and_then(Value::as_u64) {
            self.osk_height = u32::try_from(h).unwrap_or(u32::MAX);
        }
        match v.get("output") {
            Some(Value::Null) => self.output.clear(),
            Some(Value::String(o)) => self.output = o.clone(),
            _ => {}
        }
        if let Some(a) = v.get("audio").and_then(Value::as_bool) {
            self.audio = a;
        }
        if let Some(r) = v.get("followRotation").and_then(Value::as_bool) {
            self.follow_rotation = r;
        }
        if let Some(b) = v.get("bitrate").and_then(Value::as_u64) {
            self.bitrate = u32::try_from(b).unwrap_or(u32::MAX);
        }
        if let Some(f) = v.get("fps").and_then(Value::as_u64) {
            self.fps = u32::try_from(f).unwrap_or(u32::MAX);
        }
    }
}

fn config_dir() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".config")
        })
        .join("smartconnect-linux")
}

pub fn path() -> PathBuf {
    config_dir().join("gui.json")
}

pub fn display_path() -> PathBuf {
    config_dir().join("display.json")
}

pub fn webcam_path() -> PathBuf {
    config_dir().join("webcam.json")
}

pub fn load() -> Prefs {
    let mut prefs: Prefs = std::fs::read_to_string(path())
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default();
    reload_shared(&mut prefs.display);
    reload_webcam(&mut prefs.webcam);
    prefs
}

/// Re-reads `display.json` (another front-end may have changed it). Returns whether anything changed.
pub fn reload_shared(display: &mut DisplayPrefs) -> bool {
    let before = display.clone();
    if let Some(v) = std::fs::read_to_string(display_path())
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
    {
        display.apply_shared(&v);
    }
    *display = std::mem::take(display).sanitized();
    *display != before
}

/// Re-reads `webcam.json` (shared with the helper and other front-ends).
pub fn reload_webcam(webcam: &mut WebcamPrefs) -> bool {
    let before = webcam.clone();
    if let Some(v) = std::fs::read_to_string(webcam_path())
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
    {
        if let Some(q) = v.get("quality").and_then(Value::as_str) {
            let lower = q.to_lowercase();
            if ["low", "medium", "high", "max"].contains(&lower.as_str()) {
                webcam.quality = lower;
            }
        }
    }
    *webcam != before
}

fn write_atomic(path: &Path, text: String) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, text + "\n")?;
    std::fs::rename(tmp, path)
}

/// Writes `gui.json`, `display.json`, and `webcam.json`.
pub fn save(prefs: &Prefs) -> std::io::Result<()> {
    write_atomic(
        &path(),
        serde_json::to_string_pretty(prefs).map_err(std::io::Error::other)?,
    )?;
    save_shared(&prefs.display)?;
    save_webcam(&prefs.webcam)
}

pub fn save_shared(display: &DisplayPrefs) -> std::io::Result<()> {
    write_atomic(
        &display_path(),
        serde_json::to_string_pretty(&display.shared()).map_err(std::io::Error::other)?,
    )
}

pub fn save_webcam(webcam: &WebcamPrefs) -> std::io::Result<()> {
    write_atomic(
        &webcam_path(),
        serde_json::to_string_pretty(&json!({ "quality": webcam.quality }))
            .map_err(std::io::Error::other)?,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_and_bad_fields_fall_back_to_defaults() {
        let p: Prefs = serde_json::from_str(
            r#"{"display":{"mode":"sideways","scale":5000,"customWidth":12,"oskHeight":3}}"#,
        )
        .unwrap();
        let d = p.display.sanitized();
        assert_eq!(d.mode, "extend");
        assert_eq!(d.scale, 200);
        assert_eq!(d.custom_width, MIN_SIDE);
        assert_eq!(d.osk_height, OSK_DEFAULT);
        assert_eq!(d.extend_resolution, "native");
        assert_eq!(d.mirror_resolution, "phone");
    }

    #[test]
    fn files_mount_path_defaults_and_expands() {
        let home = PathBuf::from(std::env::var_os("HOME").unwrap_or_default());
        // Empty → ~/Phone.
        assert_eq!(FilesPrefs::default().mount_path(), home.join("Phone"));
        // A `~/…` path is expanded against $HOME; an absolute path is taken as-is.
        assert_eq!(
            FilesPrefs {
                mount_dir: "~/Storage/Phone".into(),
                ..Default::default()
            }
            .mount_path(),
            home.join("Storage/Phone")
        );
        assert_eq!(
            FilesPrefs {
                mount_dir: "/mnt/phone".into(),
                ..Default::default()
            }
            .mount_path(),
            PathBuf::from("/mnt/phone")
        );
        assert_eq!(
            FilesPrefs::default().share_path(),
            home.join("SmartConnect")
        );
        assert_eq!(
            FilesPrefs {
                share_dir: "~/Public".into(),
                ..Default::default()
            }
            .share_path(),
            home.join("Public")
        );
    }

    #[test]
    fn gui_json_round_trips() {
        let mut p = Prefs::default();
        p.display.mode = "mirror".into();
        p.display.output = "HDMI-A-1".into();
        p.display.audio = true;
        let back: Prefs = serde_json::from_str(&serde_json::to_string(&p).unwrap()).unwrap();
        assert_eq!(back, p);
    }

    #[test]
    fn shared_file_matches_the_contract() {
        let d = DisplayPrefs::default();
        assert_eq!(
            d.shared(),
            json!({"mode":"extend","size":"native","displayScale":2.0,"oskHeight":45,"output":null,"audio":false,"followRotation":false,"bitrate":8000,"fps":60})
        );
        let mut d = DisplayPrefs {
            mode: "mirror".into(),
            output: "HDMI-A-1".into(),
            scale: 150,
            ..DisplayPrefs::default()
        };
        assert_eq!(d.shared()["size"], "requested");
        assert_eq!(d.shared()["displayScale"], 1.5);
        assert_eq!(d.shared()["output"], "HDMI-A-1");
        d.mirror_resolution = "custom".into();
        (d.custom_width, d.custom_height) = (1280, 720);
        assert_eq!(d.shared()["size"], "1280x720");
    }

    #[test]
    fn shared_file_is_taken_over() {
        let mut d = DisplayPrefs::default();
        d.apply_shared(&json!({"mode":"mirror","size":"1280x720","displayScale":1.75,"oskHeight":60,"output":"HDMI-A-1","audio":true,"followRotation":true}));
        assert_eq!(
            (
                d.mode.as_str(),
                d.mirror_resolution.as_str(),
                d.custom_width,
                d.custom_height
            ),
            ("mirror", "custom", 1280, 720)
        );
        assert_eq!(
            (
                d.scale,
                d.osk_height,
                d.output.as_str(),
                d.audio,
                d.follow_rotation
            ),
            (175, 60, "HDMI-A-1", true, true)
        );
        // The extend choice is kept for when the mode switches back.
        assert_eq!(d.extend_resolution, "native");
        // Partial or odd files only change what they validly hold.
        d.apply_shared(&json!({"mode":"sideways","size":"requested","output":null}));
        assert_eq!(
            (
                d.mode.as_str(),
                d.mirror_resolution.as_str(),
                d.output.as_str()
            ),
            ("mirror", "phone", "")
        );
    }
}
