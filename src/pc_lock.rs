//! Service 22 (PC lock): the phone's "Lock PC" tile.
//!
//! Protocol (phone `service/f.java` = ControlServiceHelper, bean `PcLockData`):
//! - phone -> PC: `{"ver":"1","cmd":1}` = lock now.
//! - PC -> phone: `{"ver":"1","cmd":2,"state":1}` once locked, `{"ver":"1","cmd":2,"state":0}`
//!   once unlocked again. State 1 puts the tile in its "locked" state, state 0 resets it.
//!   The phone only registers the helper when our svc 22 version is >= 2.
//!
//! Locking: `--lock-command` if given, else Noctalia's lock IPC when Noctalia is running, else
//! `loginctl lock-session` on the user's graphical session (`loginctl show-user <uid> -p Display`).
//! The lock is only reported to the phone once it is seen: logind's `LockedHint` turns true within
//! a few seconds (GNOME, KDE, swaylock via swayidle all set it). If nothing reacts to the logind
//! path, the phone gets `state 0` and the desktop a notification saying why. GNOME Shell, for
//! one, only creates its lock screen when GDM is the display manager.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::process::Command;
use tokio::sync::broadcast::error::RecvError;
use tracing::{info, warn};

use crate::server::Daemon;

pub const SERVICE_PC_LOCK: i64 = 22;

/// True while a task is waiting to report the unlock, so repeated taps don't stack trackers.
static TRACKING: AtomicBool = AtomicBool::new(false);

/// True when the phone payload is the lock request (`cmd 1`).
pub fn is_lock_request(data: &Value) -> bool {
    data.get("cmd").and_then(Value::as_i64) == Some(1)
}

/// PC -> phone lock state report (`cmd 2`, state 1 = locked, 0 = unlocked).
pub fn state_msg(locked: bool) -> Value {
    json!({ "ver": "1", "cmd": 2, "state": i32::from(locked) })
}

#[derive(Debug)]
enum Locker {
    /// `--lock-command`, run with `sh -c`.
    Command(String),
    /// Noctalia's lock screen; it ignores logind's Lock signal.
    Noctalia { qs: PathBuf, pid: u32 },
    /// `loginctl lock-session <id>`: GNOME, KDE, and lockers started by swayidle/hypridle.
    Logind,
}

/// Finds a running Noctalia shell: a `qs`/`quickshell` process with `noctalia-shell` in its
/// arguments. Returns its executable (to run `qs ipc` with the same build) and pid.
fn find_noctalia() -> Option<(PathBuf, u32)> {
    for entry in std::fs::read_dir("/proc").ok()?.flatten() {
        let Some(pid) = entry.file_name().to_str().and_then(|s| s.parse::<u32>().ok()) else {
            continue;
        };
        let Ok(cmdline) = std::fs::read(entry.path().join("cmdline")) else {
            continue;
        };
        let mut args = cmdline.split(|&b| b == 0).map(String::from_utf8_lossy);
        let argv0 = args.next().unwrap_or_default();
        let name = argv0.rsplit('/').next().unwrap_or_default();
        if !matches!(name, "qs" | "quickshell") || !args.any(|a| a.contains("noctalia-shell")) {
            continue;
        }
        let exe = std::fs::read_link(entry.path().join("exe")).unwrap_or_else(|_| PathBuf::from(name));
        return Some((exe, pid));
    }
    None
}

fn pick_locker(lock_command: Option<&str>) -> Locker {
    if let Some(cmd) = lock_command {
        return Locker::Command(cmd.to_string());
    }
    match find_noctalia() {
        Some((qs, pid)) => Locker::Noctalia { qs, pid },
        None => Locker::Logind,
    }
}

/// The user's graphical login session (logind `User.Display`), or `auto` (the caller's session,
/// else the user's display session) when that can't be read.
async fn graphical_session() -> String {
    let uid = unsafe { libc::getuid() };
    let out = Command::new("loginctl")
        .args(["show-user", &uid.to_string(), "-p", "Display", "--value"])
        .output()
        .await;
    match out {
        Ok(o) if o.status.success() && !String::from_utf8_lossy(&o.stdout).trim().is_empty() => {
            String::from_utf8_lossy(&o.stdout).trim().to_string()
        }
        _ => "auto".to_string(),
    }
}

async fn locked_hint(session: &str) -> Option<bool> {
    let out = Command::new("loginctl")
        .args(["show-session", session, "-p", "LockedHint", "--value"])
        .output()
        .await
        .ok()?;
    match String::from_utf8_lossy(&out.stdout).trim() {
        "yes" => Some(true),
        "no" => Some(false),
        _ => None,
    }
}

async fn lock(locker: &Locker, session: &str) -> Result<(), String> {
    let mut cmd = match locker {
        Locker::Command(c) => {
            let mut cmd = Command::new("sh");
            cmd.args(["-c", c]);
            cmd
        }
        Locker::Noctalia { qs, pid } => {
            let mut cmd = Command::new(qs);
            cmd.args(["ipc", "--pid", &pid.to_string(), "call", "lockScreen", "lock"]);
            cmd
        }
        Locker::Logind => {
            let mut cmd = Command::new("loginctl");
            cmd.args(["lock-session", session]);
            cmd
        }
    };
    match cmd.output().await {
        Ok(o) if o.status.success() => Ok(()),
        Ok(o) => Err(format!("exited with {}: {}", o.status, String::from_utf8_lossy(&o.stderr).trim())),
        Err(e) => Err(e.to_string()),
    }
}

