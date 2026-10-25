//! The PC screen as a GStreamer source, on whichever desktop this is. Contract used by `session`:
//!
//! ```ignore
//! pub enum Mode {
//!     /// Record the focused (or primary) physical output.
//!     Mirror,
//!     /// Give the phone a screen of its own, `width`x`height` pixels at `scale` (e.g. 2.0 = 200%),
//!     /// on the side of the primary output the user chose (`desktop::Placement`), and record it. `output` names an existing connector to
//!     /// use instead (e.g. an HDMI dummy plug "HDMI-A-1"). Undone when the Capture is dropped.
//!     Extend { width: u32, height: u32, output: Option<String>, scale: Option<f64> },
//! }
//! pub struct Capture { pub width: u32, pub height: u32, /* keeps the cast session alive */ }
//! pub fn start(mode: Mode) -> anyhow::Result<Capture>;
//! impl Capture {
//!     /// gst-launch fragment producing system-memory raw video.
//!     pub fn source_description(&self) -> String;
//!     /// Call with the bin built from `source_description()` (screencopy feeds an appsrc in it).
//!     pub fn bind(&self, bin: &gst::Element);
//!     /// Screen rectangle (in compositor logical px) that the captured stream covers.
//!     pub fn geometry(&self) -> (i32, i32, u32, u32);
//!     /// Output (connector) name being captured, e.g. "eDP-1", "Virtual-1", "HEADLESS-2".
//!     pub fn output_name(&self) -> String;
//!     /// Where phone input goes and how it gets there on this desktop.
//!     pub fn input_target(&self) -> input::Target;
//!     /// Extend: new size (portrait when height > width) without removing the screen, where
//!     /// the desktop allows it; re-read source_description()/geometry() afterwards.
//!     pub fn reconfigure_extend(&mut self, width: u32, height: u32) -> anyhow::Result<()>;
//!     /// Extend: follow a new placement (`desktop::Placement`, the side the user chose).
//!     pub fn reposition(&mut self) -> anyhow::Result<bool>;
//! }
//! ```
//!
//! | Desktop  | Extend screen                                  | Pixels                         | Input           |
//! |----------|------------------------------------------------|--------------------------------|-----------------|
//! | niri     | spare output switched on (vkms `Virtual-1`)    | Mutter ScreenCast API (niri)   | virtual pointer |
//! | GNOME    | Mutter virtual monitor (`RecordVirtual`)       | Mutter ScreenCast API          | Mutter RemoteDesktop |
//! | KDE      | KWin virtual output (`zkde_screencast`)        | KWin screencast → PipeWire     | KWin fake input |
//! | Hyprland | `hyprctl output create headless`               | wlr-screencopy → appsrc        | virtual pointer |
//! | sway     | `swaymsg create_output`                        | wlr-screencopy → appsrc        | virtual pointer |
//! | other    | none (`--output NAME` only)                    | wlr-screencopy → appsrc        | virtual pointer |
//!
//! Mirror falls back to the xdg-desktop-portal picker when the direct path fails (shown once; the
//! restore token is kept). Every backend removes or switches off the screen it added on Drop; a
//! SIGKILL skips that (see each module for how to clean up by hand).

pub mod gnome;
pub mod hyprland;
pub mod kwin;
mod mutter;
pub mod niri;
mod portal;
mod screencopy;
pub mod sway;

use anyhow::{Result, bail};

use crate::desktop::{self, Desktop};
use crate::input::{Route, Target};

/// After `pipewiresrc` when the producer offers system memory (GNOME, KDE, most portals).
const SHM_CONVERT: &str = "video/x-raw ! videoconvert";
/// After `pipewiresrc` for niri, which offers only DMA-BUF (BGRx + modifiers, no SHM):
/// `videoconvert` can't take it and `glupload` fails to import it; VA-API `vapostproc` imports it
/// and hands out system-memory raw video.
const DMABUF_CONVERT: &str = "vapostproc ! video/x-raw ! videoconvert";

#[derive(Debug, Clone, PartialEq)]
pub enum Mode {
    Mirror,
    Extend { width: u32, height: u32, output: Option<String>, scale: Option<f64> },
}

/// One desktop's capture session. Dropping it stops the cast and undoes any screen it added.
pub(crate) trait Backend {
    fn source_description(&self) -> String;
    /// Stream size in pixels.
    fn size(&self) -> (u32, u32);
    fn geometry(&self) -> (i32, i32, u32, u32);
    fn output_name(&self) -> String;
    /// Whether `reconfigure_extend` can resize the screen in place.
    fn is_extend(&self) -> bool;
    fn reconfigure_extend(&mut self, width: u32, height: u32) -> Result<()>;
    fn route(&self) -> Route {
        Route::Wayland
    }
    fn bind(&self, _bin: &gst::Element) {}
    /// Extend: move the screen to where `desktop::Placement` now says (the user dragged the phone
    /// to another side). `Ok(false)`: this desktop can't move it.
    fn reposition(&mut self) -> Result<bool> {
        Ok(false)
    }
}

