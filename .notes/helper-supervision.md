# Daemon-supervised helpers (stream host + cross control) — 2026-10-21

## What
Feature tiles on the phone did nothing because no PC process answered phone-initiated requests:
- **App streaming tile**: the phone sends svc 15 cmd 2 `{pkg, pkgName, cmp?, icon, source:1, userId}` (user picked
  the app in the phone's own picker, e.g. Calculator/WhatsApp) and re-sends it every few seconds. No PC app picker is
  involved; `smartconnect-mirror --host` already answers this (cmd 1 echo, `source:1`). It just wasn't running.
- **Cross Control tile**: svc 26 cmd 0 (`srcServiceId 19`, cid 0). The crosscontrol client already answered cmd 0
  with cmd 1 `{cid, trigger:1}` and arms the edge, but only if it was running.

`src/helpers.rs` (`Helpers`, started from `main.rs`):
- `supervise_host`: after the first verified session, ensures one `smartconnect-mirror --host`; restart backoff
  2 s → 60 s (reset after a 60 s run). If the host socket (`$XDG_RUNTIME_DIR/smartconnect-mirror.sock`) already
  answers (user-started host) it does nothing. `--host` itself refuses to start twice.
- `answer_cross_control`: on svc 26 cmd 0, if no crosscontrol holds `$XDG_RUNTIME_DIR/smartconnect-crosscontrol.lock`,
  spawns `smartconnect-crosscontrol --edge ${SMARTCONNECT_CROSS_EDGE:-right} --accept`.
- Children: stdout/stderr → `$XDG_RUNTIME_DIR/smartconnectd/<name>.log`, `PR_SET_PDEATHSIG=SIGTERM`, and SIGTERM +
  2 s grace on daemon Ctrl+C/SIGTERM (main now also handles SIGTERM) so they send cmd 7 / cmd 5 to the phone.
- Binary lookup: `$SMARTCONNECT_MIRROR_BIN` / `$SMARTCONNECT_CROSSCONTROL_BIN` (…`_WEBCAM_BIN`, `_DISPLAY_BIN`),
  next to smartconnectd, `$PATH` (no build-directory fallback since
  [packaging-and-workspace.md](packaging-and-workspace.md)). `SMARTCONNECT_HELPERS=0` disables; skipped without a display.

`crosscontrol/`: `--accept` (send cmd 1 right after start; it missed the phone's cmd 0) and a single-instance flock
lock with takeover: a newer instance SIGINTs the holder (pid in the lock file) and waits ≤ 3 s for the lock.

```mermaid
sequenceDiagram
    participant P as phone
    participant D as smartconnectd
    participant C as crosscontrol
    P->>D: svc26 cmd0 (tile tapped)
    alt lock held (client running)
        D-->>C: relayed via watch; C sends cmd1 {cid,trigger:1}
    else no client
        D->>C: spawn --edge right --accept
        C->>P: svc26 cmd1 {cid,trigger:1}
    end
    P->>C: cmd2 {code:1,w,h} → armed (enter on edge)
```

## Why
- Supervising in the daemon (not systemd units) because the helpers need the daemon's socket and graphical env, and
  the daemon already knows when a session is verified. The lock/socket probes keep exactly one host / one client
  even when the GUI or Noctalia start their own.
- Cross control only *arms* on a phone request (spec: phone cmd 0 → PC cmd 1 → cmd 2; entering still happens on
  the edge crossing), matching the existing in-client cmd 0 handling.
- Takeover (not refuse) in crosscontrol: the Noctalia/GTK toggles don't know about a daemon-spawned client, so their
  "start" must win rather than fail.
- Known gap: the GUI/plugin toggle shows "off" while a daemon-spawned client runs.

## Reply-window warning (`reply window failed to run`)
Not a spawn failure: qs logs show the window loaded fine (env had WAYLAND_DISPLAY etc.); the qs process was killed
together with the Noctalia shell restart 3 s later (non-zero exit). Now: spawn errors warn, non-zero exits log info.
Also found: closing the reply window from the compositor left `qs` running forever → `onClosed: Qt.quit()` in
`desktop/reply.qml` (verified with `niri msg action close-window`).

## Display sender (`smartconnect-display`, svc 202 cmd 50) — added later
`answer_display`: on phone svc 202 cmd 50 (the user starts "PC screen" on the phone), if no instance holds
`$XDG_RUNTIME_DIR/smartconnect-display.lock`, it spawns `smartconnect-display --mode <shareType>`. The other options
come from the shared `~/.config/smartconnect-linux/display.json`. If the helper exits with an error within 20 s, or
can't be started, the daemon sends the phone cmd 26 so it leaves "connecting". Details:
[display-settings-and-phone-start.md](display-settings-and-phone-start.md). Untested on the device.
