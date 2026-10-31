//! Phone → PC sharing: links, text and files the phone sends with its "send to PC" action.
//!
//! The phone has no push channel for this: a shared link/text arrives as the `text` field of a
//! service 2 `FileExSendData` (or a service 24 drag-and-drop `cmd 1`), and shared files arrive as a
//! service 2 offer with a fex `port` + certificate `fingerprint` that we pull with
//! [`smartconnect_fex::client::pull`]. Received files are saved in the user's download folder
//! (`~/Downloads`, or the XDG download folder) under a sanitized, non-overwriting name, and the
//! desktop gets a notification with **Open**, **Save as…** and **Dismiss**. We never open or run
//! anything on arrival: every launch goes through the user's own choice in the xdg-desktop-portal.
//!
//! Security rules (see `docs/protocol/file-exchange.md`):
//! - Only `http`/`https` links get an Open action; other text is shown, never launched.
//! - Received names are sanitised (no separators, `..`, control characters or leading dots),
//!   extracted into a private `$XDG_RUNTIME_DIR/smartconnectd/shares/<random>` directory, then
//!   copied to the download folder.
//! - Nothing is auto-opened. The staging copy is deleted on Dismiss, after a successful Open/Save,
//!   when replaced by a newer share, or after [`SHARE_TTL`]; the download copy is the user's.
//! - Spawned commands receive argv only (never a shell), and notification actions are rate-limited.

use std::collections::{HashMap, VecDeque};
use std::ffi::OsString;
use std::net::IpAddr;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use futures_util::StreamExt;
use serde_json::Value;
use tokio::sync::broadcast;
use tracing::{debug, info, warn};
use zbus::zvariant;

use crate::clipboard::{SERVICE_CLIPBOARD, SERVICE_FILE_EX};

/// Drag-and-drop service in the Windows `serviceVersions` map; `cmd 1` carries the text.
pub const SERVICE_DRAG_DROP: i64 = 24;
/// How long a staged share survives without an action.
pub const SHARE_TTL: Duration = Duration::from_secs(30 * 60);
/// Upper bound for a phone → PC file transfer (the fex client enforces it too).
const MAX_SHARE_BYTES: usize = 256 * 1024 * 1024;
/// How long the phone may take to serve a file offer.
const PULL_TIMEOUT: Duration = Duration::from_secs(120);
/// How long the user may keep a portal dialog open.
const PORTAL_TIMEOUT: Duration = Duration::from_secs(10 * 60);
/// Text longer than this is truncated in the notification body.
const TEXT_MAX_CHARS: usize = 2000;
/// A share notification stays in the message tray; GNOME never expires it on its own.
const NOTIFY_TIMEOUT: i32 = 0;

/// Desktop-side operations the share manager needs. A fake implementation exercises the flows in
/// tests without a session bus, a notification daemon or a portal.
pub trait Desktop: Send + Sync + 'static {
    fn notify(&self, popup: Popup) -> impl Future<Output = Result<u32>> + Send;
    fn close(&self, id: u32) -> impl Future<Output = ()> + Send;
    fn open_uri(&self, uri: String) -> impl Future<Output = Result<()>> + Send;
    fn open_path(&self, path: PathBuf) -> impl Future<Output = Result<()>> + Send;
    /// Native save dialog: `directory` selects a destination folder for a received folder.
    fn choose_save(
        &self,
        name: String,
        directory: bool,
    ) -> impl Future<Output = Result<Option<PathBuf>>> + Send;
    fn events(&self) -> broadcast::Receiver<DesktopEvent>;
}

/// A notification button/body click, or the notification disappearing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DesktopEvent {
    Action { id: u32, action: String },
    Closed { id: u32, reason: u32 },
}

/// What the popup is about. Actions are derived from this, never from phone-supplied strings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PopupKind {
    Link,
    Text,
    File,
    Folder,
}

/// One notification to show. The manager owns the text; the desktop layer only renders it.
#[derive(Debug, Clone)]
pub struct Popup {
    pub summary: String,
    pub body: String,
    pub kind: PopupKind,
    pub replaces: u32,
}

impl Popup {
    /// `id, label, id, label, …` for `org.freedesktop.Notifications`.
    fn actions(&self) -> Vec<(&'static str, &'static str)> {
        match self.kind {
            PopupKind::Link => vec![("open", "Open"), ("dismiss", "Dismiss")],
            PopupKind::Text => vec![("dismiss", "Dismiss")],
            PopupKind::File | PopupKind::Folder => {
                vec![
                    ("open", "Open"),
                    ("save", "Save as…"),
                    ("dismiss", "Dismiss"),
                ]
            }
        }
    }
}

/// A link, plain text, or a file/folder that has already been pulled into the staging area.
#[derive(Debug, Clone)]
enum Content {
    Link {
        uri: String,
    },
    Text {
        text: String,
    },
    File {
        /// The private staging directory (always removed when the share is finished).
        dir: PathBuf,
        /// The file/folder to open or save: the copy in Downloads, or the staged copy if saving
        /// there failed.
        path: PathBuf,
        name: String,
        size: u64,
        is_dir: bool,
        /// `path` is a persistent copy in the user's download folder, not staging.
        saved: bool,
    },
}

impl Content {
    fn kind(&self) -> PopupKind {
        match self {
            Self::Link { .. } => PopupKind::Link,
            Self::Text { .. } => PopupKind::Text,
            Self::File { is_dir: true, .. } => PopupKind::Folder,
            Self::File { .. } => PopupKind::File,
        }
    }

    fn summary(&self) -> &'static str {
        match self {
            Self::Link { .. } => "Shared link",
            Self::Text { .. } => "Shared text",
            Self::File {
                is_dir: false,
                saved: true,
                ..
            } => "Shared file saved",
            Self::File {
                is_dir: true,
                saved: true,
                ..
            } => "Shared folder saved",
            Self::File { is_dir: false, .. } => "Shared file",
            Self::File { .. } => "Shared folder",
        }
    }

    fn body(&self) -> String {
        match self {
            Self::Link { uri } => uri.clone(),
            Self::Text { text } => text.chars().take(TEXT_MAX_CHARS).collect(),
            Self::File {
                name,
                size,
                is_dir,
                path,
                saved,
                ..
            } => {
                let what = if *is_dir { "folder" } else { "file" };
                let mut body = format!("{name}\n{} • {what}", format_size(*size));
                if *saved {
                    body.push_str(&format!("\nSaved to {}", path.display()));
                }
                body
            }
        }
    }

    fn staged_dir(&self) -> Option<&Path> {
        match self {
            Self::File { dir, .. } => Some(dir),
            _ => None,
        }
    }
}

#[derive(Clone)]
struct Pending {
    token: u64,
    notification: Option<u32>,
    content: Content,
}

/// What a phone share frame meant. Classification is deliberately conservative.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Incoming {
    Link(String),
    Text(String),
    File(FileShare),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileShare {
    pub name: String,
    pub port: u16,
    pub fingerprint: String,
    pub token: String,
    /// `requestId` or equivalent, used to ignore a duplicate offer on another service.
    pub key: Option<String>,
}

/// A transfer pulled from the phone and unpacked.
#[derive(Debug)]
struct Staged {
    dir: PathBuf,
    path: PathBuf,
    name: String,
    size: u64,
    is_dir: bool,
}

/// Token-bucket-ish limiter: a short gap between popups and a cap per minute.
#[derive(Default)]
struct RateLimiter {
    recent: VecDeque<Instant>,
}

impl RateLimiter {
    fn allow_at(&mut self, now: Instant) -> bool {
        while self
            .recent
            .front()
            .is_some_and(|t| now.duration_since(*t) >= Duration::from_secs(60))
        {
            self.recent.pop_front();
        }
        if self.recent.len() >= 12 {
            return false;
        }
        if self
            .recent
            .back()
            .is_some_and(|t| now.duration_since(*t) < Duration::from_secs(1))
        {
            return false;
        }
        self.recent.push_back(now);
        true
    }
}

/// Pulls phone shares, stages file bytes privately, and shows the action notification.
pub struct ShareManager<D: Desktop> {
    desktop: Arc<D>,
    staging_root: PathBuf,
    ttl: Duration,
    pending: Mutex<Option<Pending>>,
    rate: Mutex<RateLimiter>,
    last_offer: Mutex<Option<(String, Instant)>>,
    next_token: AtomicU64,
}

impl<D: Desktop> ShareManager<D> {
    /// `staging_root` should live under `$XDG_RUNTIME_DIR`; it is created mode 0700.
    pub fn new(desktop: Arc<D>, staging_root: PathBuf) -> Arc<Self> {
        if let Err(e) = create_private_dir(&staging_root) {
            warn!(path = %staging_root.display(), "sharing: can't create the staging directory: {e:#}");
        }
        Arc::new(Self {
            desktop,
            staging_root,
            ttl: SHARE_TTL,
            pending: Mutex::default(),
            rate: Mutex::default(),
            last_offer: Mutex::default(),
            next_token: AtomicU64::new(1),
        })
    }

