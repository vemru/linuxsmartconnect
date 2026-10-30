//! Daemon lifecycle and the "Start at login" autostart entry.
//!
//! The GTK app owns the daemon's run for the user's convenience:
//!
//! - **Opening the app starts the daemon.** If the control socket isn't answering, prefer the
//!   installed `systemctl --user` unit, else spawn `smartconnectd` as a child (found the same way
//!   the daemon finds its helpers: `$SMARTCONNECT_SMARTCONNECTD_BIN`, then next to this binary,
//!   then `PATH`).
//! - **Quit stops the daemon** — and only Quit (closing the window just hides it). We stop exactly
//!   what we started: the child we spawned (SIGTERM), or the systemd unit (`systemctl --user stop`).
//! - **Start at login** writes/removes `~/.config/autostart/dev.smartconnect.Linux.desktop` with
//!   `Exec=smartconnect-gui --background`, so the app comes up hidden to the tray at login.

use std::io;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use crate::daemon::socket_path;
use crate::mirror::binary;

/// The systemd user unit name and the daemon binary name (the same string serves both).
const UNIT: &str = "smartconnectd";

/// The autostart entry's file name (its desktop-entry basename, per the XDG autostart spec).
const AUTOSTART_FILE: &str = "dev.smartconnect.Linux.desktop";

// ── Control socket ──────────────────────────────────────────────────────────

/// Whether the daemon's control socket is answering. A listener accepts the connection; a stale
/// socket file with no listener refuses it, so this never false-positives on a leftover file.
pub fn socket_alive() -> bool {
    socket_alive_at(&socket_path())
}

fn socket_alive_at(path: &Path) -> bool {
    UnixStream::connect(path).is_ok()
}

// ── systemd user unit ───────────────────────────────────────────────────────

/// Runs `systemctl --user <args>` to completion, returning whether it exited 0. A missing
/// `systemctl` (no user systemd) is simply `false`.
fn systemctl_ok(args: &[&str]) -> bool {
    Command::new("systemctl")
        .arg("--user")
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Whether a `smartconnectd` user unit file is installed (`systemctl --user cat` finds it).
fn unit_installed() -> bool {
    systemctl_ok(&["cat", UNIT])
}

/// Whether systemd reports the unit active right now.
fn unit_active() -> bool {
    Command::new("systemctl")
        .args(["--user", "is-active", UNIT])
        .output()
        .map(|o| o.stdout.trim_ascii() == b"active")
        .unwrap_or(false)
}

/// Spawns `smartconnectd` as our own child, found by the project's binary-lookup convention.
fn spawn_child() -> io::Result<Child> {
    Command::new(binary(UNIT)).spawn()
}

// ── Daemon manager ──────────────────────────────────────────────────────────

/// Remembers how the daemon is running so Quit can stop the right thing.
#[derive(Default)]
pub struct Daemon {
    /// Set when this app spawned the daemon itself.
    child: Option<Child>,
    /// Set when the daemon runs under the `systemctl --user` unit.
    systemd: bool,
}

impl Daemon {
    /// Makes sure the daemon is running. Returns the manager and whether this call actually launched
    /// it (so the UI can show a "Starting…" state until the socket answers).
    pub fn ensure_running() -> (Self, bool) {
        if socket_alive() {
            // Already up; don't touch it. Note whether systemd owns it, for Quit.
            return (
                Self {
                    child: None,
                    systemd: unit_active(),
                },
                false,
            );
        }
        if unit_installed() && systemctl_ok(&["start", UNIT]) {
            return (
                Self {
                    child: None,
                    systemd: true,
                },
                true,
            );
        }
        match spawn_child() {
            Ok(child) => (
                Self {
                    child: Some(child),
                    systemd: false,
                },
                true,
            ),
            Err(e) => {
                eprintln!("smartconnect-gui: could not start {UNIT}: {e}");
                (Self::default(), false)
            }
        }
    }

    /// Stops the daemon we manage: our child (SIGTERM, then a short wait, then SIGKILL) or, failing
    /// that, the systemd unit. A daemon someone else started (e.g. by hand) is left alone.
    pub fn stop(&mut self) {
        if let Some(mut child) = self.child.take() {
            if let Ok(pid) = libc::pid_t::try_from(child.id()) {
                // SAFETY: SIGTERM to our own, not-yet-reaped child so it can shut down cleanly.
                unsafe { libc::kill(pid, libc::SIGTERM) };
            }
            // Reap without blocking the UI for long (~1s budget), then force it.
            for _ in 0..50 {
                match child.try_wait() {
                    Ok(Some(_)) => return,
                    Ok(None) => std::thread::sleep(Duration::from_millis(20)),
                    Err(_) => break,
                }
            }
            let _ = child.kill();
            let _ = child.wait();
        } else if self.systemd {
            systemctl_ok(&["stop", UNIT]);
        }
    }
}

// ── Autostart entry (Start at login) ────────────────────────────────────────

fn config_home() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".config")
        })
}

