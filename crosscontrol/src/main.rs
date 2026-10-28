//! smartconnect-crosscontrol: Cross Control (Motorola "Universal Control", svc 26).
//!
//! Pushing the PC pointer into one screen edge starts a svc 26 session (cmd 1 trigger:1 → phone
//! cmd 2 with width/height); from then on relative motion drives a virtual cursor in phone pixels
//! and buttons, wheel and keys go to the phone over svc 16 with displayId 0. Pushing back past the
//! shared edge (or Ctrl+Alt+Esc) returns control to the PC.
//!
//! Two ways to grab input, picked by `--backend` (default: whichever the desktop supports):
//! - `layer-shell`: a 1-px wlr-layer-shell strip plus pointer lock (niri, sway, Hyprland, KDE).
//! - `portal`: the xdg-desktop-portal InputCapture barrier plus libei (GNOME).
//!
//! See docs/protocol/cross-control.md.

mod core;
mod layer;
mod portal;

use std::io::{BufRead, BufReader, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use crate::core::{Backend, Core, Edge, Opts};

static STOP: AtomicBool = AtomicBool::new(false);

extern "C" fn on_signal(_: libc::c_int) {
    STOP.store(true, Ordering::SeqCst);
}

fn socket_path() -> PathBuf {
    runtime_dir().join("smartconnectd.sock")
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

/// The phone's place next to the screen, shared with the GTK app (`crosscontrol.json` under
/// `$XDG_CONFIG_HOME/smartconnect-linux/`): `{"edge":"right","start":0.2,"end":0.8}`, where start and
/// end are fractions along that edge (from the top or the left). Command-line flags override it.
pub fn config_path() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".config"))
        .join("smartconnect-linux/crosscontrol.json")
}

fn parse_edge(s: &str) -> Option<Edge> {
    Some(match s {
        "left" => Edge::Left,
        "right" => Edge::Right,
        "top" => Edge::Top,
        "bottom" => Edge::Bottom,
        _ => return None,
    })
}

/// `START:END` (fractions, 0 ≤ START < END ≤ 1).
fn parse_span(s: &str) -> Option<(f64, f64)> {
    let (a, b) = s.split_once(':')?;
    let (a, b): (f64, f64) = (a.trim().parse().ok()?, b.trim().parse().ok()?);
    (0.0..=1.0).contains(&a).then_some(())?;
    (a < b && b <= 1.0).then_some((a, b))
}

fn apply_config(o: &mut Opts, v: &Value) {
    if let Some(edge) = v.get("edge").and_then(Value::as_str).and_then(parse_edge) {
        o.edge = edge;
    }
    if let (Some(a), Some(b)) = (v.get("start").and_then(Value::as_f64), v.get("end").and_then(Value::as_f64))
        && let Some(span) = parse_span(&format!("{a}:{b}"))
    {
        o.span = span;
    }
}

fn parse_args() -> Result<Opts> {
    let mut o = Opts {
        edge: Edge::Right,
        span: (0.0, 1.0),
        output: None,
        gain: 1.6,
        exit_px: 12.0,
        in_use_cap: true,
        accept: false,
        backend: Backend::Auto,
    };
    if let Some(v) = std::fs::read_to_string(config_path()).ok().and_then(|t| serde_json::from_str::<Value>(&t).ok()) {
        apply_config(&mut o, &v);
    }
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--edge" => {
                let v = args.next();
                o.edge = v.as_deref().and_then(parse_edge).with_context(|| format!("--edge left|right|top|bottom (got {v:?})"))?;
            }
            "--span" => {
                let v = args.next();
                o.span = v.as_deref().and_then(parse_span).with_context(|| format!("--span START:END, fractions 0..1 (got {v:?})"))?;
            }
            "--backend" => {
                o.backend = match args.next().as_deref() {
                    Some("auto") => Backend::Auto,
                    Some("layer-shell") => Backend::LayerShell,
                    Some("portal") => Backend::Portal,
                    v => bail!("--backend auto|layer-shell|portal (got {v:?})"),
                }
            }
            "--output" => o.output = args.next(),
            "--gain" => o.gain = args.next().context("--gain N")?.parse()?,
            "--exit-px" => o.exit_px = args.next().context("--exit-px N")?.parse()?,
            "--no-in-use-cap" => o.in_use_cap = false,
            "--accept" => o.accept = true,
            "-h" | "--help" => {
                println!(
                    "smartconnect-crosscontrol [--edge left|right|top|bottom] [--span START:END] [--backend auto|layer-shell|portal]\n\
                     \x20                         [--output NAME] [--gain 1.6] [--exit-px 12] [--no-in-use-cap] [--accept]\n\
                     Push the pointer into the chosen screen edge to control the phone; push back (or Ctrl+Alt+Esc) to return.\n\
                     --edge/--span: where the phone sits (span = part of the edge, fractions from the top/left; default 0:1).\n\
                     Defaults come from {} (the GTK app's Cross Control page writes it).\n\
                     --backend: layer-shell = edge strip + pointer lock (niri, sway, Hyprland, KDE); portal = InputCapture\n\
                     portal (GNOME; asks once for permission); auto (default) = layer-shell when available, else portal.\n\
                     --output only applies to layer-shell (the portal puts a barrier on that edge of every monitor).",
                    config_path().display()
                );
                std::process::exit(0);
            }
            other => bail!("unknown argument {other}"),
        }
    }
    Ok(o)
}

