//! smartconnect-mirror: shows the phone screen and phone apps in Linux windows.
//! Specs: evidence-2026-10-20/mirroring-spec.md and appstream-and-display-spec.md (section A).
//!
//! smartconnectd relays the signalling over its control socket (`watch` svc 1/15/16/17, `send`);
//! every stream (the whole-screen mirror, Ready For desktop, or one phone app) is its own WebRTC
//! session with its own GStreamer pipeline (`webrtcbin` answerer for the phone's H.264 track) and
//! window.
//!
//!   mirror:     PC svc1 cmd1 (view=1) → phone svc15 cmd2 {pkg:"mirror"} → PC svc15 cmd1
//!   view (phone tile): phone svc1 cmd0 {view} → PC svc1 cmd1 {view, reply:1} → phone svc15 cmd2 {pkg:"mirror"|"desktop"|…}
//!   app (PC):   PC svc15 cmd1 {pkg,cmp,userId,source:0}
//!   app (phone, e.g. notification "Open"): phone svc15 cmd2 {pkg,pkgName,icon,source:1} → PC cmd1 (echo, source:1)
//!   then per session: phone cmd3 (offer, sessionId) → PC cmd4 (answer); cmd20 ICE both ways;
//!   phone cmd8 state:1 {displayId} (svc 16 input target); PC cmd9 heartbeat /30 s; PC cmd7 close.
//!
//! Notification action buttons (daemon local event, see actions.rs) are answered without a window
//! and closed again once the phone has fired the intent.
//!
//! Modes:
//!   smartconnect-mirror                  whole-screen mirror (exits when it ends)
//!   smartconnect-mirror --desktop        Ready For desktop on a phone virtual display (exits when it ends)
//!   smartconnect-mirror --host           long-running stream host (phone-initiated streams + control socket)
//!   smartconnect-mirror --app PKG [CMP]  stream one app (via the host if one runs, else standalone)
//!   smartconnect-mirror --list-apps      print the phone's app list (svc 17)
//!   smartconnect-mirror --sessions       list the host's open sessions

mod actions;
mod ice;
mod icons;
mod keys;
mod resize;
mod views;

use std::cell::RefCell;
use std::collections::{HashMap, HashSet, VecDeque};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
use gst::glib;
use gst::prelude::*;
use serde_json::{Value, json};

use keys::*;

/// How often the host checks app windows for a new size (when resizing is enabled).
const RESIZE_POLL: Duration = Duration::from_millis(250);

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
fn socket_path() -> PathBuf {
    runtime_dir().join("smartconnectd.sock")
}
fn host_socket_path() -> PathBuf {
    runtime_dir().join("smartconnect-mirror.sock")
}
fn apps_cache_path() -> PathBuf {
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".cache")
        });
    base.join("smartconnect/apps.json")
}

/// The phone calls `releaseAllSessions()` whenever a cmd 1 carries a different `instanceId` than the
/// previous one, so every process (mirror, host, one-shot --app) shares one id per login session.
fn instance_id() -> i64 {
    let path = runtime_dir().join("smartconnect-mirror.instance");
    if let Some(id) = std::fs::read_to_string(&path)
        .ok()
        .and_then(|s| s.trim().parse().ok())
    {
        return id;
    }
    let id = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(1);
    let _ = std::fs::write(&path, id.to_string());
    id
}

/// The daemon's `status` (plain-line command) on a fresh connection; `pcAddress` picks our ICE address.
fn daemon_status() -> Option<Value> {
    let mut s = UnixStream::connect(socket_path()).ok()?;
    s.set_read_timeout(Some(Duration::from_secs(2))).ok()?;
    s.write_all(b"status\n").ok()?;
    let mut line = String::new();
    BufReader::new(s).read_line(&mut line).ok()?;
    serde_json::from_str(&line).ok()
}

/// The daemon's command connection (send-only; replies are drained by a thread).
struct Daemon {
    cmd: Mutex<UnixStream>,
}

impl Daemon {
    fn connect() -> Result<Arc<Self>> {
        let cmd = UnixStream::connect(socket_path()).context("is smartconnectd running?")?;
        let drain = cmd.try_clone()?;
        std::thread::spawn(move || {
            for line in BufReader::new(drain).lines().map_while(Result::ok) {
                if line.contains("\"ok\":false") {
                    eprintln!("daemon: {line}");
                }
            }
        });
        Ok(Arc::new(Daemon {
            cmd: Mutex::new(cmd),
        }))
    }

    fn send(&self, service: i64, data: Value) {
        let line = json!({ "cmd": "send", "service": service, "data": data }).to_string() + "\n";
        if let Err(e) = self.cmd.lock().unwrap().write_all(line.as_bytes()) {
            eprintln!("daemon write failed: {e}");
        }
    }

    fn send15(&self, cmd: i64, data: Option<Value>) {
        let mut msg = json!({ "ver": "1", "cmd": cmd });
        if let Some(d) = data {
            msg["data"] = d;
        }
        self.send(15, msg);
    }

    fn input(&self, cmd: i64, data: Value) {
        self.send(
            16,
            json!({ "cmd": cmd, "data": data, "messageId": 0, "instanceId": 0 }),
        );
    }
}

/// Streams phone frames (and daemon-local events) for `services` into `tx` as `Ev::Phone`.
fn watch(services: &[i64], tx: mpsc::Sender<Ev>) -> Result<()> {
    let watch = UnixStream::connect(socket_path()).context("is smartconnectd running?")?;
    (&watch)
        .write_all(format!("{}\n", json!({ "cmd": "watch", "services": services })).as_bytes())?;
    std::thread::spawn(move || {
        for line in BufReader::new(watch).lines().map_while(Result::ok) {
            if let Ok(v) = serde_json::from_str::<Value>(&line)
                && let Some(svc) = v.get("service").and_then(Value::as_i64)
            {
                let _ = tx.send(Ev::Phone(svc, v["data"].clone()));
            }
        }
        eprintln!("daemon closed the watch stream");
        let _ = tx.send(Ev::DaemonGone);
    });
    Ok(())
}

enum Ev {
    Phone(i64, Value),
    DaemonGone,
    /// A command on the host's control socket, with where to send the one-line reply.
    Ctl(Value, mpsc::Sender<String>),
}

// ---------------------------------------------------------------------------------------------
// One stream session = one webrtcbin pipeline = one window (or none, for a background action).

#[derive(Default)]
struct SessState {
    mid: String,
    answered: bool,
    pending_local: Vec<(u32, String)>,
    /// The phone's virtual display for this session (svc 16 input target).
    display_id: Option<i64>,
    cap_requested: HashSet<i64>,
    /// Modifier keys currently held on the phone, as (scancode, flags).
    held_mods: HashSet<(i64, i64)>,
    scroll_acc: f64,
    /// Since when ICE has been failed/disconnected/closed (set from webrtcbin's thread).
    ice_bad_since: Option<Instant>,
    closing: bool,
    /// Headless (background action) sessions: when to close.
    headless: Option<actions::HeadlessTimer>,
    resize: resize::ResizeDebouncer,
    /// Last mouse motion timestamp for throttling to ~125 Hz (8ms).
    last_mouse_move: Option<Instant>,
    /// Most recent un-dispatched mouse coordinate to flush on click or at rest.
    pending_mouse: Option<(i64, i64)>,
    /// Base stream size reported by the phone (unrotated, from cmd 3).
    base_stream_size: Option<(u32, u32)>,
    /// Current rotation reported by the phone in cmd 16 / 23 (0..3).
    current_rotation: u32,
    /// Last rotation value for which window orientation was adapted.
    last_adapted_rotation: Option<u32>,
}

impl SessState {
    /// Checks whether rotation adaptation is needed and updates last_adapted_rotation.
    /// Returns true if orientation adaptation should proceed (prevents oscillation loops).
    fn check_and_mark_adapted_rotation(&mut self, rot: u32) -> bool {
        if self.last_adapted_rotation == Some(rot) {
            return false;
        }
        self.last_adapted_rotation = Some(rot);
        true
    }
}

struct Session {
    sid: i64,
    pkg: String,
    user_id: Value,
    cmp: Value,
    shortcut_id: Value,
    /// A notification action running without a window (video goes to a fakesink).
    headless: bool,
    d: Arc<Daemon>,
    st: Mutex<SessState>,
    pipeline: gst::Pipeline,
    webrtc: gst::Element,
    title: Mutex<String>,
    tag: Mutex<Option<gst::Element>>,
    sink: Mutex<Option<gst::Element>>,
    flip: Mutex<Option<gst::Element>>,
    /// The video window's X11 id once known, and the title last written to it.
    xwin: Mutex<Option<(u32, String)>>,
    /// Flag set when the window close button is pressed or window close is requested.
    closed_by_user: Arc<AtomicBool>,
    /// Whether the GL close hook has been installed on this session's window.
    close_hook_installed: AtomicBool,
}

/// What a new session is: the phone's ids for it plus how we show it.
struct SessionInfo {
    sid: i64,
    pkg: String,
    user_id: Value,
    cmp: Value,
    shortcut_id: Value,
    title: String,
    headless: bool,
}

struct CloseHookData {
    closed: Arc<AtomicBool>,
    orig_cb: gst_gl::ffi::GstGLWindowCB,
    orig_data: glib::ffi::gpointer,
}

unsafe extern "C" fn on_gl_window_close(data: glib::ffi::gpointer) {
    if data.is_null() {
        return;
    }
    let hook_data = unsafe { &*(data as *mut CloseHookData) };
    hook_data.closed.store(true, Ordering::SeqCst);
    if let Some(orig) = hook_data.orig_cb {
        unsafe { orig(hook_data.orig_data) };
    }
}

unsafe extern "C" fn destroy_close_hook(data: glib::ffi::gpointer) {
    if !data.is_null() {
        unsafe {
            drop(Box::from_raw(data as *mut CloseHookData));
        }
    }
}

impl Session {
    fn new(info: SessionInfo, filter: ice::CandidateFilter, d: Arc<Daemon>) -> Result<Arc<Self>> {
        let SessionInfo {
            sid,
            pkg,
            user_id,
            cmp,
            shortcut_id,
            title,
            headless,
        } = info;
        let pipeline = gst::Pipeline::with_name(&format!("session-{sid}"));
        let latency: u32 = std::env::var("MIRROR_LATENCY")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(30);
        let webrtc = gst::ElementFactory::make("webrtcbin")
            .property_from_str("bundle-policy", "max-bundle")
            .property("latency", latency)
            .build()?;
        pipeline.add(&webrtc)?;
        let s = Arc::new(Session {
            sid,
            pkg,
            user_id,
            cmp,
            shortcut_id,
            headless,
            d,
            st: Mutex::default(),
            pipeline: pipeline.clone(),
            webrtc: webrtc.clone(),
            title: Mutex::new(title),
            tag: Mutex::new(None),
            sink: Mutex::new(None),
            flip: Mutex::new(None),
            xwin: Mutex::new(None),
            closed_by_user: Arc::new(AtomicBool::new(false)),
            close_hook_installed: AtomicBool::new(false),
        });
        {
            let w = Arc::downgrade(&s);
            webrtc.connect("on-ice-candidate", false, move |args| {
                let (mline, cand) = (args[1].get::<u32>().ok()?, args[2].get::<String>().ok()?);
                if !filter.allows(&cand) {
                    return None; // docker/VPN/other networks: the phone would try them all
                }
                let s = w.upgrade()?;
                let answered = s.st.lock().unwrap().answered;
                if answered {
                    s.send_candidate(mline, &cand);
                } else {
                    s.st.lock().unwrap().pending_local.push((mline, cand));
                }
                None
            });
        }
        {
            let w = Arc::downgrade(&s);
            webrtc.connect_notify(Some("ice-connection-state"), move |wb, _| {
                use gst_webrtc::WebRTCICEConnectionState as S;
                let state: S = wb.property("ice-connection-state");
                if let Some(s) = w.upgrade() {
                    println!("[{}] ICE: {state:?}", s.sid);
                    let bad = matches!(state, S::Failed | S::Disconnected | S::Closed);
                    let mut st = s.st.lock().unwrap();
                    st.ice_bad_since = if bad {
                        st.ice_bad_since.or_else(|| Some(Instant::now()))
                    } else {
                        None
                    };
                }
            });
        }
        {
            let w = Arc::downgrade(&s);
            webrtc.connect_pad_added(move |_, pad| {
                if pad.direction() != gst::PadDirection::Src {
                    return;
                }
                let Some(s) = w.upgrade() else { return };
                // Never panic here: this runs on a GStreamer streaming thread (a panic aborts the process).
                if let Err(e) = s.attach_decoder(pad) {
                    eprintln!("[{}] could not attach the decoder: {e:#}", s.sid);
                }
            });
        }
        pipeline.set_state(gst::State::Playing)?;
        Ok(s)
    }

