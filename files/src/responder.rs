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
//!
//! Two more duties once the phone has mounted:
//! - **Network check.** After an I/O error the phone's NFS client sends `cmd=2006` and unmounts
//!   unless `cmd=2007` `{port, nfsport}` (the ports it mounted) arrives within 3 s. We answer it.
//! - **Pin memory.** Accepted phone fingerprints are kept in `<state dir>/phone-fps` (the last
//!   [`MAX_REMEMBERED_FPS`]), so a restarted `serve` accepts the phone's reconnect before it has
//!   seen a fresh `type:0`.
//!
//! See `docs/protocol/file-exchange.md`.

use std::path::{Path, PathBuf};
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
/// The phone's NFS client asks whether the server is still there (after an I/O error).
const CMD_NETWORK_REQUEST: i64 = 2006;
/// Our answer: the ports the phone mounted. Without it the phone unmounts after 3 s.
const CMD_NETWORK_RESPONSE: i64 = 2007;
/// How many phone fingerprints `phone-fps` keeps (the phone's NFS cert changes when its app
/// restarts, so old ones go stale).
const MAX_REMEMBERED_FPS: usize = 4;
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
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
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
    v.get("pcAddress")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
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
/// phone `type:0` it pins the phone fp and (throttled) replies `type:1`; it answers `cmd=2006`
/// network checks with `cmd=2007`. `state_dir` holds the remembered phone fingerprints.
pub async fn run(params: ReplyParams, pinned: PinnedFps, state_dir: &Path) -> Result<()> {
    let path = socket_path();
    let watch = UnixStream::connect(&path).await.with_context(|| {
        format!(
            "connecting to the daemon at {} (is smartconnectd running and the phone connected?)",
            path.display()
        )
    })?;
    let mut watch = BufReader::new(watch);
    watch
        .get_mut()
        .write_all(b"{\"cmd\":\"watch\",\"services\":[168]}\n")
        .await
        .context("subscribing to File Station frames")?;

    let store = FpStore::new(state_dir);
    let remembered = store.load();
    if !remembered.is_empty() {
        pinned.write().unwrap().extend(remembered.iter().cloned());
        info!(
            "accepting {} remembered phone certificate(s) until the phone handshakes",
            remembered.len()
        );
    }

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
        let n = watch
            .read_line(&mut line)
            .await
            .context("reading from the daemon control socket")?;
        if n == 0 {
            anyhow::bail!("the daemon closed the control connection");
        }
        let phone_fp = match parse_frame(&line) {
            Some(Incoming::Handshake { fp }) => fp,
            Some(Incoming::NetworkRequest) => {
                // The phone saw an I/O error; tell it the server it mounted is still here.
                send(&path, &network_response_frame(params.port)).await;
                info!("answered the phone's network check (cmd 2007)");
                continue;
            }
            None => continue,
        };

        // Pin the phone's client cert so the TLS server accepts it.
        let is_new = {
            let mut set = pinned.write().unwrap();
            set.insert(phone_fp.clone())
        };
        if is_new {
            info!(
                "pinned phone certificate fp {}…",
                &phone_fp[..phone_fp.len().min(8)]
            );
        }
        if last_fp.as_deref() != Some(phone_fp.as_str()) {
            store.remember(&phone_fp);
        }

        // Throttle: reply on a new phone, or once per REPLY_INTERVAL otherwise.
        let changed = last_fp.as_deref() != Some(phone_fp.as_str());
        if changed || last_reply.elapsed() >= REPLY_INTERVAL {
            if send(&path, &send_frame(&build_reply(&params))).await && changed {
                info!(
                    "replied type:1 (SSL) — phone should now mount {:?}",
                    params.label
                );
            }
            last_fp = Some(phone_fp);
            last_reply = Instant::now();
        }
    }
}

/// Sends one control-socket frame on a fresh connection. Returns whether it was written.
async fn send(path: &Path, frame: &Value) -> bool {
    match UnixStream::connect(path).await {
        Ok(mut s) => {
            if let Err(e) = s.write_all(format!("{frame}\n").as_bytes()).await {
                warn!("sending a File Station frame failed: {e}");
                return false;
            }
            s.shutdown().await.ok();
            true
        }
        Err(e) => {
            warn!("opening a send connection to the daemon failed: {e}");
            false
        }
    }
}

/// The `cmd=2007` answer: the same single port we advertised for portmap and NFS, so the phone's
/// "same server?" check passes.
fn network_response_frame(port: u16) -> Value {
    let body = json!({ "port": port, "nfsport": port });
    json!({
        "cmd": "send",
        "service": SERVICE_FILE_STATION,
        "data": { "ver": "1.0", "cmd": CMD_NETWORK_RESPONSE, "data": body.to_string() },
    })
}

/// Service 25 (device status), whose `cap` map decides which feature tiles the phone draws.
const SERVICE_DEVICE_STATUS: i64 = 25;
/// The `cap` key for the phone's "File explorer" tile (local service 169, gated on PC service 20).
const CAP_FILE_EXPLORER: &str = "20";
/// The `cap` key that lists this PC under "Files" in Smart Connect's Files tab. The phone drops a
/// PC (scType 2, verCode ≥ 900001000) from that list unless `"206"` is present and non-zero.
const CAP_FILES_TAB: &str = "206";