    /// Starts the desktop-event loop. Call once, after the desktop connection is ready.
    pub fn start(self: &Arc<Self>) {
        let me = self.clone();
        tokio::spawn(async move {
            let mut events = me.desktop.events();
            loop {
                match events.recv().await {
                    Ok(DesktopEvent::Action { id, action }) => me.handle_action(id, &action).await,
                    Ok(DesktopEvent::Closed { id, reason }) => me.handle_closed(id, reason).await,
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        warn!(n, "sharing: missed desktop notification events")
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        });
    }

    #[cfg(test)]
    fn new_with_ttl(desktop: Arc<D>, staging_root: PathBuf, ttl: Duration) -> Arc<Self> {
        let manager = Self::new(desktop, staging_root);
        Arc::new(Self {
            ttl,
            ..Arc::try_unwrap(manager)
                .ok()
                .expect("a fresh manager has one owner")
        })
    }

    /// Handles one frame from a verified, connected phone.
    pub async fn on_frame(self: &Arc<Self>, service_id: i64, data: &Value, phone: IpAddr) {
        let Some(incoming) = classify(service_id, data) else {
            return;
        };
        if !self.rate.lock().unwrap().allow_at(Instant::now()) {
            warn!(
                service_id,
                "sharing: ignoring a share popup over the rate limit"
            );
            return;
        }
        match incoming {
            Incoming::Link(uri) => {
                info!(%uri, "sharing: phone sent a link");
                self.present(Content::Link { uri }).await;
            }
            Incoming::Text(text) => {
                info!(chars = text.chars().count(), "sharing: phone sent text");
                self.present(Content::Text { text }).await;
            }
            Incoming::File(file) => {
                if !self.accept_offer(&file) {
                    return;
                }
                info!(name = %file.name, port = file.port, "sharing: phone offered a file; pulling it");
                self.clone().receive_file(file, phone).await;
            }
        }
    }

    /// Ignores a repeated offer (the phone sometimes announces the same transfer on svc 2 and 24).
    fn accept_offer(&self, file: &FileShare) -> bool {
        let key = file
            .key
            .clone()
            .unwrap_or_else(|| format!("{}:{}", file.port, file.fingerprint));
        let mut last = self.last_offer.lock().unwrap();
        if let Some((seen, at)) = last.as_ref()
            && *seen == key
            && at.elapsed() < Duration::from_secs(60)
        {
            debug!(%key, "sharing: ignoring a duplicate file offer");
            return false;
        }
        *last = Some((key, Instant::now()));
        true
    }

    async fn receive_file(self: Arc<Self>, file: FileShare, phone: IpAddr) {
        let offer = smartconnect_fex::client::Offer {
            host: phone,
            port: file.port,
            fingerprint: file.fingerprint.clone(),
            token: file.token.clone(),
        };
        let pulled = tokio::time::timeout(
            PULL_TIMEOUT,
            smartconnect_fex::client::pull(&offer, MAX_SHARE_BYTES),
        )
        .await;
        let result = match pulled {
            Ok(Ok(pulled)) => smartconnect_fex::zip::read_all(&pulled.archive)
                .and_then(|entries| stage_entries(&self.staging_root, entries, &file.name)),
            Ok(Err(e)) => Err(e),
            Err(_) => Err(anyhow!(
                "the phone did not finish the transfer in {} s",
                PULL_TIMEOUT.as_secs()
            )),
        };
        match result {
            Ok(staged) => {
                info!(name = %staged.name, bytes = staged.size, path = %staged.path.display(), "sharing: file received");
                // Save it where the user expects downloads to land, so dismissing the notification
                // (or missing it) doesn't lose the file. `Save as…` still copies it elsewhere.
                let (path, saved) = match downloads_dir() {
                    Some(downloads) => match auto_save(&staged, &downloads).await {
                        Some(saved) => (saved, true),
                        None => (staged.path.clone(), false),
                    },
                    None => (staged.path.clone(), false),
                };
                let content = Content::File {
                    dir: staged.dir,
                    path,
                    name: staged.name,
                    size: staged.size,
                    is_dir: staged.is_dir,
                    saved,
                };
                self.present(content).await;
            }
            Err(e) => {
                warn!(name = %file.name, "sharing: receiving the file failed: {e:#}");
                let id = self
                    .desktop
                    .notify(Popup {
                        summary: "Couldn't receive a shared file".into(),
                        body: format!("{}\n{}", file.name, first_line(&e.to_string())),
                        kind: PopupKind::Text,
                        replaces: 0,
                    })
                    .await;
                match id {
                    Ok(id) => {
                        let me = self.clone();
                        tokio::spawn(async move {
                            tokio::time::sleep(Duration::from_secs(20)).await;
                            me.desktop.close(id).await;
                        });
                    }
                    Err(e) => debug!("sharing: could not show the error notification: {e:#}"),
                }
            }
        }
    }

    /// Shows one popup, replacing the previous share (same notification id) and cleaning its staging.
    async fn present(self: &Arc<Self>, content: Content) {
        let token = self.next_token.fetch_add(1, Ordering::Relaxed);
        let popup = Popup {
            summary: content.summary().into(),
            body: content.body(),
            kind: content.kind(),
            replaces: 0,
        };
        let (old, replaces) = {
            let mut pending = self.pending.lock().unwrap();
            let old = pending.take();
            let replaces = old.as_ref().and_then(|p| p.notification).unwrap_or(0);
            *pending = Some(Pending {
                token,
                notification: None,
                content,
            });
            (old, replaces)
        };
        if let Some(old) = old {
            // The new notification replaces the old one, so the old id stays on screen.
            cleanup(&old.content).await;
        }

        let id = match self.desktop.notify(Popup { replaces, ..popup }).await {
            Ok(id) => id,
            Err(e) => {
                warn!("sharing: showing the notification failed: {e:#}");
                if let Some(p) = self.take(token) {
                    cleanup(&p.content).await;
                }
                return;
            }
        };
        let current = {
            let mut pending = self.pending.lock().unwrap();
            match pending.as_mut() {
                Some(p) if p.token == token => {
                    p.notification = Some(id);
                    true
                }
                _ => false,
            }
        };
        if !current {
            // A newer share won the race while the notification was being created.
            self.desktop.close(id).await;
            return;
        }
        self.schedule_expiry(token);
    }

    fn schedule_expiry(self: &Arc<Self>, token: u64) {
        let me = self.clone();
        let ttl = self.ttl;
        tokio::spawn(async move {
            tokio::time::sleep(ttl).await;
            let Some(p) = me.take(token) else { return };
            info!("sharing: staged share expired");
            me.close_pending(&p).await;
            cleanup(&p.content).await;
        });
    }

    fn take(&self, token: u64) -> Option<Pending> {
        let mut pending = self.pending.lock().unwrap();
        if pending.as_ref().is_some_and(|p| p.token == token) {
            pending.take()
        } else {
            None
        }
    }

    fn take_by_notification(&self, id: u32) -> Option<Pending> {
        let mut pending = self.pending.lock().unwrap();
        if pending.as_ref().and_then(|p| p.notification) == Some(id) {
            pending.take()
        } else {
            None
        }
    }

    async fn close_pending(&self, p: &Pending) {
        if let Some(id) = p.notification {
            self.desktop.close(id).await;
        }
    }

    /// A button or a click on the notification body.
    async fn handle_action(self: &Arc<Self>, id: u32, action: &str) {
        let Some(p) = self
            .pending
            .lock()
            .unwrap()
            .as_ref()
            .filter(|p| p.notification == Some(id))
            .cloned()
        else {
            return;
        };
        match action {
            "dismiss" => {
                let Some(p) = self.take(p.token) else { return };
                self.close_pending(&p).await;
                cleanup(&p.content).await;
                info!("sharing: dismissed");
            }
            "default" | "open" => match &p.content {
                Content::Link { uri } => match self.desktop.open_uri(uri.clone()).await {
                    Ok(()) => self.finish(p.token).await,
                    Err(e) => warn!(%uri, "sharing: opening the link failed: {e:#}"),
                },
                Content::File {
                    dir,
                    path,
                    is_dir,
                    saved,
                    ..
                } => match self.desktop.open_path(path.clone()).await {
                    Ok(()) if *is_dir && *saved => {
                        // The folder opened is the persistent copy in Downloads; staging is no
                        // longer needed.
                        self.finish(p.token).await;
                    }
                    Ok(()) if *is_dir => {
                        // The file manager is browsing the staged folder now; deleting it would
                        // empty the window. Keep it for a day (the runtime dir is tmpfs).
                        if let Some(p) = self.take(p.token) {
                            self.close_pending(&p).await;
                        }
                        info!(path = %dir.display(), "sharing: folder opened; staging kept for the file manager");
                        let staged = dir.clone();
                        tokio::spawn(async move {
                            tokio::time::sleep(Duration::from_secs(24 * 60 * 60)).await;
                            let _ = tokio::fs::remove_dir_all(&staged).await;
                        });
                    }
                    Ok(()) => self.finish(p.token).await,
                    Err(e) => {
                        warn!(path = %path.display(), "sharing: opening the file failed: {e:#}")
                    }
                },
                Content::Text { .. } => debug!("sharing: text has no open action"),
            },
            "save" => {
                if let Content::File {
                    path, name, is_dir, ..
                } = &p.content
                {
                    match self.desktop.choose_save(name.clone(), *is_dir).await {
                        Ok(Some(dest)) => match save_content(path, &dest, *is_dir).await {
                            Ok(saved) => {
                                info!(path = %saved.display(), "sharing: saved");
                                self.finish(p.token).await;
                            }
                            Err(e) => {
                                warn!(path = %dest.display(), "sharing: saving failed: {e:#}")
                            }
                        },
                        Ok(None) => debug!("sharing: save dialog cancelled"),
                        Err(e) => warn!("sharing: save dialog failed: {e:#}"),
                    }
                }
            }
            other => debug!(action = other, "sharing: unknown notification action"),
        }
    }

    async fn finish(&self, token: u64) {
        let Some(p) = self.take(token) else { return };
        self.close_pending(&p).await;
        cleanup(&p.content).await;
    }

    async fn handle_closed(&self, id: u32, reason: u32) {
        let Some(p) = self.take_by_notification(id) else {
            return;
        };
        info!(reason, "sharing: notification closed");
        cleanup(&p.content).await;
    }
}

/// Classifies a phone frame. Returns `None` for frames that are not shares (most notably the
/// clipboard image offer on service 2, `serviceId` 9, which the clipboard module owns).
pub fn classify(service_id: i64, data: &Value) -> Option<Incoming> {
    match service_id {
        SERVICE_FILE_EX => {
            if data.get("serviceId").and_then(Value::as_i64) == Some(SERVICE_CLIPBOARD) {
                return None;
            }
            // A file offer can carry a `text` field too (the phone puts the file name there), so
            // check the transfer fields before treating it as a text share.
            file_share(data).or_else(|| text_field(data).map(text_incoming))
        }
        SERVICE_DRAG_DROP => {
            if data.get("cmd").and_then(Value::as_i64) != Some(1) {
                return None;
            }
            file_share(data).or_else(|| text_field(data).map(text_incoming))
        }
        SERVICE_CLIPBOARD => {
            // A URL copied on the phone is not a share, but the phone's share sheet can end up as
            // a clipboard update on some builds; only pop up when it is clearly a web link.
            if data.get("msgId").and_then(Value::as_i64) != Some(3) {
                return None;
            }
            let text = data.pointer("/data/text").and_then(Value::as_str)?;
            is_web_link(text).then(|| Incoming::Link(text.to_owned()))
        }
        _ => None,
    }
}

fn text_field(data: &Value) -> Option<&str> {
    ["text", "url", "link", "content"]
        .iter()
        .find_map(|k| data.get(*k).and_then(Value::as_str))
        .or_else(|| data.pointer("/data/text").and_then(Value::as_str))
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

fn text_incoming(text: &str) -> Incoming {
    if is_web_link(text) {
        Incoming::Link(text.to_owned())
    } else {
        Incoming::Text(text.to_owned())
    }
}

fn file_share(data: &Value) -> Option<Incoming> {
    let port = data
        .get("port")
        .and_then(Value::as_u64)
        .and_then(|p| u16::try_from(p).ok())
        .filter(|p| *p > 0)?;
    let fingerprint = data
        .get("fingerprint")
        .and_then(Value::as_str)
        .filter(|f| !f.is_empty())?;
    let raw_name = data
        .get("filePath")
        .or_else(|| data.get("fileName"))
        .or_else(|| data.get("name"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    let name = sanitize_file_name(raw_name);
    let key = data
        .get("requestId")
        .and_then(|v| {
            v.as_str()
                .map(str::to_owned)
                .or_else(|| v.as_u64().map(|n| n.to_string()))
        })
        .or_else(|| {
            data.get("token")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
        });
    Some(Incoming::File(FileShare {
        name,
        port,
        fingerprint: fingerprint.to_owned(),
        token: data
            .get("token")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        key,
    }))
}

/// Only ordinary web links may be launched. Everything else is shown as inert text.
pub fn is_web_link(text: &str) -> bool {
    let lower = text.trim().to_ascii_lowercase();
    if lower
        .chars()
        .any(|c| c.is_control() || c.is_whitespace() || c == '\\')
    {
        return false;
    }
    let rest = if let Some(rest) = lower.strip_prefix("https://") {
        rest
    } else if let Some(rest) = lower.strip_prefix("http://") {
        rest
    } else {
        return false;
    };
    // Need a non-empty host with at least one dot or a bracketed IPv6 literal, and no userinfo
    // tricks such as `https://trusted@evil`.
    let authority = match rest.split(['/', '?', '#']).next() {
        Some(a) if !a.is_empty() => a,
        _ => return false,
    };
    if authority.contains('@') || authority.contains('\\') {
        return false;
    }
    let host = authority.split(':').next().unwrap_or_default();
    let host = host
        .trim_start_matches('[')
        .split(']')
        .next()
        .unwrap_or_default();
    host.contains('.') || authority.starts_with('[')
}

/// Strips directories, traversal, control characters and leading dots. Always returns a usable
/// single path component.
pub fn sanitize_file_name(raw: &str) -> String {
    let base = raw.rsplit(['/', '\\']).next().unwrap_or(raw);
    let mut out = String::with_capacity(base.len());
    for c in base.chars() {
        if c.is_control() {
            continue;
        }
        out.push(match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
            c => c,
        });
    }
    let mut out = out.trim().trim_matches('.').trim().to_owned();
    if out.is_empty() {
        out = "shared-file".into();
    }
    // Keep a normal extension when truncating very long names, never in the middle of a char.
    const MAX: usize = 200;
    if out.len() > MAX {
        let ext = Path::new(&out)
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| format!(".{e}"))
            .unwrap_or_default();
        let mut keep = MAX.saturating_sub(ext.len()).min(out.len());
        while keep > 0 && !out.is_char_boundary(keep) {
            keep -= 1;
        }
        out.truncate(keep);
        out = format!("{}{}", out.trim_end_matches(['.', ' ']), ext);
    }
    out
}

/// 1.2 KB, 3.4 MB, 1.1 GB: the notification has little room.
pub fn format_size(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

fn first_line(s: &str) -> &str {
    s.lines().next().unwrap_or(s)
}

fn create_private_dir(path: &Path) -> Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)
        .with_context(|| format!("creating {}", path.display()))
}

fn random_hex(bytes: usize) -> String {
    use rand::RngCore;
    let mut buf = vec![0u8; bytes];
    rand::thread_rng().fill_bytes(&mut buf);
    hex::encode(buf)
}

/// Writes archive entries into a fresh private directory. Entry names are decoded as relative
/// paths, each component sanitised; nothing can escape the directory or overwrite a peer.
fn stage_entries(
    root: &Path,
    entries: Vec<smartconnect_fex::zip::Entry>,
    hint: &str,
) -> Result<Staged> {
    if entries.is_empty() {
        bail!("the transfer holds no files");
    }
    let dir = root.join(random_hex(12));
    create_private_dir(&dir)?;
    let result = (|| {
        let single = entries.len() == 1 && !entries[0].name.contains(['/', '\\']);
        let size: u64 = entries.iter().map(|e| e.data.len() as u64).sum();
        if single {
            let entry = &entries[0];
            let mut name = sanitize_file_name(&entry.name);
            if name == "shared-file" {
                name = sanitize_file_name(hint);
            }
            let path = unique_path(&dir, Path::new(&name));
            std::fs::write(&path, &entry.data)
                .with_context(|| format!("writing {}", path.display()))?;
            return Ok(Staged {
                dir: dir.clone(),
                path,
                name,
                size,
                is_dir: false,
            });
        }

        let folder = {
            let name = sanitize_file_name(hint);
            if name == "shared-file" {
                "shared-files".to_owned()
            } else {
                name
            }
        };
        let base = dir.join(&folder);
        create_private_dir(&base)?;
        for entry in &entries {
            let rel = safe_relative_path(&entry.name)
                .with_context(|| format!("unsafe entry {:?}", entry.name))?;
            let path = unique_path(&base, &rel);
            if let Some(parent) = path.parent() {
                create_private_dir(parent)?;
            }
            std::fs::write(&path, &entry.data)
                .with_context(|| format!("writing {}", path.display()))?;
        }
        Ok(Staged {
            dir: dir.clone(),
            path: base,
            name: folder,
            size,
            is_dir: true,
        })
    })();
    if result.is_err() {
        let _ = std::fs::remove_dir_all(&dir);
    }
    result
}

/// A zip entry name as a safe relative path (`a/b/c.txt`), rejecting `..` and absolute paths.
fn safe_relative_path(name: &str) -> Option<PathBuf> {
    let mut out = PathBuf::new();
    for part in name.split(['/', '\\']) {
        let part = sanitize_file_name(part);
        if part == "shared-file" || part.is_empty() {
            continue;
        }
        out.push(part);
    }
    (!out.as_os_str().is_empty()).then_some(out)
}

/// Never overwrite: `report.pdf`, then `report (2).pdf`, in the same directory.
fn unique_path(root: &Path, rel: &Path) -> PathBuf {
    let exists = |p: &Path| std::fs::symlink_metadata(p).is_ok();
    let candidate = root.join(rel);
    if !exists(&candidate) {
        return candidate;
    }
    let parent = candidate.parent().unwrap_or(root);
    let stem = candidate
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("shared-file");
    let ext = candidate
        .extension()
        .and_then(|s| s.to_str())
        .map(|e| format!(".{e}"))
        .unwrap_or_default();
    for i in 2..1000 {
        let candidate = parent.join(format!("{stem} ({i}){ext}"));
        if !exists(&candidate) {
            return candidate;
        }
    }
    parent.join(format!("{stem}-{}{ext}", random_hex(4)))
}

async fn cleanup(content: &Content) {
    let Some(dir) = content.staged_dir() else {
        return;
    };
    match tokio::fs::remove_dir_all(dir).await {
        Ok(()) => debug!(path = %dir.display(), "sharing: staging removed"),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => warn!(path = %dir.display(), "sharing: removing staging failed: {e}"),
    }
}

/// Copies a received file/folder into the user's download folder (the default destination for a
/// shared file), never overwriting. Returns the saved path, or `None` when there is no download
/// folder or the copy failed; the staged copy is then still offered with **Save as…**.
async fn auto_save(staged: &Staged, downloads: &Path) -> Option<PathBuf> {
    let dest = unique_path(downloads, Path::new(&staged.name));
    match save_content(&staged.path, &dest, staged.is_dir).await {
        Ok(saved) => {
            info!(path = %saved.display(), "sharing: saved to the download folder");
            Some(saved)
        }
        Err(e) => {
            warn!(path = %dest.display(), "sharing: saving to the download folder failed: {e:#}");
            None
        }
    }
}

/// Copies the staged file/folder to the user's chosen destination.
async fn save_content(source: &Path, dest: &Path, is_dir: bool) -> Result<PathBuf> {
    if is_dir {
        let dest = if dest.exists() {
            let name = dest
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("Shared folder");
            let parent = dest.parent().context("destination has no parent")?;
            let mut n = 2;
            loop {
                let candidate = parent.join(format!("{name} ({n})"));
                if !candidate.exists() {
                    break candidate;
                }
                n += 1;
            }
        } else {
            dest.to_path_buf()
        };
        let src = source.to_path_buf();
        let dst = dest.clone();
        tokio::task::spawn_blocking(move || copy_tree(&src, &dst))
            .await
            .context("copy task panicked")??;
        return Ok(dest);
    }
    if let Some(parent) = dest.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    tokio::fs::copy(source, dest)
        .await
        .with_context(|| format!("copying to {}", dest.display()))?;
    Ok(dest.to_path_buf())
}

fn copy_tree(src: &Path, dst: &Path) -> Result<()> {
    create_private_dir(dst)?;
    for entry in std::fs::read_dir(src).with_context(|| format!("reading {}", src.display()))? {
        let entry = entry?;
        let to = dst.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_tree(&entry.path(), &to)?;
        } else {
            std::fs::copy(entry.path(), &to)
                .with_context(|| format!("copying {}", entry.path().display()))?;
        }
    }
    Ok(())
}

/// A `file://` URI from a portal response to a local path. Remote hosts and non-file URIs are
/// rejected.
pub fn uri_to_path(uri: &str) -> Option<PathBuf> {
    let rest = uri.strip_prefix("file://")?;
    let rest = match rest.strip_prefix("localhost/") {
        Some(tail) => format!("/{tail}"),
        None if rest.starts_with('/') => rest.to_owned(),
        None => return None,
    };
    let bytes = rest.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let (Some(hi), Some(lo)) = (hex_digit(bytes[i + 1]), hex_digit(bytes[i + 2]))
        {
            out.push(hi << 4 | lo);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    Some(PathBuf::from(OsString::from_vec(out)))
}

/// `file://` URI with every byte outside the unreserved set percent-encoded.
pub fn path_to_file_uri(path: &Path) -> String {
    let mut out = String::from("file://");
    for &b in path.as_os_str().as_bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~' | b'/') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn hex_digit(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

fn downloads_dir() -> Option<PathBuf> {
    // The desktop's configured download folder. `XDG_DOWNLOAD_DIR` is usually *not* exported to
    // services, so also read the `user-dirs.dirs` file the desktop writes.
    let from_file = || {
        let config = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
        let text = std::fs::read_to_string(config.join("user-dirs.dirs")).ok()?;
        user_dir_from(&text, "XDG_DOWNLOAD_DIR")
    };
    if let Some(dir) = std::env::var_os("XDG_DOWNLOAD_DIR")
        .map(PathBuf::from)
        .filter(|p| p.is_dir())
        .or_else(|| from_file().filter(|p| p.is_dir()))
    {
        return Some(dir);
    }
    let home = std::env::var_os("HOME").map(PathBuf::from)?;
    let downloads = home.join("Downloads");
    Some(if downloads.is_dir() { downloads } else { home })
}

/// `XDG_DOWNLOAD_DIR="$HOME/Downloads"` (or a plain path) from `user-dirs.dirs`.
fn user_dir_from(text: &str, key: &str) -> Option<PathBuf> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default();
    for line in text.lines() {
        let line = line.trim();
        let Some(rest) = line.strip_prefix(key) else {
            continue;
        };
        let Some(value) = rest.strip_prefix('=') else {
            continue;
        };
        let value = value.trim().trim_matches('"');
        if value.is_empty() {
            continue;
        }
        if let Some(rest) = value.strip_prefix("$HOME/") {
            return Some(home.join(rest));
        }
        if value == "$HOME" {
            return Some(home);
        }
        return Some(PathBuf::from(value));
    }
    None
}

// ---------------------------------------------------------------------------------------------
// System desktop: org.freedesktop.Notifications + xdg-desktop-portal
// ---------------------------------------------------------------------------------------------

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

    #[zbus(signal)]
    fn action_invoked(&self, id: u32, action_key: String) -> zbus::Result<()>;

    #[zbus(signal)]
    fn notification_closed(&self, id: u32, reason: u32) -> zbus::Result<()>;
}

#[zbus::proxy(
    interface = "org.freedesktop.portal.OpenURI",
    default_service = "org.freedesktop.portal.Desktop",
    default_path = "/org/freedesktop/portal/desktop"
)]
trait PortalOpenUri {
    #[zbus(name = "OpenURI")]
    fn open_uri(
        &self,
        parent_window: &str,
        uri: &str,
        options: HashMap<&str, zvariant::Value<'_>>,
    ) -> zbus::Result<zvariant::OwnedObjectPath>;

