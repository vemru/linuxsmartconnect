# Phone-as-display: PC screen capture (`display/src/capture.rs`)

## What

`capture::start(Mode)` gives the svc 202 session a GStreamer source for the PC screen:

- `Mode::Mirror`: records the focused physical output (eDP-1 here).
- `Mode::Extend { width, height, output }`: turns on a spare output (the first `Virtual-*`
  vkms connector, or a named one such as an HDMI dummy `HDMI-A-1`), gives it
  `width`x`height`@60, places it to the right of the other outputs and records it. When the
  `Capture` is dropped, the output is switched back off.

`Capture` exposes `width/height` (stream pixels), `source_description()`
(`pipewiresrc path=<node> do-timestamp=true keepalive-time=1000 ! vapostproc ! video/x-raw ! videoconvert`),
`geometry()` (the logical rect in niri, used for input mapping) and `output_name()`.

## How

```mermaid
sequenceDiagram
    participant C as capture.rs
    participant N as niri (msg IPC)
    participant M as niri org.gnome.Mutter.ScreenCast
    participant P as PipeWire
    opt Extend
        C->>N: output V on / custom-mode WxH@60 / position set X Y
        C->>N: poll --json outputs until mode+pos applied (else nearest listed mode)
    end
    C->>M: CreateSession({}) -> session
    C->>M: Session.RecordMonitor(connector, {cursor-mode: 1}) -> stream
    C->>M: subscribe Stream.PipeWireStreamAdded
    C->>M: Session.Start()
    M-->>C: PipeWireStreamAdded(node_id)
    Note over C: Capture keeps the zbus connection (niri ends the cast if it closes)
    C->>P: (session pipeline) pipewiresrc path=node_id ! vapostproc ! ...
    Note over C: Drop: Session.Stop(); Extend: niri msg output V off
```

- **Mutter ScreenCast API first.** niri 26.04 serves `org.gnome.Mutter.ScreenCast` (v4) itself,
  so we can pick a monitor by connector name with no dialog. This uses zbus's blocking API
  (zbus 5, async-io backend, `blocking-api` feature). The node id comes from the
  `PipeWireStreamAdded` signal; we subscribe before `Start` and wait up to 5 s.
- **Portal fallback (Mirror only).** If the Mutter API fails, we use xdg-desktop-portal
  ScreenCast through ashpd 0.13. On niri that portal is served by `xdg-desktop-portal-gnome`
  (`niri-portals.conf`: `default=gnome;gtk`). We set `persist_mode=ExplicitlyRevoked` and keep
  the restore token in `~/.local/share/smartconnect-linux/screencast-token`, so the picker only
  appears once. A stale token is deleted and the picker is shown again. Because the stream
  lives on the portal's restricted PipeWire remote, the source description includes
  `fd=<OpenPipeWireRemote fd>`. Capture owns that fd, and pipewiresrc dups it.
- **Extend output control** goes through `niri msg output <name> on|custom-mode|mode|position|off`.
  These are temporary changes and never touch the config file. The user's config has
  `output "Virtual-1" { off }`, so we have to call `on` first. vkms accepts arbitrary custom
  modes (2712x1220 tested). Real connectors may refuse them, so if the requested mode has not
  appeared in `niri msg --json outputs` within 2 s, we fall back to the nearest listed mode
  (same orientation first, then size difference, then closeness to 60 Hz).

## Why

- **Why `vapostproc`.** niri's cast offers only DMA-BUF: `pw-cli enum-params <node> EnumFormat`
  shows BGRx with a mandatory modifier list (LINEAR, vendor modifiers, INVALID) and no SHM.
  Here is what each attempt did:
  - `pipewiresrc ! videoconvert` and `! video/x-raw ! videoconvert` both failed with
    "no more input formats / not-negotiated".
  - `glupload ! glcolorconvert ! gldownload` failed with "Failed to upload buffer".
  - `vapostproc` (VA-API) imports the dmabuf and outputs system-memory BGRx. It ran at about
    57 fps at 1080p.
  - `always-copy` is deprecated and does not change format negotiation.

  This makes VA-API a dependency. For zero-copy, the session can use `vapostproc ! vah264enc`
  instead.
- **Why Mutter over portal.** The Mutter API shows no dialog, lets us choose the output by
  connector (which Extend needs, because the portal picker can't be scripted), and needs no
  fd. The portal stays as a fallback for compositors without the Mutter API.
- **Why turn the output off on Drop.** If an invisible virtual screen stays on, the user's
  cursor and windows can get lost on it. Caveat: a SIGKILL or a panic=abort skips Drop, and
  then Virtual-1 stays on until `niri msg output Virtual-1 off` is run (or niri is reloaded
  or restarted).

## Tests

- `cargo test --release parse_and_match`: unit test that parses outputs and checks mode
  matching.
- `cargo test --release capture_smoke -- --ignored --nocapture --test-threads=1`: three live
  tests (mirror via Mutter, extend on Virtual-1 at 2712x1220, and portal). Each runs
  gst-launch into fakesink for 3 s. Result: 170 / 109 / 171 buffers, and Virtual-1 was off
  afterwards.

## Layout pinning and scale (follow-up)

- **Layout bug fixed:** niri lays out outputs with an explicit position first and auto-places the
  rest after them. Giving only Virtual-1 a position (x = right edge of the laptop panel) made niri
  move the unpositioned laptop panel (eDP-1) to the right of Virtual-1, which put the phone on the
  left. `enable_extend_output` now pins every other enabled output at its current position
  (`niri msg output <o> position set x y`, temporary) before placing the virtual output. Verified:
  eDP-1 stays at 0,0 and Virtual-1 sits at x=1920.
- **Scale:** `Mode::Extend { …, scale }` → `niri msg output <name> scale N`; `NiriOutput.scale`
  is parsed from `logical.scale`, and `wait_for_output` waits for it.
- **Pixel vs logical size:** niri's ScreenCast stream `Parameters.size` is the *logical* size,
  e.g. 1356×610 at 200 %, while the PipeWire buffers are the mode size (2712×1220).
  `Capture.width/height` now come from the output mode, with `Parameters.size` only as a
  fallback. The extend smoke test asserts the negotiated caps are full resolution.
