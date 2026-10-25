//! Phone notifications → Linux desktop notifications (spec: `evidence-2026-10-20/notification-spec.md`).
//!
//! The phone pushes every notification on svc 5 (apps on its show-list) or svc 37 (others) with no
//! handshake: `{version, key, event: 0 posted | 1 updated | 2 removed, type, actions[], data{…}}`.
//! We mirror them through `org.freedesktop.Notifications`, replacing by the phone's `key`.
//! Clicking a notification sends "open" (cmd 0 / event 2: fires its intent on the phone, which also
//! clears it there); action buttons send cmd 0 / event 1 with the phone's `action_id`.
//! There is no dismiss-only message, so closing one on Linux stays local.
//!
//! On this phone build the action/open handler (`fh6.f`) routes every PendingIntent through *app
//! streaming* when connected over LAN: it invites the PC to stream the app (svc 15 cmd 2) and only
//! fires the intent on the streamed virtual display. So every click is also announced on the
//! `watch` stream ([`click_event`]); `smartconnect-mirror --host` runs action buttons headless and
//! closes the stream again (see `.notes/notification-background-actions.md`).
//!
//! Polish: identical re-posts are dropped and rapid updates per key (progress, calls, timers) are
//! coalesced to one per [`UPDATE_MIN_INTERVAL`]; a per-app mute list lives in the state dir
//! (`notif-mute.json`, control command `{"cmd":"notif_mute","pkg":…,"mute":true|false}`).

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use anyhow::Result;
use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use futures_util::StreamExt;
use serde_json::{Value, json};
use tokio::sync::broadcast;
use tracing::{debug, info, warn};
use zbus::zvariant;

use crate::status::runtime_dir;

pub const SERVICE_NOTIFY: i64 = 5;
pub const SERVICE_NOTIFY_WIDGET: i64 = 37;
const TYPE_CUSTOM_VIEW: i64 = 7;
const BODY_MAX_CHARS: usize = 600;
/// At most one desktop update per key in this window; the latest frame wins.
const UPDATE_MIN_INTERVAL: Duration = Duration::from_millis(1000);

#[zbus::proxy(
    interface = "org.freedesktop.Notifications",
    default_service = "org.freedesktop.Notifications",
    default_path = "/org/freedesktop/Notifications"
)]
trait Notifications {
    #[allow(clippy::too_many_arguments)]
    fn notify(
        &self,
        app_name: &str,
        replaces_id: u32,
        app_icon: &str,
        summary: &str,
        body: &str,
        actions: &[&str],
        hints: HashMap<&str, zvariant::Value<'_>>,
        expire_timeout: i32,
    ) -> zbus::Result<u32>;

    fn close_notification(&self, id: u32) -> zbus::Result<()>;

    fn get_capabilities(&self) -> zbus::Result<Vec<String>>;

    fn get_server_information(&self) -> zbus::Result<(String, String, String, String)>;

    #[zbus(signal)]
    fn action_invoked(&self, id: u32, action_key: String) -> zbus::Result<()>;

    #[zbus(signal)]
    fn notification_closed(&self, id: u32, reason: u32) -> zbus::Result<()>;
}

/// A message for the phone: (serviceId, data).
pub type PhoneCommand = (i64, Value);

#[derive(Default)]
struct Shown {
    by_key: HashMap<String, u32>,
    by_id: HashMap<u32, (String, i64)>,
    replyable: std::collections::HashSet<String>,
}

pub struct Notifier {
    proxy: NotificationsProxy<'static>,
    shown: Mutex<Shown>,
    /// (title, body, app) per key, for the reply window.
    shown_text: Mutex<HashMap<String, (String, String, String)>>,
    to_phone: broadcast::Sender<PhoneCommand>,
    /// Per key: (content fingerprint, when shown) of the last desktop notification.
    last: Mutex<HashMap<String, (u64, Instant)>>,
    /// Per key: the newest frame waiting for its coalescing window to pass.
    pending: Mutex<HashMap<String, (i64, Value)>>,
    /// The daemon's `watch` stream (`Daemon::from_phone`): clicks are announced there as
    /// [`click_event`]s on [`crate::proto::SERVICE_LOCAL_EVENTS`] for the stream host.
    local_events: broadcast::Sender<PhoneCommand>,
}

