# Publication preparation and release v0.2.1 — 2026-10-30

## What

Comprehensive repository review, privacy sanitization, and release hardening following the initial v0.2.0 release:
1. Updated repository metadata, links, and AppImage update parameters to `vemru/linuxsmartconnect`.
2. Sanitized sample device names in GTK GUI files and network addresses in tests.
3. Updated license attribution with dual copyright notice.
4. Created security policy (`SECURITY.md`), GitHub Actions CI workflow (`.github/workflows/ci.yml`), issue templates (`bug_report.yml`, `feature_request.yml`), and PR checklist template (`.github/pull_request_template.md`).
5. Resolved all compiler and Clippy lints cleanly across the entire workspace under strict `-D warnings`.
6. Verified path remapping flags (`--remap-path-prefix`) for release binary builds to prevent local username/path leakage into binaries.

## How

- **Metadata:** Replaced references to `rudrahnpatel/smartconnect-linux` across `README.md`, `docs/installation.md`, `packaging/appimage/build-in-container.sh`, Noctalia plugin `manifest.json`, and all 8 `Cargo.toml` files.
- **Privacy:** In `gui/src/tray.rs` and `gui/src/window.rs`, replaced device name literals with `"Alex's Phone"`. Replaced local subnet IPs in `src/fex_host.rs` with standard RFC 5737 / documentation IPs (`192.168.1.30`, `192.168.1.35`, `192.168.1.1`).
- **Security & CI:**
  - `SECURITY.md` documents vulnerability disclosure and the threat model of the Motorola `/readyfor/` reconnect protocol.
  - `.github/workflows/ci.yml` runs formatting, Clippy, and tests in an Arch Linux container with full multimedia and Wayland dependencies.
  - Issue templates mandate data redaction prior to posting.
- **Clippy:** Added targeted `#[allow(clippy::type_complexity)]` attributes on complex GUI / monitor layout helper structs and methods, and corrected generic argument count in `bind_opt` in `crosscontrol/src/layer.rs`.
