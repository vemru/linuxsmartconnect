//! Clipboard service (serviceId 9) and the Linux clipboard behind it.
//!
//! Payloads are `ClipboardData{msgId, data}` — the clipboard message type is `data.msgId`, not
//! `cmd`. The phone opens with NEGO once the PC advertises `serviceVersions["9"] >= 2`, and only
//! pushes its copies after the PC's CONFIRM. Text travels inline in UPDATE_NOTIFY in both
//! directions; neither side acks or PULLs text. Spec: `evidence-2026-10-20/clipboard-spec.md`.
//! Never send PULL for text, msgIds 13–17, or `shareHubMountInfo` — the phone mishandles them.
//! Image/file clips arrive as an UPDATE_NOTIFY with a `filelist` (no `text`); their bytes move over
//! "fex" (service 2), which this daemon does not speak yet, so we recognise and log them but never
//! PULL. See `docs/protocol/file-exchange.md` and `docs/protocol/clipboard.md`.

use std::process::Stdio;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::sync::broadcast;
use tracing::{debug, info, warn};

pub const SERVICE_CLIPBOARD: i64 = 9;

const CLIP_NEGO: i64 = 1;
const CLIP_CONFIRM: i64 = 2;
const CLIP_UPDATE_NOTIFY: i64 = 3;
const CLIP_CLOSE: i64 = 7;
/// Version the CONFIRM agrees to; the phone only checks that it is non-zero.
const CLIP_VERSION: i64 = 1;
/// The phone truncates longer text, so don't send more.
const MAX_TEXT_CHARS: usize = 100_000;

/// A copy made on Linux, numbered once so every session sends the same `clipId`.
#[derive(Clone, Debug)]
pub struct LocalClip {
    pub id: i64,
    pub text: String,
}

/// Per-connection state: after a CLOSE the phone ignores us until it negotiates again.
#[derive(Default)]
pub struct Session {
    negotiated: bool,
}

impl Session {
    /// Handles one clipboard message from the phone and returns the reply, if it needs one.
    pub fn handle(&mut self, data: &Value, local: &LocalClipboard) -> Option<Value> {
        match data.get("msgId").and_then(Value::as_i64)? {
            CLIP_NEGO => {
                let version = data.pointer("/data/version").and_then(Value::as_i64).unwrap_or(CLIP_VERSION);
                let version = version.min(CLIP_VERSION);
                self.negotiated = version > 0;
                Some(json!({ "msgId": CLIP_CONFIRM, "data": { "version": version } }))
            }
            CLIP_UPDATE_NOTIFY => {
                match data.pointer("/data/text").and_then(Value::as_str) {
                    Some(text) => local.set_from_phone(text),
                    // An image or file clip. The bytes travel over "fex" (service 2), which this
                    // daemon does not speak yet, so we recognise the clip and log it but do NOT send
                    // a PULL (msgId 4). That is safe: the phone simply gets no pull. Pulling a clip
                    // we cannot receive, or pulling a *text* clip (which has no file list), is what
                    // crashes the phone. See docs/protocol/file-exchange.md.
                    None => describe_file_clip(&data.pointer("/data").cloned().unwrap_or(Value::Null)),
                }
                None
            }
            CLIP_CLOSE => {
                self.negotiated = false;
                None
            }
            _ => None,
        }
    }

    /// The UPDATE_NOTIFY that carries a Linux copy to the phone, once the phone is listening.
    pub fn outgoing(&self, clip: &LocalClip) -> Option<Value> {
        self.negotiated.then(|| {
            let text: String = clip.text.chars().take(MAX_TEXT_CHARS).collect();
            json!({ "msgId": CLIP_UPDATE_NOTIFY, "data": { "clipId": clip.id, "text": text } })
        })
    }
}

/// The Linux clipboard, through wl-clipboard (Wayland, compositor with data-control).
pub struct LocalClipboard {
    /// Last text that crossed the link in either direction; a watcher event with the same text
    /// is our own `wl-copy` (or a clipboard manager re-owning it) and must not go back out.
    last_synced: Mutex<Option<String>>,
    changes: broadcast::Sender<LocalClip>,
    next_id: AtomicI64,
}

