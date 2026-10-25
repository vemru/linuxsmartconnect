//! Phone as an external display for this PC (Smart Connect service 202).
//! Spec: evidence-2026-10-20/appstream-and-display-spec.md, section B.
//!
//! - `session`: svc 202 signaling over the daemon control socket + webrtcbin (PC sends video).
//! - `opts`: command line over the shared settings file (`~/.config/smartconnect-linux/display.json`).
//! - `desktop`: which desktop this is (niri, Hyprland, sway, GNOME, KDE, other), its outputs.
//! - `capture`: the PC screen as a GStreamer source, one backend per desktop (Mutter ScreenCast,
//!   KWin screencast, wlr-screencopy, portal fallback); Extend adds a screen for the phone.
//! - `input`: data-channel touch/mouse/keyboard packets from the phone → injected on this PC.
//! - `audio`: the PC's sound for the phone's audio section (opt-in).
//! - `ice`: which local ICE candidates are offered to the phone.

mod audio;
mod capture;
mod desktop;
mod ice;
mod input;
mod opts;
mod session;

fn main() -> anyhow::Result<()> {
    session::run()
}
