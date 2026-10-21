//! Local control socket (`$XDG_RUNTIME_DIR/smartconnectd.sock`) for desktop UIs and the CLI.
//! One command per line; replies are JSON lines:
//! - `status`     → the current `PhoneStatus`
//! - `subscribe`  → the current status, then one line per change, until the client hangs up
//! - `connect`    → `{"ok":bool,"message":…}` (dial the phone's `/readyfor/mdns`)
//! - `disconnect` → `{"ok":bool,"message":…}`
//! - `pair`       → `{"ok":bool,"message":…,"qr":"<svg path>"}` (fresh invite for a new phone)
//! - `{"cmd":"watch","services":[…]}` → `{"service":N,"data":…}` per phone frame of those services;
//!   an empty list means every phone service. Daemon-local events use the reserved id
//!   [`crate::proto::SERVICE_LOCAL_EVENTS`] (-1) and are only sent when that id is listed, e.g.
//!   `{"service":-1,"data":{"event":"notification_action","kind":"action",…}}`
//!   (`notifications::click_event`).

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use serde_json::json;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::broadcast::error::RecvError;
use tracing::warn;

use crate::discovery;
use crate::server::Daemon;

pub fn socket_path() -> PathBuf {
    let dir = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
    dir.join("smartconnectd.sock")
}

pub async fn serve(daemon: Arc<Daemon>) -> Result<()> {
    crate::sms::spawn_collector(daemon.clone());
    let path = socket_path();
    let _ = std::fs::remove_file(&path);
    let listener = UnixListener::bind(&path).with_context(|| format!("binding {}", path.display()))?;
    loop {
        let (stream, _) = listener.accept().await?;
        let daemon = daemon.clone();
        tokio::spawn(async move {
            if let Err(e) = handle(daemon, stream).await {
                warn!("control: {e:#}");
            }
        });
    }
}

async fn handle(daemon: Arc<Daemon>, stream: UnixStream) -> Result<()> {
    let (rd, mut wr) = stream.into_split();
    let mut lines = BufReader::new(rd).lines();
    while let Some(line) = lines.next_line().await? {
        // JSON commands carry arguments: {"cmd":"reply","key":…,"text":…}
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) {
            // {"cmd":"watch","services":[1,15]}: stream {"service":N,"data":…} for those phone services.
            if v.get("cmd").and_then(serde_json::Value::as_str) == Some("watch") {
                let services: Vec<i64> = v
                    .get("services")
                    .and_then(serde_json::Value::as_array)
                    .map(|a| a.iter().filter_map(serde_json::Value::as_i64).collect())
                    .unwrap_or_default();
                let mut frames = daemon.from_phone.subscribe();
                wr.write_all(b"{\"ok\":true,\"message\":\"watching\"}\n").await?;
                loop {
                    match frames.recv().await {
                        Ok((svc, data)) if watched(&services, svc) => {
                            let line = json!({ "service": svc, "data": data });
                            wr.write_all(format!("{line}\n").as_bytes()).await?;
                        }
                        Ok(_) | Err(RecvError::Lagged(_)) => {}
                        Err(RecvError::Closed) => return Ok(()),
                    }
                }
            }
            let reply = json_command(&daemon, &v);
            wr.write_all(format!("{reply}\n").as_bytes()).await?;
            continue;
        }
        let reply = match line.trim() {
            "" => continue,
            "status" => daemon.status.snapshot(),
            "subscribe" => {
                let mut changes = daemon.status.subscribe();
                wr.write_all(format!("{}\n", daemon.status.snapshot()).as_bytes()).await?;
                loop {
                    match changes.recv().await {
                        Ok(json) => wr.write_all(format!("{json}\n").as_bytes()).await?,
                        Err(RecvError::Lagged(_)) => {
                            wr.write_all(format!("{}\n", daemon.status.snapshot()).as_bytes()).await?
                        }
                        Err(RecvError::Closed) => return Ok(()),
                    }
                }
            }
            "connect" => {
                let (ok, message) = discovery::connect_now(&daemon).await;
                if !ok {
                    // The user wants to be connected: keep trying in the background (backoff,
                    // presence probes, Bluetooth-on / network-up nudges).
                    daemon.spawn_auto_reconnect("Connect didn't get through yet");
                }
                json!({ "ok": ok, "message": message }).to_string()
            }
            "disconnect" => {
                let ok = daemon.disconnect();
                let message = if ok { "disconnected" } else { "not connected" };
                json!({ "ok": ok, "message": message }).to_string()
            }
            "pair" => match daemon.new_pairing_qr() {
                Ok(qr) => json!({ "ok": true, "message": "scan this QR in Smart Connect", "qr": qr }).to_string(),
                Err(e) => json!({ "ok": false, "message": format!("{e:#}") }).to_string(),
            },
            other => json!({ "ok": false, "message": format!("unknown command {other:?}") }).to_string(),
        };
        wr.write_all(format!("{reply}\n").as_bytes()).await?;
    }
    Ok(())
}

