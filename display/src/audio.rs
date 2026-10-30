//! What this PC plays, sent to the phone (opt-in `--audio` / `"audio": true`).
//!
//! The phone's offer always has a receive-only Opus audio section (it asks for audio whether or not
//! `setAudio` is on), and its WebRTC stack plays a received audio track by itself. So the sender
//! only has to answer that section send-only and feed it: the default output's monitor →
//! Opus → RTP. The phone's audio switch (cmd 54) mutes and unmutes the branch.

use std::time::Duration;

use anyhow::{Context, Result, bail};
use gst::prelude::*;

/// A gst-launch fragment producing raw audio, instead of the default output's monitor.
pub const ENV_SRC: &str = "SMARTCONNECT_DISPLAY_AUDIO_SRC";

/// The PC's audio: what the default output plays (its monitor).
pub fn source_description() -> String {
    if let Some(s) = std::env::var(ENV_SRC).ok().filter(|s| !s.trim().is_empty()) {
        return s;
    }
    if gst::ElementFactory::find("pulsesrc").is_some() {
        // PulseAudio or pipewire-pulse. No clock from the sound card: the video branch times its
        // frames on the system clock and should stay on it. A small buffer (40 ms, 10 ms periods
        // instead of pulsesrc's 200 ms): the phone holds the video back to keep it in sync with
        // late audio, so audio latency becomes display latency.
        "pulsesrc device=@DEFAULT_MONITOR@ provide-clock=false buffer-time=40000 latency-time=10000"
            .to_owned()
    } else {
        // PipeWire without its Pulse server: a capture stream on the default sink's monitor.
        "pipewiresrc stream-properties=\"props,stream.capture.sink=true\" do-timestamp=true"
            .to_owned()
    }
}

/// Opus payload type of the offer's audio section (`a=rtpmap:<pt> opus/48000/2`).
pub fn pick_opus(sdp: &str) -> Option<u32> {
    let mut in_audio = false;
    for l in sdp.lines().map(str::trim) {
        if l.starts_with("m=") {
            in_audio = l.starts_with("m=audio");
        } else if in_audio
            && let Some((pt, codec)) = l.strip_prefix("a=rtpmap:").and_then(|r| r.split_once(' '))
            && codec.to_ascii_lowercase().starts_with("opus/48000")
        {
            return pt.parse().ok();
        }
    }
    None
}

pub fn rtp_caps(pt: u32) -> String {
    format!("application/x-rtp,media=audio,encoding-name=OPUS,clock-rate=48000,payload={pt}")
}

/// `src` → 48 kHz stereo → mute switch (`audiovol`) → Opus (10 ms frames, the phone asks for
/// `ptime:10`) → RTP with payload type `pt`.
pub fn branch_description(src: &str, pt: u32, on: bool) -> String {
    format!(
        "{src} ! queue max-size-time=40000000 leaky=downstream ! audioconvert ! audioresample \
         ! audio/x-raw,rate=48000,channels=2 ! volume name=audiovol mute={} \
         ! opusenc bitrate=128000 frame-size=10 ! rtpopuspay pt={pt} ! capsfilter caps={}",
        !on,
        rtp_caps(pt)
    )
}

/// Checks that `src` delivers audio within 2 s, so a missing sound server can't take the video
/// down with it (an element error stops the whole pipeline).
pub fn preflight(src: &str) -> Result<()> {
    let p = gst::parse::launch(&format!("{src} ! fakesink name=sink sync=false"))
        .context("audio source")?
        .downcast::<gst::Pipeline>()
        .map_err(|_| anyhow::anyhow!("not a pipeline"))?;
    let (tx, rx) = std::sync::mpsc::channel::<()>();
    let sink = p.by_name("sink").context("no sink")?;
    let pad = sink.static_pad("sink").context("no sink pad")?;
    let tx = std::sync::Mutex::new(Some(tx));
    pad.add_probe(gst::PadProbeType::BUFFER, move |_, _| {
        if let Some(tx) = tx.lock().unwrap().take() {
            let _ = tx.send(());
        }
        gst::PadProbeReturn::Ok
    });
    p.set_state(gst::State::Playing)
        .context("starting the audio source")?;
    let bus = p.bus().context("no bus")?;
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    let result = loop {
        if rx.try_recv().is_ok() {
            break Ok(());
        }
        if let Some(msg) = bus.timed_pop_filtered(
            gst::ClockTime::from_mseconds(50),
            &[gst::MessageType::Error],
        ) && let gst::MessageView::Error(e) = msg.view()
        {
            break Err(anyhow::anyhow!("{} ({:?})", e.error(), e.debug()));
        }
        if std::time::Instant::now() >= deadline {
            break Err(anyhow::anyhow!("no audio within 2 s"));
        }
    };
    let _ = p.set_state(gst::State::Null);
    result
}

/// Fails if an element named in `desc` is not installed (gives a clearer error than parse::launch).
pub fn check_elements() -> Result<()> {
    for e in [
        "opusenc",
        "rtpopuspay",
        "audioconvert",
        "audioresample",
        "volume",
    ] {
        if gst::ElementFactory::find(e).is_none() {
            bail!("GStreamer element {e} is missing (gst-plugins-base/good)");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_opus_in_the_audio_section_only() {
        let sdp = "v=0\r\nm=audio 9 UDP/TLS/RTP/SAVPF 111 63 0\r\na=mid:0\r\na=recvonly\r\n\
a=rtpmap:111 opus/48000/2\r\na=fmtp:111 minptime=10;stereo=1;useinbandfec=1\r\na=rtpmap:63 red/48000/2\r\n\
m=video 9 UDP/TLS/RTP/SAVPF 108\r\na=rtpmap:108 H264/90000\r\n";
        assert_eq!(pick_opus(sdp), Some(111));
        assert_eq!(
            pick_opus("m=video 9 X 96\r\na=rtpmap:96 opus/48000/2\r\n"),
            None
        );
        assert_eq!(
            pick_opus("m=audio 9 X 100\r\na=rtpmap:100 OPUS/48000/2\r\n"),
            Some(100)
        );
    }

    /// Live: the default output's monitor delivers audio here (reads it for at most 2 s).
    /// `cargo test --release audio_smoke -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn audio_smoke_default_monitor() {
        gst::init().unwrap();
        check_elements().unwrap();
        let src = source_description();
        eprintln!("source: {src}");
        preflight(&src).expect("audio from the default monitor");
        // The whole branch builds and negotiates with the real source.
        let desc = format!("{} ! fakesink", branch_description(&src, 111, true));
        let p = gst::parse::launch(&desc).unwrap();
        p.set_state(gst::State::Playing).unwrap();
        let bus = p.bus().unwrap();
        let err =
            bus.timed_pop_filtered(gst::ClockTime::from_seconds(1), &[gst::MessageType::Error]);
        p.set_state(gst::State::Null).unwrap();
        assert!(err.is_none(), "{err:?}");
    }

    #[test]
    fn branch_mutes_when_off() {
        let d = branch_description("audiotestsrc", 111, false);
        assert!(d.contains("volume name=audiovol mute=true") && d.contains("rtpopuspay pt=111"));
        assert!(d.ends_with("payload=111"));
        assert!(branch_description("x", 96, true).contains("mute=false"));
    }
}