    fn open_file(
        &self,
        parent_window: &str,
        fd: zvariant::OwnedFd,
        options: HashMap<&str, zvariant::Value<'_>>,
    ) -> zbus::Result<zvariant::OwnedObjectPath>;

    fn open_directory(
        &self,
        parent_window: &str,
        fd: zvariant::OwnedFd,
        options: HashMap<&str, zvariant::Value<'_>>,
    ) -> zbus::Result<zvariant::OwnedObjectPath>;
}

#[zbus::proxy(
    interface = "org.freedesktop.portal.FileChooser",
    default_service = "org.freedesktop.portal.Desktop",
    default_path = "/org/freedesktop/portal/desktop"
)]
trait PortalFileChooser {
    fn save_file(
        &self,
        parent_window: &str,
        title: &str,
        options: HashMap<&str, zvariant::Value<'_>>,
    ) -> zbus::Result<zvariant::OwnedObjectPath>;

    fn select_folder(
        &self,
        parent_window: &str,
        title: &str,
        options: HashMap<&str, zvariant::Value<'_>>,
    ) -> zbus::Result<zvariant::OwnedObjectPath>;
}

/// The real desktop: GNOME's notification daemon and the xdg-desktop-portal dialogs.
pub struct SystemDesktop {
    conn: zbus::Connection,
    notif: NotificationsProxy<'static>,
    events: broadcast::Sender<DesktopEvent>,
}

