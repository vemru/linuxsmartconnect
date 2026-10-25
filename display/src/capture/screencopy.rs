//! wlr-screencopy (`zwlr_screencopy_manager_v1`) → GStreamer `appsrc`, for compositors without a
//! no-picker PipeWire path (Hyprland, sway, other wlroots compositors). One thread per capture, with
//! its own Wayland connection: it asks for a frame of the output (cursor included), copies it into
//! a shared-memory buffer, and pushes the pixels to the appsrc of the source bin the session built
//! from `source_description()` (handed over with `bind`).
//!
//! - Frames come with damage (`copy_with_damage`, v2+): an idle screen costs nothing, and at most
//!   one frame per `FRAME_INTERVAL` is taken. The first frame of each size is copied at once. While nothing changes, the last frame is pushed again
//!   every `KEEPALIVE` so the encoder keeps producing keyframes for a phone that just connected.
//! - The size and format come from each frame's `buffer` event, so an output that is resized while
//!   it is captured changes the appsrc caps on the fly.
//! - Requesting frames also makes the compositor render the output: Hyprland applies a headless
//!   output's new mode only on its next frame.

use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use gst::prelude::*;
use wayland_client::{
    Connection, Dispatch, EventQueue, Proxy, QueueHandle, WEnum,
    backend::WaylandError,
    globals::{GlobalListContents, registry_queue_init},
    protocol::{
        wl_buffer::WlBuffer,
        wl_output::{self, WlOutput},
        wl_registry::WlRegistry,
        wl_shm::{self, WlShm},
        wl_shm_pool::WlShmPool,
    },
};
use wayland_protocols::xdg::xdg_output::zv1::client::{
    zxdg_output_manager_v1::ZxdgOutputManagerV1,
    zxdg_output_v1::{self, ZxdgOutputV1},
};
use wayland_protocols_wlr::screencopy::v1::client::{
    zwlr_screencopy_frame_v1::{self, ZwlrScreencopyFrameV1},
    zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1,
};

use super::Backend;
use crate::desktop::Output;

/// Shortest time between two captured frames (≈ 60 fps).
const FRAME_INTERVAL: Duration = Duration::from_millis(16);
/// Re-push the last frame this often while the screen is idle.
const KEEPALIVE: Duration = Duration::from_secs(1);

/// The capture thread for one output.
pub struct Feeder {
    shared: Arc<Shared>,
    thread: Option<std::thread::JoinHandle<()>>,
    /// Name of the appsrc element in `source_description()`.
    element: String,
}

#[derive(Default)]
struct Shared {
    stop: AtomicBool,
    appsrc: Mutex<Option<gst::Element>>,
    /// (gst format, width, height) of the last frame.
    format: Mutex<Option<(&'static str, u32, u32)>>,
}

impl Feeder {
    /// Starts capturing output `name` and waits for its first frame description.
    pub fn start(name: &str) -> Result<Feeder> {
        gst::init()?; // buffers are made in the capture thread, possibly before the session's init
        let shared = Arc::new(Shared::default());
        let (tx, rx) = std::sync::mpsc::channel::<Result<()>>();
        let (name_c, shared_c) = (name.to_owned(), shared.clone());
        let thread = std::thread::Builder::new()
            .name("screencopy".into())
            .spawn(move || {
                let tx2 = tx.clone();
                if let Err(e) = run(&name_c, &shared_c, tx) {
                    eprintln!("capture: screencopy of {name_c} stopped: {e:#}");
                    let _ = tx2.send(Err(e));
                }
            })
            .context("starting the screencopy thread")?;
        let element = format!("screencopy{}", std::process::id());
        let feeder = Feeder { shared, thread: Some(thread), element };
        match rx.recv_timeout(Duration::from_secs(5)) {
            Ok(Ok(())) => Ok(feeder),
            Ok(Err(e)) => Err(e),
            Err(_) => bail!("no frame from {name} within 5 s (wlr-screencopy)"),
        }
    }

    /// Pixel size of the last frame.
    pub fn size(&self) -> Option<(u32, u32)> {
        self.shared.format.lock().unwrap().map(|(_, w, h)| (w, h))
    }

    /// Waits until frames come in at `size` (after a mode change).
    pub fn wait_for_size(&self, size: (u32, u32), timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if self.size() == Some(size) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        false
    }

    pub fn source_description(&self) -> String {
        let (format, w, h) = self.shared.format.lock().unwrap().unwrap_or(("BGRx", 1920, 1080));
        format!(
            "appsrc name={} is-live=true do-timestamp=true format=time max-buffers=2 leaky-type=downstream \
             caps=video/x-raw,format={format},width={w},height={h},framerate=0/1 ! videoconvert",
            self.element
        )
    }

