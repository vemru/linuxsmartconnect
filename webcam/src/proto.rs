//! Service 3 (webcam) messages. Every payload is `{"ver":"1","cmd":N,"data":{…}}` inside the normal
//! control-channel envelope. Protocol write-up: `evidence-2026-10-20/webcam-spec.md`.
//!
//! The phone is the WebRTC offerer and only sends video. Its SDP and ICE candidates travel as JSON
//! *strings* inside `data.webRTCData`; answers and candidates going back use the same shape.

use std::net::Ipv4Addr;

use serde_json::{Value, json};

pub const SERVICE: i64 = 3;
const VER: &str = "1";

pub const CMD_HANDSHAKE: i64 = 0;
pub const CMD_START: i64 = 1;
pub const CMD_STOP: i64 = 2;
/// Camera commands the phone hands to its camera screen (answered with a status, cmd 5).
pub const CMD_QUERY_CAMERAS: i64 = 3;
pub const CMD_SWITCH_CAMERA: i64 = 4;
pub const CMD_CAMERA_STATUS: i64 = 5;
pub const CMD_TOGGLE_TORCH: i64 = 7;
pub const CMD_ZOOM: i64 = 9;
pub const CMD_HEARTBEAT: i64 = 13;
pub const CMD_WEBRTC_ANSWER: i64 = 17;
pub const CMD_WEBRTC_CANDIDATE: i64 = 18;
pub const CMD_ERROR: i64 = 65535;

/// Error code the phone understands as "the PC has no webcam driver" (it shows an install hint).
pub const ERR_NO_DRIVER: i64 = 12;

/// The `port` the PC hands out in the handshake/start replies. In WebRTC mode the phone only echoes
/// it back in its start message; media goes wherever ICE decides. 6466 is the phone's own default.
pub const PORT: i64 = 6466;

fn msg(cmd: i64, data: Option<Value>) -> Value {
    let mut m = json!({ "ver": VER, "cmd": cmd });
    if let Some(d) = data {
        m["data"] = d;
    }
    m
}

/// PC-initiated start ("Start Webcam" on the PC). No `isReply` key: that is what marks it as a
/// request. The phone answers with its own cmd 0 carrying `isReply:1` (or cmd 65535 on refusal).
pub fn pc_request() -> Value {
    msg(
        CMD_HANDSHAKE,
        Some(json!({ "minL": 1, "maxL": 1, "heartbeat": 1, "lite": 0 })),
    )
}

/// Reply to the phone's own request (its tile was tapped): `isReply:1` plus a port starts it.
pub fn handshake_reply() -> Value {
    msg(
        CMD_HANDSHAKE,
        Some(json!({
            "minL": 1, "maxL": 1, "heartbeat": 1, "isReply": 1, "port": PORT,
            "supportedMediaFormat": 1, "supportedWebRTC": 1,
        })),
    )
}

/// After the phone accepted a PC-initiated request: "start streaming" (it needs `data.port`).
pub fn start() -> Value {
    msg(CMD_START, Some(json!({ "port": PORT, "heartbeat": 1 })))
}

pub fn stop() -> Value {
    msg(CMD_STOP, None)
}

/// The phone tears the session down when it hears nothing for 30 s.
pub fn heartbeat() -> Value {
    msg(CMD_HEARTBEAT, None)
}

pub fn error(code: i64) -> Value {
    msg(CMD_ERROR, Some(json!({ "code": code, "cmd": 0 })))
}

pub fn camera_command(cmd: i64, data: Option<Value>) -> Value {
    msg(cmd, data)
}

pub fn answer(session_id: i64, sdp: &str) -> Value {
    let inner = json!({ "type": "answer", "sdp": sdp }).to_string();
    msg(
        CMD_WEBRTC_ANSWER,
        Some(json!({ "sessionId": session_id, "type": "answer", "webRTCData": inner })),
    )
}

pub fn candidate(session_id: i64, mid: &str, mline: u32, candidate: &str) -> Value {
    let inner = json!({ "type": "candidate", "label": mline, "id": mid, "candidate": candidate })
        .to_string();
    msg(
        CMD_WEBRTC_CANDIDATE,
        Some(json!({ "sessionId": session_id, "type": "candidate", "webRTCData": inner })),
    )
}