impl Notifier {
    /// Connects to the session bus; `to_phone` carries click/action results to the live session,
    /// `local_events` announces them to desktop helpers (see [`click_event`]).
    pub async fn new(
        to_phone: broadcast::Sender<PhoneCommand>,
        local_events: broadcast::Sender<PhoneCommand>,
    ) -> Result<Arc<Self>> {
        let conn = zbus::Connection::session().await?;
        let proxy = NotificationsProxy::new(&conn).await?;
        let me = Arc::new(Self {
            proxy,
            shown: Mutex::default(),
            shown_text: Mutex::default(),
            to_phone,
            last: Mutex::default(),
            pending: Mutex::default(),
            local_events,
        });
        tokio::spawn(me.clone().follow_signals());
        Ok(me)
    }

    /// Shows a plain notification from the daemon itself (not one mirrored from the phone).
    pub async fn notify_local(&self, summary: &str, body: &str) {
        if let Err(e) = self.proxy.notify("Smart Connect", 0, "dialog-warning", summary, body, &[], HashMap::new(), -1).await {
            debug!("notifications: local notification failed: {e}");
        }
    }

    /// Handles one svc 5/37 frame from the phone.
    pub async fn handle(self: &Arc<Self>, service_id: i64, n: &Value) {
        let Some(key) = n.get("key").and_then(Value::as_str) else { return };
        match n.get("event").and_then(Value::as_i64) {
            Some(0 | 1) => {
                if n.get("type").and_then(Value::as_i64) == Some(TYPE_CUSTOM_VIEW) {
                    return; // custom RemoteViews: no usable text (Windows drops these too)
                }
                let pkg = n.pointer("/data/package_name").and_then(Value::as_str).unwrap_or_default();
                if is_muted(pkg) {
                    debug!(%pkg, "notifications: muted app");
                    return;
                }
                let fp = fingerprint(n);
                let since = {
                    let last = self.last.lock().unwrap();
                    match last.get(key) {
                        Some((f, _)) if *f == fp => return, // identical re-post (the phone resends on every tick)
                        Some((_, at)) => Some(at.elapsed()),
                        None => None,
                    }
                };
                match since {
                    Some(elapsed) if elapsed < UPDATE_MIN_INTERVAL => {
                        // Coalesce: keep only the newest frame; one delayed task flushes it.
                        let first = self.pending.lock().unwrap().insert(key.to_owned(), (service_id, n.clone())).is_none();
                        if first {
                            let (me, key) = (self.clone(), key.to_owned());
                            tokio::spawn(async move {
                                tokio::time::sleep(UPDATE_MIN_INTERVAL - elapsed).await;
                                let next = me.pending.lock().unwrap().remove(&key);
                                if let Some((svc, frame)) = next {
                                    me.show_now(svc, &key, &frame).await;
                                }
                            });
                        }
                    }
                    _ => self.show_now(service_id, key, n).await,
                }
            }
            Some(2) => {
                self.pending.lock().unwrap().remove(key);
                self.last.lock().unwrap().remove(key);
                self.shown_text.lock().unwrap().remove(key);
                let id = {
                    let mut shown = self.shown.lock().unwrap();
                    shown.replyable.remove(key);
                    let id = shown.by_key.remove(key);
                    if let Some(id) = id {
                        shown.by_id.remove(&id);
                    }
                    id
                };
                if let Some(id) = id {
                    let _ = self.proxy.close_notification(id).await;
                }
            }
            _ => {}
        }
    }

    async fn show_now(&self, service_id: i64, key: &str, n: &Value) {
        self.last.lock().unwrap().insert(key.to_owned(), (fingerprint(n), Instant::now()));
        if let Err(e) = self.show(service_id, key, n).await {
            warn!("notifications: showing failed: {e:#}");
        }
    }

