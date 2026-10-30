//! smartconnect-webcam: the phone's camera as a Linux webcam (the Smart Connect "Webcam" tile,
//! service 3). Protocol write-up: `evidence-2026-10-20/webcam-spec.md`.
//!
//! smartconnectd relays the svc 3 signalling over its control socket (`watch` / `send`). The media
//! never touches the daemon: the phone opens a WebRTC session straight to this process
//! (H.264 over DTLS-SRTP/UDP), which decodes it with GStreamer and writes the frames to a
//! v4l2loopback device, or offers them as a PipeWire camera node when no loopback device exists.
//!
//!   tile on the phone:  phone cmd 0 {isReply:0} → (daemon spawns `--answer`) → PC cmd 0 {isReply:1, port}
//!   start from the PC:  PC cmd 0 (no isReply) → phone cmd 0 {isReply:1} → PC cmd 1 {port}
//!   then:               phone cmd 1 {webRTCData: offer} → PC cmd 17 answer; cmd 18 ICE both ways;
//!                       PC cmd 13 heartbeat every 10 s (the phone gives up after 30 s of silence);
//!                       cmd 2 = stop (either side).

mod output;
mod proto;

use std::fs::File;
use std::io::{BufRead, BufReader, Read, Seek, Write};
use std::net::Ipv4Addr;
use std::os::fd::AsRawFd;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use gst::prelude::*;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use output::Output;
use proto::Phone;

const USAGE: &str =
    "usage: smartconnect-webcam [--start | --stop | --check | --test-pattern [SECONDS]
                           | --switch-camera | --torch | --zoom N | --cameras
                           | --pause | --resume | --mirror [on|off] | --flip-v [on|off]
                           | --brightness N | --contrast N | --status]
                           [--output auto|v4l2|pipewire|none] [--device /dev/videoN] [--size WxH]
                           [--quality low|medium|high|max]
  --start (default)  ask the phone to start its camera and serve it as a webcam until stopped
                     (Ctrl+C, --stop, or Stop on the phone)
  --answer           answer the phone's own request (its Webcam tile); smartconnectd runs this
  --stop             stop the running webcam session
  --check            show where frames would go and how to set up v4l2loopback
  --test-pattern [S] feed a test pattern to the output for S seconds (default 10), no phone needed
  --switch-camera    front <-> back camera        --torch     toggle the phone's light
  --zoom N           zoom level (0 = none)        --cameras   ask the phone for its camera state
  --pause            pause camera stream          --resume    resume camera stream
  --mirror [on|off]  flip stream horizontally     --flip-v    flip stream vertically
  --brightness N     brightness (-1.0 to 1.0)     --contrast  contrast (0.0 to 2.0)
  --status           show live status of webcam session and effects
  --output           auto (default): v4l2loopback when present, else PipeWire; none = decode only
  --quality          encoder bitrate the phone may use: low 1.5, medium 4, high 8 (default),
                     max 15 Mbit/s. The phone always sends 1280x720 at 30 fps in this mode.
                     Default from ~/.config/smartconnect-linux/webcam.json {\"quality\": \"high\"}
env: SMARTCONNECT_WEBCAM_OUTPUT, SMARTCONNECT_WEBCAM_DEVICE, SMARTCONNECT_WEBCAM_DECODER,
     SMARTCONNECT_WEBCAM_FORMAT (default YUY2)";

/// The phone gives up after 60 s from the trigger to the stream; so do we.
const START_TIMEOUT: Duration = Duration::from_secs(60);
/// The phone tears the session down after 30 s without a heartbeat.
const HEARTBEAT_EVERY: Duration = Duration::from_secs(10);
const ICE_GRACE: Duration = Duration::from_secs(15);
const STATS_EVERY: Duration = Duration::from_secs(5);

static STOP: AtomicBool = AtomicBool::new(false);

extern "C" fn on_signal(_: libc::c_int) {
    STOP.store(true, Ordering::SeqCst);
}

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

fn daemon_socket() -> PathBuf {
    runtime_dir().join("smartconnectd.sock")
}

fn ctl_socket_path() -> PathBuf {
    runtime_dir().join("smartconnect-webcam-ctl.sock")
}

fn config_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("XDG_CONFIG_HOME").filter(|v| !v.is_empty()) {
        return PathBuf::from(dir).join("smartconnect-linux");
    }
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default()
        .join(".config/smartconnect-linux")
}

fn webcam_prefs_path() -> PathBuf {
    config_dir().join("webcam.json")
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct WebcamEffects {
    pub mirror: bool,
    pub flip_vertical: bool,
    pub brightness: f64,
    pub contrast: f64,
}

impl Default for WebcamEffects {
    fn default() -> Self {
        Self {
            mirror: false,
            flip_vertical: false,
            brightness: 0.0,
            contrast: 1.0,
        }
    }
}

fn load_prefs() -> (String, WebcamEffects) {
    let p = webcam_prefs_path();
    let Ok(data) = std::fs::read_to_string(&p) else {
        return ("high".into(), WebcamEffects::default());
    };
    let val: Value = serde_json::from_str(&data).unwrap_or_default();
    let quality = val
        .get("quality")
        .and_then(|v| v.as_str())
        .unwrap_or("high")
        .to_string();
    let fx: WebcamEffects = serde_json::from_value(val).unwrap_or_default();
    (quality, fx)
}

fn save_prefs(quality: &str, fx: WebcamEffects) {
    let p = webcam_prefs_path();
    if let Some(parent) = p.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let val = json!({
        "quality": quality,
        "mirror": fx.mirror,
        "flipVertical": fx.flip_vertical,
        "brightness": fx.brightness,
        "contrast": fx.contrast,
    });
    let _ = std::fs::write(&p, val.to_string());
}

fn apply_effects(bin: &gst::Bin, fx: WebcamEffects) {
    if let Some(flip) = bin.by_name("flip") {
        let dir = match (fx.mirror, fx.flip_vertical) {
            (false, false) => "identity",
            (true, false) => "horiz",
            (false, true) => "vert",
            (true, true) => "180",
        };
        flip.set_property_from_str("video-direction", dir);
    }
    if let Some(balance) = bin.by_name("balance") {
        balance.set_property("brightness", fx.brightness.clamp(-1.0, 1.0));
        balance.set_property("contrast", fx.contrast.clamp(0.0, 2.0));
    }
}

fn send_ctl_cmd(req: Value) -> Result<Value> {
    let sock = ctl_socket_path();
    let mut stream = UnixStream::connect(&sock)?;
    writeln!(stream, "{req}")?;
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line)?;
    Ok(serde_json::from_str(&line)?)
}

