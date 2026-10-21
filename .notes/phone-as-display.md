# Phone as a display for the PC: svc 202 session (`display/src/session.rs`)

Spec: `evidence-2026-10-20/appstream-and-display-spec.md` section B (research material, not in the repo).
Sibling notes: [display-screen-capture.md](display-screen-capture.md) (the video source) and
[display-input-injection.md](display-input-injection.md) (what happens to the phone's touches).

## What

`smartconnect-display` shows this PC's screen on the phone through the unmodified Smart Connect
app ("R4SS", service 202). The phone opens its full-screen `ScreenShareActivity` and plays the
stream; touches, mouse and keyboard on the phone come back over a WebRTC data channel.

- `--mode mirror` (default): the focused physical output (e.g. `eDP-1`).
- `--mode extend`: a spare output (vkms `Virtual-1`, or `--output NAME`, e.g. an HDMI dummy plug)
  is switched on at the stream size, captured, and switched off again on exit.
- `--test-pattern [--pattern smpte]`: `videotestsrc` instead of a capture (no input injection),
  to test the phone side without touching the desktop.
- `--size WxH|native`: stream size. The default is what the phone asks for (`widthReq x heightReq`,
  1920x1080 here); `native` is the phone panel in landscape (e.g. 2712x1220, no letterboxing).

The daemon only relays: `src/proto.rs` now advertises `"202": 1` in `service_versions()` and in
`service_caps()` (the spec says the Windows PC sends it in `caps`; see "Verified" for why both).

## How

`session.rs` talks to `smartconnectd`'s control socket (`$XDG_RUNTIME_DIR/smartconnectd.sock`):
one connection sends `{"cmd":"watch","services":[202]}` and reads `{"service":202,"data":…}` lines,
another sends `{"cmd":"send","service":202,"data":{"ver":"1","cmd":N,"data":{…},"sessionId":S}}`.
Everything runs on a glib main loop; phone frames arrive over an mpsc channel polled every 15 ms
(the same shape as `mirror/src/main.rs`).

```mermaid
sequenceDiagram
    participant D as smartconnect-display
    participant P as Phone (R4SS)
    Note over D: mirror: capture opened up front
    D->>P: cmd 0 {shareType, setAudio:false, setScale, key_pc_ss_version:"pc_ss_ver_2Dot5", sessionId:S}
    Note over P: ScreenShareActivity, PeerConnection (recv audio+video, DataChannel "touchDataChannel")
    P->>D: cmd 7 {setOrientation}
    P->>D: cmd 5 {sdpOffer, resolution:"{width,height,widthReq,heightReq,fps,maxHeight}"}
    Note over D: extend: switch the output on at the stream size, capture it
    Note over D: pipeline READY, set-remote-description, create-answer (video sendonly)
    D->>P: cmd 1 {answer:"{type:answer,sdp}", messageType:"answer"}
    D->>P: cmd 2 {candidate:"{id,label,candidate}", messageType:"candidate"} (LAN UDP only)
    P->>D: cmd 2 {label,id,candidate,type} (flat)
    Note over D: pipeline PLAYING: source ! scale ! H.264 ! RTP ! webrtcbin
    D-->>P: SRTP H.264
    P-->>D: SCTP touchDataChannel: binary input packets
    opt user picks a resolution on the phone
        P->>D: cmd 70 {setWidth,setHeight,setAuto}
        Note over D: extend: drop source, re-create output at new size, swap source bin; rescale
    end
    alt PC stops (Ctrl+C / SIGTERM)
        D->>P: cmd 26 {}
    else phone stops
        P->>D: cmd 52 {stopFrom:0}
    end
```

Pipeline (one per cmd 5 offer):

```
[source bin: capture.source_description() | videotestsrc] ! identity
  ! queue name=head leaky ! videoconvert ! videoscale add-borders=true
  ! capsfilter name=size (NV12, W x H, PAR 1/1)
  ! vah264enc key-int-max=30 b-frames=0 cbr target-usage=7   (or x264enc zerolatency ultrafast)
  ! video/x-h264,profile=constrained-baseline ! h264parse ! rtph264pay pt=<offer PT>
  ! capsfilter name=rtpcaps ! webrtcbin (sink pad requested with caps, transceiver sendonly)
```

Key functions: `Stream::start` (pipeline + negotiation), `pick_h264` / `video_only_pt` (SDP),
`tap_sctp` (input bytes), `Stream::attach_source` / `detach_source` (source swap on resize),
`App::on_offer` / `on_resize` / `on_phone` (state machine), `open_source` (capture or test pattern).

## Why (and the pitfalls found on the device)

- **The answer said `a=inactive` about half the time.** Linking `rtph264pay ! webrtcbin` in a
  launch line gives webrtcbin a sink pad with no caps. If the stream's real caps (with
  `profile-level-id` taken from the encoder's SPS, e.g. `42c028`) reach the pad before
  `set-remote-description`, they no longer intersect the offer's `42e034`, webrtcbin does not
  match our transceiver to the offer's video m-line, creates a fresh recvonly one, and answers
  `inactive` (the phone then shows "Connecting to device..." forever). Fix: request the sink pad
  with caps that have no fmtp fields, set the transceiver to `sendonly` with those caps as
  `codec-preferences`, and keep the pipeline in READY until the answer is set. READY is enough
  for webrtcbin's signalling thread; no media flows yet. Verified 8 of 8 runs since.
- **The video m-line is trimmed to one H.264 payload type** (packetization-mode=1, constrained
  baseline `42e0..`/`42c0..` preferred). The phone offers VP8/VP9/H.265/AV1 and RTX too; one PT
  keeps the answer and the payloader caps trivially consistent. The `video-orientation` extmap is
  dropped as in the mirror viewer.
- **The phone sends its binary input packets as data-channel STRING messages.** With real
  capture, webrtcbin raised hundreds of `on-message-string` signals with an empty string: GStreamer
  copies string messages up to the first NUL, and a touch packet starts with `u32 0`. (INFERRED
  from that: the phone uses SCTP PPID 51 for binary payloads.) `on-message-data` never fires for
  them, so `tap_sctp` adds a buffer probe on every src pad of webrtcbin's internal `sctpdec` and
  passes the raw payloads to the input module. It skips the DCEP OPEN message (byte 0 = 3, carries
  the label). `on-message-data` stays as a fallback when the tap could not be installed.
- **Every svc 202 frame reaches every watcher.** A second instance (or a leftover one) would answer
  the same offer. Frames whose `sessionId` is not ours are ignored, except cmd 50.
- **A trailing caps filter is not a valid launch fragment.** The test source ends in
  `! video/x-raw,…`, so `attach_source` appends `! identity` before building the bin.
- **Extend opens the capture on the offer, not up front.** The output has to be created at the
  stream size, which comes from cmd 5 (`widthReq x heightReq`, or the panel for `--size native`).
  Mirror opens it before cmd 0 because the portal fallback may need the user.
- **cmd 70 in extend swaps the source in the running pipeline.** The old source bin goes to NULL
  and is removed, a new capture is opened at the new size and a new source bin is linked to the
  `head` queue. The encoder renegotiates the new size, so the WebRTC session and the phone's decoder
  keep going (no new cmd 0). Mirror only changes the `size` caps filter. (Update: the output is now
  resized in place, `Capture::reconfigure_extend`, so its windows stay on it; off/on is only the
  fallback. See [display-rotation.md](display-rotation.md).)
- **Keyframes:** `key-int-max=30` is one keyframe per 0.5-1 s, so the phone shows a picture
  quickly after connecting or after loss. webrtcbin also forwards the phone's PLI/FIR upstream as
  force-key-unit events.

## How to run

```sh
cd display && cargo build --release
B=$CARGO_TARGET_DIR/release/smartconnect-display   # or target/release/
$B --test-pattern                  # moving ball; --pattern smpte shows the frame edges
$B                                 # mirror the focused output
$B --mode extend                   # phone = new output (vkms Virtual-1 must exist: modprobe vkms)
$B --mode extend --size native     # fill the phone panel exactly (e.g. 2712x1220)
$B --mode extend --output HDMI-A-1 # an HDMI dummy plug instead of vkms
SMARTCONNECT_DISPLAY_DEBUG_RESIZE=1280x720 $B --mode extend   # pretend the phone sent cmd 70 after 15 s
$B --mode extend --follow-rotation --audio   # opt-ins (display-rotation.md, display-audio.md)
SMARTCONNECT_DISPLAY_DEBUG_ROTATE=2 $B --mode extend --follow-rotation   # pretend cmd 7 portrait after 15 s
```

Defaults come from `~/.config/smartconnect-linux/display.json` (see
[display-settings-and-phone-start.md](display-settings-and-phone-start.md)); `--no-config` ignores it.

Ctrl+C sends cmd 26 and switches an extend output off. The last offer and answer are written to
`$XDG_RUNTIME_DIR/smartconnect-display.{offer,answer}.sdp`.

## Verified on the device (Motorola, Smart Connect 09.0.3.013, niri 26.04)

- The phone accepts svc 202 cmd 0 even from a daemon that did not yet advertise 202 in
  `serviceVersions`/`caps`, and starts the session (offer within about 0.3 s). The proto.rs entries
  are kept for the phone's quick-settings tile and remote-capability check (spec B.0); that tile
  path is not verified yet.
- Test pattern: ball and SMPTE bars shown full-height in `ScreenShareActivity` (16:9 inside the
  20:9 panel, pillarboxed); `--size native` fills the panel exactly.
- Mirror with real capture: the laptop screen shown live on the phone.
- Extend with real capture: `Virtual-1` switched on at 1920x1080, its (empty) workspace shown on the
  phone; switched off again on exit. A simulated cmd 70 (1280x720) re-created the output and the
  stream kept going.
- Stop from the PC (cmd 26) closes the activity on the phone. The phone sends cmd 52 when it stops
  (seen after an ICE failure caused by two instances answering the same offer).
- Offer details seen: recvonly audio (`opus`…), recvonly video with H.264 PT 108
  `profile-level-id=42e034;packetization-mode=1`, SCTP `touchDataChannel`; resolution
  `{width:<panel w>,height:<panel h>,widthReq:1920,heightReq:1080,fps:60,maxHeight:1440}`.
  The phone sends cmd 7 `setOrientation` (1 in mirror = landscape; in extend the activity follows
  how the phone is held, 2 = portrait).

## Not verified / missing

- **Input end to end.** The data channel opens and the raw SCTP tap is installed, but nobody
  touched the phone during a tapped run, so the byte layout and the injection are untested live.
- **Orientation:** opt-in `--follow-rotation` turns the extend output in place on cmd 7, see
  [display-rotation.md](display-rotation.md). Untested on the device; without it, a portrait phone
  letterboxes the landscape output.
- **Phone-initiated start (cmd 50):** the daemon now starts the helper, see
  [display-settings-and-phone-start.md](display-settings-and-phone-start.md) (also: `display.json`
  defaults, one instance, ICE candidate filter). Untested on the device.
- **Audio:** opt-in `--audio`, cmd 54/27, see [display-audio.md](display-audio.md). Untested on the
  device.
- Reconnect requests (cmd 16/72) re-send cmd 0 with a new `sessionId`; not seen live. A request to
  switch between mirror and extend keeps the current source.
- cmd 14/28/35/36 (switch, scale, resolution list, stats).
- **Cross control conflict:** the cross-control edge strip sits on an edge of eDP-1, which in extend
  becomes the boundary to the phone output. While `Virtual-1` was on, niri reported `Virtual-1` at
  x=1920 and eDP-1 moved to x=3840 (x=3200 with a 1280-wide output), so the phone was effectively
  to the left of the laptop screen. Not resolved here.

## Keyboard button → PC on-screen keyboard (phone cmd 62)

**What:** the keyboard button in the phone's display toolbar now shows a keyboard. **How:** the
phone sends `cmd 62` with the string payload `{"setOSK":true}` / `{"setOSK":false}` on each tap
(observed live). `Osk` in `display/src/session.rs` starts `$SMARTCONNECT_OSK` (a command line) or
the first of `wvkbd-deskintl` / `wvkbd-mobintl` found on `PATH`, and kills it on `false` and when the
session ends. It first runs `niri msg action focus-monitor <captured output>` because layer-shell
surfaces open on the focused output. If no keyboard is installed it prints a hint and sends a
desktop notification. **Why:** this matches Windows. The button doesn't open the phone's own IME;
it asks the PC to show its touch keyboard on the shared screen, so the keyboard appears on the
phone and taps reach it through the injected pointer. wvkbd types through
`zwp_virtual_keyboard_v1`, which niri supports, and needs no input-method setup. It's in the AUR
(`yay -S wvkbd`).

## Display scale for Extend (`--display-scale`)

**What:** `--display-scale N` (default 2 = 200 %, the value Windows sends as `setScale` for the
phone display; accepts `1.5` or `150%`) sets the extend output's niri scale. **How:**
`capture::Mode::Extend` carries `scale`, and `enable_extend_output` runs
`niri msg output <name> scale N` before reading the geometry. `wait_for_output` also waits for the
scale, so input mapping uses the scaled logical rect. **Verified live:** Virtual-1 came up at
1356×610 logical (2712×1220 px at 200 %), and pipewiresrc still negotiated full-resolution
2712×1220 buffers, so the picture stays sharp. **Why:** at scale 1 a 2712×1220 panel at phone size
is unreadable. `setScale` sent to the phone is left at `--scale` (default 100): its effect on the
phone isn't known, and changing it isn't needed for the PC-side scale.
