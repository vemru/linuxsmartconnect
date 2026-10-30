//! Service 3 (Webcam tile): the phone's camera as a Linux webcam.
//!
//! The daemon only relays signalling. The media is a WebRTC session (H.264 over DTLS-SRTP/UDP)
//! straight from the phone to the `smartconnect-webcam` helper, which decodes it and feeds
//! v4l2loopback or a PipeWire camera node; see `webcam/` and `evidence-2026-10-20/webcam-spec.md`.
//!
//! What the daemon does itself: when the user taps Webcam on the phone, the phone sends svc 3
//! `cmd 0` with `isReply:0` and waits for the PC to agree. If no helper is running, `helpers.rs`
//! starts `smartconnect-webcam --answer`, which replies and runs the session. (The Windows app's
//! `/readyfor/webcam` route is a loopback-only WebSocket for its own UI; the phone never uses it.)

use serde_json::{Value, json};

pub const SERVICE_WEBCAM: i64 = 3;

/// Error code that makes the phone show "install the webcam driver on the PC".
pub const ERR_NO_DRIVER: i64 = 12;

/// The phone asking the PC to start the webcam (its tile was tapped): `cmd 0` with `isReply:0`.
/// `isReply:1` is the phone accepting a PC-initiated request, which the running helper handles.
pub fn is_phone_request(data: &Value) -> bool {
    let int = |v: Option<&Value>| {
        v.and_then(|v| {
            v.as_i64()
                .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
        })
    };
    int(data.get("cmd")) == Some(0) && int(data.pointer("/data/isReply")) == Some(0)
}

/// svc 3 `cmd 65535` (error) for the phone.
pub fn error_msg(code: i64) -> Value {
    json!({ "ver": "1", "cmd": 65535, "data": { "code": code, "cmd": 0 } })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognises_only_the_phones_own_request() {
        let req = json!({"ver":"1","cmd":0,"data":{"minL":1,"maxL":1,"heartbeat":1,"isReply":0,"lite":1}});
        assert!(is_phone_request(&req));
        let accept = json!({"ver":"1","cmd":0,"data":{"minL":1,"maxL":1,"heartbeat":1,"isReply":1,"lite":1}});
        assert!(!is_phone_request(&accept));
        assert!(!is_phone_request(
            &json!({"ver":"1","cmd":1,"data":{"isReply":0}})
        ));
        assert!(!is_phone_request(&json!({"ver":"1","cmd":0})));
    }

    #[test]
    fn error_shape() {
        assert_eq!(
            error_msg(ERR_NO_DRIVER),
            json!({"ver":"1","cmd":65535,"data":{"code":12,"cmd":0}})
        );
    }
}
