# smartconnect-linux: Feature Build Plan

Audience: a CLI coding agent working in `vemru/linuxsmartconnect` (Rust workspace, `smartconnectd` daemon, GTK4/Libadwaita GUI, MIT OR Apache-2.0 dual license).

Goal: add the features below, implemented fresh to fit this project's scope. They are inspired by the *feature list* of other phone-link apps; no code is copied from any other project.

## Ground rules

1. **No copied code.** Do not paste or port code from other projects (especially GPL ones). Implement from the behavior described here.
2. **Stock phone app only.** The phone runs the unmodified Motorola/Lenovo Smart Connect app. Never assume a feature exists in the protocol; if a phase needs data the phone may not send, verify first and degrade gracefully.
3. **Read before writing.** Phase 0 maps the real code layout. Use real crate, module and file names from that map; the names below are placeholders.
4. **One phase = one branch/PR-sized commit set.** Stop at the end of each phase, report, and wait for a go-ahead.
5. **Every phase must:** pass `cargo fmt --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace`; add tests for new logic; update `docs/usage.md` and `CHANGELOG.md`; keep security defaults (RFC 1918 / link-local checks, socket permissions).
6. **Daemon is the source of truth.** Logic lives in `smartconnectd`; GUI, CLI, tray and Noctalia plugin are thin clients of the control socket.
7. **Sensitive things default OFF:** storage mount, PC-folder sharing, cross control.

---

## Phase 0: Recon (no code changes)

- Map the workspace: crates, binaries, control-socket protocol (message types, framing), config format and location, event/handler structure for clipboard, notifications, files, mirror, webcam.
- Note where the GUI gets state from the daemon and how pages are registered.
- Output: `.notes/feature-plan-recon.md` with the map and the concrete file paths each later phase will touch.

**Done when:** the recon note exists and lists the integration point for each phase below.

---

## Phase 1: Capability matrix

**What:** the daemon reports, per feature, whether it can work right now and why not.

- Add a `capabilities` request on the control socket returning, per feature (`mirror`, `webcam`, `app_streaming`, `cross_control`, `phone_display`, `storage_mount`, `folder_share`, `clipboard`, `notifications`): `Available` or `Unavailable { reason, fix_hint }`.
- Probes: GStreamer elements present, `/dev/video*` / v4l2loopback loaded, `/dev/fuse` and `fusermount3`, Wayland `wlr-layer-shell` / GNOME InputCapture portal, phone connected, wl-clipboard present.
- GUI: bind button sensitivity and tooltip to capabilities; show the `fix_hint` text when disabled.
- Re-probe on demand and on relevant events (phone connect/disconnect).

**Done when:** with v4l2loopback unloaded, the Webcam control is disabled with a clear reason; loading the module and refreshing enables it. Unit tests cover probe results mapping to states.

---

## Phase 2: Connection Doctor

**What:** one place that finds and explains what keeps the phone from connecting or a feature from working.

- Add `smartconnectd doctor [--json]` returning structured checks `{id, title, status: Ok|Warn|Fail, detail, fix}`.
- Checks: avahi running; ports 52020 and 2049 listening; firewall (firewalld/ufw/nftables) not blocking them for the LAN; Bluetooth service state; phone reachable on LAN; plus everything from Phase 1.
- GUI page "Diagnostics" renders the same JSON; "Copy report" button (redact IPs/serials optionally).
- Fixes: show the exact command; offer one-click run through `pkexec` only for safe, whitelisted commands.

**Done when:** `doctor --json` is stable and tested with mocked probes; stopping avahi produces a `Fail` with a working fix hint.

---

## Phase 3: Per-feature switches

**What:** each paired phone has its own on/off switches, enforced by the daemon.

- Config: per-device feature flags with defaults (sensitive ones OFF).
- Enforce in the daemon handlers (reject/ignore when off), not only in the GUI. Reflect state in `capabilities` (`Unavailable { reason: "disabled in settings" }`).
- GUI settings page with a switch per feature; CLI `smartconnectd feature <device> <name> on|off`.
- Migration: existing users keep current behavior except the sensitive features, which prompt once.