    fn attach_decoder(self: &Arc<Self>, pad: &gst::Pad) -> Result<()> {
        if self.headless {
            // No window: keep the stream flowing (an unlinked pad would error the pipeline).
            let bin =
                gst::parse::bin_from_description("queue ! fakesink sync=false async=false", true)?;
            self.pipeline.add(&bin)?;
            pad.link(
                &bin.static_pad("sink")
                    .context("fakesink bin has no sink pad")?,
            )?;
            bin.sync_state_with_parent()?;
            return Ok(());
        }
        println!(
            "[{}] video arriving: {:?}",
            self.sid,
            pad.current_caps().map(|c| c.to_string())
        );
        let desc = format!(
            "queue ! rtph264depay ! h264parse ! {} ! videoconvert name=conv n-threads=2 ! videoflip name=flip ! videoconvert n-threads=2 ! queue max-size-buffers=1 max-size-time=0 max-size-bytes=0 leaky=downstream ! taginject name=tag ! {} name=sink sync=false",
            pick_decoder(),
            pick_sink()
        );
        let bin = match gst::parse::bin_from_description(&desc, true) {
            Ok(b) => b,
            Err(e) => {
                eprintln!(
                    "[{}] could not construct decoder bin with {}: {e}; falling back to avdec_h264",
                    self.sid,
                    pick_decoder()
                );
                let fallback = format!(
                    "queue ! rtph264depay ! h264parse ! avdec_h264 thread-type=slice max-threads=4 ! videoconvert name=conv n-threads=2 ! videoflip name=flip ! videoconvert n-threads=2 ! queue max-size-buffers=1 max-size-time=0 max-size-bytes=0 leaky=downstream ! taginject name=tag ! {} name=sink sync=false",
                    pick_sink()
                );
                gst::parse::bin_from_description(&fallback, true)?
            }
        };
        let tag = bin.by_name("tag").context("no taginject")?;
        tag.set_property("tags", title_tags(&self.title.lock().unwrap()));
        *self.tag.lock().unwrap() = Some(tag.clone());
        *self.sink.lock().unwrap() = bin.by_name("sink");
        if let Some(flip) = bin.by_name("flip") {
            let rot = self.st.lock().unwrap().current_rotation;
            let dir = rotation_to_direction(rot);
            flip.set_property_from_str("video-direction", dir);
            *self.flip.lock().unwrap() = Some(flip);
        }
        self.pipeline.add(&bin)?;
        // Link before starting the bin, then bring it up to the pipeline's state.
        pad.link(
            &bin.static_pad("sink")
                .context("decoder bin has no sink pad")?,
        )?;
        bin.sync_state_with_parent()?;
        // The video sink sends pointer/keyboard navigation events upstream; turn them into input.
        // Listen on tag's sink pad (downstream of videoflip) so coordinates match the rendered
        // window display in both portrait and landscape without inverted transforms by videoflip.
        let w = Arc::downgrade(self);
        if let Some(conv) = bin.by_name("conv") {
            let sid = self.sid;
            if let Some(sp) = conv.static_pad("sink") {
                sp.add_probe(gst::PadProbeType::EVENT_DOWNSTREAM, move |_pad, info| {
                    if let Some(gst::PadProbeData::Event(ev)) = &info.data
                        && let gst::EventView::Caps(c) = ev.view()
                    {
                        println!("[{sid}] DECODER OUTPUT CAPS: {}", c.caps());
                    }
                    gst::PadProbeReturn::Ok
                });
            }
        }
        if let Some(tag_el) = bin.by_name("tag") {
            let sid = self.sid;
            if let Some(sp) = tag_el.static_pad("src") {
                sp.add_probe(gst::PadProbeType::EVENT_DOWNSTREAM, move |pad, info| {
                    if let Some(gst::PadProbeData::Event(ev)) = &info.data
                        && ev.type_() == gst::EventType::Caps
                    {
                        println!(
                            "[{sid}] SINK INPUT CAPS (after flip): {:?}",
                            pad.current_caps().map(|c| c.to_string())
                        );
                    }
                    gst::PadProbeReturn::Ok
                });
            }
        }
        tag.static_pad("sink")
            .context("no tag sink pad")?
            .add_probe(gst::PadProbeType::EVENT_UPSTREAM, move |_, info| {
                if let (Some(s), Some(gst::PadProbeData::Event(ev))) = (w.upgrade(), &info.data)
                    && ev.type_() == gst::EventType::Navigation
                {
                    s.on_navigation(ev);
                }
                gst::PadProbeReturn::Ok
            });
        Ok(())
    }

    fn set_title(&self, title: &str) {
        let changed = {
            let mut t = self.title.lock().unwrap();
            std::mem::replace(&mut *t, title.to_owned()) != title
        };
        if changed {
            println!("[{}] title → {title}", self.sid);
            // taginject re-sends its tags downstream on the next buffer; glimagesink retitles the window.
            if let Some(tag) = self.tag.lock().unwrap().as_ref() {
                tag.set_property("tags", title_tags(title));
            }
        }
    }

    fn send_candidate(&self, mline: u32, candidate: &str) {
        let mid = self.st.lock().unwrap().mid.clone();
        let data =
            json!({ "type": "candidate", "label": mline, "id": mid, "candidate": candidate })
                .to_string();
        self.d.send15(
            20,
            Some(json!({
                "sessionId": self.sid, "type": "candidate", "userId": self.user_id, "pkg": self.pkg, "data": data,
            })),
        );
    }

    fn add_remote_candidate(&self, body: &Value) -> Result<()> {
        let c: Value =
            serde_json::from_str(body.get("data").and_then(Value::as_str).unwrap_or("{}"))?;
        if let Some(cand) = c.get("candidate").and_then(Value::as_str) {
            let label = c.get("label").and_then(Value::as_u64).unwrap_or(0) as u32;
            self.webrtc
                .emit_by_name::<()>("add-ice-candidate", &[&label, &cand]);
        }
        Ok(())
    }

    /// Answers the offer (cmd 4). On the phone, this answer is what creates the app's virtual
    /// display and launches the app on it.
    fn answer(&self, offer_sdp: &str) -> Result<()> {
        let offer_sdp = h264_only(offer_sdp);
        if let Some(mid) = offer_sdp.lines().find_map(|l| l.strip_prefix("a=mid:")) {
            self.st.lock().unwrap().mid = mid.trim().to_owned();
        }
        let sdp = gst_sdp::SDPMessage::parse_buffer(offer_sdp.as_bytes())
            .map_err(|_| anyhow!("bad offer SDP"))?;
        let offer =
            gst_webrtc::WebRTCSessionDescription::new(gst_webrtc::WebRTCSDPType::Offer, sdp);
        let p = gst::Promise::new();
        self.webrtc
            .emit_by_name::<()>("set-remote-description", &[&offer, &p]);
        p.wait();
        let p = gst::Promise::new();
        self.webrtc
            .emit_by_name::<()>("create-answer", &[&None::<gst::Structure>, &p]);
        p.wait();
        let reply = p.get_reply().context("no answer")?;
        let answer = reply
            .value("answer")?
            .get::<gst_webrtc::WebRTCSessionDescription>()?;
        let text = answer.sdp().as_text()?;
        self.webrtc
            .emit_by_name::<()>("set-local-description", &[&answer, &None::<gst::Promise>]);
        println!(
            "[{}] answer: {} bytes, H.264={}",
            self.sid,
            text.len(),
            text.contains("H264")
        );
        self.d.send15(
            4,
            Some(json!({
                "sessionId": self.sid, "error": 0, "isWebRTC": 1, "resize": 0, "userId": self.user_id,
                "webRTCData": json!({ "type": "answer", "sdp": text }).to_string(),
            })),
        );
        let pending = {
            let mut st = self.st.lock().unwrap();
            st.answered = true;
            if self.headless {
                st.headless = Some(actions::HeadlessTimer::new(Instant::now()));
            }
            std::mem::take(&mut st.pending_local)
        };
        for (m, c) in pending {
            self.send_candidate(m, &c);
        }
        Ok(())
    }

    /// Records the session's display id and asks the phone, once per display, for a virtual
    /// keyboard + mouse on it (svc 16 REQUEST_CAP, caps bit0 keyboard | bit1 mouse).
    fn learn_display(&self, body: &Value) {
        let Some(d) = body
            .get("displayId")
            .and_then(Value::as_i64)
            .filter(|d| *d >= 0)
        else {
            return;
        };
        let first = {
            let mut st = self.st.lock().unwrap();
            st.display_id = Some(d);
            st.cap_requested.insert(d)
        };
        if first && !self.headless {
            println!(
                "[{}] input: requesting keyboard+mouse on display {d}",
                self.sid
            );
            self.d
                .input(1, json!({ "ver": 1, "displayId": d, "caps": 3 }));
        }
    }

    /// cmd 3 `width`/`height`: the size the phone currently streams (resize baseline).
    fn learn_stream_size(&self, body: &Value) {
        let dim = |k: &str| {
            body.get(k)
                .and_then(Value::as_u64)
                .and_then(|v| u32::try_from(v).ok())
        };
        if let (Some(w), Some(h)) = (dim("width"), dim("height")) {
            let mut st = self.st.lock().unwrap();
            st.resize.set_stream_size(w, h);
            if st.base_stream_size.is_none() {
                st.base_stream_size = Some((w, h));
            }
        }
    }

    fn display_id(&self) -> Option<i64> {
        self.st.lock().unwrap().display_id
    }

    /// The phone reported the app focused/merged/raised on this session's display: for a background
    /// action that is when it fires the intent.
    fn mark_fired(&self) {
        if let Some(t) = self.st.lock().unwrap().headless.as_mut() {
            println!("[{}] the phone is running the action now", self.sid);
            t.fired(Instant::now());
        }
    }