impl SystemDesktop {
    pub async fn connect() -> Result<Arc<Self>> {
        let conn = zbus::Connection::session()
            .await
            .context("connecting to the session bus")?;
        let notif = NotificationsProxy::new(&conn)
            .await
            .context("org.freedesktop.Notifications is not available")?;
        let (events, _) = broadcast::channel(64);
        let me = Arc::new(Self {
            conn,
            notif,
            events,
        });

        match zbus::fdo::DBusProxy::new(&me.conn).await {
            Ok(bus) => match zbus::names::BusName::try_from("org.freedesktop.portal.Desktop") {
                Ok(name) => match bus.name_has_owner(name).await {
                    Ok(true) => info!("sharing: xdg-desktop-portal is available"),
                    Ok(false) => warn!(
                        "sharing: xdg-desktop-portal is not running; Open/Save will fall back to xdg-open/gio/zenity"
                    ),
                    Err(e) => warn!("sharing: can't query the desktop portal: {e}"),
                },
                Err(e) => debug!("sharing: invalid portal bus name: {e}"),
            },
            Err(e) => warn!("sharing: can't query the session bus: {e}"),
        }
        let gnome_portal = Path::new("/usr/libexec/xdg-desktop-portal-gnome").exists();
        if !gnome_portal {
            warn!(
                "sharing: xdg-desktop-portal-gnome was not found at /usr/libexec/xdg-desktop-portal-gnome; \
                 file dialogs may fall back to another portal backend"
            );
        }

        tokio::spawn(me.clone().follow_signals());
        Ok(me)
    }

    async fn follow_signals(self: Arc<Self>) {
        let (Ok(mut actions), Ok(mut closed)) = (
            self.notif.receive_action_invoked().await,
            self.notif.receive_notification_closed().await,
        ) else {
            warn!("sharing: can't subscribe to notification signals; Open/Save buttons won't work");
            return;
        };
        loop {
            tokio::select! {
                Some(sig) = actions.next() => {
                    if let Ok(args) = sig.args() {
                        let _ = self.events.send(DesktopEvent::Action { id: args.id, action: args.action_key });
                    }
                }
                Some(sig) = closed.next() => {
                    if let Ok(args) = sig.args() {
                        let _ = self.events.send(DesktopEvent::Closed { id: args.id, reason: args.reason });
                    }
                }
                else => break,
            }
        }
    }

    async fn notify_inner(&self, popup: Popup) -> Result<u32> {
        let actions: Vec<&str> = popup
            .actions()
            .iter()
            .flat_map(|(id, label)| [*id, *label])
            .collect();
        let mut hints: HashMap<&str, zvariant::Value<'_>> = HashMap::new();
        hints.insert(
            "desktop-entry",
            zvariant::Value::from("dev.smartconnect.Linux"),
        );
        hints.insert("resident", zvariant::Value::Bool(true));
        hints.insert("urgency", zvariant::Value::U8(1));
        hints.insert("category", zvariant::Value::from("transfer"));
        let id = self
            .notif
            .notify(
                "Smart Connect",
                popup.replaces,
                "dev.smartconnect.Linux",
                &popup.summary,
                &popup.body,
                &actions,
                hints,
                NOTIFY_TIMEOUT,
            )
            .await
            .context("org.freedesktop.Notifications.Notify")?;
        info!(id, replaces = popup.replaces, kind = ?popup.kind, "sharing: notification shown");
        Ok(id)
    }