/// `watch` filter: the listed services, or every phone service for an empty list. Daemon-local
/// event ids (negative) are only delivered when listed explicitly.
fn watched(services: &[i64], svc: i64) -> bool {
    services.contains(&svc) || (services.is_empty() && svc >= 0)
}

fn json_command(daemon: &Daemon, v: &serde_json::Value) -> String {
    let s = |k: &str| v.get(k).and_then(serde_json::Value::as_str).unwrap_or_default();
    match s("cmd") {
        "reply" => {
            let (key, text) = (s("key"), s("text"));
            if !daemon.is_connected() {
                return json!({ "ok": false, "message": "phone not connected" }).to_string();
            }
            if key.is_empty() || text.trim().is_empty() {
                return json!({ "ok": false, "message": "empty reply" }).to_string();
            }
            let svc = daemon.status.notification_service(key).unwrap_or(crate::notifications::SERVICE_NOTIFY);
            let _ = daemon.to_phone.send((svc, crate::notifications::reply_command(key, text)));
            json!({ "ok": true, "message": "reply sent" }).to_string()
        }
        // {"cmd":"notif_mute","pkg":"com.example","mute":true|false}: per-app mute list (no pkg: list it).
        "notif_mute" => crate::notifications::mute_command(v).to_string(),
        // {"cmd":"send","service":N,"data":{…}}: forward a service message to the phone.
        "send" => {
            let (Some(svc), Some(data)) = (v.get("service").and_then(serde_json::Value::as_i64), v.get("data")) else {
                return json!({ "ok": false, "message": "send needs service and data" }).to_string();
            };
            // A helper's svc 25 `cap` (e.g. cross control's in-use bit for "26") would replace the
            // phone's whole tile map: merge it into the daemon's map and send the full map instead.
            if svc == crate::proto::SERVICE_DEVICE_STATUS {
                if let Some(cap) = data.get("cap").and_then(serde_json::Value::as_object) {
                    daemon.cap_overrides.lock().unwrap().extend(cap.iter().map(|(k, v)| (k.clone(), v.clone())));
                    if daemon.is_connected() {
                        let _ = daemon.to_phone.send((svc, daemon.device_status()));
                    }
                    return json!({ "ok": true, "message": "merged into the device status map" }).to_string();
                }
            }
            if !daemon.is_connected() {
                return json!({ "ok": false, "message": "phone not connected" }).to_string();
            }
            // A notification click sent by a desktop client: announce it like the daemon's own clicks.
            if let Some(ev) = crate::notifications::click_event(svc, data) {
                let _ = daemon.from_phone.send((crate::proto::SERVICE_LOCAL_EVENTS, ev));
            }
            let _ = daemon.to_phone.send((svc, data.clone()));
            json!({ "ok": true, "message": "sent" }).to_string()
        }
        // SMS (svc 204, see sms.rs). Requests are fire-and-forget; data lands in `sms_state`.
        "sms_state" => crate::sms::store().lock().unwrap().snapshot().to_string(),
        "sms_threads" | "sms_thread" | "sms_send" | "sms_mark_read" if !daemon.is_connected() => {
            json!({ "ok": false, "message": "phone not connected" }).to_string()
        }
        // {"cmd":"sms_threads"}: (re)sync SMS from the phone; returns the cached conversation list.
        "sms_threads" => {
            crate::sms::request_sync(daemon);
            let st = crate::sms::store().lock().unwrap();
            json!({ "ok": true, "enabled": st.enabled(), "threads": st.threads() }).to_string()
        }
        // {"cmd":"sms_thread","thread":"<address or thread key>"}: cached messages, oldest first.
        "sms_thread" => {
            let key = [s("thread"), s("number")].into_iter().find(|k| !k.is_empty()).unwrap_or_default();
            if key.is_empty() {
                return json!({ "ok": false, "message": "sms_thread needs thread" }).to_string();
            }
            let _ = daemon.to_phone.send((crate::sms::SERVICE_MESSAGE, crate::sms::request_messages(crate::sms::KIND_SMS)));
            let st = crate::sms::store().lock().unwrap();
            json!({ "ok": true, "thread": crate::sms::thread_key(key), "messages": st.thread(key) }).to_string()
        }
        // {"cmd":"sms_send","number":"+91…"|"thread":"<key>","text":"…"}
        "sms_send" => {
            let text = s("text");
            let to = if !s("number").is_empty() {
                s("number").to_owned()
            } else {
                let st = crate::sms::store().lock().unwrap();
                st.thread(s("thread")).last().map(|m| m.counterpart().to_owned()).unwrap_or_default()
            };
            if to.is_empty() || text.trim().is_empty() {
                return json!({ "ok": false, "message": "sms_send needs number (or known thread) and text" }).to_string();
            }
            static NEXT_REQ: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(1);
            let req = (std::process::id() as i64 % 1000) * 100_000 + NEXT_REQ.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let _ = daemon.to_phone.send((crate::sms::SERVICE_MESSAGE, crate::sms::send_sms(req, &[to.clone()], text, -1)));
            json!({ "ok": true, "message": "sms handed to phone", "requestId": req, "to": to }).to_string()
        }
        // {"cmd":"sms_mark_read","id":N} (SMS id from sms_thread)
        "sms_mark_read" => {
            let Some(id) = v.get("id").and_then(serde_json::Value::as_i64) else {
                return json!({ "ok": false, "message": "sms_mark_read needs id" }).to_string();
            };
            let _ = daemon.to_phone.send((crate::sms::SERVICE_MESSAGE, crate::sms::mark_read(id, crate::sms::KIND_SMS, true)));
            json!({ "ok": true, "message": "sent" }).to_string()
        }
        other => json!({ "ok": false, "message": format!("unknown command {other:?}") }).to_string(),
    }
}

/// Client side: send one command to the running daemon and print its JSON answer
/// (`subscribe` keeps printing until interrupted).
pub async fn request(cmd: &str) -> Result<()> {
    let path = socket_path();
    let stream = UnixStream::connect(&path)
        .await
        .with_context(|| format!("is smartconnectd running? ({})", path.display()))?;
    let (rd, mut wr) = stream.into_split();
    wr.write_all(format!("{cmd}\n").as_bytes()).await?;
    let mut lines = BufReader::new(rd).lines();
    while let Some(line) = lines.next_line().await? {
        println!("{line}");
        if cmd != "subscribe" {
            break;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::watched;
    use crate::proto::SERVICE_LOCAL_EVENTS;

    #[test]
    fn local_events_only_when_listed() {
        assert!(watched(&[], 15), "empty list = every phone service");
        assert!(!watched(&[], SERVICE_LOCAL_EVENTS), "local events are opt-in");
        assert!(watched(&[15, SERVICE_LOCAL_EVENTS], SERVICE_LOCAL_EVENTS));
        assert!(watched(&[1, 15], 15));
        assert!(!watched(&[1, 15], 5));
    }
}