    fn headless_due(&self, now: Instant) -> Option<&'static str> {
        self.st.lock().unwrap().headless.and_then(|t| t.due(now))
    }

    /// The window's size if it changed and settled (see resize.rs), as a cmd 17 payload.
    fn poll_resize(&self, x: &X11, instance_id: i64, now: Instant) -> Option<Value> {
        if self.headless || self.pkg == "mirror" {
            return None; // the phone never resizes the mirror display
        }
        let display_id = self.display_id()?;
        let (win, _) = self.xwin.lock().unwrap().clone()?;
        let size = x.window_size(win)?;
        let wanted = self.st.lock().unwrap().resize.observe(size, now)?;
        println!(
            "[{}] window is {}x{}: asking the phone to resize display {display_id}",
            self.sid, wanted.0, wanted.1
        );
        let t = resize::ResizeTarget {
            display_id,
            pkg: &self.pkg,
            user_id: &self.user_id,
            cmp: &self.cmp,
            shortcut_id: &self.shortcut_id,
            instance_id,
        };
        Some(resize::payload(&t, wanted))
    }

    fn flush_pending_mouse(&self) {
        let Some(d) = self.display_id() else { return };
        if let Some((x, y)) = self.st.lock().unwrap().pending_mouse.take() {
            self.d.input(4, json!({ "displayId": d, "x": x, "y": y }));
        }
    }

    /// Window pointer/keyboard events (GstNavigation from the video sink) → phone input (svc 16).
    /// Coordinates are already in video-frame pixels, which is what the phone expects.
    fn on_navigation(&self, ev: &gst::EventRef) {
        use gst_video::NavigationEvent as N;
        let Some(d) = self.display_id() else { return };
        let Ok(nav) = N::parse(ev) else { return };
        let pressed = matches!(nav, N::MouseButtonPress { .. } | N::KeyPress { .. });
        let input = |cmd, data| self.d.input(cmd, data);
        match nav {
            N::MouseMove { x, y, .. } => {
                let (send, coord) = {
                    let mut st = self.st.lock().unwrap();
                    let now = Instant::now();
                    let cx = (x as i64).max(0);
                    let cy = (y as i64).max(0);
                    st.pending_mouse = Some((cx, cy));
                    if st
                        .last_mouse_move
                        .is_none_or(|t| now.duration_since(t) >= Duration::from_millis(8))
                    {
                        st.last_mouse_move = Some(now);
                        (true, st.pending_mouse.take())
                    } else {
                        (false, None)
                    }
                };
                if let (true, Some((x, y))) = (send, coord) {
                    input(4, json!({ "displayId": d, "x": x, "y": y }));
                }
            }
            N::MouseButtonPress {
                button: b, x, y, ..
            }
            | N::MouseButtonRelease {
                button: b, x, y, ..
            } => {
                self.flush_pending_mouse();
                let cx = (x as i64).max(0);
                let cy = (y as i64).max(0);
                // X11/GTK number buttons 1..9; GL-on-Wayland passes evdev codes (BTN_LEFT = 272…).
                let b = match b {
                    272 => 1,
                    273 => 3,
                    274 => 2,
                    275 => 8,
                    276 => 9,
                    b => b,
                };
                match b {
                    1..=3 => input(
                        5,
                        json!({ "displayId": d, "key": b, "x": cx, "y": cy, "down": pressed }),
                    ),
                    // Mouse "back" side button → Android BACK.
                    8 if pressed => self.android_key(d, KEYCODE_BACK),
                    // X11 delivers wheel clicks as buttons 4 (up) / 5 (down).
                    4 | 5 if pressed => input(
                        5,
                        json!({ "displayId": d, "key": 4, "x": x as i64, "y": y as i64, "down": b == 5 }),
                    ),
                    _ => {}
                }
            }
            N::MouseScroll { x, y, delta_y, .. } if delta_y != 0.0 => {
                // X11 sends ±1 per wheel click; Wayland smooth scroll sends ~10 units per notch.
                let unit = if delta_y.abs() <= 1.5 { 1.0 } else { 10.0 };
                let notches = {
                    let mut st = self.st.lock().unwrap();
                    st.scroll_acc += delta_y / unit;
                    let n = st.scroll_acc.trunc();
                    st.scroll_acc -= n;
                    n as i64
                };
                // Phone: BTN_WHEEL `down:true` → content scrolls down. GstNavigation: delta_y > 0 = wheel up.
                for _ in 0..notches.abs() {
                    input(
                        5,
                        json!({ "displayId": d, "key": 4, "x": x as i64, "y": y as i64, "down": notches < 0 }),
                    );
                }
            }
            N::KeyPress { key, .. } | N::KeyRelease { key, .. } => self.on_key(d, &key, pressed),
            _ => {}
        }
    }

    fn on_key(&self, d: i64, key: &str, down: bool) {
        let sc = scancode(key);
        let (ctrl, alt, shift) = {
            let mut st = self.st.lock().unwrap();
            if let Some(code) = sc.filter(|c| is_modifier(*c)) {
                if down {
                    st.held_mods.insert(code);
                } else {
                    st.held_mods.remove(&code);
                }
            }
            let h = |c: (i64, i64)| st.held_mods.contains(&c);
            (
                h((0x1D, 0)) || h((0x1D, E0)),
                h((0x38, 0)) || h((0x38, E0)),
                h((0x2A, 0)) || h((0x36, 0)),
            )
        };
        // Phone navigation bindings (the key itself is swallowed, press and release).
        let phone_key = match key {
            "Escape" if !shift => Some(KEYCODE_BACK),
            "h" | "H" if ctrl && alt => Some(KEYCODE_HOME),
            "r" | "R" if ctrl && alt => Some(KEYCODE_APP_SWITCH),
            "b" | "B" if ctrl && alt => Some(KEYCODE_BACK),
            _ => None,
        };
        if let Some(k) = phone_key {
            if down {
                self.android_key(d, k);
            }
            return;
        }
        if let Some((code, flags)) = sc {
            self.d.input(3, json!({ "displayId": d, "flags": flags, "key": code, "down": down, "metaState": 0 }));
        } else if down {
            match key_text(key) {
                Some(text) => self.d.input(7, json!({ "displayId": d, "text": text })),
                None => println!("[{}] input: unmapped key {key}", self.sid),
            }
        }
    }

    /// An Android keycode (BACK/HOME/APP_SWITCH…) as a press + release on the virtual keyboard.
    fn android_key(&self, d: i64, keycode: i64) {
        for down in [true, false] {
            self.d.input(
                3,
                json!({ "displayId": d, "flags": FLAG_NATIVE, "key": keycode, "down": down, "metaState": 0 }),
            );
        }
    }

    /// Tears the window down. `tell_phone`: we are the ones closing, so ask the phone to end the session.
    /// Returns false when the session was already closing.
    fn close(&self, why: &str, tell_phone: bool) -> bool {
        let (d, held) = {
            let mut st = self.st.lock().unwrap();
            if std::mem::replace(&mut st.closing, true) {
                return false;
            }
            (st.display_id, std::mem::take(&mut st.held_mods))
        };
        println!("[{}] closing {} ({why})", self.sid, self.pkg);
        if tell_phone {
            if let Some(d) = d {
                for (code, flags) in held {
                    self.d.input(
                        3,
                        json!({ "displayId": d, "flags": flags, "key": code, "down": false, "metaState": 0 }),
                    );
                }
            }
            self.d
                .send15(7, Some(json!({ "sessionId": self.sid, "error": 0 })));
        }
        let _ = self.pipeline.set_state(gst::State::Null);
        true
    }

    fn install_close_hook(&self) {
        if self.close_hook_installed.load(Ordering::SeqCst) {
            return;
        }
        let Some(sink) = self.sink.lock().unwrap().clone() else {
            return;
        };
        if sink.find_property("context").is_none() {
            return;
        }
        use gst_gl::prelude::*;
        let Some(ctx) = sink.property::<Option<gst_gl::GLContext>>("context") else {
            return;
        };
        let Some(win) = ctx.window() else { return };

        if self.close_hook_installed.swap(true, Ordering::SeqCst) {
            return;
        }

        use glib::translate::ToGlibPtr;
        let raw_win: *mut gst_gl::ffi::GstGLWindow = win.to_glib_none().0;
        unsafe {
            let orig_cb = (*raw_win).close;
            let orig_data = (*raw_win).close_data;
            let hook_data = Box::into_raw(Box::new(CloseHookData {
                closed: self.closed_by_user.clone(),
                orig_cb,
                orig_data,
            }));
            gst_gl::ffi::gst_gl_window_set_close_callback(
                raw_win,
                Some(on_gl_window_close),
                hook_data as glib::ffi::gpointer,
                Some(destroy_close_hook),
            );
        }
        println!("[{}] window close hook installed", self.sid);
    }

    /// Window closed (the sink posts an error, close callback fired, or X11 window destroyed), stream ended, or ICE down for > 5 s.
    fn poll_dead(&self, x11: Option<&X11>) -> Option<String> {
        if self.closed_by_user.load(Ordering::Relaxed) {
            return Some("window closed by user".into());
        }
        if let Some(bus) = self.pipeline.bus() {
            while let Some(msg) = bus.pop() {
                match msg.view() {
                    gst::MessageView::Error(e) => {
                        return Some(format!("window closed or failed: {}", e.error()));
                    }
                    gst::MessageView::Eos(_) => return Some("end of stream".into()),
                    _ => {}
                }
            }
        }
        if let Some(x) = x11 {
            let xw = self.xwin.lock().unwrap();
            if let Some((win, _)) = *xw
                && x.window_size(win).is_none()
            {
                return Some("window closed by user (window destroyed)".into());
            }
        }
        let ice_dead = self
            .st
            .lock()
            .unwrap()
            .ice_bad_since
            .is_some_and(|t| t.elapsed() > Duration::from_secs(5));
        ice_dead.then(|| "connection to the phone lost (ICE down > 5 s)".into())
    }
}

/// taginject's `tags` property is a GstTagList in structure-string form.
fn title_tags(title: &str) -> String {
    format!("title=\"{}\"", title.replace('\\', "").replace('"', "'"))
}

/// Window titles for glimagesink's X11 (XWayland) windows: the sink ignores title tags, so we
/// look up its GL window handle and set `_NET_WM_NAME`/`WM_NAME`/`WM_CLASS` ourselves.
struct X11 {
    conn: x11rb::rust_connection::RustConnection,
    net_wm_name: u32,
    utf8: u32,
}

impl X11 {
    fn connect() -> Option<Self> {
        use x11rb::protocol::xproto::ConnectionExt as _;
        let (conn, _) = x11rb::connect(None).ok()?;
        let atom = |n: &str| {
            conn.intern_atom(false, n.as_bytes())
                .ok()?
                .reply()
                .ok()
                .map(|r| r.atom)
        };
        Some(X11 {
            net_wm_name: atom("_NET_WM_NAME")?,
            utf8: atom("UTF8_STRING")?,
            conn,
        })
    }

    fn set_title(&self, win: u32, title: &str, first: bool) {
        use x11rb::connection::Connection as _;
        use x11rb::protocol::xproto::{AtomEnum, PropMode};
        use x11rb::wrapper::ConnectionExt as _;
        let c = &self.conn;
        let _ = c.change_property8(
            PropMode::REPLACE,
            win,
            self.net_wm_name,
            self.utf8,
            title.as_bytes(),
        );
        let _ = c.change_property8(
            PropMode::REPLACE,
            win,
            AtomEnum::WM_NAME,
            AtomEnum::STRING,
            title.as_bytes(),
        );
        if first {
            let class = b"smartconnect-mirror\0smartconnect-mirror\0";
            let _ = c.change_property8(
                PropMode::REPLACE,
                win,
                AtomEnum::WM_CLASS,
                AtomEnum::STRING,
                class,
            );
        }
        let _ = c.flush();
    }

    /// The window's client-area size in pixels.
    fn window_size(&self, win: u32) -> Option<(u32, u32)> {
        use x11rb::protocol::xproto::ConnectionExt as _;
        let g = self.conn.get_geometry(win).ok()?.reply().ok()?;
        Some((u32::from(g.width), u32::from(g.height)))
    }

    fn resize_window(&self, win: u32, w: u32, h: u32) {
        use x11rb::connection::Connection as _;
        use x11rb::protocol::xproto::{ConfigureWindowAux, ConnectionExt as _};
        let aux = ConfigureWindowAux::default().width(w).height(h);
        let _ = self.conn.configure_window(win, &aux);
        let _ = self.conn.flush();
    }
}

/// The X11 window id of a glimagesink's GL window (0 until it exists).
fn gl_window_handle(sink: &gst::Element) -> u32 {
    use gst_gl::prelude::*;
    if sink.find_property("context").is_none() {
        return 0;
    }
    let Some(ctx) = sink.property::<Option<gst_gl::GLContext>>("context") else {
        return 0;
    };
    let Some(win) = ctx.window() else { return 0 };
    use glib::translate::ToGlibPtr;
    // SAFETY: `win` is a valid GstGLWindow for the duration of the call.
    unsafe { gst_gl::ffi::gst_gl_window_get_window_handle(win.to_glib_none().0) as u32 }
}

/// Maps Android display rotation (0 = natural, 1 = 90° CCW / landscape, 2 = 180°, 3 = 270°)
/// to GStreamer videoflip direction string.
fn rotation_to_direction(rot: u32) -> &'static str {
    if let Ok(dir) = std::env::var("SMARTCONNECT_ROTATION_DIR") {
        return match dir.as_str() {
            "90r" => "90r",
            "90l" => "90l",
            "180" => "180",
            "identity" => "identity",
            _ => "identity",
        };
    }
    match rot {
        1 => "90l",
        2 => "180",
        3 => "90r",
        _ => "identity",
    }
}

/// Calculates target window dimensions based on orientation transition and phone aspect ratio (600:1336).
fn calculate_adapted_window_size(curr_w: u32, curr_h: u32, target_landscape: bool) -> (u32, u32) {
    if target_landscape {
        let h = curr_h.clamp(400, 600);
        let w = ((h as f64) * (1336.0 / 600.0)) as u32;
        (w, h)
    } else {
        let h = curr_h.max(curr_w).clamp(600, 960);
        let w = ((h as f64) * (600.0 / 1336.0)) as u32;
        (w, h)
    }
}

fn create_keyframe_request_event() -> gst::Event {
    gst_video::UpstreamForceKeyUnitEvent::builder()
        .all_headers(true)
        .build()
}

