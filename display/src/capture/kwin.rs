//! KDE Plasma: KWin's own screencast protocol (`zkde_screencast_unstable_v1`), which streams an
//! output to PipeWire with no picker and can create a virtual output just for the stream; input
//! goes through `org_kde_kwin_fake_input` (`input/kwin.rs`).
//!
//! - Extend: `stream_virtual_output(_with_description)` makes a virtual output that lives as long
//!   as the stream (KWin removes it when we close the stream or this process dies). Its size is
//!   given in logical px plus a scale, so we ask for (W/scale)x(H/scale) at `scale`. KWin places it
//!   in the layout itself: the side chosen in `desktop::Placement` is not applied on KDE yet. Resizing re-creates it (its windows move back to another screen).
//! - Mirror: `stream_output` of the primary output.
//! - KWin offers both protocols only to trusted programs: a `.desktop` file with this binary's
//!   absolute path in `Exec` and the interfaces in `X-KDE-Wayland-Interfaces`
//!   (`packaging/smartconnect-display.desktop`). Without it the globals are simply missing, the
//!   error says how to fix it, and Mirror falls back to the portal picker.
//! - The Wayland connection lives in a thread that keeps reading events (KWin would drop a client
//!   whose socket fills up) until the capture is dropped.
//!
//! Not verified on a real Plasma session yet.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use wayland_client::{
    Connection, Dispatch, Proxy, QueueHandle, WEnum,
    globals::{GlobalListContents, registry_queue_init},
    protocol::{
        wl_output::{self, WlOutput},
        wl_registry::{self, WlRegistry},
    },
};
use wayland_protocols::xdg::xdg_output::zv1::client::{
    zxdg_output_manager_v1::ZxdgOutputManagerV1,
    zxdg_output_v1::{self, ZxdgOutputV1},
};
use wayland_protocols_plasma::screencast::v1::client::{
    zkde_screencast_stream_unstable_v1::{self, ZkdeScreencastStreamUnstableV1},
    zkde_screencast_unstable_v1::ZkdeScreencastUnstableV1,
};

use super::screencopy::dispatch;
use super::{Backend, Mode, SHM_CONVERT};
use crate::desktop;
use crate::input::{Route, kwin_trust_hint};

/// zkde_screencast `pointer` mode: draw the cursor into the stream.
const POINTER_EMBEDDED: u32 = 2;
/// Name we give the virtual output.
const VIRTUAL_NAME: &str = "smartconnect";

#[derive(Clone)]
enum Request {
    Output(String),
    Virtual { width: u32, height: u32, scale: f64 },
}

pub struct KwinCapture {
    request: Request,
    node_id: u32,
    size: (u32, u32),
    shared: Arc<Mutex<Shared>>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

#[derive(Default)]
struct Shared {
    /// Name and logical rect of the streamed output (the virtual one once KWin announces it).
    output: String,
    geometry: (i32, i32, u32, u32),
}

impl Backend for KwinCapture {
    fn source_description(&self) -> String {
        format!(
            "pipewiresrc path={} do-timestamp=true keepalive-time=1000 ! {SHM_CONVERT}",
            self.node_id
        )
    }

    fn size(&self) -> (u32, u32) {
        self.size
    }

    fn geometry(&self) -> (i32, i32, u32, u32) {
        self.shared.lock().unwrap().geometry
    }

    fn output_name(&self) -> String {
        self.shared.lock().unwrap().output.clone()
    }

    fn is_extend(&self) -> bool {
        matches!(self.request, Request::Virtual { .. })
    }

    fn reconfigure_extend(&mut self, width: u32, height: u32) -> Result<()> {
        let Request::Virtual { scale, .. } = self.request else {
            bail!("only an extend capture can be resized")
        };
        // Same name: the old output has to go first.
        self.shutdown();
        *self = start_request(Request::Virtual {
            width,
            height,
            scale,
        })?;
        Ok(())
    }

