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
    let dir = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
    dir.join("smartconnectd.sock")
}

/// Mirror of the daemon's `PhoneStatus` (see `src/status.rs`). Every field is optional on the
/// wire so an older/newer daemon never breaks parsing.
#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct Status {
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
    pub wallpaper: Option<String>,
    pub pairing_qr: Option<String>,
    pub notifications: Vec<Notification>,
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
        self.wallpaper.as_deref().map(|w| w.split('?').next().unwrap_or(w)).filter(|p| !p.is_empty())
    }
}

pub enum Event {
    Status(Box<Status>),
    /// The daemon is not running (or just went away).
    Down,
}

/// Starts the subscriber thread. It never panics and exits only when the receiver is dropped.
pub fn subscribe() -> async_channel::Receiver<Event> {
    let (tx, rx) = async_channel::unbounded();
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
    rx
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
    let raw: Value = serde_json::from_str(text.trim()).map_err(|_| "smartconnectd closed the connection".to_owned())?;
    Ok(Reply {
        ok: raw.get("ok").and_then(Value::as_bool).unwrap_or(false),
        message: raw.get("message").and_then(Value::as_str).unwrap_or_default().to_owned(),
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
        Some(list.iter().filter_map(Value::as_str).map(str::to_owned).collect())
    }
}