impl Session {
    fn request_keyframe(&self) {
        if let Some(sink) = self.sink.lock().unwrap().as_ref() {
            let ev = create_keyframe_request_event();
            let _ = sink.send_event(ev);
        }
    }

    /// Applies rotation to the video stream (via videoflip) and the X11 window.
    fn set_rotation(&self, rot: u32, x11: Option<&X11>) {
        let is_landscape = (rot % 2) == 1;
        let changed = {
            let mut st = self.st.lock().unwrap();
            let old = st.current_rotation;
            st.current_rotation = rot;
            old != rot
        };
        if let Some(flip) = self.flip.lock().unwrap().as_ref() {
            let dir = rotation_to_direction(rot);
            flip.set_property_from_str("video-direction", dir);
            println!("[{}] stream rotation → {rot} ({dir})", self.sid);
        }
        if changed {
            // Unpause encoder if the phone paused it on orientation change
            self.d.send15(6, Some(json!({ "sessionId": self.sid })));
            self.request_keyframe();
        }
        if let Some(x) = x11 {
            self.adapt_window_orientation(x, is_landscape);
        }
    }

    fn adapt_window_orientation(&self, x: &X11, target_landscape: bool) {
        let win = self
            .xwin
            .lock()
            .unwrap()
            .as_ref()
            .map(|(w, _)| *w)
            .or_else(|| {
                self.sink
                    .lock()
                    .unwrap()
                    .as_ref()
                    .map(gl_window_handle)
                    .filter(|w| *w != 0)
            });
        let Some(win) = win else { return };
        let rot = self.st.lock().unwrap().current_rotation;
        if !self.st.lock().unwrap().check_and_mark_adapted_rotation(rot) {
            return;
        }
        if let Some((curr_w, curr_h)) = x.window_size(win) {
            let curr_landscape = curr_w > curr_h;
            if curr_landscape != target_landscape {
                let (new_w, new_h) =
                    calculate_adapted_window_size(curr_w, curr_h, target_landscape);
                x.resize_window(win, new_w, new_h);
                println!(
                    "[{}] rotation changed: resized window from {curr_w}x{curr_h} to {new_w}x{new_h}",
                    self.sid
                );
            }
        }
    }

    fn sync_x11_title(&self, x: &X11) {
        self.install_close_hook();
        let title = self.title.lock().unwrap().clone();
        let mut xw = self.xwin.lock().unwrap();
        match &mut *xw {
            Some((win, last)) if *last != title => {
                x.set_title(*win, &title, false);
                *last = title;
            }
            Some(_) => {}
            None => {
                let Some(sink) = self.sink.lock().unwrap().clone() else {
                    return;
                };
                let win = gl_window_handle(&sink);
                if win != 0 {
                    x.set_title(win, &title, true);
                    *xw = Some((win, title));
                }
            }
        }
    }
}

/// Raises (focuses) a window by title on niri, Hyprland and sway; other desktops: no-op.
fn raise_window(title: String) {
    std::thread::spawn(move || {
        let has = |k: &str| std::env::var_os(k).is_some_and(|v| !v.is_empty());
        let run = |argv: &[&str]| {
            let _ = std::process::Command::new(argv[0])
                .args(&argv[1..])
                .stdout(std::process::Stdio::null())
                .status();
        };
        if has("HYPRLAND_INSTANCE_SIGNATURE") {
            let Ok(out) = std::process::Command::new("hyprctl")
                .args(["-j", "clients"])
                .output()
            else {
                return;
            };
            let Ok(list) = serde_json::from_slice::<Value>(&out.stdout) else {
                return;
            };
            let addr = list
                .as_array()
                .into_iter()
                .flatten()
                .find(|w| w["title"].as_str() == Some(&title))
                .and_then(|w| w["address"].as_str());
            if let Some(addr) = addr {
                run(&[
                    "hyprctl",
                    "dispatch",
                    "focuswindow",
                    &format!("address:{addr}"),
                ]);
            }
        } else if has("SWAYSOCK") {
            // sway criteria take a regex: match the title literally.
            let quoted: String = title
                .chars()
                .flat_map(|c| {
                    if "\\^$.|?*+()[]{}\"".contains(c) {
                        vec!['\\', c]
                    } else {
                        vec![c]
                    }
                })
                .collect();
            run(&["swaymsg", &format!("[title=\"^{quoted}$\"] focus")]);
        } else {
            let Ok(out) = std::process::Command::new("niri")
                .args(["msg", "--json", "windows"])
                .output()
            else {
                return;
            };
            let Ok(list) = serde_json::from_slice::<Value>(&out.stdout) else {
                return;
            };
            let id = list
                .as_array()
                .into_iter()
                .flatten()
                .find(|w| w["title"].as_str() == Some(&title))
                .and_then(|w| w["id"].as_u64());
            if let Some(id) = id {
                run(&[
                    "niri",
                    "msg",
                    "action",
                    "focus-window",
                    "--id",
                    &id.to_string(),
                ]);
            }
        }
    });
}

// ---------------------------------------------------------------------------------------------
// The host: routes svc 15 frames to sessions, starts new ones one at a time.

#[derive(Clone, Debug)]
struct StartReq {
    pkg: String,
    cmp: Value,
    user_id: Value,
    shortcut_id: Value,
    source: i64,
    name: Option<String>,
    /// Answer without a window and close once the phone ran the notification action.
    headless: bool,
}

struct Host {
    d: Arc<Daemon>,
    instance_id: i64,
    /// View sessions (whole-screen mirror, Ready For desktop, virtual phone) belong to this process.
    own_mirror: bool,
    /// App sessions (any other pkg, incl. phone-initiated) belong to this process.
    own_apps: bool,
    /// Answer the phone's svc 1 cmd 0 (Screen share / Mobile desktop started on the phone).
    answer_views: bool,
    /// Run notification action buttons without a window (actions.rs).
    background: bool,
    actions: actions::PendingActions,
    /// Ask the phone to resize app displays to their window (resize.rs; opt-in).
    resize: bool,
    last_resize_poll: Instant,
    /// The desktop resolution from the phone's last view invite (used for the "desktop" stream).
    view_resolution: Option<(i64, i64)>,
    /// One-shot process: quit once its sessions are gone.
    exit_when_idle: bool,
    sessions: HashMap<i64, Arc<Session>>,
    queue: VecDeque<StartReq>,
    inflight: Option<(StartReq, Instant)>,
    had_session: bool,
    last_hb: Instant,
    /// pkg → app name (svc 17 list, cmd 2 pkgName, cmd 13 appName).
    names: HashMap<String, String>,
    apps: Value,
    x11: Option<X11>,
    main_loop: glib::MainLoop,
}

impl Host {
    fn title_for(&self, pkg: &str) -> String {
        if let Some(t) = views::title(pkg) {
            return t.into();
        }
        format!(
            "{} — Phone",
            self.names
                .get(pkg)
                .cloned()
                .unwrap_or_else(|| pkg.to_owned())
        )
    }

    fn owns_pkg(&self, pkg: &str) -> bool {
        if views::is_view_pkg(pkg) {
            self.own_mirror
        } else {
            self.own_apps
        }
    }

    fn by_display(&self, d: i64) -> Option<Arc<Session>> {
        self.sessions
            .values()
            .find(|s| s.display_id() == Some(d))
            .cloned()
    }

    fn request(&mut self, r: StartReq, front: bool) {
        if let Some(n) = &r.name {
            self.names.insert(r.pkg.clone(), n.clone());
        }
        // The phone re-sends unanswered invites; one cmd 1 per app is enough.
        let starting = self.inflight.as_ref().is_some_and(|(q, _)| q.pkg == r.pkg)
            || self.queue.iter().any(|q| q.pkg == r.pkg);
        if starting {
            println!("already starting {}; ignoring the repeated request", r.pkg);
            return;
        }
        if front {
            self.queue.push_front(r)
        } else {
            self.queue.push_back(r)
        }
        self.pump();
    }

    /// Sends the next queued cmd 1. The phone keeps a single pending start per PC, so wait for the
    /// cmd 3 offer (or a cmd 8 state 0 / cmd 12 merge, or 20 s) before the next one.
    fn pump(&mut self) {
        if self
            .inflight
            .as_ref()
            .is_some_and(|(_, t)| t.elapsed() < Duration::from_secs(20))
        {
            return;
        }
        if let Some((r, _)) = self.inflight.take() {
            eprintln!("start of {} timed out", r.pkg);
        }
        let Some(r) = self.queue.pop_front() else {
            return;
        };
        println!(
            "requesting stream: pkg {} cmp {} source {}",
            r.pkg, r.cmp, r.source
        );
        // The size is the bound the phone fits the stream into. For Ready For desktop it is the
        // desktop resolution, so use the one the user picked on the phone when we know it.
        let (w, h) = match (r.pkg.as_str(), self.view_resolution) {
            ("desktop", Some(res)) => res,
            _ => (1920, 1080),
        };
        let mm = |px: i64| (px as f64 * 344.0 / 1920.0).round() as i64;
        let mut data = json!({
            "pkg": r.pkg, "userId": r.user_id,
            "width": w, "height": h, "phyWidth": mm(w), "phyHeight": mm(h),
            "displayWidth": w, "displayHeight": h, "displayDensity": 160, "displayScale": 1.0,
            "align": 0, "instanceId": self.instance_id, "port": 0, "source": r.source,
            "supportedMediaFormat": 1, "supportWebRTC": 1,
            // Booleans in the phone's bean: sending 0 makes Gson throw and the start silently dies.
            "supportCustomDisplay": false, "vrSupportUnfolded": false,
        });
        if r.pkg != "mirror" {
            data["cmp"] = r.cmp.clone();
            data["shortcutId"] = r.shortcut_id.clone();
        }
        self.d.send15(1, Some(data));
        self.inflight = Some((r, Instant::now()));
    }

    fn finish_inflight(&mut self, pkg: &str, ok: bool, why: &str) {
        if self.inflight.as_ref().is_some_and(|(r, _)| r.pkg == pkg) {
            if !ok {
                eprintln!("could not start {pkg}: {why}");
            }
            self.inflight = None;
            self.pump();
        }
    }

    fn close_session(&mut self, sid: i64, why: &str, tell_phone: bool) {
        if let Some(s) = self.sessions.remove(&sid) {
            s.close(why, tell_phone);
        }
    }

    fn close_all(&mut self, why: &str, tell_phone: bool) {
        for sid in self.sessions.keys().copied().collect::<Vec<_>>() {
            self.close_session(sid, why, tell_phone);
        }
    }