    async fn show(&self, service_id: i64, key: &str, n: &Value) -> Result<()> {
        let d = n.get("data").cloned().unwrap_or(Value::Null);
        let s = |k: &str| d.get(k).and_then(Value::as_str).unwrap_or_default().to_owned();
        let app = s("apk_name");
        let pkg = s("package_name");
        let title = first_non_empty(&[s("conversation_title"), s("title"), app.clone()]);
        let body = truncate(&body_text(&d), BODY_MAX_CHARS);

        // The phone's apk_icon is a white glyph on black; the phone's launcher icon (app-icon cache)
        // or a themed desktop icon looks far better.
        let app_icon = cached_app_icon(&smartconnect_fex::icons::default_cache_dir(), &pkg)
            .or_else(|| themed_icon(&pkg, &app))
            .or_else(|| save_image(&format!("app-{}", sanitize(&pkg)), d.get("apk_icon")))
            .unwrap_or_default();
        // Without a picture the notification server shows `app_icon` instead.
        let picture = save_image(&format!("pic-{}", sanitize(key)), d.get("big_picture"))
            .or_else(|| save_image(&format!("person-{}", sanitize(key)), sender_avatar(n)));

        // "default" = click on the notification body: reply if the app takes replies (opening the app
        // on the phone is usually blocked by Android's background-launch rules), else open on phone.
        let replyable = n
            .get("actions")
            .and_then(Value::as_array)
            .is_some_and(|a| a.iter().any(|a| a.get("remote_input").and_then(Value::as_bool) == Some(true)));
        let mut actions: Vec<String> = if replyable {
            // Noctalia renders the default action as a button too, so it doubles as "Reply".
            vec!["default".into(), "Reply".into()]
        } else {
            vec!["default".into(), "Open on phone".into()]
        };
        for a in n.get("actions").and_then(Value::as_array).into_iter().flatten() {
            let (Some(id), Some(name)) = (a.get("action_id").and_then(Value::as_i64), a.get("action_name").and_then(Value::as_str))
            else {
                continue;
            };
            // Free-text reply needs inline-reply support from the desktop; skip those buttons for now.
            if a.get("remote_input").and_then(Value::as_bool) == Some(true) {
                continue;
            }
            actions.push(format!("a{id}"));
            actions.push(name.to_owned());
        }
        let actions: Vec<&str> = actions.iter().map(String::as_str).collect();

        let urgency: u8 = match d.get("priority").and_then(Value::as_i64).unwrap_or(0) {
            p if p >= 2 => 2,
            p if p <= -1 => 0,
            _ => 1,
        };
        let mut hints: HashMap<&str, zvariant::Value<'_>> = HashMap::new();
        hints.insert("urgency", zvariant::Value::U8(urgency));
        hints.insert("x-smartconnect-key", zvariant::Value::from(key.to_owned()));
        if let Some(pic) = picture.as_deref() {
            hints.insert("image-path", zvariant::Value::from(pic.to_owned()));
        }
        let progress = d.get("progress").cloned().unwrap_or(Value::Null);
        let (cur, max) = (progress.get("current").and_then(Value::as_i64).unwrap_or(0), progress.get("max").and_then(Value::as_i64).unwrap_or(0));
        if max > 0 {
            hints.insert("value", zvariant::Value::I32(((cur * 100) / max).clamp(0, 100) as i32));
        }

        let replaces = self.shown.lock().unwrap().by_key.get(key).copied().unwrap_or(0);
        let summary = title.clone();
        // Markup bodies fold "\n" into a space. Quickshell shells (Noctalia) render Qt StyledText
        // without advertising body-markup, so treat them as markup servers too.
        let markup = self.proxy.get_capabilities().await.is_ok_and(|c| c.iter().any(|c| c == "body-markup"))
            || self.proxy.get_server_information().await.is_ok_and(|(name, ..)| name == "quickshell");
        let shown_body = if markup { markup_body(&body) } else { body.clone() };
        let id = self.proxy.notify(&app, replaces, &app_icon, &summary, &shown_body, &actions, hints, -1).await?;
        let mut shown = self.shown.lock().unwrap();
        shown.by_key.insert(key.to_owned(), id);
        shown.by_id.insert(id, (key.to_owned(), service_id));
        self.shown_text.lock().unwrap().insert(key.to_owned(), (title.clone(), body.clone(), app.clone()));
        if replyable {
            shown.replyable.insert(key.to_owned());
        } else {
            shown.replyable.remove(key);
        }
        debug!(%pkg, id, "notifications: shown");
        Ok(())
    }

    /// A small Quickshell window with a text field; it sends the reply over the control socket.
    fn open_reply_window(&self, key: &str) {
        let (title, body, app) = self
            .shown_text
            .lock()
            .unwrap()
            .get(key)
            .cloned()
            .unwrap_or_default();
        let qml = runtime_dir().join("reply.qml");
        if let Err(e) = std::fs::write(&qml, include_str!("../desktop/reply.qml")) {
            warn!("notifications: writing reply window: {e}");
            return;
        }
        let socket = crate::control::socket_path();
        let key = key.to_owned();
        tokio::spawn(async move {
            let status = tokio::process::Command::new("qs")
                .arg("-p")
                .arg(&qml)
                .env("SC_KEY", &key)
                .env("SC_TITLE", title)
                .env("SC_BODY", body)
                .env("SC_APP", app)
                .env("SC_SOCKET", socket)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .await;
            // Only a spawn error means qs is missing; a non-zero exit is usually the window being
            // killed along with the shell (e.g. `killall qs` to reload Noctalia), not a failure.
            match status {
                Ok(s) if s.success() => {}
                Ok(s) => info!(%s, "notifications: reply window exited abnormally"),
                Err(e) => warn!("notifications: reply window failed to start (is quickshell `qs` on PATH?): {e}"),
            }
        });
    }

