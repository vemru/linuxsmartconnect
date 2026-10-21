# Per-app notification mute in the GTK app and the Noctalia plugin

## What

- **GTK app**: each notification row has a bell-slash toggle, shown pressed while that app is muted.
  The Notifications page has a *Muted apps* expander (app name, package, *Unmute*). A toast confirms
  every change.
- **Noctalia plugin**: a bell-off button on each notification in the panel, drawn in the primary
  colour while muted. Settings → *Muted apps* lists the packages with *Unmute* buttons.
- A muted app gets no desktop popups. Its notifications still appear in both front-ends' lists,
  because the daemon's status keeps them.

## How

- The daemon puts each notification's Android package in the status as `pkg` (`src/status.rs`). The
  per-app list lives in the daemon (`notif-mute.json`):
  `{"cmd":"notif_mute","pkg":P,"mute":true|false}` changes it, and `{"cmd":"notif_mute"}` lists it.
  Every reply carries the whole list as `muted`, and the front-ends replace theirs with it.
- GTK (`gui/src/window.rs`, helpers `daemon::mute_line` and `Reply::muted`): it fetches the list on
  the first status after the daemon (re)appears and whenever the Notifications page is shown. A click
  on a toggle asks for the opposite of the daemon's list. If that fails or changes nothing, the toggle
  is put back to the daemon's state. App names come from the notifications seen (`pkg` → app) or from
  the cached phone app list (`AppsPage::app_name`). The button is hidden for notifications without a
  package (an older daemon).
- Noctalia (`Services/SmartConnect.qml`): it uses the long-lived command socket. A reply with `muted`
  goes to `mutedApps` and does not touch `busy` or `lastMessage`, so a connect in flight keeps its
  "Working…". The list is loaded on every (re)connect and when the panel or the settings open.

## Why

- The toggle shows the state (pressed = muted) rather than the action. With an action icon, a muted
  app showed a plain bell, which reads as "not muted".
- The daemon owns the list, so the GTK app, the plugin and the control socket API always agree. The
  front-ends keep no copy of their own beyond the last reply.

## Verified, and not

Checked against a fake daemon socket with placeholder notifications: the GTK app sent
`{"cmd":"notif_mute","mute":true,"pkg":"com.example.mail"}` and showed both toggles pressed,
"2 apps show no desktop popups" and the toast. The plugin service's mute and list checks passed in
an isolated Quickshell instance. Not checked against the real daemon: `pkg` needs a daemon built
with it (it is used after its next restart).