/// What a phone svc 3 payload means for us.
#[derive(Debug, PartialEq)]
pub enum Phone {
    /// cmd 0. `is_reply == Some(0)`: the phone asks (tile tapped); `Some(1)`: it accepted our request.
    Handshake {
        is_reply: Option<i64>,
    },
    /// cmd 1 with the WebRTC offer; `phone_ip` is the address the phone uses on the LAN.
    Start {
        offer_sdp: Option<String>,
        phone_ip: Option<String>,
    },
    Stop,
    Candidate {
        session_id: Option<i64>,
        mid: String,
        mline: u32,
        candidate: String,
    },
    Heartbeat,
    /// cmd 5 (camera state: facing, torch, zoom…) or cmd 3 (camera list).
    CameraState(Value),
    Error {
        code: i64,
        about_cmd: i64,
    },
    Other(i64),
}

/// The phone sends some payloads (camera replies) as a JSON-encoded string instead of an object.
fn normalize(data: &Value) -> Value {
    match data {
        Value::String(s) => serde_json::from_str(s).unwrap_or(Value::Null),
        v => v.clone(),
    }
}

fn int(v: &Value) -> Option<i64> {
    v.as_i64()
        .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
}

pub fn parse(payload: &Value) -> Option<Phone> {
    let payload = normalize(payload);
    let cmd = payload.get("cmd").and_then(int)?;
    let data = payload.get("data").map(normalize).unwrap_or(Value::Null);
    Some(match cmd {
        CMD_HANDSHAKE => Phone::Handshake {
            is_reply: data.get("isReply").and_then(int),
        },
        CMD_START => Phone::Start {
            offer_sdp: data
                .get("webRTCData")
                .and_then(Value::as_str)
                .and_then(|s| serde_json::from_str::<Value>(s).ok())
                .and_then(|o| o.get("sdp").and_then(Value::as_str).map(str::to_owned)),
            phone_ip: data.get("url").and_then(Value::as_str).map(str::to_owned),
        },
        CMD_STOP => Phone::Stop,
        CMD_WEBRTC_CANDIDATE => {
            let c: Value = data
                .get("webRTCData")
                .and_then(Value::as_str)
                .and_then(|s| serde_json::from_str(s).ok())?;
            Phone::Candidate {
                session_id: data.get("sessionId").and_then(int),
                mid: c
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or("0")
                    .to_owned(),
                mline: c.get("label").and_then(int).unwrap_or(0) as u32,
                candidate: c.get("candidate").and_then(Value::as_str)?.to_owned(),
            }
        }
        CMD_HEARTBEAT => Phone::Heartbeat,
        CMD_CAMERA_STATUS | CMD_QUERY_CAMERAS => Phone::CameraState(payload.clone()),
        CMD_ERROR => Phone::Error {
            code: data.get("code").and_then(int).unwrap_or(-1),
            about_cmd: data.get("cmd").and_then(int).unwrap_or(-1),
        },
        other => Phone::Other(other),
    })
}

/// Human meaning of the phone's cmd 65535 codes.
pub fn error_text(code: i64) -> &'static str {
    match code {
        1 => "the phone's webcam is in use by another PC",
        5 => "the webcam feature is disabled by the phone's administrator",
        6 => "the user declined on the phone",
        7 => "the request was declined on the phone",
        8 => "not available for this Android user (multi-user)",
        9 => "the phone lacks a free video encoder",
        10 => "the phone's camera is already in use",
        _ => "unknown error",
    }
}