    async fn follow_signals(self: Arc<Self>) {
        let (Ok(mut invoked), Ok(mut closed)) =
            (self.proxy.receive_action_invoked().await, self.proxy.receive_notification_closed().await)
        else {
            warn!("notifications: can't subscribe to notification signals; clicks won't reach the phone");
            return;
        };
        loop {
            tokio::select! {
                Some(sig) = invoked.next() => {
                    let Ok(args) = sig.args() else { continue };
                    let Some((key, svc)) = self.shown.lock().unwrap().by_id.get(&args.id).cloned() else { continue };
                    let replyable = self.shown.lock().unwrap().replyable.contains(&key);
                    let cmd = match args.action_key.as_str() {
                        "default" if replyable => { self.open_reply_window(&key); continue; }
                        "default" => json!({ "cmd": 0, "event": 2, "key": key, "version": "1.0" }),
                        a => match a.strip_prefix('a').and_then(|i| i.parse::<i64>().ok()) {
                            Some(i) => json!({ "cmd": 0, "event": 1, "key": key, "version": "1.0", "data": { "action_id": i } }),
                            None => continue,
                        },
                    };
                    info!(action = %args.action_key, "notifications: forwarding click to the phone");
                    // Tell the stream host first: the phone answers with an app-stream invite.
                    if let Some(ev) = click_event(svc, &cmd) {
                        let _ = self.local_events.send((crate::proto::SERVICE_LOCAL_EVENTS, ev));
                    }
                    let _ = self.to_phone.send((svc, cmd));
                }
                Some(sig) = closed.next() => {
                    if let Ok(args) = sig.args() {
                        let mut shown = self.shown.lock().unwrap();
                        if let Some((key, _)) = shown.by_id.remove(&args.id) {
                            shown.by_key.remove(&key);
                        }
                    }
                }
                else => return,
            }
        }
    }
}

/// Daemon-local event for a notification click the phone will carry out through app streaming:
/// `{"event":"notification_action","kind":"action"|"open","pkg","key","actionId","service"}`.
/// `kind` "action" = an action button (phone event 1), "open" = a click on the notification itself
/// (event 2). On a LAN link the phone then invites the PC to stream `pkg` (svc 15 cmd 2) and fires
/// the intent on that app's virtual display; the stream host uses this event to run action buttons
/// without a window. `None` for anything else (replies, mute, permission requests).
pub fn click_event(service: i64, cmd: &Value) -> Option<Value> {
    if !matches!(service, SERVICE_NOTIFY | SERVICE_NOTIFY_WIDGET) || cmd.get("cmd").and_then(Value::as_i64) != Some(0) {
        return None;
    }
    let kind = match cmd.get("event").and_then(Value::as_i64) {
        Some(1) => "action",
        Some(2) => "open",
        _ => return None,
    };
    let key = cmd.get("key").and_then(Value::as_str)?;
    let action_id = cmd.pointer("/data/action_id").cloned().unwrap_or(Value::Null);
    Some(json!({
        "event": "notification_action", "kind": kind, "pkg": key_package(key)?, "key": key,
        "actionId": action_id, "service": service,
    }))
}

/// The package inside an Android notification key (`<user>|<package>|<id>|<tag>|<uid>`).
pub fn key_package(key: &str) -> Option<String> {
    let pkg = key.split('|').nth(1)?.trim();
    (!pkg.is_empty()).then(|| pkg.to_owned())
}

/// The phone fills the app's own RemoteInput reply action with this text and fires it.
pub fn reply_command(key: &str, text: &str) -> Value {
    json!({ "cmd": 0, "event": 0, "key": key, "version": "1.0",
            "data": { "reply_content": text, "smart_reply": false } })
}

