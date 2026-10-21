//! Answers the phone's File Station handshake so it mounts the PC (the **PC→phone** direction).
//!
//! When a phone is connected it repeatedly auto-sends `cmd=1001` with `type:0` on serviceId 168 —
//! "you be the NFS server". We watch service 168 over the daemon control socket, and for each
//! `type:0` we:
//!   1. pin the phone's `fp` (its client-cert fingerprint) into the shared [`PinnedFps`] set so the
//!      TLS server will accept it, and
//!   2. reply `cmd=1001` with `type:1` carrying **our** cert fingerprint, this PC's LAN IP and the
//!      NFS listener port, forcing `dt2type:2` (SSL) so the phone connects over plain mutual-TLS TCP.
//!
//! The phone re-sends ~once a second, so we throttle: reply the first time we see a given phone `fp`
//! and then at most once every [`REPLY_INTERVAL`] (so a dropped reply is retried without flooding).
//! See `docs/protocol/file-exchange.md`.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::time::Instant;
use tracing::{info, warn};

use crate::daemon::socket_path;
use crate::server::PinnedFps;

/// WS service id of File Station on both the phone and the PC (OBSERVED on device).
const SERVICE_FILE_STATION: i64 = 168;
/// File Station handshake command.
const CMD_HANDSHAKE: i64 = 1001;
/// Don't re-reply to the same phone more often than this (it re-sends ~1/s).
const REPLY_INTERVAL: Duration = Duration::from_secs(5);

/// Everything the reply needs that doesn't change between frames.
pub struct ReplyParams {
    /// Our TLS cert fingerprint (uppercase hex) — the phone pins this to accept our server cert.
    pub our_fp: String,
    /// This PC's address on the phone's LAN (from the daemon's `pcAddress`, or overridden).
    pub ip: String,
    /// The single port advertised for portmap, NFS and the (unused, read-only) data channel.
    pub port: u16,
    /// The exported path label the phone will mount, e.g. `/PhoneShare`.
    pub label: String,
}

/// Builds the `type:1` handshake reply JSON object the phone expects. Forces `dt2type:2` (SSL) and
/// advertises the same port for portmap/NFS/data so one TLS listener serves everything.
pub fn build_reply(p: &ReplyParams) -> Value {
    let port = i64::from(p.port);
    let timestamp = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0);
    json!({
        "type": 1,
        "path": p.label,
        "port": port,
        "nfsport": port,
        "nfsDataPort": port,
        "fp": p.our_fp,
        "ip": p.ip,
        "nfs_version": 8,
        "nfs_capability": 25,
        "dt2type": 2,
        "deviceId": "smartconnect-linux",
        "timestamp": timestamp,
        "screen_state": true,
    })
}

/// Wraps the reply JSON in the control-socket `send` frame for serviceId 168.
fn send_frame(reply: &Value) -> Value {
    json!({
        "cmd": "send",
        "service": SERVICE_FILE_STATION,
        "data": { "ver": "1.0", "cmd": CMD_HANDSHAKE, "data": reply.to_string() },
    })
}

/// Reads this PC's LAN address from the daemon's status (`pcAddress`).
async fn daemon_pc_address() -> Option<String> {
    let mut stream = UnixStream::connect(socket_path()).await.ok()?;
    stream.write_all(b"status\n").await.ok()?;
    // The status reply is a single JSON line.
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line).await.ok()?;
    let v: Value = serde_json::from_str(line.trim()).ok()?;
    v.get("pcAddress").and_then(Value::as_str).filter(|s| !s.is_empty()).map(str::to_owned)
}

/// Resolves the PC's LAN IP: the explicit override, else the daemon's `pcAddress`, else a private
/// IPv4 of a local interface.
pub async fn resolve_ip(override_ip: Option<String>) -> Result<String> {
    if let Some(ip) = override_ip {
        return Ok(ip);
    }
    if let Some(ip) = daemon_pc_address().await {
        return Ok(ip);
    }
    local_private_ipv4().context(
        "could not determine this PC's LAN IP (daemon gave no pcAddress and no private IPv4 found) \
         — pass --ip",
    )
}

/// First non-loopback private IPv4 address of a local interface (fallback when the daemon is silent).
fn local_private_ipv4() -> Option<String> {
    let addrs = if_addrs::get_if_addrs().ok()?;
    addrs
        .into_iter()
        .filter_map(|i| match i.addr.ip() {
            std::net::IpAddr::V4(v4)
                if v4.is_private() && !v4.is_loopback() && !v4.is_link_local() =>
            {
                Some(v4.to_string())
            }
            _ => None,
        })
        .next()
}