    async fn request_stream(&self) -> Result<(String, zbus::proxy::SignalStream<'static>, String)> {
        let token = format!("smartconnect_{}", random_hex(8));
        let sender = self
            .conn
            .unique_name()
            .context("the session bus gave us no unique name")?
            .as_str()
            .trim_start_matches(':')
            .replace('.', "_");
        let path = format!("/org/freedesktop/portal/desktop/request/{sender}/{token}");
        let proxy = zbus::Proxy::new(
            &self.conn,
            "org.freedesktop.portal.Desktop",
            path.clone(),
            "org.freedesktop.portal.Request",
        )
        .await
        .context("creating the portal request proxy")?;
        let stream = proxy
            .receive_signal("Response")
            .await
            .context("subscribing to the portal response")?;
        Ok((token, stream, path))
    }

    async fn wait_response(
        &self,
        stream: zbus::proxy::SignalStream<'static>,
        what: &str,
    ) -> Result<HashMap<String, zvariant::OwnedValue>> {
        let mut stream = stream;
        let msg = tokio::time::timeout(PORTAL_TIMEOUT, stream.next())
            .await
            .with_context(|| {
                format!(
                    "{what}: the portal didn't answer within {} s",
                    PORTAL_TIMEOUT.as_secs()
                )
            })?
            .with_context(|| format!("{what}: the portal response stream ended"))?;
        let body = msg.body();
        let (code, results): (u32, HashMap<String, zvariant::OwnedValue>) = body
            .deserialize()
            .with_context(|| format!("{what}: bad portal response"))?;
        match code {
            0 => Ok(results),
            1 => Err(anyhow::Error::new(Cancelled)),
            other => bail!("{what}: the portal returned response {other}"),
        }
    }

    async fn open_uri_inner(&self, uri: String) -> Result<()> {
        let portal = PortalOpenUriProxy::new(&self.conn)
            .await
            .context("xdg-desktop-portal OpenURI is unavailable")?;
        let (token, stream, expected) = self.request_stream().await?;
        let mut options: HashMap<&str, zvariant::Value<'_>> = HashMap::new();
        options.insert("ask", zvariant::Value::Bool(true));
        options.insert("handle_token", zvariant::Value::from(token));
        let handle = portal
            .open_uri("", &uri, options)
            .await
            .context("OpenURI.OpenURI")?;
        if handle.as_str() != expected {
            debug!(
                handle = handle.as_str(),
                expected, "sharing: portal used another request path"
            );
        }
        self.wait_response(stream, "OpenURI").await?;
        info!(%uri, "sharing: portal chose an application for the link");
        Ok(())
    }

    async fn open_path_inner(&self, path: PathBuf) -> Result<()> {
        let directory = path.is_dir();
        // The portal takes an fd, never a path: a regular file opens in the app the user picks from
        // `ask`, and a directory opens in their file manager (OpenFile rejects directories).
        let file = tokio::fs::File::open(&path)
            .await
            .with_context(|| format!("opening {}", path.display()))?;
        let std_file: std::fs::File = file.into_std().await;
        let fd = zvariant::OwnedFd::from(std::os::fd::OwnedFd::from(std_file));
        let portal = PortalOpenUriProxy::new(&self.conn)
            .await
            .context("xdg-desktop-portal OpenURI is unavailable")?;
        let (token, stream, expected) = self.request_stream().await?;
        let mut options: HashMap<&str, zvariant::Value<'_>> = HashMap::new();
        // `ask` belongs to OpenURI.OpenFile (a regular file). OpenDirectory has no app picker: it
        // opens the folder in the user's file manager.
        if !directory {
            options.insert("ask", zvariant::Value::Bool(true));
        }
        options.insert("handle_token", zvariant::Value::from(token));
        let (handle, what) = if directory {
            (
                portal
                    .open_directory("", fd, options)
                    .await
                    .context("OpenURI.OpenDirectory")?,
                "OpenDirectory",
            )
        } else {
            (
                portal
                    .open_file("", fd, options)
                    .await
                    .context("OpenURI.OpenFile")?,
                "OpenFile",
            )
        };
        if handle.as_str() != expected {
            debug!(
                handle = handle.as_str(),
                expected, "sharing: portal used another request path"
            );
        }
        self.wait_response(stream, what).await?;
        info!(path = %path.display(), what, "sharing: portal chose an application");
        Ok(())
    }

    async fn choose_save_inner(&self, name: String, directory: bool) -> Result<Option<PathBuf>> {
        let portal = PortalFileChooserProxy::new(&self.conn)
            .await
            .context("xdg-desktop-portal FileChooser is unavailable")?;
        let (token, stream, expected) = self.request_stream().await?;
        let mut options: HashMap<&str, zvariant::Value<'_>> = HashMap::new();
        options.insert("handle_token", zvariant::Value::from(token));
        options.insert("modal", zvariant::Value::Bool(true));
        if let Some(downloads) = downloads_dir() {
            let mut bytes = downloads.as_os_str().as_bytes().to_vec();
            bytes.push(0);
            options.insert(
                "current_folder",
                zvariant::Value::Array(zvariant::Array::from(bytes)),
            );
        }
        let title = if directory {
            "Choose where to save the folder"
        } else {
            "Save shared file"
        };
        // SelectFolder is optional in the portal: xdg-desktop-portal 1.22 with the GNOME backend
        // here does not export it at all. Fall back to SaveFile, where the chosen name becomes the
        // folder to create. `chosen_is_folder` tells the result handling whether the portal
        // returned a parent folder (SelectFolder) or the target itself (SaveFile).
        options.insert("current_name", zvariant::Value::from(name.clone()));
        let (handle, chosen_is_folder) = if directory {
            match portal.select_folder("", title, options.clone()).await {
                Ok(handle) => (handle, true),
                Err(zbus::Error::MethodError(error, _, _))
                    if error.as_str() == "org.freedesktop.DBus.Error.UnknownMethod" =>
                {
                    warn!(
                        "sharing: this portal has no FileChooser.SelectFolder; saving the folder through SaveFile"
                    );
                    (
                        portal
                            .save_file("", title, options)
                            .await
                            .context("FileChooser.SaveFile")?,
                        false,
                    )
                }
                Err(e) => return Err(e).context("FileChooser.SelectFolder"),
            }
        } else {
            (
                portal
                    .save_file("", title, options)
                    .await
                    .context("FileChooser.SaveFile")?,
                false,
            )
        };
        if handle.as_str() != expected {
            debug!(
                handle = handle.as_str(),
                expected, "sharing: portal used another request path"
            );
        }
        let results = self.wait_response(stream, "FileChooser").await?;
        let Some(uri) = result_uris(&results).first().cloned() else {
            return Err(anyhow!("the file chooser returned no destination"));
        };
        let dest = uri_to_path(&uri)
            .with_context(|| format!("the chooser returned a non-local URI: {uri}"))?;
        Ok(Some(if directory && chosen_is_folder {
            dest.join(&name)
        } else {
            dest
        }))
    }

    async fn fallback_open(&self, target: &str, is_uri: bool) -> Result<()> {
        let mut errors = Vec::new();
        let mut xdg = tokio::process::Command::new("xdg-open");
        xdg.arg(target);
        match xdg.status().await {
            Ok(status) if status.success() => return Ok(()),
            Ok(status) => errors.push(format!("xdg-open exited with {status}")),
            Err(e) => errors.push(format!("xdg-open: {e}")),
        }
        let mut gio = tokio::process::Command::new("gio");
        gio.arg("open").arg(target);
        match gio.status().await {
            Ok(status) if status.success() => return Ok(()),
            Ok(status) => errors.push(format!("gio open exited with {status}")),
            Err(e) => errors.push(format!("gio open: {e}")),
        }
        let kind = if is_uri { "link" } else { "file" };
        bail!(
            "no portal and no working opener for the {kind}: {}",
            errors.join("; ")
        )
    }

    async fn fallback_save(&self, name: &str, directory: bool) -> Result<Option<PathBuf>> {
        let mut cmd = tokio::process::Command::new("zenity");
        cmd.arg("--file-selection");
        if directory {
            cmd.arg("--directory")
                .arg("--title=Choose where to save the folder");
        } else {
            cmd.arg("--save").arg("--title=Save shared file");
        }
        let start = downloads_dir().map(|d| if directory { d } else { d.join(name) });
        if let Some(start) = start {
            cmd.arg(format!("--filename={}", start.display()));
        }
        let output = cmd.output().await.context("starting zenity")?;
        if !output.status.success() {
            return Ok(None); // Cancel exits non-zero.
        }
        let chosen = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        if chosen.is_empty() {
            return Ok(None);
        }
        let dest = PathBuf::from(chosen);
        Ok(Some(if directory { dest.join(name) } else { dest }))
    }
}

/// Portal "the user cancelled": not an error worth falling back on.
#[derive(Debug)]
struct Cancelled;

impl std::fmt::Display for Cancelled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the user cancelled the dialog")
    }
}

impl std::error::Error for Cancelled {}

fn is_cancelled(e: &anyhow::Error) -> bool {
    e.downcast_ref::<Cancelled>().is_some()
}

