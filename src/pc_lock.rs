//! Service 22 (PC lock): the phone's "Lock PC" tile.
//!
//! Protocol (phone `service/f.java` = ControlServiceHelper, bean `PcLockData`):
//! - phone -> PC: `{"ver":"1","cmd":1}` = lock now.
//! - PC -> phone: `{"ver":"1","cmd":2,"state":1}` once locked, `{"ver":"1","cmd":2,"state":0}`
//!   once unlocked again. The phone only registers the helper when our svc 22 version is >= 2.

use std::time::Duration;

use serde_json::{Value, json};
use tokio::process::Command;
use tokio::sync::broadcast::error::RecvError;
use tracing::{info, warn};

use crate::server::Daemon;

pub const SERVICE_PC_LOCK: i64 = 22;

/// True when the phone payload is the lock request (`cmd 1`).
pub fn is_lock_request(data: &Value) -> bool {
    data.get("cmd").and_then(Value::as_i64) == Some(1)
}

/// PC -> phone lock state report (`cmd 2`, state 1 = locked, 0 = unlocked).
pub fn state_msg(locked: bool) -> Value {
    json!({ "ver": "1", "cmd": 2, "state": i32::from(locked) })
}

async fn locked_hint() -> Option<bool> {
    let out = Command::new("loginctl")
        .args(["show-session", "auto", "-p", "LockedHint", "--value"])
        .output()
        .await
        .ok()?;
    match String::from_utf8_lossy(&out.stdout).trim() {
        "yes" => Some(true),
        "no" => Some(false),
        _ => None,
    }
}

/// Listens for svc 22 frames from the phone and locks the session via `loginctl lock-session`.
pub async fn run(daemon: std::sync::Arc<Daemon>) {
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
        info!("pc-lock: phone asked to lock the session");
        match Command::new("loginctl").arg("lock-session").status().await {
            Ok(s) if s.success() => {}
            Ok(s) => {
                warn!("pc-lock: loginctl lock-session exited with {s}");
                continue;
            }
            Err(e) => {
                warn!("pc-lock: cannot run loginctl: {e}");
                continue;
            }
        }
        let _ = daemon.to_phone.send((SERVICE_PC_LOCK, state_msg(true)));
        // Report the unlock so the phone's tile resets (the locker sets LockedHint via logind).
        let to_phone = daemon.to_phone.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(3)).await;
            // Give up after ~24 h, or if LockedHint is unavailable.
            for _ in 0..(24 * 3600 / 2) {
                match locked_hint().await {
                    Some(true) => tokio::time::sleep(Duration::from_secs(2)).await,
                    Some(false) => {
                        info!("pc-lock: session unlocked");
                        let _ = to_phone.send((SERVICE_PC_LOCK, state_msg(false)));
                        return;
                    }
                    None => return,
                }
            }
        });
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
}