/// Waits up to `limit` for logind's `LockedHint` on `session` to turn true.
async fn wait_locked(session: &str, limit: Duration) -> bool {
    let deadline = tokio::time::Instant::now() + limit;
    loop {
        match locked_hint(session).await {
            Some(true) => return true,
            None => return false,
            Some(false) if tokio::time::Instant::now() >= deadline => return false,
            Some(false) => tokio::time::sleep(Duration::from_millis(250)).await,
        }
    }
}

async fn has_system_bus_name(name: &str) -> bool {
    let Ok(conn) = zbus::Connection::system().await else { return false };
    let Ok(dbus) = zbus::fdo::DBusProxy::new(&conn).await else { return false };
    let Ok(name) = zbus::names::BusName::try_from(name) else { return false };
    dbus.name_has_owner(name).await.unwrap_or(false)
}

/// Why `loginctl lock-session` locked nothing, for the log and the desktop notification.
async fn no_lock_reason() -> String {
    let gnome = std::env::var("XDG_CURRENT_DESKTOP").is_ok_and(|d| d.split(':').any(|d| d.eq_ignore_ascii_case("GNOME")));
    if gnome && !has_system_bus_name("org.gnome.DisplayManager").await {
        return "GNOME Shell only has a lock screen when GDM is the display manager, and this session wasn't started by GDM."
            .to_string();
    }
    "No screen locker reacted to logind's Lock signal. Pass --lock-command to smartconnectd, or have your idle daemon lock on it."
        .to_string()
}

/// Reports the unlock to the phone once logind's `LockedHint` turns false again.
async fn track_unlock(daemon: std::sync::Arc<Daemon>, session: String) {
    // Give up after ~24 h, or if LockedHint becomes unreadable.
    for _ in 0..(24 * 3600 / 2) {
        match locked_hint(&session).await {
            Some(true) => tokio::time::sleep(Duration::from_secs(2)).await,
            Some(false) => {
                info!("pc-lock: session unlocked");
                let _ = daemon.to_phone.send((SERVICE_PC_LOCK, state_msg(false)));
                return;
            }
            None => return,
        }
    }
}

/// Listens for svc 22 frames from the phone and locks the desktop session.
pub async fn run(daemon: std::sync::Arc<Daemon>, lock_command: Option<String>) {
    let mut frames = daemon.from_phone.subscribe();
    loop {
        let (svc, data) = match frames.recv().await {
            Ok(f) => f,
            Err(RecvError::Lagged(_)) => continue,
            Err(RecvError::Closed) => return,
        };
        if svc != SERVICE_PC_LOCK || !is_lock_request(&data) {
            continue;
        }
        let session = graphical_session().await;
        let locker = pick_locker(lock_command.as_deref());
        info!(?locker, %session, "pc-lock: phone asked to lock the session");
        if let Err(e) = lock(&locker, &session).await {
            warn!(?locker, "pc-lock: locking failed: {e}");
            let _ = daemon.to_phone.send((SERVICE_PC_LOCK, state_msg(false)));
            continue;
        }
        let seen = wait_locked(&session, Duration::from_secs(3)).await;
        if !seen && matches!(locker, Locker::Logind) {
            let why = no_lock_reason().await;
            warn!("pc-lock: nothing locked the screen: {why}");
            if let Some(n) = daemon.notifier.get() {
                n.notify_local("Couldn't lock this PC", &why).await;
            }
            // Resets the phone's tile instead of leaving it waiting.
            let _ = daemon.to_phone.send((SERVICE_PC_LOCK, state_msg(false)));
            continue;
        }
        let _ = daemon.to_phone.send((SERVICE_PC_LOCK, state_msg(true)));
        if !seen {
            // Noctalia and custom commands needn't set LockedHint: trust them, but the unlock
            // can't be followed, so the tile stays "locked" until the next lock or reconnect.
            info!("pc-lock: the screen locker doesn't report logind's LockedHint; the phone won't be told when the PC unlocks");
        } else if !TRACKING.swap(true, Ordering::SeqCst) {
            let daemon = daemon.clone();
            tokio::spawn(async move {
                track_unlock(daemon, session).await;
                TRACKING.store(false, Ordering::SeqCst);
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_lock_request() {
        assert!(is_lock_request(&json!({"ver":"1","cmd":1})));
        assert!(!is_lock_request(&json!({"ver":"1","cmd":2,"state":1})));
        assert!(!is_lock_request(&json!({})));
    }

    #[test]
    fn state_message_shape() {
        assert_eq!(state_msg(true), json!({"ver":"1","cmd":2,"state":1}));
        assert_eq!(state_msg(false), json!({"ver":"1","cmd":2,"state":0}));
    }

    #[test]
    fn lock_command_overrides_detection() {
        assert!(matches!(pick_locker(Some("swaylock -f")), Locker::Command(c) if c == "swaylock -f"));
    }
}
