//! Hotspot service (serviceId 13): the phone turns on a hotspot and the PC joins it.
//!
//! From the phone APK (`mobiledesktop.service.k` = HotspotServiceHelper, `service.beans.HotspotData`):
//! the PC chooses the SSID and passphrase, not the phone.
//! 1. Tile tap: the phone sends `cmd 0` (invite). If the phone has no validated network it sends
//!    `cmd 5 {netAvailable}` instead.
//! 2. The PC replies `cmd 1 {authType, ssid, pwd, token, wsPort, fp}`. The phone starts a SoftAP
//!    with exactly that config (max 1 client). authType is the SoftAp security type
//!    (0 open, 1 WPA2-PSK, 2 WPA3 transition, 3 WPA3-SAE).
//! 3. Once tethered, the phone sends `cmd 2`; the PC then joins with nmcli.
//! 4. `cmd 4` = stop (either side); `cmd 6 {errorCode}` = cancel/failure (1312 busy, 1313 stopped,
//!    1314 tether failed, 145/147 denied, 306 admin-disabled, 1537 not owner user).
//!
//! `wsPort` is sent as 0: with a port the phone would also dial our WebSocket over the hotspot
//! with `token`, whose meaning we have not reversed. The existing link recovers through the
//! daemon's normal reconnect instead.
//!
//! The passphrase is never logged: `HotspotConfig`'s Debug redacts it.

use std::fmt;
use std::sync::Arc;

use rand::Rng;
use rand::distributions::Alphanumeric;
use serde_json::{Value, json};
use tokio::process::Command;
use tokio::sync::broadcast::error::RecvError;
use tracing::{info, warn};

use crate::server::Daemon;

pub const SERVICE_HOTSPOT: i64 = 13;

pub const CMD_INVITE: i64 = 0;
pub const CMD_CONFIG: i64 = 1;
pub const CMD_TETHERED: i64 = 2;
pub const CMD_STOP: i64 = 4;
pub const CMD_NO_NET: i64 = 5;
pub const CMD_ERROR: i64 = 6;

const AUTH_WPA2_PSK: i64 = 1;
const VER: &str = "1.0.0";

#[derive(Clone, PartialEq, Eq)]
pub struct HotspotConfig {
    pub ssid: String,
    pub pwd: String,
}

impl fmt::Debug for HotspotConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HotspotConfig")
            .field("ssid", &self.ssid)
            .field("pwd", &"<redacted>")
            .finish()
    }
}

impl HotspotConfig {
    /// A fresh SSID/passphrase for one hotspot session.
    pub fn generate() -> Self {
        let mut rng = rand::thread_rng();
        let tag: String = (0..4).map(|_| rng.sample(Alphanumeric) as char).collect();
        let pwd: String = (0..16).map(|_| rng.sample(Alphanumeric) as char).collect();
        Self {
            ssid: format!("SmartConnect-{tag}"),
            pwd,
        }
    }

    /// `cmd 1` reply carrying the SoftAP config the phone should start.
    pub fn config_msg(&self, fp: &str) -> Value {
        json!({
            "ver": VER, "cmd": CMD_CONFIG, "authType": AUTH_WPA2_PSK,
            "ssid": self.ssid, "pwd": self.pwd, "token": "", "wsPort": 0, "fp": fp,
        })
    }
}

/// What the phone's svc 13 message asks us to do.
#[derive(Debug, PartialEq, Eq)]
pub enum Event {
    Invite,
    Tethered,
    Stopped,
    NoNetwork,
    Error(i64),
    Other(i64),
}

pub fn parse(data: &Value) -> Option<Event> {
    let cmd = data.get("cmd").and_then(Value::as_i64)?;
    Some(match cmd {
        CMD_INVITE => Event::Invite,
        CMD_TETHERED => Event::Tethered,
        CMD_STOP => Event::Stopped,
        CMD_NO_NET => Event::NoNetwork,
        CMD_ERROR => Event::Error(data.get("errorCode").and_then(Value::as_i64).unwrap_or(0)),
        c => Event::Other(c),
    })
}

pub fn stop_msg() -> Value {
    json!({ "ver": VER, "cmd": CMD_STOP, "errorCode": 1313 })
}