/// One-line summary of a camera status (cmd 5) for the log.
pub fn describe_camera_state(v: &Value) -> String {
    let d = &v["data"];
    if v.get("cmd").and_then(int) == Some(CMD_QUERY_CAMERAS) {
        return format!("cameras: {d}");
    }
    let facing = match d.get("cameraId").and_then(int) {
        Some(0) => "front",
        Some(1) => "back",
        _ => "?",
    };
    let torch = d.pointer("/flash/state").and_then(int).unwrap_or(0);
    let zoom = d.pointer("/zoom/state").and_then(int).unwrap_or(0);
    let zoom_max = d.pointer("/zoom/max").and_then(int).unwrap_or(0);
    format!(
        "camera: {facing}, torch {}, zoom {zoom}/{zoom_max}",
        if torch == 1 { "on" } else { "off" }
    )
}

/// Only offer our candidates on the phone's LAN: same /24 as the address it streams from, or any
/// private IPv4 when that is unknown. Docker/VPN/IPv6 candidates would just be tried and time out,
/// and the phone disables TCP candidates.
pub fn keep_local_candidate(candidate: &str, phone_ip: Option<Ipv4Addr>) -> bool {
    let f: Vec<&str> = candidate.split_whitespace().collect();
    if f.len() < 8 || !f[2].eq_ignore_ascii_case("udp") {
        return false;
    }
    let Ok(ip) = f[4].parse::<Ipv4Addr>() else {
        return false;
    };
    match phone_ip {
        Some(p) => ip.octets()[..3] == p.octets()[..3],
        None => ip.is_private(),
    }
}

/// Keeps only H.264 in the offer (the phone may list H.265 too) and drops the video-orientation
/// header extension, so the phone rotates frames itself instead of tagging them.
pub fn h264_only(sdp: &str) -> String {
    let lines: Vec<&str> = sdp
        .split("\r\n")
        .flat_map(|l| l.split('\n'))
        .filter(|l| !l.is_empty())
        .collect();
    let h264: Vec<&str> = lines
        .iter()
        .filter_map(|l| l.strip_prefix("a=rtpmap:"))
        .filter(|r| r.contains(" H264/90000"))
        .filter_map(|r| r.split(' ').next())
        .collect();
    if h264.is_empty() {
        return sdp.to_owned();
    }
    fn pt_of(l: &str) -> Option<&str> {
        ["a=rtpmap:", "a=fmtp:", "a=rtcp-fb:"]
            .iter()
            .find_map(|p| l.strip_prefix(p))
            .and_then(|r| r.split(' ').next())
    }
    let mut out = Vec::new();
    let mut in_video = false;
    for l in lines {
        if l.starts_with("m=") {
            in_video = l.starts_with("m=video");
        }
        if l.starts_with("a=extmap:") && l.contains("urn:3gpp:video-orientation") {
            continue;
        }
        if l.starts_with("m=video") {
            let parts: Vec<&str> = l.split(' ').collect();
            let keep: Vec<&str> = parts[3..]
                .iter()
                .copied()
                .filter(|pt| h264.contains(pt))
                .collect();
            out.push(format!("{} {}", parts[..3].join(" "), keep.join(" ")));
        } else if in_video && let Some(pt) = pt_of(l) {
            if h264.contains(&pt) {
                out.push(l.to_owned());
            }
        } else {
            out.push(l.to_owned());
        }
    }
    out.join("\r\n") + "\r\n"
}

/// `a=mid` of each m= section, in order (the ICE `label` is the index into this list).
/// The offer's `o=` session id and version (RFC 4566). A re-sent offer keeps both, a
/// renegotiation keeps the id and bumps the version, a new peer connection gets a new id.
pub fn sdp_origin(sdp: &str) -> Option<(String, u64)> {
    let o = sdp.lines().find_map(|l| l.trim_end().strip_prefix("o="))?;
    let mut f = o.split_whitespace();
    let _user = f.next()?;
    let id = f.next()?.to_owned();
    let version = f.next()?.parse().ok()?;
    Some((id, version))
}

#[derive(Debug, PartialEq, Eq)]
pub enum OfferKind {
    /// The offer we already answered, sent again (the phone re-sends its start message whenever
    /// its camera screen reports "streaming started", e.g. after a front/back switch).
    Repeat,
    /// Same peer connection, new offer: answer it on the existing webrtcbin.
    Renegotiation,
    /// A new peer connection (or the first offer).
    NewSession,
}

