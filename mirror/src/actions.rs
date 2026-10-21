//! Notification action buttons without a window ("background actions").
//!
//! On a LAN link the phone carries out a PC-side notification click through app streaming: it
//! keeps the PendingIntent, invites us to stream the app (svc 15 cmd 2), creates the app's virtual
//! display and launches the app there when our cmd 4 answer arrives, and fires the intent from a
//! trampoline activity on that display as soon as the app's task gains focus there. It reports that
//! focus change as cmd 13 for the display at the same moment (phone app, `appstream.a.j` → `J`).
//! ICE and video are not needed for the intent to fire.
//!
//! So for an action button (daemon local event `kind: "action"`) the host answers the following
//! invite for the same package headlessly (no window, video into a fakesink) and closes the session
//! (cmd 7) shortly after the phone reported the app focused. "Open" clicks keep their window.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use serde_json::Value;

/// Daemon-local events on the `watch` stream (`proto::SERVICE_LOCAL_EVENTS` in the daemon).
pub const SERVICE_LOCAL_EVENTS: i64 = -1;
/// How long after an action click an invite for the same package counts as its result. The phone
/// invites within about a second.
pub const ACTION_WINDOW: Duration = Duration::from_secs(6);
/// After the phone reported the app focused or raised on the session's display (cmd 13/12/14):
/// time for its trampoline activity to fire the intent before the display goes away.
pub const FIRE_GRACE: Duration = Duration::from_millis(2500);
/// No such report at all: close anyway (the intent may still have fired).
pub const NO_FIRE_TIMEOUT: Duration = Duration::from_secs(12);
/// Upper bound for a headless session, however many focus reports keep arriving.
pub const MAX_LIFETIME: Duration = Duration::from_secs(30);
pub const ENV_BACKGROUND: &str = "SMARTCONNECT_BACKGROUND_ACTIONS";

/// On unless `SMARTCONNECT_BACKGROUND_ACTIONS=0` (then action buttons open a window, like "Open").
pub fn enabled(env: Option<&str>) -> bool {
    !matches!(env.map(str::trim), Some("0" | "false" | "no" | "off"))
}

#[derive(Debug, PartialEq)]
pub enum ClickKind {
    /// An action button (phone event 1).
    Action,
    /// A click on the notification itself (phone event 2).
    Open,
}

#[derive(Debug, PartialEq)]
pub struct Click {
    pub kind: ClickKind,
    pub pkg: String,
}

/// `{"event":"notification_action","kind":"action"|"open","pkg":…,"key":…,"actionId":…}`.
pub fn parse_click(ev: &Value) -> Option<Click> {
    if ev.get("event").and_then(Value::as_str) != Some("notification_action") {
        return None;
    }
    let pkg = ev.get("pkg").and_then(Value::as_str).filter(|p| !p.is_empty())?.to_owned();
    let kind = match ev.get("kind").and_then(Value::as_str)? {
        "action" => ClickKind::Action,
        "open" => ClickKind::Open,
        _ => return None,
    };
    Some(Click { kind, pkg })
}

/// Recent action clicks per package, waiting for the phone's invite.
#[derive(Debug, Default)]
pub struct PendingActions {
    until: HashMap<String, Instant>,
}

impl PendingActions {
    /// An action button arms background mode for its package; "Open" disarms it (the user wants
    /// to see the app).
    pub fn note(&mut self, click: &Click, now: Instant) {
        self.until.retain(|_, t| *t > now);
        match click.kind {
            ClickKind::Action => {
                self.until.insert(click.pkg.clone(), now + ACTION_WINDOW);
            }
            ClickKind::Open => {
                self.until.remove(&click.pkg);
            }
        }
    }

    /// True (once) when an invite for `pkg` follows a recent action click.
    pub fn take(&mut self, pkg: &str, now: Instant) -> bool {
        self.until.remove(pkg).is_some_and(|t| t > now)
    }
}

/// When to close a headless session.
#[derive(Debug, Clone, Copy)]
pub struct HeadlessTimer {
    answered_at: Instant,
    fired_at: Option<Instant>,
}