    fn on_frame(&mut self, svc: i64, msg: &Value) -> Result<()> {
        match svc {
            1 => return self.on_svc1(msg),
            17 => return self.on_svc17(msg),
            actions::SERVICE_LOCAL_EVENTS => return self.on_local_event(msg),
            15 => {}
            _ => return Ok(()),
        }
        let cmd = msg.get("cmd").and_then(Value::as_i64).unwrap_or(-1);
        let body = msg.get("data").cloned().unwrap_or(Value::Null);
        let sid = body.get("sessionId").and_then(Value::as_i64);
        let pkg = body
            .get("pkg")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned();
        let mine = sid.and_then(|s| self.sessions.get(&s).cloned());
        let shown = |d: Option<i64>, me: &Self| d.and_then(|d| me.by_display(d));
        // The stream invite (cmd 2) and focus changes (cmd 13) carry the app's icon as base64 PNG:
        // keep it for the GUI (`<icon cache>/<pkg>.png`, written once).
        if matches!(cmd, 2 | 13)
            && let Some(icon) = body
                .get("icon")
                .and_then(Value::as_str)
                .filter(|i| !i.is_empty())
            && let Some(path) = icons::harvest(&pkg, icon)
        {
            println!("saved the icon of {pkg} to {}", path.display());
        }
        match cmd {
            // Stream invite: the phone set up a view (after svc 1) or wants to show an app here
            // (notification "Open" or action button, hand-off, the phone's App streaming picker).
            // Answer cmd 1 with source 1 and the echoed fields.
            2 => {
                if !self.owns_pkg(&pkg) {
                    return Ok(());
                }
                if views::is_view_pkg(&pkg) && self.sessions.values().any(|s| s.pkg == pkg) {
                    println!("ignoring a new {pkg} setup: already open");
                    return Ok(());
                }
                let headless = !views::is_view_pkg(&pkg)
                    && self.background
                    && self.actions.take(&pkg, Instant::now());
                println!(
                    "phone invites stream: pkg {pkg} ({}) source {}{}",
                    body["pkgName"],
                    body["source"],
                    if headless {
                        " — notification action, running it without a window"
                    } else {
                        ""
                    }
                );
                self.request(
                    StartReq {
                        pkg: pkg.clone(),
                        cmp: body.get("cmp").cloned().unwrap_or(Value::Null),
                        user_id: body.get("userId").cloned().unwrap_or(json!(0)),
                        shortcut_id: body.get("shortcutId").cloned().unwrap_or(Value::Null),
                        source: body.get("source").and_then(Value::as_i64).unwrap_or(1),
                        name: body
                            .get("pkgName")
                            .and_then(Value::as_str)
                            .map(str::to_owned),
                        headless,
                    },
                    true,
                );
            }
            3 => {
                let Some(sid) = sid else { return Ok(()) };
                let offer = body.get("webRTCData").and_then(Value::as_str);
                let s = match (mine, offer) {
                    (Some(s), _) => s,
                    (None, Some(_)) if self.owns_pkg(&pkg) => {
                        let req = self
                            .inflight
                            .as_ref()
                            .map(|(r, _)| r)
                            .filter(|r| r.pkg == pkg);
                        let headless = req.is_some_and(|r| r.headless);
                        let field = |k: &str, fallback: Option<&Value>| {
                            body.get(k)
                                .filter(|v| !v.is_null())
                                .or(fallback)
                                .cloned()
                                .unwrap_or(Value::Null)
                        };
                        let info = SessionInfo {
                            sid,
                            pkg: pkg.clone(),
                            user_id: body.get("userId").cloned().unwrap_or(json!(0)),
                            cmp: field("cmp", req.map(|r| &r.cmp)),
                            shortcut_id: field("shortcutId", req.map(|r| &r.shortcut_id)),
                            title: self.title_for(&pkg),
                            headless,
                        };
                        let filter = ice::CandidateFilter::current(
                            daemon_status()
                                .as_ref()
                                .and_then(ice::pc_address_from_status),
                        );
                        println!(
                            "[{sid}] new session: pkg {pkg} {}x{}{}; ICE: offering {filter}",
                            body["width"],
                            body["height"],
                            if headless { " (headless)" } else { "" }
                        );
                        let s = Session::new(info, filter, self.d.clone())?;
                        self.sessions.insert(sid, s.clone());
                        self.had_session = true;
                        self.finish_inflight(&pkg, true, "");
                        s
                    }
                    _ => return Ok(()),
                };
                s.learn_display(&body);
                s.learn_stream_size(&body);
                if let Some(offer) = offer
                    && !s.st.lock().unwrap().answered
                {
                    let offer: Value = serde_json::from_str(offer)?;
                    s.answer(offer["sdp"].as_str().context("offer without sdp")?)?;
                    self.d.send15(9, None);
                    self.last_hb = Instant::now();
                }
            }
            20 => {
                if let Some(s) = mine {
                    s.add_remote_candidate(&body)?;
                }
            }
            8 => {
                let state = body.get("state").and_then(Value::as_i64).unwrap_or(-1);
                let target = mine.or_else(|| {
                    if sid.is_some() {
                        return None;
                    }
                    body.get("displayId")
                        .and_then(Value::as_i64)
                        .and_then(|d| self.by_display(d))
                });
                match (state, target) {
                    (1, Some(s)) => {
                        println!(
                            "[{}] connected: display {} supportTouch {}",
                            s.sid, body["displayId"], body["supportTouch"]
                        );
                        s.learn_display(&body);
                    }
                    (0, Some(s)) => {
                        let why = format!(
                            "phone stopped it (reason {} {})",
                            body["reason"], body["msg"]
                        );
                        self.close_session(s.sid, &why, false);
                    }
                    (0, None) => {
                        let why = format!("reason {} {}", body["reason"], body["msg"]);
                        self.finish_inflight(&pkg, false, &why);
                    }
                    (2, Some(s)) => {
                        println!(
                            "[{}] phone paused encoder (state 2 reason {}); resuming with cmd 6",
                            s.sid, body["reason"]
                        );
                        self.d.send15(6, Some(json!({ "sessionId": s.sid })));
                        s.request_keyframe();
                    }
                    (3, Some(s)) => {
                        println!("[{}] phone resumed encoder (state 3)", s.sid);
                        s.request_keyframe();
                    }
                    (st, Some(s)) => println!("[{}] state {st} reason {}", s.sid, body["reason"]),
                    _ => {}
                }
            }
            7 => {
                if let Some(s) = mine {
                    self.close_session(s.sid, "phone closed the session", false);
                }
            }
            // App merged into an existing display / move-to-top: raise that window. For a
            // background action this is the phone running the intent on that display.
            12 | 14 => {
                if cmd == 12 {
                    self.finish_inflight(&pkg, true, "");
                }
                if let Some(s) = shown(body.get("displayId").and_then(Value::as_i64), self) {
                    if s.headless {
                        s.mark_fired();
                    } else {
                        println!("[{}] raise (cmd {cmd})", s.sid);
                        raise_window(s.title.lock().unwrap().clone());
                    }
                }
            }
            // Focus changed inside a display: retitle that window. The phone fires a pending
            // notification intent at exactly this moment (background actions: close soon).
            13 => {
                let d = body.get("displayId").and_then(Value::as_i64);
                if let Some(s) = shown(d, self) {
                    if s.headless {
                        s.mark_fired();
                    } else if let Some(name) = body
                        .get("appName")
                        .and_then(Value::as_str)
                        .filter(|_| !views::is_view_pkg(&s.pkg))
                    {
                        if !pkg.is_empty() {
                            self.names.insert(pkg.clone(), name.to_owned());
                        }
                        s.set_title(&format!("{name} — Phone"));
                    }
                }
            }
            16 => {
                if let Some(s) = mine {
                    let rot = body.get("rotation").and_then(Value::as_u64).unwrap_or(0) as u32;
                    println!(
                        "[{}] cmd 16: rotation {rot} display {}",
                        s.sid, body["displayId"]
                    );
                    s.learn_display(&body);
                    s.set_rotation(rot, self.x11.as_ref());
                }
            }
            23 => {
                if let Some(s) = mine {
                    println!(
                        "[{}] cmd 23 display state: state {} display {}",
                        s.sid, body["state"], body["displayId"]
                    );
                    s.learn_display(&body);
                }
            }
            26 => {
                let d = body.get("displayId").and_then(Value::as_i64);
                if let Some(s) = shown(d, self) {
                    println!(
                        "[{}] resize result: success {} error {}",
                        s.sid, body["success"], body["error"]
                    );
                }
            }
            10 | 21 => {} // heartbeat ack, audio app list
            _ => {
                if self.own_apps || mine.is_some() {
                    println!("svc15 cmd {cmd} {}", truncate(&body.to_string(), 300));
                }
            }
        }
        Ok(())
    }

    fn on_svc1(&mut self, msg: &Value) -> Result<()> {
        // cmd 0: the user started a view on the phone (Screen share / Mobile desktop tile, phone
        // cards). The phone waits 20 s for our cmd 1, then invites the stream on svc 15.
        if let Some(inv) = views::parse_invite(msg) {
            if self.answer_views {
                let pkg = views::view_pkg(inv.view).unwrap_or("?");
                println!(
                    "phone asks to start {pkg} (view {}, resolution {:?}): accepting",
                    inv.view, inv.resolution
                );
                if inv.view == 2 {
                    self.view_resolution = inv.resolution;
                }
                self.d.send(1, views::accept(inv.view));
            }
            return Ok(());
        }
        let cmd = msg.get("cmd").and_then(Value::as_i64);
        // 5 = stop/cancel, 11 = error: the phone ended (or refused) the view.
        if self.own_mirror && matches!(cmd, Some(5 | 11)) {
            println!(
                "svc1 cmd {} {}",
                cmd.unwrap_or(0),
                truncate(&msg.to_string(), 300)
            );
            let ended: Vec<i64> = self
                .sessions
                .values()
                .filter(|s| views::is_view_pkg(&s.pkg))
                .map(|s| s.sid)
                .collect();
            for sid in ended {
                self.close_session(sid, "phone ended the view", false);
            }
            if self.exit_when_idle && self.sessions.is_empty() {
                self.main_loop.quit();
            }
        }
        Ok(())
    }

    fn on_svc17(&mut self, msg: &Value) -> Result<()> {
        if msg.get("cmd").and_then(Value::as_i64) != Some(1) {
            return Ok(());
        }
        let apps = app_list(msg);
        for a in apps.as_array().into_iter().flatten() {
            if let (Some(p), Some(n)) = (a["pkg"].as_str(), a["name"].as_str()) {
                self.names.insert(p.to_owned(), n.to_owned());
            }
        }
        println!("app list: {} apps", apps.as_array().map_or(0, Vec::len));
        save_apps(&apps);
        self.apps = apps;
        Ok(())
    }

    /// Daemon-local events (`watch` service -1): notification clicks the phone will carry out
    /// through app streaming.
    fn on_local_event(&mut self, ev: &Value) -> Result<()> {
        if !(self.background && self.own_apps) {
            return Ok(());
        }
        if let Some(click) = actions::parse_click(ev) {
            let what = match click.kind {
                actions::ClickKind::Action => {
                    "action button: the next invite runs without a window"
                }
                actions::ClickKind::Open => "open: shown in a window",
            };
            println!("notification click for {}: {what}", click.pkg);
            self.actions.note(&click, Instant::now());
        }
        Ok(())
    }

    fn on_ctl(&mut self, v: &Value) -> Value {
        match v.get("cmd").and_then(Value::as_str).unwrap_or("") {
            "app" => {
                let Some(pkg) = v.get("pkg").and_then(Value::as_str) else {
                    return json!({ "ok": false, "message": "app needs pkg" });
                };
                if let Some(s) = self.sessions.values().find(|s| s.pkg == pkg && !s.headless) {
                    raise_window(s.title.lock().unwrap().clone());
                }
                self.request(
                    start_req_for(pkg, v.get("cmp").and_then(Value::as_str), &self.apps),
                    false,
                );
                json!({ "ok": true, "message": "requested" })
            }
            "sessions" => json!({ "ok": true, "sessions": self.sessions.values().map(|s| json!({
                "sessionId": s.sid, "pkg": s.pkg, "displayId": s.display_id(), "title": *s.title.lock().unwrap(),
                "headless": s.headless,
            })).collect::<Vec<_>>() }),
            "close" => match v.get("sessionId").and_then(Value::as_i64) {
                Some(sid) if self.sessions.contains_key(&sid) => {
                    self.close_session(sid, "closed via control socket", true);
                    json!({ "ok": true })
                }
                _ => json!({ "ok": false, "message": "no such session" }),
            },
            "apps" => json!({ "ok": true, "apps": self.apps }),
            "mirror" => {
                println!("asking the phone to start mirroring");
                self.d.send(1, views::start(1));
                json!({ "ok": true, "message": "requested" })
            }
            "desktop" => {
                println!("asking the phone to start Ready For desktop");
                self.d.send(1, views::start(2));
                json!({ "ok": true, "message": "requested" })
            }
            _ => {
                json!({ "ok": false, "message": "commands: app {pkg,cmp?}, sessions, close {sessionId}, apps, mirror, desktop" })
            }
        }
    }

    /// Periodic work on the main loop.
    fn tick(&mut self) {
        for s in self.sessions.values() {
            s.flush_pending_mouse();
            s.install_close_hook();
        }
        let now = Instant::now();
        let dead: Vec<(i64, String)> = self
            .sessions
            .values()
            .filter_map(|s| s.poll_dead(self.x11.as_ref()).map(|w| (s.sid, w)))
            .collect();
        for (sid, why) in dead {
            self.close_session(sid, &why, true);
        }
        let done: Vec<(i64, &str)> = self
            .sessions
            .values()
            .filter_map(|s| s.headless_due(now).map(|w| (s.sid, w)))
            .collect();
        for (sid, why) in done {
            self.close_session(sid, why, true);
        }
        if let Some(x) = &self.x11 {
            for s in self.sessions.values() {
                s.sync_x11_title(x);
            }
            if self.resize && now.duration_since(self.last_resize_poll) >= RESIZE_POLL {
                self.last_resize_poll = now;
                for s in self.sessions.values() {
                    if let Some(req) = s.poll_resize(x, self.instance_id, now) {
                        self.d.send15(17, Some(req));
                    }
                }
            }
        }
        // One heartbeat covers every session of this PC.
        if !self.sessions.is_empty() && self.last_hb.elapsed() >= Duration::from_secs(30) {
            self.d.send15(9, None);
            self.last_hb = Instant::now();
        }
        self.pump();
        if self.exit_when_idle
            && self.had_session
            && self.sessions.is_empty()
            && self.inflight.is_none()
            && self.queue.is_empty()
        {
            println!("all windows closed");
            self.main_loop.quit();
        }
    }
}

