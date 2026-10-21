//! Supervises the desktop helpers that answer phone-initiated requests, so tapping a feature tile
//! on the phone always reaches something on the PC:
//!
//! * `smartconnect-mirror --host` (app streaming, notification "Open", whole-screen mirror invites):
//!   started once a phone session is verified, restarted with backoff if it dies, stopped (SIGTERM,
//!   it closes its sessions) when the daemon exits. A host the user started themselves is left alone:
//!   the host socket answering means "a host is running", and `--host` itself refuses to start twice.
//! * `smartconnect-crosscontrol`: started on svc 26 cmd 0 ("phone wants to be controlled") only when
//!   no instance holds its lock; a running instance answers cmd 0 itself. Started with `--accept` so
//!   it replies cmd 1 `{trigger:1}` at once (it missed the cmd 0) and arms the edge.
//! * `smartconnect-webcam --answer`: started on svc 3 cmd 0 `{isReply:0}` (Webcam tile tapped) when no
//!   webcam session holds its lock; the helper replies to the phone and runs the session. Needs no
//!   display (it writes to v4l2loopback or PipeWire), so it is started even without one.
//! * `smartconnect-display --mode <shareType>`: started on svc 202 cmd 50 (the user started "PC
//!   screen on this phone" on the phone) when no display session holds its lock; it answers with
//!   cmd 0 and runs the session. Its other choices come from the shared
//!   `~/.config/smartconnect-linux/display.json`. If it dies before it got going, the phone is
//!   sent cmd 26 so it stops waiting.
//!
//! Binaries ([`binary`]): `$SMARTCONNECT_MIRROR_BIN` / `$SMARTCONNECT_CROSSCONTROL_BIN` /
//! `$SMARTCONNECT_WEBCAM_BIN` / `$SMARTCONNECT_DISPLAY_BIN`, else the directory smartconnectd runs
//! from (an install, or the workspace's target directory), else `$PATH`. `SMARTCONNECT_HELPERS=0`
//! disables this.

use std::os::fd::AsRawFd;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::Value;
use tokio::process::{Child, Command};
use tokio::sync::broadcast::error::RecvError;
use tracing::{info, warn};

use crate::server::Daemon;
use crate::status::runtime_dir;
use crate::webcam;

pub const MIRROR: &str = "smartconnect-mirror";
pub const CROSSCONTROL: &str = "smartconnect-crosscontrol";
pub const WEBCAM: &str = "smartconnect-webcam";
pub const DISPLAY: &str = "smartconnect-display";
const SERVICE_CROSS_CONTROL: i64 = 26;
/// Svc 202: this PC's screen on the phone.
const SERVICE_DISPLAY: i64 = 202;
/// A display helper that exits unsuccessfully this soon never got the phone going.
const DISPLAY_EARLY_EXIT: Duration = Duration::from_secs(20);

/// PIDs of the children we started, signalled on daemon shutdown.
#[derive(Default)]
pub struct Helpers {
    pids: Mutex<Vec<u32>>,
}

impl Helpers {
    pub fn enabled() -> bool {
        std::env::var("SMARTCONNECT_HELPERS").map_or(true, |v| v != "0")
    }

    /// Starts the supervisor tasks.
    pub fn start(self: &Arc<Self>, daemon: Arc<Daemon>) {
        if !Self::enabled() {
            info!("helpers: disabled by SMARTCONNECT_HELPERS=0");
            return;
        }
        tokio::spawn(self.clone().answer_webcam(daemon.clone()));
        if std::env::var_os("WAYLAND_DISPLAY").is_none() && std::env::var_os("DISPLAY").is_none() {
            warn!("helpers: no graphical session; not starting the stream host / cross control");
            return;
        }
        tokio::spawn(self.clone().supervise_host(daemon.clone()));
        tokio::spawn(self.clone().answer_cross_control(daemon.clone()));
        tokio::spawn(self.clone().answer_display(daemon));
    }

