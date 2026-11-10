# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project
adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html). The README's
[feature matrix](README.md#feature-status) shows what is verified on a real phone.

### [Unreleased]

## [0.3.2] - 2026-11-10

### Security & Packaging

- **Fail-Closed Advertised Fingerprint Verification:** Enforced fail-closed validation on advertised mDNS TLS certificate fingerprints (`fp=...`) in `src/discovery.rs`. Malformed or non-decodable fingerprint strings now explicitly reject TLS certificate verification (`rustls::CertificateError::ApplicationVerificationFailure`) rather than silently falling back to unpinned acceptance.
- **Standalone Release Tarball Packaging:** Bundled complete desktop assets into the release distribution tarball, including `smartconnectd.service`, `.desktop` application launchers, scalable SVG icons, and `packaging/install.sh` / `uninstall.sh`.

## [0.3.1] - 2026-11-09

### Security & Hardening

- **Discovery TLS Fingerprint Pinning:** Pinned advertised SHA-256 certificate fingerprints from mDNS TXT `fp=...` records in `src/discovery.rs` during invite push verification, replacing permissive `AcceptAny` (#9, closes #1).
- **Harmonized UNIX Socket Fallback Paths:** Standardized socket path resolution across `SmartConnect.qml`, `packaging/smartconnect-share`, `desktop/mirror_prototype.py`, and `gui/src/display.rs` to securely fall back to `$XDG_STATE_HOME/smartconnect-linux/run` (mode 0700), eliminating all `/tmp` socket fallbacks (#10, closes #2).
- **Zenity Overwrite Confirmation:** Added `--confirm-overwrite` to the fallback zenity file chooser in `src/share.rs` to protect against accidental file overwriting when `xdg-desktop-portal` is unavailable (#11, closes #3).
- **Mirror Control Socket EUID Validation:** Enforced `SO_PEERCRED` validation in `mirror/src/main.rs:serve_control`, rejecting connections whose peer credentials do not match `libc::geteuid()` (#12, closes #4).
- **CI Smoke Test Hardening:** Hardened CI container smoke tests across Arch Linux, Fedora 40, and Ubuntu 24.04 with unprivileged test user execution (#8).

## [0.3.0] - 2026-11-09

### Added

- **Capability Matrix:** Diagnostic capability evaluation (`src/capabilities.rs`), `capabilities` control socket query, and dynamic GUI card sensitivity and status badges.
- **Connection Doctor:** Automated troubleshooting engine (`src/doctor.rs`), `smartconnectd doctor [--json]` CLI command, and GTK4 Diagnostics dialog with safe repair actions.
- **Per-Device Feature Switches:** Granular per-device toggles (`$XDG_STATE_HOME/smartconnect-linux/features.json`), socket commands (`feature.get`/`set`/`list`), and GTK4 Feature Settings page.
- **Searchable Activity Timeline:** SQLite FTS5 indexed event log (`src/timeline.rs`) with 30-day retention pruning, socket queries, and GTK4 Timeline page.
- **Command Palette:** GTK4 fuzzy launcher (`gui/src/palette.rs`) activated via single-instance `smartconnect-gui --palette` (<50ms) with Wayland compositor keybinding support.
- **Photos Browser:** GTK4 photos gallery (`gui/src/photos.rs`) over FUSE with async thumbnail caching and multi-select actions.
- **Send to Phone:** Push files to phone via FEX server, `smartconnectd send <paths...>` CLI, and context menus for Nautilus, Dolphin, Nemo, and Thunar.
- **Verified Installer and Uninstaller:** SHA-256 verification of installer artifacts, clean uninstaller with state purge prompts (`packaging/install.sh --uninstall [--purge]`), and multi-distro CI matrix (Arch, Fedora 40, Ubuntu 24.04).
- **OTP Double-Copy Protection:** Sensitive numeric OTP/2FA token pattern detection requiring double-copy confirmation within 3 seconds before clipboard synchronization.
- **Reconnect Backoff with Jitter:** Randomized jitter added to exponential reconnect backoff to prevent synchronized reconnect storms across local networks.

### Fixed

- **Installer Uninstall Flag:** Properly shift arguments before forwarding to `packaging/uninstall.sh` in `install.sh --uninstall`.
- **Smoke-Test Non-Root Execution:** Execute CI install smoke tests as an unprivileged user across all containerized Linux distributions.
- **Store Error Handling:** Handled `FeatureStore` and `TimelineStore` file operations gracefully without panics or unwraps.

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