fn truncate(s: &str, n: usize) -> &str {
    match s.char_indices().nth(n) {
        Some((i, _)) => &s[..i],
        None => s,
    }
}

/// svc 17 cmd 1 → `[{pkg,name,cmp,userId,iconName,work}]` (personal + work-profile apps; the
/// download token/port are dropped). `iconName` is the file name the GUI looks up in the icon cache.
fn app_list(msg: &Value) -> Value {
    let pick = |key: &str| -> Vec<Value> {
        msg.get(key)
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .map(|a| {
                json!({
                    "pkg": a["pkg"], "name": a["name"], "cmp": a["cmp"], "userId": a["userId"],
                    "iconName": a["iconName"], "work": key == "wpApps",
                })
            })
            .collect()
    };
    let mut all = pick("apps");
    all.extend(pick("wpApps"));
    Value::Array(all)
}

fn save_apps(apps: &Value) {
    let path = apps_cache_path();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(&path, apps.to_string());
}

fn load_apps() -> Value {
    std::fs::read_to_string(apps_cache_path())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or(json!([]))
}

/// A PC-initiated start (source 0). `cmp`/`userId`/name come from the cached app list when known;
/// an empty `cmp` makes the phone resolve the package's launcher activity.
fn start_req_for(pkg: &str, cmp: Option<&str>, apps: &Value) -> StartReq {
    let mut list = apps.clone();
    if list.as_array().is_none_or(Vec::is_empty) {
        list = load_apps();
    }
    let known = list
        .as_array()
        .into_iter()
        .flatten()
        .find(|a| a["pkg"].as_str() == Some(pkg))
        .cloned();
    let cmp = cmp
        .map(|c| json!(c))
        .or_else(|| known.as_ref().map(|a| a["cmp"].clone()))
        .unwrap_or(json!(""));
    StartReq {
        pkg: pkg.to_owned(),
        cmp,
        user_id: known
            .as_ref()
            .map(|a| a["userId"].clone())
            .filter(|u| !u.is_null())
            .unwrap_or(json!(0)),
        shortcut_id: Value::Null,
        source: 0,
        name: known
            .as_ref()
            .and_then(|a| a["name"].as_str())
            .map(str::to_owned),
        headless: false,
    }
}

/// Keep only H.264 payload types (the phone lists H.265 first, which we don't decode yet).
fn h264_only(sdp: &str) -> String {
    let lines: Vec<&str> = sdp.split("\r\n").flat_map(|l| l.split('\n')).collect();
    let h264: Vec<String> = lines
        .iter()
        .filter_map(|l| l.strip_prefix("a=rtpmap:"))
        .filter(|r| r.contains(" H264/90000"))
        .filter_map(|r| r.split(' ').next().map(str::to_owned))
        .collect();
    if h264.is_empty() {
        eprintln!("offer has no H.264; leaving codecs as offered");
        return sdp.to_owned();
    }
    let pt_of = |l: &str| -> Option<String> {
        ["a=rtpmap:", "a=fmtp:", "a=rtcp-fb:"]
            .iter()
            .find_map(|p| l.strip_prefix(p))
            .and_then(|r| r.split(' ').next())
            .map(str::to_owned)
    };
    let mut out = Vec::new();
    for l in lines {
        if l.starts_with("a=extmap:") && l.contains("urn:3gpp:video-orientation") {
            continue; // make the phone rotate frames itself instead of tagging them
        }
        if l.starts_with("m=video") {
            let parts: Vec<&str> = l.split(' ').collect();
            let keep: Vec<&str> = parts[3..]
                .iter()
                .copied()
                .filter(|pt| h264.iter().any(|h| h == pt))
                .collect();
            out.push(format!("{} {}", parts[..3].join(" "), keep.join(" ")));
        } else if let Some(pt) = pt_of(l) {
            if h264.contains(&pt) {
                out.push(l.to_owned());
            }
        } else {
            out.push(l.to_owned());
        }
    }
    out.join("\r\n")
}

const BINDINGS: &str =
    "keys: everything is sent as real key presses (Ctrl+C/V/A, Alt+Tab, arrows, F-keys…)
  Esc            → Android BACK      (Shift+Esc sends a real Escape)
  Ctrl+Alt+B     → BACK   (also mouse back button)
  Ctrl+Alt+H     → HOME
  Ctrl+Alt+R     → RECENTS (app switcher)
  Super combos are usually taken by the desktop compositor.";

const USAGE: &str = "usage: smartconnect-mirror [--host [--resize] | --desktop | --app PKG [CMP] | --list-apps | --sessions | --close SESSIONID]
  (no args)        mirror the whole phone screen
  --desktop        Ready For desktop on a phone virtual display (experimental)
  --host           stay up: open a window for every stream the phone starts here (notification
                   Open, App streaming / Screen share / Mobile desktop tiles, hand-off), run
                   notification action buttons without a window, and accept requests on
                   $XDG_RUNTIME_DIR/smartconnect-mirror.sock
  --resize         (with --host) ask the phone to resize app displays to their windows (experimental;
                   also SMARTCONNECT_MIRROR_RESIZE=1)
  --app PKG [CMP]  stream one phone app (through the host when one runs)
  --list-apps [--json]  print the phone's launchable apps (pkg, userId, name, cmp; --json: one JSON array)
  --sessions       print the host's open sessions (JSON)
  --close SID      close one host session
environment:
  SMARTCONNECT_BACKGROUND_ACTIONS=0   notification action buttons open a window like Open does
  SMARTCONNECT_ICE_ADDRESSES=IP,...   local ICE addresses to offer (IPs, IPv4 CIDRs, or all)
  MIRROR_LATENCY=ms                   webrtc jitterbuffer latency in ms (default: 30)
  MIRROR_DECODER=...                  custom H.264 decoder (or cpu, vaapi, nvdec)
  MIRROR_SINK=...                     custom GStreamer video sink element";

/// Hardware H.264 decoding when available (VA-API for Intel/AMD, NVDEC with cudadownload for NVIDIA),
/// else slice-threaded libav. Supports integrated, discrete, and hybrid dual-GPU systems.
/// Override: MIRROR_DECODER=… (or "cpu", "vaapi", "nvdec")
fn pick_decoder() -> String {
    pick_decoder_from(std::env::var("MIRROR_DECODER").ok().as_deref())
}

fn pick_decoder_from(override_dec: Option<&str>) -> String {
    let ok = |n: &str| gst::ElementFactory::make(n).build().is_ok();
    if let Some(d) = override_dec {
        match d {
            "cpu" => return "avdec_h264 thread-type=slice max-threads=4".into(),
            "nvdec" | "nvidia" => {
                if ok("nvh264dec") && ok("cudadownload") {
                    return "nvh264dec ! cudadownload ! video/x-raw".into();
                } else {
                    eprintln!(
                        "nvdec requested but nvh264dec/cudadownload failed instantiation; falling back to avdec_h264"
                    );
                    return "avdec_h264 thread-type=slice max-threads=4".into();
                }
            }
            "vaapi" => {
                if ok("vah264dec") {
                    return "vah264dec".into();
                } else if ok("vaapih264dec") {
                    return "vaapih264dec".into();
                } else {
                    eprintln!(
                        "vaapi requested but vah264dec/vaapih264dec failed instantiation; falling back to avdec_h264"
                    );
                    return "avdec_h264 thread-type=slice max-threads=4".into();
                }
            }
            _ => return d.into(),
        }
    }
    if ok("vah264dec") {
        "vah264dec".into()
    } else if ok("vaapih264dec") {
        "vaapih264dec".into()
    } else if ok("nvh264dec") && ok("cudadownload") {
        // Dedicated NVIDIA GPU: decode via NVDEC, then download to system RAM (video/x-raw)
        // so it renders universally across all display configurations (single dGPU or hybrid iGPU/dGPU).
        "nvh264dec ! cudadownload ! video/x-raw".into()
    } else {
        "avdec_h264 thread-type=slice max-threads=4".into()
    }
}

/// glimagesink forwards wheel events on Wayland (waylandsink has no scroll support). Override: MIRROR_SINK=…
fn pick_sink() -> String {
    if let Ok(s) = std::env::var("MIRROR_SINK") {
        return s;
    }
    let ok = |n: &str| gst::ElementFactory::make(n).build().is_ok();
    if ok("glimagesink") {
        "glimagesink".into()
    } else {
        "autovideosink".into()
    }
}

/// One request/reply on the host's control socket; None when no host is running.
fn host_call(req: &Value) -> Option<Value> {
    let mut s = UnixStream::connect(host_socket_path()).ok()?;
    s.write_all(format!("{req}\n").as_bytes()).ok()?;
    let mut line = String::new();
    BufReader::new(s).read_line(&mut line).ok()?;
    serde_json::from_str(&line).ok()
}

fn serve_control(tx: mpsc::Sender<Ev>) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let path = host_socket_path();
    if UnixStream::connect(&path).is_ok() {
        bail!("a host is already running ({})", path.display());
    }
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
        let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
    }
    let _ = std::fs::remove_file(&path);
    let listener = UnixListener::bind(&path)?;
    let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    std::thread::spawn(move || {
        for conn in listener.incoming().map_while(Result::ok) {
            let tx = tx.clone();
            std::thread::spawn(move || {
                let Ok(mut w) = conn.try_clone() else { return };
                for line in BufReader::new(conn).lines().map_while(Result::ok) {
                    let reply = match serde_json::from_str::<Value>(&line) {
                        Ok(v) => {
                            let (rtx, rrx) = mpsc::channel();
                            if tx.send(Ev::Ctl(v, rtx)).is_err() {
                                return;
                            }
                            rrx.recv_timeout(Duration::from_secs(5))
                                .unwrap_or_else(|_| "{\"ok\":false}".into())
                        }
                        Err(e) => json!({ "ok": false, "message": e.to_string() }).to_string(),
                    };
                    if w.write_all(format!("{reply}\n").as_bytes()).is_err() {
                        return;
                    }
                }
            });
        }
    });
    Ok(())
}

