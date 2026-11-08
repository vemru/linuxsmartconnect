//! Capability matrix: reports per-feature availability and reasons/fix hints when unavailable.
//!
//! Features tracked:
//! - `mirror`: whole phone screen mirroring
//! - `webcam`: phone camera as Linux webcam via v4l2loopback or PipeWire
//! - `app_streaming`: stream individual phone apps into desktop windows
//! - `cross_control`: mouse and keyboard edge glide between PC and phone
//! - `phone_display`: extend or duplicate desktop onto the phone
//! - `storage_mount`: mount phone /sdcard locally via FUSE
//! - `folder_share`: share local PC directory with the phone
//! - `clipboard`: bi-directional clipboard synchronization
//! - `notifications`: mirror phone notifications to the desktop

use std::path::Path;
use std::process::{Command, Stdio};

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "camelCase")]
pub enum FeatureStatus {
    #[default]
    Available,
    #[serde(rename_all = "camelCase")]
    Unavailable {
        reason: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        fix_hint: Option<String>,
    },
}

impl FeatureStatus {
    pub fn available() -> Self {
        Self::Available
    }

    pub fn unavailable(reason: impl Into<String>, fix_hint: Option<String>) -> Self {
        Self::Unavailable {
            reason: reason.into(),
            fix_hint,
        }
    }

    pub fn is_available(&self) -> bool {
        matches!(self, Self::Available)
    }

    pub fn reason(&self) -> Option<&str> {
        match self {
            Self::Available => None,
            Self::Unavailable { reason, .. } => Some(reason),
        }
    }

