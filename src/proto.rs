//! Control-channel wire types. Everything is UTF-8 JSON in WebSocket text frames.

use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

pub const SERVICE_MANAGER: i64 = 0;
/// `SERVICE_ID_DEVICE_PAIRED` in the Windows UI; put in the QR's `wsData.serviceId`.
pub const SERVICE_DEVICE_PAIRED: i64 = 19;

/// `serviceVersions` advertised in the verify reply (top level and in `device`). The phone caches
/// it and only brings a service up when the PC lists it (no entry for 9 = no clipboard NEGO), so
/// list only services the daemon answers. Versions are the Windows 09.0.3.013 values.
pub fn service_versions() -> Value {
    json!({ "0": 3, "1": 10, "2": 5, "3": 3, "9": 4, "13": 1, "15": 16, "22": 2, "25": 1, "26": 1, "202": 1 })
}

/// Per-service capability levels advertised as `device.caps` (Windows sends 3 for clipboard).
pub fn service_caps() -> Value {
    json!({ "1": 2, "2": 3, "9": 3, "15": 3, "16": 3, "26": 2, "202": 1 })
}

/// Service 25 (device status) id.
pub const SERVICE_DEVICE_STATUS: i64 = 25;

/// Per-service flags the PC reports in its svc 25 `cmd 1` `cap` map. The phone only draws a
/// feature tile on the device page when the service is in BOTH `serviceVersions` (verify reply)
/// AND this map (`DeviceStatusServiceHelper.L`, persisted as `sp_latest_device_cap`). Bits, from
/// the phone's own map: 1 = on, 2 = extra mode, 8 = busy with another device (tile shows
/// "in use"), 16 = turned off by the user. Each message replaces the whole map for this PC.
/// Keep only services the daemon answers (see evidence `device-tiles-spec.md`): 26 = Cross Control, 22 = PC lock, 13 = Hotspot,
/// 3 = Webcam (answered by the `smartconnect-webcam` helper, see `webcam.rs`).
/// "202" (phone as a display) is deliberately left out: the phone merges 26 and 202 into one Cross
/// Control tile that relaunches whichever of the two last connected, and with "202" here a tap
/// after any display session went straight to Extend (`QuickSettingRepository`
/// `startLastUseUCExtend`). Without it the tile always sends svc 26 cmd 0: the daemon arms shared
/// mouse and keyboard and the GTK app opens its Display tab, where Duplicate/Extend are.
/// "1" = Mobile desktop + Screen share tiles (answered by `smartconnect-mirror --host`, see
/// `.notes/phone-view-invites.md`); on by default since Mobile desktop was used from the phone.
pub fn device_status_caps() -> Value {
    caps_with_extra(EXTRA_TILES.get().map(Vec::as_slice).unwrap_or_default())
}

fn caps_with_extra(extra: &[(String, i64)]) -> Value {
    let mut cap = json!({ "1": 1, "2": 1, "3": 1, "13": 1, "15": 1, "22": 1, "26": 1 });
    if let Some(map) = cap.as_object_mut() {
        map.extend(extra.iter().map(|(svc, flags)| (svc.clone(), json!(flags))));
    }
    cap
}

/// Extra svc 25 `cap` entries from `--extra-tiles`, set once at startup.
static EXTRA_TILES: std::sync::OnceLock<Vec<(String, i64)>> = std::sync::OnceLock::new();

/// Adds tiles to every svc 25 map this daemon sends (`--extra-tiles`). Call once, before serving.
pub fn set_extra_tiles(tiles: Vec<(String, i64)>) {
    let _ = EXTRA_TILES.set(tiles);
}

/// Parses `--extra-tiles` values: `SVC` (flags 1 = "available") or `SVC=FLAGS`, e.g. `1` or `1=3`.
/// A tile still only shows when the service is also in [`service_versions`].
pub fn parse_extra_tiles(specs: &[String]) -> Result<Vec<(String, i64)>, String> {
    specs
        .iter()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(|s| {
            let (svc, flags) = s.split_once('=').map_or((s, "1"), |(a, b)| (a.trim(), b.trim()));
            let svc: u32 = svc.parse().map_err(|_| format!("--extra-tiles: {s:?} is not a service id"))?;
            let flags: i64 = flags.parse().map_err(|_| format!("--extra-tiles: bad flags in {s:?}"))?;
            if !(0..=255).contains(&flags) {
                return Err(format!("--extra-tiles: flags in {s:?} must be 0-255"));
            }
            Ok((svc.to_string(), flags))
        })
        .collect()
}

/// Daemon-local events on the control socket's `watch` stream, e.g. notification clicks for the
/// stream host (`notifications::click_event`). Never sent to or received from the phone; delivered
/// only to watchers that list this id explicitly.
pub const SERVICE_LOCAL_EVENTS: i64 = -1;

