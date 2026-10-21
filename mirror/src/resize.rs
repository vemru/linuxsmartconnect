//! App window resize → phone resize request (svc 15 cmd 17), debounced.
//!
//! The phone resizes the app's virtual display in place and changes the capture size of the same
//! WebRTC session, then answers cmd 26 `{displayId, success}` (success false when the new bound
//! maps to the size it already has). The video frame size changes mid-stream; the video sink keeps
//! its window and scales. Never sent for the whole-screen mirror (the phone always refuses it).

use std::time::{Duration, Instant};

use serde_json::{Value, json};

/// A window size must hold this long before it is requested (interactive resizes fire many).
pub const SETTLE: Duration = Duration::from_millis(400);
/// At most one request per this interval.
pub const MIN_INTERVAL: Duration = Duration::from_millis(1000);
/// Smaller windows (minimised, being mapped) are ignored.
pub const MIN_SIDE: u32 = 120;
/// A window within this many pixels of the stream size already matches it.
pub const TOLERANCE: u32 = 4;
pub const ENV_RESIZE: &str = "SMARTCONNECT_MIRROR_RESIZE";

/// Opt-in until verified on a device: `SMARTCONNECT_MIRROR_RESIZE=1` (or `--resize`).
pub fn enabled(env: Option<&str>) -> bool {
    matches!(env.map(str::trim), Some("1" | "true" | "yes" | "on"))
}

#[derive(Debug, Default)]
pub struct ResizeDebouncer {
    pending: Option<((u32, u32), Instant)>,
    last_requested: Option<(u32, u32)>,
    last_sent_at: Option<Instant>,
    stream: Option<(u32, u32)>,
}

impl ResizeDebouncer {
    /// The phone's current stream size for this session (cmd 3 `width`/`height`).
    pub fn set_stream_size(&mut self, w: u32, h: u32) {
        if w > 0 && h > 0 {
            self.stream = Some((w, h));
        }
    }

    /// Feeds the window's current client size; returns the size to request, if any.
    pub fn observe(&mut self, size: (u32, u32), now: Instant) -> Option<(u32, u32)> {
        if size.0 < MIN_SIDE || size.1 < MIN_SIDE {
            self.pending = None;
            return None;
        }
        let since = match self.pending {
            Some((s, t)) if s == size => t,
            _ => {
                self.pending = Some((size, now));
                return None;
            }
        };
        if now.duration_since(since) < SETTLE || self.last_requested == Some(size) {
            return None;
        }
        // Nothing requested yet and the window already shows the stream 1:1: nothing to do.
        if self.last_requested.is_none() && self.stream.is_some_and(|s| near(s, size)) {
            return None;
        }
        if self.last_sent_at.is_some_and(|t| now.duration_since(t) < MIN_INTERVAL) {
            return None;
        }
        self.last_requested = Some(size);
        self.last_sent_at = Some(now);
        Some(size)
    }
}

fn near(a: (u32, u32), b: (u32, u32)) -> bool {
    a.0.abs_diff(b.0) <= TOLERANCE && a.1.abs_diff(b.1) <= TOLERANCE
}

/// What the request is about: the session's display and app, plus our constant `instanceId`.
pub struct ResizeTarget<'a> {
    pub display_id: i64,
    pub pkg: &'a str,
    pub user_id: &'a Value,
    pub cmp: &'a Value,
    pub shortcut_id: &'a Value,
    pub instance_id: i64,
}