pub struct Capture {
    pub width: u32,
    pub height: u32,
    desktop: Desktop,
    inner: Box<dyn Backend>,
}

impl Capture {
    fn new(desktop: Desktop, inner: Box<dyn Backend>) -> Capture {
        let (width, height) = inner.size();
        Capture { width, height, desktop, inner }
    }

    pub fn source_description(&self) -> String {
        self.inner.source_description()
    }

    pub fn bind(&self, bin: &gst::Element) {
        self.inner.bind(bin);
    }

    pub fn geometry(&self) -> (i32, i32, u32, u32) {
        self.inner.geometry()
    }

    pub fn output_name(&self) -> String {
        self.inner.output_name()
    }

    pub fn desktop(&self) -> Desktop {
        self.desktop
    }

    pub fn input_target(&self) -> Target {
        Target { rect: self.geometry(), output: self.output_name(), route: self.inner.route() }
    }

    /// Extend only: whether `reconfigure_extend` can resize this capture's screen in place.
    pub fn is_extend(&self) -> bool {
        self.inner.is_extend()
    }

    /// Extend only: switches the screen to `width`x`height` WITHOUT removing it where the desktop
    /// allows (so its windows stay on it) and records it again; re-read `source_description()`.
    /// Used for the phone's resolution menu (cmd 70) and rotation (cmd 7): `height > width` =
    /// portrait. On error the screen is still owned by this Capture (drop it to remove it).
    pub fn reconfigure_extend(&mut self, width: u32, height: u32) -> Result<()> {
        let r = self.inner.reconfigure_extend(width, height);
        (self.width, self.height) = self.inner.size();
        r
    }

    /// Extend: moves the screen to the side the placement file now names; re-read `geometry()`.
    pub fn reposition(&mut self) -> Result<bool> {
        if !self.is_extend() {
            return Ok(false);
        }
        self.inner.reposition()
    }
}

pub fn start(mode: Mode) -> Result<Capture> {
    let desk = Desktop::detect();
    let made: Result<Box<dyn Backend>> = match desk {
        Desktop::Niri => niri::start(&mode).map(|c| Box::new(c) as Box<dyn Backend>),
        Desktop::Gnome => gnome::start(&mode).map(|c| Box::new(c) as Box<dyn Backend>),
        Desktop::Kde => kwin::start(&mode).map(|c| Box::new(c) as Box<dyn Backend>),
        Desktop::Hyprland => hyprland::start(&mode),
        Desktop::Sway => sway::start(&mode),
        Desktop::Other => other(&mode),
    };
    let inner = match made {
        Ok(c) => c,
        Err(e) => return mirror_via_portal(desk, &mode, e),
    };
    Ok(Capture::new(desk, inner))
}

/// Mirror only: when the desktop's direct path failed, ask xdg-desktop-portal (a picker, once).
fn mirror_via_portal(desk: Desktop, mode: &Mode, err: anyhow::Error) -> Result<Capture> {
    if *mode != Mode::Mirror {
        return Err(err);
    }
    eprintln!("capture: {} capture failed ({err:#}); falling back to the screen-sharing portal", desk.name());
    let cast = pollster::block_on(portal::start_mirror(desk))?;
    Ok(Capture::new(desk, Box::new(cast)))
}

/// A Wayland compositor we have no specific support for: wlr-screencopy of an existing output.
fn other(mode: &Mode) -> Result<Box<dyn Backend>> {
    let outs = desktop::wl_outputs()?;
    let out = match mode {
        Mode::Mirror => desktop::primary(&outs, None).cloned(),
        Mode::Extend { output: Some(name), .. } => outs.iter().find(|o| &o.name == name && o.logical.is_some()).cloned(),
        Mode::Extend { output: None, .. } => bail!(
            "{} has no known way to add a screen for the phone (supported: GNOME, KDE Plasma, Hyprland, \
             sway, niri). Use Duplicate, or connect a spare monitor or HDMI dummy plug, switch it on, \
             and choose it under Advanced (--output NAME)",
            std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_else(|_| "this desktop".into())
        ),
    };
    let Some(out) = out else { bail!("no enabled output to capture") };
    Ok(Box::new(screencopy::Plain::start(&out)?))
}

/// Nearest value in `choices` to `want` (`want` itself when there are none).
fn nearest(choices: &[f64], want: f64) -> f64 {
    choices.iter().copied().min_by(|a, b| (a - want).abs().total_cmp(&(b - want).abs())).unwrap_or(want)
}