    /// SIGTERM every child we started and give them a moment to tell the phone (cmd 7 / cmd 5).
    pub async fn shutdown(&self) {
        let pids = std::mem::take(&mut *self.pids.lock().unwrap());
        for &pid in &pids {
            signal(pid, libc::SIGTERM);
        }
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline && pids.iter().any(|&p| alive(p)) {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    fn spawn(&self, name: &str, args: &[&str]) -> std::io::Result<Child> {
        let bin = binary(name);
        let log_path = runtime_dir().join(format!("{name}.log"));
        let log = std::fs::OpenOptions::new().create(true).append(true).open(&log_path)?;
        let mut cmd = Command::new(&bin);
        cmd.args(args).stdin(Stdio::null()).stdout(log.try_clone()?).stderr(log).kill_on_drop(true);
        // SAFETY: prctl is async-signal-safe; the child gets SIGTERM if the daemon dies uncleanly.
        unsafe {
            cmd.pre_exec(|| {
                libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM);
                Ok(())
            });
        }
        let child = cmd.spawn().map_err(|e| {
            let hint = if e.kind() == std::io::ErrorKind::NotFound {
                format!(" (not next to smartconnectd or on PATH; set {})", env_var(name))
            } else {
                String::new()
            };
            std::io::Error::new(e.kind(), format!("{}: {e}{hint}", bin.display()))
        })?;
        if let Some(pid) = child.id() {
            self.pids.lock().unwrap().push(pid);
            info!(pid, bin = %bin.display(), ?args, log = %log_path.display(), "helpers: started {name}");
        }
        Ok(child)
    }

    fn forget(&self, pid: Option<u32>) {
        if let Some(pid) = pid {
            self.pids.lock().unwrap().retain(|&p| p != pid);
        }
    }

    /// Keeps one `smartconnect-mirror --host` alive from the first verified session on.
    async fn supervise_host(self: Arc<Self>, daemon: Arc<Daemon>) {
        let mut backoff = Duration::from_secs(2);
        loop {
            // Wait for a verified phone session (the host is useless before the first one).
            while !daemon.is_connected() {
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
            if host_running().await {
                // Someone else's host (user-started, or one that outlived a daemon restart).
                tokio::time::sleep(Duration::from_secs(5)).await;
                continue;
            }
            let mut child = match self.spawn(MIRROR, &["--host"]) {
                Ok(c) => c,
                Err(e) => {
                    warn!("helpers: can't start the stream host ({e}); phone app-stream invites won't be answered");
                    tokio::time::sleep(Duration::from_secs(60)).await;
                    continue;
                }
            };
            let started = Instant::now();
            let status = child.wait().await;
            self.forget(child.id());
            if started.elapsed() > Duration::from_secs(60) {
                backoff = Duration::from_secs(2);
            }
            warn!(?status, retry_in = ?backoff, "helpers: stream host exited");
            tokio::time::sleep(backoff).await;
            backoff = (backoff * 2).min(Duration::from_secs(60));
        }
    }

    /// Phone svc 26 cmd 0 (user tapped Cross Control): make sure a crosscontrol client answers.
    async fn answer_cross_control(self: Arc<Self>, daemon: Arc<Daemon>) {
        let mut frames = daemon.from_phone.subscribe();
        let mut ours: Option<Child> = None;
        loop {
            let data: Value = match frames.recv().await {
                Ok((SERVICE_CROSS_CONTROL, d)) => d,
                Ok(_) | Err(RecvError::Lagged(_)) => continue,
                Err(RecvError::Closed) => return,
            };
            if data.get("cmd").and_then(Value::as_i64) != Some(0) {
                continue;
            }
            if let Some(c) = ours.as_mut()
                && matches!(c.try_wait(), Ok(None))
            {
                continue; // ours is up (maybe still connecting); it answers the phone itself
            }
            if let Some(c) = ours.take() {
                self.forget(c.id());
            }
            if crosscontrol_running() {
                info!("helpers: cross control requested by the phone; the running client answers");
                continue;
            }
            let edge = std::env::var("SMARTCONNECT_CROSS_EDGE").unwrap_or_else(|_| "right".into());
            match self.spawn(CROSSCONTROL, &["--edge", &edge, "--accept"]) {
                Ok(c) => ours = Some(c),
                Err(e) => warn!("helpers: can't start cross control for the phone's request: {e}"),
            }
        }
    }

    /// Phone svc 3 cmd 0 `{isReply:0}` (user tapped Webcam): make sure a webcam helper answers.
    async fn answer_webcam(self: Arc<Self>, daemon: Arc<Daemon>) {
        let mut frames = daemon.from_phone.subscribe();
        loop {
            let data: Value = match frames.recv().await {
                Ok((webcam::SERVICE_WEBCAM, d)) => d,
                Ok(_) | Err(RecvError::Lagged(_)) => continue,
                Err(RecvError::Closed) => return,
            };
            if !webcam::is_phone_request(&data) {
                continue;
            }
            if lock_held(&webcam_lock_path()) {
                info!("helpers: webcam requested by the phone; the running webcam session answers");
                continue;
            }
            match self.spawn(WEBCAM, &["--answer"]) {
                Ok(mut child) => {
                    let me = self.clone();
                    tokio::spawn(async move {
                        let status = child.wait().await;
                        me.forget(child.id());
                        info!(?status, "helpers: webcam session ended");
                    });
                }
                Err(e) => {
                    warn!("helpers: can't start the webcam helper for the phone's request: {e}");
                    // The phone then explains that the PC needs its webcam driver.
                    let _ = daemon.to_phone.send((webcam::SERVICE_WEBCAM, webcam::error_msg(webcam::ERR_NO_DRIVER)));
                }
            }
        }
    }
}

impl Helpers {
    /// Phone svc 202 cmd 50 (the user started "PC screen on this phone" on the phone): make sure a
    /// display sender answers. A running `smartconnect-display` (it holds its lock) answers cmd 50
    /// itself; one we started and that is still starting up is left to it.
    async fn answer_display(self: Arc<Self>, daemon: Arc<Daemon>) {
        let mut frames = daemon.from_phone.subscribe();
        let ours: Arc<Mutex<Option<u32>>> = Arc::default();
        loop {
            let data: Value = match frames.recv().await {
                Ok((SERVICE_DISPLAY, d)) => d,
                Ok(_) | Err(RecvError::Lagged(_)) => continue,
                Err(RecvError::Closed) => return,
            };
            let Some(share) = display_request(&data) else { continue };
            if ours.lock().unwrap().is_some_and(alive) {
                continue; // ours is up (maybe still connecting); it answers the phone itself
            }
            if lock_held(&display_lock_path()) {
                info!("helpers: the phone asks for this PC's screen; the running display session answers");
                continue;
            }
            let args: Vec<&str> = share.map_or_else(Vec::new, |s| vec!["--mode", s]);
            match self.spawn(DISPLAY, &args) {
                Ok(mut child) => {
                    let pid = child.id();
                    *ours.lock().unwrap() = pid;
                    let (me, ours, daemon) = (self.clone(), ours.clone(), daemon.clone());
                    tokio::spawn(async move {
                        let started = Instant::now();
                        let status = child.wait().await;
                        me.forget(pid);
                        let mut slot = ours.lock().unwrap();
                        if *slot == pid {
                            *slot = None;
                        }
                        drop(slot);
                        let failed = status.as_ref().map_or(true, |s| !s.success());
                        // (Not if another instance holds the lock now: that one answers the phone.)
                        if failed && started.elapsed() < DISPLAY_EARLY_EXIT && !lock_held(&display_lock_path()) {
                            warn!(?status, "helpers: {DISPLAY} failed to start (see its log); telling the phone to stop waiting");
                            let _ = daemon.to_phone.send((SERVICE_DISPLAY, display_stop_msg(&data)));
                        } else {
                            info!(?status, "helpers: display session ended");
                        }
                    });
                }
                Err(e) => {
                    warn!("helpers: can't start {DISPLAY} for the phone's request: {e}");
                    // Otherwise the phone stays in "connecting" and ignores its next start request.
                    let _ = daemon.to_phone.send((SERVICE_DISPLAY, display_stop_msg(&data)));
                }
            }
        }
    }
}

/// Svc 202 cmd 50 from the phone: `Some(shareType)` (`"extend"`/`"mirror"`, or `None` when it
/// names neither, which leaves the mode to display.json); `None` for any other message. The inner
/// `data` is a JSON string (`{"shareType":"extend","setAudio":false,"setScale":200}`) or an object.
pub fn display_request(data: &Value) -> Option<Option<&'static str>> {
    let int = |v: Option<&Value>| v.and_then(|v| v.as_i64().or_else(|| v.as_str().and_then(|s| s.parse().ok())));
    if int(data.get("cmd")) != Some(50) {
        return None;
    }
    let inner = match data.get("data") {
        Some(Value::String(s)) => serde_json::from_str(s).unwrap_or(Value::Null),
        Some(v) => v.clone(),
        None => Value::Null,
    };
    Some(match inner.get("shareType").and_then(Value::as_str) {
        Some("extend") => Some("extend"),
        Some("mirror") => Some("mirror"),
        _ => None,
    })
}

/// Svc 202 cmd 26 (stop from the PC) for the session in `request`. The phone resets a pending
/// start with it and answers cmd 52.
fn display_stop_msg(request: &Value) -> Value {
    let sid = request.get("sessionId").and_then(Value::as_i64).unwrap_or(0);
    serde_json::json!({ "ver": "1", "cmd": 26, "data": {}, "sessionId": sid })
}

/// `smartconnect-display` holds an exclusive flock on this file (with its pid) while it runs.
pub fn display_lock_path() -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from).unwrap_or_else(std::env::temp_dir).join("smartconnect-display.lock")
}

