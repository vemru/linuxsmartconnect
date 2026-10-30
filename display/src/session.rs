//! Svc 202 ("R4SS") signaling over the daemon control socket + the webrtcbin sender.
//! Spec: evidence-2026-10-20/appstream-and-display-spec.md section B; notes: .notes/phone-as-display.md.
//!
//!   PC cmd 0 {shareType,setAudio,setScale,key_pc_ss_version,sessionId:S}
//!   phone cmd 5 {sdpOffer, resolution:"{width,height,widthReq,heightReq,fps,maxHeight}"}
//!   PC cmd 1 {answer:"{type,sdp}",messageType:"answer"}; PC cmd 2 {candidate:"{id,label,candidate}"}
//!   phone cmd 2 {label,id,candidate} (flat); data channel "touchDataChannel" → input packets
//!   phone cmd 7 rotation, 54 audio switch, 70 resize, 16/72 restart, 52 stop; PC cmd 26 stop, 27 audio state.
//!
//! The phone is the WebRTC offerer (recvonly audio+video + SCTP); this process answers and SENDS
//! H.264 (and, with `--audio`, Opus).

use std::io::{BufRead, BufReader, Read, Seek, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use gst::glib;
use gst::prelude::*;
use serde_json::{Value, json};

use crate::desktop::{self, Desktop};
use crate::opts::{self, Opts, Share, SizePref};
use crate::{audio, capture, ice, input};

const SERVICE: i64 = 202;
const PC_SS_VERSION: &str = "pc_ss_ver_2Dot5";
/// Rotation is applied this long after the phone's last cmd 7 (it often sends two in a row).
const ROTATION_SETTLE: Duration = Duration::from_millis(350);
/// Portrait streams are at most this tall (`$SMARTCONNECT_DISPLAY_PORTRAIT_MAX`, 0 = no limit).
/// The phone only checks its decoder for landscape sizes (up to 2560x1440); 1080x1920 is the
/// common portrait size every H.264 decoder takes.
const PORTRAIT_MAX: u32 = 1920;

// ---------------------------------------------------------------------------------------------
// Daemon control socket.

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

    /// Sends one svc 202 message: `{ver:"1",cmd,data,sessionId}` (the daemon adds the envelope).
    fn send202(&self, cmd: i64, sid: i32, data: Value) {
        let msg = json!({ "ver": "1", "cmd": cmd, "data": data, "sessionId": sid });
        if cmd != 2 {
            println!("→ phone cmd {cmd}: {}", truncate(&msg.to_string(), 300));
        }
        let line = json!({ "cmd": "send", "service": SERVICE, "data": msg }).to_string() + "\n";
        if let Err(e) = self.cmd.lock().unwrap().write_all(line.as_bytes()) {
            eprintln!("daemon write failed: {e}");
        }
    }
}

/// This PC's address as the phone reached it (the daemon status' `pcAddress`), if known.
fn pc_address() -> Option<String> {
    let s = UnixStream::connect(socket_path()).ok()?;
    s.set_read_timeout(Some(Duration::from_secs(1))).ok()?;
    (&s).write_all(b"status\n").ok()?;
    let mut line = String::new();
    BufReader::new(&s).read_line(&mut line).ok()?;
    let v: Value = serde_json::from_str(&line).ok()?;
    v.get("pcAddress")
        .and_then(Value::as_str)
        .map(str::to_owned)
}

enum Ev {
    Phone(Value),
    DaemonGone,
}

fn watch(tx: mpsc::Sender<Ev>) -> Result<()> {
    let watch = UnixStream::connect(socket_path()).context("is smartconnectd running?")?;
    (&watch)
        .write_all(format!("{}\n", json!({ "cmd": "watch", "services": [SERVICE] })).as_bytes())?;
    std::thread::spawn(move || {
        for line in BufReader::new(watch).lines().map_while(Result::ok) {
            if let Ok(v) = serde_json::from_str::<Value>(&line)
                && v.get("service").and_then(Value::as_i64) == Some(SERVICE)
            {
                let _ = tx.send(Ev::Phone(v["data"].clone()));
            }
        }
        eprintln!("daemon closed the watch stream");
        let _ = tx.send(Ev::DaemonGone);
    });
    Ok(())
}

/// The phone sends the inner `data` as a JSON string (sometimes a bare string like "iceFailure").
fn inner(msg: &Value) -> Value {
    match msg.get("data") {
        Some(Value::String(s)) => {
            serde_json::from_str(s).unwrap_or_else(|_| Value::String(s.clone()))
        }
        Some(v) => v.clone(),
        None => Value::Null,
    }
}

/// A field that may itself be a JSON string (`resolution` in cmd 5).
fn json_field(v: &Value, k: &str) -> Value {
    match v.get(k) {
        Some(Value::String(s)) => serde_json::from_str(s).unwrap_or(Value::Null),
        Some(v) => v.clone(),
        None => Value::Null,
    }
}

fn truncate(s: &str, n: usize) -> &str {
    match s.char_indices().nth(n) {
        Some((i, _)) => &s[..i],
        None => s,
    }
}

fn even((w, h): (u32, u32)) -> (u32, u32) {
    (w.max(2) & !1, h.max(2) & !1)
}

// ---------------------------------------------------------------------------------------------
// One instance at a time: every svc 202 frame reaches every watcher, and two senders answering
// the same offer break ICE (seen on the device). smartconnectd checks this lock before it starts
// one for the phone (cmd 50).

fn lock_path() -> PathBuf {
    runtime_dir().join("smartconnect-display.lock")
}

/// Takes `$XDG_RUNTIME_DIR/smartconnect-display.lock` (an exclusive flock, our pid inside) for the
/// life of the process. With `replace`, a running instance is asked to stop (SIGTERM: it tells the
/// phone and switches its output off) and we wait for it; otherwise we refuse to start.
fn single_instance(replace: bool) -> Result<std::fs::File> {
    single_instance_at(&lock_path(), replace)
}