    pub fn bind(&self, bin: &gst::Element) {
        let found = bin.downcast_ref::<gst::Bin>().and_then(|b| b.by_name(&self.element));
        if found.is_none() {
            eprintln!("capture: no appsrc {} in the source bin", self.element);
        }
        *self.shared.appsrc.lock().unwrap() = found;
    }
}

impl Drop for Feeder {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Relaxed);
        *self.shared.appsrc.lock().unwrap() = None;
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Mirror (or `--output` on an unknown desktop): an existing output, captured as it is.
pub struct Plain {
    feeder: Feeder,
    output: String,
    geometry: (i32, i32, u32, u32),
}

impl Plain {
    pub fn start(out: &Output) -> Result<Plain> {
        let feeder = Feeder::start(&out.name)?;
        Ok(Plain { feeder, output: out.name.clone(), geometry: out.logical.unwrap_or_default() })
    }
}

impl Backend for Plain {
    fn source_description(&self) -> String {
        self.feeder.source_description()
    }

    fn size(&self) -> (u32, u32) {
        self.feeder.size().unwrap_or((self.geometry.2, self.geometry.3))
    }

    fn geometry(&self) -> (i32, i32, u32, u32) {
        self.geometry
    }

    fn output_name(&self) -> String {
        self.output.clone()
    }

    fn is_extend(&self) -> bool {
        false
    }

    fn reconfigure_extend(&mut self, _: u32, _: u32) -> Result<()> {
        bail!("this output is captured as it is and cannot be resized")
    }

    fn bind(&self, bin: &gst::Element) {
        self.feeder.bind(bin);
    }
}

// ---------------------------------------------------------------------------------------------
// The capture thread

fn gst_format(f: wl_shm::Format) -> Option<&'static str> {
    // wl_shm formats are little-endian words: ARGB8888 is B, G, R, A in memory.
    Some(match f {
        wl_shm::Format::Argb8888 => "BGRA",
        wl_shm::Format::Xrgb8888 => "BGRx",
        wl_shm::Format::Abgr8888 => "RGBA",
        wl_shm::Format::Xbgr8888 => "RGBx",
        _ => return None,
    })
}

#[derive(Default)]
struct Frame {
    /// (format, width, height, stride) from the `buffer` event.
    shm: Option<(wl_shm::Format, u32, u32, u32)>,
    buffer_done: bool,
    y_invert: bool,
    ready: bool,
    failed: bool,
}

#[derive(Default)]
struct State {
    outputs: Vec<(WlOutput, Option<String>)>,
    frame: Frame,
}

/// A shared-memory wl_buffer and its mapping.
struct ShmBuf {
    key: (wl_shm::Format, u32, u32, u32),
    _fd: OwnedFd,
    ptr: *mut libc::c_void,
    len: usize,
    pool: WlShmPool,
    buffer: WlBuffer,
}

impl ShmBuf {
    fn new(shm: &WlShm, qh: &QueueHandle<State>, key: (wl_shm::Format, u32, u32, u32)) -> Result<ShmBuf> {
        let (format, w, h, stride) = key;
        let len = (stride * h) as usize;
        // SAFETY: plain syscalls on a fresh fd that we own; the mapping is unmapped in Drop.
        let fd = unsafe {
            let raw = libc::memfd_create(c"smartconnect-screencopy".as_ptr(), libc::MFD_CLOEXEC);
            if raw < 0 {
                bail!("memfd_create: {}", std::io::Error::last_os_error());
            }
            OwnedFd::from_raw_fd(raw)
        };
        if unsafe { libc::ftruncate(fd.as_raw_fd(), len as libc::off_t) } < 0 {
            bail!("ftruncate: {}", std::io::Error::last_os_error());
        }
        let ptr = unsafe { libc::mmap(std::ptr::null_mut(), len, libc::PROT_READ | libc::PROT_WRITE, libc::MAP_SHARED, fd.as_raw_fd(), 0) };
        if ptr == libc::MAP_FAILED {
            bail!("mmap: {}", std::io::Error::last_os_error());
        }
        let pool = shm.create_pool(fd.as_fd(), len as i32, qh, ());
        let buffer = pool.create_buffer(0, w as i32, h as i32, stride as i32, format, qh, ());
        Ok(ShmBuf { key, _fd: fd, ptr, len, pool, buffer })
    }