/// `--list-apps`: asks the phone to re-send its app list (svc 17 cmd 2) and prints it.
/// With `json`, prints the list as one JSON array (`pkg`, `name`, `cmp`, `userId`, `work`) instead of TSV.
fn list_apps(json: bool) -> Result<()> {
    let (tx, rx) = mpsc::channel();
    watch(&[17], tx)?;
    let d = Daemon::connect()?;
    std::thread::sleep(Duration::from_millis(200));
    d.send(17, json!({ "ver": "1", "cmd": 2 }));
    let deadline = Instant::now() + Duration::from_secs(15);
    let apps = loop {
        let left = deadline.saturating_duration_since(Instant::now());
        match rx.recv_timeout(left) {
            Ok(Ev::Phone(17, msg)) if msg.get("cmd").and_then(Value::as_i64) == Some(1) => {
                break app_list(&msg);
            }
            Ok(Ev::Phone(..)) => continue,
            _ => {
                let cached = load_apps();
                if cached.as_array().is_some_and(|a| !a.is_empty()) {
                    eprintln!(
                        "the phone did not answer; showing the cached list ({})",
                        apps_cache_path().display()
                    );
                    break cached;
                }
                bail!("the phone did not send its app list (svc 17) within 15 s");
            }
        }
    };
    save_apps(&apps);
    if json {
        println!("{apps}");
        return Ok(());
    }
    for a in apps.as_array().into_iter().flatten() {
        println!(
            "{}\t{}\t{}\t{}",
            a["pkg"].as_str().unwrap_or(""),
            a["userId"],
            a["name"].as_str().unwrap_or(""),
            a["cmp"].as_str().unwrap_or("")
        );
    }
    Ok(())
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mode = args.first().map(String::as_str).unwrap_or("");
    match mode {
        "-h" | "--help" => {
            println!("{USAGE}");
            return Ok(());
        }
        "--list-apps" => return list_apps(args.get(1).is_some_and(|a| a == "--json")),
        "--sessions" => {
            let r = host_call(&json!({ "cmd": "sessions" })).context("no host running")?;
            println!("{r}");
            return Ok(());
        }
        "--close" => {
            let sid: i64 = args.get(1).context("--close SESSIONID")?.parse()?;
            println!(
                "{}",
                host_call(&json!({ "cmd": "close", "sessionId": sid }))
                    .context("no host running")?
            );
            return Ok(());
        }
        "--app" => {
            let pkg = args.get(1).context("--app needs a package name")?;
            let mut req = json!({ "cmd": "app", "pkg": pkg });
            if let Some(cmp) = args.get(2) {
                req["cmp"] = json!(cmp);
            }
            if let Some(r) = host_call(&req) {
                println!("host: {r}");
                return Ok(());
            }
            println!("no host running: streaming {pkg} from this process");
        }
        // A running host owns the views too (one process, one instanceId, no double answers).
        "" | "--desktop" => {
            let cmd = if mode.is_empty() { "mirror" } else { "desktop" };
            if let Some(r) = host_call(&json!({ "cmd": cmd })) {
                println!("host: {r}");
                return Ok(());
            }
        }
        "--host" => {}
        other => bail!("unknown argument {other}\n{USAGE}"),
    }
    for extra in args
        .iter()
        .skip(1)
        .filter(|a| a.starts_with("--") && mode != "--app")
    {
        if !(mode == "--host" && extra == "--resize") {
            eprintln!("ignoring unknown argument {extra} for {mode:?}");
        }
    }

    // GL's Wayland window has no keyboard support, its X11 window has keys + wheel + buttons:
    // prefer X11 (XWayland) when there is one. MIRROR_GL_WAYLAND=1 keeps native Wayland.
    if std::env::var_os("DISPLAY").is_some()
        && std::env::var_os("GST_GL_WINDOW").is_none()
        && std::env::var_os("MIRROR_GL_WAYLAND").is_none()
    {
        // SAFETY: single-threaded here, before GStreamer or any other thread starts.
        unsafe { std::env::set_var("GST_GL_WINDOW", "x11") };
    }
    gst::init()?;
    let main_loop = glib::MainLoop::new(None, false);

    let (tx, rx) = mpsc::channel::<Ev>();
    watch(&[1, 15, 17, actions::SERVICE_LOCAL_EVENTS], tx.clone())?;
    let d = Daemon::connect()?;
    let is_host = mode == "--host";
    if is_host {
        serve_control(tx.clone())?;
    }
    let env = |k: &str| std::env::var(k).ok();
    let resize = is_host
        && (args.iter().any(|a| a == "--resize")
            || resize::enabled(env(resize::ENV_RESIZE).as_deref()));
    let background = is_host && actions::enabled(env(actions::ENV_BACKGROUND).as_deref());
    let host = Rc::new(RefCell::new(Host {
        d: d.clone(),
        instance_id: instance_id(),
        own_mirror: matches!(mode, "" | "--host" | "--desktop"),
        own_apps: matches!(mode, "--host" | "--app"),
        answer_views: is_host,
        background,
        actions: actions::PendingActions::default(),
        resize,
        last_resize_poll: Instant::now(),
        view_resolution: None,
        exit_when_idle: !is_host,
        sessions: HashMap::new(),
        queue: VecDeque::new(),
        inflight: None,
        had_session: false,
        last_hb: Instant::now(),
        names: HashMap::new(),
        apps: load_apps(),
        x11: X11::connect(),
        main_loop: main_loop.clone(),
    }));
    {
        let mut h = host.borrow_mut();
        let apps = h.apps.clone();
        for a in apps.as_array().into_iter().flatten() {
            if let (Some(p), Some(n)) = (a["pkg"].as_str(), a["name"].as_str()) {
                h.names.insert(p.to_owned(), n.to_owned());
            }
        }
    }

    {
        let h = host.clone();
        glib::timeout_add_local(Duration::from_millis(15), move || {
            let mut h = h.borrow_mut();
            while let Ok(ev) = rx.try_recv() {
                match ev {
                    Ev::DaemonGone => {
                        h.close_all("daemon went away", false);
                        h.main_loop.quit();
                        return glib::ControlFlow::Break;
                    }
                    Ev::Phone(svc, data) => {
                        if let Err(e) = h.on_frame(svc, &data) {
                            eprintln!("handling svc {svc}: {e:#}");
                        }
                    }
                    Ev::Ctl(v, reply) => {
                        let r = h.on_ctl(&v);
                        let _ = reply.send(r.to_string());
                    }
                }
            }
            h.tick();
            glib::ControlFlow::Continue
        });
    }
    for sig in [2, 15] {
        let h = host.clone();
        glib::unix_signal_add_local(sig, move || {
            let mut h = h.borrow_mut();
            h.close_all("signal", true);
            h.main_loop.quit();
            glib::ControlFlow::Break
        });
    }
    println!("{BINDINGS}");

    match mode {
        "" => {
            println!("asking the phone to start mirroring");
            d.send(1, views::start(1));
        }
        "--desktop" => {
            println!("asking the phone to start Ready For desktop");
            d.send(1, views::start(2));
        }
        "--app" => {
            let cmp = args.get(2).map(String::as_str);
            let mut h = host.borrow_mut();
            let r = start_req_for(&args[1], cmp, &h.apps);
            h.request(r, false);
        }
        _ => println!(
            "host: waiting for streams (control socket {}); background notification actions {}, window resize {}",
            host_socket_path().display(),
            if background { "on" } else { "off" },
            if resize { "on" } else { "off" }
        ),
    }
    main_loop.run();
    if is_host {
        let _ = std::fs::remove_file(host_socket_path());
    }
    // Let the stop messages reach the daemon before the socket closes.
    std::thread::sleep(Duration::from_millis(200));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn h264_only_keeps_h264_and_drops_orientation() {
        let offer = "v=0\r\nm=video 9 UDP/TLS/RTP/SAVPF 96 98 102\r\na=mid:0\r\n\
                     a=extmap:3 urn:3gpp:video-orientation\r\n\
                     a=rtpmap:96 H265/90000\r\na=rtcp-fb:96 nack\r\n\
                     a=rtpmap:98 H264/90000\r\na=fmtp:98 profile-level-id=42e01f\r\n\
                     a=rtpmap:102 rtx/90000\r\na=fmtp:102 apt=96";
        let out = h264_only(offer);
        assert!(out.contains("m=video 9 UDP/TLS/RTP/SAVPF 98"));
        assert!(
            out.contains("a=rtpmap:98 H264/90000")
                && out.contains("a=fmtp:98 profile-level-id=42e01f")
        );
        assert!(
            !out.contains("H265") && !out.contains("rtx") && !out.contains("video-orientation")
        );
        assert!(out.contains("a=mid:0"));
    }

    /// A host whose "daemon" is one end of a socket pair; the other end sees what it sends.
    fn test_host(answer_views: bool) -> (Host, UnixStream) {
        let (ours, theirs) = UnixStream::pair().unwrap();
        theirs
            .set_read_timeout(Some(Duration::from_millis(100)))
            .unwrap();
        let host = Host {
            d: Arc::new(Daemon {
                cmd: Mutex::new(ours),
            }),
            instance_id: 7,
            own_mirror: true,
            own_apps: true,
            answer_views,
            background: true,
            actions: actions::PendingActions::default(),
            resize: false,
            last_resize_poll: Instant::now(),
            view_resolution: None,
            exit_when_idle: false,
            sessions: HashMap::new(),
            queue: VecDeque::new(),
            inflight: None,
            had_session: false,
            last_hb: Instant::now(),
            names: HashMap::new(),
            apps: json!([]),
            x11: None,
            main_loop: glib::MainLoop::new(None, false),
        };
        (host, theirs)
    }

    /// The `send` requests written so far, as (service, data).
    fn sent(theirs: &UnixStream) -> Vec<(i64, Value)> {
        let mut out = Vec::new();
        let mut r = BufReader::new(theirs);
        let mut line = String::new();
        while r.read_line(&mut line).is_ok_and(|n| n > 0) {
            let v: Value = serde_json::from_str(&line).unwrap();
            assert_eq!(v["cmd"], "send");
            out.push((v["service"].as_i64().unwrap(), v["data"].clone()));
            line.clear();
        }
        out
    }

    fn click(kind: &str, pkg: &str) -> Value {
        json!({ "event": "notification_action", "kind": kind, "pkg": pkg, "key": format!("0|{pkg}|1|null|10001"), "actionId": 1, "service": 5 })
    }

    fn invite(pkg: &str) -> Value {
        json!({ "ver": "1", "cmd": 2, "data": { "deviceId": "<phone-id>", "pkg": pkg, "pkgName": "Example", "source": 1, "userId": 0, "cmp": null } })
    }

    #[test]
    fn action_click_then_invite_runs_headless() {
        let (mut h, theirs) = test_host(true);
        h.on_frame(
            actions::SERVICE_LOCAL_EVENTS,
            &click("action", "com.example.clock"),
        )
        .unwrap();
        h.on_frame(15, &invite("com.example.clock")).unwrap();
        let (req, _) = h.inflight.as_ref().expect("cmd 1 in flight");
        assert!(req.headless);
        let out = sent(&theirs);
        assert_eq!(out.len(), 1);
        let (svc, data) = &out[0];
        assert_eq!((*svc, data["cmd"].as_i64()), (15, Some(1)));
        assert_eq!(data["data"]["pkg"], "com.example.clock");
        assert_eq!(data["data"]["source"], 1);
        assert_eq!(data["data"]["supportCustomDisplay"], false);
    }

    #[test]
    fn open_clicks_and_plain_invites_get_a_window() {
        let (mut h, _theirs) = test_host(true);
        h.on_frame(actions::SERVICE_LOCAL_EVENTS, &click("action", "com.a"))
            .unwrap();
        h.on_frame(actions::SERVICE_LOCAL_EVENTS, &click("open", "com.a"))
            .unwrap();
        h.on_frame(15, &invite("com.a")).unwrap();
        assert!(
            !h.inflight.as_ref().unwrap().0.headless,
            "Open wins over an earlier button click"
        );
        let (mut h, _theirs) = test_host(true);
        h.on_frame(actions::SERVICE_LOCAL_EVENTS, &click("action", "com.other"))
            .unwrap();
        h.on_frame(15, &invite("com.a")).unwrap();
        assert!(
            !h.inflight.as_ref().unwrap().0.headless,
            "a click for another app"
        );
        let (mut h, _theirs) = test_host(true);
        h.background = false;
        h.on_frame(actions::SERVICE_LOCAL_EVENTS, &click("action", "com.a"))
            .unwrap();
        h.on_frame(15, &invite("com.a")).unwrap();
        assert!(
            !h.inflight.as_ref().unwrap().0.headless,
            "SMARTCONNECT_BACKGROUND_ACTIONS=0"
        );
    }

    #[test]
    fn repeated_invites_send_one_start() {
        let (mut h, theirs) = test_host(true);
        h.on_frame(15, &invite("com.a")).unwrap();
        h.on_frame(15, &invite("com.a")).unwrap();
        assert_eq!(sent(&theirs).len(), 1);
        assert!(h.queue.is_empty());
    }

    #[test]
    fn phone_view_invite_is_accepted_and_sized() {
        let (mut h, theirs) = test_host(true);
        let svc1 = json!({ "ver": "1.0.0", "cmd": 0, "view": 2, "resolution": { "width": 2560, "height": 1440 } });
        h.on_frame(1, &svc1).unwrap();
        h.on_frame(15, &invite("desktop")).unwrap();
        let out = sent(&theirs);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].0, 1);
        assert_eq!(
            (
                out[0].1["cmd"].as_i64(),
                out[0].1["view"].as_i64(),
                out[0].1["reply"].as_i64()
            ),
            (Some(1), Some(2), Some(1))
        );
        let start = &out[1].1["data"];
        assert_eq!((out[1].0, start["pkg"].as_str()), (15, Some("desktop")));
        assert_eq!(
            (start["width"].as_i64(), start["height"].as_i64()),
            (Some(2560), Some(1440))
        );
        assert_eq!(h.title_for("desktop"), "Mobile desktop — Smart Connect");
    }

    #[test]
    fn only_the_host_answers_view_invites() {
        let (mut h, theirs) = test_host(false);
        h.on_frame(1, &json!({ "ver": "1.0.0", "cmd": 0, "view": 1 }))
            .unwrap();
        h.on_frame(1, &json!({ "ver": "1.0.0", "cmd": 31, "view": 7 }))
            .unwrap();
        assert!(sent(&theirs).is_empty());
    }

    #[test]
    fn start_request_defaults() {
        let apps = json!([{ "pkg": "com.example", "name": "Example", "cmp": "com.example.Main", "userId": 10 }]);
        let r = start_req_for("com.example", None, &apps);
        assert_eq!(
            (r.cmp.as_str(), r.user_id.as_i64(), r.source),
            (Some("com.example.Main"), Some(10), 0)
        );
        assert_eq!(r.name.as_deref(), Some("Example"));
        assert!(!r.headless, "PC-initiated starts always get a window");
    }

    #[test]
    fn test_rotation_to_direction_mapping() {
        assert_eq!(rotation_to_direction(0), "identity");
        assert_eq!(rotation_to_direction(1), "90l");
        assert_eq!(rotation_to_direction(2), "180");
        assert_eq!(rotation_to_direction(3), "90r");
        // Unknown or out-of-bounds rotation should default to identity
        assert_eq!(rotation_to_direction(4), "identity");
        assert_eq!(rotation_to_direction(99), "identity");
    }

    #[test]
    fn test_rotation_aspect_ratio_calculations() {
        // Portrait (e.g. 400x890) -> Landscape transition (target_landscape = true)
        // h clamped to [400, 600]: 890.min(600).max(400) = 600
        // w = 600 * (1336.0 / 600.0) = 1336
        let (w, h) = calculate_adapted_window_size(400, 890, true);
        assert_eq!((w, h), (1336, 600));
        assert!(w > h, "Landscape target width must exceed height");

        // Smaller portrait (e.g. 200x300) -> Landscape transition
        // h clamped to [400, 600]: 300.min(600).max(400) = 400
        // w = 400 * (1336.0 / 600.0) = 890
        let (w_small, h_small) = calculate_adapted_window_size(200, 300, true);
        assert_eq!((w_small, h_small), (890, 400));
        assert!(w_small > h_small);

        // Landscape (e.g. 1336x600) -> Portrait transition (target_landscape = false)
        // h clamped to [600, 960]: 600.max(1336).min(960).max(600) = 960
        // w = 960 * (600.0 / 1336.0) = 431
        let (w_port, h_port) = calculate_adapted_window_size(1336, 600, false);
        assert_eq!((w_port, h_port), (431, 960));
        assert!(h_port > w_port, "Portrait target height must exceed width");

        // Small landscape (e.g. 800x400) -> Portrait transition
        // h clamped to [600, 960]: 400.max(800).min(960).max(600) = 800
        // w = 800 * (600.0 / 1336.0) = 359
        let (w_port_small, h_port_small) = calculate_adapted_window_size(800, 400, false);
        assert_eq!((w_port_small, h_port_small), (359, 800));
        assert!(h_port_small > w_port_small);
    }

    #[test]
    fn test_last_adapted_rotation_prevents_oscillation() {
        let mut st = SessState::default();
        assert_eq!(st.last_adapted_rotation, None);

        // First transition to rotation 1 (landscape) triggers adaptation
        assert!(st.check_and_mark_adapted_rotation(1));
        assert_eq!(st.last_adapted_rotation, Some(1));

        // Duplicate rotation 1 notification (or resize echo) must NOT re-adapt
        assert!(!st.check_and_mark_adapted_rotation(1));
        assert_eq!(st.last_adapted_rotation, Some(1));

        // Rotation 2 (180 deg) triggers adaptation
        assert!(st.check_and_mark_adapted_rotation(2));
        assert_eq!(st.last_adapted_rotation, Some(2));
        assert!(!st.check_and_mark_adapted_rotation(2));

        // Rotation 3 (270 deg) triggers adaptation
        assert!(st.check_and_mark_adapted_rotation(3));
        assert_eq!(st.last_adapted_rotation, Some(3));
        assert!(!st.check_and_mark_adapted_rotation(3));

        // Transition back to 0 (portrait) triggers adaptation
        assert!(st.check_and_mark_adapted_rotation(0));
        assert_eq!(st.last_adapted_rotation, Some(0));

        // Immediate duplicate of 0 is suppressed
        assert!(!st.check_and_mark_adapted_rotation(0));
    }

    #[test]
    fn test_keyframe_request_generation() {
        let _ = gst::init();
        let ev = create_keyframe_request_event();
        assert_eq!(ev.type_(), gst::EventType::CustomUpstream);
        let s = ev
            .structure()
            .expect("ForceKeyUnit event must have structure");
        assert_eq!(s.name(), "GstForceKeyUnit");
        assert_eq!(s.get::<bool>("all-headers"), Ok(true));
    }

    #[test]
    fn test_session_request_keyframe_sends_upstream() {
        let _ = gst::init();
        let (ours, _theirs) = UnixStream::pair().unwrap();
        let pipeline = gst::Pipeline::with_name("test-keyframe");
        let src = gst::ElementFactory::make("fakesrc").build().unwrap();
        let sink = gst::ElementFactory::make("fakesink").build().unwrap();
        pipeline.add(&src).unwrap();
        pipeline.add(&sink).unwrap();
        src.link(&sink).unwrap();

        let received = Arc::new(Mutex::new(Vec::new()));
        let received_clone = received.clone();
        src.static_pad("src").unwrap().add_probe(
            gst::PadProbeType::EVENT_UPSTREAM,
            move |_pad, info| {
                if let Some(gst::PadProbeData::Event(ref ev)) = info.data
                    && let Some(s) = ev.structure()
                {
                    received_clone.lock().unwrap().push(s.name().to_string());
                }
                gst::PadProbeReturn::Ok
            },
        );

        pipeline.set_state(gst::State::Paused).unwrap();

        let s = Arc::new(Session {
            sid: 12345,
            pkg: "com.test".into(),
            user_id: json!(0),
            cmp: Value::Null,
            shortcut_id: Value::Null,
            headless: false,
            d: Arc::new(Daemon {
                cmd: Mutex::new(ours),
            }),
            st: Mutex::default(),
            pipeline: gst::Pipeline::with_name("test-kf-session"),
            webrtc: gst::ElementFactory::make("identity").build().unwrap(),
            title: Mutex::new("Test".into()),
            tag: Mutex::new(None),
            sink: Mutex::new(Some(sink)),
            flip: Mutex::new(None),
            xwin: Mutex::new(None),
            closed_by_user: Arc::new(AtomicBool::new(false)),
            close_hook_installed: AtomicBool::new(false),
        });

        s.request_keyframe();

        let captured = received.lock().unwrap().clone();
        assert!(
            captured.contains(&"GstForceKeyUnit".to_string()),
            "Upstream pad probe should have captured GstForceKeyUnit event, got: {:?}",
            captured
        );

        let _ = pipeline.set_state(gst::State::Null);
    }

    #[test]
    fn test_pick_decoder_returns_constructible_decoder() {
        let _ = gst::init();
        let dec = pick_decoder();
        assert!(!dec.is_empty());

        // Validate that the returned decoder element (or element bin) can actually be instantiated
        let sink = pick_sink();
        let desc = format!(
            "queue ! rtph264depay ! h264parse ! {} ! videoconvert name=conv n-threads=2 ! videoflip name=flip ! videoconvert n-threads=2 ! queue max-size-buffers=1 max-size-time=0 max-size-bytes=0 leaky=downstream ! taginject name=tag ! {} name=sink sync=false",
            dec, sink
        );
        let bin = gst::parse::bin_from_description(&desc, true);
        assert!(
            bin.is_ok(),
            "pick_decoder() produced '{}', which failed pipeline construction: {:?}",
            dec,
            bin.err()
        );
    }

    #[test]
    fn test_pick_decoder_fallback_behavior() {
        let _ = gst::init();
        let ok = |n: &str| gst::ElementFactory::make(n).build().is_ok();

        // 1. Explicit cpu requested
        let cpu_dec = pick_decoder_from(Some("cpu"));
        assert!(cpu_dec.starts_with("avdec_h264"));

        // 2. Default (None) selection
        let auto_dec = pick_decoder_from(None);
        if ok("vah264dec") {
            assert_eq!(auto_dec, "vah264dec");
        } else if ok("vaapih264dec") {
            assert_eq!(auto_dec, "vaapih264dec");
        } else if ok("nvh264dec") && ok("cudadownload") {
            assert_eq!(auto_dec, "nvh264dec ! cudadownload ! video/x-raw");
        } else {
            assert!(auto_dec.starts_with("avdec_h264"));
        }

        // 3. Fallback when nvdec requested but unavailable
        let nv_dec = pick_decoder_from(Some("nvdec"));
        if ok("nvh264dec") && ok("cudadownload") {
            assert_eq!(nv_dec, "nvh264dec ! cudadownload ! video/x-raw");
        } else {
            assert!(
                nv_dec.starts_with("avdec_h264"),
                "Unavailable nvdec should fall back to avdec_h264, got: {}",
                nv_dec
            );
        }

        // 4. Fallback when vaapi requested but unavailable
        let va_dec = pick_decoder_from(Some("vaapi"));
        if ok("vah264dec") {
            assert_eq!(va_dec, "vah264dec");
        } else if ok("vaapih264dec") {
            assert_eq!(va_dec, "vaapih264dec");
        } else {
            assert!(
                va_dec.starts_with("avdec_h264"),
                "Unavailable vaapi should fall back to avdec_h264, got: {}",
                va_dec
            );
        }
    }

    #[test]
    fn test_window_close_detection() {
        let (ours, _theirs) = UnixStream::pair().unwrap();
        let s = Arc::new(Session {
            sid: 99999,
            pkg: "com.close.test".into(),
            user_id: json!(0),
            cmp: Value::Null,
            shortcut_id: Value::Null,
            headless: false,
            d: Arc::new(Daemon {
                cmd: Mutex::new(ours),
            }),
            st: Mutex::default(),
            pipeline: gst::Pipeline::with_name("test-close"),
            webrtc: gst::ElementFactory::make("identity").build().unwrap(),
            title: Mutex::new("Test Close".into()),
            tag: Mutex::new(None),
            sink: Mutex::new(None),
            flip: Mutex::new(None),
            xwin: Mutex::new(None),
            closed_by_user: Arc::new(AtomicBool::new(false)),
            close_hook_installed: AtomicBool::new(false),
        });

        // Initially not dead
        assert!(s.poll_dead(None).is_none());

        // When closed_by_user is signaled (e.g. from glimagesink close callback or WM_DELETE_WINDOW):
        s.closed_by_user.store(true, Ordering::SeqCst);
        assert_eq!(s.poll_dead(None), Some("window closed by user".into()));
    }

    #[test]
    fn test_rotation_tracking() {
        let _ = gst::init();
        let (ours, _theirs) = UnixStream::pair().unwrap();
        let flip = gst::ElementFactory::make("videoflip").build().unwrap();
        let s = Arc::new(Session {
            sid: 12345,
            pkg: "com.test".into(),
            user_id: json!(0),
            cmp: Value::Null,
            shortcut_id: Value::Null,
            headless: false,
            d: Arc::new(Daemon {
                cmd: Mutex::new(ours),
            }),
            st: Mutex::default(),
            pipeline: gst::Pipeline::with_name("test"),
            webrtc: gst::ElementFactory::make("identity").build().unwrap(),
            title: Mutex::new("Test".into()),
            tag: Mutex::new(None),
            sink: Mutex::new(None),
            flip: Mutex::new(Some(flip)),
            xwin: Mutex::new(None),
            closed_by_user: Arc::new(AtomicBool::new(false)),
            close_hook_installed: AtomicBool::new(false),
        });
        assert_eq!(s.st.lock().unwrap().current_rotation, 0);
        assert_eq!(s.st.lock().unwrap().last_adapted_rotation, None);

        s.set_rotation(1, None);
        assert_eq!(s.st.lock().unwrap().current_rotation, 1);
        assert_eq!(rotation_to_direction(1), "90l");
        assert_eq!(rotation_to_direction(2), "180");
        assert_eq!(rotation_to_direction(3), "90r");

        s.set_rotation(0, None);
        assert_eq!(s.st.lock().unwrap().current_rotation, 0);
        assert_eq!(rotation_to_direction(0), "identity");
    }
}
