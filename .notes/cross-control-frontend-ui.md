# Cross Control toggle in the desktop frontends

## What
- Noctalia plugin: "Cross Control" button + edge-cycle icon button (right/left/top/bottom, saved as
  `pluginSettings.crossEdge`) in Panel.qml; process handling in Services/SmartConnect.qml.
- GTK app: "Cross Control" PreferencesGroup on the Phone page with an `adw::SwitchRow` and an
  `adw::ComboRow` edge selector.
- Per-notification "Mute app" was NOT added: status notifications (`PhoneNotification` in
  `src/status.rs`) carry no package name, so the UI has nothing to send as `pkg` to `notif_mute`.

## How
- Both frontends run `smartconnect-crosscontrol --edge <edge>` as a child (found as in
  [packaging-and-workspace.md](packaging-and-workspace.md)) and stop it with SIGINT (the binary then
  tells the phone to stop).
- GTK: `gui/src/mirror.rs` was generalised (`Mirror::viewer()` / `Mirror::cross_control()`,
  `start(args)`, `stopping()`); `window.rs` adds `set_cross_control` + `watch_cross` (500 ms poll).
  The switch is re-synced in `refresh_phone`; a helper in the SIGINT-sent state counts as off so
  the switch doesn't flip back on while it winds down. `shutdown()` stops both helpers.
- QML mirrors the existing mirror Process pattern (crossProcess, crossStatus error line).

## Why
- Reusing the mirror child-process code keeps one SIGINT/reap path. Edge is chosen before start
  (selector disabled while running) because the binary only reads `--edge` at startup.
- To enable muting later, the daemon would need to expose `package_name` (e.g. `pkg`) in
  `PhoneNotification`.

Update (2026-10-21): the daemon now sends `pkg` with each status notification, and both
front-ends have the per-app mute. See [notification-mute-ui.md](notification-mute-ui.md).