fn result_uris(results: &HashMap<String, zvariant::OwnedValue>) -> Vec<String> {
    results
        .get("uris")
        .and_then(|v| v.try_clone().ok())
        .and_then(|v| Vec::<String>::try_from(v).ok())
        .unwrap_or_default()
}

impl Desktop for SystemDesktop {
    async fn notify(&self, popup: Popup) -> Result<u32> {
        self.notify_inner(popup).await
    }

    async fn close(&self, id: u32) {
        if let Err(e) = self.notif.close_notification(id).await {
            debug!(id, "sharing: closing the notification failed: {e}");
        }
    }

    async fn open_uri(&self, uri: String) -> Result<()> {
        match self.open_uri_inner(uri.clone()).await {
            Ok(()) => Ok(()),
            Err(e) if is_cancelled(&e) => Err(e),
            Err(e) => {
                warn!(%uri, "sharing: OpenURI failed, falling back: {e:#}");
                self.fallback_open(&uri, true).await
            }
        }
    }

    async fn open_path(&self, path: PathBuf) -> Result<()> {
        match self.open_path_inner(path.clone()).await {
            Ok(()) => Ok(()),
            Err(e) if is_cancelled(&e) => Err(e),
            Err(e) => {
                warn!(path = %path.display(), "sharing: OpenFile failed, falling back: {e:#}");
                self.fallback_open(&path.to_string_lossy(), false).await
            }
        }
    }

    async fn choose_save(&self, name: String, directory: bool) -> Result<Option<PathBuf>> {
        match self.choose_save_inner(name.clone(), directory).await {
            Ok(dest) => Ok(dest),
            Err(e) if is_cancelled(&e) => Ok(None),
            Err(e) => {
                warn!("sharing: FileChooser failed, falling back to zenity: {e:#}");
                self.fallback_save(&name, directory).await
            }
        }
    }

