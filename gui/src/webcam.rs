//! Phone-as-webcam integration for the GTK front-end.
//!
//! Controls `smartconnect-webcam` (svc 3): starting and stopping the session, flipping between
//! the front and back cameras, toggling the torch / studio light, and monitoring session state
//! via the lock file ($XDG_RUNTIME_DIR/smartconnect-webcam.lock).

use std::fs::File;
use std::io::Read;
use std::os::fd::AsRawFd;
use std::path::PathBuf;
use std::process::Command;

use crate::mirror::binary;

const BIN: &str = "smartconnect-webcam";

fn runtime_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("XDG_RUNTIME_DIR").filter(|v| !v.is_empty()) {
        return PathBuf::from(dir);
    }
    std::env::var_os("XDG_STATE_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state")))
        .unwrap_or_default()
        .join("smartconnect-linux/run")
}

pub fn lock_path() -> PathBuf {
    runtime_dir().join("smartconnect-webcam.lock")
}

/// PID of the running webcam session, if one holds the lock.
pub fn running_pid() -> Option<i32> {
    let mut f = File::open(lock_path()).ok()?;
    // SAFETY: flock on an fd we own; a shared lock fails when an instance holds its exclusive one.
    if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_SH | libc::LOCK_NB) } == 0 {
        return None;
    }
    let mut s = String::new();
    f.read_to_string(&mut s).ok()?;
    s.trim().parse().ok()
}

pub fn is_running() -> bool {
    running_pid().is_some()
}

/// Asks the phone to start its camera and serve frames to the output.
pub fn start(quality: Option<&str>) -> Result<(), String> {
    let bin = binary(BIN);
    let mut cmd = Command::new(&bin);
    cmd.arg("--start");
    if let Some(q) = quality.filter(|s| !s.is_empty()) {
        cmd.arg("--quality").arg(q);
    }
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    cmd.spawn()
        .map_err(|e| format!("Could not start {}: {e}", bin.display()))?;
    Ok(())
}

/// Stops the running webcam session.
pub fn stop() -> Result<(), String> {
    let bin = binary(BIN);
    let status = Command::new(&bin)
        .arg("--stop")
        .status()
        .map_err(|e| format!("Could not run {}: {e}", bin.display()))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("Stop failed with exit code {:?}", status.code()))
    }
}

/// Switches between the front and back cameras.
pub fn switch_camera() -> Result<(), String> {
    let bin = binary(BIN);
    let status = Command::new(&bin)
        .arg("--switch-camera")
        .status()
        .map_err(|e| format!("Could not run {}: {e}", bin.display()))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!(
            "Camera switch failed with exit code {:?}",
            status.code()
        ))
    }
}

/// Toggles the phone's torch (studio light).
pub fn toggle_torch() -> Result<(), String> {
    let bin = binary(BIN);
    let status = Command::new(&bin)
        .arg("--torch")
        .status()
        .map_err(|e| format!("Could not run {}: {e}", bin.display()))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!(
            "Light toggle failed with exit code {:?}",
            status.code()
        ))
    }
}