**Done when:** turning off clipboard stops sync in both directions without restarting; tests cover default values and enforcement.

---

## Phase 4: Timeline

**What:** a searchable history of what moved between PC and phone.

- SQLite (`rusqlite`, FTS5) in the XDG data dir. Table: `id, ts, device, kind, title, detail, path, direction`.
- Handlers append rows for: file transfers, clipboard items (respecting Phase 9 rules once added), notifications, session connect/disconnect.
- Control-socket queries: `timeline.list`, `timeline.search`, `timeline.clear`.
- GUI page: list, search box, filter by kind, "open file/folder" action. Setting for retention (default 30 days) and an "off" switch (Phase 3).

**Done when:** searching "pdf" finds a transferred file; retention prunes old rows; DB file is mode 0600.

---

## Phase 5: Command Palette

**What:** a fast keyboard launcher for app actions.

- Action registry (id, title, keywords, handler): connect/pair, start/stop mirror, webcam on/off, send file, open phone storage, open Timeline, open Diagnostics.
- GTK4 popup window with fuzzy match (`nucleo` or similar), arrow-key navigation, Enter to run, Esc to close.
- Wayland has no app-level global hotkeys: provide `smartconnect-gui --palette` so users bind it in their compositor; document examples for Hyprland, Sway, GNOME, KDE. Expose the same action list to the Noctalia plugin.
- Only list actions that are `Available` (Phase 1).

**Done when:** `--palette` opens in under ~200 ms when the GUI is already running (single-instance activation) and runs actions via the daemon.

---

## Phase 6: Photos browser

**What:** browse the phone's camera roll and screenshots on the PC.

- Operate over the existing FUSE mount (DCIM, Pictures/Screenshots); no new protocol work.
- GTK4 `GridView` with lazy async thumbnails cached under the XDG cache dir (keyed by path + mtime + size); never block the UI thread on the mount.
- Actions: open, copy to PC folder, drag out, select multiple.
- Page only active when the storage mount is available and enabled (Phases 1 and 3).

**Done when:** a 1,000-photo folder scrolls smoothly; thumbnails persist across restarts; unmounting the phone disables the page cleanly.

---

## Phase 7: "Send to phone" in file managers

**What:** right-click files on the PC and send them to the phone.

- CLI: `smartconnect send [--device NAME] <paths...>` talking to the daemon over the control socket; progress via notification and Timeline.
- Integrations installed by the packaging script: Nautilus (Python extension or script), Dolphin (`.desktop` service menu), Thunar (custom action snippet). Keep each small and optional.
- Handle directories, multiple files and a missing phone with clear errors.

**Done when:** right-click → Send to phone works in at least Nautilus and Dolphin; failures show a readable notification.

---

## Phase 8: Packaging and install

**What:** easier install, verified downloads, clean removal.

- `packaging/install.sh`: add SHA-256 verification of downloaded artifacts, step-by-step output, `--uninstall` that removes everything the installer added (including services, desktop entries, file-manager integrations, optional v4l2loopback config).
- CI release job: build `.deb` (`cargo-deb`), `.rpm` (`cargo-generate-rpm`), AppImage, tarball; publish `SHA256SUMS` with the release.
- Keep the existing Arch `PKGBUILD` working.

**Done when:** a tagged release produces all artifacts plus checksums; install then uninstall on a clean VM leaves no files behind.

---

## Phase 9 (optional, after the above)

- **Clipboard extras:** optional encrypted-at-rest history ring; never store text that looks like a one-time code or came from a password manager; "Copy code" action on notifications (regex + keywords like "code", "OTP", "verification").
- **Media/battery/toggles:** first sniff what the Motorola app actually sends. If media state exists, expose it via MPRIS (`zbus`); if battery state exists, add low-battery notifications. Skip anything the protocol doesn't carry and say so in `docs/usage.md`.

---

## Reporting format after each phase

1. What changed (files, new commands, config keys).
2. How to test it manually (exact commands).
3. Anything skipped or discovered (protocol gaps, risks).
4. Proposed next step; wait for approval.