fn spawn_ctl_server(
    active_bin: Arc<Mutex<Option<gst::Bin>>>,
    fx: Arc<Mutex<WebcamEffects>>,
    quality: Arc<Mutex<String>>,
    stats: Arc<Stats>,
    daemon: Arc<Daemon>,
) -> Result<()> {
    let sock = ctl_socket_path();
    let _ = std::fs::remove_file(&sock);
    if let Some(p) = sock.parent() {
        let _ = std::fs::create_dir_all(p);
        let _ = std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o700));
    }
    let listener = UnixListener::bind(&sock)?;
    let _ = std::fs::set_permissions(&sock, std::fs::Permissions::from_mode(0o600));
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            // Verify peer credentials (SO_PEERCRED)
            let fd = stream.as_raw_fd();
            let mut ucred = libc::ucred {
                pid: 0,
                uid: 0,
                gid: 0,
            };
            let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
            let cred_ok = unsafe {
                libc::getsockopt(
                    fd,
                    libc::SOL_SOCKET,
                    libc::SO_PEERCRED,
                    &mut ucred as *mut _ as *mut libc::c_void,
                    &mut len,
                ) == 0
                    && ucred.uid == libc::geteuid()
            };
            if !cred_ok {
                eprintln!("webcam-ctl: refusing connection from untrusted UID");
                continue;
            }

            let mut reader = BufReader::new(&stream);
            let mut line = String::new();
            if reader.read_line(&mut line).is_ok() {
                let req: Value = serde_json::from_str(&line).unwrap_or_default();
                let cmd = req.get("cmd").and_then(Value::as_str).unwrap_or("");
                let mut reply = json!({ "ok": true });
                match cmd {
                    "get_status" => {
                        let cur_fx = *fx.lock().unwrap();
                        let cur_q = quality.lock().unwrap().clone();
                        let size = stats
                            .size
                            .lock()
                            .unwrap()
                            .map(|(w, h)| format!("{w}x{h}"))
                            .unwrap_or_else(|| "1280x720".into());
                        let total = stats.frames.load(Ordering::Relaxed);
                        reply = json!({
                            "ok": true,
                            "active": true,
                            "quality": cur_q,
                            "size": size,
                            "frames": total,
                            "mirror": cur_fx.mirror,
                            "flipVertical": cur_fx.flip_vertical,
                            "brightness": cur_fx.brightness,
                            "contrast": cur_fx.contrast,
                        });
                    }
                    "set_effects" => {
                        let mut cur_fx = fx.lock().unwrap();
                        if let Some(m) = req.get("mirror").and_then(Value::as_bool) {
                            cur_fx.mirror = m;
                        }
                        if let Some(fv) = req.get("flipVertical").and_then(Value::as_bool) {
                            cur_fx.flip_vertical = fv;
                        }
                        if let Some(b) = req
                            .get("brightness")
                            .and_then(Value::as_f64)
                            .filter(|v| v.is_finite())
                        {
                            cur_fx.brightness = b.clamp(-1.0, 1.0);
                        }
                        if let Some(c) = req
                            .get("contrast")
                            .and_then(Value::as_f64)
                            .filter(|v| v.is_finite())
                        {
                            cur_fx.contrast = c.clamp(0.0, 2.0);
                        }
                        if let Some(ref bin) = *active_bin.lock().unwrap() {
                            apply_effects(bin, *cur_fx);
                        }
                        let cur_q = quality.lock().unwrap().clone();
                        save_prefs(&cur_q, *cur_fx);
                        reply = json!({
                            "ok": true,
                            "mirror": cur_fx.mirror,
                            "flipVertical": cur_fx.flip_vertical,
                            "brightness": cur_fx.brightness,
                            "contrast": cur_fx.contrast,
                        });
                    }
                    "set_quality" => {
                        if let Some(q) = req.get("quality").and_then(Value::as_str) {
                            if proto::Quality::parse(q).is_some() {
                                *quality.lock().unwrap() = q.to_string();
                                let cur_fx = *fx.lock().unwrap();
                                save_prefs(q, cur_fx);
                                reply = json!({ "ok": true });
                            } else {
                                reply = json!({ "ok": false, "error": "invalid quality" });
                            }
                        }
                    }
                    "switch_camera" => {
                        daemon.send(proto::camera_command(proto::CMD_SWITCH_CAMERA, None));
                        reply = json!({ "ok": true });
                    }
                    "toggle_torch" => {
                        daemon.send(proto::camera_command(proto::CMD_TOGGLE_TORCH, None));
                        reply = json!({ "ok": true });
                    }
                    "stop" => {
                        STOP.store(true, Ordering::SeqCst);
                        reply = json!({ "ok": true });
                    }
                    _ => {
                        reply = json!({ "ok": false, "error": "unknown command" });
                    }
                }
                let mut resp_stream = &stream;
                let _ = writeln!(resp_stream, "{reply}");
            }
        }
        let _ = std::fs::remove_file(ctl_socket_path());
    });
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// smartconnectd control socket

struct Daemon {
    cmd: Mutex<UnixStream>,
}