/// The webcam helper holds an exclusive flock on this file (with its pid) while a session runs.
pub fn webcam_lock_path() -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from).unwrap_or_else(std::env::temp_dir).join("smartconnect-webcam.lock")
}

/// Where the helper `name` is: `$SMARTCONNECT_<NAME>_BIN` (see [`env_var`]), else a file of that
/// name next to smartconnectd (an install puts all binaries in one directory, and so does a
/// workspace build), else the bare name, which `Command` looks up on `$PATH`.
pub fn binary(name: &str) -> PathBuf {
    if let Some(p) = std::env::var_os(env_var(name)).filter(|p| !p.is_empty()) {
        return PathBuf::from(p);
    }
    std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|dir| dir.join(name)))
        .filter(|p| p.is_file())
        .unwrap_or_else(|| PathBuf::from(name))
}

/// The override variable for a helper: `smartconnect-mirror` → `SMARTCONNECT_MIRROR_BIN`.
pub fn env_var(name: &str) -> String {
    format!("SMARTCONNECT_{}_BIN", name.trim_start_matches("smartconnect-").to_uppercase().replace('-', "_"))
}

pub fn host_socket_path() -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from).unwrap_or_else(std::env::temp_dir).join("smartconnect-mirror.sock")
}

/// The crosscontrol client holds an exclusive flock on this file while it runs.
pub fn crosscontrol_lock_path() -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("smartconnect-crosscontrol.lock")
}

