# Phone screen mirroring (WebRTC viewer) — working 2026-10-21

Status: phone screen streams to Linux and mouse input works (verified on device). Keyboard shortcuts,
Noctalia button and HW decode in progress. Spec: `evidence-2026-10-20/mirroring-spec.md`.

## Architecture
- **Daemon = relay.** `smartconnectd` advertises `serviceVersions` `"1":10, "15":16` and caps
  `"1":2, "15":3, "16":3` (proto.rs), broadcasts every phone frame on `daemon.from_phone`, and exposes
  two JSON control commands: `{"cmd":"watch","services":[…]}` (stream phone frames) and
  `{"cmd":"send","service":N,"data":{…}}` (forward to the phone).
- **Viewer = separate process** `mirror/` (`smartconnect-mirror`, gstreamer-rs): webrtcbin answerer,
  `queue ! rtph264depay ! h264parse ! avdec_h264 ! videoconvert ! autovideosink`, navigation events from
  the video window → svc 16 input.

```mermaid
sequenceDiagram
    participant V as smartconnect-mirror
    participant D as smartconnectd
    participant P as Phone
    V->>D: watch [1,15]; send svc1 cmd1 {view:1}
    D->>P: svc1 cmd1
    P->>D: svc15 cmd2 setup {pkg:"mirror"}
    D->>V: frame
    V->>P: svc15 cmd1 ReceivePeerStartData {supportWebRTC:1, …}
    P->>V: svc15 cmd3 {sessionId, webRTCData: offer}
    V->>P: svc15 cmd4 {webRTCData: answer (H.264 only)}
    V-->>P: svc15 cmd20 ICE (LAN candidates only)
    P-->>V: SRTP H.264 over UDP
    V->>P: svc15 cmd9 heartbeat /30 s ; svc16 cmd1 REQUEST_CAP, cmd4/5 mouse
```

## Pitfalls found on the device (each cost a silent failure)
1. `ReceivePeerStartData.supportCustomDisplay` / `vrSupportUnfolded` are **booleans**. Sending `0` makes
   the phone's Gson throw inside an executor (`submit` swallows it) → nothing logged, 15 s
   `MSG_START_TIMEOUT`. Found by enabling the app's debug logging: production builds use
   `Log.isLoggable`, so `adb shell setprop log.tag.MotoDesktopLog DEBUG` + restart the app.
2. Python GI + webrtcbin produced an empty answer (`"t=0 0"`) and later segfaulted → viewer moved to
   Rust (gstreamer-rs), where the synchronous offer/answer works.
3. The phone offers H.265 first; the answer is restricted to H.264 by trimming the offer (`h264_only`).
4. `decodebin` in the attached bin made `pad.link` fail with NoFormat → explicit decoder.
5. Input targets the mirror's own `displayId` (from svc15 cmd 16/23), coordinates = video-frame pixels.
6. ICE: send only LAN host candidates — Docker bridges would be tried too. (Was a `192.168.` prefix
   check; now the daemon's `pcAddress`, see `ice-candidate-filter.md`.)