/// Best readable body for each notification style.
pub fn body_text(d: &Value) -> String {
    let s = |k: &str| d.get(k).and_then(Value::as_str).unwrap_or_default().to_owned();
    let strings = |k: &str| -> Vec<String> {
        d.get(k).and_then(Value::as_array).into_iter().flatten().filter_map(|v| v.as_str().map(str::to_owned)).collect()
    };
    let messages = strings("messages");
    if !messages.is_empty() {
        let persons = strings("message_persons");
        let conversation = !s("conversation_title").is_empty();
        return messages
            .iter()
            .enumerate()
            .rev()
            .take(4)
            .rev()
            .map(|(i, m)| match persons.get(i) {
                Some(p) if conversation && !p.is_empty() => format!("{p}: {m}"),
                _ => m.clone(),
            })
            .collect::<Vec<_>>()
            .join("\n");
    }
    let lines = strings("line_text");
    if !lines.is_empty() {
        return lines.join("\n");
    }
    first_non_empty(&[s("big_text"), s("content"), s("expand_summary")])
}

/// Hash of everything that changes what the desktop shows (not the base64 images, which the phone
/// re-encodes on every post).
fn fingerprint(n: &Value) -> u64 {
    use std::hash::{Hash, Hasher};
    let d = n.get("data").cloned().unwrap_or(Value::Null);
    let mut h = std::collections::hash_map::DefaultHasher::new();
    for k in ["title", "conversation_title", "content", "big_text", "expand_summary", "priority"] {
        d.get(k).map(Value::to_string).hash(&mut h);
    }
    for k in ["messages", "message_persons", "line_text", "progress"] {
        d.get(k).map(Value::to_string).hash(&mut h);
    }
    n.get("actions").map(Value::to_string).hash(&mut h);
    h.finish()
}

/// The avatar of the person a notification is about, if the phone sent one (base64 JPEG field).
///
/// - Call (type 6): `person_icon` is the caller (`android.callPerson`).
/// - Messaging (type 3): `person_name`/`person_icon` are the style's *user*, which Android defines
///   as the device owner: WhatsApp sends the owner's own picture on every chat. The phone sends no
///   per-sender icons and no conversation (large) icon. So the picture is used only when that
///   person is also the notification's headline (`title`), as some apps (e.g. the dialer) set the
///   user to the other party; otherwise `None` and the app icon stands in.
pub fn sender_avatar(n: &Value) -> Option<&Value> {
    let d = n.get("data")?;
    let icon = d.get("person_icon").filter(|v| v.as_str().is_some_and(|s| !s.trim().is_empty()))?;
    let messaging = n.get("type").and_then(Value::as_i64) == Some(3) || d.get("messages").is_some();
    if !messaging {
        return Some(icon);
    }
    let text = |k: &str| d.get(k).and_then(Value::as_str).unwrap_or_default().trim();
    let person = text("person_name");
    (!person.is_empty() && person == text("title")).then_some(icon)
}

/// The phone's launcher icon for `pkg` in the app-icon cache (see `docs/protocol/app-streaming.md`,
/// "Icon cache"): `<pkg>.png`, else an `<iconName>` (`<pkg>_<activity>.png`), preferring the main
/// user's over another user's (`<userId>_<pkg>_<activity>.png`).
fn cached_app_icon(dir: &std::path::Path, pkg: &str) -> Option<String> {
    if pkg.is_empty() || !pkg.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-')) {
        return None;
    }
    let exact = dir.join(format!("{pkg}.png"));
    if exact.is_file() {
        return Some(exact.display().to_string());
    }
    let prefix = format!("{pkg}_");
    let mut found: Vec<(bool, String)> = std::fs::read_dir(dir)
        .ok()?
        .filter_map(|e| e.ok()?.file_name().into_string().ok())
        .filter_map(|name| {
            let stem = name.strip_suffix(".png")?;
            // `<userId>_` prefix for another user's (work profile, clone) copy of the app.
            let (other_user, rest) = match stem.split_once('_') {
                Some((user, rest)) if !user.is_empty() && user.chars().all(|c| c.is_ascii_digit()) => (true, rest),
                _ => (false, stem),
            };
            // The activity is a class name: it has a dot and no `_` in its first segment, which keeps
            // `com.foo` off another package's `com.foo_bar_<activity>.png`.
            let activity = rest.strip_prefix(&prefix)?;
            let first = activity.split('.').next().unwrap_or_default();
            (activity.contains('.') && !first.contains('_')).then_some((other_user, name))
        })
        .collect();
    found.sort();
    found.into_iter().next().map(|(_, name)| dir.join(name).display().to_string())
}