impl Daemon {
    fn connect() -> Result<Arc<Self>> {
        let cmd = UnixStream::connect(daemon_socket()).context("is smartconnectd running?")?;
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

    fn send(&self, data: Value) {
        let cmd = data.get("cmd").and_then(Value::as_i64).unwrap_or(-1);
        let line =
            json!({ "cmd": "send", "service": proto::SERVICE, "data": data }).to_string() + "\n";
        match self.cmd.lock().unwrap().write_all(line.as_bytes()) {
            Ok(()) if cmd != proto::CMD_HEARTBEAT => println!("-> svc 3 cmd {cmd}"),
            Ok(()) => {}
            Err(e) => eprintln!("daemon write failed: {e}"),
        }
    }
}

/// One `send` through the daemon, returning its reply line.
fn send_once(data: Value) -> Result<Value> {
    let mut s = UnixStream::connect(daemon_socket()).context("is smartconnectd running?")?;
    s.write_all(
        format!(
            "{}\n",
            json!({ "cmd": "send", "service": proto::SERVICE, "data": data })
        )
        .as_bytes(),
    )?;
    let mut line = String::new();
    BufReader::new(s).read_line(&mut line)?;
    Ok(serde_json::from_str(&line)?)
}

enum Ev {
    Phone(Value),
    DaemonGone,
}

/// Streams the phone's svc 3 payloads into `tx`.
fn watch(tx: mpsc::Sender<Ev>) -> Result<()> {
    let watch = UnixStream::connect(daemon_socket()).context("is smartconnectd running?")?;
    (&watch).write_all(
        format!(
            "{}\n",
            json!({ "cmd": "watch", "services": [proto::SERVICE] })
        )
        .as_bytes(),
    )?;
    let mut reader = BufReader::new(watch);
    let mut first = String::new();
    reader.read_line(&mut first)?; // {"ok":true,"message":"watching"}: from here on nothing is missed
    if !first.contains("\"ok\":true") {
        bail!("daemon refused watch: {}", first.trim());
    }
    std::thread::spawn(move || {
        for line in reader.lines().map_while(Result::ok) {
            if let Ok(v) = serde_json::from_str::<Value>(&line)
                && v.get("service").and_then(Value::as_i64) == Some(proto::SERVICE)
            {
                let _ = tx.send(Ev::Phone(v["data"].clone()));
            }
        }
        let _ = tx.send(Ev::DaemonGone);
    });
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Single-instance lock ($XDG_RUNTIME_DIR/smartconnect-webcam.lock holds the pid; smartconnectd
// checks it before spawning an answerer).

fn lock_path() -> PathBuf {
    runtime_dir().join("smartconnect-webcam.lock")
}

fn try_lock() -> Result<Option<File>> {
    let mut f = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(lock_path())?;
    // SAFETY: flock on an fd we own.
    if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        return Ok(None);
    }
    f.set_len(0)?;
    f.rewind()?;
    write!(f, "{}", std::process::id())?;
    Ok(Some(f))
}

/// Pid of the running instance, if one holds the lock.
fn running_pid() -> Option<i32> {
    let mut f = File::open(lock_path()).ok()?;
    // SAFETY: flock on an fd we own; a shared lock fails while an instance holds its exclusive one.
    if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_SH | libc::LOCK_NB) } == 0 {
        return None;
    }
    let mut s = String::new();
    f.read_to_string(&mut s).ok()?;
    s.trim().parse().ok()
}

// ---------------------------------------------------------------------------------------------
// GStreamer output chain (shared by the phone stream and the test pattern)