/// How to handle a `cmd 1` offer, given the origin of the offer already answered (if any).
pub fn classify_offer(
    answered: Option<&(String, u64)>,
    offer: Option<&(String, u64)>,
) -> OfferKind {
    match (answered, offer) {
        (Some((id, v)), Some((new_id, new_v))) if id == new_id && v == new_v => OfferKind::Repeat,
        (Some((id, _)), Some((new_id, _))) if id == new_id => OfferKind::Renegotiation,
        _ => OfferKind::NewSession,
    }
}

/// Stream quality: the phone always captures 1280x720 at 30 fps in this mode (no PC command
/// changes that), but it encodes at whatever bitrate the receiver allows. Each preset is
/// (min, start, max) in kbps, written into our answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Quality {
    Low,
    Medium,
    High,
    Max,
}

impl Quality {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "low" => Some(Self::Low),
            "medium" => Some(Self::Medium),
            "high" => Some(Self::High),
            "max" => Some(Self::Max),
            _ => None,
        }
    }

    pub fn kbps(self) -> (u32, u32, u32) {
        match self {
            Self::Low => (300, 800, 1500),
            Self::Medium => (800, 2000, 4000),
            Self::High => (1500, 4000, 8000),
            Self::Max => (2500, 8000, 15000),
        }
    }
}

/// Our answer with the quality's bitrates for the phone's encoder: `b=AS` (max, kbps) in the video
/// section and libwebrtc's `x-google-{min,start,max}-bitrate` on every H.264 `a=fmtp` line. Only
/// the copy sent to the phone carries them: they steer the sender, not our receiver.
pub fn with_bitrate(sdp: &str, q: Quality) -> String {
    let (min, start, max) = q.kbps();
    let params = format!(
        "x-google-min-bitrate={min};x-google-start-bitrate={start};x-google-max-bitrate={max}"
    );
    let lines: Vec<&str> = sdp
        .split("\r\n")
        .flat_map(|l| l.split('\n'))
        .filter(|l| !l.is_empty())
        .collect();
    let h264: Vec<String> = lines
        .iter()
        .filter_map(|l| l.strip_prefix("a=rtpmap:"))
        .filter(|r| r.contains(" H264/90000"))
        .filter_map(|r| r.split(' ').next().map(str::to_owned))
        .collect();
    let mut out: Vec<String> = Vec::with_capacity(lines.len() + 2);
    let mut in_video = false;
    let mut bandwidth_done = false;
    for l in lines {
        if l.starts_with("m=") {
            in_video = l.starts_with("m=video");
            bandwidth_done = false;
            out.push(l.to_owned());
            continue;
        }
        if in_video && l.starts_with("b=") {
            continue; // replaced below
        }
        if in_video && !bandwidth_done && l.starts_with("a=") {
            out.push(format!("b=AS:{max}"));
            bandwidth_done = true;
        }
        match l.strip_prefix("a=fmtp:").and_then(|r| r.split_once(' ')) {
            Some((pt, rest)) if in_video && h264.iter().any(|p| p == pt) => {
                let kept: Vec<&str> = rest
                    .split(';')
                    .filter(|p| !p.trim_start().starts_with("x-google-"))
                    .collect();
                out.push(format!("a=fmtp:{pt} {};{params}", kept.join(";")));
            }
            _ => out.push(l.to_owned()),
        }
    }
    out.join("\r\n") + "\r\n"
}