impl HeadlessTimer {
    /// Starts when our cmd 4 answer went out (that is what makes the phone launch the app).
    pub fn new(answered_at: Instant) -> Self {
        Self { answered_at, fired_at: None }
    }

    /// The phone reported the app focused/merged/raised on this session's display.
    pub fn fired(&mut self, now: Instant) {
        self.fired_at = Some(now);
    }

    /// `Some(reason)` once the session should be closed.
    pub fn due(&self, now: Instant) -> Option<&'static str> {
        if now.duration_since(self.answered_at) >= MAX_LIFETIME {
            return Some("background action: time limit");
        }
        match self.fired_at {
            Some(t) if now.duration_since(t) >= FIRE_GRACE => Some("background action done"),
            Some(_) => None,
            None if now.duration_since(self.answered_at) >= NO_FIRE_TIMEOUT => {
                Some("background action: the phone never reported the app focused")
            }
            None => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn click(kind: &str, pkg: &str) -> Click {
        parse_click(&json!({ "event": "notification_action", "kind": kind, "pkg": pkg, "key": "0|x|1|null|1" }))
            .unwrap()
    }

    #[test]
    fn parses_daemon_events() {
        assert_eq!(click("action", "com.a"), Click { kind: ClickKind::Action, pkg: "com.a".into() });
        assert_eq!(click("open", "com.a").kind, ClickKind::Open);
        assert!(parse_click(&json!({ "event": "other", "kind": "action", "pkg": "com.a" })).is_none());
        assert!(parse_click(&json!({ "event": "notification_action", "kind": "reply", "pkg": "com.a" })).is_none());
        assert!(parse_click(&json!({ "event": "notification_action", "kind": "action", "pkg": "" })).is_none());
    }

    #[test]
    fn an_invite_soon_after_an_action_click_is_background() {
        let t0 = Instant::now();
        let mut p = PendingActions::default();
        p.note(&click("action", "com.a"), t0);
        assert!(!p.take("com.b", t0 + Duration::from_secs(1)), "other package");
        assert!(p.take("com.a", t0 + Duration::from_secs(1)));
        assert!(!p.take("com.a", t0 + Duration::from_secs(2)), "only once");
    }

    #[test]
    fn stale_clicks_and_open_clicks_mean_a_window() {
        let t0 = Instant::now();
        let mut p = PendingActions::default();
        p.note(&click("action", "com.a"), t0);
        assert!(!p.take("com.a", t0 + ACTION_WINDOW + Duration::from_millis(1)), "too late");
        p.note(&click("action", "com.a"), t0);
        p.note(&click("open", "com.a"), t0 + Duration::from_millis(500));
        assert!(!p.take("com.a", t0 + Duration::from_secs(1)), "the user then clicked Open");
    }

    #[test]
    fn headless_close_timing() {
        let t0 = Instant::now();
        let s = |n: u64| Duration::from_millis(n);
        let mut t = HeadlessTimer::new(t0);
        assert_eq!(t.due(t0 + s(5000)), None, "waiting for the focus report");
        t.fired(t0 + s(1500));
        assert_eq!(t.due(t0 + s(3000)), None, "grace");
        assert!(t.due(t0 + s(1500) + FIRE_GRACE).is_some());
        t.fired(t0 + s(4500)); // another report (e.g. cmd 14) re-arms the grace
        assert_eq!(t.due(t0 + s(5000)), None);
        let quiet = HeadlessTimer::new(t0);
        assert!(quiet.due(t0 + NO_FIRE_TIMEOUT).is_some(), "never reported");
        let mut busy = HeadlessTimer::new(t0);
        busy.fired(t0 + MAX_LIFETIME - s(100));
        assert!(busy.due(t0 + MAX_LIFETIME).is_some(), "hard limit");
    }

    #[test]
    fn on_by_default() {
        assert!(enabled(None));
        assert!(enabled(Some("1")));
        assert!(!enabled(Some("0")));
        assert!(!enabled(Some("off")));
    }
}
