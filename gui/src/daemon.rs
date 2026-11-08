//! Talking to `smartconnectd` over its control socket (`$XDG_RUNTIME_DIR/smartconnectd.sock`).
//!
//! All socket I/O runs on worker threads: one long-lived subscriber thread that streams status
//! changes (reconnecting whenever the daemon restarts), and one short-lived connection per
//! command. Results reach the GTK main loop through an `async_channel` / `gio::spawn_blocking`.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::Duration;

use serde::Deserialize;
use serde_json::Value;

/// How long to wait before re-dialing a missing/restarted daemon.
const RETRY: Duration = Duration::from_secs(2);
/// `connect` dials the phone, which can take a while.
const COMMAND_TIMEOUT: Duration = Duration::from_secs(30);

pub fn socket_path() -> PathBuf {
    runtime_dir().join("smartconnectd.sock")
}

fn runtime_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("XDG_RUNTIME_DIR").filter(|v| !v.is_empty()) {
        return PathBuf::from(dir);
    }
    // No session runtime dir: use the private per-user state folder, never the shared /tmp.
    std::env::var_os("XDG_STATE_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state")))
        .unwrap_or_default()
        .join("smartconnect-linux/run")
}

/// Availability of a specific feature.
#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(tag = "status", rename_all = "camelCase")]
pub enum FeatureStatus {
    #[default]
    Available,
    #[serde(rename_all = "camelCase")]
    Unavailable { reason: String, fix_hint: String },
}

#[allow(dead_code)]
impl FeatureStatus {
    pub fn is_available(&self) -> bool {
        matches!(self, Self::Available)
    }

    pub fn tooltip(&self, default_tooltip: &str) -> String {
        match self {
            Self::Available => default_tooltip.to_string(),
            Self::Unavailable { reason, fix_hint } => {
                format!("{reason}\nFix: {fix_hint}")
            }
        }
    }
}

/// Feature availability matrix reported by the daemon.
#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct Capabilities {
    pub mirror: FeatureStatus,
    pub webcam: FeatureStatus,
    pub app_streaming: FeatureStatus,
    pub cross_control: FeatureStatus,
    pub phone_display: FeatureStatus,
    pub storage_mount: FeatureStatus,
    pub folder_share: FeatureStatus,
    pub clipboard: FeatureStatus,
    pub notifications: FeatureStatus,
}

/// Per-device feature switches.
#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct DeviceFeatures {
    pub mirror: bool,
    pub webcam: bool,
    pub app_streaming: bool,
    pub cross_control: bool,
    pub phone_display: bool,
    pub storage_mount: bool,
    pub folder_share: bool,
    pub clipboard: bool,
    pub notifications: bool,
}

/// Mirror of the daemon's `PhoneStatus` (see `src/status.rs`). Every field is optional on the
/// wire so an older/newer daemon never breaks parsing.
#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct Status {
    pub connected: bool,
    pub paired: bool,
    pub device_id: Option<String>,
    pub name: Option<String>,
    pub model: Option<String>,
    pub battery: Option<i64>,
    pub charging: bool,
    pub wifi: bool,
    pub wifi_signal: Option<i64>,
    pub cell_signal: Option<i64>,
    pub bluetooth: bool,
    pub unread: Option<i64>,
    pub wallpaper: Option<String>,
    pub pairing_qr: Option<String>,
    pub notifications: Vec<Notification>,
    pub capabilities: Option<Capabilities>,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct Notification {
    pub key: String,
    pub service: i64,
    pub app: String,
    /// Android package, for the per-app mute (empty from an older daemon).
    pub pkg: String,
    pub title: String,
    pub body: String,
    pub replyable: bool,
}

impl Status {
    /// Wallpaper path without the `?md5` cache-buster the daemon appends for QML.
    pub fn wallpaper_path(&self) -> Option<&str> {
        self.wallpaper
            .as_deref()
            .map(|w| w.split('?').next().unwrap_or(w))
            .filter(|p| !p.is_empty())
    }
}

pub enum Event {
    Status(Box<Status>),
    /// The daemon is not running (or just went away).
    Down,
    /// The phone's Cross Control tile was tapped (service 26 `cmd 0`).
    CrossControlRequested,
}