/// Runs for the daemon's lifetime, handling svc 13 messages from whichever phone is connected.
pub async fn run(daemon: Arc<Daemon>) {
    let mut rx = daemon.from_phone.subscribe();
    let fp = daemon.identity.fp_hex();
    let mut current: Option<HotspotConfig> = None;
    let mut joined = false;
    loop {
        let (svc, data) = match rx.recv().await {
            Ok(m) => m,
            Err(RecvError::Lagged(n)) => {
                warn!("hotspot: dropped {n} phone messages");
                continue;
            }
            Err(RecvError::Closed) => return,
        };
        if svc != SERVICE_HOTSPOT {
            continue;
        }
        match parse(&data) {
            Some(Event::Invite) => {
                let cfg = HotspotConfig::generate();
                info!(ssid = %cfg.ssid, "hotspot: phone offers its hotspot, sending config");
                let _ = daemon.to_phone.send((SERVICE_HOTSPOT, cfg.config_msg(&fp)));
                current = Some(cfg);
                joined = false;
            }
            Some(Event::Tethered) => match &current {
                Some(cfg) => match join(cfg).await {
                    Ok(()) => {
                        info!(ssid = %cfg.ssid, "hotspot: joined phone hotspot");
                        joined = true;
                    }
                    Err(e) => {
                        warn!(ssid = %cfg.ssid, "hotspot: nmcli connect failed: {e}");
                        let _ = daemon.to_phone.send((SERVICE_HOTSPOT, stop_msg()));
                    }
                },
                None => warn!("hotspot: phone says tethered but we sent no config"),
            },
            Some(Event::Stopped) | Some(Event::Error(_)) => {
                if let Some(Event::Error(code)) = parse(&data) {
                    info!(code, "hotspot: phone cancelled");
                } else {
                    info!("hotspot: phone stopped tethering");
                }
                if let Some(cfg) = current.take()
                    && joined
                {
                    forget(&cfg).await;
                }
                joined = false;
            }
            Some(Event::NoNetwork) => {
                info!("hotspot: phone has no mobile data, hotspot not started")
            }
            Some(Event::Other(c)) => info!(cmd = c, "hotspot: unhandled cmd"),
            None => {}
        }
    }
}

/// `nmcli dev wifi connect <ssid> password <pwd>`; the passphrase stays out of logs and errors.
async fn join(cfg: &HotspotConfig) -> Result<(), String> {
    let _ = Command::new("nmcli")
        .args(["dev", "wifi", "rescan", "ssid", &cfg.ssid])
        .output()
        .await;
    // The SoftAP can take a few seconds to show up in scan results.
    let mut last = String::new();
    for _ in 0..5 {
        let out = Command::new("nmcli")
            .args([
                "--wait", "30", "dev", "wifi", "connect", &cfg.ssid, "password", &cfg.pwd,
            ])
            .output()
            .await
            .map_err(|e| format!("spawn nmcli: {e}"))?;
        if out.status.success() {
            return Ok(());
        }
        last = redact(&String::from_utf8_lossy(&out.stderr), &cfg.pwd);
        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    }
    Err(last.trim().to_string())
}

/// Drops the hotspot profile so NetworkManager falls back to the usual Wi-Fi.
async fn forget(cfg: &HotspotConfig) {
    let _ = Command::new("nmcli")
        .args(["connection", "delete", "id", &cfg.ssid])
        .output()
        .await;
}

fn redact(s: &str, pwd: &str) -> String {
    if pwd.is_empty() {
        s.to_string()
    } else {
        s.replace(pwd, "<redacted>")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_phone_cmds() {
        assert_eq!(parse(&json!({"ver":"1.0.0","cmd":0})), Some(Event::Invite));
        assert_eq!(
            parse(&json!({"ver":"1.0.0","cmd":2})),
            Some(Event::Tethered)
        );
        assert_eq!(
            parse(&json!({"ver":"1.0.0","cmd":4,"errorCode":1313})),
            Some(Event::Stopped)
        );
        assert_eq!(
            parse(&json!({"ver":"1.0.0","cmd":5,"netAvailable":1})),
            Some(Event::NoNetwork)
        );
        assert_eq!(
            parse(&json!({"ver":"1.0.0","cmd":6,"errorCode":1312})),
            Some(Event::Error(1312))
        );
        assert_eq!(parse(&json!({"ver":"1.0.0"})), None);
    }

    #[test]
    fn config_msg_shape_and_redaction() {
        let cfg = HotspotConfig::generate();
        assert_eq!(cfg.pwd.len(), 16);
        let m = cfg.config_msg("abcd");
        assert_eq!(m["cmd"], 1);
        assert_eq!(m["authType"], 1);
        assert_eq!(m["ssid"], cfg.ssid.as_str());
        assert_eq!(m["pwd"], cfg.pwd.as_str());
        assert_eq!(m["wsPort"], 0);
        assert!(!format!("{cfg:?}").contains(&cfg.pwd));
        assert_eq!(redact("bad key secret1", "secret1"), "bad key <redacted>");
    }
}