fn autostart_path_in(config_home: &Path) -> PathBuf {
    config_home.join("autostart").join(AUTOSTART_FILE)
}

/// `~/.config/autostart/dev.smartconnect.Linux.desktop`.
pub fn autostart_path() -> PathBuf {
    autostart_path_in(&config_home())
}

/// Whether the autostart entry is present.
pub fn autostart_enabled() -> bool {
    autostart_path().is_file()
}

/// The autostart desktop entry. `--background` makes the app come up hidden to the tray;
/// `StartupNotify=false` because there's no window to wait for.
const AUTOSTART_ENTRY: &str = "\
[Desktop Entry]
Type=Application
Name=Smart Connect
GenericName=Phone Companion
Comment=Connect, mirror and reply to notifications from your Motorola phone
Exec=smartconnect-gui --background
Icon=dev.smartconnect.Linux
Terminal=false
Categories=Network;GTK;
Keywords=phone;motorola;mirror;notifications;
StartupNotify=false
StartupWMClass=dev.smartconnect.Linux
X-GNOME-Autostart-enabled=true
";

fn set_autostart_at(path: &Path, on: bool) -> io::Result<()> {
    if on {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(path, AUTOSTART_ENTRY)
    } else {
        match std::fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        }
    }
}

/// Adds (`on`) or removes the autostart entry. Removing one that isn't there is not an error.
pub fn set_autostart(on: bool) -> io::Result<()> {
    set_autostart_at(&autostart_path(), on)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener;

    #[test]
    fn socket_alive_only_with_a_listener() {
        let dir = std::env::temp_dir().join(format!("sc-gui-sock-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let sock = dir.join("smartconnectd.sock");

        // No socket file at all: not alive.
        assert!(!socket_alive_at(&sock));

        // A bound, listening socket: alive.
        let listener = UnixListener::bind(&sock).unwrap();
        assert!(socket_alive_at(&sock));

        // Listener gone but the socket file left behind: not alive (connection refused).
        // Another test may fork a child (Command::spawn) while the listener is open; until that
        // child execs, it holds a CLOEXEC copy of the fd, so poll briefly instead of checking once.
        drop(listener);
        assert!(sock.exists(), "std leaves the socket file on drop");
        let gone = (0..50).any(|_| {
            let alive = socket_alive_at(&sock);
            if alive {
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            !alive
        });
        assert!(
            gone,
            "socket still accepting 1 s after the listener was dropped"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn autostart_entry_round_trips() {
        let dir = std::env::temp_dir().join(format!("sc-gui-autostart-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = autostart_path_in(&dir);

        assert!(!path.is_file());
        set_autostart_at(&path, true).unwrap();
        assert!(path.is_file());
        let body = std::fs::read_to_string(&path).unwrap();
        assert!(body.contains("Exec=smartconnect-gui --background"));
        assert!(body.contains("[Desktop Entry]"));
        // Removing is idempotent.
        set_autostart_at(&path, false).unwrap();
        assert!(!path.is_file());
        set_autostart_at(&path, false).unwrap();

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn daemon_binary_uses_the_project_convention() {
        assert_eq!(
            crate::mirror::env_var(UNIT),
            "SMARTCONNECT_SMARTCONNECTD_BIN"
        );
    }

    #[test]
    fn stop_terminates_a_spawned_child() {
        // A harmless stand-in for the daemon (never the real one).
        let child = Command::new("sleep")
            .arg("60")
            .spawn()
            .expect("spawn sleep");
        let pid = child.id() as libc::pid_t;
        let mut daemon = Daemon {
            child: Some(child),
            systemd: false,
        };
        daemon.stop();
        // After stop the child is reaped; signalling the pid now fails.
        let alive = unsafe { libc::kill(pid, 0) } == 0;
        assert!(!alive, "the spawned child should be gone after stop()");
    }
}