/// Starts the subscriber thread. It never panics and exits only when the receiver is dropped.
pub fn subscribe() -> async_channel::Receiver<Event> {
    let (tx, rx) = async_channel::unbounded();
    let tx2 = tx.clone();
    std::thread::Builder::new()
        .name("smartconnectd-subscribe".into())
        .spawn(move || {
            let mut up: Option<bool> = None;
            loop {
                let result = stream_status(&tx, &mut up);
                if tx.is_closed() {
                    return;
                }
                if up != Some(false) {
                    if let Err(e) = result {
                        eprintln!("smartconnect-gui: daemon unavailable: {e}");
                    }
                    up = Some(false);
                    if tx.send_blocking(Event::Down).is_err() {
                        return;
                    }
                }
                std::thread::sleep(RETRY);
            }
        })
        .expect("spawning subscriber thread");
    std::thread::Builder::new()
        .name("smartconnectd-watch26".into())
        .spawn(move || {
            while !tx2.is_closed() {
                let _ = watch_cross_requests(&tx2);
                std::thread::sleep(RETRY);
            }
        })
        .expect("spawning watch thread");
    rx
}

/// Watches service 26 frames from the phone and reports its `cmd 0` (tile tapped).
fn watch_cross_requests(tx: &async_channel::Sender<Event>) -> std::io::Result<()> {
    let mut stream = UnixStream::connect(socket_path())?;
    stream.write_all(b"{\"cmd\":\"watch\",\"services\":[26]}\n")?;
    for line in BufReader::new(stream).lines() {
        let Ok(v) = serde_json::from_str::<Value>(&line?) else {
            continue;
        };
        if v.get("service").and_then(Value::as_i64) == Some(26)
            && v.pointer("/data/cmd").and_then(Value::as_i64) == Some(0)
            && tx.send_blocking(Event::CrossControlRequested).is_err()
        {
            return Ok(());
        }
    }
    Ok(())
}

/// One `subscribe` session: returns when the daemon hangs up or the socket errors.
fn stream_status(tx: &async_channel::Sender<Event>, up: &mut Option<bool>) -> std::io::Result<()> {
    let mut stream = UnixStream::connect(socket_path())?;
    stream.write_all(b"subscribe\n")?;
    for line in BufReader::new(stream).lines() {
        let line = line?;
        match serde_json::from_str::<Status>(&line) {
            Ok(status) => {
                *up = Some(true);
                if tx.send_blocking(Event::Status(Box::new(status))).is_err() {
                    return Ok(());
                }
            }
            Err(e) => eprintln!("smartconnect-gui: bad status line: {e}"),
        }
    }
    Ok(())
}

/// Reply to a one-shot command: `{"ok":bool,"message":…}` plus whatever else the daemon adds.
pub struct Reply {
    pub ok: bool,
    pub message: String,
    pub raw: Value,
}

/// Sends one command line and waits for its JSON reply. Blocking: call it off the main thread.
pub fn command(line: &str) -> Result<Reply, String> {
    let run = || -> std::io::Result<String> {
        let mut stream = UnixStream::connect(socket_path())?;
        stream.set_read_timeout(Some(COMMAND_TIMEOUT))?;
        stream.write_all(format!("{}\n", line.trim()).as_bytes())?;
        let mut reply = String::new();
        BufReader::new(stream).read_line(&mut reply)?;
        Ok(reply)
    };
    let text = run().map_err(|e| format!("smartconnectd: {e}"))?;
    let raw: Value = serde_json::from_str(text.trim())
        .map_err(|_| "smartconnectd closed the connection".to_owned())?;
    Ok(Reply {
        ok: raw.get("ok").and_then(Value::as_bool).unwrap_or(false),
        message: raw
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        raw,
    })
}

/// `{"cmd":"reply","key":K,"text":T}`
pub fn reply_line(key: &str, text: &str) -> String {
    serde_json::json!({ "cmd": "reply", "key": key, "text": text }).to_string()
}

/// "Open on phone": forward a notification click (phone event 2) for `key` on its service, which
/// fires the notification's own intent on the phone (and, on a LAN link, offers to stream the app).
/// Wrapped in the daemon's `send` command, the same path the daemon uses for a desktop-side click.
pub fn notif_open_line(key: &str, service: i64) -> String {
    serde_json::json!({
        "cmd": "send",
        "service": service,
        "data": { "cmd": 0, "event": 2, "key": key, "version": "1.0" },
    })
    .to_string()
}