/// An installed desktop icon for the phone app, e.g. `com.whatsapp` / "WhatsApp" → `whatsapp.svg`.
fn themed_icon(pkg: &str, app: &str) -> Option<String> {
    static CACHE: OnceLock<Mutex<HashMap<String, Option<String>>>> = OnceLock::new();
    let cache = CACHE.get_or_init(Mutex::default);
    if let Some(hit) = cache.lock().unwrap().get(pkg) {
        return hit.clone();
    }
    let found = icon_candidates(pkg, app).into_iter().find_map(|name| find_icon_file(&name));
    cache.lock().unwrap().insert(pkg.to_owned(), found.clone());
    found
}

fn icon_candidates(pkg: &str, app: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut push = |s: String| {
        if !s.is_empty() && !out.contains(&s) {
            out.push(s);
        }
    };
    let app = app.trim().to_lowercase();
    push(app.replace(' ', "-"));
    push(app.replace(' ', ""));
    push(pkg.to_owned());
    // org.telegram.messenger → telegram, com.whatsapp → whatsapp
    let parts: Vec<&str> = pkg.split('.').collect();
    if parts.len() >= 2 {
        push(parts[1].to_lowercase());
    }
    if let Some(last) = parts.last() {
        push(last.to_lowercase());
    }
    out.retain(|s| !matches!(s.as_str(), "android" | "app" | "apps" | "google" | "messenger" | "mobile"));
    out
}

fn find_icon_file(name: &str) -> Option<String> {
    if name.contains('/') {
        return None;
    }
    let mut roots: Vec<PathBuf> = Vec::new();
    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
        roots.push(home.join(".local/share/icons/hicolor"));
        roots.push(home.join(".local/share/icons"));
    }
    for theme in ["Papirus", "hicolor", "breeze", "Adwaita"] {
        roots.push(PathBuf::from("/usr/share/icons").join(theme));
    }
    let sizes = ["scalable", "48x48", "64x64", "128x128", "256x256"];
    for root in &roots {
        for size in sizes {
            for ext in ["svg", "png"] {
                let p = root.join(size).join("apps").join(format!("{name}.{ext}"));
                if p.is_file() {
                    return Some(p.display().to_string());
                }
            }
        }
        for ext in ["svg", "png"] {
            let p = root.join(format!("{name}.{ext}"));
            if p.is_file() {
                return Some(p.display().to_string());
            }
        }
    }
    None
}

/// Per-app mute list (`<state dir>/notif-mute.json`, a JSON array of package names).
fn mute_file() -> PathBuf {
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("smartconnect-linux").join("notif-mute.json")
}

fn muted() -> &'static Mutex<HashSet<String>> {
    static MUTED: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    MUTED.get_or_init(|| {
        let set = std::fs::read(mute_file())
            .ok()
            .and_then(|b| serde_json::from_slice::<Vec<String>>(&b).ok())
            .unwrap_or_default();
        Mutex::new(set.into_iter().collect())
    })
}

pub fn is_muted(pkg: &str) -> bool {
    !pkg.is_empty() && muted().lock().unwrap().contains(pkg)
}

/// Control command `{"cmd":"notif_mute","pkg":"com.example","mute":true|false}` (mute defaults to
/// true); without `pkg` it just lists the muted packages.
pub fn mute_command(v: &Value) -> Value {
    let pkg = v.get("pkg").and_then(Value::as_str).unwrap_or_default().trim();
    let mut set = muted().lock().unwrap();
    if !pkg.is_empty() {
        if v.get("mute").and_then(Value::as_bool).unwrap_or(true) {
            set.insert(pkg.to_owned());
        } else {
            set.remove(pkg);
        }
        let mut list: Vec<&String> = set.iter().collect();
        list.sort();
        let path = mute_file();
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Err(e) = std::fs::write(&path, serde_json::to_vec_pretty(&list).unwrap_or_default()) {
            return json!({ "ok": false, "message": format!("saving {}: {e}", path.display()) });
        }
    }
    let mut list: Vec<&String> = set.iter().collect();
    list.sort();
    json!({ "ok": true, "muted": list })
}

fn first_non_empty(options: &[String]) -> String {
    options.iter().find(|s| !s.trim().is_empty()).cloned().unwrap_or_default()
}

/// Plain text → notification body markup: escape the markup characters, keep line breaks.
fn markup_body(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('\n', "<br/>")
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max { s.to_owned() } else { s.chars().take(max).collect::<String>() + "…" }
}