    /// The frame as tightly packed rows (4 bytes per pixel), flipped when the compositor says so.
    fn to_gst(&self, y_invert: bool) -> Result<gst::Buffer> {
        let (_, w, h, stride) = self.key;
        let (row, stride, h) = (w as usize * 4, stride as usize, h as usize);
        // SAFETY: the mapping is `len` bytes long and stays mapped while `self` lives.
        let src = unsafe { std::slice::from_raw_parts(self.ptr as *const u8, self.len) };
        let mut buf = gst::Buffer::with_size(row * h)?;
        {
            let mut map = buf.get_mut().context("new buffer")?.map_writable()?;
            for y in 0..h {
                let sy = if y_invert { h - 1 - y } else { y };
                map[y * row..(y + 1) * row].copy_from_slice(&src[sy * stride..sy * stride + row]);
            }
        }
        Ok(buf)
    }
}

impl Drop for ShmBuf {
    fn drop(&mut self) {
        self.buffer.destroy();
        self.pool.destroy();
        unsafe { libc::munmap(self.ptr, self.len) };
    }
}

// SAFETY: the mapping is only touched from the capture thread that owns the ShmBuf.
unsafe impl Send for ShmBuf {}

/// Reads and dispatches Wayland events, waiting at most `timeout` for some to arrive. A full send
/// buffer or a spurious wake-up (EAGAIN) is not an error: the next call tries again.
pub(super) fn dispatch<S>(conn: &Connection, queue: &mut EventQueue<S>, state: &mut S, timeout: Duration) -> Result<()> {
    let would_block = |e: &WaylandError| matches!(e, WaylandError::Io(io) if io.kind() == std::io::ErrorKind::WouldBlock);
    queue.dispatch_pending(state)?;
    if let Err(e) = conn.flush()
        && !would_block(&e)
    {
        return Err(e.into());
    }
    if let Some(guard) = queue.prepare_read() {
        let mut pfd = libc::pollfd { fd: guard.connection_fd().as_raw_fd(), events: libc::POLLIN, revents: 0 };
        // SAFETY: one valid pollfd.
        if unsafe { libc::poll(&mut pfd, 1, timeout.as_millis() as i32) } > 0
            && let Err(e) = guard.read()
            && !would_block(&e)
        {
            return Err(e.into());
        }
    }
    queue.dispatch_pending(state)?;
    Ok(())
}

fn push(shared: &Shared, buf: &gst::Buffer) {
    if let Some(src) = shared.appsrc.lock().unwrap().as_ref() {
        let _ = src.emit_by_name::<gst::FlowReturn>("push-buffer", &[buf]);
    }
}

/// While waiting on the compositor: re-push the last frame once per `KEEPALIVE`.
fn keepalive(shared: &Shared, last: &Option<gst::Buffer>, last_push: &mut Instant) {
    if last_push.elapsed() >= KEEPALIVE
        && let Some(l) = last
    {
        push(shared, l);
        *last_push = Instant::now();
    }
}