impl LocalClipboard {
    pub fn new() -> Arc<Self> {
        // The phone keeps the clipId as a Java int; seed from the clock so ids keep rising across
        // daemon restarts without overflowing 31 bits.
        let seed = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64 & 0x3fff_ffff).unwrap_or(1);
        Arc::new(Self { last_synced: Mutex::new(None), changes: broadcast::channel(16).0, next_id: AtomicI64::new(seed) })
    }

    pub fn subscribe(&self) -> broadcast::Receiver<LocalClip> {
        self.changes.subscribe()
    }

    fn set_from_phone(&self, text: &str) {
        *self.last_synced.lock().unwrap() = Some(text.to_owned());
        let text = text.to_owned();
        tokio::spawn(async move {
            match wl_copy(&text).await {
                Ok(()) => info!(chars = text.chars().count(), "clipboard: phone → Linux"),
                Err(e) => warn!("clipboard: wl-copy failed: {e:#}"),
            }
        });
    }

    /// Records a Linux copy and hands it to the sessions, unless it is the text we just synced.
    fn local_changed(&self, text: String) {
        {
            let mut last = self.last_synced.lock().unwrap();
            if last.as_deref() == Some(text.as_str()) {
                return;
            }
            *last = Some(text.clone());
        }
        let clip = LocalClip { id: self.next_id.fetch_add(1, Ordering::Relaxed), text };
        info!(clip_id = clip.id, chars = clip.text.chars().count(), "clipboard: Linux → phone");
        let _ = self.changes.send(clip); // no session connected = nothing to do
    }

    /// Follows the Wayland clipboard with `wl-paste --watch`, which runs the helper on every
    /// selection change with the content on stdin. The helper frames each event as
    /// `<CLIPBOARD_STATE>\n<text>\0` on our pipe. Password-manager entries arrive with state
    /// `sensitive` and are never forwarded.
    pub async fn watch(self: Arc<Self>) -> Result<()> {
        let mut child = Command::new("wl-paste")
            .args(["--type", "text", "--watch", "sh", "-c", r#"printf '%s\n' "$CLIPBOARD_STATE"; cat; printf '\0'"#])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .context("starting wl-paste --watch (is wl-clipboard installed?)")?;
        let mut events = BufReader::new(child.stdout.take().context("wl-paste stdout")?);
        let mut buf = Vec::new();
        loop {
            buf.clear();
            if events.read_until(b'\0', &mut buf).await? == 0 {
                anyhow::bail!("wl-paste --watch exited: {:?}", child.wait().await?);
            }
            if buf.last() == Some(&b'\0') {
                buf.pop();
            }
            let event = String::from_utf8_lossy(&buf);
            let (state, text) = event.split_once('\n').unwrap_or((event.as_ref(), ""));
            match state {
                "sensitive" => debug!("clipboard: skipping a sensitive (password manager) copy"),
                "nil" | "clear" => {}
                _ if text.is_empty() => {}
                _ => self.local_changed(text.to_owned()),
            }
        }
    }
}

/// Logs what an image/file `UPDATE_NOTIFY` from the phone contains, without fetching anything.
/// `data` is the inner `{clipId, filelist, storage}` object. We cannot receive the bytes yet
/// (that needs a fex client; see docs/protocol/file-exchange.md), so this is informational only.
fn describe_file_clip(data: &Value) {
    // Casing is seen as both `filelist` and `fileList` on the wire.
    let list = data
        .get("filelist")
        .or_else(|| data.get("fileList"))
        .and_then(Value::as_array);
    let Some(list) = list else {
        info!("phone sent a non-text clip with no file list; ignoring");
        return;
    };
    let count = list.len();
    let total: i64 = list
        .iter()
        .filter_map(|f| f.get("size").and_then(Value::as_i64))
        .sum();
    // attr: 16 = directory, 128 = normal file.
    let dirs = list
        .iter()
        .filter(|f| f.get("attr").and_then(Value::as_i64) == Some(16))
        .count();
    let kind = if count == 1 && dirs == 0 {
        match list[0].get("name").and_then(Value::as_str) {
            Some(name) if is_image_name(name) => "an image",
            _ => "a file",
        }
    } else if dirs == count {
        "folders"
    } else {
        "files"
    };
    info!(
        files = count,
        bytes = total,
        "phone copied {kind} to its clipboard; file/image clipboard is not supported yet \
         (needs the fex transport, see docs/protocol/file-exchange.md)"
    );
}

/// Whether a file name looks like an image, by extension.
fn is_image_name(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    [
        ".jpg", ".jpeg", ".png", ".gif", ".webp", ".bmp", ".heic", ".heif", ".tif", ".tiff",
    ]
    .iter()
    .any(|ext| lower.ends_with(ext))
}

async fn wl_copy(text: &str) -> Result<()> {
    let mut child = Command::new("wl-copy")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("starting wl-copy")?;
    let mut stdin = child.stdin.take().context("wl-copy stdin")?;
    stdin.write_all(text.as_bytes()).await?;
    drop(stdin);
    // wl-copy forks a child that serves the selection; this waits only for the parent.
    let status = child.wait().await?;
    anyhow::ensure!(status.success(), "wl-copy exited with {status}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nego_is_confirmed_and_enables_sending() {
        let local = LocalClipboard::new();
        let mut s = Session::default();
        let clip = LocalClip { id: 5, text: "hi".into() };
        assert!(s.outgoing(&clip).is_none(), "nothing goes out before the phone negotiates");

        let confirm = s.handle(&json!({ "msgId": 1, "data": { "version": 1 } }), &local).unwrap();
        assert_eq!(confirm, json!({ "msgId": 2, "data": { "version": 1 } }));
        assert_eq!(s.outgoing(&clip).unwrap(), json!({ "msgId": 3, "data": { "clipId": 5, "text": "hi" } }));

        assert!(s.handle(&json!({ "msgId": 7, "data": { "code": 0, "reason": "" } }), &local).is_none());
        assert!(s.outgoing(&clip).is_none(), "CLOSE stops sending until the next NEGO");
    }

    #[test]
    fn own_copies_are_not_echoed() {
        let local = LocalClipboard::new();
        let mut rx = local.subscribe();
        *local.last_synced.lock().unwrap() = Some("from phone".into());
        local.local_changed("from phone".into());
        assert!(rx.try_recv().is_err());
        local.local_changed("typed on linux".into());
        assert_eq!(rx.try_recv().unwrap().text, "typed on linux");
    }

    #[test]
    fn image_clip_is_recognised_but_never_pulled() {
        // An UPDATE_NOTIFY with a file list and no text must not produce any reply (no PULL),
        // because the fex transport that would receive the bytes is not implemented.
        let local = LocalClipboard::new();
        let mut s = Session::default();
        s.negotiated = true;
        let img = json!({
            "msgId": 3,
            "data": {
                "clipId": 9,
                "filelist": [ { "attr": 128, "name": "a.jpg", "path": "/storage/emulated/0/a.jpg", "size": 1234 } ],
                "storage": "/storage/emulated/0"
            }
        });
        assert!(s.handle(&img, &local).is_none(), "file clips get no reply (never PULL)");
        // The phone's text is untouched by a file clip.
        assert!(local.last_synced.lock().unwrap().is_none());
    }

    #[test]
    fn image_names_are_detected() {
        assert!(is_image_name("photo.JPG"));
        assert!(is_image_name("x.heic"));
        assert!(!is_image_name("notes.txt"));
        assert!(!is_image_name("archive.zip"));
    }

    #[test]
    fn long_text_is_truncated_to_the_phone_limit() {
        let mut s = Session::default();
        s.negotiated = true;
        let clip = LocalClip { id: 1, text: "é".repeat(MAX_TEXT_CHARS + 10) };
        let out = s.outgoing(&clip).unwrap();
        assert_eq!(out["data"]["text"].as_str().unwrap().chars().count(), MAX_TEXT_CHARS);
    }
}