fn single_instance_at(path: &std::path::Path, replace: bool) -> Result<std::fs::File> {
    let mut f = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .with_context(|| format!("opening {}", path.display()))?;
    // SAFETY: flock on an fd we own.
    let try_lock = |f: &std::fs::File| unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0;
    if !try_lock(&f) {
        let mut text = String::new();
        let _ = f.read_to_string(&mut text);
        let pid: Option<i32> = text.trim().parse().ok();
        let who = pid.map_or("unknown pid".to_owned(), |p| format!("pid {p}"));
        if !replace {
            bail!(
                "smartconnect-display is already running ({who}); stop it first or pass --replace"
            );
        }
        if let Some(p) = pid.filter(|p| *p > 0) {
            println!("stopping the running smartconnect-display ({who})");
            // SAFETY: kill(2) with a pid read from our own lock file.
            unsafe { libc::kill(p, libc::SIGTERM) };
        }
        let deadline = Instant::now() + Duration::from_secs(4);
        while !try_lock(&f) {
            if Instant::now() >= deadline {
                bail!("the running smartconnect-display ({who}) did not stop");
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }
    f.set_len(0)?;
    f.rewind()?;
    writeln!(f, "{}", std::process::id())?;
    Ok(f)
}

// ---------------------------------------------------------------------------------------------
// Orientation (cmd 7).

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Orient {
    Landscape,
    Portrait,
}

impl Orient {
    /// cmd 7 `{"setOrientation":n}`: the phone sends 1 when its screen turns landscape and 2 when
    /// it turns portrait (ScreenShareActivity.onConfigurationChanged), at the start of a session
    /// and on every rotation.
    fn from_phone(n: i64) -> Option<Orient> {
        match n {
            1 => Some(Orient::Landscape),
            2 => Some(Orient::Portrait),
            _ => None,
        }
    }

    /// `size` turned to this orientation; a portrait size is scaled down to at most `cap` px tall
    /// (0 = no limit), keeping its aspect ratio.
    fn apply(self, (w, h): (u32, u32), cap: u32) -> (u32, u32) {
        let (long, short) = (w.max(h), w.min(h));
        match self {
            Orient::Landscape => (long, short),
            Orient::Portrait if cap > 0 && long > cap => {
                let narrow =
                    (u64::from(short) * u64::from(cap) + u64::from(long) / 2) / u64::from(long);
                (narrow as u32, cap)
            }
            Orient::Portrait => (short, long),
        }
    }
}

fn portrait_max() -> u32 {
    std::env::var("SMARTCONNECT_DISPLAY_PORTRAIT_MAX")
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(PORTRAIT_MAX)
}

/// The stream size for `base` (the landscape size from the offer, cmd 70 or `--size`): turned to
/// the phone's orientation when following it, else `base` as it is.
fn oriented_size(base: (u32, u32), follow: bool, orient: Option<Orient>, cap: u32) -> (u32, u32) {
    if !follow {
        return even(base);
    }
    even(orient.unwrap_or(Orient::Landscape).apply(base, cap))
}

// ---------------------------------------------------------------------------------------------
// Video source (capture or test pattern) and input injection (data-channel packets).

struct Source {
    /// gst-launch fragment producing raw video.
    desc: String,
    /// The size it was opened for (extend output mode / test pattern size).
    size: (u32, u32),
    /// Captured screen and how phone input reaches it (None for the test pattern).
    target: Option<input::Target>,
    /// Keeps the cast session (and, for extend, the enabled output) alive.
    cap: Option<capture::Capture>,
}

impl Source {
    /// Extend: moves the screen to the side the placement file now names (the user dragged the
    /// phone on the Cross Control page).
    fn reposition(&mut self) -> Result<bool> {
        let Some(cap) = self.cap.as_mut() else {
            return Ok(false);
        };
        if !cap.reposition()? {
            return Ok(false);
        }
        println!(
            "{} moved to {:?} (the phone's place changed)",
            cap.output_name(),
            cap.geometry()
        );
        self.target = Some(cap.input_target());
        Ok(true)
    }

    /// Extend: resizes/turns the output in place (it stays on, so its windows stay on it).
    /// `Ok(false)` = not an extend capture (open a new source instead).
    fn reconfigure(&mut self, size: (u32, u32)) -> Result<bool> {
        let Some(cap) = self.cap.as_mut().filter(|c| c.is_extend()) else {
            return Ok(false);
        };
        cap.reconfigure_extend(size.0, size.1)?;
        println!(
            "{} is now {}x{}, geometry {:?}",
            cap.output_name(),
            cap.width,
            cap.height,
            cap.geometry()
        );
        self.desc = cap.source_description();
        self.size = size;
        self.target = Some(cap.input_target());
        Ok(true)
    }
}

fn open_source(o: &Opts, size: (u32, u32)) -> Result<Source> {
    let (w, h) = size;
    if let Some(p) = &o.test_pattern {
        return Ok(Source {
            desc: format!(
                "videotestsrc is-live=true pattern={p} ! video/x-raw,width={w},height={h},framerate=30/1"
            ),
            size,
            target: None,
            cap: None,
        });
    }
    let mode = match o.mode {
        Share::Mirror => capture::Mode::Mirror,
        Share::Extend => capture::Mode::Extend {
            width: w,
            height: h,
            output: o.output.clone(),
            scale: Some(o.display_scale),
        },
    };
    let cap = capture::start(mode)?;
    println!(
        "capturing {} on {} ({}x{}), geometry {:?}",
        cap.output_name(),
        cap.desktop().name(),
        cap.width,
        cap.height,
        cap.geometry()
    );
    Ok(Source {
        desc: cap.source_description(),
        size,
        target: Some(cap.input_target()),
        cap: Some(cap),
    })
}

/// Shared with the data-channel callback (a GStreamer thread).
#[derive(Default)]
struct InputState {
    inj: Option<input::Injector>,
    output: String,
    packets: u64,
}

impl InputState {
    fn handle(&mut self, b: &[u8]) {
        self.packets += 1;
        if self.packets <= 3 || self.packets.is_multiple_of(500) {
            let ty = b
                .get(..4)
                .map(|t| u32::from_le_bytes([t[0], t[1], t[2], t[3]]))
                .unwrap_or(u32::MAX);
            let head: String = b.iter().take(32).map(|x| format!("{x:02x}")).collect();
            let what = if self.inj.is_some() {
                "injected"
            } else {
                "not injected"
            };
            println!(
                "input packet #{}: type {ty}, {} bytes, {head}… ({what})",
                self.packets,
                b.len()
            );
        }
        if let Some(inj) = self.inj.as_mut() {
            inj.handle_packet(b);
        }
    }

    /// Points injection at the captured screen (no-op for the test pattern). `fresh`: the
    /// output was (re)created, so a pointer pinned to its old wl_output must be replaced.
    fn retarget(&mut self, target: &Option<input::Target>, fresh: bool) {
        let Some(t) = target else { return };
        let (rect, output) = (t.rect, &t.output);
        if let Some(inj) = self
            .inj
            .as_mut()
            .filter(|_| !fresh && self.output == *output)
        {
            inj.set_target(t);
            println!("phone input now maps to {output} {rect:?}");
            return;
        }
        self.inj = None; // releases anything still held before the new pointer exists
        match input::Injector::open(t) {
            Ok(inj) => {
                println!("injecting phone input into {output} {rect:?}");
                self.inj = Some(inj);
                self.output = output.clone();
            }
            Err(e) => eprintln!("input injection unavailable ({e:#}); phone touches are ignored"),
        }
    }
}

type SharedInput = Arc<Mutex<InputState>>;

// ---------------------------------------------------------------------------------------------
// SDP helpers.

/// Picks the H.264 payload type to send: packetization-mode=1 + constrained baseline (42e0../42c0..),
/// else any packetization-mode=1, else the first H.264. Returns (pt, profile-level-id).
fn pick_h264(sdp: &str) -> Option<(u32, String)> {
    let mut cands: Vec<(u32, String, bool)> = Vec::new();
    for l in sdp.lines() {
        if let Some(r) = l.strip_prefix("a=rtpmap:")
            && r.contains(" H264/90000")
            && let Some(pt) = r.split(' ').next().and_then(|p| p.parse().ok())
        {
            cands.push((pt, String::new(), false));
        }
    }
    for l in sdp.lines() {
        if let Some(r) = l.strip_prefix("a=fmtp:") {
            let (pt, params) = r.split_once(' ').unwrap_or((r, ""));
            if let Some(c) = cands.iter_mut().find(|c| c.0.to_string() == pt) {
                c.2 = params.contains("packetization-mode=1");
                c.1 = params
                    .split(';')
                    .find_map(|p| p.trim().strip_prefix("profile-level-id="))
                    .unwrap_or("")
                    .to_owned();
            }
        }
    }
    let cb = |c: &&(u32, String, bool)| c.2 && (c.1.starts_with("42e0") || c.1.starts_with("42c0"));
    cands
        .iter()
        .find(cb)
        .or_else(|| cands.iter().find(|c| c.2))
        .or_else(|| cands.first())
        .map(|c| (c.0, c.1.clone()))
}

/// Keeps only payload type `keep` in the video m-line (the phone offers VP8/VP9/H265/AV1 + RTX too).
fn video_only_pt(sdp: &str, keep: u32) -> String {
    let lines: Vec<&str> = sdp.split("\r\n").flat_map(|l| l.split('\n')).collect();
    let pt_of = |l: &str| -> Option<String> {
        ["a=rtpmap:", "a=fmtp:", "a=rtcp-fb:"]
            .iter()
            .find_map(|p| l.strip_prefix(p))
            .and_then(|r| r.split(' ').next())
            .map(str::to_owned)
    };
    let keep_s = keep.to_string();
    let mut out = Vec::new();
    let mut in_video = false;
    for l in lines {
        if l.starts_with("m=") {
            in_video = l.starts_with("m=video");
            if in_video {
                let parts: Vec<&str> = l.split(' ').collect();
                out.push(format!("{} {}", parts[..3].join(" "), keep_s));
                continue;
            }
        } else if in_video {
            if l.starts_with("a=extmap:") && l.contains("video-orientation") {
                continue;
            }
            if pt_of(l).is_some_and(|pt| pt != keep_s) {
                continue;
            }
        }
        out.push(l.to_owned());
    }
    out.join("\r\n")
}

/// Direction attribute (`a=sendonly`…) of the first `m=<kind>` section, or "-" without one.
fn direction(sdp: &str, kind: &str) -> String {
    let mut lines = sdp
        .lines()
        .map(str::trim)
        .skip_while(|l| !l.starts_with(&format!("m={kind}")));
    if lines.next().is_none() {
        return "-".into();
    }
    lines
        .take_while(|l| !l.starts_with("m="))
        .find(|l| ["a=sendonly", "a=sendrecv", "a=recvonly", "a=inactive"].contains(l))
        .unwrap_or("?")
        .to_owned()
}

/// Feeds every SCTP message that webrtcbin's `sctpdec` receives (one src pad per stream) to the
/// input handler, skipping the DCEP DATA_CHANNEL_OPEN (type 3, carries the label).
fn tap_sctp(webrtc: &gst::Element, input: SharedInput, tapped: Arc<AtomicBool>) {
    let Some(bin) = webrtc.downcast_ref::<gst::Bin>() else {
        return;
    };
    bin.connect_element_added(move |_, el| {
        if el.factory().is_none_or(|f| f.name() != "sctpdec") {
            return;
        }
        let (input, tapped) = (input.clone(), tapped.clone());
        el.connect_pad_added(move |_, pad| {
            if pad.direction() != gst::PadDirection::Src {
                return;
            }
            tapped.store(true, Ordering::SeqCst);
            let input = input.clone();
            pad.add_probe(gst::PadProbeType::BUFFER, move |_, info| {
                if let Some(map) = info.buffer().and_then(|b| b.map_readable().ok()) {
                    let b = map.as_slice();
                    let dcep_open =
                        b.first() == Some(&3) && b.windows(16).any(|w| w == b"touchDataChannel");
                    if !dcep_open {
                        input.lock().unwrap().handle(b);
                    }
                }
                gst::PadProbeReturn::Ok
            });
        });
    });
}

/// Keeps the last offer/answer in $XDG_RUNTIME_DIR for debugging.
fn dump(what: &str, sdp: &str) {
    let _ = std::fs::write(
        socket_path().with_file_name(format!("smartconnect-display.{what}.sdp")),
        sdp,
    );
}

// ---------------------------------------------------------------------------------------------
// One WebRTC session (one cmd 5 offer): [source bin] ! queue ! scale ! H.264 ! RTP ! webrtcbin,
// plus, with audio, [PC audio] ! Opus ! RTP ! webrtcbin.

#[derive(Default)]
struct SigState {
    answered: bool,
    pending_local: Vec<(u32, String)>,
    /// m-line index → mid, from the offer.
    mids: Vec<String>,
    /// Candidates offered to the phone so far.
    offered: usize,
    /// UDP candidates the filter held back (offered only if none matched it).
    held: Vec<(u32, String)>,
}

impl SigState {
    fn offer(&mut self, d: &Daemon, sid: i32, mline: u32, cand: String) {
        self.offered += 1;
        if self.answered {
            send_candidate(d, sid, &self.mids, mline, &cand);
        } else {
            self.pending_local.push((mline, cand));
        }
    }
}

/// What one `Stream` sends.
struct StreamCfg<'a> {
    opts: &'a Opts,
    /// Video source fragment.
    src_desc: &'a str,
    size: (u32, u32),
    /// Audio source fragment and whether it starts unmuted; `None` answers the phone's audio
    /// section without a track (inactive), as before audio existed.
    audio: Option<(String, bool)>,
    ice: ice::Filter,
}

struct Stream {
    pipeline: gst::Pipeline,
    webrtc: gst::Element,
    /// First element after the (replaceable) source bin.
    head: gst::Element,
    srcbin: Option<gst::Element>,
    size: gst::Element,
    /// Frames are converted and scaled on the GPU (`size` caps are VA memory).
    va: bool,
    /// The audio branch's mute switch (`None`: no audio is sent).
    audio: Option<gst::Element>,
}

fn pick_encoder(o: &Opts) -> String {
    let kbps = o.bitrate;
    // key-int-max=30: a keyframe at least every 0.5–1 s (30–60 fps), so the phone recovers quickly.
    if !o.force_x264 && gst::ElementFactory::find("vah264enc").is_some() {
        format!(
            "vah264enc key-int-max=30 b-frames=0 rate-control=cbr bitrate={kbps} target-usage=7"
        )
    } else {
        format!("x264enc tune=zerolatency speed-preset=ultrafast key-int-max=30 bitrate={kbps}")
    }
}

/// The encoder's input: NV12 at the stream size, in VA (GPU) memory when `va`.
fn size_caps(w: u32, h: u32, va: bool) -> gst::Caps {
    let caps = gst::Caps::builder("video/x-raw")
        .field("format", "NV12")
        .field("width", w as i32)
        .field("height", h as i32)
        .field("pixel-aspect-ratio", gst::Fraction::new(1, 1));
    if va {
        caps.features(["memory:VAMemory"]).build()
    } else {
        caps.build()
    }
}

/// Converts and scales the captured frames for the encoder. With the VA encoder, `vapostproc` does
/// it on the GPU (~0.2 ms a frame at 2712x1220) and hands the frames over in GPU memory; on the CPU,
/// `videoconvert ! videoscale` took ~10 ms single-threaded, which with ~10 ms of encoding in the
/// same thread was more than a 60 fps frame allows, so frames queued up (the visible delay).
fn convert_description(enc: &str) -> (String, bool) {
    if enc.starts_with("vah264enc") && gst::ElementFactory::find("vapostproc").is_some() {
        ("vapostproc add-borders=true".to_owned(), true)
    } else {
        (
            "videoconvert n-threads=0 ! videoscale add-borders=true n-threads=0".to_owned(),
            false,
        )
    }
}

/// Requests a webrtcbin sink pad WITH caps and makes its transceiver send-only with those caps as
/// codec preferences, so it is matched to the offer's (recvonly) m-line of that kind. A bare
/// `! webrtcbin` races: once the stream's real caps (profile-level-id from the SPS) reach the pad
/// before set-remote-description, they no longer intersect the offer, webrtcbin makes a fresh
/// recvonly transceiver and answers `a=inactive` (seen live).
fn sendonly_pad(webrtc: &gst::Element, caps: &gst::Caps) -> Result<gst::Pad> {
    let templ = webrtc
        .pad_template("sink_%u")
        .context("no sink_%u template")?;
    let pad = webrtc
        .request_pad(&templ, None, Some(caps))
        .context("webrtcbin refused a sink pad")?;
    let trans = pad.property::<gst_webrtc::WebRTCRTPTransceiver>("transceiver");
    trans.set_property(
        "direction",
        gst_webrtc::WebRTCRTPTransceiverDirection::Sendonly,
    );
    trans.set_property("codec-preferences", caps);
    Ok(pad)
}

impl Stream {
    fn start(
        d: &Arc<Daemon>,
        sid: i32,
        cfg: StreamCfg,
        input: SharedInput,
        offer_sdp: &str,
    ) -> Result<Stream> {
        let (w, h) = cfg.size;
        let (pt, plid) = pick_h264(offer_sdp).context("the phone's offer has no H.264")?;
        let profile = if plid.starts_with("42") {
            "constrained-baseline"
        } else {
            "main"
        };
        let enc = pick_encoder(cfg.opts);
        println!(
            "sending H.264 pt {pt} (offer profile-level-id {plid}, encoding {profile}) at {w}x{h}, at most {} fps, {} kbit/s with {}",
            cfg.opts.fps,
            cfg.opts.bitrate,
            enc.split(' ').next().unwrap_or("")
        );
        let offer_sdp = video_only_pt(offer_sdp, pt);
        let mids: Vec<String> = offer_sdp
            .lines()
            .filter_map(|l| l.strip_prefix("a=mid:"))
            .map(|m| m.trim().to_owned())
            .collect();

        let rtp_caps = format!(
            "application/x-rtp,media=video,encoding-name=H264,clock-rate=90000,payload={pt}"
        );
        // drop-only: frames above the cap are dropped as they come, none are held back or added.
        let fps = cfg.opts.fps;
        let (convert, va) = convert_description(&enc);
        let feature = if va { "(memory:VAMemory)" } else { "" };
        // The one-frame leaky queue lets the encoder work in its own thread while the next frame is
        // converted, and drops a stale frame rather than letting a backlog build up.
        let desc = format!(
            "queue name=head max-size-buffers=2 leaky=downstream ! videorate drop-only=true max-rate={fps} \
             ! {convert} \
             ! capsfilter name=size caps=\"video/x-raw{feature},format=NV12,width={w},height={h},pixel-aspect-ratio=1/1\" \
             ! queue max-size-buffers=1 max-size-bytes=0 max-size-time=0 leaky=downstream \
             ! {enc} ! video/x-h264,profile={profile} ! h264parse config-interval=-1 \
             ! rtph264pay config-interval=-1 aggregate-mode=zero-latency pt={pt} \
             ! capsfilter name=rtpcaps caps={rtp_caps}"
        );
        let pipeline = gst::parse::launch(&desc)?
            .downcast::<gst::Pipeline>()
            .map_err(|_| anyhow!("not a pipeline"))?;
        let head = pipeline.by_name("head").context("no head queue")?;
        let size = pipeline.by_name("size").context("no size filter")?;

        // The pipeline also stays in READY until the answer is set (see sendonly_pad).
        let webrtc = gst::ElementFactory::make("webrtcbin")
            .name("webrtc")
            .property_from_str("bundle-policy", "max-bundle")
            .build()?;
        pipeline.add(&webrtc)?;
        let caps = gst::Caps::from_str(&rtp_caps).map_err(|_| anyhow!("bad rtp caps"))?;
        let sinkpad = sendonly_pad(&webrtc, &caps)?;
        pipeline
            .by_name("rtpcaps")
            .context("no rtpcaps")?
            .static_pad("src")
            .context("no rtpcaps src")?
            .link(&sinkpad)?;

        // PC audio on the offer's (recvonly) Opus section.
        let mut audio_vol = None;
        if let Some((src, on)) = &cfg.audio {
            match audio::pick_opus(&offer_sdp) {
                None => eprintln!("the offer has no Opus audio section; sending video only"),
                Some(apt) => {
                    let bin = gst::parse::bin_from_description(
                        &audio::branch_description(src, apt, *on),
                        true,
                    )
                    .context("building the audio branch")?;
                    pipeline.add(&bin)?;
                    let acaps = gst::Caps::from_str(&audio::rtp_caps(apt))
                        .map_err(|_| anyhow!("bad audio rtp caps"))?;
                    let apad = sendonly_pad(&webrtc, &acaps)?;
                    bin.static_pad("src")
                        .context("audio branch has no src pad")?
                        .link(&apad)?;
                    audio_vol = Some(bin.by_name("audiovol").context("no audiovol")?);
                    println!(
                        "sending audio: {src} → Opus pt {apt} ({})",
                        if *on { "on" } else { "muted" }
                    );
                }
            }
        }

        let sig = Arc::new(Mutex::new(SigState {
            mids,
            ..Default::default()
        }));
        let filter = Arc::new(cfg.ice);
        println!("offering ICE candidates on {}", filter.why);
        {
            let (d, sig, filter) = (d.clone(), sig.clone(), filter.clone());
            webrtc.connect("on-ice-candidate", false, move |args| {
                let (mline, cand) = (args[1].get::<u32>().ok()?, args[2].get::<String>().ok()?);
                let mut st = sig.lock().unwrap();
                if filter.allows(&cand) {
                    st.offer(&d, sid, mline, cand);
                } else if ice::candidate_addr(&cand)
                    .is_some_and(|(p, _)| p.eq_ignore_ascii_case("udp"))
                {
                    st.held.push((mline, cand));
                }
                None
            });
        }
        {
            // Safety net: if no candidate matched (e.g. the daemon's address is on an interface the
            // ICE agent skipped), offer the private-address ones rather than none at all.
            let (d, sig, filter) = (d.clone(), sig.clone(), filter.clone());
            webrtc.connect_notify(Some("ice-gathering-state"), move |wb, _| {
                let s: gst_webrtc::WebRTCICEGatheringState = wb.property("ice-gathering-state");
                if s != gst_webrtc::WebRTCICEGatheringState::Complete {
                    return;
                }
                let mut st = sig.lock().unwrap();
                if st.offered > 0 || st.held.is_empty() {
                    return;
                }
                let fallback = ice::Filter::private_ranges();
                let held: Vec<(u32, String)> = std::mem::take(&mut st.held)
                    .into_iter()
                    .filter(|(_, c)| fallback.allows(c))
                    .collect();
                eprintln!(
                    "no ICE candidate matched {}; offering {} private-address candidate(s) instead",
                    filter.why,
                    held.len()
                );
                for (m, c) in held {
                    st.offer(&d, sid, m, c);
                }
            });
        }
        webrtc.connect_notify(Some("ice-connection-state"), |wb, _| {
            let s: gst_webrtc::WebRTCICEConnectionState = wb.property("ice-connection-state");
            println!("ICE: {s:?}");
        });
        webrtc.connect_notify(Some("connection-state"), |wb, _| {
            let s: gst_webrtc::WebRTCPeerConnectionState = wb.property("connection-state");
            println!("peer connection: {s:?}");
        });
        // The phone sends its binary input packets as data-channel STRING messages (SCTP PPID 51):
        // webrtcbin then emits on-message-string with the payload cut at the first NUL (a touch
        // packet starts with u32 0, so it arrives as ""; seen live). Read the raw SCTP payloads
        // from sctpdec's source pads instead, whatever their PPID.
        let tapped = Arc::new(AtomicBool::new(false));
        tap_sctp(&webrtc, input.clone(), tapped.clone());
        webrtc.connect("on-data-channel", false, move |args| {
            let ch = args[1].get::<gst_webrtc::WebRTCDataChannel>().ok()?;
            let label: Option<String> = ch.property("label");
            println!(
                "data channel opened by the phone: {label:?} (raw SCTP tap: {})",
                tapped.load(Ordering::SeqCst)
            );
            let (input, tapped) = (input.clone(), tapped.clone());
            ch.connect_on_message_data(move |_, bytes| {
                if let Some(b) = bytes.filter(|_| !tapped.load(Ordering::SeqCst)) {
                    input.lock().unwrap().handle(b);
                }
            });
            None
        });

        let bus = pipeline.bus().context("no bus")?;
        std::thread::spawn(move || {
            for msg in bus.iter_timed(gst::ClockTime::NONE) {
                match msg.view() {
                    gst::MessageView::Error(e) => {
                        eprintln!(
                            "pipeline error from {:?}: {} ({:?})",
                            e.src().map(|s| s.path_string()),
                            e.error(),
                            e.debug()
                        )
                    }
                    gst::MessageView::Warning(e) => eprintln!(
                        "pipeline warning from {:?}: {}",
                        e.src().map(|s| s.path_string()),
                        e.error()
                    ),
                    gst::MessageView::Eos(_) => break,
                    _ => {}
                }
            }
        });

        let mut st = Stream {
            pipeline,
            webrtc,
            head,
            srcbin: None,
            size,
            va,
            audio: audio_vol,
        };
        st.attach_source(cfg.src_desc)?;
        let (pipeline, webrtc) = (st.pipeline.clone(), st.webrtc.clone());
        pipeline.set_state(gst::State::Ready)?;

        let sdp = gst_sdp::SDPMessage::parse_buffer(offer_sdp.as_bytes())
            .map_err(|_| anyhow!("bad offer SDP"))?;
        let offer =
            gst_webrtc::WebRTCSessionDescription::new(gst_webrtc::WebRTCSDPType::Offer, sdp);
        let p = gst::Promise::new();
        webrtc.emit_by_name::<()>("set-remote-description", &[&offer, &p]);
        p.wait();
        if let Some(e) = p
            .get_reply()
            .and_then(|r| r.value("error").ok().map(|e| format!("{e:?}")))
        {
            bail!("set-remote-description failed: {e}");
        }
        let p = gst::Promise::new();
        webrtc.emit_by_name::<()>("create-answer", &[&None::<gst::Structure>, &p]);
        p.wait();
        let reply = p.get_reply().context("no answer")?;
        if let Ok(err) = reply.value("error") {
            bail!("create-answer failed: {err:?}");
        }
        let answer = reply
            .value("answer")?
            .get::<gst_webrtc::WebRTCSessionDescription>()?;
        let text = answer.sdp().as_text()?;
        dump("offer", &offer_sdp);
        dump("answer", &text);
        let p = gst::Promise::new();
        webrtc.emit_by_name::<()>("set-local-description", &[&answer, &p]);
        p.wait();
        let (vdir, adir) = (direction(&text, "video"), direction(&text, "audio"));
        println!(
            "answer: {} bytes, video {vdir}, audio {adir}, data channel {}",
            text.len(),
            text.contains("m=application")
        );
        if vdir != "a=sendonly" && vdir != "a=sendrecv" {
            eprintln!("warning: the answer does not send video ({vdir})");
        }
        if st.audio.is_some() && adir != "a=sendonly" && adir != "a=sendrecv" {
            eprintln!("warning: the answer does not send audio ({adir})");
        }
        d.send202(
            1,
            sid,
            json!({
                "answer": json!({ "type": "answer", "sdp": text }).to_string(),
                "messageType": "answer",
            }),
        );
        let (pending, mids) = {
            let mut s = sig.lock().unwrap();
            s.answered = true;
            (std::mem::take(&mut s.pending_local), s.mids.clone())
        };
        for (m, c) in pending {
            send_candidate(d, sid, &mids, m, &c);
        }
        pipeline.set_state(gst::State::Playing)?;
        Ok(st)
    }

    /// Adds a source bin (gst-launch fragment) in front of the encoder chain.
    fn attach_source(&mut self, desc: &str) -> Result<()> {
        // `identity` closes the fragment: a trailing caps filter ("… ! video/x-raw,…") is only
        // valid between two elements.
        let bin = gst::parse::bin_from_description(&format!("{desc} ! identity"), true)?
            .upcast::<gst::Element>();
        self.pipeline.add(&bin)?;
        bin.link(&self.head)?;
        bin.sync_state_with_parent()?;
        self.srcbin = Some(bin);
        Ok(())
    }

    /// The current source bin (built from the source description).
    fn source_bin(&self) -> Option<&gst::Element> {
        self.srcbin.as_ref()
    }

    /// Stops and removes the source bin (before its capture session is closed).
    fn detach_source(&mut self) {
        if let Some(bin) = self.srcbin.take() {
            let _ = bin.set_state(gst::State::Null);
            bin.unlink(&self.head);
            let _ = self.pipeline.remove(&bin);
        }
    }

    fn add_remote_candidate(&self, c: &Value) {
        if let Some(cand) = c.get("candidate").and_then(Value::as_str) {
            let label = c.get("label").and_then(Value::as_u64).unwrap_or(0) as u32;
            println!("phone candidate: {}", truncate(cand, 90));
            self.webrtc
                .emit_by_name::<()>("add-ice-candidate", &[&label, &cand]);
        }
    }

    fn resize(&self, (w, h): (u32, u32)) {
        println!("stream size → {w}x{h}");
        self.size.set_property("caps", size_caps(w, h, self.va));
    }

    /// Mutes/unmutes the audio branch. False when no audio is being sent.
    fn set_audio(&self, on: bool) -> bool {
        match &self.audio {
            Some(vol) => {
                vol.set_property("mute", !on);
                println!("audio {}", if on { "on" } else { "muted" });
                true
            }
            None => false,
        }
    }

    fn stop(mut self) {
        let _ = self.pipeline.set_state(gst::State::Null);
        self.detach_source();
    }
}

fn send_candidate(d: &Daemon, sid: i32, mids: &[String], mline: u32, cand: &str) {
    let mid = mids
        .get(mline as usize)
        .cloned()
        .unwrap_or_else(|| mline.to_string());
    println!("local candidate: {}", truncate(cand, 90));
    d.send202(
        2,
        sid,
        json!({
            "candidate": json!({ "id": mid, "label": mline, "candidate": cand }).to_string(),
            "messageType": "candidate",
        }),
    );
}

// ---------------------------------------------------------------------------------------------
// On-screen keyboard.

/// The PC's on-screen keyboard, toggled by the keyboard button in the phone's display toolbar
/// (cmd 62 `{"setOSK":bool}`). Like Windows, it is shown on the shared screen itself, so it appears
/// on the phone and taps reach it through the injected pointer. Runs `$SMARTCONNECT_OSK` (a command
/// line) or wvkbd, a layer-shell keyboard that types through virtual-keyboard-v1. GNOME has neither
/// layer-shell nor virtual-keyboard-v1, so there it switches on GNOME's own screen keyboard (it
/// opens when a text field gets focus) and restores the setting afterwards.
#[derive(Default)]
struct Osk {
    child: Option<std::process::Child>,
    /// GNOME: `screen-keyboard-enabled` before we turned it on.
    gnome_was: Option<String>,
}

const GNOME_OSK: [&str; 2] = [
    "org.gnome.desktop.a11y.applications",
    "screen-keyboard-enabled",
];

impl Osk {
    const CANDIDATES: [&'static str; 2] = ["wvkbd-deskintl", "wvkbd-mobintl"];

    /// wvkbd sized for fingers on a phone: the phone shows the whole shared output, so the
    /// keyboard gets `pct` % of that output's (logical) height, a bigger font, and the layout
    /// with a number row (its ⌨ key cycles to symbols and emoji).
    fn wvkbd_args(logical_height: Option<u32>, pct: u32) -> Vec<String> {
        let h = logical_height
            .map_or(300, |lh| lh * pct / 100)
            .clamp(120, 900)
            .to_string();
        [
            "-L",
            &h,
            "-H",
            &h,
            "--fn",
            "Sans 20",
            "--landscape-layers",
            "full,special,emoji",
        ]
        .iter()
        .map(|a| (*a).to_owned())
        .collect()
    }

    /// `target` = the shared screen (None for the test pattern).
    fn show(&mut self, target: Option<&input::Target>, pct: u32, desk: Desktop) {
        if self
            .child
            .as_mut()
            .is_some_and(|c| matches!(c.try_wait(), Ok(None)))
            || self.gnome_was.is_some()
        {
            return;
        }
        let custom = std::env::var("SMARTCONNECT_OSK")
            .ok()
            .filter(|s| !s.trim().is_empty());
        if desk == Desktop::Gnome && custom.is_none() {
            self.show_gnome();
            return;
        }
        // Layer-shell surfaces open on the focused output: focus the one the phone shows.
        if let Some(t) = target.filter(|t| !t.output.is_empty()) {
            desktop::focus_output(desk, &t.output);
        }
        let commands: Vec<Vec<String>> = match custom {
            Some(cmd) => vec![cmd.split_whitespace().map(str::to_owned).collect()],
            None => Self::CANDIDATES
                .iter()
                .map(|c| {
                    let mut argv = vec![(*c).to_owned()];
                    argv.extend(Self::wvkbd_args(target.map(|t| t.rect.3), pct));
                    argv
                })
                .collect(),
        };
        for argv in &commands {
            match std::process::Command::new(&argv[0])
                .args(&argv[1..])
                .spawn()
            {
                Ok(child) => {
                    println!("on-screen keyboard: started {}", argv[0]);
                    self.child = Some(child);
                    return;
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => {
                    eprintln!("on-screen keyboard: {}: {e}", argv[0]);
                    return;
                }
            }
        }
        let hint = "No on-screen keyboard found: install wvkbd (e.g. `yay -S wvkbd`) or set SMARTCONNECT_OSK.";
        eprintln!("{hint}");
        notify("Phone keyboard button", hint);
    }

    fn show_gnome(&mut self) {
        let get = std::process::Command::new("gsettings")
            .arg("get")
            .args(GNOME_OSK)
            .output();
        let was = match get {
            Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).trim().to_owned(),
            _ => {
                eprintln!(
                    "on-screen keyboard: could not read GNOME's screen-keyboard setting (gsettings)"
                );
                return;
            }
        };
        if std::process::Command::new("gsettings")
            .arg("set")
            .args(GNOME_OSK)
            .arg("true")
            .status()
            .is_ok_and(|s| s.success())
        {
            println!(
                "on-screen keyboard: GNOME screen keyboard on (it opens when a text field is focused)"
            );
            self.gnome_was = Some(was);
        }
    }

    fn hide(&mut self) {
        if let Some(mut c) = self.child.take() {
            let _ = c.kill();
            let _ = c.wait();
            println!("on-screen keyboard: hidden");
        }
        if let Some(was) = self.gnome_was.take() {
            let _ = std::process::Command::new("gsettings")
                .arg("set")
                .args(GNOME_OSK)
                .arg(&was)
                .status();
            println!("on-screen keyboard: GNOME screen keyboard back to {was}");
        }
    }
}

impl Drop for Osk {
    fn drop(&mut self) {
        self.hide();
    }
}

// ---------------------------------------------------------------------------------------------
// State machine.

struct App {
    d: Arc<Daemon>,
    o: Opts,
    sid: i32,
    share: Share,
    src: Option<Source>,
    input: SharedInput,
    stream: Option<Stream>,
    /// Current stream size.
    size: (u32, u32),
    /// Stream size before the phone's orientation is applied (offer, cmd 70 or --size).
    base: (u32, u32),
    /// What the phone last reported in cmd 7.
    orient: Option<Orient>,
    /// When to apply a rotation the phone reported (cmd 7 often comes twice; let it settle).
    rotate_at: Option<Instant>,
    /// The PC-audio source when `--audio` is on and works here.
    audio_src: Option<String>,
    /// The phone's audio switch: `setAudio` we sent in cmd 0, then cmd 54 from the phone.
    audio_on: bool,
    /// Fixed ICE candidate filter (tests); `None` = environment / daemon `pcAddress` / interfaces.
    fixed_ice: Option<ice::Filter>,
    osk: Osk,
    desktop: Desktop,
    /// The placement file's mtime last seen, and when it was last checked.
    placement: (Option<std::time::SystemTime>, Instant),
    main_loop: glib::MainLoop,
}

impl App {
    fn connection_ready(&mut self) {
        let set_audio = self.audio_src.is_some() && self.audio_on;
        self.d.send202(
            0,
            self.sid,
            json!({
                "shareType": self.share.as_str(), "setAudio": set_audio, "setScale": self.o.scale,
                "key_pc_ss_version": PC_SS_VERSION, "sessionId": self.sid,
                "logLevel": 2, "collectStats": false,
            }),
        );
    }

    /// Whether the source has to be re-made for a new stream size (extend output / test pattern).
    fn size_matters(&self) -> bool {
        self.o.test_pattern.is_some() || self.share == Share::Extend
    }

    fn follows_rotation(&self) -> bool {
        self.o.follow_rotation && self.size_matters()
    }

    /// The stream size for `self.base` and the phone's orientation.
    fn wanted_size(&self) -> (u32, u32) {
        oriented_size(
            self.base,
            self.follows_rotation(),
            self.orient,
            portrait_max(),
        )
    }

    /// Opens (or reopens, when the size matters and changed) the video source for `size`.
    fn ensure_source(&mut self, size: (u32, u32)) -> Result<()> {
        let size_matters = self.size_matters();
        if self
            .src
            .as_ref()
            .is_some_and(|s| !size_matters || s.size == size)
        {
            return Ok(());
        }
        self.src = None; // close the old cast (and turn the old extend output off) first
        let src = open_source(&self.o, size)?;
        // A new extend output is a new wl_output: the pinned pointer must be re-made.
        self.input
            .lock()
            .unwrap()
            .retarget(&src.target, self.share == Share::Extend);
        self.src = Some(src);
        Ok(())
    }

    /// Gives the running stream a source of `size`: an extend output is resized/turned in place
    /// (its windows stay on it); if that fails, or for the test pattern, a new source is opened.
    fn swap_source(&mut self, size: (u32, u32)) -> Result<()> {
        if let Some(src) = self.src.as_mut() {
            match src.reconfigure(size) {
                Ok(true) => {
                    self.input.lock().unwrap().retarget(&src.target, false);
                    return Ok(());
                }
                Ok(false) => {}
                Err(e) => eprintln!(
                    "resizing the output in place failed ({e:#}); switching it off and on again"
                ),
            }
        }
        self.ensure_source(size)
    }

    /// Hands the stream's new source bin to the capture (screencopy feeds an appsrc in it).
    fn bind_source(&self) {
        if let (Some(cap), Some(bin)) = (
            self.src.as_ref().and_then(|s| s.cap.as_ref()),
            self.stream.as_ref().and_then(Stream::source_bin),
        ) {
            cap.bind(bin);
        }
    }

    fn teardown(&mut self) {
        if let Some(s) = self.stream.take() {
            s.stop();
        }
    }

    fn quit(&mut self, why: &str) {
        println!("stopping: {why}");
        self.osk.hide();
        self.teardown();
        self.input.lock().unwrap().inj = None;
        self.src = None;
        self.main_loop.quit();
    }

    fn on_offer(&mut self, body: &Value) -> Result<()> {
        let sdp = body.get("sdpOffer").and_then(Value::as_str).unwrap_or("");
        if sdp.is_empty() {
            let err = body.get("errorCode").and_then(Value::as_str).unwrap_or("");
            bail!("cmd 5 without an offer (errorCode {err:?})");
        }
        let res = json_field(body, "resolution");
        let g = |k: &str| {
            res.get(k)
                .and_then(Value::as_u64)
                .filter(|v| *v > 0)
                .map(|v| v as u32)
        };
        let requested = (
            g("widthReq").unwrap_or(1920),
            g("heightReq").unwrap_or(1080),
        );
        let (pw, ph) = (
            g("width").unwrap_or(requested.0),
            g("height").unwrap_or(requested.1),
        );
        let native = (pw.max(ph), pw.min(ph)); // the panel in landscape
        self.base = even(match self.o.size {
            SizePref::Requested => requested,
            SizePref::Native => native,
            SizePref::Fixed(w, h) => (w, h),
        });
        let size = self.wanted_size();
        self.rotate_at = None;
        println!(
            "phone panel {pw}x{ph}, asks for {}x{}@{:?} (max height {:?}); sending {}x{}{}",
            requested.0,
            requested.1,
            g("fps"),
            g("maxHeight"),
            size.0,
            size.1,
            if self.follows_rotation() {
                format!(" ({:?})", self.orient.unwrap_or(Orient::Landscape))
            } else {
                String::new()
            }
        );
        self.teardown();
        self.ensure_source(size)?;
        let desc = self.src.as_ref().context("no source")?.desc.clone();
        let cfg = StreamCfg {
            opts: &self.o,
            src_desc: &desc,
            size,
            audio: self.audio_src.clone().map(|s| (s, self.audio_on)),
            ice: self
                .fixed_ice
                .clone()
                .unwrap_or_else(|| ice::Filter::resolve(pc_address)),
        };
        self.stream = Some(Stream::start(
            &self.d,
            self.sid,
            cfg,
            self.input.clone(),
            sdp,
        )?);
        self.bind_source();
        self.size = size;
        Ok(())
    }

    /// A new stream size (cmd 70, or a rotation): extend resizes/turns the output and swaps the
    /// source in the running pipeline; mirror only rescales.
    fn on_resize(&mut self, size: (u32, u32)) -> Result<()> {
        let size = even(size);
        if self.stream.is_none() || size == self.size {
            return Ok(());
        }
        if self.size_matters() {
            self.stream.as_mut().context("no stream")?.detach_source();
            self.swap_source(size)?;
            let desc = self.src.as_ref().context("no source")?.desc.clone();
            self.stream
                .as_mut()
                .context("no stream")?
                .attach_source(&desc)?;
            self.bind_source();
        }
        self.stream.as_ref().context("no stream")?.resize(size);
        self.size = size;
        Ok(())
    }

    /// cmd 7: remembered for the next offer; applied (after a short settle) when following it.
    fn on_orientation(&mut self, n: i64) {
        let Some(o) = Orient::from_phone(n) else {
            println!("phone orientation {n}: unknown value, ignored");
            return;
        };
        let changed = self.orient != Some(o);
        self.orient = Some(o);
        if !changed {
            return;
        }
        if !self.follows_rotation() {
            if o == Orient::Portrait && self.share == Share::Extend {
                println!(
                    "the phone is portrait; --follow-rotation would turn the extended display to match"
                );
            }
            return;
        }
        if self.stream.is_some() {
            self.rotate_at = Some(Instant::now() + ROTATION_SETTLE);
        }
    }

    /// cmd 54: the phone's audio switch.
    fn on_phone_audio(&mut self, on: bool) {
        self.audio_on = on;
        let sent = self.stream.as_ref().is_some_and(|s| s.set_audio(on));
        if on && !sent {
            // Nothing to unmute: this session's answer has no audio. Put the phone's switch back.
            let hint = if self.o.audio {
                "the PC's audio could not be captured"
            } else {
                "start with --audio (or \"audio\": true)"
            };
            println!("the phone turned audio on, but no audio is being sent ({hint})");
            self.audio_on = false;
            self.d.send202(27, self.sid, json!({ "setAudio": false }));
        }
    }

    /// Extend: once a second, whether the user moved the phone to another side; moves the screen.
    fn follow_placement(&mut self) {
        if self.share != Share::Extend || self.placement.1.elapsed() < Duration::from_secs(1) {
            return;
        }
        self.placement.1 = Instant::now();
        let stamp = desktop::placement_stamp();
        if stamp == self.placement.0 {
            return;
        }
        self.placement.0 = stamp;
        let Some(src) = self.src.as_mut() else { return };
        match src.reposition() {
            Ok(true) => self.input.lock().unwrap().retarget(&src.target, false),
            Ok(false) => println!(
                "the phone's place changed; this desktop can't move its screen while it runs"
            ),
            Err(e) => eprintln!("moving the phone's screen: {e:#}"),
        }
    }

    /// Runs from the main loop's poll timer.
    fn tick(&mut self) {
        self.follow_placement();
        if self.rotate_at.is_some_and(|t| Instant::now() >= t) {
            self.rotate_at = None;
            let size = self.wanted_size();
            println!(
                "phone rotated ({:?}): extended display → {}x{}",
                self.orient.unwrap_or(Orient::Landscape),
                size.0,
                size.1
            );
            if let Err(e) = self.on_resize(size) {
                eprintln!("rotation: {e:#}");
            }
        }
    }

    fn on_phone(&mut self, msg: &Value) -> Result<()> {
        let cmd = msg.get("cmd").and_then(Value::as_i64).unwrap_or(-1);
        // Every svc 202 frame reaches every watcher: ignore other sessions (e.g. a second instance).
        if let Some(other) = msg
            .get("sessionId")
            .and_then(Value::as_i64)
            .filter(|s| *s != self.sid as i64 && *s != 0)
            && cmd != 50
        {
            println!("ignoring cmd {cmd} for session {other}");
            return Ok(());
        }
        let body = inner(msg);
        if cmd != 2 {
            println!("← phone cmd {cmd}: {}", truncate(&body.to_string(), 300));
        }
        let share_of = |b: &Value, dflt: Share| {
            b.get("shareType")
                .and_then(Value::as_str)
                .and_then(Share::parse)
                .unwrap_or(dflt)
        };
        match cmd {
            5 => {
                if let Err(e) = self.on_offer(&body) {
                    // Nothing else will answer this offer: without a stop the phone waits on
                    // "Connecting…" (and ignores its next start request).
                    eprintln!("svc 202: {e:#}");
                    notify("Phone screen sharing failed", &format!("{e:#}"));
                    self.d.send202(26, self.sid, json!({}));
                    self.quit("could not start the stream");
                }
            }
            2 => {
                if let Some(s) = &self.stream {
                    s.add_remote_candidate(&body);
                }
            }
            7 => {
                if let Some(n) = body.get("setOrientation").and_then(Value::as_i64) {
                    self.on_orientation(n);
                }
            }
            70 => {
                let w = body.get("setWidth").and_then(Value::as_u64).unwrap_or(0) as u32;
                let h = body.get("setHeight").and_then(Value::as_u64).unwrap_or(0) as u32;
                if w > 0 && h > 0 {
                    self.base = even((w, h));
                    self.rotate_at = None;
                    self.on_resize(self.wanted_size())?;
                }
            }
            54 => {
                if let Some(on) = body.get("setAudio").and_then(Value::as_bool) {
                    self.on_phone_audio(on);
                }
            }
            16 | 72 => {
                // Reconnect: new session id, same mode unless the phone says otherwise. Switching
                // between mirror and extend needs another source, so keep ours (log it).
                let share = share_of(&body, self.share);
                if share != self.share {
                    println!(
                        "phone asks for {} but this process sends {}; keeping it",
                        share.as_str(),
                        self.share.as_str()
                    );
                }
                if let Some(on) = body.get("setAudio").and_then(Value::as_bool) {
                    self.audio_on = on;
                }
                self.teardown();
                self.sid += 1;
                self.connection_ready();
            }
            50 => {
                if self.stream.is_none() {
                    let share = share_of(&body, self.share);
                    if share != self.share {
                        println!(
                            "phone asks for {} but this process sends {}",
                            share.as_str(),
                            self.share.as_str()
                        );
                    }
                    self.connection_ready();
                }
            }
            62 => {
                if body.get("setOSK").and_then(Value::as_bool).unwrap_or(false) {
                    let target = self.src.as_ref().and_then(|s| s.target.clone());
                    self.osk
                        .show(target.as_ref(), self.o.osk_height, self.desktop);
                } else {
                    self.osk.hide();
                }
            }
            52 => self.quit("the phone stopped screen sharing"),
            12 => self.quit("the phone reports an ICE failure"),
            13 | 66 | 68 => {
                eprintln!("phone refused: {body}");
                self.quit("refused by the phone");
            }
            _ => {}
        }
        Ok(())
    }
}

/// A desktop notification (best effort): a phone-started session has no terminal to read.
fn notify(summary: &str, body: &str) {
    let _ = std::process::Command::new("notify-send")
        .args(["-a", "Smart Connect", summary, body])
        .spawn();
}

/// `--audio`: the PC-audio source if it delivers sound here, else `None` (with the reason).
fn prepare_audio() -> Option<String> {
    let src = audio::source_description();
    match audio::check_elements().and_then(|_| audio::preflight(&src)) {
        Ok(()) => {
            println!("audio: {src}");
            Some(src)
        }
        Err(e) => {
            eprintln!("audio disabled: {src}: {e:#}");
            notify(
                "Phone screen: no PC audio",
                &format!("Sending video only: {e:#}"),
            );
            None
        }
    }
}

pub fn run() -> Result<()> {
    let Some(o) = opts::parse_args()? else {
        return Ok(());
    };
    let _lock = single_instance(o.replace)?;
    if let Some(edge) = o.side {
        desktop::set_side(edge);
    }
    gst::init()?;
    let audio_src = if o.audio { prepare_audio() } else { None };
    let main_loop = glib::MainLoop::new(None, false);
    let (tx, rx) = mpsc::channel::<Ev>();
    watch(tx)?;
    let d = Daemon::connect()?;
    let sid = (std::process::id() % 1000 + 1) as i32;
    let app = std::rc::Rc::new(std::cell::RefCell::new(App {
        d: d.clone(),
        o: o.clone(),
        sid,
        share: o.mode,
        src: None,
        input: Arc::default(),
        stream: None,
        size: (0, 0),
        base: (1920, 1080),
        orient: None,
        rotate_at: None,
        audio_on: audio_src.is_some(),
        audio_src,
        fixed_ice: None,
        osk: Osk::default(),
        desktop: Desktop::detect(),
        placement: (desktop::placement_stamp(), Instant::now()),
        main_loop: main_loop.clone(),
    }));
    // Mirror captures an existing output: open it before asking the phone (the portal may ask
    // once). Extend needs the phone's size first, so it opens on the offer.
    if o.mode == Share::Mirror && o.test_pattern.is_none() {
        app.borrow_mut().ensure_source((1920, 1080))?;
    }
    {
        let app = app.clone();
        glib::timeout_add_local(Duration::from_millis(15), move || {
            let mut a = app.borrow_mut();
            while let Ok(ev) = rx.try_recv() {
                match ev {
                    Ev::DaemonGone => {
                        a.quit("daemon went away");
                        return glib::ControlFlow::Break;
                    }
                    Ev::Phone(msg) => {
                        if let Err(e) = a.on_phone(&msg) {
                            eprintln!("svc 202: {e:#}");
                        }
                    }
                }
            }
            a.tick();
            glib::ControlFlow::Continue
        });
    }
    for signal in [2, 15] {
        let app = app.clone();
        glib::unix_signal_add_local(signal, move || {
            let mut a = app.borrow_mut();
            let sid = a.sid;
            a.d.send202(26, sid, json!({}));
            a.quit("signal");
            glib::ControlFlow::Break
        });
    }
    // Debug aid: SMARTCONNECT_DISPLAY_DEBUG_RESIZE=WxH acts as if the phone sent cmd 70 after 15 s,
    // SMARTCONNECT_DISPLAY_DEBUG_ROTATE=1|2 as if it sent cmd 7 {setOrientation} after 15 s.
    if let Some((w, h)) = std::env::var("SMARTCONNECT_DISPLAY_DEBUG_RESIZE")
        .ok()
        .and_then(|v| {
            let (w, h) = v.split_once('x')?;
            Some((w.parse::<u32>().ok()?, h.parse::<u32>().ok()?))
        })
    {
        let app = app.clone();
        glib::timeout_add_local_once(Duration::from_secs(15), move || {
            println!("debug: simulating cmd 70 {w}x{h}");
            let msg = json!({ "cmd": 70, "data": json!({ "setWidth": w, "setHeight": h, "setAuto": 0 }).to_string() });
            if let Err(e) = app.borrow_mut().on_phone(&msg) {
                eprintln!("resize: {e:#}");
            }
        });
    }
    if let Some(n) = std::env::var("SMARTCONNECT_DISPLAY_DEBUG_ROTATE")
        .ok()
        .and_then(|v| v.trim().parse::<i64>().ok())
    {
        let app = app.clone();
        glib::timeout_add_local_once(Duration::from_secs(15), move || {
            println!("debug: simulating cmd 7 setOrientation {n}");
            let msg = json!({ "cmd": 7, "data": json!({ "setOrientation": n }).to_string() });
            let _ = app.borrow_mut().on_phone(&msg);
        });
    }
    println!(
        "asking the phone to start ({} mode, session {sid}{}{})",
        o.mode.as_str(),
        if app.borrow().audio_src.is_some() {
            ", with audio"
        } else {
            ""
        },
        if o.follow_rotation {
            ", following rotation"
        } else {
            ""
        }
    );
    app.borrow_mut().connection_ready();
    main_loop.run();
    // Let the stop message reach the daemon before the socket closes.
    std::thread::sleep(Duration::from_millis(200));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const OFFER: &str = "v=0\r\nm=audio 9 UDP/TLS/RTP/SAVPF 111\r\na=mid:0\r\na=recvonly\r\na=rtpmap:111 opus/48000/2\r\n\
m=video 9 UDP/TLS/RTP/SAVPF 96 97 102 108 39\r\na=mid:1\r\na=recvonly\r\n\
a=extmap:13 urn:3gpp:video-orientation\r\na=rtpmap:96 VP8/90000\r\na=rtcp-fb:96 nack\r\n\
a=rtpmap:97 rtx/90000\r\na=fmtp:97 apt=96\r\na=rtpmap:102 H264/90000\r\n\
a=fmtp:102 level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=640c34\r\n\
a=rtpmap:108 H264/90000\r\na=rtcp-fb:108 nack pli\r\n\
a=fmtp:108 level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e034\r\n\
a=rtpmap:39 H264/90000\r\na=fmtp:39 level-asymmetry-allowed=1;packetization-mode=0;profile-level-id=42e034\r\n\
m=application 9 UDP/DTLS/SCTP webrtc-datachannel\r\na=mid:2\r\na=sctp-port:5000";

    #[test]
    fn picks_constrained_baseline_packetization_mode_1() {
        assert_eq!(pick_h264(OFFER), Some((108, "42e034".to_owned())));
    }

    #[test]
    fn keeps_only_the_chosen_video_payload() {
        let sdp = video_only_pt(OFFER, 108);
        assert!(sdp.contains("m=video 9 UDP/TLS/RTP/SAVPF 108\r\n"));
        assert!(sdp.contains("a=rtcp-fb:108 nack pli"));
        assert!(
            !sdp.contains("VP8")
                && !sdp.contains("a=fmtp:102")
                && !sdp.contains("video-orientation")
        );
        // Other m-sections are untouched.
        assert!(sdp.contains("a=rtpmap:111 opus/48000/2") && sdp.contains("a=sctp-port:5000"));
        assert_eq!(audio::pick_opus(&sdp), Some(111));
    }

    #[test]
    fn reads_section_directions() {
        assert_eq!(direction(OFFER, "audio"), "a=recvonly");
        assert_eq!(direction(OFFER, "video"), "a=recvonly");
        assert_eq!(
            direction("v=0\r\nm=video 9 X 96\r\na=mid:0\r\n", "video"),
            "?"
        );
        assert_eq!(
            direction("v=0\r\nm=video 9 X 96\r\na=sendonly\r\n", "audio"),
            "-"
        );
    }

    #[test]
    fn osk_is_sized_from_the_shared_screen() {
        let args = Osk::wvkbd_args(Some(610), 45);
        assert_eq!(&args[..4], ["-L", "274", "-H", "274"]);
        assert!(args.contains(&"full,special,emoji".to_owned()));
        assert_eq!(Osk::wvkbd_args(None, 45)[1], "300");
    }

    #[test]
    fn phone_strings_are_parsed() {
        let msg =
            json!({ "cmd": 70, "data": "{\"setWidth\":1280,\"setHeight\":720,\"setAuto\":0}" });
        assert_eq!(inner(&msg)["setWidth"], 1280);
        assert_eq!(
            inner(&json!({ "cmd": 12, "data": "iceFailure" })),
            json!("iceFailure")
        );
        let body = json!({ "resolution": "{\"widthReq\":1920,\"heightReq\":1080}" });
        assert_eq!(json_field(&body, "resolution")["heightReq"], 1080);
        assert_eq!(even((2711, 1221)), (2710, 1220));
        assert_eq!(
            inner(&json!({ "cmd": 7, "data": "{\"setOrientation\":2}" }))["setOrientation"],
            2
        );
    }

    #[test]
    fn orientation_turns_the_stream() {
        assert_eq!(Orient::from_phone(1), Some(Orient::Landscape));
        assert_eq!(Orient::from_phone(2), Some(Orient::Portrait));
        assert_eq!(Orient::from_phone(0), None);
        // Requested 1920x1080: portrait is the same size turned.
        assert_eq!(
            oriented_size((1920, 1080), true, Some(Orient::Portrait), 1920),
            (1080, 1920)
        );
        assert_eq!(
            oriented_size((1920, 1080), true, Some(Orient::Landscape), 1920),
            (1920, 1080)
        );
        assert_eq!(oriented_size((1920, 1080), true, None, 1920), (1920, 1080));
        // Native 2712x1220: portrait capped at 1920 tall, aspect kept, even sides.
        assert_eq!(
            oriented_size((2712, 1220), true, Some(Orient::Portrait), 1920),
            (864, 1920)
        );
        assert_eq!(
            oriented_size((2712, 1220), true, Some(Orient::Portrait), 0),
            (1220, 2712)
        );
        assert_eq!(
            oriented_size((2560, 1440), true, Some(Orient::Portrait), 1920),
            (1080, 1920)
        );
        // Not following: the size as given (even a portrait --size).
        assert_eq!(
            oriented_size((1080, 1920), false, Some(Orient::Landscape), 1920),
            (1080, 1920)
        );
        assert_eq!(oriented_size((2711, 1221), false, None, 1920), (2710, 1220));
    }

    #[test]
    fn lock_refuses_a_second_instance() {
        // A private path, so it never meets a real instance (and never signals one).
        let path =
            std::env::temp_dir().join(format!("sc-display-lock-test-{}.lock", std::process::id()));
        let first = single_instance_at(&path, false).expect("first instance");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap().trim(),
            std::process::id().to_string()
        );
        let err = single_instance_at(&path, false).expect_err("second instance");
        assert!(
            format!("{err:#}").contains(&format!("pid {}", std::process::id())),
            "{err:#}"
        );
        // The daemon's probe (helpers::lock_held) sees it held: a shared lock fails.
        let probe = std::fs::File::open(&path).unwrap();
        assert_ne!(
            unsafe { libc::flock(probe.as_raw_fd(), libc::LOCK_SH | libc::LOCK_NB) },
            0
        );
        drop(first);
        assert_eq!(
            unsafe { libc::flock(probe.as_raw_fd(), libc::LOCK_SH | libc::LOCK_NB) },
            0,
            "free after exit"
        );
        drop(probe);
        let again = single_instance_at(&path, false).expect("after the first one ended");
        drop(again);
        let _ = std::fs::remove_file(&path);
    }

    /// A phone-like WebRTC peer for offline tests: offerer with receive-only audio (Opus 111) and
    /// video (H.264 108, constrained baseline) and a "touchDataChannel", like the real offer.
    /// Counts the RTP it receives and decodes nothing, but parses the H.264 to see its size.
    struct FakePhone {
        pipeline: gst::Pipeline,
        wb: gst::Element,
        /// [audio, video, other] RTP packets.
        counts: Arc<[std::sync::atomic::AtomicUsize; 3]>,
        /// Size of the H.264 it last received.
        video_size: Arc<Mutex<Option<(i32, i32)>>>,
        /// Its ICE candidates, for our side.
        cands: mpsc::Receiver<(u32, String)>,
        offer: String,
    }

    impl FakePhone {
        fn new() -> FakePhone {
            use std::sync::atomic::AtomicUsize;
            gst::init().unwrap();
            let pipeline = gst::Pipeline::new();
            let wb = gst::ElementFactory::make("webrtcbin")
                .property_from_str("bundle-policy", "max-bundle")
                .build()
                .unwrap();
            pipeline.add(&wb).unwrap();
            for caps in [
                "application/x-rtp,media=audio,encoding-name=OPUS,clock-rate=48000,payload=111,encoding-params=(string)2",
                "application/x-rtp,media=video,encoding-name=H264,clock-rate=90000,payload=108,packetization-mode=(string)1,profile-level-id=(string)42e034",
            ] {
                let caps = gst::Caps::from_str(caps).unwrap();
                wb.emit_by_name::<gst_webrtc::WebRTCRTPTransceiver>(
                    "add-transceiver",
                    &[&gst_webrtc::WebRTCRTPTransceiverDirection::Recvonly, &caps],
                );
            }
            let counts: Arc<[AtomicUsize; 3]> = Arc::new([
                AtomicUsize::new(0),
                AtomicUsize::new(0),
                AtomicUsize::new(0),
            ]);
            let video_size = Arc::new(Mutex::new(None));
            {
                let (pipeline, counts, video_size) =
                    (pipeline.clone(), counts.clone(), video_size.clone());
                wb.connect_pad_added(move |_, pad| {
                    let caps = pad.current_caps().unwrap_or_else(|| pad.query_caps(None));
                    let media = caps
                        .structure(0)
                        .and_then(|s| s.get::<String>("media").ok())
                        .unwrap_or_default();
                    let i = match media.as_str() {
                        "audio" => 0,
                        "video" => 1,
                        _ => 2,
                    };
                    let tail = if i == 1 {
                        "rtph264depay ! h264parse name=parse ! fakesink sync=false"
                    } else {
                        "fakesink sync=false"
                    };
                    let bin =
                        gst::parse::bin_from_description(&format!("queue ! {tail}"), true).unwrap();
                    pipeline.add(&bin).unwrap();
                    bin.sync_state_with_parent().unwrap();
                    pad.link(&bin.static_pad("sink").unwrap()).unwrap();
                    let counts = counts.clone();
                    pad.add_probe(gst::PadProbeType::BUFFER, move |_, _| {
                        counts[i].fetch_add(1, Ordering::SeqCst);
                        gst::PadProbeReturn::Ok
                    });
                    if let Some(parse) = bin.by_name("parse") {
                        let video_size = video_size.clone();
                        parse.static_pad("src").unwrap().connect_notify(
                            Some("caps"),
                            move |pad, _| {
                                if let Some(s) = pad
                                    .current_caps()
                                    .and_then(|c| c.structure(0).map(|s| s.to_owned()))
                                    && let (Ok(w), Ok(h)) =
                                        (s.get::<i32>("width"), s.get::<i32>("height"))
                                {
                                    *video_size.lock().unwrap() = Some((w, h));
                                }
                            },
                        );
                    }
                    println!("phone: receiving {media:?} on {}", pad.name());
                });
            }
            let (ctx, cands) = mpsc::channel::<(u32, String)>();
            wb.connect("on-ice-candidate", false, move |args| {
                let _ = ctx.send((
                    args[1].get::<u32>().unwrap(),
                    args[2].get::<String>().unwrap(),
                ));
                None
            });
            pipeline.set_state(gst::State::Playing).unwrap();
            let _dc = wb.emit_by_name::<Option<gst_webrtc::WebRTCDataChannel>>(
                "create-data-channel",
                &[&"touchDataChannel", &None::<gst::Structure>],
            );
            let p = gst::Promise::new();
            wb.emit_by_name::<()>("create-offer", &[&None::<gst::Structure>, &p]);
            p.wait();
            let offer = p
                .get_reply()
                .unwrap()
                .value("offer")
                .unwrap()
                .get::<gst_webrtc::WebRTCSessionDescription>()
                .unwrap();
            let p = gst::Promise::new();
            wb.emit_by_name::<()>("set-local-description", &[&offer, &p]);
            p.wait();
            let offer = offer.sdp().as_text().unwrap();
            assert_eq!(audio::pick_opus(&offer), Some(111), "phone offer:\n{offer}");
            FakePhone {
                pipeline,
                wb,
                counts,
                video_size,
                cands,
                offer,
            }
        }

        fn count(&self, i: usize) -> usize {
            self.counts[i].load(Ordering::SeqCst)
        }

        fn size(&self) -> Option<(i32, i32)> {
            *self.video_size.lock().unwrap()
        }

        /// Applies one svc 202 message from our side (as the daemon would deliver it). Returns the
        /// answer SDP for cmd 1; other commands are returned as (cmd, inner data).
        fn handle(&self, msg: &Value) -> Option<(i64, Value)> {
            let cmd = msg["cmd"].as_i64().unwrap();
            match cmd {
                1 => {
                    let a: Value =
                        serde_json::from_str(msg["data"]["answer"].as_str().unwrap()).unwrap();
                    let sdp =
                        gst_sdp::SDPMessage::parse_buffer(a["sdp"].as_str().unwrap().as_bytes())
                            .unwrap();
                    let desc = gst_webrtc::WebRTCSessionDescription::new(
                        gst_webrtc::WebRTCSDPType::Answer,
                        sdp,
                    );
                    let p = gst::Promise::new();
                    self.wb
                        .emit_by_name::<()>("set-remote-description", &[&desc, &p]);
                    p.wait();
                    Some((1, a["sdp"].clone()))
                }
                2 => {
                    let c: Value =
                        serde_json::from_str(msg["data"]["candidate"].as_str().unwrap()).unwrap();
                    let mline = c["label"].as_u64().unwrap() as u32;
                    self.wb.emit_by_name::<()>(
                        "add-ice-candidate",
                        &[&mline, &c["candidate"].as_str().unwrap()],
                    );
                    None
                }
                _ => Some((cmd, msg["data"].clone())),
            }
        }

        fn stop(&self) {
            let _ = self.pipeline.set_state(gst::State::Null);
        }
    }

    /// Our side's daemon connection: what it sends comes out of the returned channel.
    fn fake_daemon() -> (Arc<Daemon>, mpsc::Receiver<Value>) {
        let (ours, theirs) = UnixStream::pair().unwrap();
        let (tx, rx) = mpsc::channel::<Value>();
        std::thread::spawn(move || {
            for line in BufReader::new(theirs).lines().map_while(Result::ok) {
                if let Ok(v) = serde_json::from_str::<Value>(&line) {
                    assert_eq!(v["service"], 202);
                    let _ = tx.send(v["data"].clone());
                }
            }
        });
        (
            Arc::new(Daemon {
                cmd: Mutex::new(ours),
            }),
            rx,
        )
    }

    /// Relays signalling between our side and the phone until `done` or 15 s; returns the answer
    /// and the other commands we sent.
    fn pump(
        phone: &FakePhone,
        rx: &mpsc::Receiver<Value>,
        add_cand: &dyn Fn(u32, String),
        done: &dyn Fn() -> bool,
    ) -> (String, Vec<(i64, Value)>) {
        let deadline = Instant::now() + Duration::from_secs(15);
        let (mut answer, mut other) = (String::new(), Vec::new());
        while Instant::now() < deadline && !done() {
            while let Ok((mline, cand)) = phone.cands.try_recv() {
                add_cand(mline, cand);
            }
            if let Ok(msg) = rx.recv_timeout(Duration::from_millis(50)) {
                match phone.handle(&msg) {
                    Some((1, sdp)) => answer = sdp.as_str().unwrap().to_owned(),
                    Some(o) => other.push(o),
                    None => {}
                }
            }
        }
        (answer, other)
    }

    /// Offline loopback (no phone, no daemon): `Stream::start` answers the fake phone with the test
    /// pattern (and, with `with_audio`, a test tone). Returns the answer, the audio/video RTP the
    /// phone got, and (with audio) the audio packets in the second after muting.
    fn loopback(with_audio: bool) -> (String, usize, usize, usize) {
        let phone = FakePhone::new();
        let (d, rx) = fake_daemon();
        let o = Opts {
            test_pattern: Some("ball".into()),
            force_x264: true,
            bitrate: 2000,
            ..Opts::default()
        };
        let cfg = StreamCfg {
            opts: &o,
            src_desc: "videotestsrc is-live=true pattern=ball ! video/x-raw,width=640,height=360,framerate=30/1",
            size: (640, 360),
            audio: with_audio.then(|| ("audiotestsrc is-live=true wave=sine".into(), true)),
            ice: ice::Filter::any("test"),
        };
        let stream = Stream::start(&d, 7, cfg, Arc::default(), &phone.offer).expect("answer");
        let add =
            |m: u32, c: String| stream.add_remote_candidate(&json!({ "label": m, "candidate": c }));
        let done = || (!with_audio || phone.count(0) > 20) && phone.count(1) > 20;
        let (answer, _) = pump(&phone, &rx, &add, &done);
        let (a, v) = (phone.count(0), phone.count(1));
        println!(
            "answer:\n{answer}\nphone got {a} audio, {v} video, {} other RTP packets",
            phone.count(2)
        );
        // Muting (the phone's audio switch) keeps the audio stream going: Opus encodes silence.
        let mut muted = 0;
        if stream.set_audio(false) {
            std::thread::sleep(Duration::from_secs(1));
            muted = phone.count(0) - a;
            println!("{muted} audio packets in 1 s while muted");
        }
        stream.stop();
        phone.stop();
        (answer, a, v, muted)
    }

    /// `cargo test --release loopback -- --ignored --nocapture --test-threads=1`
    #[test]
    #[ignore]
    fn loopback_audio_and_video_reach_a_phone_like_peer() {
        let (answer, a, v, muted) = loopback(true);
        assert_eq!(direction(&answer, "audio"), "a=sendonly");
        assert_eq!(direction(&answer, "video"), "a=sendonly");
        assert!(answer.to_ascii_uppercase().contains("OPUS/48000/2"));
        assert!(a > 20 && v > 20, "audio {a}, video {v}");
        assert!(muted > 20, "muted audio stopped flowing ({muted})");
    }

    /// Without `--audio` the answer is what the device verified: audio section inactive, video sent.
    #[test]
    #[ignore]
    fn loopback_without_audio_keeps_the_audio_section_inactive() {
        let (answer, a, v, _) = loopback(false);
        assert_eq!(direction(&answer, "audio"), "a=inactive");
        assert_eq!(direction(&answer, "video"), "a=sendonly");
        assert!(a == 0 && v > 20, "audio {a}, video {v}");
    }

    /// Offline, through the state machine (`App::on_phone`) with the test pattern and
    /// `--follow-rotation`: offer → cmd 7 portrait → cmd 70 → cmd 7 landscape, checking the size
    /// of the H.264 the fake phone receives each time; then cmd 54 audio-on without audio is
    /// answered with cmd 27 `{setAudio:false}`.
    #[test]
    #[ignore]
    fn loopback_rotation_and_resolution_follow_the_phone() {
        let phone = FakePhone::new();
        let (d, rx) = fake_daemon();
        let o = Opts {
            test_pattern: Some("smpte".into()),
            force_x264: true,
            bitrate: 2000,
            size: SizePref::Fixed(640, 360),
            follow_rotation: true,
            ..Opts::default()
        };
        let mut app = App {
            d,
            share: o.mode,
            o,
            sid: 7,
            src: None,
            input: Arc::default(),
            stream: None,
            size: (0, 0),
            base: (1920, 1080),
            orient: None,
            rotate_at: None,
            audio_src: None,
            audio_on: false,
            fixed_ice: Some(ice::Filter::any("test")),
            osk: Osk::default(),
            desktop: Desktop::Other,
            placement: (None, Instant::now()),
            main_loop: glib::MainLoop::new(None, false),
        };
        let phone_msg = |cmd: i64, data: Value| json!({ "ver": "1", "cmd": cmd, "sessionId": 7, "data": data.to_string() });
        let resolution = json!({ "width": 1220, "height": 2712, "widthReq": 1920, "heightReq": 1080, "fps": 60, "maxHeight": 1440 });
        app.on_phone(&phone_msg(5, json!({ "sdpOffer": phone.offer, "resolution": resolution.to_string(), "errorCode": "" }))).unwrap();
        let mut sizes = Vec::new();
        let mut step = |app: &mut App, want: (i32, i32)| {
            let add = |m: u32, c: String| {
                if let Some(s) = &app.stream {
                    s.add_remote_candidate(&json!({ "label": m, "candidate": c }));
                }
            };
            let (_, other) = pump(&phone, &rx, &add, &|| phone.size() == Some(want));
            println!("phone receives {:?} (wanted {want:?})", phone.size());
            sizes.push(phone.size());
            other
        };
        step(&mut app, (640, 360));
        // The phone turns portrait (it sends cmd 7 twice); applied after the settle time.
        app.on_phone(&phone_msg(7, json!({ "setOrientation": 2 })))
            .unwrap();
        app.on_phone(&phone_msg(7, json!({ "setOrientation": 2 })))
            .unwrap();
        assert!(app.rotate_at.is_some());
        std::thread::sleep(ROTATION_SETTLE + Duration::from_millis(50));
        app.tick();
        assert_eq!(app.size, (360, 640));
        step(&mut app, (360, 640));
        // The user picks 1280x720 in the phone's menu while portrait: sent turned.
        app.on_phone(&phone_msg(
            70,
            json!({ "setWidth": 1280, "setHeight": 720, "setAuto": 0 }),
        ))
        .unwrap();
        assert_eq!(app.size, (720, 1280));
        step(&mut app, (720, 1280));
        app.on_phone(&phone_msg(7, json!({ "setOrientation": 1 })))
            .unwrap();
        std::thread::sleep(ROTATION_SETTLE + Duration::from_millis(50));
        app.tick();
        assert_eq!(app.size, (1280, 720));
        step(&mut app, (1280, 720));
        // Audio switched on at the phone, but this session sends none: the phone is told it's off.
        app.on_phone(&phone_msg(
            54,
            json!({ "shareType": "extend", "setAudio": true }),
        ))
        .unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut reply = None;
        while reply.is_none() && Instant::now() < deadline {
            if let Ok(msg) = rx.recv_timeout(Duration::from_millis(100))
                && msg["cmd"] == 27
            {
                reply = Some(msg["data"].clone());
            }
        }
        app.teardown();
        phone.stop();
        assert_eq!(
            sizes,
            vec![
                Some((640, 360)),
                Some((360, 640)),
                Some((720, 1280)),
                Some((1280, 720))
            ]
        );
        assert_eq!(reply, Some(json!({ "setAudio": false })));
        assert!(!app.audio_on);
    }
}