/// Svc 25 `cmd 1` payload (`RemoteDeviceStatusData` on the phone). Send once after the verify
/// reply, and again whenever the map changes. `cap` is `device_status_caps()` with `overrides`
/// (per-service flags set by helpers, e.g. cross control's in-use bit) applied on top: the phone
/// replaces its whole map with each message, so it must always be complete. The phone also reads
/// `battery`, `deviceName`, `model` and `realDeviceName`.
pub fn device_status_msg(device_name: &str, battery: i64, overrides: &serde_json::Map<String, Value>) -> Value {
    let mut cap = device_status_caps();
    if let Some(map) = cap.as_object_mut() {
        map.extend(overrides.iter().map(|(k, v)| (k.clone(), v.clone())));
    }
    json!({
        "ver": "1",
        "cmd": 1,
        "cap": cap,
        "battery": battery,
        "deviceName": device_name,
        "realDeviceName": device_name,
        "model": "Linux PC",
    })
}

/// Envelope (`framework/websocket.WebsocketMsg` on Windows, `WsData` on the phone).
/// The phone additionally sends `ver` and `senderId`; Windows ignores both.
#[derive(Debug, Serialize, Deserialize)]
pub struct WsMsg {
    #[serde(rename = "deviceId", default, skip_serializing_if = "String::is_empty")]
    pub device_id: String,
    #[serde(rename = "srcServiceId", default)]
    pub src_service_id: i64,
    #[serde(rename = "serviceId", default)]
    pub service_id: i64,
    #[serde(rename = "msgId", default, skip_serializing_if = "is_zero")]
    pub msg_id: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ver: Option<Value>,
    #[serde(rename = "senderId", default, skip_serializing_if = "Option::is_none")]
    pub sender_id: Option<i64>,
}

fn is_zero(v: &u64) -> bool {
    *v == 0
}

static NEXT_MSG_ID: AtomicU64 = AtomicU64::new(1);

impl WsMsg {
    pub fn outgoing(device_id: &str, service_id: i64, data: Value) -> Self {
        Self {
            device_id: device_id.to_owned(),
            src_service_id: service_id,
            service_id,
            msg_id: NEXT_MSG_ID.fetch_add(1, Ordering::Relaxed),
            data: Some(data),
            ver: None,
            sender_id: None,
        }
    }
}

/// The fields the PC needs from the phone's first message (`R4Data`).
pub struct Hello<'a> {
    pub token: Option<&'a str>,
    pub cmd: Option<i64>,
    pub paired: Option<i64>,
    pub device_name: Option<&'a str>,
    pub real_device_id: Option<&'a str>,
}

impl<'a> Hello<'a> {
    pub fn parse(data: &'a Value) -> Self {
        let device = data.get("device");
        Self {
            token: data.get("token").and_then(Value::as_str),
            cmd: data.get("cmd").and_then(Value::as_i64),
            paired: data.get("paired").and_then(Value::as_i64),
            device_name: device.and_then(|d| d.get("deviceName")).and_then(Value::as_str),
            real_device_id: device.and_then(|d| d.get("realDeviceId")).and_then(Value::as_str),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_status_keeps_every_tile_when_a_helper_overrides_one() {
        let mut overrides = serde_json::Map::new();
        overrides.insert("26".into(), json!(9));
        let msg = device_status_msg("PC", 80, &overrides);
        assert_eq!(msg["cap"]["26"], 9);
        assert_eq!(msg["cap"]["15"], 1, "the other tiles stay");
        assert_eq!(msg["cap"].as_object().unwrap().len(), device_status_caps().as_object().unwrap().len());
    }

    #[test]
    fn extra_tiles_parse_and_merge() {
        let specs = vec!["20".to_owned(), " 2=3 ".to_owned(), String::new()];
        let tiles = parse_extra_tiles(&specs).unwrap();
        assert_eq!(tiles, vec![("20".to_owned(), 1), ("2".to_owned(), 3)]);
        let cap = caps_with_extra(&tiles);
        assert_eq!(cap["20"], 1);
        assert_eq!(cap["2"], 3);
        assert_eq!(cap["26"], 1, "the default tiles stay");
        assert!(cap.get("202").is_none(), "the Cross Control tile must not relaunch Extend by itself");
        assert_eq!(caps_with_extra(&[])["1"], 1, "Screen share and Mobile desktop are on by default");
        assert!(parse_extra_tiles(&["x".to_owned()]).is_err());
        assert!(parse_extra_tiles(&["1=-1".to_owned()]).is_err());
        assert!(parse_extra_tiles(&["1=abc".to_owned()]).is_err());
    }
}
