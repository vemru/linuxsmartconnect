# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project
adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html). The README's
[feature matrix](README.md#feature-status) shows what is verified on a real phone.

## [Unreleased]

### Added

- **Phase 9 Clipboard OTP Protection & Reconnect Jitter:**
  - Added double-copy confirmation requirement for one-time passwords, 2FA tokens, and sensitive numeric codes before syncing over the clipboard channel.
  - Added randomized backoff jitter to automatic reconnection loops to prevent synchronized reconnect storms on network interface state changes.
- **Phase 8 Installer Verification, Uninstaller & Multi-Distro CI:**
  - Added full `--uninstall` and `--uninstall --purge` flags to `packaging/install.sh` for clean removal of binaries, systemd units, desktop files, icons, and state directories.
  - Added multi-distro CI smoke tests across Arch Linux, Fedora 40, and Ubuntu 24.04 running as an unprivileged user.
  - Added `cargo-deny` security advisory checks to CI workflow.
- **Phase 7 File Manager Integrations & CLI Send:**
  - Added "Send to phone" context menu actions for major Linux file managers (Nautilus, Dolphin, Nemo, and Thunar).
  - Added CLI `smartconnectd send <path...>` command and control socket file transfer initiation.
- **Phase 6 Photos Browser:**
  - Implemented GTK4 Photos gallery view in `smartconnect-gui` with asynchronous thumbnail caching and image preview modal.
- **Phase 5 Command Palette:**
  - Added quick-action Command Palette with fuzzy search across connected phone features, settings, and apps (`smartconnectd palette`).
  - Added compositor keybinding shortcuts for Wayland environments.
- **Phase 4 Searchable Activity Timeline:**
  - Added searchable interaction timeline powered by SQLite FTS5 index (`$XDG_STATE_HOME/smartconnect-linux/timeline.db`).
  - Records file shares, clipboard synchronizations, notification interactions, and connection events with full-text search.
  - Added Timeline view tab in `smartconnect-gui` with search filter and item details.
- **Phase 3 Per-Feature Switches:**
  - Added per-device feature on/off switches with daemon enforcement and GUI settings (`$XDG_STATE_HOME/smartconnect-linux/features.json`).
  - Sensitive features (`cross_control`, `storage_mount`, `folder_share`) default to OFF; standard features default to ON.
  - Enforced in daemon session loop and helpers (ignoring incoming and preventing outgoing sync/connections when disabled).
  - Capabilities matrix reflects disabled state as `Unavailable { reason: "disabled in settings", fix_hint: "Enable in Settings" }`.
  - Added CLI `smartconnectd feature <device> [<name> on|off]` and control socket `feature.get`, `feature.set`, `feature.list` commands.
  - Added GTK4 Feature Settings page in `smartconnect-gui` with live switches and daemon synchronization.
- **Phase 2 Connection Doctor:**
  - Added comprehensive diagnostics for local network interfaces, mDNS discovery (Avahi), listener ports (52020, 2049), firewall configurations (firewalld, UFW), Bluetooth services, and runtime helper dependencies.
  - Added `smartconnectd doctor [--json]` CLI command and control socket `doctor` command.
  - Added "Diagnostics" view tab to `smartconnect-gui` with live test status badges, one-click safe pkexec repair buttons, and clipboard export.
- **Phase 1 Capability Matrix:**
  - Implemented runtime probing and capability reporting across 9 features (`mirror`, `webcam`, `app_streaming`, `cross_control`, `phone_display`, `storage_mount`, `folder_share`, `clipboard`, `notifications`).
  - Added `src/capabilities.rs` with `SystemProber` abstraction, checking `v4l2loopback`, `/dev/fuse`, `fusermount3`, `webrtcbin`, display/Wayland compositors, and kernel dependencies.
  - Added control socket query support for `capabilities` keyword and `{"cmd": "capabilities"}` JSON command, plus CLI `smartconnectd capabilities`.
  - Bound GUI button sensitivity, subtitle status, and `fix_hint` tooltips in `smartconnect-gui`.

## [0.2.2] - 2026-10-31

### Fixed

- **Shared files are saved to the download folder:** A file or folder sent from the phone is now copied to `~/Downloads` (or the desktop's XDG download folder) under a sanitized, non-overwriting name (`report.pdf` → `report (2).pdf`) before the notification appears; **Save as…** still lets you pick another location. The notification names the saved path, and dismissing it no longer loses the file.
- **A phone disconnect can no longer break the home folder in a file manager:** While the phone is away, the FUSE mount answers `stat`/metadata from the last seen attributes instead of `EIO`, so browsing `/home/<user>` does not fail on the mount folder. File contents and directory listings still report the phone as offline until it reconnects.
- **No stale FUSE mount after a helper crash:** `smartconnect-files mount` now runs a watchdog child holding a pipe; if the helper dies (including `SIGKILL` or a panic), the pipe closes and the mount is lazily released, so "Transport endpoint is not connected" is not left at the mount folder. The watchdog also probes the mount every few seconds and releases it when the helper is alive but the mount stopped answering, so a wedged helper cannot hang a file manager showing the folder.

### Security

- **FUSE Mount Hardening:** Enforced `nosuid` and `nodev` mount flags on phone FUSE mounts (`smartconnect-files mount`).
- **Symlink Collision Protection:** Hardened `unique_path` with `symlink_metadata` checks to prevent following dangling symlinks when saving received files.

## [0.2.1] - 2026-10-30

### Added

- **Security & Community Standards:**
  - Added repository `SECURITY.md` detailing private vulnerability reporting, supported versions, and the `/readyfor/` reconnect protocol threat model with existing mitigations.
  - Added GitHub Actions CI workflow (`.github/workflows/ci.yml`) running code formatting, strict clippy checks, and workspace tests in an Arch Linux container.
  - Added GitHub issue templates (`bug_report.yml`, `feature_request.yml`) with strict redaction guidelines for personal data, device IDs, and local network details.
  - Added GitHub pull request template (`.github/PULL_REQUEST_TEMPLATE.md`) enforcing security, redaction, documentation, and test checklists.

### Changed

- Pointed all repository URLs, homepages, and AppImage update feeds to `vemru/linuxsmartconnect`.
- Sanitized sample phone names and network test addresses to use documented placeholders.
- Updated `LICENSE` copyright attribution.
- Resolved all workspace compiler and Clippy lints cleanly under strict `-D warnings`.

## [0.2.0] - 2026-10-30

### Added

- **Phone → PC sharing:** A link, text or a file sent from the phone's share sheet (or the phone's File explorer) appears as a desktop notification on the PC, with **Open**, **Save as…**, and **Dismiss**. Open delegates to `xdg-desktop-portal` or `xdg-open`.
- **Screen mirror orientation adaptation & rotation:** Automatically rotate video stream with `videoflip` to match window orientation, adapt input coordinates on rotation, unpause encoder on rotation, and request keyframes to fix stream freezing.
- **Hardware video decoding across GPUs:** Support hardware decoding across integrated, dedicated, and hybrid GPUs (VA-API `vah264dec`, `vaapih264dec`) with graceful fallback to software decoding (`openh264dec`, `avdec_h264`).
- **File exchange direct download & token negotiation:** Support token in `versionNegotiation` and `/downloadFile` direct downloads in `smartconnect-fex`.
- **Right-click file manager sharing:** Context menu script (`packaging/smartconnect-share`) to quickly link/copy and share multiple files or folders directly to the connected phone.
- **AppImage bundle:** Standalone AppImage for Linux distributions built with `packaging/appimage/make-appimage.sh`.
- **Firewall helper:** Pairing window warns when firewalld blocks port 52020 and offers one-click opening on GNOME and KDE.
- **PC folder sharing to phone:** Export local folder (`~/SmartConnect`) read-only over mutual TLS and NFSv3 to Motorola Smart Connect's Files tab.
- **Cross Control modes:** Single unified interface supporting Shared mouse & keyboard, Duplicate display, and Extend display.
- **Cross Control on GNOME:** Support `xdg-desktop-portal` InputCapture interface for cursor glide and input injection under GNOME Shell.
- **App icons caching:** Asynchronous download of application icons via FEX server (service 17) to `~/.cache/smartconnect-linux/app-icons/`.
- **Tray icon & daemon lifecycle:** System tray icon (StatusNotifierItem) reflecting connection status, background start, and autostart configuration.

### Changed

- **Default share folder:** Moved default PC folder share from `$HOME` to `~/SmartConnect` to protect credentials, dotfiles, and private user configurations.
- **GTK GUI layout:** Redesigned main window with header device menu, consolidated tabs, and status badges.

### Fixed

- **Screen mirror cleanup:** Terminate phone app stream immediately on window close instead of lingering in the background.
- **Display latency reduction:** Offloaded pixel format conversion to GPU (VA-API `vapostproc`) and dedicated encoder thread.
- **Clipboard synchronization on GNOME:** Implemented Xwayland (XFixes) clipboard monitor for GNOME desktop sessions lacking data-control protocol.
- **FUSE mount reconnection:** FUSE phone filesystem survives transient network disconnects and reconnects gracefully.

## [0.1.1] - 2026-10-29

### Security & Hardening

- **FEX Peer IP Restriction:** Enforced exact peer IP validation on all FEX routes, rejecting connections from other devices on the same local subnet.
- **FEX Download Token Enforcement:** Made download tokens mandatory for file retrieval endpoints, responding with 403 Forbidden for missing or invalid tokens.
- **Token Storage Permissions:** Restricted portal restore tokens to mode `0600` inside private `0700` directories.
- **Runtime Socket Security:** Replaced `/tmp` fallback paths with secure per-user runtime directories (`$XDG_STATE_HOME/smartconnect-linux/run`) and enforced `SO_PEERCRED` checks matching the running user's EUID.
- **Path Traversal & Symlink Protection:** Hardened NFS file sharing to reject symlinks escaping the designated share root (`~/SmartConnect`).
- **Dependency Updates:** Updated Tokio to 1.54.1 addressing upstream security advisories.

## [0.1.0] - 2026-10-25

### Added

- **Core Daemon (`smartconnectd`):** Unofficial Linux peer daemon implementing the Motorola Smart Connect wire protocol over TCP 52020, WebSocket, and mDNS discovery.
- **Cryptographic Pairing:** QR code and PIN-based pairing handshake with ephemeral RSA key exchange and AES-256-GCM session encryption.
- **Automatic Reconnection:** Seamless background reconnection over local Wi-Fi and Bluetooth LE wake.
- **Clipboard Synchronization:** Bidirectional text clipboard sharing via Wayland protocols (`wl-clipboard`), skipping password-manager copies.
- **Notification Integration:** Desktop notification synchronization with multi-line chat previews, contact avatars, inline action buttons, and per-app mute.
- **Screen Mirroring (`smartconnect-mirror`):** WebRTC H.264 video streaming with mouse and keyboard input injection.
- **App Streaming:** Launch and interact with phone applications in isolated desktop windows.
- **Cross Control (`smartconnect-crosscontrol`):** Glide PC mouse and keyboard smoothly between Linux desktop and phone display via Wayland layer-shell.
- **Phone as Display (`smartconnect-display`):** Extend or duplicate Linux desktop onto the phone display with touch input injection and on-screen keyboard support.
- **Webcam Helper (`smartconnect-webcam`):** Use phone cameras as Linux webcam video sources via `v4l2loopback` or PipeWire camera nodes.
- **GTK4 / Libadwaita Interface (`smartconnect-gui`):** Modern desktop frontend for device management, pairing, app streaming, and settings.
- **Packaging:** Automated installer (`packaging/install.sh`), systemd user units, desktop files, and Arch Linux PKGBUILD.