async fn host_running() -> bool {
    tokio::net::UnixStream::connect(host_socket_path()).await.is_ok()
}

fn crosscontrol_running() -> bool {
    lock_held(&crosscontrol_lock_path())
}

fn lock_held(path: &std::path::Path) -> bool {
    let Ok(f) = std::fs::File::open(path) else { return false };
    // SAFETY: flock on an fd we own; a shared lock fails while the client holds its exclusive one.
    let free = unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_SH | libc::LOCK_NB) } == 0;
    !free
}

fn signal(pid: u32, sig: libc::c_int) {
    if let Ok(pid) = libc::pid_t::try_from(pid) {
        // SAFETY: kill(2) on a child we spawned.
        unsafe { libc::kill(pid, sig) };
    }
}

fn alive(pid: u32) -> bool {
    std::path::Path::new(&format!("/proc/{pid}")).exists()
        && !std::fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|s| s.contains(") Z "))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_override_wins() {
        // SAFETY: test-only, single env var not read concurrently by other tests.
        unsafe { std::env::set_var("SMARTCONNECT_CROSSCONTROL_BIN", "/x/cc") };
        assert_eq!(binary(CROSSCONTROL), PathBuf::from("/x/cc"));
        unsafe { std::env::remove_var("SMARTCONNECT_CROSSCONTROL_BIN") };
    }

    #[test]
    fn override_variable_names() {
        assert_eq!(env_var(MIRROR), "SMARTCONNECT_MIRROR_BIN");
        assert_eq!(env_var(CROSSCONTROL), "SMARTCONNECT_CROSSCONTROL_BIN");
        assert_eq!(env_var(WEBCAM), "SMARTCONNECT_WEBCAM_BIN");
        assert_eq!(env_var(DISPLAY), "SMARTCONNECT_DISPLAY_BIN");
    }

    #[test]
    fn falls_back_to_path_lookup() {
        // Nothing of this name next to the test binary and no override: `Command` searches $PATH.
        let name = "smartconnect-no-such-helper";
        assert_eq!(binary(name), PathBuf::from(name));
        // Something next to the running executable wins over $PATH.
        let exe = std::env::current_exe().unwrap();
        let own = exe.file_name().unwrap().to_str().unwrap();
        assert_eq!(binary(own), exe);
    }

    #[test]
    fn display_start_requests() {
        use serde_json::json;
        // As the phone sends it: inner data as a JSON string, sessionId 0 after a previous stop.
        let req = json!({"ver":"1","cmd":50,"data":"{\"shareType\":\"extend\",\"setAudio\":false,\"setScale\":200}","sessionId":0});
        assert_eq!(display_request(&req), Some(Some("extend")));
        let req = json!({"ver":"1","cmd":"50","data":{"shareType":"mirror"}});
        assert_eq!(display_request(&req), Some(Some("mirror")));
        // A start request with no usable mode leaves it to display.json.
        assert_eq!(display_request(&json!({"cmd":50,"data":"{\"shareType\":\"tablet\"}"})), Some(None));
        assert_eq!(display_request(&json!({"cmd":50})), Some(None));
        // Anything else is not a start request.
        assert_eq!(display_request(&json!({"cmd":52,"data":"{\"stopFrom\":0}"})), None);
        assert_eq!(display_request(&json!({"cmd":5,"data":"{\"shareType\":\"extend\"}"})), None);
        // The stop that releases a waiting phone.
        let stop = display_stop_msg(&json!({"cmd":50,"sessionId":12}));
        assert_eq!(stop, json!({"ver":"1","cmd":26,"data":{},"sessionId":12}));
        assert!(display_lock_path().ends_with("smartconnect-display.lock"));
    }

    #[test]
    fn unlocked_file_means_not_running() {
        let path = std::env::temp_dir().join(format!("sc-helpers-test-{}.lock", std::process::id()));
        assert!(!lock_held(&path), "missing file");
        std::fs::write(&path, b"").unwrap();
        assert!(!lock_held(&path));
        let f = std::fs::File::open(&path).unwrap();
        assert_eq!(unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) }, 0);
        assert!(lock_held(&path));
        drop(f);
        assert!(!lock_held(&path));
        let _ = std::fs::remove_file(&path);
    }
}
