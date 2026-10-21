# Phone as display: the PC's audio on the phone (svc 202, opt-in) — 2026-10-21

`--audio` / `"audio": true`. **Untested on the device** (written while the phone was unavailable).

## What

What the PC plays (the default output's monitor) goes to the phone as Opus, so a video playing on
the extended or mirrored screen is heard on the phone. cmd 0 then says `setAudio: true`. The audio
switch in the phone's toolbar menu (cmd 54) mutes and unmutes it. Without the option nothing
changes: the phone's audio section is answered `inactive`, exactly as verified on the device.

## Wire facts used

- The phone's offer **always** has a receive-only audio section (`OfferToReceiveAudio=true`; Opus
  PT 111 `stereo=1`, `ptime:10`, plus RED/ISAC/G722/PCMx/CN/telephone-event), whatever `setAudio`
  says. OBSERVED in code and in offers saved on the device.
- `setAudio` in cmd 0 / cmd 14 is stored in the phone's preferences and shown by its audio switch.
  The phone sends the stored value back in cmd 50 / 16 / 72. OBSERVED in code (`p68`).
- cmd 54 `{"shareType","setAudio"}` is the phone's audio switch (`j24` checkbox). The phone updates
  its own state, then tells the PC. cmd 27 `{"setAudio"}` from the PC updates the phone's switch.
  OBSERVED in code. No code on the phone was found that mutes the received track; the PC is
  expected to stop sending (INFERRED).
- The phone's WebRTC stack plays a received remote audio track by itself (audio device module
  enabled). INFERRED from code.

## How

- `audio::source_description()`: `pulsesrc device=@DEFAULT_MONITOR@ provide-clock=false`
  (PulseAudio or pipewire-pulse), else `pipewiresrc stream-properties="props,stream.capture.sink=true"`,
  or `$SMARTCONNECT_DISPLAY_AUDIO_SRC`. `audio::preflight` checks that it delivers a buffer within
  2 s at startup. If not, audio is off for the run, with a desktop notification, and cmd 0 says
  `setAudio:false`.
- Per offer (`Stream::start`): the Opus PT from the offer's audio section (`pick_opus`), then a branch
  `src ! queue ! audioconvert ! audioresample ! 48 kHz stereo ! volume name=audiovol ! opusenc
  frame-size=10 ! rtpopuspay pt=<PT>`. It is linked to a webrtcbin sink pad requested with Opus caps,
  with the transceiver send-only and those caps as codec preferences: the same anti-`a=inactive`
  pattern as the video (`sendonly_pad`).
- cmd 54: `Stream::set_audio(on)` sets `volume mute`. If the phone turns audio on and nothing is being
  sent (option off or capture failed), the PC answers cmd 27 `{setAudio:false}` so the phone's switch
  shows the truth. Reconnects (16/72) take the phone's `setAudio` for the next cmd 0.

## Why

- **Mute, not stop:** Opus keeps encoding silence (100 packets/s with 10 ms frames), so the
  receiver's jitter buffer and lip-sync state see one continuous stream. Toggling never needs a
  renegotiation, and with the option on the audio section is always negotiated.
- **Opt-in and preflighted:** an element error in the audio branch would stop the whole pipeline,
  video included. The default path is untouched, so the device-verified answer stays as it was.
- **`provide-clock=false`:** the video is timestamped on the system clock (`do-timestamp`). A
  sound-card clock as pipeline clock would make it drift against that.
- **The monitor, not a virtual sink:** nothing to install or re-route. The downside: the sound also
  keeps playing on the PC (with the phone a little behind). Muting the PC output may also mute the
  monitor, depending on the sound server. Documented as a limitation.
- 10 ms frames follow the phone's `ptime:10`.

## Verified (locally, no phone)

- `loopback_audio_and_video_reach_a_phone_like_peer` (a phone-like webrtcbin offerer in-process): the answer has audio
  `a=sendonly` OPUS/48000/2 and video `a=sendonly`. Audio and H.264 RTP arrive. Muted, Opus still
  flows at 100 packets/s.
- `loopback_without_audio_keeps_the_audio_section_inactive`: without the option the answer is
  unchanged (audio `inactive`).
- `audio_smoke_default_monitor`: the default monitor here (PipeWire 1.6 via pipewire-pulse)
  delivers audio and the full Opus branch negotiates.

## Not verified

- That the phone plays the audio, how its switch looks after our cmd 0 `setAudio:true`, and A/V
  delay on the phone.