fn sanitize(s: &str) -> String {
    s.chars().map(|c| if c.is_ascii_alphanumeric() || c == '.' || c == '-' { c } else { '_' }).take(120).collect()
}

/// Decodes a base64 JPEG field (the phone wraps lines, so strip whitespace) into the runtime dir.
fn save_image(name: &str, field: Option<&Value>) -> Option<String> {
    let b64: String = field?.as_str()?.chars().filter(|c| !c.is_whitespace()).collect();
    if b64.is_empty() {
        return None;
    }
    let bytes = B64.decode(b64).ok()?;
    let dir = runtime_dir().join("icons");
    std::fs::create_dir_all(&dir).ok()?;
    let path = dir.join(format!("{name}.jpg"));
    std::fs::write(&path, bytes).ok()?;
    Some(path.display().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn markup_body_keeps_line_breaks_and_escapes() {
        assert_eq!(markup_body("Hi\n<b>&"), "Hi<br/>&lt;b&gt;&amp;");
    }

    #[test]
    fn messaging_body_shows_recent_messages_with_senders() {
        let d = json!({ "conversation_title": "Team", "messages": ["a", "b", "c", "d", "e"],
                        "message_persons": ["Ann", "Bob", "Ann", "Bob", "Cy"] });
        assert_eq!(body_text(&d), "Bob: b\nAnn: c\nBob: d\nCy: e");
        let dm = json!({ "messages": ["hi"], "message_persons": ["Alice"], "content": "hi" });
        assert_eq!(body_text(&dm), "hi");
    }

    #[test]
    fn fingerprint_ignores_images_but_not_text() {
        let a = json!({ "key": "k", "data": { "title": "t", "content": "c", "apk_icon": "AAAA" } });
        let b = json!({ "key": "k", "data": { "title": "t", "content": "c", "apk_icon": "BBBB" } });
        let c = json!({ "key": "k", "data": { "title": "t", "content": "d" } });
        assert_eq!(fingerprint(&a), fingerprint(&b));
        assert_ne!(fingerprint(&a), fingerprint(&c));
        let p1 = json!({ "data": { "progress": { "current": 1, "max": 10 } } });
        let p2 = json!({ "data": { "progress": { "current": 2, "max": 10 } } });
        assert_ne!(fingerprint(&p1), fingerprint(&p2));
    }

    #[test]
    fn icon_candidates_from_package_and_name() {
        assert_eq!(icon_candidates("com.whatsapp", "WhatsApp"), ["whatsapp", "com.whatsapp"]);
        let t = icon_candidates("org.telegram.messenger", "Telegram");
        assert_eq!(t[0], "telegram");
        assert!(icon_candidates("com.google.android.gm", "Gmail").contains(&"gmail".to_owned()));
    }

    #[test]
    fn click_events_for_actions_and_open_only() {
        let key = "0|com.example.clock|7|null|10123";
        let action = json!({ "cmd": 0, "event": 1, "key": key, "version": "1.0", "data": { "action_id": 2 } });
        let ev = click_event(SERVICE_NOTIFY, &action).unwrap();
        assert_eq!(ev["event"], "notification_action");
        assert_eq!(ev["kind"], "action");
        assert_eq!(ev["pkg"], "com.example.clock");
        assert_eq!(ev["actionId"], 2);
        assert_eq!(ev["service"], SERVICE_NOTIFY);
        let open = json!({ "cmd": 0, "event": 2, "key": key, "version": "1.0" });
        let ev = click_event(SERVICE_NOTIFY_WIDGET, &open).unwrap();
        assert_eq!(ev["kind"], "open");
        assert!(ev["actionId"].is_null());
        assert!(click_event(SERVICE_NOTIFY, &reply_command(key, "hi")).is_none(), "replies need no stream");
        assert!(click_event(15, &action).is_none(), "other services");
        assert!(click_event(SERVICE_NOTIFY, &json!({ "cmd": 1, "pkg": "x" })).is_none());
        assert!(click_event(SERVICE_NOTIFY, &json!({ "cmd": 0, "event": 1, "key": "nokey" })).is_none());
    }

    #[test]
    fn package_from_notification_key() {
        assert_eq!(key_package("0|com.whatsapp|12|null|10200").as_deref(), Some("com.whatsapp"));
        assert_eq!(key_package("0|android|1|tag|1000").as_deref(), Some("android"));
        assert_eq!(key_package("broken"), None);
        assert_eq!(key_package("0||1"), None);
    }

    /// A type 3 (messaging) frame; `person` is the style's user Person, as the phone sends it.
    fn messaging(title: &str, conversation: &str, person: &str, icon: &str, senders: &[&str]) -> Value {
        let messages: Vec<String> = senders.iter().enumerate().map(|(i, _)| format!("<message {i}>")).collect();
        json!({ "key": "0|com.example.chat|1|null|10100", "event": 0, "type": 3,
                "data": { "package_name": "com.example.chat", "apk_icon": "QUFB", "title": title,
                          "conversation_title": conversation, "person_name": person, "person_icon": icon,
                          "messages": messages, "message_persons": senders } })
    }

    #[test]
    fn messaging_avatar_is_never_the_self_person() {
        // 1:1 chat: the user Person is the phone owner ("You"); the sender is the title.
        let dm = messaging("<Contact A>", "", "You", "U0VMRg==", &["<Contact A>"]);
        assert_eq!(sender_avatar(&dm), None);
        // Group: the owner again, whoever wrote last; no per-sender icons on the wire.
        let group = messaging("<Group>: <Contact B>", "<Group>", "You", "U0VMRg==", &["<Contact A>", "<Contact B>"]);
        assert_eq!(sender_avatar(&group), None);
        // The owner replied last: still not their own picture.
        let replied = messaging("<Contact A>", "", "You", "U0VMRg==", &["<Contact A>", "You"]);
        assert_eq!(sender_avatar(&replied), None);
    }

    #[test]
    fn messaging_avatar_used_when_the_person_is_the_headline() {
        // Apps such as the dialer set the style's user to the other party (same name as the title).
        let n = messaging("<Contact A>", "", "<Contact A>", "QVZBVEFS", &["<Contact A>"]);
        assert_eq!(sender_avatar(&n).and_then(Value::as_str), Some("QVZBVEFS"));
        // No icon or no name: nothing to show.
        assert_eq!(sender_avatar(&messaging("<Contact A>", "", "<Contact A>", "", &["<Contact A>"])), None);
        assert_eq!(sender_avatar(&messaging("", "", "", "QVZBVEFS", &["<Contact A>"])), None);
        let mut no_icon = messaging("<Contact A>", "", "<Contact A>", "x", &["<Contact A>"]);
        no_icon["data"].as_object_mut().unwrap().remove("person_icon");
        assert_eq!(sender_avatar(&no_icon), None);
    }

    #[test]
    fn call_avatar_is_the_caller() {
        let call = json!({ "key": "k", "type": 6, "data": { "title": "<Caller>", "person_name": "<Caller>", "person_icon": "Q0FMTA==" } });
        assert_eq!(sender_avatar(&call).and_then(Value::as_str), Some("Q0FMTA=="));
        // A call style whose person differs from the title is still the caller.
        let call = json!({ "key": "k", "type": 6, "data": { "title": "Incoming call", "person_name": "<Caller>", "person_icon": "Q0FMTA==" } });
        assert!(sender_avatar(&call).is_some());
        assert_eq!(sender_avatar(&json!({ "key": "k", "type": 0, "data": { "title": "t" } })), None);
    }

    #[test]
    fn cached_app_icon_lookup() {
        let dir = std::env::temp_dir().join(format!("sc-notif-icons-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let touch = |name: &str| std::fs::write(dir.join(name), b"png").unwrap();
        assert_eq!(cached_app_icon(&dir, "com.example.chat"), None);
        touch("11_com.example.chat_com.example.chat.Main.png");
        touch("com.example.chat_bar_com.other.Main.png"); // another package's icon
        let got = cached_app_icon(&dir, "com.example.chat").unwrap();
        assert!(got.ends_with("11_com.example.chat_com.example.chat.Main.png"), "{got}");
        touch("com.example.chat_com.example.chat.Main.png");
        let got = cached_app_icon(&dir, "com.example.chat").unwrap();
        assert!(got.ends_with("/com.example.chat_com.example.chat.Main.png"), "main user first: {got}");
        touch("com.example.chat.png");
        assert!(cached_app_icon(&dir, "com.example.chat").unwrap().ends_with("/com.example.chat.png"));
        assert_eq!(cached_app_icon(&dir, "../etc"), None);
        assert_eq!(cached_app_icon(&dir, ""), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn plain_and_bigtext_bodies() {
        assert_eq!(body_text(&json!({ "content": "short", "big_text": "long version" })), "long version");
        assert_eq!(body_text(&json!({ "content": "short" })), "short");
    }
}