pub fn mids(sdp: &str) -> Vec<String> {
    let mut out = Vec::new();
    for l in sdp.lines() {
        if l.starts_with("m=") {
            out.push(out.len().to_string()); // default when the section has no a=mid
        } else if let Some(mid) = l.strip_prefix("a=mid:")
            && let Some(last) = out.last_mut()
        {
            *last = mid.trim().to_owned();
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repeated_offers_are_not_answered_again() {
        let a = "v=0\r\no=- 6009361234 2 IN IP4 127.0.0.1\r\ns=-\r\n";
        let b = "v=0\r\no=- 6009361234 3 IN IP4 127.0.0.1\r\ns=-\r\n";
        let c = "v=0\r\no=- 999 1 IN IP4 127.0.0.1\r\ns=-\r\n";
        let first = sdp_origin(a).unwrap();
        assert_eq!(first, ("6009361234".to_owned(), 2));
        assert_eq!(
            classify_offer(Some(&first), sdp_origin(a).as_ref()),
            OfferKind::Repeat
        );
        assert_eq!(
            classify_offer(Some(&first), sdp_origin(b).as_ref()),
            OfferKind::Renegotiation
        );
        assert_eq!(
            classify_offer(Some(&first), sdp_origin(c).as_ref()),
            OfferKind::NewSession
        );
        assert_eq!(
            classify_offer(None, sdp_origin(a).as_ref()),
            OfferKind::NewSession
        );
    }

    #[test]
    fn bitrate_goes_into_the_video_section_and_h264_fmtp() {
        let sdp = "v=0\r\no=- 1 2 IN IP4 0.0.0.0\r\nm=video 9 UDP/TLS/RTP/SAVPF 127\r\nc=IN IP4 0.0.0.0\r\n\
a=mid:0\r\na=rtpmap:127 H264/90000\r\na=fmtp:127 packetization-mode=1;profile-level-id=42e01f\r\n";
        let out = with_bitrate(sdp, Quality::High);
        assert!(
            out.contains("c=IN IP4 0.0.0.0\r\nb=AS:8000\r\na=mid:0"),
            "{out}"
        );
        assert!(out.contains("a=fmtp:127 packetization-mode=1;profile-level-id=42e01f;x-google-min-bitrate=1500;x-google-start-bitrate=4000;x-google-max-bitrate=8000"));
        // Idempotent: applying another preset replaces the old values.
        let again = with_bitrate(&out, Quality::Low);
        assert_eq!(again.matches("b=AS:").count(), 1);
        assert!(
            again.contains("b=AS:1500")
                && again.contains("x-google-max-bitrate=1500")
                && !again.contains("=8000")
        );
        assert_eq!(Quality::parse("MEDIUM"), Some(Quality::Medium));
        assert_eq!(Quality::parse("ultra"), None);
    }

    #[test]
    fn phone_request_and_acceptance() {
        let req = json!({"ver":"1","cmd":0,"data":{"minL":1,"maxL":1,"heartbeat":1,"isReply":0,"lite":1}});
        assert_eq!(parse(&req), Some(Phone::Handshake { is_reply: Some(0) }));
        let acc = json!({"ver":"1","cmd":0,"data":{"minL":1,"maxL":1,"heartbeat":1,"isReply":1,"lite":1}});
        assert_eq!(parse(&acc), Some(Phone::Handshake { is_reply: Some(1) }));
    }

    #[test]
    fn our_messages_have_the_shapes_the_phone_reads() {
        let r = handshake_reply();
        assert_eq!(r["cmd"], 0);
        assert_eq!(r["data"]["isReply"], 1);
        assert_eq!(r["data"]["heartbeat"], 1);
        assert!(r["data"]["port"].as_i64().unwrap() > 0);
        // A PC request must not carry isReply at all (the phone checks for the key).
        assert!(!pc_request().to_string().contains("isReply"));
        assert!(start()["data"]["port"].as_i64().is_some());
        assert_eq!(stop(), json!({"ver":"1","cmd":2}));
        let a = answer(7, "v=0");
        let inner: Value = serde_json::from_str(a["data"]["webRTCData"].as_str().unwrap()).unwrap();
        assert_eq!(inner, json!({"type":"answer","sdp":"v=0"}));
        let c = candidate(7, "0", 0, "candidate:1 1 udp 1 192.168.1.20 5000 typ host");
        let inner: Value = serde_json::from_str(c["data"]["webRTCData"].as_str().unwrap()).unwrap();
        assert_eq!(inner["id"], "0");
        assert_eq!(inner["label"], 0);
    }

    #[test]
    fn parses_start_with_offer() {
        let offer =
            json!({"sdp":"v=0\r\nm=video 9 UDP/TLS/RTP/SAVPF 96\r\n","type":"offer"}).to_string();
        let start = json!({"ver":"1","cmd":1,"data":{"prefix":"srtp://","url":"192.168.1.30","port":6466,"w":0,"h":0,
            "cameraId":0,"key":"","mediaFormat":0,"isWebRTC":1,"webRTCData":offer,"isVideoDump":0}});
        match parse(&start) {
            Some(Phone::Start {
                offer_sdp: Some(sdp),
                phone_ip,
            }) => {
                assert!(sdp.starts_with("v=0"));
                assert_eq!(phone_ip.as_deref(), Some("192.168.1.30"));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn parses_candidates_and_string_payloads() {
        let inner = json!({"type":"candidate","label":0,"id":"0","candidate":"candidate:9 1 udp 5 192.168.1.30 40000 typ host"});
        let c = json!({"ver":"1","cmd":18,"data":{"sessionId":42,"type":"candidate","webRTCData":inner.to_string()}});
        assert_eq!(
            parse(&c),
            Some(Phone::Candidate {
                session_id: Some(42),
                mid: "0".into(),
                mline: 0,
                candidate: "candidate:9 1 udp 5 192.168.1.30 40000 typ host".into()
            })
        );
        // Camera replies arrive as a JSON string.
        let status = Value::String(
            json!({"ver":1,"cmd":5,"data":{"cameraId":1,"zoom":{"ability":1,"state":2,"max":8}}})
                .to_string(),
        );
        match parse(&status) {
            Some(Phone::CameraState(v)) => assert_eq!(
                describe_camera_state(&v),
                "camera: back, torch off, zoom 2/8"
            ),
            other => panic!("{other:?}"),
        }
        let err = json!({"ver":"1","cmd":65535,"data":{"code":10,"cmd":0}});
        assert_eq!(
            parse(&err),
            Some(Phone::Error {
                code: 10,
                about_cmd: 0
            })
        );
    }

    #[test]
    fn candidate_filter() {
        let phone = Some(Ipv4Addr::new(192, 168, 1, 30));
        assert!(keep_local_candidate(
            "candidate:1 1 UDP 2015363327 192.168.1.20 41234 typ host",
            phone
        ));
        assert!(!keep_local_candidate(
            "candidate:2 1 UDP 2015363327 172.17.0.1 41234 typ host",
            phone
        ));
        assert!(!keep_local_candidate(
            "candidate:3 1 TCP 1015363327 192.168.1.20 9 typ host tcptype active",
            phone
        ));
        assert!(!keep_local_candidate(
            "candidate:4 1 UDP 2015363327 fe80::1 41234 typ host",
            phone
        ));
        assert!(keep_local_candidate(
            "candidate:5 1 UDP 2015363327 10.0.0.5 41234 typ host",
            None
        ));
        assert!(!keep_local_candidate(
            "candidate:6 1 UDP 2015363327 8.8.8.8 41234 typ host",
            None
        ));
    }

    #[test]
    fn trims_offer_to_h264_and_reads_mids() {
        let offer = "v=0\r\na=group:BUNDLE 0 1\r\nm=video 9 UDP/TLS/RTP/SAVPF 96 97 98\r\na=mid:0\r\n\
a=extmap:3 urn:3gpp:video-orientation\r\na=rtpmap:96 H265/90000\r\na=rtcp-fb:96 nack\r\n\
a=rtpmap:97 H264/90000\r\na=fmtp:97 profile-level-id=42e01f\r\na=rtpmap:98 rtx/90000\r\na=fmtp:98 apt=96\r\n\
m=application 9 UDP/DTLS/SCTP webrtc-datachannel\r\na=mid:1\r\n";
        let out = h264_only(offer);
        assert!(out.contains("m=video 9 UDP/TLS/RTP/SAVPF 97\r\n"));
        assert!(out.contains("a=rtpmap:97 H264/90000"));
        assert!(!out.contains("H265"));
        assert!(!out.contains("video-orientation"));
        assert!(!out.contains("a=fmtp:98"));
        assert!(out.contains("m=application"));
        assert_eq!(mids(&out), vec!["0".to_owned(), "1".to_owned()]);
    }
}