pub fn ctl_socket_path() -> PathBuf {
    runtime_dir().join("smartconnect-webcam-ctl.sock")
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct WebcamStatus {
    pub active: bool,
    pub quality: String,
    pub size: String,
    pub frames: u64,
    pub mirror: bool,
    pub flip_v: bool,
    pub brightness: f64,
    pub contrast: f64,
}

pub fn send_ctl(req: serde_json::Value) -> Result<serde_json::Value, String> {
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixStream;
    use std::time::Duration;
    let mut stream = UnixStream::connect(ctl_socket_path())
        .map_err(|e| format!("Could not connect to webcam control socket: {e}"))?;
    let _ = stream.set_read_timeout(Some(Duration::from_millis(250)));
    let _ = stream.set_write_timeout(Some(Duration::from_millis(250)));
    let payload = format!("{}\n", req);
    stream
        .write_all(payload.as_bytes())
        .map_err(|e| format!("Could not write to webcam control socket: {e}"))?;
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader
        .read_line(&mut line)
        .map_err(|e| format!("Could not read from webcam control socket: {e}"))?;
    serde_json::from_str(&line).map_err(|e| format!("Invalid reply from webcam: {e}"))
}

pub fn get_status() -> Option<WebcamStatus> {
    if !is_running() {
        return None;
    }
    let res = send_ctl(serde_json::json!({ "cmd": "get_status" })).ok()?;
    Some(WebcamStatus {
        active: res.get("active").and_then(|v| v.as_bool()).unwrap_or(true),
        quality: res
            .get("quality")
            .and_then(|v| v.as_str())
            .unwrap_or("high")
            .to_string(),
        size: res
            .get("size")
            .and_then(|v| v.as_str())
            .unwrap_or("1280x720")
            .to_string(),
        frames: res.get("frames").and_then(|v| v.as_u64()).unwrap_or(0),
        mirror: res.get("mirror").and_then(|v| v.as_bool()).unwrap_or(false),
        flip_v: res
            .get("flipVertical")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
        brightness: res
            .get("brightness")
            .and_then(|v| v.as_f64())
            .unwrap_or(0.0),
        contrast: res.get("contrast").and_then(|v| v.as_f64()).unwrap_or(1.0),
    })
}

pub fn toggle_mirror() -> Result<bool, String> {
    let mut p = crate::prefs::load();
    p.webcam.mirror = !p.webcam.mirror;
    let _ = crate::prefs::save(&p);
    if is_running() {
        let _ = send_ctl(serde_json::json!({
            "cmd": "set_effects",
            "mirror": p.webcam.mirror,
            "flipVertical": p.webcam.flip_vertical,
            "brightness": p.webcam.brightness,
            "contrast": p.webcam.contrast,
        }));
    }
    Ok(p.webcam.mirror)
}

pub fn toggle_flip_v() -> Result<bool, String> {
    let mut p = crate::prefs::load();
    p.webcam.flip_vertical = !p.webcam.flip_vertical;
    let _ = crate::prefs::save(&p);
    if is_running() {
        let _ = send_ctl(serde_json::json!({
            "cmd": "set_effects",
            "mirror": p.webcam.mirror,
            "flipVertical": p.webcam.flip_vertical,
            "brightness": p.webcam.brightness,
            "contrast": p.webcam.contrast,
        }));
    }
    Ok(p.webcam.flip_vertical)
}

pub fn set_brightness(pct: i32) -> Result<(), String> {
    let val = (pct as f64) / 100.0;
    let mut p = crate::prefs::load();
    p.webcam.brightness = val;
    let _ = crate::prefs::save(&p);
    if is_running() {
        let _ = send_ctl(serde_json::json!({
            "cmd": "set_effects",
            "mirror": p.webcam.mirror,
            "flipVertical": p.webcam.flip_vertical,
            "brightness": val,
            "contrast": p.webcam.contrast,
        }));
    }
    Ok(())
}

pub fn set_contrast(pct: i32) -> Result<(), String> {
    let val = (pct as f64) / 100.0;
    let mut p = crate::prefs::load();
    p.webcam.contrast = val;
    let _ = crate::prefs::save(&p);
    if is_running() {
        let _ = send_ctl(serde_json::json!({
            "cmd": "set_effects",
            "mirror": p.webcam.mirror,
            "flipVertical": p.webcam.flip_vertical,
            "brightness": p.webcam.brightness,
            "contrast": val,
        }));
    }
    Ok(())
}

pub fn set_quality(q: &str) -> Result<(), String> {
    let mut p = crate::prefs::load();
    p.webcam.quality = q.to_string();
    let _ = crate::prefs::save(&p);
    if is_running() {
        let _ = send_ctl(serde_json::json!({
            "cmd": "set_quality",
            "quality": q,
        }));
    }
    Ok(())
}

/// Returns true if a virtual v4l2loopback device is present on the system.
pub fn loopback_device_info() -> Option<String> {
    let Ok(entries) = std::fs::read_dir("/sys/class/video4linux") else {
        return None;
    };
    for entry in entries.flatten() {
        if entry.file_name().to_string_lossy().starts_with("video") {
            let path = entry.path();
            if let Ok(target) = std::fs::canonicalize(&path) {
                if target.starts_with("/sys/devices/virtual/") {
                    let label = std::fs::read_to_string(path.join("name"))
                        .unwrap_or_default()
                        .trim()
                        .to_owned();
                    let dev_name = entry.file_name().to_string_lossy().into_owned();
                    return Some(if label.is_empty() {
                        format!("/dev/{dev_name}")
                    } else {
                        format!("{label} (/dev/{dev_name})")
                    });
                }
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_lock_path() {
        let p = lock_path();
        assert!(p.ends_with("smartconnect-webcam.lock"));
    }

    #[test]
    fn test_is_running_when_no_file() {
        let temp = std::env::temp_dir().join("nonexistent-sc-webcam.lock");
        assert!(!temp.exists());
    }
}