fn pick_decoder_from(override_dec: Option<&str>) -> String {
    let ok = |n: &str| gst::ElementFactory::make(n).build().is_ok();
    if let Some(d) = override_dec {
        match d {
            "cpu" => return "avdec_h264 thread-type=slice max-threads=4".into(),
            "nvdec" | "nvidia" => {
                if ok("nvh264dec") && ok("cudadownload") {
                    return "nvh264dec ! cudadownload".into();
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
        "nvh264dec ! cudadownload".into()
    } else {
        "avdec_h264 thread-type=slice max-threads=4".into()
    }
}

fn pick_decoder() -> String {
    let env_d = std::env::var("SMARTCONNECT_WEBCAM_DECODER").ok();
    pick_decoder_from(env_d.as_deref())
}

/// Raw video → fixed size/format/rate (the phone may rotate or change size; a v4l2loopback device
/// can't change format mid-stream) → leaky queue (a stalled consumer never blocks decoding) → sink.
fn output_chain(out: &Output, (w, h): (i32, i32)) -> String {
    let format = std::env::var("SMARTCONNECT_WEBCAM_FORMAT")
        .ok()
        .filter(|s| !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric()))
        .unwrap_or_else(|| "YUY2".into());
    let sink = match out {
        Output::V4l2 { device, .. } => {
            format!("v4l2sink name=sink device=\"{}\" sync=false", device.display())
        }
        // No buffer pool: PipeWire's pool only recycles buffers while some app reads the camera,
        // and an exhausted pool would stall the whole pipeline until then.
        Output::PipeWire => {
            "pipewiresink name=sink mode=provide use-bufferpool=false sync=false client-name=smartconnect-webcam".into()
        }
        Output::Discard => "fakesink name=sink sync=false".into(),
    };
    format!(
        "videoconvert ! videoflip name=flip video-direction=identity ! videobalance name=balance ! videoscale add-borders=true ! videorate ! \
         video/x-raw,format={format},width={w},height={h},framerate=30/1,pixel-aspect-ratio=1/1 ! \
         queue leaky=downstream max-size-buffers=2 max-size-bytes=0 max-size-time=0 ! {sink}"
    )
}

fn configure_sink(bin: &gst::Bin, out: &Output) {
    if *out == Output::PipeWire
        && let Some(sink) = bin.by_name("sink")
    {
        let props = gst::Structure::builder("props")
            .field("media.class", "Video/Source")
            .field("media.role", "Camera")
            .field("node.name", "smartconnect-webcam")
            .field("node.description", output::PIPEWIRE_DESCRIPTION)
            .build();
        sink.set_property("stream-properties", &props);
    }
}

/// Counts decoded frames on `pad` and remembers the latest size.
#[derive(Default)]
struct Stats {
    frames: AtomicU64,
    size: Mutex<Option<(i32, i32)>>,
    printed_frames: AtomicU64,
}

impl Stats {
    fn watch(self: &Arc<Self>, pad: &gst::Pad) {
        let s = self.clone();
        pad.add_probe(
            gst::PadProbeType::BUFFER | gst::PadProbeType::EVENT_DOWNSTREAM,
            move |_, info| {
                match &info.data {
                    Some(gst::PadProbeData::Buffer(_)) => {
                        if s.frames.fetch_add(1, Ordering::Relaxed) == 0 {
                            println!("first decoded frame");
                        }
                    }
                    Some(gst::PadProbeData::Event(ev)) => {
                        if let gst::EventView::Caps(c) = ev.view()
                            && let Some(st) = c.caps().structure(0)
                            && let (Ok(w), Ok(h)) =
                                (st.get::<i32>("width"), st.get::<i32>("height"))
                        {
                            *s.size.lock().unwrap() = Some((w, h));
                        }
                    }
                    _ => {}
                }
                gst::PadProbeReturn::Ok
            },
        );
    }

    fn line(&self, secs: f64) -> String {
        let total = self.frames.load(Ordering::Relaxed);
        let new = total - self.printed_frames.swap(total, Ordering::Relaxed);
        let size = self
            .size
            .lock()
            .unwrap()
            .map_or("?".into(), |(w, h)| format!("{w}x{h}"));
        format!(
            "frames: {total} total, {:.1} fps, decoded size {size}",
            new as f64 / secs
        )
    }
}

// ---------------------------------------------------------------------------------------------
// One WebRTC session (webrtcbin answerer for the phone's H.264 track)

#[derive(Default)]
struct RtcState {
    mids: Vec<String>,
    answered: bool,
    pending_local: Vec<(u32, String)>,
    phone_ip: Option<Ipv4Addr>,
    session_id: i64,
    ice_bad_since: Option<Instant>,
}

struct Rtc {
    d: Arc<Daemon>,
    quality: proto::Quality,
    pipeline: gst::Pipeline,
    webrtc: gst::Element,
    st: Mutex<RtcState>,
    out: Output,
    size: (i32, i32),
    stats: Arc<Stats>,
    fx: Arc<Mutex<WebcamEffects>>,
    active_bin: Arc<Mutex<Option<gst::Bin>>>,
}

impl Rtc {
    #[allow(clippy::too_many_arguments)]
    fn new(
        d: Arc<Daemon>,
        out: Output,
        size: (i32, i32),
        phone_ip: Option<Ipv4Addr>,
        quality: proto::Quality,
        stats: Arc<Stats>,
        fx: Arc<Mutex<WebcamEffects>>,
        active_bin: Arc<Mutex<Option<gst::Bin>>>,
    ) -> Result<Arc<Self>> {
        let pipeline = gst::Pipeline::with_name("smartconnect-webcam");
        let webrtc = gst::ElementFactory::make("webrtcbin")
            .property_from_str("bundle-policy", "max-bundle")
            .build()?;
        pipeline.add(&webrtc)?;
        let rtc = Arc::new(Rtc {
            d,
            quality,
            pipeline: pipeline.clone(),
            webrtc: webrtc.clone(),
            st: Mutex::new(RtcState {
                phone_ip,
                ..Default::default()
            }),
            out,
            size,
            stats,
            fx,
            active_bin,
        });
        let w = Arc::downgrade(&rtc);
        webrtc.connect("on-ice-candidate", false, move |args| {
            let (mline, cand) = (args[1].get::<u32>().ok()?, args[2].get::<String>().ok()?);
            let rtc = w.upgrade()?;
            let mut st = rtc.st.lock().unwrap();
            if !proto::keep_local_candidate(&cand, st.phone_ip) {
                return None;
            }
            if st.answered {
                drop(st);
                rtc.send_candidate(mline, &cand);
            } else {
                st.pending_local.push((mline, cand));
            }
            None
        });
        let w = Arc::downgrade(&rtc);
        webrtc.connect_notify(Some("ice-connection-state"), move |wb, _| {
            use gst_webrtc::WebRTCICEConnectionState as S;
            let state: S = wb.property("ice-connection-state");
            println!("ICE: {state:?}");
            if let Some(rtc) = w.upgrade() {
                let bad = matches!(state, S::Failed | S::Disconnected | S::Closed);
                let mut st = rtc.st.lock().unwrap();
                st.ice_bad_since = if bad {
                    st.ice_bad_since.or_else(|| Some(Instant::now()))
                } else {
                    None
                };
            }
        });
        let w = Arc::downgrade(&rtc);
        webrtc.connect_pad_added(move |_, pad| {
            if pad.direction() != gst::PadDirection::Src {
                return;
            }
            // Runs on a streaming thread: never panic here.
            if let Some(rtc) = w.upgrade()
                && let Err(e) = rtc.attach(pad)
            {
                eprintln!("could not attach the decoder: {e:#}");
            }
        });
        pipeline.set_state(gst::State::Playing)?;
        Ok(rtc)
    }

    fn attach(&self, pad: &gst::Pad) -> Result<()> {
        let caps = pad
            .current_caps()
            .map(|c| c.to_string())
            .unwrap_or_default();
        if !caps.contains("video") {
            println!("ignoring non-video stream: {caps}");
            return Ok(());
        }
        println!("video arriving: {caps}");
        let desc = format!(
            "queue ! rtph264depay ! h264parse ! {} ! identity name=dec ! {}",
            pick_decoder(),
            output_chain(&self.out, self.size)
        );
        let bin = gst::parse::bin_from_description(&desc, true)?;
        configure_sink(&bin, &self.out);
        apply_effects(&bin, *self.fx.lock().unwrap());
        *self.active_bin.lock().unwrap() = Some(bin.clone());
        let dec = bin.by_name("dec").context("no decoder")?;
        self.stats
            .watch(&dec.static_pad("src").context("decoder has no src pad")?);
        self.pipeline.add(&bin)?;
        pad.link(
            &bin.static_pad("sink")
                .context("decoder bin has no sink pad")?,
        )?;
        bin.sync_state_with_parent()?;
        println!("writing frames to {}", self.out.describe());
        Ok(())
    }

    fn send_candidate(&self, mline: u32, candidate: &str) {
        let (mid, sid) = {
            let st = self.st.lock().unwrap();
            (
                st.mids
                    .get(mline as usize)
                    .cloned()
                    .unwrap_or_else(|| mline.to_string()),
                st.session_id,
            )
        };
        self.d.send(proto::candidate(sid, &mid, mline, candidate));
    }

    fn add_remote_candidate(&self, mline: u32, candidate: &str) {
        self.webrtc
            .emit_by_name::<()>("add-ice-candidate", &[&mline, &candidate]);
    }

    fn answer(&self, offer_sdp: &str) -> Result<()> {
        let offer_sdp = proto::h264_only(offer_sdp);
        self.st.lock().unwrap().mids = proto::mids(&offer_sdp);
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
        let reply = p.get_reply().context("webrtcbin gave no answer")?;
        let answer = reply
            .value("answer")?
            .get::<gst_webrtc::WebRTCSessionDescription>()?;
        let text = answer.sdp().as_text()?;
        if !text.contains("H264") {
            bail!("the answer has no H.264 (did the phone offer it?)");
        }
        self.webrtc
            .emit_by_name::<()>("set-local-description", &[&answer, &None::<gst::Promise>]);
        let sid = self.st.lock().unwrap().session_id;
        let (_, _, max) = self.quality.kbps();
        println!(
            "answer: {} bytes, H.264, quality {:?} (up to {max} kbit/s)",
            text.len(),
            self.quality
        );
        self.d.send(proto::answer(
            sid,
            &proto::with_bitrate(&text, self.quality),
        ));
        let pending = {
            let mut st = self.st.lock().unwrap();
            st.answered = true;
            std::mem::take(&mut st.pending_local)
        };
        for (m, c) in pending {
            self.send_candidate(m, &c);
        }
        Ok(())
    }

    /// An error from the pipeline, if one was posted.
    fn bus_error(&self) -> Option<String> {
        let bus = self.pipeline.bus()?;
        while let Some(msg) =
            bus.pop_filtered(&[gst::MessageType::Error, gst::MessageType::Warning])
        {
            match msg.view() {
                gst::MessageView::Error(e) => {
                    return Some(format!("{} ({:?})", e.error(), e.debug()));
                }
                gst::MessageView::Warning(w) => {
                    eprintln!("warning: {} ({:?})", w.error(), w.debug())
                }
                _ => {}
            }
        }
        None
    }

    fn ice_down_for(&self) -> Option<Duration> {
        self.st.lock().unwrap().ice_bad_since.map(|t| t.elapsed())
    }

    fn close(&self) {
        *self.active_bin.lock().unwrap() = None;
        let _ = self.pipeline.set_state(gst::State::Null);
    }
}

// ---------------------------------------------------------------------------------------------
// Session state machine

#[derive(Debug, PartialEq, Clone, Copy)]
enum Phase {
    /// We asked the phone (PC-initiated); waiting for its cmd 0 {isReply:1}.
    Requested,
    /// The phone was told to start; waiting for its cmd 1 with the WebRTC offer.
    Starting,
    Streaming,
}

enum Flow {
    Continue,
    Done(i32),
}

struct Session {
    d: Arc<Daemon>,
    out: Output,
    size: (i32, i32),
    phase: Phase,
    since: Instant,
    last_heartbeat: Instant,
    last_stats: Instant,
    rtc: Option<Arc<Rtc>>,
    early_remote: Vec<(u32, String)>,
    /// `o=` session id and version of the offer the current `rtc` answered.
    offer_origin: Option<(String, u64)>,
    quality: proto::Quality,
    stats: Arc<Stats>,
    fx: Arc<Mutex<WebcamEffects>>,
    active_bin: Arc<Mutex<Option<gst::Bin>>>,
}

impl Session {
    #[allow(clippy::too_many_arguments)]
    fn new(
        d: Arc<Daemon>,
        out: Output,
        size: (i32, i32),
        phase: Phase,
        quality: proto::Quality,
        stats: Arc<Stats>,
        fx: Arc<Mutex<WebcamEffects>>,
        active_bin: Arc<Mutex<Option<gst::Bin>>>,
    ) -> Self {
        let now = Instant::now();
        Session {
            d,
            out,
            size,
            phase,
            since: now,
            last_heartbeat: now,
            last_stats: now,
            rtc: None,
            early_remote: Vec::new(),
            offer_origin: None,
            quality,
            stats,
            fx,
            active_bin,
        }
    }

    fn enter(&mut self, phase: Phase) {
        println!("phase: {phase:?}");
        self.phase = phase;
        self.since = Instant::now();
    }

    fn on_phone(&mut self, msg: Phone) -> Flow {
        match msg {
            Phone::Handshake { is_reply: Some(1) } if self.phase == Phase::Requested => {
                println!("the phone accepted; asking it to start");
                self.d.send(proto::start());
                self.enter(Phase::Starting);
            }
            Phone::Handshake { is_reply: Some(0) } if self.phase == Phase::Requested => {
                // Both sides asked at once: treat it as the phone's own request.
                self.d.send(proto::handshake_reply());
                self.enter(Phase::Starting);
            }
            Phone::Handshake { is_reply } => println!(
                "phone handshake (isReply {is_reply:?}) in {:?}; ignored",
                self.phase
            ),
            Phone::Start {
                offer_sdp: Some(sdp),
                phone_ip,
            } => {
                let ip = phone_ip.as_deref().and_then(|s| s.parse().ok());
                let origin = proto::sdp_origin(&sdp);
                let kind = match &self.rtc {
                    Some(_) => proto::classify_offer(self.offer_origin.as_ref(), origin.as_ref()),
                    None => proto::OfferKind::NewSession,
                };
                match kind {
                    // Answering the same offer twice makes the phone's peer connection fail (its
                    // signalling state is already stable) and it stops the webcam.
                    proto::OfferKind::Repeat => {
                        println!(
                            "the phone re-sent its offer (camera switch?); keeping the session"
                        );
                        return Flow::Continue;
                    }
                    proto::OfferKind::Renegotiation => {
                        println!("the phone renegotiated; answering on the same connection");
                        if let Some(rtc) = self.rtc.clone()
                            && let Err(e) = rtc.answer(&sdp)
                        {
                            eprintln!("renegotiation failed: {e:#}");
                            return self.finish("WebRTC renegotiation failed", true, 1);
                        }
                        self.offer_origin = origin;
                        return Flow::Continue;
                    }
                    proto::OfferKind::NewSession => {}
                }
                if let Some(old) = self.rtc.take() {
                    println!("the phone started a new WebRTC session");
                    old.close();
                }
                match Rtc::new(
                    self.d.clone(),
                    self.out.clone(),
                    self.size,
                    ip,
                    self.quality,
                    self.stats.clone(),
                    self.fx.clone(),
                    self.active_bin.clone(),
                )
                .and_then(|rtc| {
                    rtc.answer(&sdp)?;
                    Ok(rtc)
                }) {
                    Ok(rtc) => {
                        for (m, c) in self.early_remote.drain(..) {
                            rtc.add_remote_candidate(m, &c);
                        }
                        self.rtc = Some(rtc);
                        self.offer_origin = origin;
                        self.enter(Phase::Streaming);
                    }
                    Err(e) => {
                        eprintln!("WebRTC setup failed: {e:#}");
                        return self.finish("WebRTC setup failed", true, 1);
                    }
                }
            }
            Phone::Start {
                offer_sdp: None, ..
            } => {
                eprintln!(
                    "the phone started without a WebRTC offer (legacy SRTP mode is not supported)"
                );
                return self.finish("no WebRTC offer", true, 1);
            }
            Phone::Candidate {
                session_id,
                mline,
                candidate,
                ..
            } => match &self.rtc {
                Some(rtc) => {
                    if let Some(sid) = session_id {
                        rtc.st.lock().unwrap().session_id = sid;
                    }
                    rtc.add_remote_candidate(mline, &candidate);
                }
                None => self.early_remote.push((mline, candidate)),
            },
            Phone::Heartbeat => {}
            Phone::CameraState(v) => println!("{}", proto::describe_camera_state(&v)),
            Phone::Stop => return self.finish("the phone stopped the webcam", false, 0),
            Phone::Error { code, about_cmd } => {
                eprintln!(
                    "phone error {code} (about cmd {about_cmd}): {}",
                    proto::error_text(code)
                );
                return self.finish("phone refused", false, 1);
            }
            Phone::Other(cmd) => println!("phone svc 3 cmd {cmd} (unhandled)"),
        }
        Flow::Continue
    }

    fn tick(&mut self) -> Flow {
        if self.phase != Phase::Requested && self.last_heartbeat.elapsed() >= HEARTBEAT_EVERY {
            self.last_heartbeat = Instant::now();
            self.d.send(proto::heartbeat());
        }
        match self.phase {
            Phase::Requested if self.since.elapsed() > START_TIMEOUT => {
                eprintln!(
                    "the phone did not accept within 60 s (if its screen is locked, it asks for confirmation there)"
                );
                return self.finish("no answer", true, 1);
            }
            Phase::Starting if self.since.elapsed() > START_TIMEOUT => {
                eprintln!("the phone did not start streaming within 60 s");
                return self.finish("no stream", true, 1);
            }
            _ => {}
        }
        if let Some(rtc) = self.rtc.clone() {
            if let Some(err) = rtc.bus_error() {
                eprintln!("pipeline error: {err}");
                return self.finish("pipeline error", true, 1);
            }
            if rtc.ice_down_for().is_some_and(|d| d > ICE_GRACE) {
                return self.finish("the WebRTC connection dropped", true, 1);
            }
            if self.last_stats.elapsed() >= STATS_EVERY {
                println!(
                    "{}",
                    rtc.stats.line(self.last_stats.elapsed().as_secs_f64())
                );
                self.last_stats = Instant::now();
            }
        }
        Flow::Continue
    }

    fn finish(&mut self, why: &str, tell_phone: bool, code: i32) -> Flow {
        println!("stopping: {why}");
        if tell_phone {
            self.d.send(proto::stop());
        }
        if let Some(rtc) = self.rtc.take() {
            println!(
                "{}",
                rtc.stats
                    .line(self.last_stats.elapsed().as_secs_f64().max(0.001))
            );
            rtc.close();
        }
        Flow::Done(code)
    }
}

fn run_session(
    answer_phone: bool,
    out: Output,
    size: (i32, i32),
    quality: proto::Quality,
) -> Result<i32> {
    let Some(_lock) = try_lock()? else {
        if answer_phone {
            println!("a webcam session is already running; it answers the phone");
            return Ok(0);
        }
        bail!(
            "a webcam session is already running (pid {:?}); stop it with --stop",
            running_pid()
        );
    };
    gst::init()?;
    for e in [
        "webrtcbin",
        "rtph264depay",
        "h264parse",
        "videoconvert",
        "videoscale",
        "videorate",
    ] {
        if gst::ElementFactory::find(e).is_none() {
            if answer_phone && let Ok(d) = Daemon::connect() {
                d.send(proto::error(proto::ERR_NO_DRIVER));
            }
            bail!("GStreamer element {e} is missing (install gst-plugins-good/bad)");
        }
    }
    let d = Daemon::connect()?;
    let (tx, rx) = mpsc::channel();
    watch(tx)?;
    // SAFETY: the handler only stores to an atomic.
    unsafe {
        libc::signal(libc::SIGINT, on_signal as *const () as libc::sighandler_t);
        libc::signal(libc::SIGTERM, on_signal as *const () as libc::sighandler_t);
    }
    println!("output: {}", out.describe());
    if out == Output::PipeWire {
        println!("(no v4l2loopback device; run with --check for the setup commands)");
    }
    let (_saved_quality, saved_fx) = load_prefs();
    let fx = Arc::new(Mutex::new(saved_fx));
    let active_bin = Arc::new(Mutex::new(None));
    let quality_name = Arc::new(Mutex::new(quality.to_string()));
    let stats = Arc::new(Stats::default());
    spawn_ctl_server(
        active_bin.clone(),
        fx.clone(),
        quality_name.clone(),
        stats.clone(),
        d.clone(),
    )?;

    let mut s = if answer_phone {
        println!("answering the phone's webcam request");
        d.send(proto::handshake_reply());
        Session::new(
            d,
            out,
            size,
            Phase::Starting,
            quality,
            stats,
            fx,
            active_bin,
        )
    } else {
        println!("asking the phone to start its camera");
        d.send(proto::pc_request());
        Session::new(
            d,
            out,
            size,
            Phase::Requested,
            quality,
            stats,
            fx,
            active_bin,
        )
    };
    loop {
        let flow = if STOP.load(Ordering::SeqCst) {
            s.finish("interrupted", true, 0)
        } else {
            Flow::Continue
        };
        if let Flow::Done(code) = flow {
            std::thread::sleep(Duration::from_millis(200));
            return Ok(code);
        }
        let flow = match rx.recv_timeout(Duration::from_millis(200)) {
            Ok(Ev::Phone(data)) => match proto::parse(&data) {
                Some(m) => s.on_phone(m),
                None => Flow::Continue,
            },
            Ok(Ev::DaemonGone) => s.finish("smartconnectd closed the connection", false, 1),
            Err(mpsc::RecvTimeoutError::Timeout) => Flow::Continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => s.finish("watch ended", false, 1),
        };
        if let Flow::Done(code) = flow {
            // Let the daemon forward a final cmd 2 before the socket closes.
            std::thread::sleep(Duration::from_millis(200));
            return Ok(code);
        }
        if let Flow::Done(code) = s.tick() {
            std::thread::sleep(Duration::from_millis(200));
            return Ok(code);
        }
    }
}

fn test_pattern(out: Output, size: (i32, i32), secs: u64) -> Result<i32> {
    gst::init()?;
    println!("test pattern → {} for {secs} s", out.describe());
    let desc = format!(
        "videotestsrc is-live=true pattern=smpte ! video/x-raw,width=1280,height=720,framerate=30/1 ! identity name=dec ! {}",
        output_chain(&out, size)
    );
    let pipeline = gst::parse::launch(&desc)?
        .downcast::<gst::Pipeline>()
        .map_err(|_| anyhow!("not a pipeline"))?;
    configure_sink(pipeline.upcast_ref(), &out);
    let stats = Arc::new(Stats::default());
    stats.watch(
        &pipeline
            .by_name("dec")
            .context("no identity")?
            .static_pad("src")
            .context("no pad")?,
    );
    pipeline.set_state(gst::State::Playing)?;
    let bus = pipeline.bus().context("no bus")?;
    let start = Instant::now();
    let mut last = Instant::now();
    let mut code = 0;
    while start.elapsed() < Duration::from_secs(secs) {
        if let Some(msg) = bus.timed_pop_filtered(
            gst::ClockTime::from_mseconds(200),
            &[gst::MessageType::Error],
        ) && let gst::MessageView::Error(e) = msg.view()
        {
            eprintln!("pipeline error: {} ({:?})", e.error(), e.debug());
            code = 1;
            break;
        }
        if last.elapsed() >= STATS_EVERY {
            println!("{}", stats.line(last.elapsed().as_secs_f64()));
            last = Instant::now();
        }
    }
    println!("{}", stats.line(last.elapsed().as_secs_f64().max(0.001)));
    pipeline.set_state(gst::State::Null)?;
    Ok(code)
}

fn check(out: &Output) -> i32 {
    println!("frames would go to: {}", out.describe());
    let devices = output::loopback_devices();
    if devices.is_empty() {
        println!("\n{}", output::SETUP_HINT);
    } else {
        for (dev, label) in devices {
            println!("  v4l2loopback device: {} \"{label}\"", dev.display());
        }
    }
    if gst::init().is_ok() {
        let decoder = pick_decoder();
        println!("  decoder:        {decoder}");
        let ok = |n: &str| gst::ElementFactory::make(n).build().is_ok();
        println!(
            "  vah264dec       {}",
            if ok("vah264dec") {
                "ok"
            } else {
                "not available"
            }
        );
        println!(
            "  vaapih264dec    {}",
            if ok("vaapih264dec") {
                "ok"
            } else {
                "not available"
            }
        );
        println!(
            "  nvh264dec       {}",
            if ok("nvh264dec") && ok("cudadownload") {
                "ok"
            } else {
                "not available"
            }
        );
        for e in [
            "webrtcbin",
            "nicesrc",
            "rtph264depay",
            "h264parse",
            "avdec_h264",
            "v4l2sink",
            "pipewiresink",
        ] {
            println!(
                "  {e:<14} {}",
                if gst::ElementFactory::find(e).is_some() {
                    "ok"
                } else {
                    "MISSING"
                }
            );
        }
    }
    match running_pid() {
        Some(pid) => println!("a session is running (pid {pid})"),
        None => println!("no session running"),
    }
    0
}

fn stop_running() -> Result<i32> {
    let Some(pid) = running_pid() else {
        // Nothing local is running; still tell the phone, in case it kept streaming.
        let reply = send_once(proto::stop())?;
        println!("no webcam session running here; sent stop to the phone: {reply}");
        return Ok(0);
    };
    // SAFETY: kill(2) on the pid recorded by the running instance.
    unsafe { libc::kill(pid, libc::SIGTERM) };
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if running_pid().is_none() {
            println!("stopped (pid {pid})");
            return Ok(0);
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    bail!("pid {pid} did not stop within 5 s")
}

/// `quality` from `$XDG_CONFIG_HOME/smartconnect-linux/webcam.json` (shared with the GUI and the
/// Noctalia plugin), so sessions the daemon starts from the phone's tile use it too.
fn configured_quality() -> Option<proto::Quality> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    let text = std::fs::read_to_string(base.join("smartconnect-linux").join("webcam.json")).ok()?;
    let v: Value = serde_json::from_str(&text).ok()?;
    proto::Quality::parse(v.get("quality")?.as_str()?)
}

fn parse_size(s: &str) -> Result<(i32, i32)> {
    let (w, h) = s.split_once('x').context("--size needs WxH")?;
    let w: i32 = w.parse()?;
    let h: i32 = h.parse()?;
    if w <= 0 || h <= 0 || w > 8192 || h > 8192 {
        bail!("dimensions must be positive and <= 8192 (got {w}x{h})");
    }
    Ok((w, h))
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut mode = "--start".to_owned();
    let mut kind = std::env::var("SMARTCONNECT_WEBCAM_OUTPUT").unwrap_or_else(|_| "auto".into());
    let mut device = std::env::var_os("SMARTCONNECT_WEBCAM_DEVICE").map(PathBuf::from);
    let mut size = (1280, 720);
    let mut number: Option<i64> = None;
    let mut quality = configured_quality().unwrap_or(proto::Quality::High);
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--output" => kind = it.next().context("--output needs a value")?.clone(),
            "--device" => device = Some(PathBuf::from(it.next().context("--device needs a path")?)),
            "--size" => size = parse_size(it.next().context("--size needs WxH")?)?,
            "--quality" => {
                let q = it.next().context("--quality needs low|medium|high|max")?;
                quality = proto::Quality::parse(q)
                    .with_context(|| format!("unknown quality {q:?}: use low|medium|high|max"))?;
            }
            "--mirror" => {
                mode = "--mirror".into();
                if let Some(next) = it.as_slice().first()
                    && !next.starts_with("--")
                {
                    let s = it.next().unwrap();
                    match s.as_str() {
                        "on" | "true" | "1" => number = Some(1),
                        "off" | "false" | "0" => number = Some(0),
                        _ => bail!("--mirror accepts on|off"),
                    }
                }
            }
            "--flip-v" => {
                mode = "--flip-v".into();
                if let Some(next) = it.as_slice().first()
                    && !next.starts_with("--")
                {
                    let s = it.next().unwrap();
                    match s.as_str() {
                        "on" | "true" | "1" => number = Some(1),
                        "off" | "false" | "0" => number = Some(0),
                        _ => bail!("--flip-v accepts on|off"),
                    }
                }
            }
            "--brightness" => {
                mode = "--brightness".into();
                let b_str = it
                    .next()
                    .context("--brightness needs a number (-1.0 to 1.0 or -100 to 100)")?;
                let b_val: f64 = b_str.parse().context("invalid brightness number")?;
                let pct = if b_val.abs() <= 1.0 && b_str.contains('.') {
                    (b_val * 100.0).round() as i64
                } else {
                    b_val.round() as i64
                };
                number = Some(pct);
            }
            "--contrast" => {
                mode = "--contrast".into();
                let c_str = it
                    .next()
                    .context("--contrast needs a number (0.0 to 2.0 or 0 to 200)")?;
                let c_val: f64 = c_str.parse().context("invalid contrast number")?;
                let pct = if c_val <= 2.0 && c_str.contains('.') {
                    (c_val * 100.0).round() as i64
                } else {
                    c_val.round() as i64
                };
                number = Some(pct);
            }
            "-h" | "--help" => {
                println!("{USAGE}");
                return Ok(());
            }
            m if m.starts_with("--") => mode = m.to_owned(),
            n => {
                number = Some(
                    n.parse()
                        .with_context(|| format!("unexpected argument {n:?}\n{USAGE}"))?,
                )
            }
        }
    }
    let code = match mode.as_str() {
        "--start" | "--answer" => {
            let out = output::choose(&kind, device.as_deref())?;
            run_session(mode == "--answer", out, size, quality)?
        }
        "--stop" => stop_running()?,
        "--check" => check(&output::choose(&kind, device.as_deref())?),
        "--test-pattern" => test_pattern(
            output::choose(&kind, device.as_deref())?,
            size,
            number.unwrap_or(10).max(1) as u64,
        )?,
        "--switch-camera" | "--torch" | "--cameras" | "--zoom" => {
            let msg = match mode.as_str() {
                "--switch-camera" => proto::camera_command(proto::CMD_SWITCH_CAMERA, None),
                "--torch" => proto::camera_command(proto::CMD_TOGGLE_TORCH, None),
                "--cameras" => proto::camera_command(proto::CMD_QUERY_CAMERAS, None),
                _ => proto::camera_command(
                    proto::CMD_ZOOM,
                    Some(json!({ "state": number.context("--zoom needs a level")? })),
                ),
            };
            println!("{}", send_once(msg)?);
            0
        }
        "--pause" => {
            println!("{}", send_once(proto::pause())?);
            0
        }
        "--resume" => {
            println!("{}", send_once(proto::resume(proto::PORT))?);
            0
        }
        "--mirror" => {
            let (q, mut fx) = load_prefs();
            let val = match number {
                Some(1) => true,
                Some(0) => false,
                _ => !fx.mirror,
            };
            fx.mirror = val;
            if send_ctl_cmd(json!({ "cmd": "set_effects", "mirror": fx.mirror })).is_ok() {
                println!("Live webcam mirror set to: {}", fx.mirror);
            } else {
                save_prefs(&q, fx);
                println!("Saved webcam mirror to config: {}", fx.mirror);
            }
            0
        }
        "--flip-v" => {
            let (q, mut fx) = load_prefs();
            let val = match number {
                Some(1) => true,
                Some(0) => false,
                _ => !fx.flip_vertical,
            };
            fx.flip_vertical = val;
            if send_ctl_cmd(json!({ "cmd": "set_effects", "flipVertical": fx.flip_vertical }))
                .is_ok()
            {
                println!("Live webcam vertical flip set to: {}", fx.flip_vertical);
            } else {
                save_prefs(&q, fx);
                println!("Saved webcam vertical flip to config: {}", fx.flip_vertical);
            }
            0
        }
        "--brightness" => {
            let (q, mut fx) = load_prefs();
            let b_val = number.unwrap_or(0) as f64 / 100.0;
            fx.brightness = b_val.clamp(-1.0, 1.0);
            if send_ctl_cmd(json!({ "cmd": "set_effects", "brightness": fx.brightness })).is_ok() {
                println!("Live webcam brightness set to: {:.2}", fx.brightness);
            } else {
                save_prefs(&q, fx);
                println!("Saved webcam brightness to config: {:.2}", fx.brightness);
            }
            0
        }
        "--contrast" => {
            let (q, mut fx) = load_prefs();
            let c_val = number.unwrap_or(100) as f64 / 100.0;
            fx.contrast = c_val.clamp(0.0, 2.0);
            if send_ctl_cmd(json!({ "cmd": "set_effects", "contrast": fx.contrast })).is_ok() {
                println!("Live webcam contrast set to: {:.2}", fx.contrast);
            } else {
                save_prefs(&q, fx);
                println!("Saved webcam contrast to config: {:.2}", fx.contrast);
            }
            0
        }
        "--status" => {
            if let Ok(st) = send_ctl_cmd(json!({ "cmd": "get_status" })) {
                println!("Session:     Active (running)");
                println!(
                    "Quality:     {}",
                    st.get("quality").and_then(Value::as_str).unwrap_or("?")
                );
                println!(
                    "Size:        {}",
                    st.get("size").and_then(Value::as_str).unwrap_or("?")
                );
                println!(
                    "Frames:      {}",
                    st.get("frames").and_then(Value::as_u64).unwrap_or(0)
                );
                println!(
                    "Mirror:      {}",
                    st.get("mirror").and_then(Value::as_bool).unwrap_or(false)
                );
                println!(
                    "Flip V:      {}",
                    st.get("flipVertical")
                        .and_then(Value::as_bool)
                        .unwrap_or(false)
                );
                println!(
                    "Brightness:  {:.0}%",
                    st.get("brightness").and_then(Value::as_f64).unwrap_or(0.0) * 100.0
                );
                println!(
                    "Contrast:    {:.0}%",
                    st.get("contrast").and_then(Value::as_f64).unwrap_or(1.0) * 100.0
                );
            } else {
                let (q, fx) = load_prefs();
                println!("Session:     Idle (no webcam session running)");
                println!("Config:      {}", webcam_prefs_path().display());
                println!("Quality:     {q}");
                println!("Mirror:      {}", fx.mirror);
                println!("Flip V:      {}", fx.flip_vertical);
                println!("Brightness:  {:.0}%", fx.brightness * 100.0);
                println!("Contrast:    {:.0}%", fx.contrast * 100.0);
            }
            0
        }
        other => bail!("unknown option {other}\n{USAGE}"),
    };
    std::process::exit(code);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_pick_decoder_instantiable() {
        let _ = gst::init();
        let dec = pick_decoder();
        assert!(!dec.is_empty());
        assert!(
            gst::parse::bin_from_description(
                &format!("queue ! h264parse ! {dec} ! fakesink"),
                true
            )
            .is_ok(),
            "pick_decoder() '{dec}' failed element construction"
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
            assert_eq!(auto_dec, "nvh264dec ! cudadownload");
        } else {
            assert!(auto_dec.starts_with("avdec_h264"));
        }

        // 3. Fallback when nvdec requested but unavailable
        let nv_dec = pick_decoder_from(Some("nvdec"));
        if ok("nvh264dec") && ok("cudadownload") {
            assert_eq!(nv_dec, "nvh264dec ! cudadownload");
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
    fn test_parse_size() {
        assert_eq!(parse_size("1280x720").unwrap(), (1280, 720));
        assert_eq!(parse_size("1920x1080").unwrap(), (1920, 1080));
        assert!(parse_size("invalid").is_err());
        assert!(parse_size("0x0").is_err());
        assert!(parse_size("-100x100").is_err());
        assert!(parse_size("99999x99999").is_err());
    }
}