/// svc 15 cmd 17 `AppResizeData`: every peer-start field plus `displayId`. `width`/`height` are the
/// new bound the phone fits the app's display into (as in cmd 1); the physical size keeps the
/// 1920 px ↔ 344 mm ratio of our cmd 1 (the phone derives the desktop-mode DPI from it).
pub fn payload(t: &ResizeTarget, (w, h): (u32, u32)) -> Value {
    let mm = |px: u32| (f64::from(px) * 344.0 / 1920.0).round() as i64;
    json!({
        "displayId": t.display_id, "pkg": t.pkg, "userId": t.user_id, "cmp": t.cmp, "shortcutId": t.shortcut_id,
        "width": w, "height": h, "displayWidth": w, "displayHeight": h, "displayDensity": 160,
        "phyWidth": mm(w), "phyHeight": mm(h), "displayScale": 1.0, "align": 0,
        "instanceId": t.instance_id, "supportedMediaFormat": 1, "supportWebRTC": 1,
        // Booleans in the phone's bean (see cmd 1).
        "supportCustomDisplay": false, "vrSupportUnfolded": false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    #[test]
    fn waits_for_the_size_to_settle() {
        let t0 = Instant::now();
        let mut d = ResizeDebouncer::default();
        d.set_stream_size(610, 1356);
        assert_eq!(d.observe((900, 1000), t0), None, "first sighting");
        assert_eq!(d.observe((950, 1000), t0 + ms(100)), None, "still moving");
        assert_eq!(d.observe((950, 1000), t0 + ms(300)), None, "not settled yet");
        assert_eq!(d.observe((950, 1000), t0 + ms(600)), Some((950, 1000)));
        assert_eq!(d.observe((950, 1000), t0 + ms(2000)), None, "already requested");
    }

    #[test]
    fn no_request_when_the_window_matches_the_stream() {
        let t0 = Instant::now();
        let mut d = ResizeDebouncer::default();
        d.set_stream_size(610, 1356);
        assert_eq!(d.observe((612, 1354), t0), None);
        assert_eq!(d.observe((612, 1354), t0 + ms(1000)), None);
        // The user drags it bigger later.
        assert_eq!(d.observe((800, 1356), t0 + ms(2000)), None);
        assert_eq!(d.observe((800, 1356), t0 + ms(2500)), Some((800, 1356)));
    }

    #[test]
    fn rate_limited_and_back_to_a_previous_size_works() {
        let t0 = Instant::now();
        let mut d = ResizeDebouncer::default();
        d.observe((800, 600), t0);
        assert_eq!(d.observe((800, 600), t0 + ms(500)), Some((800, 600)));
        d.observe((1000, 700), t0 + ms(600));
        assert_eq!(d.observe((1000, 700), t0 + ms(1100)), None, "less than MIN_INTERVAL since the last request");
        assert_eq!(d.observe((1000, 700), t0 + ms(1600)), Some((1000, 700)));
        d.observe((800, 600), t0 + ms(1700));
        assert_eq!(d.observe((800, 600), t0 + ms(2700)), Some((800, 600)), "a new size again");
    }

    #[test]
    fn tiny_windows_are_ignored() {
        let t0 = Instant::now();
        let mut d = ResizeDebouncer::default();
        d.observe((1, 1), t0);
        assert_eq!(d.observe((1, 1), t0 + ms(1000)), None);
    }

    #[test]
    fn payload_fields() {
        let (u, c, s) = (json!(0), json!("com.example.Main"), Value::Null);
        let t =
            ResizeTarget { display_id: 67, pkg: "com.example", user_id: &u, cmp: &c, shortcut_id: &s, instance_id: 42 };
        let p = payload(&t, (1920, 1080));
        assert_eq!(p["displayId"], 67);
        assert_eq!((p["width"].as_u64(), p["height"].as_u64()), (Some(1920), Some(1080)));
        assert_eq!((p["phyWidth"].as_i64(), p["phyHeight"].as_i64()), (Some(344), Some(194)));
        assert_eq!(p["supportCustomDisplay"], false, "a boolean, never 0");
        assert_eq!(p["supportWebRTC"], 1);
        assert_eq!(p["instanceId"], 42);
    }

    #[test]
    fn opt_in() {
        assert!(!enabled(None));
        assert!(!enabled(Some("0")));
        assert!(enabled(Some("1")));
        assert!(enabled(Some(" yes ")));
    }
}