    fn route(&self) -> Route {
        Route::Kwin
    }
}

impl KwinCapture {
    fn shutdown(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for KwinCapture {
    fn drop(&mut self) {
        self.shutdown();
    }
}

pub fn start(mode: &Mode) -> Result<KwinCapture> {
    let request = match mode {
        Mode::Mirror => {
            let outs = desktop::wl_outputs()?;
            Request::Output(
                desktop::primary(&outs, None)
                    .ok_or_else(|| anyhow!("no enabled output to mirror"))?
                    .name
                    .clone(),
            )
        }
        Mode::Extend {
            output: Some(name), ..
        } => Request::Output(name.clone()),
        Mode::Extend {
            width,
            height,
            output: None,
            scale,
        } => Request::Virtual {
            width: *width,
            height: *height,
            scale: scale.unwrap_or(1.0),
        },
    };
    start_request(request)
}

fn start_request(request: Request) -> Result<KwinCapture> {
    let stop = Arc::new(AtomicBool::new(false));
    let shared = Arc::new(Mutex::new(Shared::default()));
    let (tx, rx) = std::sync::mpsc::channel::<Result<(u32, (u32, u32))>>();
    let (req, stop_c, shared_c) = (request.clone(), stop.clone(), shared.clone());
    let thread = std::thread::Builder::new()
        .name("kwin-screencast".into())
        .spawn(move || {
            let tx2 = tx.clone();
            if let Err(e) = run(req, &stop_c, &shared_c, tx) {
                let _ = tx2.send(Err(e));
            }
        })
        .context("starting the KWin screencast thread")?;
    let mut cap = KwinCapture {
        request,
        node_id: 0,
        size: (0, 0),
        shared,
        stop,
        thread: Some(thread),
    };
    match rx.recv_timeout(Duration::from_secs(5)) {
        Ok(Ok((node, size))) => {
            cap.node_id = node;
            cap.size = size;
        }
        Ok(Err(e)) => return Err(e),
        Err(_) => bail!("KWin did not start the screencast within 5 s"),
    }
    if let Request::Virtual { .. } = cap.request {
        // KWin announces the new output right after the stream; wait briefly for its geometry.
        let deadline = Instant::now() + Duration::from_secs(2);
        while cap.shared.lock().unwrap().geometry.2 == 0 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
    }
    Ok(cap)
}

// ---------------------------------------------------------------------------------------------
// The Wayland thread

#[derive(Default)]
struct OutInfo {
    name: Option<String>,
    rect: (i32, i32, u32, u32),
    /// Current mode in pixels.
    mode: (u32, u32),
}

#[derive(Default)]
struct State {
    outputs: Vec<(WlOutput, OutInfo)>,
    xdg: Option<ZxdgOutputManagerV1>,
    node: Option<u32>,
    failed: Option<String>,
    closed: bool,
}

fn run(
    request: Request,
    stop: &AtomicBool,
    shared: &Mutex<Shared>,
    started: std::sync::mpsc::Sender<Result<(u32, (u32, u32))>>,
) -> Result<()> {
    let conn = Connection::connect_to_env().context("connect to Wayland")?;
    let (globals, mut queue) = registry_queue_init::<State>(&conn).context("Wayland registry")?;
    let qh = queue.handle();
    let mut state = State::default();
    let mgr: ZkdeScreencastUnstableV1 = globals.bind(&qh, 1..=4, ()).with_context(|| {
        format!(
            "KWin did not offer zkde_screencast_unstable_v1. {}",
            kwin_trust_hint()
        )
    })?;
    state.xdg = globals.bind(&qh, 1..=3, ()).ok();
    for g in globals
        .contents()
        .clone_list()
        .into_iter()
        .filter(|g| g.interface == "wl_output")
    {
        bind_output(&mut state, globals.registry(), g.name, g.version, &qh);
    }
    queue.roundtrip(&mut state)?;
    queue.roundtrip(&mut state)?;
    let before: Vec<String> = state
        .outputs
        .iter()
        .filter_map(|(_, o)| o.name.clone())
        .collect();

    let (stream, size) = match &request {
        Request::Output(name) => {
            let (wl, info) = state
                .outputs
                .iter()
                .find(|(_, o)| o.name.as_deref() == Some(name))
                .ok_or_else(|| {
                    anyhow!(
                        "output {name:?} is not connected (KWin sees: {})",
                        before.join(", ")
                    )
                })?;
            *shared.lock().unwrap() = Shared {
                output: name.clone(),
                geometry: info.rect,
            };
            (mgr.stream_output(wl, POINTER_EMBEDDED, &qh, ()), info.mode)
        }
        Request::Virtual {
            width,
            height,
            scale,
        } => {
            let s = scale.max(0.25);
            let (lw, lh) = (
                (f64::from(*width) / s).round() as i32,
                (f64::from(*height) / s).round() as i32,
            );
            let stream = if mgr.version() >= 4 {
                mgr.stream_virtual_output_with_description(
                    VIRTUAL_NAME.into(),
                    "Phone (Smart Connect)".into(),
                    lw,
                    lh,
                    s,
                    POINTER_EMBEDDED,
                    &qh,
                    (),
                )
            } else if mgr.version() >= 2 {
                mgr.stream_virtual_output(VIRTUAL_NAME.into(), lw, lh, s, POINTER_EMBEDDED, &qh, ())
            } else {
                bail!(
                    "this KWin is too old for virtual outputs (zkde_screencast v{})",
                    mgr.version()
                );
            };
            let (x, y) = desktop::spot(
                &desktop::wl_outputs().unwrap_or_default(),
                None,
                (lw as u32, lh as u32),
            );
            *shared.lock().unwrap() = Shared {
                output: "virtual output".into(),
                geometry: (x, y, lw as u32, lh as u32),
            };
            (stream, (*width, *height))
        }
    };
    let deadline = Instant::now() + Duration::from_secs(5);
    while state.node.is_none() && state.failed.is_none() && Instant::now() < deadline {
        dispatch(&conn, &mut queue, &mut state, Duration::from_millis(100))?;
    }
    match (state.node, state.failed.take()) {
        (Some(node), _) => {
            let _ = started.send(Ok((node, size)));
        }
        (None, Some(e)) => bail!("KWin refused the screencast: {e}"),
        (None, None) => bail!("KWin did not start the screencast within 5 s"),
    }

    let mut announced = !matches!(request, Request::Virtual { .. });
    while !stop.load(Ordering::Relaxed) && !state.closed {
        dispatch(&conn, &mut queue, &mut state, Duration::from_millis(200))?;
        if !announced
            && let Some((_, o)) = state
                .outputs
                .iter()
                .find(|(_, o)| o.name.as_ref().is_some_and(|n| !before.contains(n)) && o.rect.2 > 0)
        {
            let name = o.name.clone().unwrap_or_default();
            println!("{name}: KWin virtual output at {:?}", o.rect);
            *shared.lock().unwrap() = Shared {
                output: name,
                geometry: o.rect,
            };
            announced = true;
        }
    }
    if state.closed {
        eprintln!("capture: KWin closed the screencast");
    }
    stream.close();
    let _ = conn.flush();
    Ok(())
}

fn bind_output(
    state: &mut State,
    registry: &WlRegistry,
    name: u32,
    version: u32,
    qh: &QueueHandle<State>,
) {
    let idx = state.outputs.len();
    let out: WlOutput = registry.bind(name, version.min(4), qh, idx);
    if let Some(m) = &state.xdg {
        m.get_xdg_output(&out, qh, idx);
    }
    state.outputs.push((out, OutInfo::default()));
}

impl Dispatch<WlRegistry, GlobalListContents> for State {
    fn event(
        s: &mut Self,
        reg: &WlRegistry,
        ev: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        // A virtual output appears as a new wl_output global.
        if let wl_registry::Event::Global {
            name,
            interface,
            version,
        } = ev
            && interface == "wl_output"
        {
            bind_output(s, reg, name, version, qh);
        }
    }
}

impl Dispatch<ZkdeScreencastStreamUnstableV1, ()> for State {
    fn event(
        s: &mut Self,
        _: &ZkdeScreencastStreamUnstableV1,
        ev: zkde_screencast_stream_unstable_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use zkde_screencast_stream_unstable_v1::Event;
        match ev {
            Event::Created { node } => s.node = Some(node),
            Event::Failed { error } => s.failed = Some(error),
            Event::Closed => s.closed = true,
            _ => {}
        }
    }
}

impl Dispatch<WlOutput, usize> for State {
    fn event(
        s: &mut Self,
        _: &WlOutput,
        ev: wl_output::Event,
        i: &usize,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let o = &mut s.outputs[*i].1;
        match ev {
            wl_output::Event::Name { name } => o.name = Some(name),
            wl_output::Event::Mode {
                flags: WEnum::Value(f),
                width,
                height,
                ..
            } if f.contains(wl_output::Mode::Current) => {
                o.mode = (width.max(0) as u32, height.max(0) as u32);
            }
            _ => {}
        }
    }
}

impl Dispatch<ZxdgOutputV1, usize> for State {
    fn event(
        s: &mut Self,
        _: &ZxdgOutputV1,
        ev: zxdg_output_v1::Event,
        i: &usize,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let o = &mut s.outputs[*i].1;
        match ev {
            zxdg_output_v1::Event::LogicalPosition { x, y } => (o.rect.0, o.rect.1) = (x, y),
            zxdg_output_v1::Event::LogicalSize { width, height } => {
                (o.rect.2, o.rect.3) = (width.max(0) as u32, height.max(0) as u32)
            }
            zxdg_output_v1::Event::Name { name } => {
                o.name.get_or_insert(name);
            }
            _ => {}
        }
    }
}

macro_rules! ignore_events {
    ($($t:ty),*) => {$(
        impl Dispatch<$t, ()> for State {
            fn event(_: &mut Self, _: &$t, _: <$t as Proxy>::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
        }
    )*};
}
ignore_events!(ZkdeScreencastUnstableV1, ZxdgOutputManagerV1);
