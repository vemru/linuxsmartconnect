//! Drives the `cmd=1001` File Station handshake through the running daemon's control socket.
//!
//! The daemon owns the phone's control WebSocket, so the helper asks it to carry the handshake:
//! it `send`s the `cmd=1001` frame on **serviceId 168** (File Station) and `watch`es service 168 for
//! the phone's `type:1` reply, which carries the phone's NFS port and server certificate
//! fingerprint. No daemon-specific command is needed — the generic `send`/`watch` suffice (the
//! generic send sets `srcServiceId == serviceId == 168`, which is exactly what File Station wants).
//! See `docs/protocol/file-exchange.md`.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

use crate::nfs::MountTarget;

/// WS service id of File Station on both the phone and the PC (OBSERVED on device).
const SERVICE_FILE_STATION: i64 = 168;
/// File Station handshake command.
const CMD_HANDSHAKE: i64 = 1001;

pub(crate) fn socket_path() -> PathBuf {
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

/// Performs the handshake and returns where/how to mount the phone. `our_fp` is our TLS cert
/// fingerprint (the phone pins it to accept our client certificate).
pub async fn handshake(our_fp: &str) -> Result<MountTarget> {
    let path = socket_path();

    // One connection streams the phone's frames (a `watch` connection serves only the watch loop).
    let watch = UnixStream::connect(&path).await.with_context(|| {
        format!(
            "connecting to the daemon at {} (is smartconnectd running?)",
            path.display()
        )
    })?;
    let mut watch = BufReader::new(watch);
    watch
        .get_mut()
        .write_all(b"{\"cmd\":\"watch\",\"services\":[168]}\n")
        .await
        .context("subscribing to File Station frames")?;

    // A second connection sends the request (the watch connection no longer reads commands).
    let handshake_json =
        json!({ "type": 0, "dt2type": 2, "nfs_dependency": 24, "fp": our_fp }).to_string();
    let frame = json!({
        "cmd": "send",
        "service": SERVICE_FILE_STATION,
        "data": { "ver": "1.0", "cmd": CMD_HANDSHAKE, "data": handshake_json },
    });
    let mut send = UnixStream::connect(&path)
        .await
        .context("opening a send connection to the daemon")?;
    send.write_all(format!("{frame}\n").as_bytes())
        .await
        .context("sending the File Station handshake")?;
    send.shutdown().await.ok();

    // Read frames until the phone's `type:1` server reply arrives (or we time out).
    let mut line = String::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        line.clear();
        let read = tokio::time::timeout_at(deadline, watch.read_line(&mut line))
            .await
            .map_err(|_| {
                anyhow::anyhow!("the phone did not reply to the File Station handshake in time")
            })?
            .context("reading from the daemon")?;
        if read == 0 {
            bail!("the daemon closed the control connection");
        }
        if let Some(target) = parse_reply(&line, our_fp) {
            return Ok(target);
        }
    }
}

/// What the daemon says about the phone (for the mount's reconnect loop).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PhoneState {
    /// The control socket does not answer (daemon stopped or restarting).
    DaemonDown,
    Unpaired,
    Disconnected,
    Connected,
}

/// Asks the daemon's `status` (blocking, short timeout). Called from the reconnect thread.
pub fn phone_state() -> PhoneState {
    use std::io::{BufRead, BufReader, Write};
    let ask = || -> std::io::Result<String> {
        let mut s = std::os::unix::net::UnixStream::connect(socket_path())?;
        s.set_read_timeout(Some(Duration::from_secs(3)))?;
        s.write_all(b"status\n")?;
        let mut line = String::new();
        BufReader::new(s).read_line(&mut line)?;
        Ok(line)
    };
    match ask() {
        Ok(line) => parse_state(&line),
        Err(_) => PhoneState::DaemonDown,
    }
}

fn parse_state(line: &str) -> PhoneState {
    let Ok(v) = serde_json::from_str::<Value>(line.trim()) else {
        return PhoneState::DaemonDown;
    };
    let flag = |k: &str| v.get(k).and_then(Value::as_bool).unwrap_or(false);
    match (flag("paired"), flag("connected")) {
        (_, true) => PhoneState::Connected,
        (true, false) => PhoneState::Disconnected,
        (false, false) => PhoneState::Unpaired,
    }
}

/// Parses one watched frame; returns a [`MountTarget`] if it is the phone's `type:1` server reply.
fn parse_reply(line: &str, _our_fp: &str) -> Option<MountTarget> {
    let frame: Value = serde_json::from_str(line.trim()).ok()?;
    // {"service":168,"data":{"ver":"1.0","cmd":1001,"data":"<handshake json string>"}}
    let inner = frame.pointer("/data/data")?.as_str()?;
    let hs: Value = serde_json::from_str(inner).ok()?;
    if hs.get("type").and_then(Value::as_i64) != Some(1) {
        return None;
    }
    let nfs_port = hs.get("nfsport").and_then(json_u16)?;
    let mount_port = hs.get("port").and_then(json_u16)?;
    let ip = hs.get("ip").and_then(Value::as_str)?.to_string();
    let fingerprint = hs.get("fp").and_then(Value::as_str)?.to_string();
    let path = hs
        .get("path")
        .and_then(Value::as_str)
        .filter(|p| !p.is_empty())
        .unwrap_or("/sdcard")
        .to_string();
    Some(MountTarget {
        ip,
        mount_port,
        nfs_port,
        fingerprint,
        path,
    })
}

/// A port is sometimes a JSON number and sometimes a string on this channel.
fn json_u16(v: &Value) -> Option<u16> {
    v.as_u64()
        .and_then(|n| u16::try_from(n).ok())
        .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
        .filter(|&p| p != 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_lines() {
        assert_eq!(
            parse_state(r#"{"connected":true,"paired":true}"#),
            PhoneState::Connected
        );
        assert_eq!(
            parse_state(r#"{"connected":false,"paired":true,"name":"x"}"#),
            PhoneState::Disconnected
        );
        assert_eq!(
            parse_state(r#"{"connected":false,"paired":false}"#),
            PhoneState::Unpaired
        );
        assert_eq!(parse_state("garbage"), PhoneState::DaemonDown);
    }
}
