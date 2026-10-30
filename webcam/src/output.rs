//! Where decoded frames go: a v4l2loopback device (every V4L2 app sees a camera) or, without one,
//! a PipeWire camera node (apps that use PipeWire/the camera portal see it).
//!
//! Only virtual devices are ever written to: a loopback device lives under `/sys/devices/virtual`,
//! real cameras (USB/PCI) never do, so a built-in webcam can't be picked by accident.

use std::path::{Path, PathBuf};

use anyhow::{Result, bail};

/// The name we suggest for the loopback device (`card_label`) and preferably pick.
pub const CARD_LABEL: &str = "Phone Camera";
pub const PIPEWIRE_DESCRIPTION: &str = "Phone Camera (Smart Connect)";

pub const SETUP_HINT: &str = "\
No v4l2loopback device found, so the phone camera is offered as a PipeWire camera node instead
(PipeWire-aware apps only: Firefox/Chromium with PipeWire camera support, OBS, the camera portal).
For a camera that every app, Google Meet, and Chrome sees, load v4l2loopback (needs root):

  sudo packaging/setup-v4l2loopback.sh

Or manually (Fedora: sudo dnf install akmod-v4l2loopback; Arch: sudo pacman -S v4l2loopback-dkms):

  sudo modprobe v4l2loopback devices=1 video_nr=10 card_label=\"Phone Camera\" exclusive_caps=1

To load it at every boot:

  echo v4l2loopback | sudo tee /etc/modules-load.d/v4l2loopback.conf
  echo 'options v4l2loopback devices=1 video_nr=10 card_label=\"Phone Camera\" exclusive_caps=1' \\
    | sudo tee /etc/modprobe.d/v4l2loopback.conf
";

#[derive(Debug, Clone, PartialEq)]
pub enum Output {
    /// `/dev/videoN` of a v4l2loopback device, with its card label.
    V4l2 {
        device: PathBuf,
        label: String,
    },
    PipeWire,
    /// Decode and drop (testing).
    Discard,
}

impl Output {
    pub fn describe(&self) -> String {
        match self {
            Output::V4l2 { device, label } => {
                format!("v4l2loopback {} (\"{label}\")", device.display())
            }
            Output::PipeWire => format!("PipeWire camera node \"{PIPEWIRE_DESCRIPTION}\""),
            Output::Discard => "nowhere (decode only)".into(),
        }
    }
}

/// A loopback device: `/sys/class/video4linux/videoN` resolves into `/sys/devices/virtual/`.
pub fn is_virtual(sys_entry: &Path) -> bool {
    std::fs::canonicalize(sys_entry).is_ok_and(|p| p.starts_with("/sys/devices/virtual/"))
}

/// All loopback devices as (`/dev/videoN`, label), the "Phone Camera" one first.
pub fn loopback_devices() -> Vec<(PathBuf, String)> {
    loopback_devices_in(Path::new("/sys/class/video4linux"))
}

fn loopback_devices_in(class_dir: &Path) -> Vec<(PathBuf, String)> {
    let Ok(entries) = std::fs::read_dir(class_dir) else {
        return Vec::new();
    };
    let mut found: Vec<(PathBuf, String)> = entries
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().starts_with("video") && is_virtual(&e.path()))
        .map(|e| {
            let label = std::fs::read_to_string(e.path().join("name"))
                .unwrap_or_default()
                .trim()
                .to_owned();
            (Path::new("/dev").join(e.file_name()), label)
        })
        .collect();
    found.sort_by_key(|(dev, label)| (!label.contains(CARD_LABEL), dev.clone()));
    found
}

/// Resolves `--output`/`SMARTCONNECT_WEBCAM_OUTPUT` (`auto`, `v4l2`, `pipewire`, `none`) and
/// `--device`/`SMARTCONNECT_WEBCAM_DEVICE`.
pub fn choose(kind: &str, device: Option<&Path>) -> Result<Output> {
    let v4l2 = || -> Result<Option<Output>> {
        if let Some(dev) = device {
            let name = dev
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            let sys = Path::new("/sys/class/video4linux").join(&name);
            if !is_virtual(&sys) {
                bail!(
                    "{} is not a v4l2loopback device; refusing to write to a real camera",
                    dev.display()
                );
            }
            let label = std::fs::read_to_string(sys.join("name"))
                .unwrap_or_default()
                .trim()
                .to_owned();
            return Ok(Some(Output::V4l2 {
                device: dev.to_owned(),
                label,
            }));
        }
        Ok(loopback_devices()
            .into_iter()
            .next()
            .map(|(device, label)| Output::V4l2 { device, label }))
    };
    match kind {
        "auto" => Ok(v4l2()?.unwrap_or(Output::PipeWire)),
        "v4l2" => match v4l2()? {
            Some(o) => Ok(o),
            None => bail!("no v4l2loopback device found\n\n{SETUP_HINT}"),
        },
        "pipewire" => Ok(Output::PipeWire),
        "none" => Ok(Output::Discard),
        other => bail!("unknown output {other:?} (auto, v4l2, pipewire, none)"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn real_cameras_are_never_loopback() {
        // Whatever this machine has: every device we'd pick must resolve under /sys/devices/virtual.
        for (dev, _) in loopback_devices() {
            let sys = Path::new("/sys/class/video4linux").join(dev.file_name().unwrap());
            assert!(
                std::fs::canonicalize(sys)
                    .unwrap()
                    .starts_with("/sys/devices/virtual/")
            );
        }
    }

    #[test]
    fn explicit_non_virtual_device_is_refused() {
        // /dev/video-does-not-exist has no sysfs entry, so it can't be shown to be virtual.
        assert!(choose("v4l2", Some(Path::new("/dev/video-does-not-exist"))).is_err());
        assert_eq!(choose("pipewire", None).unwrap(), Output::PipeWire);
        assert_eq!(choose("none", None).unwrap(), Output::Discard);
        assert!(choose("bogus", None).is_err());
    }

    #[test]
    fn prefers_the_phone_camera_label() {
        let dir = std::env::temp_dir().join(format!("sc-webcam-v4l-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // Plain directories are not under /sys/devices/virtual, so nothing qualifies.
        std::fs::create_dir_all(dir.join("video5")).unwrap();
        std::fs::write(dir.join("video5/name"), "Dummy video device").unwrap();
        assert!(loopback_devices_in(&dir).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