fn run(name: &str, shared: &Shared, ready: std::sync::mpsc::Sender<Result<()>>) -> Result<()> {
    let conn = Connection::connect_to_env().context("connect to Wayland")?;
    let (globals, mut queue) = registry_queue_init::<State>(&conn).context("Wayland registry")?;
    let qh = queue.handle();
    let mut state = State::default();
    let shm: WlShm = globals.bind(&qh, 1..=1, ()).context("wl_shm")?;
    let mgr: ZwlrScreencopyManagerV1 = globals
        .bind(&qh, 1..=3, ())
        .context("this compositor does not offer wlr-screencopy (zwlr_screencopy_manager_v1)")?;
    let xo: Option<ZxdgOutputManagerV1> = globals.bind(&qh, 1..=3, ()).ok();
    for g in globals.contents().clone_list().into_iter().filter(|g| g.interface == "wl_output") {
        let idx = state.outputs.len();
        let out: WlOutput = globals.registry().bind(g.name, g.version.min(4), &qh, idx);
        if let Some(m) = &xo {
            m.get_xdg_output(&out, &qh, idx);
        }
        state.outputs.push((out, None));
    }
    queue.roundtrip(&mut state)?;
    queue.roundtrip(&mut state)?;
    let output = state
        .outputs
        .iter()
        .find(|(_, n)| n.as_deref() == Some(name))
        .map(|(o, _)| o.clone())
        .ok_or_else(|| {
            let names: Vec<_> = state.outputs.iter().filter_map(|(_, n)| n.clone()).collect();
            anyhow!("no Wayland output named {name} (outputs: {})", names.join(", "))
        })?;

    let mut buf: Option<ShmBuf> = None;
    let mut last: Option<gst::Buffer> = None;
    let mut last_push = Instant::now();
    let mut ready = Some(ready);
    let mut failures = 0;
    // The first frame of a size is copied at once; later ones wait for damage.
    let mut immediate = true;
    while !shared.stop.load(Ordering::Relaxed) {
        let started = Instant::now();
        state.frame = Frame::default();
        let frame = mgr.capture_output(1, &output, &qh, ());
        // Buffer description: v3 ends it with buffer_done, older versions send only `buffer`.
        // (Hyprland sends it only when it next renders the output, which an idle one doesn't.)
        while !shared.stop.load(Ordering::Relaxed)
            && !state.frame.failed
            && !(state.frame.buffer_done || (mgr.version() < 3 && state.frame.shm.is_some()))
        {
            dispatch(&conn, &mut queue, &mut state, Duration::from_millis(100))?;
            keepalive(shared, &last, &mut last_push);
        }
        let Some(key) = state.frame.shm.filter(|_| !state.frame.failed) else {
            frame.destroy();
            failures += 1;
            if failures > 50 {
                bail!("the compositor keeps refusing frames of {name}");
            }
            std::thread::sleep(Duration::from_millis(100));
            continue;
        };
        let (format, w, h, _) = key;
        let gfmt = gst_format(format).ok_or_else(|| anyhow!("unsupported screencopy format {format:?}"))?;
        if buf.as_ref().is_none_or(|b| b.key != key) {
            buf = Some(ShmBuf::new(&shm, &qh, key)?);
            immediate = true;
            let caps = format!("video/x-raw,format={gfmt},width={w},height={h},framerate=0/1");
            if shared.format.lock().unwrap().replace((gfmt, w, h)).is_some() {
                // A resized output: tell the appsrc before the first buffer of the new size.
                if let (Some(src), Ok(c)) = (shared.appsrc.lock().unwrap().as_ref(), caps.parse::<gst::Caps>()) {
                    src.set_property("caps", &c);
                }
            }
            if let Some(tx) = ready.take() {
                let _ = tx.send(Ok(()));
            }
        }
        let b = buf.as_ref().expect("buffer");
        if mgr.version() >= 2 && !immediate {
            frame.copy_with_damage(&b.buffer);
        } else {
            frame.copy(&b.buffer);
        }
        while !shared.stop.load(Ordering::Relaxed) && !state.frame.ready && !state.frame.failed {
            dispatch(&conn, &mut queue, &mut state, Duration::from_millis(100))?;
            keepalive(shared, &last, &mut last_push);
        }
        if state.frame.ready {
            failures = 0;
            immediate = false;
            let g = b.to_gst(state.frame.y_invert)?;
            push(shared, &g);
            last = Some(g);
            last_push = Instant::now();
        }
        frame.destroy();
        if let Some(rest) = FRAME_INTERVAL.checked_sub(started.elapsed()) {
            std::thread::sleep(rest);
        }
    }
    let _ = conn.flush();
    Ok(())
}

impl Dispatch<ZwlrScreencopyFrameV1, ()> for State {
    fn event(s: &mut Self, _: &ZwlrScreencopyFrameV1, ev: zwlr_screencopy_frame_v1::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        use zwlr_screencopy_frame_v1::Event;
        match ev {
            Event::Buffer { format: WEnum::Value(format), width, height, stride } => {
                // Only the first offer counts (some compositors offer several).
                if s.frame.shm.is_none() {
                    s.frame.shm = Some((format, width, height, stride));
                }
            }
            Event::Flags { flags: WEnum::Value(f) } => s.frame.y_invert = f.contains(zwlr_screencopy_frame_v1::Flags::YInvert),
            Event::BufferDone => s.frame.buffer_done = true,
            Event::Ready { .. } => s.frame.ready = true,
            Event::Failed => s.frame.failed = true,
            _ => {}
        }
    }
}

impl Dispatch<WlOutput, usize> for State {
    fn event(s: &mut Self, _: &WlOutput, ev: wl_output::Event, i: &usize, _: &Connection, _: &QueueHandle<Self>) {
        if let wl_output::Event::Name { name } = ev {
            s.outputs[*i].1 = Some(name);
        }
    }
}

impl Dispatch<ZxdgOutputV1, usize> for State {
    fn event(s: &mut Self, _: &ZxdgOutputV1, ev: zxdg_output_v1::Event, i: &usize, _: &Connection, _: &QueueHandle<Self>) {
        if let zxdg_output_v1::Event::Name { name } = ev {
            s.outputs[*i].1.get_or_insert(name);
        }
    }
}

impl Dispatch<WlRegistry, GlobalListContents> for State {
    fn event(_: &mut Self, _: &WlRegistry, _: wayland_client::protocol::wl_registry::Event, _: &GlobalListContents, _: &Connection, _: &QueueHandle<Self>) {}
}

macro_rules! ignore_events {
    ($($t:ty),*) => {$(
        impl Dispatch<$t, ()> for State {
            fn event(_: &mut Self, _: &$t, _: <$t as Proxy>::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
        }
    )*};
}
ignore_events!(WlShm, WlShmPool, WlBuffer, ZwlrScreencopyManagerV1, ZxdgOutputManagerV1);