/// `{"cmd":"notif_mute","pkg":P,"mute":B}` mutes or unmutes an app's desktop popups; without a
/// package it only asks for the list. Either way the reply carries the whole list in `muted`.
pub fn mute_line(change: Option<(&str, bool)>) -> String {
    match change {
        Some((pkg, mute)) => serde_json::json!({ "cmd": "notif_mute", "pkg": pkg, "mute": mute }),
        None => serde_json::json!({ "cmd": "notif_mute" }),
    }
    .to_string()
}

impl Reply {
    /// The muted packages in a `notif_mute` reply.
    pub fn muted(&self) -> Option<Vec<String>> {
        let list = self.raw.get("muted")?.as_array()?;
        Some(
            list.iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect(),
        )
    }
}

/// Queries the daemon's local feature capabilities.
#[allow(dead_code)]
pub fn capabilities() -> Result<Capabilities, String> {
    let rep = command("capabilities")?;
    if !rep.ok {
        return Err(rep.message);
    }
    let caps_val = rep.raw.get("capabilities").cloned().unwrap_or(rep.raw);
    serde_json::from_value(caps_val).map_err(|e| format!("parsing capabilities: {e}"))
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CheckStatus {
    #[default]
    Ok,
    Warn,
    Fail,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DoctorCheck {
    pub id: String,
    pub title: String,
    pub status: CheckStatus,
    pub detail: String,
    pub fix: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DoctorReport {
    pub checks: Vec<DoctorCheck>,
}

pub fn doctor() -> Result<DoctorReport, String> {
    let rep = command("doctor")?;
    if !rep.ok {
        return Err(rep.message);
    }
    let report_val = rep.raw.get("report").cloned().unwrap_or(rep.raw);
    serde_json::from_value(report_val).map_err(|e| format!("parsing doctor report: {e}"))
}

pub fn run_fix_pkexec(fix_cmd: &str) -> Result<(), String> {
    const SAFE_FIX_COMMANDS: &[&str] = &[
        "systemctl start avahi-daemon",
        "systemctl start bluetooth",
        "modprobe v4l2loopback devices=1 video_nr=9 card_label='SmartConnect Webcam' exclusive_caps=1",
        "modprobe fuse",
        "firewall-cmd --add-port=52020/tcp --permanent",
        "firewall-cmd --add-port=2049/tcp --permanent",
        "firewall-cmd --reload",
        "ufw allow 52020/tcp",
        "ufw allow 2049/tcp",
    ];
    fn clean_cmd(s: &str) -> &str {
        s.trim().strip_prefix("sudo ").unwrap_or(s.trim()).trim()
    }
    let is_safe = if fix_cmd.contains("&&") {
        fix_cmd
            .split("&&")
            .all(|sub| SAFE_FIX_COMMANDS.iter().any(|&s| s == clean_cmd(sub)))
    } else {
        SAFE_FIX_COMMANDS.iter().any(|&s| s == clean_cmd(fix_cmd))
    };
    if !is_safe {
        return Err(format!(
            "Command '{fix_cmd}' is not in the safe fix whitelist"
        ));
    }
    let stripped = if fix_cmd.contains("&&") {
        fix_cmd
            .split("&&")
            .map(clean_cmd)
            .collect::<Vec<_>>()
            .join(" && ")
    } else {
        clean_cmd(fix_cmd).to_string()
    };
    let status = std::process::Command::new("pkexec")
        .arg("sh")
        .arg("-c")
        .arg(&stripped)
        .status()
        .map_err(|e| format!("running pkexec: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("Command exited with status {status}"))
    }
}

pub fn fetch_features(device: &str) -> Result<DeviceFeatures, String> {
    let cmd = serde_json::json!({ "cmd": "feature.get", "device": device }).to_string();
    let rep = command(&cmd)?;
    if !rep.ok {
        return Err(rep.message);
    }
    let feats_val = rep.raw.get("features").cloned().unwrap_or(rep.raw);
    serde_json::from_value(feats_val).map_err(|e| format!("parsing features: {e}"))
}

pub fn set_feature(device: &str, feature: &str, enabled: bool) -> Result<(), String> {
    let cmd = serde_json::json!({
        "cmd": "feature.set",
        "device": device,
        "feature": feature,
        "enabled": enabled
    })
    .to_string();
    let rep = command(&cmd)?;
    if rep.ok {
        Ok(())
    } else {
        Err(rep.message)
    }
}