    fn events(&self) -> broadcast::Receiver<DesktopEvent> {
        self.events.subscribe()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::atomic::{AtomicBool, Ordering};

    #[derive(Default)]
    struct FakeDesktop {
        shown: Mutex<Vec<Popup>>,
        closed: Mutex<Vec<u32>>,
        opened_uris: Mutex<Vec<String>>,
        opened_paths: Mutex<Vec<PathBuf>>,
        save_to: Mutex<Option<PathBuf>>,
        choose_saved: Mutex<Vec<(String, bool)>>,
        tx: Mutex<Option<broadcast::Sender<DesktopEvent>>>,
        fail_notify: AtomicBool,
    }

    impl FakeDesktop {
        fn with_save(path: PathBuf) -> Arc<Self> {
            let me = Arc::new(Self::default());
            *me.save_to.lock().unwrap() = Some(path);
            me
        }

        fn popups(&self) -> Vec<Popup> {
            self.shown.lock().unwrap().clone()
        }

        fn sender(&self) -> broadcast::Sender<DesktopEvent> {
            self.tx
                .lock()
                .unwrap()
                .get_or_insert_with(|| broadcast::channel(8).0)
                .clone()
        }
    }

    impl Desktop for FakeDesktop {
        async fn notify(&self, popup: Popup) -> Result<u32> {
            if self.fail_notify.load(Ordering::SeqCst) {
                bail!("fake notification failure");
            }
            let mut shown = self.shown.lock().unwrap();
            shown.push(popup);
            Ok(100 + shown.len() as u32)
        }

        async fn close(&self, id: u32) {
            self.closed.lock().unwrap().push(id);
        }

        async fn open_uri(&self, uri: String) -> Result<()> {
            self.opened_uris.lock().unwrap().push(uri);
            Ok(())
        }

        async fn open_path(&self, path: PathBuf) -> Result<()> {
            self.opened_paths.lock().unwrap().push(path);
            Ok(())
        }

        async fn choose_save(&self, name: String, directory: bool) -> Result<Option<PathBuf>> {
            self.choose_saved.lock().unwrap().push((name, directory));
            Ok(self.save_to.lock().unwrap().clone())
        }

        fn events(&self) -> broadcast::Receiver<DesktopEvent> {
            self.sender().subscribe()
        }
    }

    struct TestDir(PathBuf);

    impl TestDir {
        fn new(tag: &str) -> Self {
            let path = std::env::temp_dir()
                .join(format!("smartconnect-share-{}-{tag}", std::process::id()));
            let _ = std::fs::remove_dir_all(&path);
            create_private_dir(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn link_allowlist_is_strict() {
        assert!(is_web_link("https://example.com/a?b=1#c"));
        assert!(is_web_link("HTTP://example.com"));
        assert!(is_web_link("https://[::1]/x"));
        assert!(!is_web_link("http://localhost/x"));
        assert!(!is_web_link("javascript:alert(1)"));
        assert!(!is_web_link("file:///etc/passwd"));
        assert!(!is_web_link("https://user@evil.example.com/"));
        assert!(!is_web_link("https://example.com/\nrm -rf /"));
        assert!(!is_web_link("data:text/html,<script>"));
        assert!(!is_web_link("smb://example.com/share"));
    }

    #[test]
    fn classification_keeps_clipboard_images_out() {
        let image = json!({"serviceId": 9, "port": 50000, "fingerprint": "aa"});
        assert_eq!(classify(SERVICE_FILE_EX, &image), None);

        let link = json!({"serviceId": 2, "text": "https://example.com/x"});
        assert_eq!(
            classify(SERVICE_FILE_EX, &link),
            Some(Incoming::Link("https://example.com/x".into()))
        );

        let text = json!({"serviceId": 2, "text": "hello"});
        assert_eq!(
            classify(SERVICE_FILE_EX, &text),
            Some(Incoming::Text("hello".into()))
        );

        let file = json!({"serviceId": 2, "port": 50123, "fingerprint": "abcd", "filePath": "/sdcard/Download/a.pdf", "requestId": "7"});
        let Some(Incoming::File(f)) = classify(SERVICE_FILE_EX, &file) else {
            panic!("expected a file")
        };
        assert_eq!(f.name, "a.pdf");
        assert_eq!(f.port, 50123);
        assert_eq!(f.key.as_deref(), Some("7"));

        // A FileExSendData offer carries `text` too; the transfer fields must win.
        let file_with_text = json!({"serviceId": 2, "text": "a.pdf", "port": 50001, "fingerprint": "ff", "filePath": "/a/a.pdf"});
        assert!(matches!(
            classify(SERVICE_FILE_EX, &file_with_text),
            Some(Incoming::File(_))
        ));

        let drag = json!({"cmd": 1, "text": "note"});
        assert_eq!(
            classify(SERVICE_DRAG_DROP, &drag),
            Some(Incoming::Text("note".into()))
        );
        assert_eq!(
            classify(SERVICE_DRAG_DROP, &json!({"cmd": 2, "text": "note"})),
            None
        );

        let clip = json!({"msgId": 3, "data": {"text": "https://example.com/c"}});
        assert_eq!(
            classify(SERVICE_CLIPBOARD, &clip),
            Some(Incoming::Link("https://example.com/c".into()))
        );
        assert_eq!(
            classify(
                SERVICE_CLIPBOARD,
                &json!({"msgId": 3, "data": {"text": "hello"}})
            ),
            None
        );
    }

    #[test]
    fn names_cannot_escape_or_hide() {
        assert_eq!(
            sanitize_file_name("/storage/emulated/0/Download/report.pdf"),
            "report.pdf"
        );
        assert_eq!(
            sanitize_file_name("../../.ssh/authorized_keys"),
            "authorized_keys"
        );
        assert_eq!(sanitize_file_name("..\\.bashrc"), "bashrc");
        assert_eq!(sanitize_file_name("a\u{0}b\nc.txt"), "abc.txt");
        assert_eq!(sanitize_file_name("..."), "shared-file");
        assert_eq!(sanitize_file_name(""), "shared-file");
        assert_eq!(sanitize_file_name("a/b\\c:d*e?f"), "c_d_e_f");
        assert_eq!(sanitize_file_name(".."), "shared-file");
        assert_eq!(
            safe_relative_path("a/../../b.txt"),
            Some(PathBuf::from("a/b.txt"))
        );
        assert_eq!(safe_relative_path("../.."), None);
    }

    #[test]
    fn rate_limiter_caps_bursts() {
        let mut limiter = RateLimiter::default();
        let start = Instant::now();
        assert!(limiter.allow_at(start));
        assert!(!limiter.allow_at(start + Duration::from_millis(100)));
        let mut allowed = 1;
        for i in 1..20 {
            if limiter.allow_at(start + Duration::from_secs(i)) {
                allowed += 1;
            }
        }
        assert!(allowed <= 12, "allowed {allowed}");
    }

    #[test]
    fn size_is_human_readable() {
        assert_eq!(format_size(12), "12 B");
        assert_eq!(format_size(1536), "1.5 KB");
        assert_eq!(format_size(5 * 1024 * 1024), "5.0 MB");
    }

    #[test]
    fn user_dirs_files_name_the_download_folder() {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_default();
        assert_eq!(
            user_dir_from("XDG_DOWNLOAD_DIR=\"$HOME/Downloads\"\n", "XDG_DOWNLOAD_DIR"),
            Some(home.join("Downloads"))
        );
        assert_eq!(
            user_dir_from(
                "# written by xdg-user-dirs\nXDG_DOWNLOAD_DIR=\"/srv/dl\"\n",
                "XDG_DOWNLOAD_DIR"
            ),
            Some(PathBuf::from("/srv/dl"))
        );
        assert_eq!(
            user_dir_from("XDG_DESKTOP_DIR=\"$HOME/Desktop\"\n", "XDG_DOWNLOAD_DIR"),
            None
        );
    }

    #[test]
    fn a_saved_share_body_names_the_download_path() {
        let content = Content::File {
            dir: PathBuf::from("/tmp/stage"),
            path: PathBuf::from("/home/a/Downloads/report.pdf"),
            name: "report.pdf".into(),
            size: 6,
            is_dir: false,
            saved: true,
        };
        assert_eq!(content.summary(), "Shared file saved");
        assert!(
            content
                .body()
                .contains("Saved to /home/a/Downloads/report.pdf"),
            "{}",
            content.body()
        );
    }

    #[tokio::test]
    async fn a_received_file_is_saved_to_downloads_without_overwriting() {
        let dir = TestDir::new("autosave");
        let staged_dir = dir.0.join("shares/staged");
        create_private_dir(&staged_dir).unwrap();
        let source = staged_dir.join("report.pdf");
        std::fs::write(&source, b"new").unwrap();
        let downloads = dir.0.join("Downloads");
        create_private_dir(&downloads).unwrap();
        std::fs::write(downloads.join("report.pdf"), b"old").unwrap();
        let staged = Staged {
            dir: staged_dir,
            path: source,
            name: "report.pdf".into(),
            size: 3,
            is_dir: false,
        };
        let saved = auto_save(&staged, &downloads)
            .await
            .expect("the file is saved");
        assert_eq!(saved, downloads.join("report (2).pdf"));
        assert_eq!(std::fs::read(&saved).unwrap(), b"new");
        assert_eq!(std::fs::read(downloads.join("report.pdf")).unwrap(), b"old");
    }

    #[test]
    fn unique_path_does_not_overwrite_broken_symlinks() {
        let dir = TestDir::new("symlink-unique");
        let symlink = dir.0.join("target.txt");
        #[cfg(unix)]
        std::os::unix::fs::symlink(dir.0.join("nonexistent.txt"), &symlink).unwrap();
        let unique = unique_path(&dir.0, Path::new("target.txt"));
        assert_eq!(unique, dir.0.join("target (2).txt"));
    }

    #[test]
    fn portal_uris_become_paths() {
        assert_eq!(
            uri_to_path("file:///home/a/report%20one.pdf"),
            Some(PathBuf::from("/home/a/report one.pdf"))
        );
        assert_eq!(
            uri_to_path("file://localhost/tmp/x"),
            Some(PathBuf::from("/tmp/x"))
        );
        assert_eq!(uri_to_path("file://remote/tmp/x"), None);
        assert_eq!(uri_to_path("https://example.com/x"), None);
    }

    #[test]
    fn file_uris_round_trip() {
        let path = Path::new("/home/a/My Folder/report one.pdf");
        let uri = path_to_file_uri(path);
        assert_eq!(uri, "file:///home/a/My%20Folder/report%20one.pdf");
        assert_eq!(uri_to_path(&uri), Some(path.to_path_buf()));
    }

    #[test]
    fn staging_uses_safe_names_and_detects_folders() {
        let dir = TestDir::new("stage");
        let entries = vec![
            smartconnect_fex::zip::Entry {
                name: "../../evil.txt".into(),
                data: b"x".to_vec(),
            },
            smartconnect_fex::zip::Entry {
                name: "sub/ok.txt".into(),
                data: b"y".to_vec(),
            },
        ];
        let staged = stage_entries(&dir.0, entries, "My Folder").unwrap();
        assert!(staged.is_dir);
        assert_eq!(staged.name, "My Folder");
        assert_eq!(staged.size, 2);
        assert!(staged.path.starts_with(&dir.0));
        assert!(staged.path.join("evil.txt").is_file());
        assert!(staged.path.join("sub/ok.txt").is_file());
        assert!(!dir.0.join("evil.txt").exists());
    }

    #[tokio::test]
    async fn a_link_shows_a_popup_and_never_opens_itself() {
        let dir = TestDir::new("link");
        let fake = Arc::new(FakeDesktop::default());
        let manager = ShareManager::new(fake.clone(), dir.0.join("shares"));
        manager
            .on_frame(
                SERVICE_DRAG_DROP,
                &json!({"cmd": 1, "text": "https://example.com/x"}),
                "127.0.0.1".parse().unwrap(),
            )
            .await;

        let popups = fake.popups();
        assert_eq!(popups.len(), 1);
        assert_eq!(popups[0].kind, PopupKind::Link);
        assert!(popups[0].body.contains("https://example.com/x"));
        assert_eq!(
            popups[0]
                .actions()
                .iter()
                .map(|(id, _)| *id)
                .collect::<Vec<_>>(),
            vec!["open", "dismiss"]
        );
        assert!(
            fake.opened_uris.lock().unwrap().is_empty(),
            "nothing may open on arrival"
        );
        assert!(fake.opened_paths.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn plain_text_has_no_open_action() {
        let dir = TestDir::new("text");
        let fake = Arc::new(FakeDesktop::default());
        let manager = ShareManager::new(fake.clone(), dir.0.join("shares"));
        manager
            .on_frame(
                SERVICE_DRAG_DROP,
                &json!({"cmd": 1, "text": "rm -rf /"}),
                "127.0.0.1".parse().unwrap(),
            )
            .await;
        let popups = fake.popups();
        assert_eq!(popups[0].kind, PopupKind::Text);
        assert_eq!(
            popups[0]
                .actions()
                .iter()
                .map(|(id, _)| *id)
                .collect::<Vec<_>>(),
            vec!["dismiss"]
        );
    }

    #[tokio::test]
    async fn open_action_uses_the_selected_application_and_closes() {
        let dir = TestDir::new("open");
        let fake = Arc::new(FakeDesktop::default());
        let manager = ShareManager::new(fake.clone(), dir.0.join("shares"));
        manager
            .on_frame(
                SERVICE_DRAG_DROP,
                &json!({"cmd": 1, "text": "https://example.com/y"}),
                "127.0.0.1".parse().unwrap(),
            )
            .await;
        manager.handle_action(101, "default").await;
        assert_eq!(
            fake.opened_uris.lock().unwrap().as_slice(),
            ["https://example.com/y"]
        );
        assert_eq!(fake.closed.lock().unwrap().as_slice(), [101]);
        assert!(manager.pending.lock().unwrap().is_none());
    }

    #[tokio::test]
    async fn opening_a_folder_keeps_the_staging_for_the_file_manager() {
        let dir = TestDir::new("open-dir");
        let fake = Arc::new(FakeDesktop::default());
        let manager = ShareManager::new(fake.clone(), dir.0.join("shares"));
        let staged = dir.0.join("shares/folder");
        create_private_dir(&staged).unwrap();
        std::fs::write(staged.join("inside.txt"), b"x").unwrap();
        {
            let mut pending = manager.pending.lock().unwrap();
            *pending = Some(Pending {
                token: 1,
                notification: Some(101),
                content: Content::File {
                    dir: staged.clone(),
                    path: staged.clone(),
                    name: "folder".into(),
                    size: 1,
                    is_dir: true,
                    saved: false,
                },
            });
        }
        manager.handle_action(101, "open").await;
        assert_eq!(
            fake.opened_paths.lock().unwrap().as_slice(),
            std::slice::from_ref(&staged)
        );
        // The file manager browses this folder: it must not be deleted underneath it.
        assert!(staged.join("inside.txt").exists());
        assert!(manager.pending.lock().unwrap().is_none());
        assert_eq!(fake.closed.lock().unwrap().as_slice(), [101]);
    }

    #[tokio::test]
    async fn dismiss_removes_the_staged_bytes() {
        let dir = TestDir::new("dismiss");
        let fake = Arc::new(FakeDesktop::default());
        let manager = ShareManager::new(fake.clone(), dir.0.join("shares"));
        let staged_dir = dir.0.join("shares/staged");
        create_private_dir(&staged_dir).unwrap();
        let file = staged_dir.join("a.txt");
        std::fs::write(&file, b"secret").unwrap();
        {
            let mut pending = manager.pending.lock().unwrap();
            *pending = Some(Pending {
                token: 1,
                notification: Some(101),
                content: Content::File {
                    dir: staged_dir.clone(),
                    path: file,
                    name: "a.txt".into(),
                    size: 6,
                    is_dir: false,
                    saved: false,
                },
            });
        }
        manager.handle_action(101, "dismiss").await;
        assert!(!staged_dir.exists());
        assert_eq!(fake.closed.lock().unwrap().as_slice(), [101]);
    }

    #[tokio::test]
    async fn save_copies_the_file_then_removes_staging() {
        let dir = TestDir::new("save");
        let dest = dir.0.join("chosen.txt");
        let fake = FakeDesktop::with_save(dest.clone());
        let manager = ShareManager::new(fake.clone(), dir.0.join("shares"));
        let staged_dir = dir.0.join("shares/staged");
        create_private_dir(&staged_dir).unwrap();
        let file = staged_dir.join("a.txt");
        std::fs::write(&file, b"payload").unwrap();
        {
            let mut pending = manager.pending.lock().unwrap();
            *pending = Some(Pending {
                token: 1,
                notification: Some(101),
                content: Content::File {
                    dir: staged_dir.clone(),
                    path: file,
                    name: "a.txt".into(),
                    size: 7,
                    is_dir: false,
                    saved: false,
                },
            });
        }
        manager.handle_action(101, "save").await;
        assert_eq!(std::fs::read(&dest).unwrap(), b"payload");
        assert!(!staged_dir.exists());
        assert_eq!(fake.choose_saved.lock().unwrap()[0].0, "a.txt");
    }

    #[tokio::test]
    async fn closed_notification_cleans_up() {
        let dir = TestDir::new("closed");
        let fake = Arc::new(FakeDesktop::default());
        let manager = ShareManager::new(fake.clone(), dir.0.join("shares"));
        let staged_dir = dir.0.join("shares/staged");
        create_private_dir(&staged_dir).unwrap();
        let file = staged_dir.join("a.txt");
        std::fs::write(&file, b"x").unwrap();
        {
            let mut pending = manager.pending.lock().unwrap();
            *pending = Some(Pending {
                token: 1,
                notification: Some(101),
                content: Content::File {
                    dir: staged_dir.clone(),
                    path: file,
                    name: "a.txt".into(),
                    size: 1,
                    is_dir: false,
                    saved: false,
                },
            });
        }
        manager.handle_closed(101, 1).await;
        assert!(!staged_dir.exists());
    }

    /// Live check against the running GNOME session (notification + OpenURI/FileChooser portals,
    /// including OpenDirectory). Closes every dialog via Request.Close, so it does not leave a
    /// window open. Run with:
    /// `cargo test -p smartconnect-linux --lib live_desktop -- --ignored --nocapture`
    #[tokio::test]
    #[ignore]
    async fn live_desktop_notification_and_portals() {
        let desktop = SystemDesktop::connect().await.expect("a GNOME session");
        let proxy = zbus::Proxy::new(
            &desktop.conn,
            "org.freedesktop.Notifications",
            "/org/freedesktop/Notifications",
            "org.freedesktop.Notifications",
        )
        .await
        .unwrap();
        let caps: Vec<String> = proxy
            .call_method("GetCapabilities", &())
            .await
            .unwrap()
            .body()
            .deserialize()
            .unwrap();
        let info: (String, String, String, String) = proxy
            .call_method("GetServerInformation", &())
            .await
            .unwrap()
            .body()
            .deserialize()
            .unwrap();
        println!("notification server: {info:?}; capabilities: {caps:?}");
        assert!(
            caps.iter().any(|c| c == "actions"),
            "GNOME must support notification actions"
        );

        let popup = Popup {
            summary: "Smart Connect share test".into(),
            body: "https://example.com/shared-link".into(),
            kind: PopupKind::Link,
            replaces: 0,
        };
        let id = desktop.notify(popup.clone()).await.expect("a notification");
        println!("notification id: {id}");
        tokio::time::sleep(Duration::from_secs(2)).await;
        let replaced = desktop
            .notify(Popup {
                summary: "Smart Connect share test (updated)".into(),
                body: "https://example.com/shared-link-2".into(),
                ..popup
            })
            .await
            .expect("the replacement notification");
        println!("replacement id: {replaced}");
        tokio::time::sleep(Duration::from_secs(2)).await;
        desktop.close(replaced).await;

        // OpenURI: the returned request must be the token path we subscribed to. On GNOME 50.6
        // Wayland the dialog opened and, with it focused, Escape produced Response 1 (verified
        // manually). Request.Close closes it deterministically and emits no Response.
        let (token, _stream, expected) = desktop.request_stream().await.unwrap();
        let portal = PortalOpenUriProxy::new(&desktop.conn).await.unwrap();
        let mut options: HashMap<&str, zvariant::Value<'_>> = HashMap::new();
        options.insert("ask", zvariant::Value::Bool(true));
        options.insert("handle_token", zvariant::Value::from(token));
        let handle = portal
            .open_uri("", "https://example.com/", options)
            .await
            .unwrap();
        println!("OpenURI handle: {}", handle.as_str());
        assert_eq!(handle.as_str(), expected);
        tokio::time::sleep(Duration::from_secs(1)).await;
        close_request(&desktop.conn, &expected).await;
        println!("OpenURI dialog opened and was closed");

        // SaveFile: same request path check and close.
        let (token, _stream, expected) = desktop.request_stream().await.unwrap();
        let chooser = PortalFileChooserProxy::new(&desktop.conn).await.unwrap();
        let mut options: HashMap<&str, zvariant::Value<'_>> = HashMap::new();
        options.insert("handle_token", zvariant::Value::from(token));
        options.insert(
            "current_name",
            zvariant::Value::from("share-test.pdf".to_owned()),
        );
        let handle = chooser
            .save_file("", "Save shared file", options)
            .await
            .unwrap();
        println!("SaveFile handle: {}", handle.as_str());
        assert_eq!(handle.as_str(), expected);
        tokio::time::sleep(Duration::from_secs(1)).await;
        close_request(&desktop.conn, &expected).await;
        println!("SaveFile dialog opened and was closed");

        // SelectFolder: the folder's Save as… dialog. This portal does not export SelectFolder, so
        // the production fallback is SaveFile with the folder name; exercise whichever is present.
        let (token, _stream, expected) = desktop.request_stream().await.unwrap();
        let mut options: HashMap<&str, zvariant::Value<'_>> = HashMap::new();
        options.insert("handle_token", zvariant::Value::from(token));
        options.insert(
            "current_name",
            zvariant::Value::from("share-dir".to_owned()),
        );
        let handle = match chooser
            .select_folder("", "Choose where to save the folder", options.clone())
            .await
        {
            Ok(handle) => {
                println!("SelectFolder is available");
                handle
            }
            Err(zbus::Error::MethodError(error, _, _))
                if error.as_str() == "org.freedesktop.DBus.Error.UnknownMethod" =>
            {
                println!("SelectFolder is unavailable; using the SaveFile fallback");
                chooser
                    .save_file("", "Choose where to save the folder", options)
                    .await
                    .unwrap()
            }
            Err(e) => panic!("SelectFolder: {e}"),
        };
        println!("Save-folder handle: {}", handle.as_str());
        assert_eq!(handle.as_str(), expected);
        tokio::time::sleep(Duration::from_secs(1)).await;
        close_request(&desktop.conn, &expected).await;
        println!("save-folder dialog opened and was closed");

        // OpenDirectory: the folder's Open action. The portal takes an fd, not a path; this also
        // proves the directory fd marshals where OpenFile takes a regular file. OpenDirectory
        // answers at once (it opens the file manager; there is no app picker), so wait for the
        // Response instead of closing the request.
        let dir = std::env::temp_dir();
        let file = tokio::fs::File::open(&dir).await.unwrap();
        let std_file: std::fs::File = file.into_std().await;
        let fd = zvariant::OwnedFd::from(std::os::fd::OwnedFd::from(std_file));
        let (token, stream, expected) = desktop.request_stream().await.unwrap();
        let mut options: HashMap<&str, zvariant::Value<'_>> = HashMap::new();
        options.insert("handle_token", zvariant::Value::from(token));
        let handle = portal.open_directory("", fd, options).await.unwrap();
        println!("OpenDirectory handle: {}", handle.as_str());
        assert_eq!(handle.as_str(), expected);
        let response = tokio::time::timeout(
            Duration::from_secs(15),
            desktop.wait_response(stream, "OpenDirectory"),
        )
        .await
        .expect("the OpenDirectory response")
        .expect("OpenDirectory must succeed");
        assert!(response.is_empty());
        println!("OpenDirectory opened {} in the file manager", dir.display());
    }

    async fn close_request(conn: &zbus::Connection, path: &str) {
        let proxy = zbus::Proxy::new(
            conn,
            "org.freedesktop.portal.Desktop",
            path,
            "org.freedesktop.portal.Request",
        )
        .await
        .expect("the request object the portal returned");
        let result: zbus::Result<()> = proxy.call_method("Close", &()).await.map(|_| ());
        result.expect("Request.Close");
    }

    #[tokio::test]
    async fn an_unanswered_share_expires() {
        let dir = TestDir::new("expiry");
        let fake = Arc::new(FakeDesktop::default());
        let manager = ShareManager::new_with_ttl(
            fake.clone(),
            dir.0.join("shares"),
            Duration::from_millis(50),
        );
        manager
            .present(Content::Text {
                text: "hello".into(),
            })
            .await;
        assert!(manager.pending.lock().unwrap().is_some());
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert!(manager.pending.lock().unwrap().is_none());
        assert_eq!(fake.closed.lock().unwrap().as_slice(), [101]);
    }

    #[tokio::test]
    async fn the_following_share_replaces_the_notification_id() {
        let dir = TestDir::new("replace");
        let fake = Arc::new(FakeDesktop::default());
        let manager = ShareManager::new(fake.clone(), dir.0.join("shares"));
        manager
            .present(Content::Link {
                uri: "https://example.com/1".into(),
            })
            .await;
        manager
            .present(Content::Link {
                uri: "https://example.com/2".into(),
            })
            .await;
        let popups = fake.popups();
        assert_eq!(popups.len(), 2);
        assert_eq!(popups[1].replaces, 101);
    }

    #[tokio::test]
    async fn failed_notification_drops_the_staged_file() {
        let dir = TestDir::new("notifyfail");
        let fake = Arc::new(FakeDesktop::default());
        fake.fail_notify.store(true, Ordering::SeqCst);
        let manager = ShareManager::new(fake.clone(), dir.0.join("shares"));
        let staged_dir = dir.0.join("shares/staged");
        create_private_dir(&staged_dir).unwrap();
        manager
            .present(Content::File {
                dir: staged_dir.clone(),
                path: staged_dir.join("a.txt"),
                name: "a.txt".into(),
                size: 0,
                is_dir: false,
                saved: false,
            })
            .await;
        assert!(!staged_dir.exists());
        assert!(manager.pending.lock().unwrap().is_none());
    }
}