/// Runs the handshake responder until the control connection drops or an error occurs. For each
/// phone `type:0` it pins the phone fp and (throttled) replies `type:1`.
pub async fn run(params: ReplyParams, pinned: PinnedFps) -> Result<()> {
    let path = socket_path();
    let watch = UnixStream::connect(&path).await.with_context(|| {
        format!("connecting to the daemon at {} (is smartconnectd running and the phone connected?)", path.display())
    })?;
    let mut watch = BufReader::new(watch);
    watch
        .get_mut()
        .write_all(b"{\"cmd\":\"watch\",\"services\":[168]}\n")
        .await
        .context("subscribing to File Station frames")?;

    info!(
        "sharing {:?} to the phone on {}:{} (fp {}…); waiting for the phone's File Station handshake",
        params.label,
        params.ip,
        params.port,
        &params.our_fp[..params.our_fp.len().min(8)],
    );

    let mut last_fp: Option<String> = None;
    let mut last_reply = Instant::now() - REPLY_INTERVAL;
    let mut line = String::new();
    loop {
        line.clear();
        let n = watch.read_line(&mut line).await.context("reading from the daemon control socket")?;
        if n == 0 {
            anyhow::bail!("the daemon closed the control connection");
        }
        let Some(phone_fp) = parse_type0_fp(&line) else {
            continue;
        };

        // Pin the phone's client cert so the TLS server accepts it.
        let is_new = {
            let mut set = pinned.write().unwrap();
            set.insert(phone_fp.clone())
        };
        if is_new {
            info!("pinned phone certificate fp {}…", &phone_fp[..phone_fp.len().min(8)]);
        }

        // Throttle: reply on a new phone, or once per REPLY_INTERVAL otherwise.
        let changed = last_fp.as_deref() != Some(phone_fp.as_str());
        if changed || last_reply.elapsed() >= REPLY_INTERVAL {
            let frame = send_frame(&build_reply(&params));
            match UnixStream::connect(&path).await {
                Ok(mut send) => {
                    if let Err(e) = send.write_all(format!("{frame}\n").as_bytes()).await {
                        warn!("sending the File Station reply failed: {e}");
                    } else {
                        send.shutdown().await.ok();
                        if changed {
                            info!("replied type:1 (SSL) — phone should now mount {:?}", params.label);
                        }
                    }
                }
                Err(e) => warn!("opening a send connection to the daemon failed: {e}"),
            }
            last_fp = Some(phone_fp);
            last_reply = Instant::now();
        }
    }
}

/// If `line` is a watched File Station frame carrying a `cmd=1001` `type:0` handshake, returns the
/// phone's `fp` (its client-cert fingerprint). Otherwise `None`.
fn parse_type0_fp(line: &str) -> Option<String> {
    let frame: Value = serde_json::from_str(line.trim()).ok()?;
    // {"service":168,"data":{"ver":"1.0","cmd":1001,"data":"<handshake json string>"}}
    if frame.pointer("/data/cmd").and_then(Value::as_i64) != Some(CMD_HANDSHAKE) {
        return None;
    }
    let inner = frame.pointer("/data/data")?.as_str()?;
    let hs: Value = serde_json::from_str(inner).ok()?;
    if hs.get("type").and_then(Value::as_i64) != Some(0) {
        return None;
    }
    hs.get("fp").and_then(Value::as_str).filter(|s| !s.is_empty()).map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reply_has_the_expected_shape() {
        let p = ReplyParams {
            our_fp: "ABCDEF".into(),
            ip: "192.168.1.20".into(),
            port: 2049,
            label: "/PhoneShare".into(),
        };
        let r = build_reply(&p);
        assert_eq!(r["type"], 1);
        assert_eq!(r["dt2type"], 2); // SSL forced
        assert_eq!(r["path"], "/PhoneShare");
        assert_eq!(r["fp"], "ABCDEF");
        assert_eq!(r["ip"], "192.168.1.20");
        // One port advertised for portmap, NFS and the data channel.
        assert_eq!(r["port"], 2049);
        assert_eq!(r["nfsport"], 2049);
        assert_eq!(r["nfsDataPort"], 2049);
        assert_eq!(r["nfs_version"], 8);
        assert_eq!(r["nfs_capability"], 25);
        assert_eq!(r["screen_state"], true);
    }

    #[test]
    fn send_frame_is_a_station_data_envelope() {
        let r = build_reply(&ReplyParams {
            our_fp: "FP".into(),
            ip: "10.0.0.2".into(),
            port: 40000,
            label: "/PhoneShare".into(),
        });
        let f = send_frame(&r);
        assert_eq!(f["cmd"], "send");
        assert_eq!(f["service"], 168);
        assert_eq!(f["data"]["cmd"], 1001);
        assert_eq!(f["data"]["ver"], "1.0");
        // The handshake JSON rides as a (double-encoded) string.
        let inner: Value = serde_json::from_str(f["data"]["data"].as_str().unwrap()).unwrap();
        assert_eq!(inner["type"], 1);
        assert_eq!(inner["nfsport"], 40000);
    }

    #[test]
    fn parses_phone_type0_fp_and_ignores_others() {
        let frame = json!({
            "service": 168,
            "data": { "ver": "1.0", "cmd": 1001,
                      "data": json!({"type":0,"dt2type":3,"fp":"PHONEFP","nfs_version":8}).to_string() }
        });
        assert_eq!(parse_type0_fp(&frame.to_string()).as_deref(), Some("PHONEFP"));

        // type:1 (a server reply, not a request) is ignored.
        let reply = json!({
            "service": 168,
            "data": { "ver": "1.0", "cmd": 1001,
                      "data": json!({"type":1,"fp":"X"}).to_string() }
        });
        assert_eq!(parse_type0_fp(&reply.to_string()), None);

        // a different command is ignored.
        let other = json!({ "service": 168, "data": { "ver": "1.0", "cmd": 2003, "data": "{}" } });
        assert_eq!(parse_type0_fp(&other.to_string()), None);
    }
}