#[cfg(test)]
pub(crate) mod tests {
    use std::time::Duration;

    use gst::prelude::*;

    use super::*;

    #[test]
    fn nearest_scale() {
        assert_eq!(nearest(&[1.0, 1.333, 2.0], 1.5), 1.333);
        assert_eq!(nearest(&[1.0, 1.333, 2.0], 1.75), 2.0);
        assert_eq!(nearest(&[], 1.75), 1.75);
    }

    /// Runs `<desc> ! fakesink` in gst-launch-1.0 for 3 s: the buffer count and the caps
    /// pipewiresrc negotiated (the real buffer size). `fd`: a portal PipeWire fd to inherit.
    pub(crate) fn run_pipeline(desc: &str, fd: Option<i32>) -> (usize, String) {
        use std::os::unix::process::CommandExt;
        let desc = format!("{desc} ! fakesink silent=false");
        let mut cmd = std::process::Command::new("gst-launch-1.0");
        cmd.arg("-v").args(desc.split_whitespace()).stdout(std::process::Stdio::piped());
        if let Some(fd) = fd {
            // The portal fd is CLOEXEC; let the child inherit it under the same number.
            unsafe {
                cmd.pre_exec(move || {
                    let fl = libc::fcntl(fd, libc::F_GETFD);
                    libc::fcntl(fd, libc::F_SETFD, fl & !libc::FD_CLOEXEC);
                    Ok(())
                });
            }
        }
        let mut child = cmd.spawn().expect("spawn gst-launch-1.0");
        std::thread::sleep(Duration::from_secs(3));
        let _ = child.kill();
        let out = child.wait_with_output().unwrap();
        let text = String::from_utf8_lossy(&out.stdout);
        let caps = text.lines().find(|l| l.contains("GstPipeWireSrc") && l.contains("caps = ")).unwrap_or_default().to_owned();
        (text.lines().filter(|l| l.contains("chain")).count(), caps)
    }

    /// Runs a source in-process for 3 s (an appsrc has to be bound): buffer count and sink caps.
    pub(crate) fn run_bound(cap: &dyn Backend) -> (usize, String) {
        gst::init().unwrap();
        let pipeline = gst::parse::launch(&format!("{} ! fakesink name=sink", cap.source_description())).unwrap();
        cap.bind(&pipeline);
        let count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let sink = pipeline.downcast_ref::<gst::Bin>().unwrap().by_name("sink").unwrap();
        let c = count.clone();
        sink.static_pad("sink").unwrap().add_probe(gst::PadProbeType::BUFFER, move |_, _| {
            c.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            gst::PadProbeReturn::Ok
        });
        pipeline.set_state(gst::State::Playing).unwrap();
        std::thread::sleep(Duration::from_secs(3));
        let caps = sink.static_pad("sink").unwrap().current_caps().map(|c| c.to_string()).unwrap_or_default();
        pipeline.set_state(gst::State::Null).unwrap();
        (count.load(std::sync::atomic::Ordering::Relaxed), caps)
    }

    /// Live, any desktop: cargo test --release -p smartconnect-display capture_smoke -- --ignored --nocapture --test-threads=1
    #[test]
    #[ignore]
    fn capture_smoke_mirror() {
        let cap = start(Mode::Mirror).expect("start mirror capture");
        eprintln!("{}: {} {}x{} {:?}", cap.desktop().name(), cap.output_name(), cap.width, cap.height, cap.geometry());
        let (n, caps) = run_bound(cap.inner.as_ref());
        eprintln!("mirror: {n} buffers in 3 s; {caps}");
        assert!(n > 0);
    }

    /// Live, any desktop: a phone-sized screen for 3 s, then gone.
    #[test]
    #[ignore]
    fn capture_smoke_extend() {
        let cap = start(Mode::Extend { width: 2712, height: 1220, output: None, scale: Some(2.0) }).expect("start extend capture");
        let (n, caps) = run_bound(cap.inner.as_ref());
        eprintln!("{}: {} {}x{} {:?}; {n} buffers; {caps}", cap.desktop().name(), cap.output_name(), cap.width, cap.height, cap.geometry());
        assert!(n > 0);
    }

    /// Live, any desktop: forces the portal fallback (no picker after the first approval).
    #[test]
    #[ignore]
    fn capture_smoke_portal() {
        let cap = pollster::block_on(portal::start_mirror(Desktop::detect())).expect("portal capture");
        let (n, _) = run_pipeline(&cap.source_description(), Some(cap.fd()));
        eprintln!("portal frames in 3 s: {n}");
        assert!(n > 0);
    }
}