/// Talks to smartconnectd: one socket for `watch`, one for `send` (replies drained by a thread).
pub struct Daemon {
    cmd: UnixStream,
    pub watch: UnixStream,
    buf: Vec<u8>,
}

impl Daemon {
    fn connect() -> Result<Self> {
        let watch = UnixStream::connect(socket_path()).context("is smartconnectd running?")?;
        let cmd = UnixStream::connect(socket_path())?;
        (&watch).write_all(b"{\"cmd\":\"watch\",\"services\":[26,16,25]}\n")?;
        watch.set_nonblocking(true)?;
        let drain = cmd.try_clone()?;
        std::thread::spawn(move || {
            for line in BufReader::new(drain).lines().map_while(Result::ok) {
                if line.contains("\"ok\":false") {
                    eprintln!("daemon: {line}");
                }
            }
        });
        Ok(Self { cmd, watch, buf: Vec::new() })
    }

    pub fn send(&mut self, service: i64, data: Value) {
        let line = json!({ "cmd": "send", "service": service, "data": data }).to_string() + "\n";
        if let Err(e) = self.cmd.write_all(line.as_bytes()) {
            eprintln!("daemon write failed: {e}");
        }
    }

    pub fn input(&mut self, cmd: i64, data: Value) {
        self.send(16, json!({ "cmd": cmd, "data": data, "messageId": 0, "instanceId": 0 }));
    }

    /// Reads whatever is available; returns complete JSON lines. `Err` = daemon gone.
    pub fn poll_lines(&mut self) -> Result<Vec<Value>> {
        let mut tmp = [0u8; 8192];
        loop {
            match self.watch.read(&mut tmp) {
                Ok(0) => bail!("daemon closed the watch socket"),
                Ok(n) => self.buf.extend_from_slice(&tmp[..n]),
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(e) => return Err(e.into()),
            }
        }
        let mut out = Vec::new();
        while let Some(pos) = self.buf.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = self.buf.drain(..=pos).collect();
            if let Ok(v) = serde_json::from_slice::<Value>(&line) {
                out.push(v);
            }
        }
        Ok(out)
    }
}

/// One client at a time: holds an exclusive flock on `$XDG_RUNTIME_DIR/smartconnect-crosscontrol.lock`
/// (smartconnectd checks it before starting one for the phone). A newer instance takes over: it
/// SIGINTs the holder (which tells the phone to stop) and waits for the lock.
fn single_instance() -> Result<std::fs::File> {
    let path = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("smartconnect-crosscontrol.lock");
    let mut f = std::fs::OpenOptions::new().read(true).write(true).create(true).truncate(false).open(&path)?;
    let try_lock = |f: &std::fs::File| unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0;
    if !try_lock(&f) {
        let mut pid = String::new();
        let _ = f.read_to_string(&mut pid);
        if let Ok(pid) = pid.trim().parse::<libc::pid_t>() {
            println!("taking over from running instance (pid {pid})");
            unsafe { libc::kill(pid, libc::SIGINT) };
        }
        let deadline = Instant::now() + Duration::from_secs(3);
        while !try_lock(&f) {
            if Instant::now() > deadline {
                bail!("another smartconnect-crosscontrol is running and didn't stop ({})", path.display());
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
    f.set_len(0)?;
    write!(f, "{}", std::process::id())?;
    Ok(f)
}

fn main() -> Result<()> {
    let opts = parse_args()?;
    let _lock = single_instance()?;
    unsafe {
        libc::signal(libc::SIGINT, on_signal as *const () as usize);
        libc::signal(libc::SIGTERM, on_signal as *const () as usize);
    }
    let backend = match opts.backend {
        Backend::Auto if layer::available() => Backend::LayerShell,
        Backend::Auto => Backend::Portal,
        b => b,
    };
    println!("backend: {}", if backend == Backend::Portal { "InputCapture portal" } else { "layer-shell" });
    println!("phone on the {:?} edge, span {:.2}..{:.2}", opts.edge, opts.span.0, opts.span.1);
    let core = Core::new(opts, Daemon::connect()?);
    match backend {
        Backend::Portal => portal::run(core),
        _ => layer::run(core),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spans() {
        assert_eq!(parse_span("0.25:0.75"), Some((0.25, 0.75)));
        assert_eq!(parse_span("0:1"), Some((0.0, 1.0)));
        assert_eq!(parse_span("0.8:0.2"), None);
        assert_eq!(parse_span("0.5:1.2"), None);
        assert_eq!(parse_span("half"), None);
    }

    #[test]
    fn config_overrides_defaults() {
        let mut o = Opts { edge: Edge::Right, span: (0.0, 1.0), output: None, gain: 1.6, exit_px: 12.0, in_use_cap: true, accept: false, backend: Backend::Auto };
        apply_config(&mut o, &json!({"edge": "left", "start": 0.1, "end": 0.6}));
        assert_eq!(o.edge, Edge::Left);
        assert_eq!(o.span, (0.1, 0.6));
        apply_config(&mut o, &json!({"edge": "sideways", "start": 0.9, "end": 0.2}));
        assert_eq!((o.edge, o.span), (Edge::Left, (0.1, 0.6)), "bad values are ignored");
    }
}
