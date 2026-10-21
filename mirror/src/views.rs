//! The phone's "views" (service 1 sessions): the whole-screen mirror, Ready For desktop on a
//! virtual display, and the virtual phone. Each runs as a service-15 stream whose package is the
//! view name instead of an app.
//!
//! Started on the PC: svc 1 cmd 1 `{view, reply: 0}`. Started on the phone (Screen share tile =
//! view 1, Mobile desktop tile = view 2): the phone sends svc 1 cmd 0 `{view, resolution?}` and waits
//! 20 s for our cmd 1; it takes the view from our cmd 1 (phone app, `service.q` + `zc7`), then
//! invites the stream with svc 15 cmd 2 `{pkg: "mirror"|"desktop"|"mobile"}`.

use serde_json::{Value, json};

/// Service-15 packages of view sessions (`appstream.a.y()` on the phone).
pub fn is_view_pkg(pkg: &str) -> bool {
    matches!(pkg, "mirror" | "desktop" | "mobile")
}

/// Wire `view` (the phone's display mode + 1) → service-15 package.
pub fn view_pkg(view: i64) -> Option<&'static str> {
    match view {
        1 => Some("mirror"),
        2 => Some("desktop"),
        3 => Some("mobile"),
        _ => None,
    }
}

/// Window title of a view session.
pub fn title(pkg: &str) -> Option<&'static str> {
    match pkg {
        "mirror" => Some("Phone — Smart Connect"),
        "desktop" => Some("Mobile desktop — Smart Connect"),
        "mobile" => Some("Virtual phone — Smart Connect"),
        _ => None,
    }
}

#[derive(Debug, PartialEq)]
pub struct ViewInvite {
    pub view: i64,
    /// The desktop resolution the user picked on the phone (sent when our svc 1 version is ≥ 6).
    pub resolution: Option<(i64, i64)>,
}

/// Phone svc 1 cmd 0: the user started a view on the phone.
pub fn parse_invite(msg: &Value) -> Option<ViewInvite> {
    if msg.get("cmd").and_then(Value::as_i64) != Some(0) {
        return None;
    }
    let view = msg.get("view").and_then(Value::as_i64).filter(|v| view_pkg(*v).is_some())?;
    let resolution = msg
        .get("resolution")
        .and_then(|r| Some((r.get("width")?.as_i64()?, r.get("height")?.as_i64()?)))
        .filter(|(w, h)| *w > 0 && *h > 0);
    Some(ViewInvite { view, resolution })
}

/// Our svc 1 cmd 1 answering a phone invite. The phone reads `view` (and `drawerMode`) from it;
/// `reply: 1` makes a phone whose invite already timed out ignore it instead of treating it as a
/// new PC-initiated start (INFERRED from the phone's idle-state check).
pub fn accept(view: i64) -> Value {
    json!({ "ver": "1.0.0", "cmd": 1, "view": view, "reply": 1, "drawerMode": false })
}

/// A PC-initiated start (svc 1 cmd 1). `reply` must not be 1 here.
pub fn start(view: i64) -> Value {
    json!({ "ver": "1.0.0", "cmd": 1, "view": view, "reply": 0, "drawerMode": false })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invite_parsing() {
        assert_eq!(
            parse_invite(&json!({ "ver": "1.0.0", "cmd": 0, "view": 1 })),
            Some(ViewInvite { view: 1, resolution: None })
        );
        assert_eq!(
            parse_invite(
                &json!({ "ver": "1.0.0", "cmd": 0, "view": 2, "resolution": { "width": 2560, "height": 1440 } })
            ),
            Some(ViewInvite { view: 2, resolution: Some((2560, 1440)) })
        );
        assert_eq!(
            parse_invite(&json!({ "cmd": 0, "view": 2, "resolution": { "width": 0, "height": 0 } }))
                .unwrap()
                .resolution,
            None
        );
        assert!(parse_invite(&json!({ "cmd": 0, "view": 7 })).is_none(), "unknown view");
        assert!(parse_invite(&json!({ "cmd": 31, "view": 7 })).is_none(), "supported-views broadcast");
        assert!(parse_invite(&json!({ "cmd": 11, "errorCode": 145 })).is_none());
    }

    #[test]
    fn answers() {
        let a = accept(2);
        assert_eq!((a["cmd"].as_i64(), a["view"].as_i64(), a["reply"].as_i64()), (Some(1), Some(2), Some(1)));
        assert_eq!(a["ver"], "1.0.0");
        assert_eq!(a["drawerMode"], false);
        assert_eq!(start(1)["reply"], 0);
    }

    #[test]
    fn view_packages() {
        assert_eq!(view_pkg(1), Some("mirror"));
        assert_eq!(view_pkg(2), Some("desktop"));
        assert_eq!(view_pkg(3), Some("mobile"));
        assert_eq!(view_pkg(0), None);
        assert!(is_view_pkg("desktop") && !is_view_pkg("com.example"));
        assert_eq!(title("desktop"), Some("Mobile desktop — Smart Connect"));
        assert_eq!(title("com.example"), None);
    }
}