/// Announces the share to the phone's UI while we run (`on`), or withdraws it: the "File explorer"
/// tile (`"20"`: 1 = available, 16 = turned off) and the entry in the Files tab (`"206"`: 1, or 0 to
/// drop it). The daemon merges a helper's `cap` into its own tile map, so only these keys change.
pub async fn announce_tile(on: bool) {
    send(&socket_path(), &tile_frame(on)).await;
}

fn tile_frame(on: bool) -> Value {
    let (explorer, files_tab) = if on { (1, 1) } else { (16, 0) };
    json!({
        "cmd": "send",
        "service": SERVICE_DEVICE_STATUS,
        "data": { "cap": { CAP_FILE_EXPLORER: explorer, CAP_FILES_TAB: files_tab } },
    })
}

/// A File Station frame the responder acts on.
#[derive(Debug, PartialEq)]
enum Incoming {
    /// `cmd=1001` `type:0`: the phone wants us to be its NFS server; `fp` is its client cert.
    Handshake { fp: String },
    /// `cmd=2006`: the phone's NFS client asks whether we're still there.
    NetworkRequest,
}

/// Classifies a watched service 168 frame
/// (`{"service":168,"data":{"ver":"1.0","cmd":N,"data":"<json string>"}}`).
fn parse_frame(line: &str) -> Option<Incoming> {
    let frame: Value = serde_json::from_str(line.trim()).ok()?;
    match frame.pointer("/data/cmd").and_then(Value::as_i64)? {
        CMD_HANDSHAKE => {
            let inner = frame.pointer("/data/data")?.as_str()?;
            let hs: Value = serde_json::from_str(inner).ok()?;
            if hs.get("type").and_then(Value::as_i64) != Some(0) {
                return None;
            }
            let fp = hs
                .get("fp")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())?;
            Some(Incoming::Handshake { fp: fp.to_owned() })
        }
        CMD_NETWORK_REQUEST => Some(Incoming::NetworkRequest),
        _ => None,
    }
}

/// The remembered phone fingerprints (`<state dir>/phone-fps`, newest last, mode 0600).
struct FpStore {
    path: PathBuf,
}

impl FpStore {
    fn new(state_dir: &Path) -> Self {
        Self {
            path: state_dir.join("phone-fps"),
        }
    }

    fn load(&self) -> Vec<String> {
        std::fs::read_to_string(&self.path)
            .map(|s| {
                s.lines()
                    .map(str::trim)
                    .filter(|l| !l.is_empty())
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Moves `fp` to the end of the list, keeping the newest [`MAX_REMEMBERED_FPS`].
    fn remember(&self, fp: &str) {
        let mut fps = self.load();
        fps.retain(|f| !f.eq_ignore_ascii_case(fp));
        fps.push(fp.to_owned());
        let skip = fps.len().saturating_sub(MAX_REMEMBERED_FPS);
        let text: String = fps[skip..].iter().map(|f| format!("{f}\n")).collect();
        if let Err(e) = crate::cert::write_private(&self.path, text.as_bytes()) {
            warn!("could not remember the phone certificate: {e:#}");
        }
    }
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
    fn tile_frame_only_touches_the_share_keys() {
        assert_eq!(
            tile_frame(true),
            json!({ "cmd": "send", "service": 25, "data": { "cap": { "20": 1, "206": 1 } } })
        );
        assert_eq!(
            tile_frame(false)["data"]["cap"],
            json!({ "20": 16, "206": 0 })
        );
    }

    #[test]
    fn network_request_gets_our_ports_back() {
        let req = json!({ "service": 168, "data": { "ver": "1.0", "cmd": 2006, "data": "{}" } });
        assert_eq!(
            parse_frame(&req.to_string()),
            Some(Incoming::NetworkRequest)
        );
        let f = network_response_frame(2049);
        assert_eq!(f["cmd"], "send");
        assert_eq!(f["service"], 168);
        assert_eq!(f["data"]["cmd"], 2007);
        let inner: Value = serde_json::from_str(f["data"]["data"].as_str().unwrap()).unwrap();
        assert_eq!(inner, json!({ "port": 2049, "nfsport": 2049 }));
    }

    #[test]
    fn fp_store_keeps_the_newest_few() {
        let dir = std::env::temp_dir().join(format!("scf-fps-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let store = FpStore::new(&dir);
        assert!(store.load().is_empty());
        for fp in ["A", "B", "C", "D", "E"] {
            store.remember(fp);
        }
        store.remember("c"); // seen again: moves to the end, case-insensitively
        assert_eq!(store.load(), ["B", "D", "E", "c"]);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn parses_phone_type0_fp_and_ignores_others() {
        let frame = json!({
            "service": 168,
            "data": { "ver": "1.0", "cmd": 1001,
                      "data": json!({"type":0,"dt2type":3,"fp":"PHONEFP","nfs_version":8}).to_string() }
        });
        assert_eq!(
            parse_frame(&frame.to_string()),
            Some(Incoming::Handshake {
                fp: "PHONEFP".into()
            })
        );

        // type:1 (a server reply, not a request) is ignored.
        let reply = json!({
            "service": 168,
            "data": { "ver": "1.0", "cmd": 1001,
                      "data": json!({"type":1,"fp":"X"}).to_string() }
        });
        assert_eq!(parse_frame(&reply.to_string()), None);

        // a different command is ignored.
        let other = json!({ "service": 168, "data": { "ver": "1.0", "cmd": 2003, "data": "{}" } });
        assert_eq!(parse_frame(&other.to_string()), None);
    }
}
