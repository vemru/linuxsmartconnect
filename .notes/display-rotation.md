# Phone as display: follow the phone's rotation (svc 202 cmd 7) — 2026-10-21

Opt-in: `--follow-rotation` / `"followRotation": true`. **Untested on the device** (written while the
phone was unavailable); the PC side is tested locally (below).

## What

In extend mode (and with `--test-pattern`), when the phone turns portrait the PC's virtual monitor
turns portrait too, and back. The stream changes to the turned size, so a portrait-held phone shows
a full-height desktop instead of a landscape strip. Mirror mode ignores rotation: the laptop screen
stays landscape.

## Wire facts used (from the APK and the daemon's session logs)

- `cmd 7 {"setOrientation":n}`: the phone sends **1 when its screen becomes landscape, 2
  otherwise** (`ScreenShareActivity.onConfigurationChanged` → `z68.d`). OBSERVED in code.
- It is sent on a configuration change relative to the orientation at activity start. In practice
  it arrived at the start of every logged session, often **twice** about 0.3 s apart, before or
  after cmd 5. Extend sessions held in portrait sent `2`, and rotating later sent `1`. OBSERVED on
  the device (session logs).
- cmd 5's `resolution.width/height` were `1220x2712` (portrait) in every session, including
  landscape mirror sessions, so they don't tell the orientation. OBSERVED.
- The phone fits its video view to each frame's aspect ratio whenever the frame size changes
  (`onFrameResolutionChanged`, also honours CVO rotation). It uses that view size as the touch and
  mouse `resX/resY` (`z68.j`). So a portrait stream fills a portrait phone, and the input module's
  fraction mapping stays right. OBSERVED in code.
- `maxHeight` (1440 here) is only the tallest entry of the phone's preset **landscape** list
  (2560x1440, 1920x1080, 1280x720) that its decoder accepts. Nothing tells us the decoder's
  portrait limit.

## How

```mermaid
sequenceDiagram
    participant P as Phone
    participant A as App (session.rs)
    participant C as capture.rs
    participant N as niri
    P->>A: cmd 7 {setOrientation:2} (×2)
    Note over A: orient = Portrait; rotate_at = now + 350 ms (poll timer)
    A->>A: tick(): wanted = Portrait.apply(base, cap 1920)
    A->>A: on_resize: detach source bin
    A->>C: Source::reconfigure → Capture::reconfigure_extend(w, h)
    C->>N: Session.Stop (old cast)
    C->>N: output V custom-mode HxW@60 (output stays ON)
    alt custom mode refused / no portrait mode listed
        C->>N: output V mode <nearest landscape> + transform 90
    end
    C->>N: new ScreenCast session on V → new PipeWire node
    A->>A: input.retarget(new logical rect) (same wl_output, pointer stays pinned)
    A->>A: attach new source bin, size caps → turned size (encoder renegotiates)
```

- `Orient::apply(base, cap)`: landscape = (long, short); portrait = (short, long), scaled down
  (aspect kept, even sides) so it is at most `cap` tall. `cap` is 1920 unless
  `SMARTCONNECT_DISPLAY_PORTRAIT_MAX` says otherwise (0 = no limit). `base` is the landscape size
  from the offer (`--size`), and cmd 70 replaces it. So cmd 70 1280x720 while portrait sends
  720x1280.
- `Capture::reconfigure_extend` keeps the output **on**: stop cast → `apply_extend_mode` → new
  Mutter cast. The old Capture's backend is swapped into a throwaway Capture that never owned the
  output. `apply_extend_mode` tries a custom mode first (vkms takes any), then the nearest listed
  mode. If no listed mode has the wanted orientation, it uses the other orientation turned with
  `transform 90` (`pick_fallback_mode`). `Capture.transformed` makes Drop set `transform normal`
  before `off`. Enabling an output now always starts with `transform normal`.
- cmd 70 uses the same in-place path, so the earlier off/on (which threw the output's windows onto
  the laptop screen) is gone. If the in-place change fails, the old off/on path is the fallback, and
  then the input injector is re-created, because a pointer pinned to a removed wl_output would be
  stale.
- The phone's cmd 7 is applied after a 350 ms settle (it comes in pairs; quick flips collapse).
  `on_offer` uses the last known orientation, so a cmd 7 that came before the offer opens the output
  turned straight away.

## Why

- **Custom mode first, transform as fallback:** a mode of the exact stream size gives 1:1 pixels and
  leaves the output's transform alone. EDIDs of real connectors (HDMI dummies) rarely list portrait
  modes, and that's when `transform 90` is used.
- **In place, not off/on:** rotation happens much more often than a resolution change. Switching the
  output off moves every window on it to the laptop screen.
- **Portrait cap 1920:** the phone only checks its decoder for landscape sizes. 1080x1920 is the
  everyday portrait size every H.264 decoder takes. A native portrait 1220x2712 might exceed the
  decoder's maximum height and give a black screen. The env var lifts the cap for testing.
- **Not CVO:** sending landscape frames with the 3GPP video-orientation extension would avoid
  portrait encodes, but GStreamer has no sender for that extension. The PC monitor itself has to be
  portrait anyway for the desktop to be laid out portrait.

## Verified (locally, no phone)

- `capture_smoke_extend_rotate` (vkms `Virtual-1`, 200 %): 1920x1080 → portrait custom mode
  1080x1920 (logical 540x960, cast buffers 1080x1920) → portrait via `transform 90` (cast buffers
  also 1080x1920, so niri casts a transformed output upright) → landscape again → off.
- `loopback_rotation_and_resolution_follow_the_phone` drives `App::on_phone` with a phone-like
  webrtcbin and the test pattern: offer 640x360 → cmd 7 `2` (twice) → the received H.264 is 360x640
  → cmd 70 1280x720 → 720x1280 → cmd 7 `1` → 1280x720.
- `orientation_turns_the_stream` (sizes, cap, rounding).

## Not verified

- On the phone: that it shows the portrait stream full-height after a mid-stream size change, that
  touches map correctly in portrait, and decode at the turned sizes.
- A real connector's custom-mode refusal and the transform fallback on hardware (only vkms with a
  forced transform).
