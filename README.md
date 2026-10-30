# smartconnect-linux

[![Version](https://img.shields.io/badge/version-v0.2.1-blue.svg)](https://github.com/vemru/linuxsmartconnect/releases/tag/v0.2.1)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](LICENSE)
[![Platform: Linux](https://img.shields.io/badge/platform-Linux%20%28Wayland%29-green.svg)](docs/installation.md)
[![Rust: 1.88+](https://img.shields.io/badge/rust-1.88%2B-orange.svg)](https://www.rust-lang.org)

Native Linux companion daemon and GTK4 GUI for Motorola/Lenovo Smart Connect — screen mirroring, phone-as-webcam, file sharing, and clipboard sync over LAN.

The phone connects using the stock, unmodified Motorola/Lenovo Smart Connect app: no root, no developer options, and no extra mobile apps required.

---

## Key Features

- **Phone as Webcam:** Feed high-definition front/back camera streams into Linux desktop apps (Google Meet, Zoom, OBS, browsers) via `v4l2loopback` or PipeWire, with live bitrate, exposure, and orientation controls.
- **Screen Mirroring & App Streaming:** Low-latency WebRTC screen mirroring with keyboard/mouse navigation, plus individual Android apps launched in isolated desktop windows.
- **Cross Control:** Move your PC mouse and keyboard smoothly across screen edges onto your phone (Wayland `wlr-layer-shell` and GNOME InputCapture portal).
- **Phone as External Display:** Extend your workspace with the phone as a second monitor or duplicate your desktop display with touch and on-screen keyboard support.
- **Wireless File Sharing:** Mount the phone's storage locally via FUSE (NFSv3 over TLS) and share PC directories directly to Android's Smart Connect Files hub.
- **Clipboard & Notification Sync:** Seamless bi-directional text clipboard synchronization (excluding password-manager copies) and interactive notifications with quick reply.
- **Modern GTK4 App & Noctalia Shell Plugin:** Full-featured Libadwaita desktop interface with background system tray support and a native QML widget for Noctalia Shell.

---

## Quick Install

### Fastest Path

```sh
# Option A: Arch Linux (makepkg)
cd packaging/arch && makepkg -si

# Option B: Universal Installer (builds workspace and installs to ~/.local/bin)
git clone https://github.com/vemru/linuxsmartconnect.git && cd linuxsmartconnect
./packaging/install.sh --enable
```

<details>
<summary><b>Distro Dependencies</b> (click to expand)</summary>

Install system libraries before building (`avahi-daemon`, `bluez`, `wl-clipboard`, `fuse3`, GStreamer plugins):

- **Arch Linux:**
  ```sh
  sudo pacman -S --needed git base-devel rust avahi bluez wl-clipboard fuse3 \
    gstreamer gst-plugins-{base,good,bad} libnice gst-libav gst-plugin-va gtk4 libadwaita
  sudo systemctl enable --now avahi-daemon bluetooth
  ```
- **Fedora:**
  ```sh
  sudo dnf install git gcc pkgconf-pkg-config dbus-devel wayland-devel avahi bluez \
    wl-clipboard fuse3 gstreamer1-devel gstreamer1-plugins-{base,good,bad-free} \
    libnice-gstreamer1 gtk4-devel libadwaita-devel
  ```
- **Debian / Ubuntu:**
  ```sh
  sudo apt install git build-essential pkg-config libdbus-1-dev libwayland-dev \
    avahi-daemon bluez wl-clipboard fuse3 libgtk-4-dev libadwaita-1-dev \
    gstreamer1.0-plugins-{base,good,bad} gstreamer1.0-nice gstreamer1.0-libav
  ```

For full package matrices and build flags, see the [Installation Guide](docs/installation.md).
</details>

---

## Quick Start

1. **Start the Service & App:**
   ```sh
   smartconnect-gui
   # or manage the background user service:
   systemctl --user start smartconnectd
   ```
2. **Pair Your Phone:**
   Open **Smart Connect** on your phone, tap **+** → **Scan QR code**, and scan the QR code displayed in `smartconnect-gui` (or generated via `smartconnectd pair`).
3. **(Optional) Enable Phone as Webcam:**
   Configure the virtual camera loopback device:
   ```sh
   sudo ./packaging/setup-v4l2loopback.sh
   ```

---

> [!IMPORTANT]
> `smartconnect-linux` is strictly an application-layer LAN peer-to-peer daemon. Inbound TCP ports `52020` (Control/FEX) and `2049` (NFS File Station) enforce RFC 1918 and link-local private network validation by default. Do not expose these ports via public reverse proxies or Cloudflare Tunnels without `--allow-public`.

---

## Documentation

- [Installation Guide](docs/installation.md) — Dependencies, build options, systemd services, and firewall configurations.
- [Usage & Features](docs/usage.md) — Feature status matrix, detailed workflows, and CLI/environment references.
- [Architecture & Protocols](docs/architecture.md) — Process model, relay design, control sockets, and WebRTC pipelines.
- [Security Model](docs/security.md) & [Security Policy](SECURITY.md) — Socket permissions, authentication model, and vulnerability reporting.
- [Troubleshooting](docs/troubleshooting.md) — Known limitations, log locations, and diagnostic checklists.

---

## License

MIT License. See [LICENSE](LICENSE). Unofficial project; not affiliated with, endorsed by, or sponsored by Motorola or Lenovo.