    pub fn fix_hint(&self) -> Option<&str> {
        match self {
            Self::Available => None,
            Self::Unavailable { fix_hint, .. } => fix_hint.as_deref(),
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Capabilities {
    pub mirror: FeatureStatus,
    pub webcam: FeatureStatus,
    pub app_streaming: FeatureStatus,
    pub cross_control: FeatureStatus,
    pub phone_display: FeatureStatus,
    pub storage_mount: FeatureStatus,
    pub folder_share: FeatureStatus,
    pub clipboard: FeatureStatus,
    pub notifications: FeatureStatus,
}

/// System probe abstraction to allow testing probe combinations deterministically.
pub trait SystemProber {
    fn is_phone_connected(&self) -> bool;
    fn has_graphical_session(&self) -> bool;
    fn has_gstreamer_element(&self, element: &str) -> bool;
    fn is_v4l2loopback_loaded(&self) -> bool;
    fn is_fuse_available(&self) -> bool;
    fn is_fusermount3_available(&self) -> bool;
    fn is_cross_control_supported(&self) -> bool;
    fn is_wl_clipboard_available(&self) -> bool;
}

/// Production system prober querying the real environment.
pub struct LiveProber {
    pub phone_connected: bool,
}

impl LiveProber {
    pub fn new(phone_connected: bool) -> Self {
        Self { phone_connected }
    }
}

fn which_in_path(binary: &str) -> bool {
    let Some(path_var) = std::env::var_os("PATH") else {
        return false;
    };
    for dir in std::env::split_paths(&path_var) {
        let full = dir.join(binary);
        if full.is_file() {
            return true;
        }
    }
    false
}

fn has_virtual_video_device() -> bool {
    let Ok(entries) = std::fs::read_dir("/sys/class/video4linux") else {
        return false;
    };
    for entry in entries.flatten() {
        if entry.file_name().to_string_lossy().starts_with("video") {
            let path = entry.path();
            if let Ok(target) = std::fs::canonicalize(&path)
                && target.starts_with("/sys/devices/virtual/")
            {
                return true;
            }
        }
    }
    false
}

impl SystemProber for LiveProber {
    fn is_phone_connected(&self) -> bool {
        self.phone_connected
    }

    fn has_graphical_session(&self) -> bool {
        std::env::var_os("WAYLAND_DISPLAY").is_some() || std::env::var_os("DISPLAY").is_some()
    }

    fn has_gstreamer_element(&self, element: &str) -> bool {
        Command::new("gst-inspect-1.0")
            .arg(element)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    fn is_v4l2loopback_loaded(&self) -> bool {
        Path::new("/sys/module/v4l2loopback").exists() || has_virtual_video_device()
    }

    fn is_fuse_available(&self) -> bool {
        Path::new("/dev/fuse").exists()
    }

    fn is_fusermount3_available(&self) -> bool {
        which_in_path("fusermount3")
    }

    fn is_cross_control_supported(&self) -> bool {
        // CrossControl requires a graphical session (Wayland or X11)
        self.has_graphical_session()
    }

    fn is_wl_clipboard_available(&self) -> bool {
        if std::env::var_os("WAYLAND_DISPLAY").is_some() {
            which_in_path("wl-copy") && which_in_path("wl-paste")
        } else {
            // X11 fallback uses x11rb, requiring only DISPLAY
            std::env::var_os("DISPLAY").is_some()
        }
    }
}

/// Evaluates capabilities using the given prober.
pub fn evaluate_capabilities<P: SystemProber>(prober: &P) -> Capabilities {
    evaluate_capabilities_with_features(prober, None)
}

fn apply_feature_flag(status: FeatureStatus, enabled: bool) -> FeatureStatus {
    if !enabled {
        FeatureStatus::unavailable(
            "disabled in settings",
            Some("Enable in Settings".to_string()),
        )
    } else {
        status
    }
}

/// Evaluates capabilities with optional per-device feature switches applied.
pub fn evaluate_capabilities_with_features<P: SystemProber>(
    prober: &P,
    features: Option<&crate::features::DeviceFeatures>,
) -> Capabilities {
    let connected = prober.is_phone_connected();
    let graphical = prober.has_graphical_session();
    let has_webrtcbin = prober.has_gstreamer_element("webrtcbin");

    // 1. Screen Mirroring
    let mirror = if !graphical {
        FeatureStatus::unavailable(
            "No graphical session detected (WAYLAND_DISPLAY or DISPLAY is unset)",
            Some("Log into a Wayland or X11 desktop session".to_string()),
        )
    } else if !has_webrtcbin {
        FeatureStatus::unavailable(
            "GStreamer webrtcbin element is missing",
            Some(
                "Install gst-plugins-bad and libnice (e.g. sudo pacman -S gst-plugins-bad libnice)"
                    .to_string(),
            ),
        )
    } else if !connected {
        FeatureStatus::unavailable(
            "Phone is not connected",
            Some("Connect your phone in Smart Connect to mirror its screen".to_string()),
        )
    } else {
        FeatureStatus::available()
    };

    // 2. Webcam
    let webcam = if !prober.is_v4l2loopback_loaded() {
        FeatureStatus::unavailable(
            "v4l2loopback kernel module is not loaded",
            Some("sudo modprobe v4l2loopback devices=1 video_nr=10 card_label=\"Phone Camera\" exclusive_caps=1".to_string()),
        )
    } else if !has_webrtcbin {
        FeatureStatus::unavailable(
            "GStreamer webrtcbin element is missing",
            Some(
                "Install gst-plugins-bad and libnice (e.g. sudo pacman -S gst-plugins-bad libnice)"
                    .to_string(),
            ),
        )
    } else if !connected {
        FeatureStatus::unavailable(
            "Phone is not connected",
            Some("Connect your phone in Smart Connect to use it as a webcam".to_string()),
        )
    } else {
        FeatureStatus::available()
    };

    // 3. App Streaming
    let app_streaming = if !graphical {
        FeatureStatus::unavailable(
            "No graphical session detected (WAYLAND_DISPLAY or DISPLAY is unset)",
            Some("Log into a Wayland or X11 desktop session".to_string()),
        )
    } else if !has_webrtcbin {
        FeatureStatus::unavailable(
            "GStreamer webrtcbin element is missing",
            Some(
                "Install gst-plugins-bad and libnice (e.g. sudo pacman -S gst-plugins-bad libnice)"
                    .to_string(),
            ),
        )
    } else if !connected {
        FeatureStatus::unavailable(
            "Phone is not connected",
            Some("Connect your phone in Smart Connect to stream apps".to_string()),
        )
    } else {
        FeatureStatus::available()
    };

    // 4. Cross Control
    let cross_control = if !graphical {
        FeatureStatus::unavailable(
            "No graphical session detected (WAYLAND_DISPLAY or DISPLAY is unset)",
            Some("Log into a Wayland or X11 desktop session".to_string()),
        )
    } else if !prober.is_cross_control_supported() {
        FeatureStatus::unavailable(
            "Desktop environment does not support pointer barriers or layer-shell",
            Some(
                "Use a supported Wayland compositor (niri, GNOME, Hyprland, Sway, KDE)".to_string(),
            ),
        )
    } else if !connected {
        FeatureStatus::unavailable(
            "Phone is not connected",
            Some("Connect your phone in Smart Connect to share mouse and keyboard".to_string()),
        )
    } else {
        FeatureStatus::available()
    };

    // 5. Phone as External Display
    let phone_display = if !graphical {
        FeatureStatus::unavailable(
            "No graphical session detected (WAYLAND_DISPLAY or DISPLAY is unset)",
            Some("Log into a Wayland or X11 desktop session".to_string()),
        )
    } else if !has_webrtcbin {
        FeatureStatus::unavailable(
            "GStreamer webrtcbin element is missing",
            Some(
                "Install gst-plugins-bad and libnice (e.g. sudo pacman -S gst-plugins-bad libnice)"
                    .to_string(),
            ),
        )
    } else if !connected {
        FeatureStatus::unavailable(
            "Phone is not connected",
            Some(
                "Connect your phone in Smart Connect to extend or duplicate your screen"
                    .to_string(),
            ),
        )
    } else {
        FeatureStatus::available()
    };

    // 6. Storage Mount
    let storage_mount = if !prober.is_fuse_available() {
        FeatureStatus::unavailable(
            "/dev/fuse is not available",
            Some("Load the fuse kernel module: sudo modprobe fuse".to_string()),
        )
    } else if !prober.is_fusermount3_available() {
        FeatureStatus::unavailable(
            "fusermount3 binary not found in PATH",
            Some(
                "Install fuse3 package (e.g. sudo pacman -S fuse3 or sudo apt install fuse3)"
                    .to_string(),
            ),
        )
    } else if !connected {
        FeatureStatus::unavailable(
            "Phone is not connected",
            Some("Connect your phone in Smart Connect to mount its storage".to_string()),
        )
    } else {
        FeatureStatus::available()
    };

    // 7. Folder Share
    let folder_share = if !connected {
        FeatureStatus::unavailable(
            "Phone is not connected",
            Some("Connect your phone in Smart Connect to share PC folders".to_string()),
        )
    } else {
        FeatureStatus::available()
    };

    // 8. Clipboard
    let clipboard = if !graphical {
        FeatureStatus::unavailable(
            "No graphical session detected (WAYLAND_DISPLAY or DISPLAY is unset)",
            Some("Log into a Wayland or X11 desktop session".to_string()),
        )
    } else if !prober.is_wl_clipboard_available() {
        FeatureStatus::unavailable(
            "wl-clipboard tools (wl-copy, wl-paste) not found",
            Some("Install wl-clipboard package (e.g. sudo pacman -S wl-clipboard or sudo apt install wl-clipboard)".to_string()),
        )
    } else if !connected {
        FeatureStatus::unavailable(
            "Phone is not connected",
            Some("Connect your phone in Smart Connect to synchronize clipboard".to_string()),
        )
    } else {
        FeatureStatus::available()
    };

    // 9. Notifications
    let notifications = if !connected {
        FeatureStatus::unavailable(
            "Phone is not connected",
            Some("Connect your phone in Smart Connect to receive notifications".to_string()),
        )
    } else {
        FeatureStatus::available()
    };

    let (
        mirror,
        webcam,
        app_streaming,
        cross_control,
        phone_display,
        storage_mount,
        folder_share,
        clipboard,
        notifications,
    ) = if let Some(f) = features {
        (
            apply_feature_flag(mirror, f.mirror),
            apply_feature_flag(webcam, f.webcam),
            apply_feature_flag(app_streaming, f.app_streaming),
            apply_feature_flag(cross_control, f.cross_control),
            apply_feature_flag(phone_display, f.phone_display),
            apply_feature_flag(storage_mount, f.storage_mount),
            apply_feature_flag(folder_share, f.folder_share),
            apply_feature_flag(clipboard, f.clipboard),
            apply_feature_flag(notifications, f.notifications),
        )
    } else {
        (
            mirror,
            webcam,
            app_streaming,
            cross_control,
            phone_display,
            storage_mount,
            folder_share,
            clipboard,
            notifications,
        )
    };

    Capabilities {
        mirror,
        webcam,
        app_streaming,
        cross_control,
        phone_display,
        storage_mount,
        folder_share,
        clipboard,
        notifications,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct MockProber {
        connected: bool,
        graphical: bool,
        gstreamer: bool,
        v4l2loopback: bool,
        fuse: bool,
        fusermount3: bool,
        cross_control: bool,
        wl_clipboard: bool,
    }

    impl Default for MockProber {
        fn default() -> Self {
            Self {
                connected: true,
                graphical: true,
                gstreamer: true,
                v4l2loopback: true,
                fuse: true,
                fusermount3: true,
                cross_control: true,
                wl_clipboard: true,
            }
        }
    }

    impl SystemProber for MockProber {
        fn is_phone_connected(&self) -> bool {
            self.connected
        }
        fn has_graphical_session(&self) -> bool {
            self.graphical
        }
        fn has_gstreamer_element(&self, _el: &str) -> bool {
            self.gstreamer
        }
        fn is_v4l2loopback_loaded(&self) -> bool {
            self.v4l2loopback
        }
        fn is_fuse_available(&self) -> bool {
            self.fuse
        }
        fn is_fusermount3_available(&self) -> bool {
            self.fusermount3
        }
        fn is_cross_control_supported(&self) -> bool {
            self.cross_control
        }
        fn is_wl_clipboard_available(&self) -> bool {
            self.wl_clipboard
        }
    }

    #[test]
    fn all_available_when_probes_pass() {
        let prober = MockProber::default();
        let caps = evaluate_capabilities(&prober);
        assert!(caps.mirror.is_available());
        assert!(caps.webcam.is_available());
        assert!(caps.app_streaming.is_available());
        assert!(caps.cross_control.is_available());
        assert!(caps.phone_display.is_available());
        assert!(caps.storage_mount.is_available());
        assert!(caps.folder_share.is_available());
        assert!(caps.clipboard.is_available());
        assert!(caps.notifications.is_available());
    }

    #[test]
    fn v4l2loopback_unloaded_disables_webcam_with_fix_hint() {
        let mut prober = MockProber {
            v4l2loopback: false,
            ..Default::default()
        };
        let caps = evaluate_capabilities(&prober);
        assert!(!caps.webcam.is_available());
        assert!(caps.webcam.reason().unwrap().contains("v4l2loopback"));
        assert!(
            caps.webcam
                .fix_hint()
                .unwrap()
                .contains("modprobe v4l2loopback")
        );

        // Loading module enables it
        prober.v4l2loopback = true;
        let caps2 = evaluate_capabilities(&prober);
        assert!(caps2.webcam.is_available());
    }

    #[test]
    fn fuse_missing_disables_storage_mount() {
        let prober = MockProber {
            fuse: false,
            ..Default::default()
        };
        let caps = evaluate_capabilities(&prober);
        assert!(!caps.storage_mount.is_available());
        assert!(caps.storage_mount.reason().unwrap().contains("/dev/fuse"));

        let prober2 = MockProber {
            fuse: true,
            fusermount3: false,
            ..Default::default()
        };
        let caps2 = evaluate_capabilities(&prober2);
        assert!(!caps2.storage_mount.is_available());
        assert!(
            caps2
                .storage_mount
                .reason()
                .unwrap()
                .contains("fusermount3")
        );
    }

    #[test]
    fn phone_disconnected_disables_all_session_features() {
        let prober = MockProber {
            connected: false,
            ..Default::default()
        };
        let caps = evaluate_capabilities(&prober);
        assert!(!caps.mirror.is_available());
        assert!(!caps.webcam.is_available());
        assert!(!caps.storage_mount.is_available());
        assert!(!caps.clipboard.is_available());
        assert_eq!(caps.mirror.reason(), Some("Phone is not connected"));
    }

    #[test]
    fn serialization_roundtrip() {
        let prober = MockProber {
            v4l2loopback: false,
            ..Default::default()
        };
        let caps = evaluate_capabilities(&prober);
        let serialized = serde_json::to_string(&caps).unwrap();
        assert!(serialized.contains("\"status\":\"unavailable\""));
        assert!(serialized.contains("\"fixHint\":"));
        let deserialized: Capabilities = serde_json::from_str(&serialized).unwrap();
        assert_eq!(caps, deserialized);
    }

    #[test]
    fn disabled_feature_in_settings_overrides_capability() {
        let prober = MockProber::default();
        let features = crate::features::DeviceFeatures {
            clipboard: false,
            webcam: false,
            ..Default::default()
        };

        let caps = evaluate_capabilities_with_features(&prober, Some(&features));
        assert!(!caps.clipboard.is_available());
        assert_eq!(caps.clipboard.reason(), Some("disabled in settings"));
        assert_eq!(caps.clipboard.fix_hint(), Some("Enable in Settings"));

        assert!(!caps.webcam.is_available());
        assert_eq!(caps.webcam.reason(), Some("disabled in settings"));

        // Others that are enabled remain available
        assert!(caps.mirror.is_available());
        assert!(caps.app_streaming.is_available());
    }
}
